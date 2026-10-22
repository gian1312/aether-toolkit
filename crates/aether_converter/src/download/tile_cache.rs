//! The raw source-tile cache of the CLI `download` (CONTRACT §9b
//! `cache_dir`, changelog 38; `aether_converter` 0.2.8).
//!
//! It keeps what a tile service answered — the PNG/WebP body exactly as it
//! arrived, before any decode, and the "no tile here" of an HTTP 404 — on
//! disk across runs, shared by every job, service and zoom that names the
//! same `cache_dir`:
//!
//! ```text
//! <cache_dir>/<service_key>/service.json          {"url_template": …, "encoding": …}
//! <cache_dir>/<service_key>/z<zoom>/x<x>_y<yblock>.pack
//! ```
//!
//! `service_key` is the first 16 hex digits of SHA-256(`url_template`, query
//! string included, + "\n" + lower-cased `encoding`), so templates differing
//! only in their query never share a directory; `service.json` records the
//! template without its query string ([`service_template`]), so a key in the
//! query never reaches the disk in clear text. `yblock = y / 256`. A pack
//! holds the 256 tiles `y = yblock·256 + i` of one tile column (all integers
//! little-endian):
//!
//! ```text
//! offset  bytes
//!      0      4  magic "AETP"
//!      4      2  version u16 = 1
//!      6      4  zoom u32
//!     10      4  x u32
//!     14      4  yblock u32
//!     18   5120  index: 256 entries of 20 bytes, entry i = tile y = yblock·256 + i:
//!                  u64 offset of the body, u32 length, u8 status (0 unknown,
//!                  1 present, 2 no tile = HTTP 404), 3 reserved bytes (0),
//!                  u32 CRC-32 (IEEE) of the body
//!   5138      …  data region: bodies, appended
//! ```
//!
//! **Writing** (one writer thread per run, [`writer`]): the pack is locked
//! (`flock` on unix, an exclusive-write open on Windows — see [`lock`]), the
//! bodies are appended at the end of the file in one write, and only then
//! are their index entries written in place; closing the file releases the
//! lock. A run killed between the two leaves its bytes unreferenced: the
//! entry still reads as before (a miss for a new tile). A rewritten entry
//! (a corrupt one repaired, a repair job's fresh body) points to newly
//! appended bytes and the old ones become garbage; nothing compacts a pack
//! — delete the directory to reclaim the space.
//!
//! **Reading** takes no lock. Every entry is validated — the pack header must
//! name this very column, a present body must lie inside the file past the
//! index and match its CRC, a 404 entry must have length and CRC 0 — and any
//! entry that fails is a miss: the tile is fetched and the entry rewritten.
//! A torn entry or a body cut short by a crash therefore costs one refetch,
//! never a wrong tile. The data a valid entry points to is never rewritten in
//! place, so an index read before another process rewrote the entry still
//! reads the old, valid bytes.
//!
//! A run keeps at most [`OPEN_READERS`] packs open for reading (an LRU of
//! handles, so a strip's column-ordered hits reuse one), plus one transient
//! open per worker thread and the writer's handle while it writes a batch —
//! never one descriptor per pack. `download.rs`'s `FD_RESERVE` counts them.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

/// Pack magic.
pub(super) const MAGIC: [u8; 4] = *b"AETP";
/// The pack version this build reads and writes.
pub(super) const VERSION: u16 = 1;
/// Tiles per pack: 256 consecutive tile rows of one column.
pub(super) const SLOTS: u32 = 256;
/// Bytes before the index: magic, version, zoom, x, yblock.
pub(super) const HEADER_LEN: u64 = 18;
/// Bytes per index entry.
pub(super) const ENTRY_LEN: u64 = 20;
/// Where the data region starts.
pub(super) const DATA_START: u64 = HEADER_LEN + SLOTS as u64 * ENTRY_LEN;

/// Index entry status: nothing cached for this tile.
#[cfg_attr(not(test), allow(dead_code))]
pub(super) const STATUS_UNKNOWN: u8 = 0;
/// Index entry status: the body the service answered with (HTTP 200).
pub(super) const STATUS_PRESENT: u8 = 1;
/// Index entry status: the service answered HTTP 404 — no tile here.
pub(super) const STATUS_NO_TILE: u8 = 2;

/// How long the writer keeps trying to lock a pack another process holds
/// before this run stops writing that pack (reported once per run).
pub(super) const LOCK_TIMEOUT: Duration = Duration::from_secs(10);
/// Pause between two attempts to lock a busy pack.
const LOCK_RETRY: Duration = Duration::from_millis(20);
/// Raw bodies queued for the writer, at most (bytes). A tile that would
/// overrun it waits for room, so a cache disk slower than the network slows
/// the download instead of filling the RAM.
const WRITE_QUEUE_BYTES: usize = 64 << 20;
/// Packs a run keeps open for reading (least recently used dropped first).
/// Opening is what a hit costs on many filesystems — measured on a WSL2 9p
/// mount: 4.6 ms per open + close against 0.8 ms to read an 83 KB body — and
/// a run reads each pack up to 256 times; the run hands a strip's tiles out
/// column by column (one pack after the other) when it has a cache, so a
/// handful of handles serves nearly every hit. Few enough to stay well
/// inside the descriptors a run keeps in reserve beside its connections.
const OPEN_READERS: usize = 16;

// -- Checksums ------------------------------------------------------------------

/// The CRC-32 lookup table (IEEE 802.3, reflected polynomial 0xEDB88320).
const CRC_TABLE: [u32; 256] = {
    let mut t = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
            k += 1;
        }
        t[i] = c;
        i += 1;
    }
    t
};

/// CRC-32 (IEEE; zlib's `crc32`, Python's `zlib.crc32`) of `data`.
pub(super) fn crc32(data: &[u8]) -> u32 {
    let mut c = !0u32;
    for &b in data {
        c = CRC_TABLE[((c ^ b as u32) & 0xFF) as usize] ^ (c >> 8);
    }
    !c
}

/// SHA-256 (FIPS 180-4) of `msg` — once per run, for the service key, so
/// plain and short beats fast.
pub(super) fn sha256(msg: &[u8]) -> [u8; 32] {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
        0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
        0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
        0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
        0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
        0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
        0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
    ];
    let mut data = msg.to_vec();
    data.push(0x80);
    while data.len() % 64 != 56 {
        data.push(0);
    }
    data.extend_from_slice(&((msg.len() as u64).wrapping_mul(8)).to_be_bytes());
    for block in data.chunks_exact(64) {
        let mut w = [0u32; 64];
        for (i, c) in block.chunks_exact(4).enumerate() {
            w[i] = u32::from_be_bytes([c[0], c[1], c[2], c[3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16].wrapping_add(s0).wrapping_add(w[i - 7]).wrapping_add(s1);
        }
        let mut v = h;
        for i in 0..64 {
            let s1 = v[4].rotate_right(6) ^ v[4].rotate_right(11) ^ v[4].rotate_right(25);
            let ch = (v[4] & v[5]) ^ (!v[4] & v[6]);
            let t1 = v[7].wrapping_add(s1).wrapping_add(ch).wrapping_add(K[i]).wrapping_add(w[i]);
            let s0 = v[0].rotate_right(2) ^ v[0].rotate_right(13) ^ v[0].rotate_right(22);
            let maj = (v[0] & v[1]) ^ (v[0] & v[2]) ^ (v[1] & v[2]);
            let t2 = s0.wrapping_add(maj);
            v = [t1.wrapping_add(t2), v[0], v[1], v[2], v[3].wrapping_add(t1), v[4], v[5], v[6]];
        }
        for (a, b) in h.iter_mut().zip(v) {
            *a = a.wrapping_add(b);
        }
    }
    let mut out = [0u8; 32];
    for (o, word) in out.chunks_exact_mut(4).zip(h) {
        o.copy_from_slice(&word.to_be_bytes());
    }
    out
}

/// The template as `service.json` records it: without its query string —
/// everything from the first `?` on removed; the path, `{z}/{x}/{y}`
/// placeholders included, stays. API keys and tokens ride in the query
/// (`?key=…`, `?access_token=…`), so none reaches the disk in clear text (the
/// directory name, [`service_key`], is a hash). A key placed in the path is
/// part of what is recorded.
pub(super) fn service_template(url_template: &str) -> &str {
    url_template.split_once('?').map_or(url_template, |(path, _)| path)
}

/// The cache's directory name for one tile service: the first 16 hex digits
/// (lower case) of SHA-256 over the FULL `url_template` — query string
/// included — a newline and the encoding in lower case. Templates that differ
/// only in their query (`?layer=a` / `?layer=b`) can therefore never read each
/// other's tiles; two API keys for one service cost two directories, not
/// correctness. Python:
/// `hashlib.sha256(f"{url_template}\n{encoding.lower()}".encode()).hexdigest()[:16]`.
pub(super) fn service_key(url_template: &str, encoding: &str) -> String {
    let text = format!("{url_template}\n{}", encoding.to_lowercase());
    sha256(text.as_bytes())[..8].iter().map(|b| format!("{b:02x}")).collect()
}

// -- The pack format ------------------------------------------------------------

/// A pack: `(zoom, x, yblock)`.
pub(super) type PackKey = (u32, u32, u32);

/// The 18 header bytes of the pack `key`.
fn header(key: PackKey) -> [u8; HEADER_LEN as usize] {
    let (zoom, x, yblock) = key;
    let mut h = [0u8; HEADER_LEN as usize];
    h[0..4].copy_from_slice(&MAGIC);
    h[4..6].copy_from_slice(&VERSION.to_le_bytes());
    h[6..10].copy_from_slice(&zoom.to_le_bytes());
    h[10..14].copy_from_slice(&x.to_le_bytes());
    h[14..18].copy_from_slice(&yblock.to_le_bytes());
    h
}

/// One index entry.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Entry {
    pub offset: u64,
    pub len: u32,
    pub status: u8,
    pub crc: u32,
}

impl Entry {
    fn parse(b: &[u8]) -> Self {
        Entry {
            offset: u64::from_le_bytes(b[0..8].try_into().expect("8 bytes")),
            len: u32::from_le_bytes(b[8..12].try_into().expect("4 bytes")),
            status: b[12],
            crc: u32::from_le_bytes(b[16..20].try_into().expect("4 bytes")),
        }
    }

    fn bytes(&self) -> [u8; ENTRY_LEN as usize] {
        let mut b = [0u8; ENTRY_LEN as usize];
        b[0..8].copy_from_slice(&self.offset.to_le_bytes());
        b[8..12].copy_from_slice(&self.len.to_le_bytes());
        b[12] = self.status;
        b[16..20].copy_from_slice(&self.crc.to_le_bytes());
        b
    }

    /// Whether this entry of a pack `file_len` bytes long can be a hit (a
    /// present body's CRC is checked when it is read).
    fn usable(&self, file_len: u64) -> bool {
        match self.status {
            STATUS_PRESENT => {
                self.len > 0
                    && self.offset >= DATA_START
                    && self.offset.checked_add(self.len as u64).is_some_and(|end| end <= file_len)
            }
            STATUS_NO_TILE => self.len == 0 && self.crc == 0,
            _ => false,
        }
    }
}

/// What the cache holds for one tile.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Cached {
    /// The body the service answered with, as it arrived.
    Body(Vec<u8>),
    /// The service answered HTTP 404: there is no tile here.
    NoTile,
}

/// The header-checked index of the pack at `path` (256 entries) and the
/// pack's length, or `None` when it is absent, unreadable or not a
/// version-1 pack of exactly this column.
fn read_index(path: &Path, key: PackKey) -> Option<(Vec<Entry>, u64)> {
    read_index_from(&File::open(path).ok()?, key)
}

/// The body `e` points to in the pack at `path`, if it is all there and its
/// CRC matches.
#[cfg_attr(any(unix, windows), allow(dead_code))] // tests; runs read through shared handles
fn read_body(path: &Path, e: &Entry) -> Option<Vec<u8>> {
    let f = File::open(path).ok()?;
    read_body_from(&f, e)
}

/// [`read_body`] through an open pack.
fn read_body_from(f: &File, e: &Entry) -> Option<Vec<u8>> {
    let mut body = vec![0u8; e.len as usize];
    read_exact_at(f, &mut body, e.offset).ok()?;
    (crc32(&body) == e.crc).then_some(body)
}

/// [`read_index`] through an open pack.
fn read_index_from(f: &File, key: PackKey) -> Option<(Vec<Entry>, u64)> {
    let file_len = f.metadata().ok()?.len();
    if file_len < DATA_START {
        return None;
    }
    let mut buf = vec![0u8; DATA_START as usize];
    read_exact_at(f, &mut buf, 0).ok()?;
    if buf[..HEADER_LEN as usize] != header(key) {
        return None;
    }
    let entries = buf[HEADER_LEN as usize..].chunks_exact(ENTRY_LEN as usize).map(Entry::parse).collect();
    Some((entries, file_len))
}

/// Read exactly `buf.len()` bytes at `offset` without a shared file cursor
/// (unix `pread`, Windows `ReadFile` at an offset), so threads can read
/// through one handle at once. Elsewhere through the handle's own cursor —
/// handles are never shared there ([`TileCache::reader`]).
fn read_exact_at(f: &File, buf: &mut [u8], offset: u64) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileExt;
        f.read_exact_at(buf, offset)
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::FileExt;
        let mut done = 0;
        while done < buf.len() {
            match f.seek_read(&mut buf[done..], offset + done as u64) {
                Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
                Ok(n) => done += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }
    #[cfg(not(any(unix, windows)))]
    {
        let mut r = f;
        r.seek(SeekFrom::Start(offset))?;
        r.read_exact(buf)
    }
}

/// Why [`write_pack`] wrote nothing.
#[derive(Debug)]
pub(super) enum WriteError {
    /// Another writer holds the pack's lock.
    Busy,
    /// The file is a pack of another version; it is left as it is.
    Incompatible(u16),
    Io(io::Error),
}

impl From<io::Error> for WriteError {
    fn from(e: io::Error) -> Self {
        WriteError::Io(e)
    }
}

/// Write `items` — `(index slot, answer)`, a later slot repeat winning — into
/// the pack `key` at `path`, creating it when absent: lock, append every
/// body in one write, then write their index entries in place, close
/// (which releases the lock). [`WriteError::Busy`] when another writer holds
/// the lock; nothing is written then.
pub(super) fn write_pack(path: &Path, key: PackKey, items: &[(usize, &Cached)]) -> Result<(), WriteError> {
    let Some(mut f) = lock::open_locked(path)? else {
        return Err(WriteError::Busy);
    };
    let len = f.metadata()?.len();
    let want = header(key);
    let fresh = if len < DATA_START {
        true
    } else {
        let mut have = [0u8; HEADER_LEN as usize];
        f.seek(SeekFrom::Start(0))?;
        f.read_exact(&mut have)?;
        let version = u16::from_le_bytes([have[4], have[5]]);
        if have == want {
            false
        } else if have[0..4] == MAGIC && version != VERSION {
            return Err(WriteError::Incompatible(version));
        } else {
            // Not a pack of this column: a creation cut short (a crash between
            // creating the file and writing its header) or a damaged header.
            // Its entries cannot be trusted, so it starts over.
            true
        }
    };
    if fresh {
        f.set_len(0)?;
        let mut init = vec![0u8; DATA_START as usize];
        init[..HEADER_LEN as usize].copy_from_slice(&want);
        f.seek(SeekFrom::Start(0))?;
        f.write_all(&init)?;
    }

    let end = f.seek(SeekFrom::End(0))?;
    let mut blob: Vec<u8> = Vec::new();
    let mut entries: BTreeMap<usize, Entry> = BTreeMap::new();
    for &(slot, item) in items {
        if slot >= SLOTS as usize {
            continue;
        }
        let e = match item {
            Cached::Body(b) => {
                let Ok(n) = u32::try_from(b.len()) else { continue };
                let e = Entry { offset: end + blob.len() as u64, len: n, status: STATUS_PRESENT, crc: crc32(b) };
                blob.extend_from_slice(b);
                e
            }
            Cached::NoTile => Entry { offset: 0, len: 0, status: STATUS_NO_TILE, crc: 0 },
        };
        entries.insert(slot, e);
    }
    // 1. The bodies, appended in one write ...
    if !blob.is_empty() {
        f.write_all(&blob)?;
    }
    // 2. ... then the entries that point to them.
    for (slot, e) in entries {
        f.seek(SeekFrom::Start(HEADER_LEN + slot as u64 * ENTRY_LEN))?;
        f.write_all(&e.bytes())?;
    }
    Ok(())
}

/// The pack write lock: one writer per pack at a time, across processes and
/// across the threads of one process; readers never take it. Released when
/// the returned file is closed — and by the OS when a process dies, so no
/// lock outlives its writer.
pub(super) mod lock {
    use std::fs::File;
    use std::io;
    use std::path::Path;

    /// Open the pack at `path` read-write (created when absent) holding its
    /// write lock; `Ok(None)` when another writer holds it.
    ///
    /// unix (Linux, macOS): `flock(LOCK_EX | LOCK_NB)` on the pack itself —
    /// advisory, so readers are untouched, and per open file description, so
    /// two threads of one process exclude each other too.
    #[cfg(unix)]
    pub(in super::super) fn open_locked(path: &Path) -> io::Result<Option<File>> {
        use std::os::unix::io::AsRawFd;
        let f = std::fs::OpenOptions::new().read(true).write(true).create(true).open(path)?;
        loop {
            // SAFETY: flock only operates on the descriptor `f` keeps open.
            if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                return Ok(Some(f));
            }
            let e = io::Error::last_os_error();
            match e.kind() {
                io::ErrorKind::Interrupted => continue,
                io::ErrorKind::WouldBlock => return Ok(None),
                _ => return Err(e),
            }
        }
    }

    /// Windows: the pack opened for writing with share mode `FILE_SHARE_READ`
    /// only. While that handle is open the OS refuses every other open that
    /// asks for write access (`ERROR_SHARING_VIOLATION`), and allows every
    /// reader (readers open with the default share mode, which allows
    /// writers). `LockFileEx` byte-range locks are not used: they are
    /// mandatory on Windows, so a locked range would fail the lock-free
    /// readers, and they need a `windows-sys` feature this crate does not
    /// enable.
    #[cfg(windows)]
    pub(in super::super) fn open_locked(path: &Path) -> io::Result<Option<File>> {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_SHARE_READ: u32 = 0x0000_0001;
        const ERROR_SHARING_VIOLATION: i32 = 32;
        const ERROR_LOCK_VIOLATION: i32 = 33;
        match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .share_mode(FILE_SHARE_READ)
            .open(path)
        {
            Ok(f) => Ok(Some(f)),
            Err(e) if matches!(e.raw_os_error(), Some(ERROR_SHARING_VIOLATION | ERROR_LOCK_VIOLATION)) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Any other platform: no lock, so nothing is written (the cache is
    /// read-only there).
    #[cfg(not(any(unix, windows)))]
    pub(in super::super) fn open_locked(_path: &Path) -> io::Result<Option<File>> {
        Err(io::Error::new(io::ErrorKind::Unsupported, "no pack lock on this platform"))
    }
}

// -- The per-run cache ---------------------------------------------------------

/// What the run counts, for `[Stats] Cache:` (see [`Summary`]).
#[derive(Default)]
struct Counts {
    hits: AtomicUsize,
    fetched: AtomicUsize,
    written: AtomicUsize,
    parent_hits: AtomicUsize,
    parent_fetched: AtomicUsize,
    parent_written: AtomicUsize,
    /// Answers the writer dropped (pack locked past the timeout, I/O error).
    dropped: AtomicUsize,
}

/// A use of the cache [`TileCache::count`] tallies.
#[derive(Clone, Copy, Debug)]
pub(super) enum Use {
    /// A source tile of the run's fetch set answered from the cache.
    Hit,
    /// A source tile of the fetch set that went to the network.
    Fetched,
    /// A coarser tile the parent fill needed, answered from the cache.
    ParentHit,
    /// A coarser tile the parent fill fetched.
    ParentFetched,
}

/// The run's cache totals.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Summary {
    pub hits: usize,
    pub fetched: usize,
    pub written: usize,
    pub parent_hits: usize,
    pub parent_fetched: usize,
    pub parent_written: usize,
    pub dropped: usize,
}

impl Summary {
    /// The `[Stats]` lines (additive, informational): fetch-set tiles always,
    /// the parent fill's coarser tiles when it used the cache.
    pub(super) fn lines(&self, cache_dir: &Path) -> Vec<String> {
        let mut out = vec![format!(
            "[Stats] Cache: {} hits, {} fetched, {} written to {}",
            self.hits, self.fetched, self.written, cache_dir.display()
        )];
        if self.parent_hits + self.parent_fetched > 0 {
            out.push(format!(
                "[Stats] Cache, coarser parents: {} hits, {} fetched, {} written",
                self.parent_hits, self.parent_fetched, self.parent_written
            ));
        }
        out
    }
}

/// One answer on its way to the writer.
struct Job {
    key: PackKey,
    slot: usize,
    item: Cached,
    parent: bool,
    /// Its share of [`WRITE_QUEUE_BYTES`].
    bytes: usize,
}

/// The raw source-tile cache of one run: lookups from any thread, writes
/// through one writer thread.
pub(super) struct TileCache {
    /// `cache_dir` as the job gave it (messages).
    dir: PathBuf,
    /// `<cache_dir>/<service_key>`.
    root: PathBuf,
    tx: Mutex<Option<mpsc::Sender<Job>>>,
    writer: Mutex<Option<std::thread::JoinHandle<()>>>,
    queue: Arc<tokio::sync::Semaphore>,
    counts: Arc<Counts>,
    /// Packs open for reading, most recently used last ([`OPEN_READERS`]).
    readers: Mutex<std::collections::VecDeque<(PackKey, Arc<File>)>>,
}

impl TileCache {
    /// Open (creating as needed) the cache of tile service `url_template` /
    /// `encoding` under `dir` and start its writer; the error is the reason
    /// for the `[Warn] cache_dir …` line (the run then goes without cache).
    pub(super) fn open(dir: &Path, url_template: &str, encoding: &str) -> Result<Arc<Self>, String> {
        Self::open_with(dir, url_template, encoding, LOCK_TIMEOUT)
    }

    /// [`Self::open`] with its own lock timeout (tests).
    pub(super) fn open_with(
        dir: &Path, url_template: &str, encoding: &str, lock_timeout: Duration,
    ) -> Result<Arc<Self>, String> {
        let encoding = encoding.to_lowercase();
        let root = dir.join(service_key(url_template, &encoding));
        fs::create_dir_all(&root).map_err(|e| e.to_string())?;
        // Informational, for people looking into the directory; written once
        // (a concurrent first run's copy is as good as ours). The template
        // without its query string, so without a key in clear text (the
        // directory name hashes the full template).
        let info = root.join("service.json");
        match OpenOptions::new().write(true).create_new(true).open(&info) {
            Ok(mut f) => {
                let text = serde_json::json!({
                    "url_template": service_template(url_template), "encoding": encoding,
                });
                let mut body = serde_json::to_string_pretty(&text).map_err(|e| e.to_string())?;
                body.push('\n');
                f.write_all(body.as_bytes()).map_err(|e| e.to_string())?;
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e.to_string()),
        }
        let (tx, rx) = mpsc::channel();
        let queue = Arc::new(tokio::sync::Semaphore::new(WRITE_QUEUE_BYTES));
        let counts = Arc::new(Counts::default());
        let ctx = WriterCtx {
            dir: dir.to_path_buf(),
            root: root.clone(),
            queue: queue.clone(),
            counts: counts.clone(),
            lock_timeout,
        };
        let writer = std::thread::Builder::new()
            .name("tile-cache-writer".into())
            .spawn(move || writer(rx, ctx))
            .map_err(|e| e.to_string())?;
        Ok(Arc::new(Self {
            dir: dir.to_path_buf(),
            root,
            tx: Mutex::new(Some(tx)),
            writer: Mutex::new(Some(writer)),
            queue,
            counts,
            readers: Mutex::new(std::collections::VecDeque::new()),
        }))
    }

    /// The pack `key` open for reading: one of the run's kept handles, or
    /// opened now and kept (dropping the least recently used beyond
    /// [`OPEN_READERS`]; a dropped handle closes once no read uses it).
    /// Handles see what other writers append; a pack that was started over
    /// meanwhile simply fails the reads that follow (CRC, length) — misses.
    fn reader(&self, key: PackKey) -> Option<Arc<File>> {
        #[cfg(not(any(unix, windows)))]
        {
            // No positioned reads here, so no handle is shared.
            return File::open(self.pack_path(key)).ok().map(Arc::new);
        }
        #[cfg(any(unix, windows))]
        {
            if let Ok(mut r) = self.readers.lock() {
                if let Some(i) = r.iter().position(|(k, _)| *k == key) {
                    let kept = r.remove(i).expect("found");
                    let f = kept.1.clone();
                    r.push_back(kept);
                    return Some(f);
                }
            }
            // Opened outside the lock: an open can take milliseconds.
            let f = Arc::new(File::open(self.pack_path(key)).ok()?);
            if let Ok(mut r) = self.readers.lock() {
                if let Some((_, other)) = r.iter().find(|(k, _)| *k == key) {
                    return Some(other.clone());
                }
                r.push_back((key, f.clone()));
                while r.len() > OPEN_READERS {
                    r.pop_front();
                }
            }
            Some(f)
        }
    }

    /// `cache_dir` as the job gave it.
    pub(super) fn dir(&self) -> &Path {
        &self.dir
    }

    fn pack_path(&self, key: PackKey) -> PathBuf {
        pack_path(&self.root, key)
    }

    /// The index entries that can be hits among `tiles` (`(x, y)` at
    /// `zoom`), each pack read once — the run's lookup before any request.
    pub(super) fn index(
        &self, zoom: u32, tiles: impl IntoIterator<Item = (u32, u32)>,
    ) -> HashMap<(u32, u32), Entry> {
        let mut out = HashMap::new();
        if !self.root.join(format!("z{zoom}")).is_dir() {
            return out;
        }
        let mut by_pack: BTreeMap<(u32, u32), Vec<u32>> = BTreeMap::new();
        for (x, y) in tiles {
            by_pack.entry((x, y / SLOTS)).or_default().push(y);
        }
        for ((x, yblock), ys) in by_pack {
            let key = (zoom, x, yblock);
            let Some((entries, len)) = read_index(&self.pack_path(key), key) else { continue };
            for y in ys {
                let e = entries[(y % SLOTS) as usize];
                if e.usable(len) {
                    out.insert((x, y), e);
                }
            }
        }
        out
    }

    /// The cached answer of tile `(zoom, x, y)` whose index entry is `e`, or
    /// `None` (a miss) when its body is no longer all there or fails its CRC.
    pub(super) fn read(&self, zoom: u32, x: u32, y: u32, e: &Entry) -> Option<Cached> {
        match e.status {
            STATUS_NO_TILE => Some(Cached::NoTile),
            STATUS_PRESENT => {
                let f = self.reader((zoom, x, y / SLOTS))?;
                read_body_from(&f, e).map(Cached::Body)
            }
            _ => None,
        }
    }

    /// The cached answer of tile `(zoom, x, y)`, looked up now (the parent
    /// fill's coarser tiles).
    pub(super) fn lookup(&self, zoom: u32, x: u32, y: u32) -> Option<Cached> {
        let key = (zoom, x, y / SLOTS);
        let f = self.reader(key)?;
        let (entries, len) = read_index_from(&f, key)?;
        let e = entries[(y % SLOTS) as usize];
        if !e.usable(len) {
            return None;
        }
        match e.status {
            STATUS_NO_TILE => Some(Cached::NoTile),
            _ => read_body_from(&f, &e).map(Cached::Body),
        }
    }

    pub(super) fn count(&self, u: Use) {
        let c = &self.counts;
        match u {
            Use::Hit => c.hits.fetch_add(1, Ordering::Relaxed),
            Use::Fetched => c.fetched.fetch_add(1, Ordering::Relaxed),
            Use::ParentHit => c.parent_hits.fetch_add(1, Ordering::Relaxed),
            Use::ParentFetched => c.parent_fetched.fetch_add(1, Ordering::Relaxed),
        };
    }

    /// Hand the answer for tile `(zoom, x, y)` to the writer (`parent`: a
    /// coarser tile of the parent fill). Waits while the write queue is full;
    /// after [`Self::finish`] the answer is dropped.
    pub(super) async fn store(&self, zoom: u32, x: u32, y: u32, item: Cached, parent: bool) {
        let bytes = match &item {
            Cached::Body(b) => b.len(),
            Cached::NoTile => 0,
        }
        .clamp(1, WRITE_QUEUE_BYTES);
        let Ok(permit) = self.queue.acquire_many(bytes as u32).await else { return };
        permit.forget();
        let job = Job { key: (zoom, x, y / SLOTS), slot: (y % SLOTS) as usize, item, parent, bytes };
        let sent = match self.tx.lock() {
            Ok(tx) => tx.as_ref().is_some_and(|tx| tx.send(job).is_ok()),
            Err(_) => false,
        };
        if !sent {
            self.queue.add_permits(bytes);
        }
    }

    /// Stop taking answers, wait until the writer has written (or given up
    /// on) every queued one, and return the run's totals.
    pub(super) fn finish(&self) -> Summary {
        drop(self.tx.lock().ok().and_then(|mut tx| tx.take()));
        if let Some(h) = self.writer.lock().ok().and_then(|mut w| w.take()) {
            let _ = h.join();
        }
        let c = &self.counts;
        let n = |a: &AtomicUsize| a.load(Ordering::Relaxed);
        Summary {
            hits: n(&c.hits),
            fetched: n(&c.fetched),
            written: n(&c.written),
            parent_hits: n(&c.parent_hits),
            parent_fetched: n(&c.parent_fetched),
            parent_written: n(&c.parent_written),
            dropped: n(&c.dropped),
        }
    }
}

/// `<root>/z<zoom>/x<x>_y<yblock>.pack`.
pub(super) fn pack_path(root: &Path, (zoom, x, yblock): PackKey) -> PathBuf {
    root.join(format!("z{zoom}")).join(format!("x{x}_y{yblock}.pack"))
}

/// What the writer thread works with.
struct WriterCtx {
    dir: PathBuf,
    root: PathBuf,
    queue: Arc<tokio::sync::Semaphore>,
    counts: Arc<Counts>,
    lock_timeout: Duration,
}

/// Closes the write queue when the writer ends — also by a panic — so no
/// tile ever waits for room that will not come.
struct CloseQueue(Arc<tokio::sync::Semaphore>);

impl Drop for CloseQueue {
    fn drop(&mut self) {
        self.0.close();
    }
}

/// The answers waiting for one pack.
struct Pending {
    jobs: Vec<Job>,
    /// When its lock was first found taken (`None`: not busy).
    busy_since: Option<Instant>,
    retry_at: Instant,
}

/// The writer thread: collects queued answers per pack and writes each
/// pack's batch under its lock. A pack another writer holds is retried
/// every [`LOCK_RETRY`] while the other packs are written; one still locked
/// after the lock timeout is not written by this run (reported once). An
/// I/O error likewise ends this run's writes to that pack (reported once).
fn writer(rx: mpsc::Receiver<Job>, ctx: WriterCtx) {
    let _close = CloseQueue(ctx.queue.clone());
    let mut pending: HashMap<PackKey, Pending> = HashMap::new();
    let mut given_up: HashSet<PackKey> = HashSet::new();
    let mut zoom_dirs: HashSet<u32> = HashSet::new();
    let (warned_lock, warned_io, warned_version) = (AtomicBool::new(false), AtomicBool::new(false), AtomicBool::new(false));
    let warn_once = |flag: &AtomicBool, line: String| {
        if !flag.swap(true, Ordering::Relaxed) {
            eprintln!("{line}");
        }
    };
    let release = |jobs: &[Job]| ctx.queue.add_permits(jobs.iter().map(|j| j.bytes).sum());
    let drop_jobs = |jobs: &[Job]| {
        ctx.counts.dropped.fetch_add(jobs.len(), Ordering::Relaxed);
        release(jobs);
    };
    let mut open = true;
    loop {
        // 1. Take what is queued: block while idle, else until the next
        //    retry of a busy pack is due.
        let mut got: Vec<Job> = Vec::new();
        let next_retry = pending.values().map(|p| p.retry_at).min();
        match (open, next_retry) {
            (false, None) => break,
            (true, None) => match rx.recv() {
                Ok(j) => got.push(j),
                Err(_) => open = false,
            },
            (true, Some(at)) => match rx.recv_timeout(at.saturating_duration_since(Instant::now())) {
                Ok(j) => got.push(j),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => open = false,
            },
            (false, Some(at)) => std::thread::sleep(at.saturating_duration_since(Instant::now())),
        }
        while let Ok(j) = rx.try_recv() {
            got.push(j);
        }
        let now = Instant::now();
        for j in got {
            if given_up.contains(&j.key) {
                drop_jobs(std::slice::from_ref(&j));
                continue;
            }
            pending
                .entry(j.key)
                .or_insert_with(|| Pending { jobs: Vec::new(), busy_since: None, retry_at: now })
                .jobs
                .push(j);
        }

        // 2. Write every pack that is due.
        let due: Vec<PackKey> = pending.iter().filter(|(_, p)| p.retry_at <= now).map(|(k, _)| *k).collect();
        for key in due {
            let Some(mut p) = pending.remove(&key) else { continue };
            let path = pack_path(&ctx.root, key);
            if zoom_dirs.insert(key.0) {
                if let Err(e) = fs::create_dir_all(path.parent().expect("a pack has a directory")) {
                    zoom_dirs.remove(&key.0);
                    warn_once(&warned_io, format!(
                        "[Warn] cache_dir {}: cannot write {} ({e}); this run does not write that pack \
                         (reported once per run)", ctx.dir.display(), path.display()));
                    given_up.insert(key);
                    drop_jobs(&p.jobs);
                    continue;
                }
            }
            let items: Vec<(usize, &Cached)> = p.jobs.iter().map(|j| (j.slot, &j.item)).collect();
            match write_pack(&path, key, &items) {
                Ok(()) => {
                    let parents = p.jobs.iter().filter(|j| j.parent).count();
                    ctx.counts.written.fetch_add(p.jobs.len() - parents, Ordering::Relaxed);
                    ctx.counts.parent_written.fetch_add(parents, Ordering::Relaxed);
                    release(&p.jobs);
                }
                Err(WriteError::Busy) => {
                    let since = *p.busy_since.get_or_insert(now);
                    if now.duration_since(since) >= ctx.lock_timeout {
                        warn_once(&warned_lock, format!(
                            "[Warn] cache_dir {}: {} stayed locked by another writer for {} s; this run \
                             does not write that pack (reported once per run)",
                            ctx.dir.display(), path.display(), ctx.lock_timeout.as_secs_f64()));
                        given_up.insert(key);
                        drop_jobs(&p.jobs);
                    } else {
                        p.retry_at = now + LOCK_RETRY;
                        pending.insert(key, p);
                    }
                }
                Err(WriteError::Incompatible(v)) => {
                    warn_once(&warned_version, format!(
                        "[Warn] cache_dir {}: {} is a version-{v} pack, which this build does not write; \
                         left as it is (reported once per run)", ctx.dir.display(), path.display()));
                    given_up.insert(key);
                    drop_jobs(&p.jobs);
                }
                Err(WriteError::Io(e)) => {
                    warn_once(&warned_io, format!(
                        "[Warn] cache_dir {}: cannot write {} ({e}); this run does not write that pack \
                         (reported once per run)", ctx.dir.display(), path.display()));
                    given_up.insert(key);
                    drop_jobs(&p.jobs);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(seed: u8, n: usize) -> Vec<u8> {
        (0..n).map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed)).collect()
    }

    fn put(path: &Path, key: PackKey, items: &[(usize, Cached)]) {
        let refs: Vec<(usize, &Cached)> = items.iter().map(|(s, c)| (*s, c)).collect();
        write_pack(path, key, &refs).expect("write");
    }

    fn get(path: &Path, key: PackKey, slot: usize) -> Option<Cached> {
        let (entries, len) = read_index(path, key)?;
        let e = entries[slot];
        if !e.usable(len) {
            return None;
        }
        match e.status {
            STATUS_NO_TILE => Some(Cached::NoTile),
            _ => read_body(path, &e).map(Cached::Body),
        }
    }

    fn entry_at(bytes: &[u8], slot: usize) -> Entry {
        let o = (HEADER_LEN + slot as u64 * ENTRY_LEN) as usize;
        Entry::parse(&bytes[o..o + ENTRY_LEN as usize])
    }

    #[test]
    fn the_crc_is_the_ieee_one() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926, "the CRC-32 check value");
        assert_eq!(crc32(b""), 0);
        assert_eq!(crc32(b"The quick brown fox jumps over the lazy dog"), 0x414F_A339);
    }

    #[test]
    fn sha256_matches_fips_180_4_and_the_service_key_is_its_first_16_hex_digits() {
        let hex = |d: [u8; 32]| d.iter().map(|b| format!("{b:02x}")).collect::<String>();
        assert_eq!(hex(sha256(b"")), "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
        assert_eq!(hex(sha256(b"abc")), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        assert_eq!(
            hex(sha256(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq")),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
        // 1 000 000 × 'a' spans many blocks.
        assert_eq!(
            hex(sha256(&vec![b'a'; 1_000_000])),
            "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
        );
        // The worked example of CONTRACT §9b (Python: hashlib, see service_key).
        let aws = "https://s3.amazonaws.com/elevation-tiles-prod/terrarium/{z}/{x}/{y}.png";
        assert_eq!(service_key(aws, "terrarium"), "54d18ad3dc1a2f05");
        assert_eq!(service_key(aws, "Terrarium"), "54d18ad3dc1a2f05", "the encoding is lower-cased");
        assert_ne!(service_key(aws, "mapbox"), service_key(aws, "terrarium"));
    }

    #[test]
    fn every_query_has_its_own_directory_and_no_key_reaches_the_disk() {
        let base = "https://api.maptiler.com/tiles/terrain-rgb-v2/{z}/{x}/{y}.webp";
        let one = format!("{base}?key=AAAAsecret1");
        let two = format!("{base}?key=BBBBsecret2&lang=de");
        assert_eq!(service_template(&one), base);
        assert_eq!(service_template(&two), base);
        assert_eq!(service_template("http://h/{z}/{x}/{y}.png?a=1?b=2"), "http://h/{z}/{x}/{y}.png",
                   "everything from the FIRST `?` on");
        assert_eq!(service_template("http://h/{z}/{x}/{y}.png?"), "http://h/{z}/{x}/{y}.png");
        // The key hashes the FULL template (Python:
        // hashlib.sha256(f"{t}\n{e.lower()}".encode()).hexdigest()[:16]):
        // keyed templates get different directories ...
        assert_eq!(service_key(&one, "mapbox"), "4504d9c9fa773f2a");
        assert_eq!(service_key(&two, "MapBox"), "82f8f69eddd92b3f");
        assert_eq!(service_key(base, "mapbox"), "b08cc9d0b8473a70");
        // ... and datasets told apart only by a query parameter never share one.
        assert_eq!(service_key("https://h/tile/{z}/{x}/{y}.png?layer=a", "terrarium"), "e25a266d97d303c0");
        assert_eq!(service_key("https://h/tile/{z}/{x}/{y}.png?layer=b", "terrarium"), "05a29990a41f820f");
        // A template without a query is unchanged (the CONTRACT §9b example).
        let aws = "https://s3.amazonaws.com/elevation-tiles-prod/terrarium/{z}/{x}/{y}.png";
        assert_eq!(service_template(aws), aws);
        assert_eq!(service_key(aws, "terrarium"), "54d18ad3dc1a2f05");

        // On disk: one directory per keyed template, and each service.json
        // records the template without its query — no `?`, no key.
        let dir = tempfile::tempdir().unwrap();
        for t in [&one, &two] {
            TileCache::open(dir.path(), t, "mapbox").unwrap().finish();
        }
        let mut names: Vec<String> = fs::read_dir(dir.path()).unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        names.sort();
        assert_eq!(names, ["4504d9c9fa773f2a", "82f8f69eddd92b3f"]);
        for name in names {
            let raw = fs::read_to_string(dir.path().join(name).join("service.json")).unwrap();
            assert!(!raw.contains('?') && !raw.contains("secret"), "{raw}");
            let info: serde_json::Value = serde_json::from_str(&raw).unwrap();
            assert_eq!(info, serde_json::json!({ "url_template": base, "encoding": "mapbox" }));
        }
    }

    #[test]
    fn a_pack_round_trips_bodies_and_404s_in_the_documented_layout() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x17_y3.pack");
        let key = (15, 17, 3);
        let (a, b) = (body(1, 1000), body(2, 77));
        put(&path, key, &[(0, Cached::Body(a.clone())), (5, Cached::NoTile), (255, Cached::Body(b.clone()))]);

        assert_eq!(get(&path, key, 0), Some(Cached::Body(a.clone())));
        assert_eq!(get(&path, key, 5), Some(Cached::NoTile));
        assert_eq!(get(&path, key, 255), Some(Cached::Body(b.clone())));
        assert_eq!(get(&path, key, 1), None, "an unknown slot is a miss");

        // The bytes, field by field, as CONTRACT §9b documents them.
        let raw = fs::read(&path).unwrap();
        assert_eq!(DATA_START, 5138);
        assert_eq!(&raw[0..4], b"AETP");
        assert_eq!(u16::from_le_bytes([raw[4], raw[5]]), 1);
        assert_eq!(u32::from_le_bytes(raw[6..10].try_into().unwrap()), 15);
        assert_eq!(u32::from_le_bytes(raw[10..14].try_into().unwrap()), 17);
        assert_eq!(u32::from_le_bytes(raw[14..18].try_into().unwrap()), 3);
        assert_eq!(raw.len() as u64, DATA_START + 1000 + 77);
        let e0 = entry_at(&raw, 0);
        assert_eq!((e0.offset, e0.len, e0.status, e0.crc), (DATA_START, 1000, 1, crc32(&a)));
        assert_eq!(&raw[18 + 13..18 + 16], &[0, 0, 0], "reserved bytes are zero");
        let e5 = entry_at(&raw, 5);
        assert_eq!((e5.offset, e5.len, e5.status, e5.crc), (0, 0, 2, 0));
        let e255 = entry_at(&raw, 255);
        assert_eq!((e255.offset, e255.len, e255.status), (DATA_START + 1000, 77, 1));
        assert_eq!(&raw[DATA_START as usize..DATA_START as usize + 1000], &a[..]);
        assert_eq!(entry_at(&raw, 1), Entry::default(), "status 0, all zero");
        assert_eq!(entry_at(&raw, 1).status, STATUS_UNKNOWN);

        // A second batch appends after the first; earlier entries stay.
        let c = body(3, 10);
        put(&path, key, &[(7, Cached::Body(c.clone()))]);
        assert_eq!(get(&path, key, 7), Some(Cached::Body(c)));
        assert_eq!(get(&path, key, 0), Some(Cached::Body(a)));
        assert_eq!(fs::metadata(&path).unwrap().len(), DATA_START + 1000 + 77 + 10);

        // Another column's (or zoom's) key does not read this pack.
        assert!(read_index(&path, (15, 18, 3)).is_none());
        assert!(read_index(&path, (14, 17, 3)).is_none());
    }

    #[test]
    fn a_torn_or_cut_entry_is_a_miss_never_a_wrong_tile() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("p.pack");
        let key = (12, 1, 0);
        let a = body(9, 500);
        let fresh = || {
            let _ = fs::remove_file(&path);
            put(&path, key, &[(3, Cached::Body(a.clone())), (4, Cached::Body(a.clone()))]);
            fs::read(&path).unwrap()
        };
        let mutate = |f: &dyn Fn(&mut Vec<u8>)| {
            let mut raw = fresh();
            f(&mut raw);
            fs::write(&path, &raw).unwrap();
        };
        let at = |slot: usize| (HEADER_LEN + slot as u64 * ENTRY_LEN) as usize;

        // The entry write never landed: status 0.
        mutate(&|r| r[at(3) + 12] = 0);
        assert_eq!(get(&path, key, 3), None);
        assert_eq!(get(&path, key, 4), Some(Cached::Body(a.clone())), "the neighbour is intact");
        // A half-written entry: the new offset with a length past the end.
        mutate(&|r| r[at(3) + 8..at(3) + 12].copy_from_slice(&5000u32.to_le_bytes()));
        assert_eq!(get(&path, key, 3), None);
        // An offset into the index.
        mutate(&|r| r[at(3)..at(3) + 8].copy_from_slice(&100u64.to_le_bytes()));
        assert_eq!(get(&path, key, 3), None);
        // The body cut short (a crash during the append that a later entry
        // write still made it out of).
        let mut raw = fresh();
        raw.truncate(raw.len() - 1);
        fs::write(&path, &raw).unwrap();
        assert_eq!(get(&path, key, 4), None);
        assert_eq!(get(&path, key, 3), Some(Cached::Body(a.clone())));
        // A 404 entry that carries a length is no 404.
        mutate(&|r| {
            r[at(3) + 12] = STATUS_NO_TILE;
        });
        assert_eq!(get(&path, key, 3), None);
        // An unknown status.
        mutate(&|r| r[at(3) + 12] = 7);
        assert_eq!(get(&path, key, 3), None);
        // A damaged header: no entry of the pack is read, and the next write
        // starts the pack over.
        mutate(&|r| r[0] = b'X');
        assert_eq!(get(&path, key, 4), None);
        put(&path, key, &[(9, Cached::NoTile)]);
        assert_eq!(get(&path, key, 9), Some(Cached::NoTile));
        assert_eq!(get(&path, key, 4), None, "the old entries were not trusted");
        assert_eq!(fs::metadata(&path).unwrap().len(), DATA_START);
    }

    #[test]
    fn a_crc_mismatch_is_a_miss_and_a_rewrite_appends_and_repoints_the_entry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("p.pack");
        let key = (12, 1, 0);
        let a = body(4, 300);
        put(&path, key, &[(0, Cached::Body(a.clone()))]);
        let mut raw = fs::read(&path).unwrap();
        raw[DATA_START as usize + 17] ^= 0x40;
        fs::write(&path, &raw).unwrap();
        assert_eq!(get(&path, key, 0), None, "one flipped bit");

        put(&path, key, &[(0, Cached::Body(a.clone()))]);
        assert_eq!(get(&path, key, 0), Some(Cached::Body(a)));
        let raw = fs::read(&path).unwrap();
        assert_eq!(raw.len() as u64, DATA_START + 600, "appended; the bad bytes are garbage now");
        assert_eq!(entry_at(&raw, 0).offset, DATA_START + 300);
    }

    #[test]
    fn a_pack_of_another_version_is_neither_read_nor_written() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("p.pack");
        let key = (12, 1, 0);
        put(&path, key, &[(0, Cached::NoTile)]);
        let mut raw = fs::read(&path).unwrap();
        raw[4..6].copy_from_slice(&2u16.to_le_bytes());
        fs::write(&path, &raw).unwrap();
        assert_eq!(get(&path, key, 0), None);
        let r = write_pack(&path, key, &[(1, &Cached::NoTile)]);
        assert!(matches!(r, Err(WriteError::Incompatible(2))), "{r:?}");
        assert_eq!(fs::read(&path).unwrap(), raw, "untouched");
    }

    #[test]
    fn two_threads_writing_one_pack_lose_no_entry() {
        // Two writers of one pack, 20 rounds each of their own 64 slots plus
        // 64 shared ones; each batch locks, so every entry ends up valid
        // and each shared slot holds one of the two writers' bodies.
        let dir = tempfile::tempdir().unwrap();
        let path = Arc::new(dir.path().join("p.pack"));
        let key = (13, 5, 1);
        let busy = Arc::new(AtomicUsize::new(0));
        let spawn = |who: u8| {
            let (path, busy) = (path.clone(), busy.clone());
            std::thread::spawn(move || {
                for round in 0..20u8 {
                    let own = (0..64).map(|i| (who as usize * 64 + i, Cached::Body(body(who, 200 + i))));
                    let shared = (128..192).map(|i| (i, Cached::Body(body(100 + who, 50 + round as usize))));
                    let items: Vec<(usize, Cached)> = own.chain(shared).collect();
                    let refs: Vec<(usize, &Cached)> = items.iter().map(|(s, c)| (*s, c)).collect();
                    loop {
                        match write_pack(&path, key, &refs) {
                            Ok(()) => break,
                            Err(WriteError::Busy) => {
                                busy.fetch_add(1, Ordering::Relaxed);
                                std::thread::sleep(Duration::from_millis(1));
                            }
                            Err(e) => panic!("{e:?}"),
                        }
                    }
                }
            })
        };
        let (t0, t1) = (spawn(0), spawn(1));
        t0.join().unwrap();
        t1.join().unwrap();
        for who in 0..2u8 {
            for i in 0..64 {
                assert_eq!(get(&path, key, who as usize * 64 + i), Some(Cached::Body(body(who, 200 + i))));
            }
        }
        // The batch that landed last wrote all 64 shared slots under one
        // lock: they all hold that writer's last round, none the other's.
        let last = get(&path, key, 128).expect("a shared slot");
        assert!(last == Cached::Body(body(100, 69)) || last == Cached::Body(body(101, 69)), "{last:?}");
        for i in 128..192 {
            let want = match &last {
                Cached::Body(b) if b[0] == 100 => Cached::Body(body(100, 69)),
                _ => Cached::Body(body(101, 69)),
            };
            assert_eq!(get(&path, key, i), Some(want), "slot {i}");
        }
        // Every batch's bytes were appended whole: the file is exactly the
        // header plus the 40 batches (per batch 64 own bodies of 200..264
        // bytes and 64 shared ones of 50 + round).
        let own: usize = (0..64).map(|i| 200 + i).sum();
        let data: usize = 2 * 20 * own + (0..20).map(|r| 2 * 64 * (50 + r)).sum::<usize>();
        assert_eq!(fs::metadata(&*path).unwrap().len(), DATA_START + data as u64);
    }

    #[test]
    fn a_pack_locked_past_the_timeout_is_skipped_and_the_others_are_written() {
        let dir = tempfile::tempdir().unwrap();
        let cache = TileCache::open_with(dir.path(), "http://t/{z}/{x}/{y}.png", "terrarium",
                                         Duration::from_millis(300)).unwrap();
        let root = dir.path().join(service_key("http://t/{z}/{x}/{y}.png", "terrarium"));
        assert!(root.join("service.json").is_file());
        // Another writer holds pack x=1 (the lock is per open file, so a
        // second open in this process stands in for another process).
        fs::create_dir_all(root.join("z12")).unwrap();
        let locked = pack_path(&root, (12, 1, 0));
        let holder = lock::open_locked(&locked).unwrap().expect("free at first");
        assert!(lock::open_locked(&locked).unwrap().is_none(), "the lock excludes a second writer");

        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        let t0 = Instant::now();
        rt.block_on(async {
            cache.store(12, 1, 7, Cached::Body(body(1, 100)), false).await;
            cache.store(12, 2, 7, Cached::Body(body(2, 100)), false).await;
            cache.store(12, 2, 8, Cached::NoTile, true).await;
        });
        let s = cache.finish();
        let waited = t0.elapsed();
        assert!(waited >= Duration::from_millis(300) && waited < Duration::from_secs(5), "{waited:?}");
        assert_eq!((s.written, s.parent_written, s.dropped), (1, 1, 1), "{s:?}");
        assert_eq!(get(&pack_path(&root, (12, 2, 0)), (12, 2, 0), 7), Some(Cached::Body(body(2, 100))));
        assert_eq!(get(&pack_path(&root, (12, 2, 0)), (12, 2, 0), 8), Some(Cached::NoTile));
        drop(holder);
        assert_eq!(get(&locked, (12, 1, 0), 7), None, "nothing was written to the locked pack");
        assert!(lock::open_locked(&locked).unwrap().is_some(), "closing the file released the lock");

        // After finish the cache takes nothing more, and says so by dropping.
        rt.block_on(cache.store(12, 3, 3, Cached::NoTile, false));
        assert_eq!(cache.finish().written, 1);
    }

    #[test]
    fn the_index_and_reads_go_through_the_cache_directory_layout() {
        let dir = tempfile::tempdir().unwrap();
        let tpl = "http://t/{z}/{x}/{y}.png";
        let cache = TileCache::open(dir.path(), tpl, "Terrarium").unwrap();
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        rt.block_on(async {
            cache.store(15, 17043, 11_580, Cached::Body(body(5, 64)), false).await;
            cache.store(15, 17043, 11_520, Cached::NoTile, false).await;
            cache.store(14, 8521, 5790, Cached::Body(body(6, 64)), true).await;
        });
        let s = cache.finish();
        assert_eq!((s.written, s.parent_written), (2, 1));
        let root = dir.path().join("ebd45912e88f1353");
        assert_eq!(root, dir.path().join(service_key(tpl, "terrarium")));
        assert!(root.join("z15").join("x17043_y45.pack").is_file(), "yblock = 11580 / 256 = 45");
        assert!(root.join("z14").join("x8521_y22.pack").is_file());
        let info: serde_json::Value =
            serde_json::from_slice(&fs::read(root.join("service.json")).unwrap()).unwrap();
        assert_eq!(info, serde_json::json!({ "url_template": tpl, "encoding": "terrarium" }));

        // A second run over the same directory finds them.
        let again = TileCache::open(dir.path(), tpl, "terrarium").unwrap();
        let idx = again.index(15, [(17043, 11_580), (17043, 11_520), (17043, 11_581), (9, 9)]);
        assert_eq!(idx.len(), 2);
        assert_eq!(again.read(15, 17043, 11_580, &idx[&(17043, 11_580)]), Some(Cached::Body(body(5, 64))));
        assert_eq!(again.read(15, 17043, 11_520, &idx[&(17043, 11_520)]), Some(Cached::NoTile));
        assert_eq!(again.lookup(14, 8521, 5790), Some(Cached::Body(body(6, 64))));
        assert_eq!(again.lookup(14, 8521, 5791), None);
        assert!(again.index(13, [(1, 1)]).is_empty(), "no z13 directory");
        assert_eq!(again.finish().written, 0);
    }

    #[test]
    fn a_cache_dir_that_cannot_be_created_is_an_error_with_its_reason() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a_file");
        fs::write(&file, b"x").unwrap();
        let err = TileCache::open(&file.join("cache"), "http://t/{z}/{x}/{y}.png", "terrarium")
            .err().expect("a path under a regular file");
        assert!(!err.is_empty());
    }
}
