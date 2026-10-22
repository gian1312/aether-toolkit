// rust/aether_converter/src/ingest.rs
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{BufWriter, Write, BufReader};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use serde::Deserialize;
#[cfg(feature = "native")]
use rayon::prelude::*;
use byteorder::{LittleEndian, WriteBytesExt};
use tiff::decoder::{Decoder, DecodingResult, Limits};
use tiff::tags::Tag;
use anyhow::{Context, Result};
use proj4rs::Proj;
use proj4rs::transform::transform;
use flatgeobuf::{FeatureProperties, FgbReader, GeometryType};
use crate::buildings::{
    load_pbf_building_dir, parse_leading_metres, rasterize_buildings, resolve_building_height,
    Building, BuildingHeight, HeightSource, I16Grid, PbfBuildingSet, RasterOpts, Rounding, TileRef,
    DEFAULT_HEIGHT_M,
};
use fallible_streaming_iterator::FallibleStreamingIterator;
use crate::tiff_io::{read_full_at, BlockReader, ChunkHeader, DirectChunks};
#[cfg(feature = "bc6h")]
use image_dds::{SurfaceRgba32Float, ImageFormat, Mipmaps, Quality};

// -- Diagnostics: the per-tile `[Rust] tile …` and the `[Rust] run: …` lines --
//
// CONTRACT changelog 31: diagnostic stdout lines for people reading a log, not
// parsed by any consumer; their wording may change.

/// A monotonic clock; a no-op on wasm32, where `std::time::Instant` panics
/// (the browser build compiles this module but never ingests).
#[derive(Clone, Copy)]
struct Clock(#[cfg(not(target_arch = "wasm32"))] std::time::Instant);

impl Clock {
    fn now() -> Self {
        #[cfg(not(target_arch = "wasm32"))]
        {
            Clock(std::time::Instant::now())
        }
        #[cfg(target_arch = "wasm32")]
        {
            Clock()
        }
    }

    /// Nanoseconds since [`Clock::now`] (0 on wasm32).
    fn ns(&self) -> u64 {
        #[cfg(not(target_arch = "wasm32"))]
        {
            self.0.elapsed().as_nanos() as u64
        }
        #[cfg(target_arch = "wasm32")]
        {
            0
        }
    }
}

thread_local! {
    /// Source files opened on this thread, and the nanoseconds spent opening.
    static OPENS: std::cell::Cell<(u64, u64)> = const { std::cell::Cell::new((0, 0)) };
}

/// `File::open`, counted and timed for the per-tile diagnostic line.
fn open_counted(path: &Path) -> std::io::Result<File> {
    let t = Clock::now();
    let r = File::open(path);
    let ns = t.ns();
    OPENS.with(|c| {
        let (n, s) = c.get();
        c.set((n + 1, s + ns));
    });
    r
}

/// This thread's I/O tallies at one moment. [`IoMark::since`] is what a call
/// in between did — valid for calls that stay on one thread (a header read,
/// a window load).
#[derive(Clone, Copy)]
struct IoMark {
    opens: u64,
    open_ns: u64,
    bytes: u64,
    at: Clock,
}

/// `(opens, ns opening, bytes read, ns elapsed)` since an [`IoMark`].
type IoDelta = (u64, u64, u64, u64);

impl IoMark {
    fn now() -> Self {
        let (opens, open_ns) = OPENS.with(|c| c.get());
        IoMark { opens, open_ns, bytes: crate::tiff_io::thread_bytes_read(), at: Clock::now() }
    }

    fn since(&self) -> IoDelta {
        let (opens, open_ns) = OPENS.with(|c| c.get());
        (
            opens - self.opens,
            open_ns - self.open_ns,
            crate::tiff_io::thread_bytes_read() - self.bytes,
            self.at.ns(),
        )
    }
}

/// Where one tile's time went (the `[Rust] tile …` line). Atomic: header
/// reads and window loads run on the rayon pool.
#[derive(Default)]
struct TileDiag {
    /// Header reads that opened a file (cache misses), their open and
    /// read-and-parse nanoseconds.
    hdr_opens: AtomicU64,
    hdr_open_ns: AtomicU64,
    hdr_read_ns: AtomicU64,
    /// Wall time of the eager window loads, summed over bands.
    dec_ns: AtomicU64,
    /// Time inside deferred (shadowed-source) window loads, which run during
    /// sampling.
    lazy_ns: AtomicU64,
    dec_bytes: AtomicU64,
}

impl TileDiag {
    fn header(&self, (opens, open_ns, _, ns): IoDelta) {
        if opens > 0 {
            self.hdr_opens.fetch_add(opens, Ordering::Relaxed);
            self.hdr_open_ns.fetch_add(open_ns, Ordering::Relaxed);
            self.hdr_read_ns.fetch_add(ns.saturating_sub(open_ns), Ordering::Relaxed);
        }
    }
}

/// CPU time and page faults of this process.
struct ProcUsage {
    user_s: f64,
    sys_s: f64,
    page_faults: u64,
}

/// The batch's rayon pool size, noted by `main` when it builds the pool;
/// 0 = no batch ran (single job). The `[Rust] run:` line reports it: at exit
/// the pool is gone, so a live thread count would always read 1.
static BATCH_THREADS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Record the batch's worker-thread count for the `[Rust] run:` line.
pub fn note_batch_threads(n: usize) {
    BATCH_THREADS.store(n, Ordering::Relaxed);
}

#[cfg(unix)]
fn proc_usage() -> ProcUsage {
    // SAFETY: getrusage fills the zeroed struct it is handed; on failure it
    // stays zeroed and reads as 0.
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut ru) };
    let secs = |t: libc::timeval| t.tv_sec as f64 + t.tv_usec as f64 / 1e6;
    ProcUsage {
        user_s: secs(ru.ru_utime),
        sys_s: secs(ru.ru_stime),
        // Minor + major, the same count Windows' PageFaultCount reports.
        page_faults: (ru.ru_minflt as u64).saturating_add(ru.ru_majflt as u64),
    }
}

#[cfg(windows)]
fn proc_usage() -> ProcUsage {
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};
    // FILETIME durations are 100 ns units.
    let secs = |f: FILETIME| ((u64::from(f.dwHighDateTime) << 32) | u64::from(f.dwLowDateTime)) as f64 / 1e7;
    // SAFETY: plain Win32 queries on the current-process pseudo handle (never
    // closed) into zeroed out-structs sized as the API requires.
    unsafe {
        let me = GetCurrentProcess();
        let (mut created, mut exited, mut kernel, mut user): (FILETIME, FILETIME, FILETIME, FILETIME) =
            (std::mem::zeroed(), std::mem::zeroed(), std::mem::zeroed(), std::mem::zeroed());
        let times = GetProcessTimes(me, &mut created, &mut exited, &mut kernel, &mut user) != 0;
        let mut mem: PROCESS_MEMORY_COUNTERS = std::mem::zeroed();
        mem.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
        let faults = if GetProcessMemoryInfo(me, &mut mem, mem.cb) != 0 { u64::from(mem.PageFaultCount) } else { 0 };
        ProcUsage {
            user_s: if times { secs(user) } else { 0.0 },
            sys_s: if times { secs(kernel) } else { 0.0 },
            page_faults: faults,
        }
    }
}

#[cfg(not(any(unix, windows)))]
fn proc_usage() -> ProcUsage {
    ProcUsage { user_s: 0.0, sys_s: 0.0, page_faults: 0 }
}

/// The process diagnostic line (stdout; changelog 31/37, not parsed):
/// `[Rust] run: wall <s>s, cpu user <s>s sys <s>s, page faults <n>, threads <n>, blocked ~<s>s`.
/// `page faults` counts minor and major faults (Windows: `PageFaultCount`);
/// `threads` is the batch's rayon pool size ([`note_batch_threads`]) and is
/// omitted, with its comma, when no batch ran; `blocked` = wall × threads −
/// (user + sys), floored at 0 (a single job counts rayon's global pool).
pub fn print_run_stats(wall_s: f64) {
    let u = proc_usage();
    let batch = BATCH_THREADS.load(Ordering::Relaxed);
    let threads = match batch {
        0 => String::new(),
        n => format!(", threads {n}"),
    };
    // Thread time not spent on a CPU (changelog 37): what the compute
    // threads waited for — the file system, a free writer, each other. A
    // single job's parallel loops run on rayon's global pool.
    let pool = match batch {
        0 => compute_threads(),
        n => n,
    };
    let blocked = (wall_s * pool as f64 - (u.user_s + u.sys_s)).max(0.0);
    println!(
        "[Rust] run: wall {:.1}s, cpu user {:.1}s sys {:.1}s, page faults {}{}, blocked ~{:.1}s",
        wall_s, u.user_s, u.sys_s, u.page_faults, threads, blocked
    );
    let _ = std::io::stdout().flush();
}

/// Threads of rayon's global pool (1 without the `native` feature).
fn compute_threads() -> usize {
    #[cfg(feature = "native")]
    {
        rayon::current_num_threads()
    }
    #[cfg(not(feature = "native"))]
    {
        1
    }
}

/// Prints [`print_run_stats`] when dropped — created at the top of `main`, it
/// prints at the very end, after the batch result, on every way out
/// (an early `?`/`bail!` included).
pub struct RunStats {
    start: Clock,
}

impl RunStats {
    pub fn start() -> Self {
        RunStats { start: Clock::now() }
    }
}

impl Drop for RunStats {
    fn drop(&mut self) {
        print_run_stats(self.start.ns() as f64 / 1e9);
    }
}

// -- Source I/O off the compute threads (changelog 37) --------------------------
//
// Opening a source, reading its header and reading the byte ranges of the
// chunks a band needs are file-system waits — on a slow volume (a network
// share, a 9p mount, a disk behind an on-access scanner) more than half of a
// tile's thread time. They run on a small pool of I/O threads instead of the
// rayon compute threads: a tile submits a band's reads one band ahead (they
// are read while the previous band samples) and its compute threads only
// decode. Everything here is plain `std` and behaves the same on Linux,
// macOS and Windows; without the `native` feature (the browser build, which
// never ingests) every task simply runs inline.

/// A value one thread produces and another waits for.
struct Ticket<T> {
    slot: std::sync::Mutex<Option<T>>,
    ready: std::sync::Condvar,
}

impl<T> Ticket<T> {
    fn new() -> Arc<Self> {
        Arc::new(Ticket { slot: std::sync::Mutex::new(None), ready: std::sync::Condvar::new() })
    }

    fn put(&self, v: T) {
        *self.slot.lock().unwrap_or_else(|p| p.into_inner()) = Some(v);
        self.ready.notify_all();
    }

    /// Wait for the value and take it (each ticket has one consumer).
    fn take(&self) -> T {
        let mut g = self.slot.lock().unwrap_or_else(|p| p.into_inner());
        loop {
            if let Some(v) = g.take() {
                return v;
            }
            g = self.ready.wait(g).unwrap_or_else(|p| p.into_inner());
        }
    }
}

type IoTask = Box<dyn FnOnce() + Send>;

struct IoQueues {
    /// Reads a thread is waiting for right now (headers, a band's first
    /// reads, deferred windows): served first.
    urgent: std::collections::VecDeque<IoTask>,
    /// Reads for the next band, issued while the current one samples.
    ahead: std::collections::VecDeque<IoTask>,
}

/// The process's source I/O threads.
struct IoPool {
    queues: std::sync::Mutex<IoQueues>,
    work: std::sync::Condvar,
    /// Threads that started; 0 runs every task inline on the caller.
    live: std::sync::atomic::AtomicUsize,
}

/// I/O threads: twice the cores (a read mostly waits), 8..=32.
fn io_threads() -> usize {
    std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).saturating_mul(2).clamp(8, 32)
}

fn io_pool() -> &'static IoPool {
    static POOL: OnceLock<IoPool> = OnceLock::new();
    static START: std::sync::Once = std::sync::Once::new();
    let pool = POOL.get_or_init(|| IoPool {
        queues: std::sync::Mutex::new(IoQueues {
            urgent: std::collections::VecDeque::new(),
            ahead: std::collections::VecDeque::new(),
        }),
        work: std::sync::Condvar::new(),
        live: std::sync::atomic::AtomicUsize::new(0),
    });
    START.call_once(|| {
        if !cfg!(feature = "native") {
            return;
        }
        for i in 0..io_threads() {
            let spawned = std::thread::Builder::new()
                .name(format!("aether-io-{i}"))
                .stack_size(1 << 20)
                .spawn(move || pool.serve());
            match spawned {
                Ok(_) => {
                    pool.live.fetch_add(1, Ordering::Relaxed);
                }
                Err(_) => break, // fewer threads (or none: inline) — never a failure
            }
        }
    });
    pool
}

impl IoPool {
    fn serve(&self) {
        loop {
            let task = {
                let mut q = self.queues.lock().unwrap_or_else(|p| p.into_inner());
                loop {
                    if let Some(t) = q.urgent.pop_front().or_else(|| q.ahead.pop_front()) {
                        break t;
                    }
                    q = self.work.wait(q).unwrap_or_else(|p| p.into_inner());
                }
            };
            task();
        }
    }

    /// Run *work* on an I/O thread; its result arrives in the ticket. A
    /// panic becomes an error, so no waiter is ever left hanging.
    fn submit<X: Send + 'static>(
        &self,
        urgent: bool,
        work: impl FnOnce() -> Result<X> + Send + 'static,
    ) -> Arc<Ticket<Result<X>>> {
        let ticket = Ticket::new();
        let out = ticket.clone();
        let task: IoTask = Box::new(move || {
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(work))
                .unwrap_or_else(|_| Err(anyhow::anyhow!("internal error: a source read panicked")));
            out.put(r);
        });
        if self.live.load(Ordering::Relaxed) == 0 {
            task();
        } else {
            let mut q = self.queues.lock().unwrap_or_else(|p| p.into_inner());
            if urgent {
                q.urgent.push_back(task);
            } else {
                q.ahead.push_back(task);
            }
            drop(q);
            self.work.notify_one();
        }
        ticket
    }
}

/// Source files a [`ChunkCache`] keeps open at most. A batch reads each
/// source in the band(s) its rows reach, tile after tile; keeping the handle
/// makes that one open per file instead of one per band (and the header's
/// open serves the windows), and neighbouring tiles share the files along
/// their edge. Bounded well below every platform's descriptor limit: on
/// Unix a quarter of the soft `RLIMIT_NOFILE` (macOS defaults to 256, many
/// Linux shells to 1024), 16..=256; on Windows, which has no such limit for
/// file handles, 512.
fn max_open_sources() -> usize {
    static CAP: OnceLock<usize> = OnceLock::new();
    *CAP.get_or_init(|| {
        #[cfg(unix)]
        {
            // SAFETY: getrlimit fills the zeroed struct it is handed.
            let mut rl: libc::rlimit = unsafe { std::mem::zeroed() };
            if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut rl) } == 0 {
                let soft = rl.rlim_cur;
                // RLIM_INFINITY and huge limits clamp to the cap below.
                let quarter = if soft == libc::RLIM_INFINITY { 1024 } else { (soft / 4).min(1024) as usize };
                return quarter.clamp(16, 256);
            }
            64
        }
        #[cfg(not(unix))]
        {
            512
        }
    })
}

/// Order keys at or above this are handles in use (least recently used
/// first); below it, handles nobody is known to need soon — a header's open
/// before its windows are read, a source a tile has finished with — which
/// are evicted first, oldest first.
const OPEN_IN_USE: u64 = 1 << 62;

/// The open source files, by [`FileSource::id`].
#[derive(Default)]
struct OpenFiles {
    files: HashMap<u64, (Arc<File>, u64)>,
    /// Order key -> id; the smallest key is evicted first.
    order: std::collections::BTreeMap<u64, u64>,
    /// Next keys of the two classes.
    spare_tick: u64,
    used_tick: u64,
}

impl OpenFiles {
    fn next_key(&mut self, in_use: bool) -> u64 {
        if in_use {
            self.used_tick += 1;
            OPEN_IN_USE + self.used_tick
        } else {
            self.spare_tick += 1;
            self.spare_tick
        }
    }

    /// The handle of *id*, now in use.
    fn get(&mut self, id: u64) -> Option<Arc<File>> {
        let key = self.next_key(true);
        let (f, k) = self.files.get_mut(&id)?;
        self.order.remove(k);
        *k = key;
        self.order.insert(key, id);
        Some(f.clone())
    }

    /// Mark *id* as not needed soon (it stays open until evicted).
    fn spare(&mut self, id: u64) {
        let key = self.next_key(false);
        if let Some((_, k)) = self.files.get_mut(&id) {
            self.order.remove(k);
            *k = key;
            self.order.insert(key, id);
        }
    }

    /// Keep *file* for *id* (an existing entry wins); returns the handle to
    /// use and the evicted ones, for the caller to close outside the lock.
    fn put(&mut self, id: u64, file: Arc<File>, in_use: bool, cap: usize) -> (Arc<File>, Vec<Arc<File>>) {
        if let Some(f) = self.get(id) {
            return (f, vec![file]);
        }
        let mut evicted = Vec::new();
        while self.files.len() >= cap.max(1) {
            let Some((_, old)) = self.order.pop_first() else { break };
            if let Some((f, _)) = self.files.remove(&old) {
                evicted.push(f);
            }
        }
        let key = self.next_key(in_use);
        self.order.insert(key, id);
        self.files.insert(id, (file.clone(), key));
        (file, evicted)
    }
}

/// One terrain input of an ingest job.
///
/// `sources` is ordered by priority: for every output pixel the first source
/// with real ground under it wins. `crs` is either `"EPSG:nnnn"` or a raw proj
/// string starting with `+`; when absent the GeoTIFF's GeoKeyDirectory must
/// carry a usable EPSG code, else the job fails naming this file. `nodata`
/// overrides the file's `GDAL_NODATA` tag; matching samples become the void
/// sentinel at load time.
#[derive(Deserialize, Debug, Clone)]
pub struct SourceSpec {
    pub path: PathBuf,
    #[serde(default)]
    pub crs: Option<String>,
    #[serde(default)]
    pub nodata: Option<f64>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct IngestJob {
    pub output_path: PathBuf,
    pub format: Option<String>, // "bc6h" or "r16sint"
    pub ul_lat: f64,
    pub ul_lon: f64,
    pub resolution_m: f64,
    pub size_px: u32,
    /// Prioritised terrain inputs (first-valid-wins per pixel). The modern
    /// surface; mutually exclusive with the deprecated `base_tif`/`swiss_tifs`.
    #[serde(default)]
    pub sources: Vec<SourceSpec>,
    /// If set, a pixel no source covers is written as `round(void_fill_m * 2)`
    /// half-metres (saturating) instead of the `-9999` void sentinel.
    #[serde(default)]
    pub void_fill_m: Option<f64>,
    /// DEPRECATED alias: normalized to a trailing `sources` entry with
    /// `crs = "EPSG:4326"`. Slated for removal in the next major version.
    #[serde(default)]
    pub base_tif: Option<PathBuf>,
    /// DEPRECATED alias: normalized to leading `sources` entries with
    /// `crs = "EPSG:2056"` (in order). Slated for removal in the next major
    /// version.
    #[serde(default)]
    pub swiss_tifs: Vec<PathBuf>,
    pub buildings_file: Option<PathBuf>,
    /// Directory of Mapbox-Vector-Tile building tiles named `{z}_{x}_{y}.pbf`
    /// (gzipped or not), e.g. an OpenFreeMap planet fetch.
    ///
    /// Optional and absent from older jobs, so existing callers are unaffected.
    /// Unlike `buildings_file`, these carry a height *above ground*, which the
    /// rasterizer resolves against the terrain under each footprint.
    #[serde(default)]
    pub buildings_pbf_dir: Option<PathBuf>,
}

impl IngestJob {
    /// The job's terrain inputs in priority order, with the deprecated
    /// `base_tif`/`swiss_tifs` aliases normalized into `sources` form.
    ///
    /// Legacy shim: each `swiss_tifs` entry becomes a source with
    /// `crs = "EPSG:2056"` (in order), then `base_tif` is appended with
    /// `crs = "EPSG:4326"` — which preserves the old per-pixel priority
    /// (the high-resolution stack first, the base DEM as fallback). After
    /// this call there is exactly ONE code path.
    pub fn effective_sources(&self) -> Result<Vec<SourceSpec>> {
        let has_legacy = self.base_tif.is_some() || !self.swiss_tifs.is_empty();
        if !self.sources.is_empty() && has_legacy {
            anyhow::bail!(
                "ingest job for {:?}: 'sources' cannot be combined with the deprecated \
                 'base_tif'/'swiss_tifs' fields — the priority order would be ambiguous. \
                 Move every input into 'sources' (array order = priority) and delete the \
                 legacy fields.",
                self.output_path
            );
        }
        if !self.sources.is_empty() {
            return Ok(self.sources.clone());
        }
        let mut out = Vec::with_capacity(self.swiss_tifs.len() + 1);
        for p in &self.swiss_tifs {
            out.push(SourceSpec { path: p.clone(), crs: Some("EPSG:2056".into()), nodata: None });
        }
        if let Some(base) = &self.base_tif {
            out.push(SourceSpec { path: base.clone(), crs: Some("EPSG:4326".into()), nodata: None });
        }
        Ok(out)
    }
}

/// Key under which a source is cached.
///
/// The decoded pixels depend on the *explicit* per-source `nodata` override
/// (it is applied at load time), so the same file requested with two different
/// overrides must occupy two cache slots. The override's f64 bit pattern keeps
/// the key hashable.
pub type CacheKey = (PathBuf, Option<u64>);

/// Caller-supplied, already-decoded sources, keyed by [`CacheKey`].
///
/// An entry here is sampled **instead of** reading the file at that path, as
/// one whole in-memory image (the historical whole-file path). `ingest` itself
/// never inserts into this map any more — decoded pixels live in the
/// byte-budgeted [`ChunkCache`] — so a caller that passes an empty map (the
/// CLI does) gets the streaming path for every source.
pub type ImageCache = Arc<std::sync::Mutex<HashMap<CacheKey, Arc<LoadedImage>>>>;

/// The [`CacheKey`] for one source spec.
pub fn cache_key(spec: &SourceSpec) -> CacheKey {
    (spec.path.clone(), spec.nodata.map(f64::to_bits))
}

/// Default per-tile budget for the decoded source windows one row band holds
/// (see [`IngestOptions::band_budget_bytes`]).
pub const DEFAULT_BAND_BUDGET_BYTES: usize = 128 << 20;
/// Default budget of the decoded-chunk cache (see [`ChunkCache`]).
pub const DEFAULT_CHUNK_CACHE_BYTES: usize = 192 << 20;
/// Rows per band never exceed this, so the per-band row geometry stays small
/// even for the widest tile the `.abt` header can describe.
const MAX_BAND_ROWS: usize = 512;

/// Memory knobs of one tile conversion. Output bytes never depend on them —
/// only peak memory and speed do.
#[derive(Clone)]
pub struct IngestOptions {
    /// Upper bound on the decoded source pixels one tile holds at once.
    /// The output tile is streamed in row bands, and a band is shrunk until
    /// the source windows it needs fit this budget (a single output row that
    /// alone needs more is still processed — it cannot be split).
    pub band_budget_bytes: usize,
    /// Shared decoded-chunk cache. `None` gives the call a private cache of
    /// [`DEFAULT_CHUNK_CACHE_BYTES`]; a batch passes one cache to every tile
    /// so neighbouring tiles reuse the chunks along their shared edge.
    pub chunk_cache: Option<Arc<ChunkCache>>,
}

impl Default for IngestOptions {
    fn default() -> Self {
        IngestOptions { band_budget_bytes: DEFAULT_BAND_BUDGET_BYTES, chunk_cache: None }
    }
}

/// Rough peak memory of converting *job* with *opts*, for sizing a thread
/// pool: the output buffer (and the BC6H RGBA staging buffer), the band
/// budget, and a fixed allowance for decode transients, row geometry and
/// building rasterization. The shared chunk cache is NOT included — it is
/// paid once per process, not per job.
pub fn estimate_job_bytes(job: &IngestJob, opts: &IngestOptions) -> u64 {
    let px = u64::from(job.size_px) * u64::from(job.size_px);
    let is_bc6h = job.format.as_deref().is_some_and(|f| f.eq_ignore_ascii_case("bc6h"));
    let buffers = px * 2 + if is_bc6h { px * 16 * 2 } else { 0 };
    buffers + opts.band_budget_bytes as u64 + (64 << 20)
}

/// `(source id, chunk index)` — one decoded TIFF strip/tile, or one
/// [`ABT_STRIP_ROWS`]-row strip of an `.abt` source.
type ChunkKey = (u64, u32);

struct ChunkCacheInner {
    chunks: HashMap<ChunkKey, (Arc<Vec<i16>>, u64)>,
    lru: std::collections::BTreeMap<u64, ChunkKey>,
    bytes: usize,
    peak_bytes: usize,
    tick: u64,
    metas: HashMap<CacheKey, Arc<FileSource>>,
    next_id: u64,
}

/// Byte-budgeted LRU of decoded source chunks, plus the (small, unbounded)
/// table of parsed source headers.
///
/// Replaces the old count-based cache of WHOLE decoded files, which held every
/// source a tile touched at once — ~580 swissALTI3D 0.5 m files under one
/// 10 m tile is 4.6 GB per thread. Chunks are the TIFF's own strips/tiles,
/// already converted to half-metres; the cache never holds more than its
/// budget (a chunk larger than half the budget is not cached at all — the
/// tile that needs it keeps it privately for its own duration instead).
pub struct ChunkCache {
    inner: std::sync::Mutex<ChunkCacheInner>,
    budget: usize,
    /// Parsed projections by proj string, for the WGS84 bounding box every
    /// file source gets at header time (`None`: the string did not parse).
    projs: std::sync::Mutex<HashMap<String, Option<Arc<Proj>>>>,
    /// Source files kept open between reads ([`max_open_sources`]); an
    /// evicted handle closes when its last reader drops it.
    open: std::sync::Mutex<OpenFiles>,
}

impl ChunkCache {
    pub fn new(budget_bytes: usize) -> Self {
        ChunkCache {
            inner: std::sync::Mutex::new(ChunkCacheInner {
                chunks: HashMap::new(),
                lru: std::collections::BTreeMap::new(),
                bytes: 0,
                peak_bytes: 0,
                tick: 0,
                metas: HashMap::new(),
                next_id: 0,
            }),
            budget: budget_bytes,
            projs: std::sync::Mutex::new(HashMap::new()),
            open: std::sync::Mutex::new(OpenFiles::default()),
        }
    }

    /// An open handle to *src*'s file: the kept one, else a fresh open
    /// (counted; kept for the next read). Positioned reads never move a
    /// shared cursor, so one handle serves every thread — except on targets
    /// without them (neither Unix nor Windows), which always open afresh.
    fn file(&self, src: &FileSource) -> Result<Arc<File>> {
        if !cfg!(any(unix, windows)) {
            return Ok(Arc::new(open_counted(&src.path).with_context(|| format!("Opening {:?}", src.path))?));
        }
        if let Some(f) = self.open.lock().unwrap().get(src.id) {
            return Ok(f);
        }
        let f = Arc::new(open_counted(&src.path).with_context(|| format!("Opening {:?}", src.path))?);
        Ok(self.keep_file(src.id, f, true))
    }

    /// Keep *file* open for source *id* — `in_use` for a window read, else
    /// as a spare (a header's open); returns the handle to use.
    fn keep_file(&self, id: u64, file: Arc<File>, in_use: bool) -> Arc<File> {
        if !cfg!(any(unix, windows)) {
            return file;
        }
        let (f, evicted) = self.open.lock().unwrap().put(id, file, in_use, max_open_sources());
        drop(evicted); // closed here, outside the lock
        f
    }

    /// A tile is done with these sources: their handles become the first
    /// to go (another tile may still reuse them until then).
    fn spare_files(&self, ids: impl IntoIterator<Item = u64>) {
        let mut g = self.open.lock().unwrap();
        for id in ids {
            g.spare(id);
        }
    }

    /// Which of *keys* are not cached (no LRU touch: a plan, not a use).
    fn missing(&self, keys: &[ChunkKey]) -> Vec<bool> {
        let g = self.inner.lock().unwrap();
        keys.iter().map(|k| !g.chunks.contains_key(k)).collect()
    }

    /// The parsed header of *spec* if it is already cached.
    fn meta_cached(&self, spec: &SourceSpec) -> Option<Arc<FileSource>> {
        self.inner.lock().unwrap().metas.get(&cache_key(spec)).cloned()
    }

    /// The configured budget in bytes.
    pub fn budget(&self) -> usize {
        self.budget
    }

    /// Decoded bytes currently held.
    pub fn bytes(&self) -> usize {
        self.inner.lock().unwrap().bytes
    }

    /// The most decoded bytes ever held at once.
    pub fn peak_bytes(&self) -> usize {
        self.inner.lock().unwrap().peak_bytes
    }

    /// Whether a chunk of *bytes* may enter the cache at all.
    fn admits(&self, bytes: usize) -> bool {
        bytes <= self.budget / 2
    }

    /// Look several chunks up under one lock; misses come back `None`.
    fn get_many(&self, keys: &[ChunkKey]) -> Vec<Option<Arc<Vec<i16>>>> {
        let mut g = self.inner.lock().unwrap();
        let inner = &mut *g;
        keys.iter()
            .map(|k| {
                let (data, tick) = inner.chunks.get_mut(k)?;
                inner.tick += 1;
                inner.lru.remove(tick);
                *tick = inner.tick;
                inner.lru.insert(inner.tick, *k);
                Some(data.clone())
            })
            .collect()
    }

    /// Insert decoded chunks, evicting least-recently-used ones until the
    /// budget holds. Callers only offer chunks [`Self::admits`] accepts.
    fn insert_many(&self, items: Vec<(ChunkKey, Arc<Vec<i16>>)>) {
        let mut g = self.inner.lock().unwrap();
        let inner = &mut *g;
        for (k, data) in items {
            let sz = data.len() * 2;
            if inner.chunks.contains_key(&k) {
                continue; // another tile decoded it meanwhile
            }
            while inner.bytes + sz > self.budget {
                let Some((_, old)) = inner.lru.pop_first() else { break };
                if let Some((d, _)) = inner.chunks.remove(&old) {
                    inner.bytes -= d.len() * 2;
                }
            }
            inner.tick += 1;
            inner.lru.insert(inner.tick, k);
            inner.chunks.insert(k, (data, inner.tick));
            inner.bytes += sz;
            inner.peak_bytes = inner.peak_bytes.max(inner.bytes);
        }
    }

    /// The parsed header of one file source, read once per cache.
    ///
    /// Errors are not cached: a bad source fails every tile that lists it,
    /// with the same message each time.
    fn meta(&self, spec: &SourceSpec) -> Result<Arc<FileSource>> {
        let key = cache_key(spec);
        if let Some(m) = self.inner.lock().unwrap().metas.get(&key) {
            return Ok(m.clone());
        }
        // Parsed without the lock held: thousands of headers per batch must
        // not serialise every tile behind one file open.
        let (mut parsed, handle) = read_file_source_keep(&spec.path, spec.nodata)?;
        parsed.wgs84_bbox = self.projected_bbox(spec, &parsed.geom);
        let mut g = self.inner.lock().unwrap();
        if let Some(m) = g.metas.get(&key) {
            return Ok(m.clone());
        }
        let id = g.next_id;
        g.next_id += 1;
        let m = Arc::new(FileSource { id, ..parsed });
        g.metas.insert(key, m.clone());
        drop(g);
        // The header's open serves the window reads too, while it lasts.
        if let Some(h) = handle {
            self.keep_file(id, Arc::new(h), false);
        }
        Ok(m)
    }

    /// The WGS84 bounding box of a projected file source under the CRS
    /// *spec* resolves to (the job's `crs`, else the file's GeoKeys), with
    /// the proj string it was computed for. Best effort: anything that does
    /// not resolve here is left to `process_tile_opts`, which raises the
    /// proper error. Geographic sources need no transform and get `None`.
    fn projected_bbox(&self, spec: &SourceSpec, geom: &SourceGeom) -> Option<(String, Option<[f64; 4]>)> {
        if geom.is_abt {
            return None;
        }
        let crs = spec.crs.as_ref().or(geom.embedded_crs.as_ref())?;
        let proj_string = crs_to_proj_string(crs, &spec.path).ok()?;
        let proj = {
            let cached = self.projs.lock().unwrap().get(&proj_string).cloned();
            match cached {
                Some(p) => p,
                None => {
                    let p = Proj::from_proj_string(&proj_string).ok().map(Arc::new);
                    self.projs.lock().unwrap().insert(proj_string.clone(), p.clone());
                    p
                }
            }
        }?;
        if proj.is_latlong() {
            return None;
        }
        let bbox = wgs84_bbox_of_projected(geom, &proj, wgs84());
        Some((proj_string, bbox))
    }
}

/// WGS84 as proj4rs knows it (built once per process).
fn wgs84() -> &'static Proj {
    static WGS84: OnceLock<Proj> = OnceLock::new();
    WGS84.get_or_init(|| {
        Proj::from_proj_string("+proj=longlat +datum=WGS84 +no_defs")
            .expect("the WGS84 proj string is a constant and always parses")
    })
}

/// `[lon_min, lat_min, lon_max, lat_max]` of a geographic source's extent.
fn geographic_bbox(g: &SourceGeom) -> [f64; 4] {
    [g.origin_e, g.limit_n, g.limit_e, g.origin_n]
}

/// WGS84 bounding box `[lon_min, lat_min, lon_max, lat_max]` of a projected
/// source's extent: its corners and edge midpoints, transformed back.
/// `None` when a point is outside the projection's domain. Only the
/// shadowing heuristic in [`process_tile_opts`] uses it, so its precision
/// decides speed, never bytes.
fn wgs84_bbox_of_projected(g: &SourceGeom, proj: &Proj, wgs84: &Proj) -> Option<[f64; 4]> {
    let mut bb = [f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY];
    let em = 0.5 * (g.origin_e + g.limit_e);
    let nm = 0.5 * (g.origin_n + g.limit_n);
    for e in [g.origin_e, em, g.limit_e] {
        for n in [g.origin_n, nm, g.limit_n] {
            let mut pt = (e, n, 0.0);
            transform(proj, wgs84, &mut pt).ok()?;
            let (lon, lat) = (pt.0.to_degrees(), pt.1.to_degrees());
            if !lon.is_finite() || !lat.is_finite() {
                return None;
            }
            bb = [bb[0].min(lon), bb[1].min(lat), bb[2].max(lon), bb[3].max(lat)];
        }
    }
    Some(bb)
}

/// The value written for a pixel with no terrain under it.
///
/// Elevations are stored in **half-metres**, so this is -4999.5 m — far below
/// any real ground, and below the `-5000` half-metre (-2500 m) floor the
/// samplers in this file use to tell "no data" from "very low ground". A void
/// must never be confused with 0 (sea level): a reader that treats it as ground
/// gets flat terrain at mean sea level instead of a hole it can fill from
/// another source.
pub const VOID_ELEV: i16 = -9999;

/// How much of one tile a source actually reached.
///
/// Counted BEFORE `void_fill_m` is applied, so it answers "did any source
/// cover this pixel?" and not "is this pixel non-sentinel?" — a job that fills
/// its holes with 0 m would otherwise report full coverage of a tile no source
/// touched. [`process_tile`] returns it so the caller can refuse a run that
/// produced no terrain at all; a partially covered tile is normal and says
/// nothing on its own (the edge tiles of any area straddle the source).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TileStats {
    /// Pixels at least one source supplied a sample for.
    pub covered_px: u64,
    /// Pixels in the tile.
    pub total_px: u64,
    /// Most decoded source bytes this tile held at once in its band windows
    /// (plus any oversized chunk it had to keep privately). Caller-supplied
    /// [`ImageCache`] images are not counted — the caller owns them.
    pub peak_window_bytes: u64,
    /// Row bands the tile was streamed in.
    pub bands: u32,
}

/// Convert one Float32 DEM sample to the `.abt` half-metre unit.
///
/// `f32 as i16` is defined to produce **0** for NaN, and NaN is GDAL's default
/// Float32 nodata — so a Float32 DEM's voids used to arrive as 0 half-metres,
/// pass the `> -5000` validity test, and get written as sea level. Non-finite
/// samples now become the void sentinel instead. Finite samples are unchanged:
/// the cast already truncates toward zero and saturates at the i16 bounds.
#[inline]
pub fn f32_sample_to_half_metres(x: f32) -> i16 {
    if !x.is_finite() {
        return VOID_ELEV;
    }
    (x * 2.0) as i16
}

/// Convert one Int32 DEM sample to the `.abt` half-metre unit.
///
/// The old `(x as i16).saturating_mul(2)` narrowed **before** the multiply, so
/// the cast kept only the low 16 bits: the classic Int32 nodata `i32::MIN` has
/// them all zero and arrived as 0 half-metres — sea level again. Widen first,
/// then saturate over the full range, and map anything that saturates low to
/// the void sentinel.
#[inline]
pub fn i32_sample_to_half_metres(x: i32) -> i16 {
    i64_sample_to_half_metres(i64::from(x))
}

/// The same rule for any integer width the `tiff` crate can decode.
///
/// UInt16 is the commonest national-DEM sample format and UInt8/Int8/UInt32/
/// Int64 all turn up in the wild; every one of them used to come back as a
/// bare "Unsupported TIF format" from a reader that handled only I16/I32/F32.
#[inline]
pub fn i64_sample_to_half_metres(x: i64) -> i16 {
    let half_metres = x.saturating_mul(2);
    if half_metres < VOID_ELEV as i64 {
        VOID_ELEV
    } else if half_metres > i16::MAX as i64 {
        i16::MAX
    } else {
        half_metres as i16
    }
}

/// Convert one Float64 DEM sample to the `.abt` half-metre unit.
///
/// Same rule as [`f32_sample_to_half_metres`] — non-finite is the void
/// sentinel, and the `as i16` cast saturates rather than wrapping.
#[inline]
pub fn f64_sample_to_half_metres(x: f64) -> i16 {
    if !x.is_finite() {
        return VOID_ELEV;
    }
    (x * 2.0) as i16
}

/// Decoded integer samples of any width -> half-metres, honouring *nodata*.
#[inline]
fn int_samples<T>(v: Vec<T>, nodata: Option<f64>) -> Vec<i16>
where
    T: Copy + Into<i64>,
{
    v.into_iter()
        .map(|x| {
            let widened: i64 = x.into();
            match nodata {
                Some(nd) if widened as f64 == nd => VOID_ELEV,
                _ => i64_sample_to_half_metres(widened),
            }
        })
        .collect()
}

pub struct LoadedImage {
    pub width: u32,
    pub height: u32,
    pub data: Vec<i16>,
    pub origin_e: f64,
    pub origin_n: f64,
    pub limit_e: f64,
    pub limit_n: f64,
    /// Pixel step east (X) and south (Y), in source units. Two values, never
    /// one: non-square pixels are common (Copernicus GLO-30 north of 50°N,
    /// any geographic grid with equal metres per pixel), and one shared scale
    /// stretched such a file north–south — 2× for 0.0004 × 0.0002°.
    pub scale_x: f64,
    pub scale_y: f64,
    pub name: String,
    /// CRS read from the file's GeoKeyDirectory as `"EPSG:nnnn"`, if the file
    /// carries a usable (non-user-defined) code. A per-source explicit `crs`
    /// always wins over this.
    pub embedded_crs: Option<String>,
    /// True when the source was an `.abt` tile (detected by magic). Such a
    /// source is self-describing — geographic degrees, values already in
    /// half-metres — and refuses per-source `crs`/`nodata` overrides.
    pub is_abt: bool,
}

impl LoadedImage {
    fn geom(&self) -> SourceGeom {
        SourceGeom {
            width: self.width,
            height: self.height,
            origin_e: self.origin_e,
            origin_n: self.origin_n,
            limit_e: self.limit_e,
            limit_n: self.limit_n,
            scale_x: self.scale_x,
            scale_y: self.scale_y,
            embedded_crs: self.embedded_crs.clone(),
            is_abt: self.is_abt,
        }
    }
}

/// Where a source sits and how big it is — everything sampling needs except
/// the pixels. Identical fields to [`LoadedImage`], so a streamed source and
/// a whole in-memory one go through the very same span arithmetic.
#[derive(Clone, Debug)]
struct SourceGeom {
    width: u32,
    height: u32,
    origin_e: f64,
    origin_n: f64,
    limit_e: f64,
    limit_n: f64,
    scale_x: f64,
    scale_y: f64,
    embedded_crs: Option<String>,
    is_abt: bool,
}

/// A source file's parsed header: geometry plus how to decode its pixels.
struct FileSource {
    /// Unique per [`ChunkCache`]; keys the cached chunks.
    id: u64,
    path: PathBuf,
    geom: SourceGeom,
    layout: FileLayout,
    /// For a projected source: the proj string its CRS resolved to at header
    /// time and the source's WGS84 bounding box under it (see
    /// [`ChunkCache::projected_bbox`]).
    wgs84_bbox: Option<(String, Option<[f64; 4]>)>,
}

enum FileLayout {
    /// A GeoTIFF read chunk by chunk (its own strips or tiles).
    Tiff {
        /// Effective nodata (the job override, else the GDAL_NODATA tag), in
        /// raw sample units.
        nodata: Option<f64>,
        /// GDAL scale/offset (`value * scale + offset` = metres), when the
        /// file carries a non-identity pair.
        value_xform: Option<(f64, f64)>,
        chunk_w: u32,
        chunk_h: u32,
        /// Chunks per chunk row (1 for strips).
        across: u32,
        /// Chunk offsets and byte counts, codec, predictor, sample type and
        /// byte order — everything a band needs to read and decode a chunk
        /// without re-parsing the IFD. `None` for encodings the direct
        /// decoder does not mirror (PackBits, JPEG, WebP, ZSTD, sub-byte or
        /// multi-sample layouts, …): those re-open the `tiff` decoder per
        /// band, over a [`BlockReader`].
        direct: Option<DirectChunks>,
    },
    /// An `.abt` tile, read as [`ABT_STRIP_ROWS`]-row strips.
    Abt { size: usize, stride: usize },
}

/// Rows per chunk of an `.abt` source on the streaming path: a band reads and
/// decodes only the strips its rows reach, never the whole payload.
const ABT_STRIP_ROWS: u32 = 256;
/// Bytes of the `.abt` header (contract §6) before the first payload row.
const ABT_HEADER_BYTES: u64 = 44;

/// Decoded source pixels covering `[x0, x1) x [y0, y1)` of one source, in
/// half-metres; row `y` starts at `(y - y0) * stride`.
struct Window {
    buf: WinBuf,
    x0: u32,
    y0: u32,
    x1: u32,
    y1: u32,
    stride: usize,
}

enum WinBuf {
    /// A caller-supplied whole image, referenced without a copy.
    Img(Arc<LoadedImage>),
    Own(Vec<i16>),
}

impl Window {
    fn data(&self) -> &[i16] {
        match &self.buf {
            WinBuf::Img(img) => &img.data,
            WinBuf::Own(v) => v,
        }
    }

    /// Bytes this window holds on the tile's behalf (a caller-owned image
    /// costs the tile nothing).
    fn owned_bytes(&self) -> usize {
        match &self.buf {
            WinBuf::Img(_) => 0,
            WinBuf::Own(v) => v.len() * 2,
        }
    }

    fn view(&self) -> WinView<'_> {
        WinView { data: self.data(), x0: self.x0, y0: self.y0, x1: self.x1, y1: self.y1, stride: self.stride }
    }
}

/// Borrowed form of a [`Window`] for the hot loop.
#[derive(Clone, Copy)]
struct WinView<'a> {
    data: &'a [i16],
    x0: u32,
    y0: u32,
    x1: u32,
    y1: u32,
    stride: usize,
}

impl WinView<'static> {
    /// A window holding nothing: any read from it is a miss.
    const EMPTY: WinView<'static> = WinView { data: &[], x0: 0, y0: 0, x1: 0, y1: 0, stride: 0 };
}

/// Nudge that decides a source-pixel centre landing exactly on a cell edge.
///
/// Grids that line up exactly are the common case — the plugin warps its base
/// DEM onto the analysis grid — and then every cell edge falls on a pixel
/// centre, where a 1-ulp difference in the coordinate arithmetic would decide
/// which cell owns the pixel. This resolves it the same way every time (the
/// low edge belongs to the cell above, the high edge to this one), which keeps
/// the spans tiling and keeps a 3-to-1 average symmetric about its cell instead
/// of leaning half a pixel whichever way the last rounding went. A billionth of
/// a pixel is far below any real geometry and far above the arithmetic's noise.
const SPAN_EPS: f64 = 1e-9;

/// The half-open source-pixel span whose **centres** fall inside one output
/// cell, along one axis.
///
/// `lo`/`hi` are the cell's leading and trailing edges in source-pixel units
/// (`lo < hi`) and `dim` is the source raster's extent on that axis. Source
/// pixel `i` covers `[i, i+1)` with its centre at `i + 0.5`, so the cell
/// `[lo, hi)` owns the indices `i` with `lo <= i + 0.5 < hi`, clipped to the
/// raster. That is `round(lo) .. round(hi)`, with [`SPAN_EPS`] settling an edge
/// that lands exactly on a pixel centre. (`(v + 0.5) as u32` truncates and
/// saturates, so it costs one instruction and maps negatives to 0; `f64::ceil`
/// would lower to a libm call on a baseline x86-64 target, twice per axis per
/// output pixel.)
///
/// **Upsampling falls back to the centre pixel.** When the output cell is finer
/// than a source pixel the span can be empty — no source centre lands in it.
/// Rather than divide by zero or leave the cell void it falls back to the pixel
/// containing the cell's *centre* (callers anchor the window on the cell
/// centre, so lo/hi straddle it). It is also the continuous limit of the span
/// rule: at ratio 1 the one pixel whose centre is inside the cell is the one
/// containing the cell's centre, so nothing jumps half a pixel as the ratio
/// crosses 1.
#[inline]
fn sample_span(lo: f64, hi: f64, dim: u32) -> (u32, u32) {
    if dim == 0 {
        return (0, 0);
    }
    let start = (lo + 0.5 + SPAN_EPS) as u32;
    let end = ((hi + 0.5 + SPAN_EPS) as u32).min(dim);
    if start < end {
        (start, end)
    } else {
        // `as u32` truncates and saturates, so a negative midpoint lands on 0.
        let p = (((lo + hi) * 0.5) as u32).min(dim - 1);
        (p, p + 1)
    }
}

/// Sample one output cell out of one source image: the area-average of the
/// source pixels its footprint covers, in half-metres.
///
/// A cell covering exactly one source pixel — every cell of a source at or
/// coarser than the target, which is the whole of the warped-base-DEM path —
/// reads that pixel straight, with none of the accumulator's machinery, so the
/// common one-sample case costs what the old point sample cost.
///
/// The span is in whole-source pixel indices; *win* holds the decoded part of
/// the source the current row band can reach. A span outside the window is
/// an internal bug (the window maths promises a superset): it sets *miss*
/// instead of reading a wrong pixel or falling through to the next source,
/// and the tile is refused.
#[inline(always)]
fn sample_cell(win: &WinView, px0: u32, px1: u32, py0: u32, py1: u32, miss: &mut bool) -> Option<i16> {
    if px0 < win.x0 || px1 > win.x1 || py0 < win.y0 || py1 > win.y1 {
        *miss = true;
        return None;
    }
    let w = win.stride;
    let a = (px0 - win.x0) as usize;
    let r0 = (py0 - win.y0) as usize;
    if px1 - px0 == 1 && py1 - py0 == 1 {
        let v = win.data[r0 * w + a];
        return if v > -5000 { Some(v) } else { None };
    }
    mean_valid(win.data, w, a, (px1 - win.x0) as usize, r0, (py1 - win.y0) as usize)
}

/// Mean of the **valid** samples of one source-pixel rectangle, in half-metres.
///
/// Voids are excluded from the mean, never averaged into it: a cell beside a
/// coastline or a DEM edge must not be dragged toward -4999.5 m by the no-data
/// pixels next to it. A rectangle holding no valid sample at all returns `None`,
/// and the caller falls through to its next source exactly as a void point
/// sample used to.
///
/// The inner pass is branchless and sums into an `i32`, which is what makes it
/// vectorize — an `i64` accumulator measured 44% slower on a 60×60 footprint.
/// `32768 * 32768` is `i32::MAX + 1`, so the run is split at 32768 samples and
/// widened into `i64` between runs; no real DEM has a row that long, but it
/// splits rather than wraps if one ever does. The division rounds once at the
/// end — accumulating in `i16` would overflow after two mountain pixels.
#[inline]
fn mean_valid(data: &[i16], w: usize, a: usize, b: usize, y0: usize, y1: usize) -> Option<i16> {
    let mut sum: i64 = 0;
    let mut cnt: u32 = 0;
    for py in y0..y1 {
        let row = py * w;
        for run in data[row + a..row + b].chunks(1 << 15) {
            let mut rsum: i32 = 0;
            let mut rcnt: u32 = 0;
            for &v in run {
                let ok = v > -5000;
                rsum += if ok { v as i32 } else { 0 };
                rcnt += ok as u32;
            }
            sum += rsum as i64;
            cnt += rcnt;
        }
    }
    match cnt {
        0 => None,
        // Single-sample cells keep the exact source value — no divide, and no
        // rounding drift on the upsampling path.
        1 => Some(sum as i16),
        // Round half away from zero, integer-only.
        _ => {
            let c = cnt as i64;
            let r = if sum >= 0 {
                (2 * sum + c) / (2 * c)
            } else {
                (2 * sum - c) / (2 * c)
            };
            Some(r as i16)
        }
    }
}

/// EPSG code out of a raw GeoKeyDirectory (tag 34735) array, as `"EPSG:nnnn"`.
///
/// ProjectedCSTypeGeoKey (3072) is consulted first; only when it is absent is
/// GeographicTypeGeoKey (2048) consulted. A *present but user-defined* (32767)
/// projected key does NOT fall back to the geographic key: the pixels are in
/// an unknown projection, and sampling them as the underlying geographic CRS
/// would be silently wrong. `None` means "no usable code" and the caller must
/// error unless the source carries an explicit `crs`.
fn epsg_from_geokeys(dir: &[u16]) -> Option<String> {
    // Header: KeyDirectoryVersion, KeyRevision, MinorRevision, NumberOfKeys;
    // then 4-u16 entries (KeyID, TIFFTagLocation, Count, ValueOffset). A key
    // whose TIFFTagLocation != 0 stores its value in another tag — never the
    // case for these two SHORT-valued keys, so such entries are unusable here.
    if dir.len() < 4 {
        return None;
    }
    let n_keys = dir[3] as usize;
    let mut projected = None;
    let mut geographic = None;
    for i in 0..n_keys {
        let off = 4 + i * 4;
        if off + 4 > dir.len() {
            break;
        }
        let (key_id, location, value) = (dir[off], dir[off + 1], dir[off + 3]);
        if location != 0 {
            continue;
        }
        match key_id {
            3072 => projected = Some(value),
            2048 => geographic = Some(value),
            _ => {}
        }
    }
    let usable = |v: u16| v != 0 && v != 32767;
    match projected {
        Some(v) if usable(v) => Some(format!("EPSG:{v}")),
        Some(_) => None, // user-defined projection: explicit `crs` required
        None => geographic.filter(|&v| usable(v)).map(|v| format!("EPSG:{v}")),
    }
}

/// Inline value of one SHORT-valued GeoKey out of a raw GeoKeyDirectory
/// (tag 34735). Keys whose TIFFTagLocation != 0 store their value in another
/// tag and are not readable here — `None`.
fn geokey_short(dir: &[u16], wanted: u16) -> Option<u16> {
    if dir.len() < 4 {
        return None;
    }
    let n_keys = dir[3] as usize;
    for i in 0..n_keys {
        let off = 4 + i * 4;
        if off + 4 > dir.len() {
            break;
        }
        let (key_id, location, value) = (dir[off], dir[off + 1], dir[off + 3]);
        if key_id == wanted && location == 0 {
            return Some(value);
        }
    }
    None
}

/// Resolve a `crs` string (`"EPSG:nnnn"` or a raw `+proj=…` string) to a proj
/// string. Hard errors name the source file and how to fix the job.
fn crs_to_proj_string(crs: &str, path: &Path) -> Result<String> {
    let s = crs.trim();
    if s.starts_with('+') {
        return Ok(s.to_string());
    }
    if let Some(code_str) = s.get(..5).filter(|p| p.eq_ignore_ascii_case("EPSG:")).map(|_| &s[5..]) {
        let code: u16 = code_str.trim().parse().map_err(|_| {
            anyhow::anyhow!(
                "source {:?}: crs {:?} is not a valid EPSG code; use \"EPSG:nnnn\" \
                 or a proj string starting with '+'",
                path, s
            )
        })?;
        return match crs_definitions::from_code(code) {
            Some(def) => Ok(def.proj4.to_string()),
            None => anyhow::bail!(
                "source {:?}: unknown EPSG code {} (not in the built-in registry); \
                 supply the projection as a proj string in \"crs\" instead, e.g. \
                 \"+proj=… +ellps=… +units=m\"",
                path, code
            ),
        };
    }
    anyhow::bail!(
        "source {:?}: crs {:?} is neither \"EPSG:nnnn\" nor a proj string starting \
         with '+'; fix the \"crs\" field of this source",
        path, s
    )
}

/// How one resolved source is sampled.
enum SourceKind {
    /// Geographic CRS: pixel coordinates are lon/lat degrees and are sampled
    /// directly against the output's WGS84 grid — the historical `base_tif`
    /// path, kept byte-identical. Decision: *any* geographic CRS is treated as
    /// WGS84 degrees (datum shifts are metres — far below a DEM pixel).
    Geographic,
    /// Projected CRS: WGS84 -> source via proj4rs, by index into the job's
    /// projection group table (sources sharing a CRS share row transforms).
    Projected(usize),
}

struct ProjGroup {
    proj: Arc<Proj>,
    /// Finest source pixel size (metres) among the group's members — the unit
    /// of the row-linearity check.
    min_scale: f64,
}

/// Open *path* and tell an `.abt` tile from a GeoTIFF by the `AETH` magic
/// (never by extension). Returns the file with its cursor just past the magic
/// for an `.abt`, rewound to 0 for anything else.
fn open_source(path: &Path, nodata_override: Option<f64>) -> Result<(File, bool)> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = File::open(path).with_context(|| format!("Opening {:?}", path))?;
    let mut magic = [0u8; 4];
    let got = file.read(&mut magic).with_context(|| format!("Reading {:?}", path))?;
    if got == 4 && magic == *b"AETH" {
        if nodata_override.is_some() {
            anyhow::bail!(
                "source {:?}: .abt sources are self-describing — remove \"nodata\" \
                 from this source (its voids are already the -9999 sentinel)",
                path
            );
        }
        return Ok((file, true));
    }
    file.seek(SeekFrom::Start(0))
        .with_context(|| format!("Reading {:?}", path))?;
    Ok((file, false))
}

/// [`open_source`] for the streaming path: the file behind a [`BlockReader`]
/// whose first block — fetched with one positioned read — also answers the
/// magic sniff. The reader sits just past the magic for an `.abt`, at 0 for
/// anything else. Also returns a duplicate of the handle (`None` if the
/// system refuses one), which shares the open file: the window reads reuse
/// it instead of opening the file again.
fn open_source_blocks(path: &Path, nodata_override: Option<f64>) -> Result<(BlockReader, bool, Option<File>)> {
    use std::io::{Seek, SeekFrom};
    let file = open_counted(path).with_context(|| format!("Opening {:?}", path))?;
    let keep = file.try_clone().ok();
    let mut reader = BlockReader::new(file);
    let is_abt = reader.head(4).with_context(|| format!("Reading {:?}", path))? == b"AETH";
    if is_abt {
        if nodata_override.is_some() {
            anyhow::bail!(
                "source {:?}: .abt sources are self-describing — remove \"nodata\" \
                 from this source (its voids are already the -9999 sentinel)",
                path
            );
        }
        reader.seek(SeekFrom::Start(4)).with_context(|| format!("Reading {:?}", path))?;
    }
    Ok((reader, is_abt, keep))
}

/// A GeoTIFF decoder over *reader* with the decode-buffer cap lifted.
///
/// The default tiff cap (~256 MB) rejects large rasters with "The Decoder
/// limits are exceeded"; reads are bounded by the band machinery instead.
fn tiff_decoder<R: std::io::Read + std::io::Seek>(reader: R) -> Result<Decoder<R>> {
    Ok(Decoder::new(reader)?.with_limits(Limits::unlimited()))
}

/// Width of one stored chunk row: `(samples, bytes)`. A strip is as wide as
/// the image; a tile is its stored width, padding included. `None` for
/// sub-byte sample depths, which keep the plain `read_chunk` path.
fn chunk_row<R: std::io::Read + std::io::Seek>(dec: &mut Decoder<R>) -> Result<Option<(usize, usize)>> {
    let bits = match dec.colortype()? {
        tiff::ColorType::Gray(b) => b,
        tiff::ColorType::Multiband { bit_depth, num_samples: 1 } => bit_depth,
        _ => return Ok(None),
    };
    if bits == 0 || bits % 8 != 0 {
        return Ok(None);
    }
    let samples = if matches!(dec.get_chunk_type(), tiff::decoder::ChunkType::Tile) {
        dec.chunk_dimensions().0
    } else {
        dec.dimensions()?.0
    } as usize;
    Ok(Some((samples, samples * (bits as usize / 8))))
}

/// Decode chunk `idx` to its `dw x dh` data samples, row-major.
///
/// The chunk is read WHOLE — output row stride = the stored chunk row — and
/// the right-edge padding is cropped here. `read_chunk` / `read_image`
/// instead read a padded tile row by row and skip the padding with a small
/// read; weezl 0.1 (required by tiff 0.11) can answer that read with
/// `NoProgress` although it produced output, and tiff turns it into
/// "no lzw end code found". Every right-edge tile of a tiled LZW GeoTIFF
/// whose width is not a multiple of the tile width can hit it (swissALTI3D
/// 2 m: 500 px, 128² tiles). image-rs/image-tiff#395; fixed upstream only by
/// the weezl 0.2 upgrade (unreleased as of tiff 0.11.3). Go back to
/// `read_chunk` once a tiff release on weezl 0.2 is in the lockfile — the
/// canary in tests/test_lzw_edge_tiles.rs says when.
fn decode_chunk<R: std::io::Read + std::io::Seek>(
    dec: &mut Decoder<R>,
    idx: u32,
    row: Option<(usize, usize)>,
    nodata: Option<f64>,
    value_xform: Option<(f64, f64)>,
) -> Result<Vec<i16>> {
    let Some((row_samples, row_bytes)) = row else {
        return Ok(convert_samples(dec.read_chunk(idx)?, nodata, value_xform));
    };
    let (dw, dh) = dec.chunk_data_dimensions(idx);
    let mut buf = DecodingResult::U8(Vec::new());
    dec.read_chunk_to_buffer(&mut buf, idx, row_bytes)?;
    crop_chunk(convert_samples(buf, nodata, value_xform), idx, row_samples, dw as usize, dh as usize)
}

/// Crop a chunk decoded in full stored rows of *row_samples* to its
/// `dw x dh` data samples (the right-edge padding of a tile goes).
fn crop_chunk(mut full: Vec<i16>, idx: u32, row_samples: usize, dw: usize, dh: usize) -> Result<Vec<i16>> {
    if dw == row_samples {
        full.truncate(dw * dh);
        return Ok(full);
    }
    if dw > row_samples || full.len() < (dh - 1) * row_samples + dw {
        anyhow::bail!(
            "TIFF chunk {}: {} samples decoded for {}x{} data in {}-sample rows",
            idx, full.len(), dw, dh, row_samples
        );
    }
    let mut out = Vec::with_capacity(dw * dh);
    for y in 0..dh {
        out.extend_from_slice(&full[y * row_samples..y * row_samples + dw]);
    }
    Ok(out)
}

/// TIFF compression codes this build decodes. Everything else is refused up
/// front, naming the file and the codec, so a caller can route that one file
/// through GDAL instead of losing the batch.
fn compression_supported(code: u16) -> bool {
    match code {
        1 | 5 | 7 | 8 | 32946 | 32773 | 4 | 50001 => true, // none, LZW, JPEG, Deflate×2, PackBits, Fax4, WebP
        // ZSTD links the C libzstd and is enabled for native builds only
        // (crates/aether_converter/Cargo.toml).
        50000 => cfg!(not(target_arch = "wasm32")),
        _ => false,
    }
}

/// Human name of a TIFF compression code, for the refusal message.
fn compression_name(code: u16) -> &'static str {
    match code {
        1 => "NONE",
        2 => "CCITTRLE",
        3 => "CCITTFAX3",
        4 => "CCITTFAX4",
        5 => "LZW",
        6 => "OJPEG",
        7 => "JPEG",
        8 | 32946 => "DEFLATE",
        32773 => "PACKBITS",
        34661 => "JBIG",
        34676 | 34677 => "SGILOG",
        34712 => "JPEG2000",
        34887 => "LERC",
        34925 => "LZMA",
        50000 => "ZSTD",
        50001 => "WEBP",
        50002 | 52546 => "JXL",
        _ => "UNKNOWN",
    }
}

/// Value of one attribute (`name="v"` or `name='v'`) inside an XML start tag.
fn xml_attr<'a>(attrs: &'a str, name: &str) -> Option<&'a str> {
    let mut rest = attrs;
    while let Some(i) = rest.find(name) {
        let before_ok = i == 0 || rest.as_bytes()[i - 1].is_ascii_whitespace();
        let after = rest[i + name.len()..].trim_start();
        if before_ok {
            if let Some(after_eq) = after.strip_prefix('=') {
                let after_eq = after_eq.trim_start();
                let q = after_eq.chars().next()?;
                if q == '"' || q == '\'' {
                    let body = &after_eq[1..];
                    return body.find(q).map(|end| &body[..end]);
                }
            }
        }
        rest = &rest[i + name.len()..];
    }
    None
}

/// Band-1 `(scale, offset)` out of a GDAL_METADATA (tag 42112) XML document:
/// `<Item name="SCALE" sample="0" role="scale">0.1</Item>` and its OFFSET
/// twin. `Ok(None)` when absent or the identity (1, 0) — such a file decodes
/// exactly as before. A present but unparseable value is an error: guessing
/// would scale every elevation wrongly without a trace.
fn gdal_scale_offset(xml: &str, path: &Path) -> Result<Option<(f64, f64)>> {
    let mut scale: Option<f64> = None;
    let mut offset: Option<f64> = None;
    let mut rest = xml;
    while let Some(i) = rest.find("<Item") {
        rest = &rest[i + 5..];
        let Some(gt) = rest.find('>') else { break };
        let attrs = &rest[..gt];
        if attrs.trim_end().ends_with('/') {
            rest = &rest[gt + 1..];
            continue; // <Item ... /> carries no value
        }
        let body = &rest[gt + 1..];
        let Some(end) = body.find("</Item>") else { break };
        let value = body[..end].trim();
        rest = &body[end..];
        if xml_attr(attrs, "sample") != Some("0") {
            continue; // dataset-level metadata, not band 1
        }
        let role = xml_attr(attrs, "role")
            .or_else(|| xml_attr(attrs, "name"))
            .unwrap_or("")
            .to_ascii_lowercase();
        let slot = match role.as_str() {
            "scale" => &mut scale,
            "offset" => &mut offset,
            _ => continue,
        };
        let v: f64 = value.parse().ok().filter(|v: &f64| v.is_finite()).ok_or_else(|| {
            anyhow::anyhow!(
                "source {:?}: GDAL_METADATA {} {:?} is not a finite number — the \
                 elevations cannot be scaled; fix or strip the tag (gdal_edit.py \
                 -scale/-offset)",
                path, role.to_ascii_uppercase(), value
            )
        })?;
        *slot = Some(v);
    }
    let (s, o) = (scale.unwrap_or(1.0), offset.unwrap_or(0.0));
    Ok(if s == 1.0 && o == 0.0 { None } else { Some((s, o)) })
}

/// Everything a GeoTIFF's header says about sampling it, plus the decode
/// parameters. Shared by the streaming and the whole-file readers, so both
/// refuse exactly the same files with exactly the same messages.
struct TiffHeader {
    geom: SourceGeom,
    nodata: Option<f64>,
    value_xform: Option<(f64, f64)>,
}

fn read_tiff_header<R: std::io::Read + std::io::Seek>(
    decoder: &mut Decoder<R>,
    path: &Path,
    nodata_override: Option<f64>,
) -> Result<TiffHeader> {
    let (w, h) = decoder.dimensions()?;

    // A picture is not terrain. Elevation rasters carry ONE sample per
    // pixel; an RGB(A)/palette source — a rendered WMS/XYZ basemap export,
    // a hillshade — would have its colour values read as metres and produce
    // confident garbage. Refuse by band count/photometric, never by sample
    // type: genuine UInt8 single-band DEMs stay ingestable.
    let color = decoder.colortype()?;
    match color {
        tiff::ColorType::Gray(_) => {}
        tiff::ColorType::Multiband { num_samples: 1, .. } => {}
        other => anyhow::bail!(
            "source {:?}: {:?} pixels — this is an image (a rendered map, \
             photo or hillshade), not an elevation raster. Elevation sources \
             are single-band; converting a picture would read its colour \
             values as metres. Use a real DEM for this area instead.",
            path,
            other
        ),
    }

    // Refused before any pixel is touched, naming file and codec — the old
    // reader got as far as the decode and aborted the whole batch with the
    // tiff crate's bare "compression method Unknown(50000) is unsupported".
    let compression = decoder
        .find_tag_unsigned::<u16>(Tag::Compression)
        .ok()
        .flatten()
        .unwrap_or(1);
    if !compression_supported(compression) {
        anyhow::bail!(
            "source {:?}: unsupported compression {} ({}) — this build cannot \
             decode it. Re-encode the file (e.g. `gdal_translate -co \
             COMPRESS=DEFLATE`) or convert it through GDAL first.",
            path,
            compression_name(compression),
            compression
        );
    }

    let model_trans = decoder.get_tag_f64_vec(Tag::ModelTransformationTag).unwrap_or_default();
    let tiepoints = decoder.get_tag_f64_vec(Tag::ModelTiepointTag).unwrap_or_default();
    let pixel_scales = decoder.get_tag_f64_vec(Tag::ModelPixelScaleTag).unwrap_or_default();
    let geokey_dir = decoder.get_tag_u16_vec(Tag::GeoKeyDirectoryTag).ok();

    let (mut origin_e, mut origin_n, scale_x, scale_y) = if model_trans.len() == 16 {
        // Terms [1] and [4] rotate/shear the grid. Sampling assumes a
        // north-up grid, and ignoring them placed such a file wrongly with
        // no error at all.
        if model_trans[1] != 0.0 || model_trans[4] != 0.0 {
            anyhow::bail!(
                "source {:?}: rotated/sheared geotransform is not supported — warp \
                 it to a north-up grid first (e.g. `gdalwarp`); its \
                 ModelTransformation carries rotation terms [1]={} and [4]={}",
                path,
                model_trans[1],
                model_trans[4]
            );
        }
        (model_trans[3], model_trans[7], model_trans[0].abs(), model_trans[5].abs())
    } else if tiepoints.len() >= 6 && pixel_scales.len() >= 2 {
        (tiepoints[3], tiepoints[4], pixel_scales[0], pixel_scales[1].abs())
    } else {
        // Filename-derived georeferencing is gone for good: a wrong guess
        // placed terrain kilometres off and the run still "succeeded".
        anyhow::bail!(
            "source {:?}: no geotransform — the file carries neither a \
             ModelTransformation tag nor a ModelTiepoint+ModelPixelScale pair, \
             so it is not georeferenced. Re-export it as a GeoTIFF with a \
             geotransform (e.g. `gdal_translate`); georeferencing is never \
             derived from file names.",
            path
        );
    };

    // GTRasterTypeGeoKey (1025) = 2, RasterPixelIsPoint: the origin above
    // names the CENTRE of pixel (0,0), not its NW corner (Copernicus and
    // SRTM ship this way). Shift to the corner so sampling is not half a
    // source pixel off. GDAL applies the same correction on read.
    if geokey_dir
        .as_deref()
        .and_then(|dir| geokey_short(dir, 1025))
        == Some(2)
    {
        origin_e -= 0.5 * scale_x;
        origin_n += 0.5 * scale_y;
    }

    let embedded_crs = geokey_dir.as_deref().and_then(epsg_from_geokeys);

    // Per-source nodata: the explicit job field wins; else the GDAL_NODATA
    // ascii tag (trimmed, NUL-stripped; "nan"/"NaN" parse to f64 NaN and the
    // existing non-finite->void rule already covers that case).
    let nodata: Option<f64> = nodata_override.or_else(|| {
        decoder
            .get_tag_ascii_string(Tag::GdalNodata)
            .ok()
            .and_then(|s| s.trim_matches(['\0', ' ', '\t', '\r', '\n']).parse::<f64>().ok())
    });

    // GDAL_METADATA (42112): band scale/offset. A packed Int16 DEM in
    // decimetres (scale 0.1) used to come out ten times too high.
    let value_xform = match decoder.get_tag_ascii_string(Tag::Unknown(42112)) {
        Ok(xml) => gdal_scale_offset(&xml, path)?,
        Err(_) => None,
    };

    let limit_n = origin_n - (h as f64 * scale_y);
    let limit_e = origin_e + (w as f64 * scale_x);
    Ok(TiffHeader {
        geom: SourceGeom {
            width: w,
            height: h,
            origin_e,
            origin_n,
            limit_e,
            limit_n,
            scale_x,
            scale_y,
            embedded_crs,
            is_abt: false,
        },
        nodata,
        value_xform,
    })
}

/// Raw samples -> half-metres through GDAL's `value * scale + offset`.
/// Nodata is matched on the RAW sample (GDAL's convention: nodata is stored
/// in the file's own units), before the transform.
#[inline]
fn scaled_samples<T: Copy>(
    v: Vec<T>,
    to_f64: impl Fn(T) -> f64,
    is_nodata: impl Fn(T) -> bool,
    scale: f64,
    offset: f64,
) -> Vec<i16> {
    v.into_iter()
        .map(|x| if is_nodata(x) { VOID_ELEV } else { f64_sample_to_half_metres(to_f64(x) * scale + offset) })
        .collect()
}

/// Decoded samples of any type -> half-metres, honouring *nodata* and the
/// optional GDAL scale/offset. Without a scale/offset this is exactly the
/// historical per-type conversion; it is applied identically to a whole image
/// and to each strip/tile of one, so the two decode paths cannot differ.
fn convert_samples(result: DecodingResult, nodata: Option<f64>, value_xform: Option<(f64, f64)>) -> Vec<i16> {
    if let Some((scale, offset)) = value_xform {
        let nd = nodata;
        let nd32 = nd.map(|n| n as f32);
        return match result {
            DecodingResult::F32(v) => scaled_samples(v, f64::from, |x| Some(x) == nd32, scale, offset),
            DecodingResult::F64(v) => scaled_samples(v, |x| x, |x| Some(x) == nd, scale, offset),
            DecodingResult::F16(v) => {
                scaled_samples(v, |x| f64::from(x.to_f32()), |x| Some(x.to_f32()) == nd32, scale, offset)
            }
            DecodingResult::I8(v) => scaled_samples(v, f64::from, |x| Some(f64::from(x)) == nd, scale, offset),
            DecodingResult::U8(v) => scaled_samples(v, f64::from, |x| Some(f64::from(x)) == nd, scale, offset),
            DecodingResult::I16(v) => scaled_samples(v, f64::from, |x| Some(f64::from(x)) == nd, scale, offset),
            DecodingResult::U16(v) => scaled_samples(v, f64::from, |x| Some(f64::from(x)) == nd, scale, offset),
            DecodingResult::I32(v) => scaled_samples(v, f64::from, |x| Some(f64::from(x)) == nd, scale, offset),
            DecodingResult::U32(v) => scaled_samples(v, f64::from, |x| Some(f64::from(x)) == nd, scale, offset),
            DecodingResult::I64(v) => scaled_samples(v, |x| x as f64, |x| Some(x as f64) == nd, scale, offset),
            DecodingResult::U64(v) => scaled_samples(v, |x| x as f64, |x| Some(x as f64) == nd, scale, offset),
        };
    }
    match result {
        DecodingResult::F32(v) => match nodata {
            // Compare in the source unit, before scaling. The f32 comparison
            // uses the round-tripped f64->f32 value so a tag like
            // "-3.402823466e+38" matches the stored Float32 exactly.
            Some(nd) => {
                let nd32 = nd as f32;
                v.iter()
                    .map(|&x| if x == nd32 { VOID_ELEV } else { f32_sample_to_half_metres(x) })
                    .collect()
            }
            None => v.iter().map(|&x| f32_sample_to_half_metres(x)).collect(),
        },
        DecodingResult::I16(v) => match nodata {
            Some(nd) => v
                .iter()
                .map(|&x| if f64::from(x) == nd { VOID_ELEV } else { x.saturating_mul(2) })
                .collect(),
            None => v.iter().map(|&x| x.saturating_mul(2)).collect(),
        },
        DecodingResult::I32(v) => match nodata {
            Some(nd) => v
                .iter()
                .map(|&x| if f64::from(x) == nd { VOID_ELEV } else { i32_sample_to_half_metres(x) })
                .collect(),
            None => v.iter().map(|&x| i32_sample_to_half_metres(x)).collect(),
        },
        // Every remaining width the `tiff` crate can hand back. The three arms
        // above keep their own comparison rules verbatim (the F32 one rounds
        // the nodata tag through f32 on purpose); these are additive.
        DecodingResult::U8(v) => int_samples(v, nodata),
        DecodingResult::U16(v) => int_samples(v, nodata),
        DecodingResult::U32(v) => int_samples(v, nodata),
        DecodingResult::I8(v) => int_samples(v, nodata),
        DecodingResult::I64(v) => int_samples(v, nodata),
        // u64 is the one width that does not fit i64; saturate rather than
        // wrap, and let the half-metre rule clamp it to i16::MAX.
        DecodingResult::U64(v) => int_samples(
            v.into_iter().map(|x| x.min(i64::MAX as u64) as i64).collect::<Vec<i64>>(),
            nodata,
        ),
        // 16-bit float: widened to f32 and handled by the f32 rule. Named
        // through `to_f32()` so `half` stays out of this crate's dependencies.
        DecodingResult::F16(v) => match nodata {
            Some(nd) => {
                let nd32 = nd as f32;
                v.iter()
                    .map(|&x| {
                        let f = x.to_f32();
                        if f == nd32 { VOID_ELEV } else { f32_sample_to_half_metres(f) }
                    })
                    .collect()
            }
            None => v.iter().map(|&x| f32_sample_to_half_metres(x.to_f32())).collect(),
        },
        DecodingResult::F64(v) => match nodata {
            Some(nd) => v
                .iter()
                .map(|&x| if x == nd { VOID_ELEV } else { f64_sample_to_half_metres(x) })
                .collect(),
            None => v.iter().map(|&x| f64_sample_to_half_metres(x)).collect(),
        },
    }
}

/// Load one terrain source — a GeoTIFF or an `.abt` tile — WHOLE into RAM.
///
/// This is the historical whole-file path. `ingest` itself no longer uses it
/// (it streams windows of each source, see [`process_tile_opts`]); it stays
/// public for callers that pre-decode a source into an [`ImageCache`], and as
/// the reference the streaming path is tested byte-identical against.
///
/// `nodata_override` is the source's explicit `nodata` field; for a GeoTIFF
/// it wins over the file's GDAL_NODATA tag, and matching samples become
/// VOID_ELEV *here*, at load time, which keeps the samplers' `> -5000`
/// validity test unchanged. An `.abt` source refuses the override (it is
/// self-describing; its voids are already `-9999`).
pub fn load_source_to_ram(path: &Path, nodata_override: Option<f64>) -> Result<Arc<LoadedImage>> {
    let (file, is_abt) = open_source(path, nodata_override)?;
    if is_abt {
        return load_abt_to_ram(path, file);
    }
    let mut decoder = tiff_decoder(BufReader::with_capacity(1024 * 1024, file))?;
    let hdr = read_tiff_header(&mut decoder, path, nodata_override)?;
    let (w, h) = (hdr.geom.width, hdr.geom.height);

    // Assembled chunk by chunk through `decode_chunk`, not `read_image`: the
    // latter takes the padded-row path that trips weezl 0.1 (see there).
    let tiled = matches!(decoder.get_chunk_type(), tiff::decoder::ChunkType::Tile);
    let (cw, ch) = decoder.chunk_dimensions();
    if cw == 0 || ch == 0 {
        anyhow::bail!("source {:?}: zero-sized TIFF strip/tile ({}x{})", path, cw, ch);
    }
    let (cw, across) = if tiled { (cw, w.div_ceil(cw)) } else { (w, 1) };
    let row = chunk_row(&mut decoder)?;
    let mut data = vec![VOID_ELEV; w as usize * h as usize];
    for cy in 0..h.div_ceil(ch) {
        for cx in 0..across {
            let chunk = decode_chunk(&mut decoder, cy * across + cx, row, hdr.nodata, hdr.value_xform)?;
            let (x0, y0) = ((cx * cw) as usize, (cy * ch) as usize);
            let dw = (cw as usize).min(w as usize - x0);
            let dh = (ch as usize).min(h as usize - y0);
            if chunk.len() != dw * dh {
                anyhow::bail!(
                    "source {:?}: decoded {} samples for a {}x{} chunk of a {}x{} \
                     raster — the file is not a single-band elevation grid",
                    path, chunk.len(), dw, dh, w, h
                );
            }
            for y in 0..dh {
                let dst = (y0 + y) * w as usize + x0;
                data[dst..dst + dw].copy_from_slice(&chunk[y * dw..(y + 1) * dw]);
            }
        }
    }

    // Belt over the braces above: the samplers index `y*w + x`, so a decode
    // that returned anything but exactly w*h samples (interleaved bands,
    // truncated strips) must never reach them.
    if data.len() != w as usize * h as usize {
        anyhow::bail!(
            "source {:?}: decoded {} samples for a {}x{} raster — the file \
             is not a single-band elevation grid",
            path,
            data.len(),
            w,
            h
        );
    }

    let g = hdr.geom;
    Ok(Arc::new(LoadedImage {
        width: w,
        height: h,
        data,
        origin_e: g.origin_e,
        origin_n: g.origin_n,
        limit_e: g.limit_e,
        limit_n: g.limit_n,
        scale_x: g.scale_x,
        scale_y: g.scale_y,
        name: path.file_name().unwrap_or_default().to_string_lossy().to_string(),
        embedded_crs: g.embedded_crs,
        is_abt: false,
    }))
}

/// Parse the header of a file source for streaming: geometry, decode
/// parameters and chunk layout — no pixels. Raises every header-level hard
/// error (no geotransform, rotation, picture, codec, `.abt` corruption) the
/// whole-file path raises, before any pixel is decoded.
///
/// The parse runs over a [`BlockReader`]: the IFD and its tag data cost a few
/// 16 KB positioned reads wherever they sit, not a refill of a large buffer
/// at every seek the `tiff` crate makes.
///
/// Also returns a second handle to the file it opened (when one could be
/// made), for the window reads that follow.
fn read_file_source_keep(path: &Path, nodata_override: Option<f64>) -> Result<(FileSource, Option<File>)> {
    let (mut reader, is_abt, keep) = open_source_blocks(path, nodata_override)?;
    if is_abt {
        let (geom, size, stride) = read_abt_header(path, &mut reader)?;
        return Ok((
            FileSource {
                id: 0,
                path: path.to_path_buf(),
                geom,
                layout: FileLayout::Abt { size, stride },
                wgs84_bbox: None,
            },
            keep,
        ));
    }
    let mut decoder = tiff_decoder(reader)?;
    let hdr = read_tiff_header(&mut decoder, path, nodata_override)?;
    let tiled = matches!(decoder.get_chunk_type(), tiff::decoder::ChunkType::Tile);
    let (chunk_w, chunk_h) = decoder.chunk_dimensions();
    if chunk_w == 0 || chunk_h == 0 {
        anyhow::bail!("source {:?}: zero-sized TIFF strip/tile ({}x{})", path, chunk_w, chunk_h);
    }
    let across = if tiled { hdr.geom.width.div_ceil(chunk_w) } else { 1 };
    let chunk_w = if tiled { chunk_w } else { hdr.geom.width };
    let chunks = across as usize * hdr.geom.height.div_ceil(chunk_h) as usize;
    let direct = direct_chunks(&mut decoder, tiled, chunk_w as usize, chunks);
    Ok((
        FileSource {
            id: 0,
            path: path.to_path_buf(),
            geom: hdr.geom,
            layout: FileLayout::Tiff {
                nodata: hdr.nodata,
                value_xform: hdr.value_xform,
                chunk_w,
                chunk_h,
                across,
                direct,
            },
            wgs84_bbox: None,
        },
        keep,
    ))
}

/// [`read_file_source_keep`] without the handle.
#[cfg(test)]
fn read_file_source(path: &Path, nodata_override: Option<f64>) -> Result<FileSource> {
    read_file_source_keep(path, nodata_override).map(|(src, _)| src)
}

/// The header facts of the image *dec* points at that the direct chunk
/// decoder needs, read from the already-parsed IFD (the offset/byte-count
/// arrays come out of the block reader's cache). `None` when the file is
/// outside what [`DirectChunks`] mirrors.
fn direct_chunks<R: std::io::Read + std::io::Seek>(
    dec: &mut Decoder<R>,
    tiled: bool,
    row_samples: usize,
    chunks: usize,
) -> Option<DirectChunks> {
    fn short<R: std::io::Read + std::io::Seek>(dec: &mut Decoder<R>, tag: Tag) -> Option<u16> {
        dec.find_tag_unsigned::<u16>(tag).ok().flatten()
    }
    let (off_tag, cnt_tag) = if tiled {
        (Tag::TileOffsets, Tag::TileByteCounts)
    } else {
        (Tag::StripOffsets, Tag::StripByteCounts)
    };
    DirectChunks::from_header(ChunkHeader {
        compression: short(dec, Tag::Compression).unwrap_or(1),
        predictor: short(dec, Tag::Predictor).unwrap_or(1),
        photometric: short(dec, Tag::PhotometricInterpretation),
        samples_per_pixel: short(dec, Tag::SamplesPerPixel).unwrap_or(1),
        bits_per_sample: dec.find_tag_unsigned_vec(Tag::BitsPerSample).ok().flatten().unwrap_or_else(|| vec![1]),
        sample_format: dec.find_tag_unsigned_vec(Tag::SampleFormat).ok().flatten().unwrap_or_default(),
        planar_config: short(dec, Tag::PlanarConfiguration).unwrap_or(1),
        byte_order: dec.byte_order(),
        offsets: dec.get_tag_u64_vec(off_tag).ok()?,
        byte_counts: dec.get_tag_u64_vec(cnt_tag).ok()?,
        row_samples,
        chunks,
    })
}

/// Chunks larger than the shared cache admits, kept for one tile only so a
/// source written as one giant strip is still decoded once per tile, not once
/// per band.
type TileHold = std::sync::Mutex<HashMap<ChunkKey, Arc<Vec<i16>>>>;

/// One chunk a window needs: key, image-pixel origin and data size.
#[derive(Clone, Copy)]
struct Wanted {
    key: ChunkKey,
    cx0: u32,
    cy0: u32,
    dw: u32,
    dh: u32,
}

/// Largest positioned read a window makes in one call; a single chunk larger
/// than this is still read in one.
const MAX_RUN_BYTES: u64 = 8 << 20;
/// Chunks this close in the file share one read (the gap is read and dropped).
const MAX_RUN_GAP: u64 = 16 << 10;

/// One positioned read covering several chunks' file ranges.
struct Run {
    start: u64,
    end: u64,
    /// `(offset, len, chunk)` in file order.
    items: Vec<(u64, u64, Wanted)>,
}

/// Group chunk file ranges into reads: sorted by offset, neighbours at most
/// [`MAX_RUN_GAP`] apart share one read of at most [`MAX_RUN_BYTES`].
fn plan_runs(mut items: Vec<(u64, u64, Wanted)>) -> Vec<Run> {
    items.sort_by_key(|it| it.0);
    let mut runs: Vec<Run> = Vec::new();
    for (off, len, w) in items {
        let end = off.saturating_add(len);
        if let Some(r) = runs.last_mut() {
            if off <= r.end.saturating_add(MAX_RUN_GAP) && end.max(r.end) - r.start <= MAX_RUN_BYTES {
                r.end = r.end.max(end);
                r.items.push((off, len, w));
                continue;
            }
        }
        runs.push(Run { start: off, end, items: vec![(off, len, w)] });
    }
    runs
}

/// Reusable buffers: the smallest one of at least `n` elements (and at
/// most four times that, so a small request does not pin a huge buffer),
/// cut or grown to exactly `n`. The kept elements are stale — callers use
/// this only for buffers they overwrite completely; growth is zero-filled.
fn take_buf<T: Copy + Default>(pool: &std::sync::Mutex<Vec<Vec<T>>>, n: usize) -> Vec<T> {
    let found = {
        let mut g = pool.lock().unwrap_or_else(|p| p.into_inner());
        let best = g
            .iter()
            .enumerate()
            .filter(|(_, b)| b.capacity() >= n && b.capacity() / 4 <= n)
            .min_by_key(|(_, b)| b.capacity())
            .map(|(i, _)| i);
        best.map(|i| g.swap_remove(i))
    };
    match found {
        Some(mut b) => {
            if b.len() >= n {
                b.truncate(n);
            } else {
                b.resize(n, T::default());
            }
            b
        }
        None => vec![T::default(); n],
    }
}

/// Return a buffer to *pool*, which keeps at most *keep_bytes* of them.
fn give_buf<T>(pool: &std::sync::Mutex<Vec<Vec<T>>>, b: Vec<T>, keep_bytes: usize) {
    let mut g = pool.lock().unwrap_or_else(|p| p.into_inner());
    let held: usize = g.iter().map(|v| v.capacity()).sum();
    if (held + b.capacity()) * std::mem::size_of::<T>() <= keep_bytes {
        g.push(b);
    }
}

/// Read buffers, shared by the I/O threads (that fill them) and the decode
/// threads (that hand them back).
fn read_bufs() -> &'static std::sync::Mutex<Vec<Vec<u8>>> {
    static BUFS: OnceLock<std::sync::Mutex<Vec<Vec<u8>>>> = OnceLock::new();
    BUFS.get_or_init(|| std::sync::Mutex::new(Vec::new()))
}

/// Bytes of read buffers kept for reuse, process-wide.
const KEEP_READ_BYTES: usize = 16 << 20;

/// Hand a decoded run's bytes back for the next read.
fn recycle_read_buf(b: Vec<u8>) {
    give_buf(read_bufs(), b, KEEP_READ_BYTES);
}

/// Read `[start, end)` with one positioned read; the buffer is short only at
/// end of file. A run past [`MAX_RUN_BYTES`] (one huge chunk) is clamped to
/// the file's length first, so a corrupt byte count cannot allocate past it.
fn read_run(file: &File, path: &Path, start: u64, end: u64) -> Result<Vec<u8>> {
    let mut len = end - start;
    if len > MAX_RUN_BYTES {
        let file_len = file.metadata().with_context(|| format!("Reading {:?}", path))?.len();
        len = len.min(file_len.saturating_sub(start));
    }
    // A reused buffer's stale bytes are overwritten by the read, and cut
    // off where the file ends.
    let mut buf = take_buf(read_bufs(), len as usize);
    let got = read_full_at(file, &mut buf, start).with_context(|| format!("Reading {:?}", path))?;
    buf.truncate(got);
    Ok(buf)
}

/// Decoded-window buffers of one tile, handed back at the end of each band
/// for the next band's windows.
type WinPool = std::sync::Mutex<Vec<Vec<i16>>>;

/// Bytes of window buffers a tile keeps between bands (the windows of a
/// band fit its band budget, so this keeps what the next band will reuse).
const KEEP_WIN_BYTES: usize = DEFAULT_BAND_BUDGET_BYTES;

/// The chunks of *src* a window `[x0, x1) x [y0, y1)` needs, with the
/// rectangle of image pixels each holds.
fn window_chunks(src: &FileSource, (x0, x1, y0, y1): (u32, u32, u32, u32)) -> Vec<Wanted> {
    let g = &src.geom;
    let (cw, ch, across) = match &src.layout {
        FileLayout::Tiff { chunk_w, chunk_h, across, .. } => (*chunk_w, *chunk_h, *across),
        FileLayout::Abt { size, .. } => (*size as u32, ABT_STRIP_ROWS.min(*size as u32), 1),
    };
    let mut wanted: Vec<Wanted> = Vec::new();
    for cy in y0 / ch..=(y1 - 1) / ch {
        for cx in x0 / cw..=(x1 - 1) / cw {
            let (cx0, cy0) = (cx * cw, cy * ch);
            let dw = cw.min(g.width - cx0);
            let dh = ch.min(g.height - cy0);
            wanted.push(Wanted { key: (src.id, cy * across + cx), cx0, cy0, dw, dh });
        }
    }
    wanted
}

/// The file range `(offset, len)` that holds chunk *w*, for the layouts read
/// with positioned reads (`None`: the `tiff` decoder reads it itself).
fn chunk_extent(src: &FileSource, w: &Wanted) -> Option<(u64, u64)> {
    match &src.layout {
        FileLayout::Abt { size, stride } => {
            let off = ABT_HEADER_BYTES + u64::from(w.cy0) * *stride as u64;
            Some((off, (w.dh as u64 - 1) * *stride as u64 + (*size * 2) as u64))
        }
        FileLayout::Tiff { direct: Some(direct), .. } => Some(direct.extent(w.key.1, w.dh as usize)),
        FileLayout::Tiff { direct: None, .. } => None,
    }
}

/// One positioned read issued for a window: the chunks of one run and the
/// ticket its bytes arrive in.
struct RunRead {
    start: u64,
    /// `(offset, len, chunk)` in file order.
    items: Vec<(u64, u64, Wanted)>,
    bytes: Arc<Ticket<Result<Vec<u8>>>>,
}

/// The source reads of one tile, for the `[Rust] tile` line: bytes read
/// (on the I/O threads) and which sources had to be read at all.
struct TileReads {
    bytes: AtomicU64,
    files: Vec<AtomicBool>,
}

impl TileReads {
    fn new(sources: usize) -> Arc<Self> {
        Arc::new(TileReads { bytes: AtomicU64::new(0), files: (0..sources).map(|_| AtomicBool::new(false)).collect() })
    }
}

/// Bytes a band issues reads for when it is planned (ahead of its decode),
/// per tile — small next to a tile's band budget, so the memory estimate
/// that sizes the batch pool still holds.
const READ_AHEAD_BYTES: u64 = 32 << 20;

/// Submit *runs* of source *idx* to the I/O pool; `urgent` for reads a
/// thread is about to wait for, else they are read behind them.
fn submit_runs(
    src: &Arc<FileSource>,
    idx: usize,
    cache: &Arc<ChunkCache>,
    runs: Vec<Run>,
    urgent: bool,
    reads: &Arc<TileReads>,
) -> Vec<RunRead> {
    if !runs.is_empty() {
        reads.files[idx].store(true, Ordering::Relaxed);
    }
    runs.into_iter()
        .map(|run| {
            let (src, cache, reads) = (src.clone(), cache.clone(), reads.clone());
            let (start, end) = (run.start, run.end);
            let bytes = io_pool().submit(urgent, move || {
                let file = cache.file(&src)?;
                let buf = read_run(&file, &src.path, start, end)?;
                reads.bytes.fetch_add(buf.len() as u64, Ordering::Relaxed);
                Ok(buf)
            });
            RunRead { start, items: run.items, bytes }
        })
        .collect()
}

/// Issue the reads of the chunks a window will need that are neither cached
/// nor held, for [`load_window`] to decode. The band's reads stop at
/// *budget* bytes (urgent or ahead): what they leave is read when the window
/// loads, a window at a time per decode thread, so the raw bytes a tile
/// holds stay bounded. `None` when nothing needs reading (or the layout is
/// the `tiff` decoder's).
fn read_window_ahead(
    src: &Arc<FileSource>,
    idx: usize,
    rect: (u32, u32, u32, u32),
    cache: &Arc<ChunkCache>,
    hold: &TileHold,
    urgent: bool,
    budget: &mut u64,
    reads: &Arc<TileReads>,
) -> Option<Vec<RunRead>> {
    if matches!(src.layout, FileLayout::Tiff { direct: None, .. }) {
        return None;
    }
    let wanted = window_chunks(src, rect);
    let keys: Vec<ChunkKey> = wanted.iter().map(|w| w.key).collect();
    let missing = cache.missing(&keys);
    let items: Vec<(u64, u64, Wanted)> = {
        let held = hold.lock().unwrap();
        wanted
            .iter()
            .zip(missing)
            .filter(|(w, m)| *m && !held.contains_key(&w.key))
            .filter_map(|(w, _)| chunk_extent(src, w).map(|(off, len)| (off, len, *w)))
            .collect()
    };
    if items.is_empty() {
        return None;
    }
    let mut runs = plan_runs(items);
    let mut keep = 0;
    for r in &runs {
        let len = r.end - r.start;
        if len > *budget {
            break;
        }
        *budget -= len;
        keep += 1;
    }
    runs.truncate(keep);
    if runs.is_empty() {
        return None;
    }
    Some(submit_runs(src, idx, cache, runs, urgent, reads))
}

/// Decode the window `[x0, x1) x [y0, y1)` of one file source (clamped to
/// the file by the caller), through the chunk cache.
///
/// The chunks the cache does not hold are read with one positioned read per
/// run of neighbouring chunks — on the I/O threads, through the cache's kept
/// file handle, no IFD parse; *ahead* are the reads the band issued for this
/// window in advance — and decoded here by [`DirectChunks`] (an `.abt` strip
/// is raw rows). A file the direct decoder does not cover, and any chunk it
/// cannot decode, goes through the `tiff` decoder instead (on this thread),
/// so the samples (and any error) are the `tiff` crate's.
#[allow(clippy::too_many_arguments)]
fn load_window(
    src: &Arc<FileSource>,
    idx: usize,
    (x0, x1, y0, y1): (u32, u32, u32, u32),
    cache: &Arc<ChunkCache>,
    hold: &TileHold,
    ahead: Option<Vec<RunRead>>,
    reads: &Arc<TileReads>,
    pool: &WinPool,
) -> Result<Window> {
    let g = &src.geom;
    debug_assert!(x0 < x1 && y0 < y1 && x1 <= g.width && y1 <= g.height);
    let ww = (x1 - x0) as usize;

    // The chunk grid, and per chunk the rectangle of image pixels it holds.
    let wanted = window_chunks(src, (x0, x1, y0, y1));

    // The chunks tile the image, so their overlaps with the window cover it
    // exactly once and every sample is written below before it is read: a
    // reused buffer needs no zero fill. Should the overlaps ever not add up
    // to the window, it is zero-filled as it always was.
    let n = ww * (y1 - y0) as usize;
    let overlap: usize = wanted
        .iter()
        .map(|w| {
            let cols = x1.min(w.cx0 + w.dw).saturating_sub(x0.max(w.cx0)) as usize;
            let rows = y1.min(w.cy0 + w.dh).saturating_sub(y0.max(w.cy0)) as usize;
            cols * rows
        })
        .sum();
    let mut out = if overlap == n { take_buf(pool, n) } else { vec![0i16; n] };

    let keys: Vec<ChunkKey> = wanted.iter().map(|w| w.key).collect();
    let mut got = cache.get_many(&keys);
    {
        let held = hold.lock().unwrap();
        for (slot, k) in got.iter_mut().zip(&keys) {
            if slot.is_none() {
                *slot = held.get(k).cloned();
            }
        }
    }

    // Copy one chunk's overlap with the window into it.
    let mut place = |w: &Wanted, data: &[i16]| {
        let (ix0, ix1) = (x0.max(w.cx0), x1.min(w.cx0 + w.dw));
        let (iy0, iy1) = (y0.max(w.cy0), y1.min(w.cy0 + w.dh));
        if ix0 >= ix1 {
            return;
        }
        let n = (ix1 - ix0) as usize;
        for y in iy0..iy1 {
            let s = (y - w.cy0) as usize * w.dw as usize + (ix0 - w.cx0) as usize;
            let d = (y - y0) as usize * ww + (ix0 - x0) as usize;
            out[d..d + n].copy_from_slice(&data[s..s + n]);
        }
    };

    let mut misses: Vec<Wanted> = Vec::new();
    for (slot, w) in got.into_iter().zip(&wanted) {
        match slot {
            Some(d) => place(w, &d),
            None => misses.push(*w),
        }
    }

    // Fresh chunks enter the cache in small batches as they are made, never
    // all at the end — holding a whole window's worth of decoded chunks
    // beside the window itself would double the band's footprint.
    let mut pending: Vec<(ChunkKey, Arc<Vec<i16>>)> = Vec::new();
    let mut keep = |key: ChunkKey, d: Vec<i16>| {
        let d = Arc::new(d);
        if cache.admits(d.len() * 2) {
            pending.push((key, d));
            if pending.len() >= 16 {
                cache.insert_many(std::mem::take(&mut pending));
            }
        } else {
            hold.lock().unwrap().insert(key, d);
        }
    };
    let bad_len = |len: usize, w: &Wanted| {
        anyhow::anyhow!(
            "source {:?}: decoded {} samples for a {}x{} chunk of a {}x{} \
             raster — the file is not a single-band elevation grid",
            src.path, len, w.dw, w.dh, g.width, g.height
        )
    };
    let open = || open_counted(&src.path).with_context(|| format!("Opening {:?}", src.path));

    // The reads behind the misses: those issued ahead, plus — urgent, now —
    // any miss they do not cover (a chunk evicted since, the read-ahead
    // budget, a deferred window). A read ahead whose chunks are all cached
    // by now is dropped unused.
    let runs = |misses: &[Wanted]| -> Vec<RunRead> {
        let mut runs = ahead.unwrap_or_default();
        let covered: std::collections::HashSet<ChunkKey> =
            runs.iter().flat_map(|r| r.items.iter().map(|it| it.2.key)).collect();
        let rest: Vec<(u64, u64, Wanted)> = misses
            .iter()
            .filter(|w| !covered.contains(&w.key))
            .filter_map(|w| chunk_extent(src, w).map(|(off, len)| (off, len, *w)))
            .collect();
        if !rest.is_empty() {
            runs.extend(submit_runs(src, idx, cache, plan_runs(rest), true, reads));
        }
        let need: std::collections::HashSet<ChunkKey> = misses.iter().map(|w| w.key).collect();
        runs.retain_mut(|r| {
            r.items.retain(|it| need.contains(&it.2.key));
            !r.items.is_empty()
        });
        runs
    };

    if !misses.is_empty() {
        match &src.layout {
            FileLayout::Abt { size, stride } => {
                let (size, stride) = (*size, *stride);
                let row_bytes = size * 2;
                for run in runs(&misses) {
                    let buf = run.bytes.take()?;
                    for (off, _, w) in &run.items {
                        let base = (off - run.start) as usize;
                        let mut data = vec![0i16; size * w.dh as usize];
                        for (r, row) in data.chunks_exact_mut(size).enumerate() {
                            let s = base + r * stride;
                            let Some(raw) = buf.get(s..s + row_bytes) else {
                                anyhow::bail!(
                                    "source {:?}: truncated .abt payload (row {} of {size} is \
                                     incomplete); the file cannot be used as a terrain source",
                                    src.path,
                                    w.cy0 as usize + r
                                );
                            };
                            // RAW half-metres — .abt already stores the target unit.
                            for (v, px) in row.iter_mut().zip(raw.chunks_exact(2)) {
                                *v = i16::from_le_bytes([px[0], px[1]]);
                            }
                        }
                        place(w, &data);
                        keep(w.key, data);
                    }
                    recycle_read_buf(buf);
                }
            }
            FileLayout::Tiff { nodata, value_xform, direct: Some(direct), .. } => {
                let mut lzw = None;
                // The `tiff` decoder, opened only if a chunk needs it.
                let mut fallback: Option<(Decoder<BlockReader>, Option<(usize, usize)>)> = None;
                for run in runs(&misses) {
                    let buf = run.bytes.take()?;
                    for (off, len, w) in &run.items {
                        let lo = ((off - run.start) as usize).min(buf.len());
                        let hi = (lo as u64 + len).min(buf.len() as u64) as usize;
                        let decoded = direct.decode(&buf[lo..hi], w.dh as usize, &mut lzw);
                        let data = match decoded {
                            Some(full) => crop_chunk(
                                convert_samples(full, *nodata, *value_xform),
                                w.key.1,
                                direct.row_samples,
                                w.dw as usize,
                                w.dh as usize,
                            )?,
                            None => {
                                if fallback.is_none() {
                                    let mut dec = tiff_decoder(BlockReader::new(open()?))?;
                                    let row = chunk_row(&mut dec)?;
                                    fallback = Some((dec, row));
                                }
                                let (dec, row) = fallback.as_mut().unwrap();
                                decode_chunk(dec, w.key.1, *row, *nodata, *value_xform)?
                            }
                        };
                        if data.len() != w.dw as usize * w.dh as usize {
                            return Err(bad_len(data.len(), w));
                        }
                        place(w, &data);
                        keep(w.key, data);
                    }
                    recycle_read_buf(buf);
                }
            }
            FileLayout::Tiff { nodata, value_xform, direct: None, .. } => {
                let mut dec = tiff_decoder(BlockReader::new(open()?))?;
                let row = chunk_row(&mut dec)?;
                for w in &misses {
                    let data = decode_chunk(&mut dec, w.key.1, row, *nodata, *value_xform)?;
                    if data.len() != w.dw as usize * w.dh as usize {
                        return Err(bad_len(data.len(), w));
                    }
                    place(w, &data);
                    keep(w.key, data);
                }
            }
        }
    }
    cache.insert_many(pending);

    Ok(Window { buf: WinBuf::Own(out), x0, y0, x1, y1, stride: ww })
}



/// Parse an `.abt` header (contract §6) from just past the magic.
/// Returns the geometry, the tile size and the row stride in bytes.
fn read_abt_header<R: std::io::Read>(path: &Path, reader: &mut R) -> Result<(SourceGeom, usize, usize)> {
    use byteorder::ReadBytesExt;

    let corrupt = |what: &str| {
        anyhow::anyhow!(
            "source {:?}: truncated or corrupt .abt header ({what}); the file \
             cannot be used as a terrain source",
            path
        )
    };

    let mut rest = [0u8; 40]; // header minus the 4 magic bytes already read
    reader.read_exact(&mut rest).map_err(|_| corrupt("shorter than 44 bytes"))?;
    let mut hdr = &rest[..];
    let version = hdr.read_u16::<LittleEndian>()?;
    let size = hdr.read_u16::<LittleEndian>()? as usize;
    let ul_lat = hdr.read_f64::<LittleEndian>()?;
    let ul_lon = hdr.read_f64::<LittleEndian>()?;
    let _scale_y = hdr.read_f64::<LittleEndian>()?;
    let scale_x = hdr.read_f64::<LittleEndian>()?;
    let _base_elev = hdr.read_i16::<LittleEndian>()?;
    let stride = hdr.read_u16::<LittleEndian>()? as usize;

    // Decision: only R16SINT (version 1) tiles can be sources — a version-2
    // tile's payload is BC6H blocks, not i16 rows, and decoding it as rows
    // would produce silent garbage terrain.
    if version != 1 {
        anyhow::bail!(
            "source {:?}: .abt version {version} is not ingestable — only \
             R16SINT (version 1) tiles can be terrain sources",
            path
        );
    }
    if size == 0 {
        return Err(corrupt("size is 0"));
    }
    let row_bytes = size * 2;
    if stride < row_bytes {
        return Err(corrupt(&format!("row stride {stride} < row bytes {row_bytes}")));
    }
    // Legacy scale quirk, mirrored from the plugin's reader
    // (waveshed/core/abt.py): older converter builds wrote the tile's WHOLE
    // SPAN in scale_x rather than the per-pixel step. A real per-pixel step is
    // tiny (10 m is 9e-5 degrees), so anything at or above 0.005 degrees is a
    // span and is divided by the tile size. scale_y is ignored, as there.
    let pixel_res = if scale_x < 0.005 { scale_x } else { scale_x / size as f64 };
    if !(pixel_res > 0.0) || !pixel_res.is_finite() {
        return Err(corrupt(&format!("non-positive pixel scale {scale_x}")));
    }

    let limit_n = ul_lat - (size as f64 * pixel_res);
    let limit_e = ul_lon + (size as f64 * pixel_res);
    Ok((
        SourceGeom {
            width: size as u32,
            height: size as u32,
            origin_e: ul_lon,
            origin_n: ul_lat,
            limit_e,
            limit_n,
            scale_x: pixel_res,
            scale_y: pixel_res,
            embedded_crs: None,
            is_abt: true,
        },
        size,
        stride,
    ))
}

/// Read an `.abt` payload (cursor just past the header): RAW half-metres,
/// row-stride padding stripped.
fn read_abt_payload(path: &Path, reader: &mut BufReader<File>, size: usize, stride: usize) -> Result<Vec<i16>> {
    use std::io::Read;
    let row_bytes = size * 2;
    let mut data = vec![0i16; size * size];
    let mut row_buf = vec![0u8; row_bytes];
    let pad = (stride - row_bytes) as i64;
    for row in 0..size {
        reader.read_exact(&mut row_buf).map_err(|_| {
            anyhow::anyhow!(
                "source {:?}: truncated .abt payload (row {row} of {size} is \
                 incomplete); the file cannot be used as a terrain source",
                path
            )
        })?;
        for (i, px) in row_buf.chunks_exact(2).enumerate() {
            // RAW half-metres — .abt already stores the target unit.
            data[row * size + i] = i16::from_le_bytes([px[0], px[1]]);
        }
        if pad > 0 {
            reader.seek_relative(pad).map_err(|_| {
                anyhow::anyhow!(
                    "source {:?}: truncated or corrupt .abt header (padding seek failed); \
                     the file cannot be used as a terrain source",
                    path
                )
            })?;
        }
    }
    Ok(data)
}

/// Load an `.abt` tile as an ingest source.
///
/// Self-describing: the 44-byte header (contract §6) carries the geometry —
/// geographic degrees, square tile, same step both axes — and the payload is
/// already i16 half-metres, so pixels are loaded RAW (no metres→half-metres
/// conversion) with the row-stride padding stripped. `-9999` voids pass
/// through unchanged and read as no-data in the samplers. Sampling and
/// area-averaging then behave exactly as for a geographic GeoTIFF.
///
/// *file* has its cursor just past the magic the caller sniffed.
fn load_abt_to_ram(path: &Path, file: File) -> Result<Arc<LoadedImage>> {
    let mut reader = BufReader::with_capacity(1024 * 1024, file);
    let (g, size, stride) = read_abt_header(path, &mut reader)?;
    let data = read_abt_payload(path, &mut reader, size, stride)?;
    Ok(Arc::new(LoadedImage {
        width: g.width,
        height: g.height,
        data,
        origin_e: g.origin_e,
        origin_n: g.origin_n,
        limit_e: g.limit_e,
        limit_n: g.limit_n,
        scale_x: g.scale_x,
        scale_y: g.scale_y,
        name: path.file_name().unwrap_or_default().to_string_lossy().to_string(),
        embedded_crs: None,
        is_abt: true,
    }))
}

/// One linear segment of a row's WGS84 -> source-CRS geometry: over the pixels
/// it covers, `coord = c0 + step * local_x`.
struct Seg {
    e0: f64,
    n0: f64,
    nlo0: f64,
    step_e: f64,
    step_n: f64,
    step_nlo: f64,
}

/// Piecewise-linear source-CRS coordinates of one output row.
///
/// Either a single segment spanning the whole row (when the row-midpoint
/// check passes) or 64-px segments with per-chunk endpoint transforms —
/// O(size/64) transforms per row either way, never per-pixel. In practice a
/// conformal projection's midpoint deviation (~1.2 m over a 0.1° row for
/// EPSG:2056, growing quadratically with row width) exceeds the sub-pixel
/// tolerance, so projected sources take the chunked path — strictly *more*
/// accurate than the retired polynomial's whole-row lerp. Geographic sources
/// never reach this machinery at all. Pixel `x` maps to segment `x >> shift`
/// and local offset `x & mask`, so the hot loop stays branch- and
/// division-free.
struct RowGeom {
    segs: Vec<Seg>,
    shift: u32,
    mask: usize,
    /// Top-edge coordinate bounds, for the per-row source-overlap filter.
    min_e: f64,
    max_e: f64,
    min_n: f64,
    max_n: f64,
}

const CHUNK_SHIFT: u32 = 6; // 64-px row chunks on the chunked path
/// A row's midpoint may deviate from the endpoint lerp by at most this many
/// source pixels before the row is chunked. Measured, real projections
/// (EPSG:2056, UTM) exceed this at practical tile widths and take the chunked
/// path; the single-segment path serves quasi-linear cases (tiny tiles, very
/// coarse sources).
const ROW_LERP_TOLERANCE_PX: f64 = 0.25;

/// WGS84 (degrees) -> `dst` (east, north). `None` when the point is outside
/// the projection's domain.
#[inline]
fn fwd(wgs84: &Proj, dst: &Proj, lat: f64, lon: f64) -> Option<(f64, f64)> {
    let mut pt = (lon.to_radians(), lat.to_radians(), 0.0);
    transform(wgs84, dst, &mut pt).ok()?;
    Some((pt.0, pt.1))
}

/// Build one output row's [`RowGeom`] for one projected CRS.
///
/// `lat_hi` is the row's sample latitude (the cell's north edge lerp uses it,
/// exactly as the old code did) and `lat_lo` is one output row further south.
/// Returns `None` when any needed point fails to transform: the row is then
/// outside the projection's domain, where no source in this CRS can have
/// pixels, so the row simply has no candidates from this group. (Decision:
/// this is a coverage question, not an error — the job's other sources and
/// `void_fill_m` decide what such pixels become.)
///
/// The sampler builds rows with [`make_row_geom_shared`], which gives the
/// same geometry with fewer transforms; this per-row form is its reference.
#[cfg_attr(not(test), allow(dead_code))]
fn make_row_geom(
    wgs84: &Proj,
    dst: &Proj,
    lat_hi: f64,
    lat_lo: f64,
    ul_lon: f64,
    pixel_deg: f64,
    out_size: usize,
    min_scale: f64,
) -> Option<RowGeom> {
    let lon_end = ul_lon + out_size as f64 * pixel_deg;
    let (e_start, n_start) = fwd(wgs84, dst, lat_hi, ul_lon)?;
    let (e_end, n_end) = fwd(wgs84, dst, lat_hi, lon_end)?;
    let (_, n_lo_start) = fwd(wgs84, dst, lat_lo, ul_lon)?;
    let (_, n_lo_end) = fwd(wgs84, dst, lat_lo, lon_end)?;

    let inv_size = 1.0 / out_size as f64;
    let step_e = (e_end - e_start) * inv_size;
    let step_n = (n_end - n_start) * inv_size;
    let step_nlo = (n_lo_end - n_lo_start) * inv_size;

    // Row-midpoint linearity check: if the true midpoint sits within a
    // fraction of a source pixel of the endpoint lerp, one segment serves the
    // whole row.
    let mid_lon = ul_lon + (out_size as f64 * 0.5) * pixel_deg;
    let (e_mid, n_mid) = fwd(wgs84, dst, lat_hi, mid_lon)?;
    let lerp_e = (e_start + e_end) * 0.5;
    let lerp_n = (n_start + n_end) * 0.5;
    let dev = ((e_mid - lerp_e).powi(2) + (n_mid - lerp_n).powi(2)).sqrt();

    if dev < ROW_LERP_TOLERANCE_PX * min_scale {
        return Some(RowGeom {
            segs: vec![Seg { e0: e_start, n0: n_start, nlo0: n_lo_start, step_e, step_n, step_nlo }],
            // One segment for every x: x >> (BITS-1) == 0 for any valid index,
            // and x & usize::MAX == x. (A hardcoded 63 would be a >=-width
            // shift on a 32-bit usize — wasm — and panic in debug builds.)
            shift: usize::BITS - 1,
            mask: usize::MAX,
            min_e: e_start.min(e_end),
            max_e: e_start.max(e_end),
            min_n: n_start.min(n_end),
            max_n: n_start.max(n_end),
        });
    }

    // Slow path: 64-px chunks, endpoints transformed per chunk boundary.
    let chunk = 1usize << CHUNK_SHIFT;
    let n_segs = out_size.div_ceil(chunk);
    let mut boundaries = Vec::with_capacity(n_segs + 1);
    for i in 0..=n_segs {
        let x = (i * chunk).min(out_size);
        let lon = ul_lon + x as f64 * pixel_deg;
        let (e, n) = fwd(wgs84, dst, lat_hi, lon)?;
        let (_, n_lo) = fwd(wgs84, dst, lat_lo, lon)?;
        boundaries.push((x, e, n, n_lo));
    }
    let (mut min_e, mut max_e) = (f64::INFINITY, f64::NEG_INFINITY);
    let (mut min_n, mut max_n) = (f64::INFINITY, f64::NEG_INFINITY);
    for &(_, e, n, _) in &boundaries {
        min_e = min_e.min(e);
        max_e = max_e.max(e);
        min_n = min_n.min(n);
        max_n = max_n.max(n);
    }
    let segs = boundaries
        .windows(2)
        .map(|w| {
            let (x0, e0, n0, nlo0) = w[0];
            let (x1, e1, n1, nlo1) = w[1];
            let inv = 1.0 / (x1 - x0) as f64;
            Seg {
                e0,
                n0,
                nlo0,
                step_e: (e1 - e0) * inv,
                step_n: (n1 - n0) * inv,
                step_nlo: (nlo1 - nlo0) * inv,
            }
        })
        .collect();
    Some(RowGeom { segs, shift: CHUNK_SHIFT, mask: chunk - 1, min_e, max_e, min_n, max_n })
}

/// The transforms a row made at one latitude — its start and end longitude
/// and, for a chunked row, every segment boundary — kept for the next row,
/// whose north edge is (bit for bit, more often than not) this row's south
/// edge.
struct LatPts {
    lat_bits: u64,
    start: (f64, f64),
    end: (f64, f64),
    bounds: Option<Vec<(f64, f64)>>,
}

/// [`make_row_geom`], reusing the previous row's south-edge transforms
/// (*prev*) when this row's `lat_hi` is the same `f64`, and the
/// linearity check's end points as the segment boundaries at the same
/// longitudes. Every transform it skips is one it would have repeated with
/// bit-identical inputs, so the geometry is identical (pinned by
/// `shared_row_geometry_is_identical_to_the_per_row_one`).
#[allow(clippy::too_many_arguments)]
fn make_row_geom_shared(
    wgs84: &Proj,
    dst: &Proj,
    lat_hi: f64,
    lat_lo: f64,
    ul_lon: f64,
    pixel_deg: f64,
    out_size: usize,
    min_scale: f64,
    prev: &mut Option<LatPts>,
) -> Option<RowGeom> {
    let reuse = prev.take().filter(|p| p.lat_bits == lat_hi.to_bits());
    let lon_end = ul_lon + out_size as f64 * pixel_deg;
    let ((e_start, n_start), (e_end, n_end), hi_bounds) = match reuse {
        Some(p) => (p.start, p.end, p.bounds),
        None => (fwd(wgs84, dst, lat_hi, ul_lon)?, fwd(wgs84, dst, lat_hi, lon_end)?, None),
    };
    let lo_start = fwd(wgs84, dst, lat_lo, ul_lon)?;
    let lo_end = fwd(wgs84, dst, lat_lo, lon_end)?;
    let (n_lo_start, n_lo_end) = (lo_start.1, lo_end.1);

    let inv_size = 1.0 / out_size as f64;
    let step_e = (e_end - e_start) * inv_size;
    let step_n = (n_end - n_start) * inv_size;
    let step_nlo = (n_lo_end - n_lo_start) * inv_size;

    // Row-midpoint linearity check (see make_row_geom).
    let mid_lon = ul_lon + (out_size as f64 * 0.5) * pixel_deg;
    let (e_mid, n_mid) = fwd(wgs84, dst, lat_hi, mid_lon)?;
    let lerp_e = (e_start + e_end) * 0.5;
    let lerp_n = (n_start + n_end) * 0.5;
    let dev = ((e_mid - lerp_e).powi(2) + (n_mid - lerp_n).powi(2)).sqrt();

    if dev < ROW_LERP_TOLERANCE_PX * min_scale {
        *prev = Some(LatPts { lat_bits: lat_lo.to_bits(), start: lo_start, end: lo_end, bounds: None });
        return Some(RowGeom {
            segs: vec![Seg { e0: e_start, n0: n_start, nlo0: n_lo_start, step_e, step_n, step_nlo }],
            shift: usize::BITS - 1,
            mask: usize::MAX,
            min_e: e_start.min(e_end),
            max_e: e_start.max(e_end),
            min_n: n_start.min(n_end),
            max_n: n_start.max(n_end),
        });
    }

    // Slow path: 64-px chunks, endpoints transformed per chunk boundary.
    let chunk = 1usize << CHUNK_SHIFT;
    let n_segs = out_size.div_ceil(chunk);
    let x_at = |i: usize| (i * chunk).min(out_size);
    // One latitude's boundary transforms; the row's end points are known.
    let bounds_of = |lat: f64, start: (f64, f64), end: (f64, f64)| -> Option<Vec<(f64, f64)>> {
        (0..=n_segs)
            .map(|i| {
                let lon = ul_lon + x_at(i) as f64 * pixel_deg;
                if i == 0 && lon.to_bits() == ul_lon.to_bits() {
                    Some(start)
                } else if i == n_segs && lon.to_bits() == lon_end.to_bits() {
                    Some(end)
                } else {
                    fwd(wgs84, dst, lat, lon)
                }
            })
            .collect()
    };
    let hi = match hi_bounds {
        Some(b) => b,
        None => bounds_of(lat_hi, (e_start, n_start), (e_end, n_end))?,
    };
    let lo = bounds_of(lat_lo, lo_start, lo_end)?;
    let boundaries: Vec<(usize, f64, f64, f64)> =
        (0..=n_segs).map(|i| (x_at(i), hi[i].0, hi[i].1, lo[i].1)).collect();
    *prev = Some(LatPts { lat_bits: lat_lo.to_bits(), start: lo_start, end: lo_end, bounds: Some(lo) });

    let (mut min_e, mut max_e) = (f64::INFINITY, f64::NEG_INFINITY);
    let (mut min_n, mut max_n) = (f64::INFINITY, f64::NEG_INFINITY);
    for &(_, e, n, _) in &boundaries {
        min_e = min_e.min(e);
        max_e = max_e.max(e);
        min_n = min_n.min(n);
        max_n = max_n.max(n);
    }
    let segs = boundaries
        .windows(2)
        .map(|w| {
            let (x0, e0, n0, nlo0) = w[0];
            let (x1, e1, n1, nlo1) = w[1];
            let inv = 1.0 / (x1 - x0) as f64;
            Seg {
                e0,
                n0,
                nlo0,
                step_e: (e1 - e0) * inv,
                step_n: (n1 - n0) * inv,
                step_nlo: (nlo1 - nlo0) * inv,
            }
        })
        .collect();
    Some(RowGeom { segs, shift: CHUNK_SHIFT, mask: chunk - 1, min_e, max_e, min_n, max_n })
}

/// Rows a geometry run computes in order, sharing transforms between
/// neighbours ([`make_row_geom_shared`]); runs go in parallel.
const ROW_RUN: usize = 16;

/// One CRS group's geometry for rows `r0..r1`: `row_lat_of(y)` is row
/// `y`'s cell-centre parallel, its south edge one pixel further south.
#[allow(clippy::too_many_arguments)]
fn band_row_geoms(
    wgs84: &Proj,
    dst: &Proj,
    min_scale: f64,
    (r0, r1): (usize, usize),
    row_lat_of: &(impl Fn(usize) -> f64 + Sync),
    ul_lon: f64,
    pixel_deg: f64,
    out_size: usize,
) -> Vec<Option<RowGeom>> {
    let run = |a: usize| -> Vec<Option<RowGeom>> {
        let mut prev = None;
        (a..(a + ROW_RUN).min(r1))
            .map(|y| {
                let row_lat = row_lat_of(y);
                make_row_geom_shared(
                    wgs84, dst, row_lat, row_lat - pixel_deg, ul_lon, pixel_deg, out_size, min_scale, &mut prev,
                )
            })
            .collect()
    };
    let starts: Vec<usize> = (r0..r1).step_by(ROW_RUN).collect();
    #[cfg(feature = "native")]
    let runs: Vec<Vec<Option<RowGeom>>> = starts.into_par_iter().map(run).collect();
    #[cfg(not(feature = "native"))]
    let runs: Vec<Vec<Option<RowGeom>>> = starts.into_iter().map(run).collect();
    runs.into_iter().flatten().collect()
}

/// Convert one tile, decoding this job's `buildings_pbf_dir` (if any) for it.
///
/// Convenient for a one-tile run. A batch must not use this: the PBF decode is
/// a whole-directory scan whose result is the same for every output tile, so
/// doing it here runs it once per `.abt`. Batch callers load the set once with
/// [`load_pbf_building_dir`] and call [`process_tile`].
pub fn process_tile_with_cache(job: IngestJob, cache_arc: ImageCache) -> Result<TileStats> {
    let pbf_buildings = match &job.buildings_pbf_dir {
        Some(dir) => Some(load_pbf_building_dir(dir)?),
        None => None,
    };
    process_tile(job, cache_arc, pbf_buildings.as_ref())
}

/// Convert one tile, drawing *pbf_buildings* (decoded once per run) onto it,
/// with the default memory knobs ([`IngestOptions::default`]).
///
/// *pbf_buildings* must be `Some` whenever the job carries a
/// `buildings_pbf_dir`; passing `None` for such a job is a caller bug and is
/// refused rather than quietly producing a building-less tile.
pub fn process_tile(
    job: IngestJob,
    cache_arc: ImageCache,
    pbf_buildings: Option<&PbfBuildingSet>,
) -> Result<TileStats> {
    process_tile_opts(job, cache_arc, pbf_buildings, &IngestOptions::default())
}

/// One source of a job, resolved: a caller-supplied whole image, or a file
/// streamed window by window.
enum Src {
    Mem(Arc<LoadedImage>),
    File(Arc<FileSource>),
}

/// Bounds, over one output row, of every cell footprint edge in one projected
/// CRS (`e_lo`/`e_hi`/`n_lo`/`n_hi` of the sampling loop).
#[derive(Clone, Copy)]
struct RowExt {
    e_min: f64,
    e_max: f64,
    n_min: f64,
    n_max: f64,
}

impl RowExt {
    const EMPTY: RowExt = RowExt {
        e_min: f64::INFINITY,
        e_max: f64::NEG_INFINITY,
        n_min: f64::INFINITY,
        n_max: f64::NEG_INFINITY,
    };

    fn union(&mut self, o: &RowExt) {
        self.e_min = self.e_min.min(o.e_min);
        self.e_max = self.e_max.max(o.e_max);
        self.n_min = self.n_min.min(o.n_min);
        self.n_max = self.n_max.max(o.n_max);
    }

    fn is_empty(&self) -> bool {
        !(self.e_min <= self.e_max && self.n_min <= self.n_max)
    }
}

/// [`RowExt`] of one row. Every footprint coordinate the sampling loop forms
/// is linear in the pixel's offset within its segment, so its extremes over
/// the segment are at the segment's first and last pixel — evaluated here
/// with the loop's own expressions. (Float rounding can wiggle a lerp by an
/// ulp; the caller's 2-pixel margin swamps that.)
fn row_extent(geom: &RowGeom, out_size: usize) -> RowExt {
    let mut ext = RowExt::EMPTY;
    let seg_len = if geom.segs.len() == 1 { out_size } else { 1usize << geom.shift };
    for (si, seg) in geom.segs.iter().enumerate() {
        let first = si * seg_len;
        if first >= out_size {
            break;
        }
        let last = (first + seg_len).min(out_size) - 1;
        for x in [first, last] {
            let lx = (x & geom.mask) as f64;
            let e = seg.e0 + seg.step_e * lx;
            let n = seg.n0 + seg.step_n * lx;
            let e_lo = e - seg.step_e * 0.5;
            let e_hi = e + seg.step_e * 0.5;
            let half_n = (n - (seg.nlo0 + seg.step_nlo * lx)) * 0.5;
            let (n_hi, n_lo) = (n + half_n, n - half_n);
            ext.e_min = ext.e_min.min(e_lo).min(e_hi);
            ext.e_max = ext.e_max.max(e_lo).max(e_hi);
            ext.n_min = ext.n_min.min(n_lo).min(n_hi);
            ext.n_max = ext.n_max.max(n_lo).max(n_hi);
        }
    }
    ext
}

/// The row band a geographic source contributes to output row `row_lat`
/// (`None`: the row misses the source). Shared by the window planner and the
/// sampling loop, so both see the same rows.
#[inline]
fn geo_row_span(g: &SourceGeom, row_lat: f64, pixel_deg: f64) -> Option<(u32, u32)> {
    if row_lat <= g.origin_n && row_lat >= g.limit_n {
        let py_f = (g.origin_n - row_lat) / g.scale_y;
        if py_f >= 0.0 {
            let py = py_f as u32;
            if py < g.height {
                // The cell's own row band, hoisted: it is the same for every
                // pixel of this output row.
                let half = 0.5 * pixel_deg / g.scale_y;
                return Some(sample_span(py_f - half, py_f + half, g.height));
            }
        }
    }
    None
}

/// Whether a projected source can hold any cell of a row (the per-row
/// overlap filter, shared by planner and sampler).
#[inline]
fn prj_row_candidate(g: &SourceGeom, geom: &RowGeom) -> bool {
    g.origin_n >= geom.min_n && g.limit_n <= geom.max_n && g.limit_e >= geom.min_e && g.origin_e <= geom.max_e
}

/// `(min_e, max_e, min_n, max_n)` of the cell centres of pixels
/// `xa..=xb` of a row — both in one geometry segment — evaluated with the
/// sampling loop's own expressions at the two ends (a centre coordinate is
/// monotone in the pixel within a segment, so these are its extremes).
#[inline]
fn centre_bounds(geom: &RowGeom, xa: usize, xb: usize) -> (f64, f64, f64, f64) {
    debug_assert_eq!(xa >> geom.shift, xb >> geom.shift, "a block spans one segment");
    let at = |x: usize| {
        let seg = &geom.segs[x >> geom.shift];
        let lx = (x & geom.mask) as f64;
        (seg.e0 + seg.step_e * lx, seg.n0 + seg.step_n * lx)
    };
    let ((ea, na), (eb, nb)) = (at(xa), at(xb));
    (ea.min(eb), ea.max(eb), na.min(nb), na.max(nb))
}

/// Whether a projected source can pass the sampler's containment test
/// (`n <= origin_n && n >= limit_n && e >= origin_e && e < limit_e`) at any
/// cell centre within *c* ([`centre_bounds`]). `false` only when it fails
/// at all of them; a non-finite bound keeps the source.
#[inline]
fn may_hold(img: &SourceGeom, (min_e, max_e, min_n, max_n): (f64, f64, f64, f64)) -> bool {
    if !(min_e.is_finite() && max_e.is_finite() && min_n.is_finite() && max_n.is_finite()) {
        return true;
    }
    !(max_n < img.limit_n || min_n > img.origin_n || max_e < img.origin_e || min_e >= img.limit_e)
}

/// Lower window edge (pixels) for a footprint bound: 2 pixels of margin below
/// what `sample_span` can return, clamped to the file.
fn px_lo(v: f64, dim: u32) -> u32 {
    let v = v.floor() - 2.0;
    if !(v > 0.0) {
        0
    } else if v >= dim as f64 {
        dim
    } else {
        v as u32
    }
}

/// Upper (exclusive) window edge, 2 pixels of margin above, clamped.
fn px_hi(v: f64, dim: u32) -> u32 {
    let v = v.ceil() + 2.0;
    if !(v > 0.0) {
        0
    } else if v >= dim as f64 {
        dim
    } else {
        v as u32
    }
}

/// Which candidates of one band are shadowed: a candidate (a file source with
/// a window in this band) whose WGS84 box clipped to *band* (`[lon_min,
/// lat_min, lon_max, lat_max]`) lies inside the box of a candidate before it
/// in priority order — or whose box misses the band altogether. Sources
/// without a box are never shadowed and shadow nothing.
fn shadowed_sources(
    rects: &[Option<(u32, u32, u32, u32)>],
    bboxes: &[Option<[f64; 4]>],
    band: [f64; 4],
) -> Vec<bool> {
    let mut shadowed = vec![false; rects.len()];
    let mut above: Vec<[f64; 4]> = Vec::new();
    for (i, (rect, bb)) in rects.iter().zip(bboxes).enumerate() {
        let (Some(_), Some(bb)) = (rect, bb) else { continue };
        let clip = [bb[0].max(band[0]), bb[1].max(band[1]), bb[2].min(band[2]), bb[3].min(band[3])];
        shadowed[i] = !(clip[0] < clip[2] && clip[1] < clip[3])
            || above.iter().any(|a| a[0] <= clip[0] && a[1] <= clip[1] && a[2] >= clip[2] && a[3] >= clip[3]);
        above.push(*bb);
    }
    shadowed
}

/// Convert one tile with explicit memory knobs.
///
/// **Streaming.** The output tile is produced in row bands. For each band
/// only the part of each source the band's cell footprints can reach is
/// decoded — computed from the band's own transformed row geometry, with a
/// 2-pixel margin, clamped to the file — through the TIFF's own strips/tiles
/// and the byte-budgeted [`ChunkCache`]. The sampling arithmetic is the
/// whole-image arithmetic unchanged (spans are computed in whole-file pixel
/// indices and clamped to the file exactly as before), so the bytes do not
/// depend on the band size or the budgets; a read outside a planned window
/// refuses the tile rather than guessing.
///
/// The tile file is written on the calling thread before this returns (a
/// batch hands it to a [`TileWriter`] instead, see [`process_tile_batched`]).
pub fn process_tile_opts(
    job: IngestJob,
    cache_arc: ImageCache,
    pbf_buildings: Option<&PbfBuildingSet>,
    opts: &IngestOptions,
) -> Result<TileStats> {
    let tile = sample_tile(job, cache_arc, pbf_buildings, opts, None)?;
    let w = Clock::now();
    let written = write_tile(&tile);
    tile.line.print(w.ns(), tile.t0.ns());
    written.map(|()| tile.stats)
}

/// Called once with a batch tile's final result: the write's outcome when
/// the tile was sampled, else the sampling error.
pub type TileDone = Box<dyn FnOnce(Result<TileStats>) + Send>;

/// [`process_tile_opts`] for a batch: the tile is sampled on the calling
/// thread and its bytes are handed to *writer*, so the thread is free for
/// the next tile while the file is written, synced and renamed. *done* runs
/// exactly once — on the calling thread when sampling fails, else on the
/// writer thread once the tile is committed or its write failed (a failed
/// write has removed its temp file, exactly as in [`process_tile_opts`]).
pub fn process_tile_batched(
    job: IngestJob,
    cache_arc: ImageCache,
    pbf_buildings: Option<&PbfBuildingSet>,
    opts: &IngestOptions,
    writer: &TileWriter,
    done: TileDone,
) {
    let spare = writer.spare_buffer();
    let tile = match sample_tile(job, cache_arc, pbf_buildings, opts, spare) {
        Ok(t) => t,
        Err(e) => return done(Err(e)),
    };
    let stats = tile.stats;
    // The tile's time on this thread ends here: the write overlaps the
    // next tile (a wait for a free writer is not counted).
    let total_ns = tile.t0.ns();
    writer.submit(WriteJob {
        tile,
        then: Box::new(move |r: Result<()>, line: TileLine, write_ns: u64| {
            line.print(write_ns, total_ns);
            done(r.map(|()| stats))
        }),
    });
}

/// The payload of a finished tile.
enum TileBody {
    /// R16SINT rows of `size` samples, each followed by `pad` zero bytes
    /// (the 256-byte row alignment of §6).
    Rows { buf: Vec<i16>, size: usize, pad: usize },
    /// An already-encoded payload (BC6H blocks).
    #[allow(dead_code)]
    Raw(Vec<u8>),
}

/// The `[Rust] tile …` diagnostic line (changelog 31, extended by 37) of one
/// sampled tile. Not parsed by any consumer; goes to `AETHER_DIAG_FILE`
/// when that is set, else to stdout ([`crate::diag::out`]).
struct TileLine {
    name: String,
    sources: usize,
    header_s: f64,
    open_ms: f64,
    read_ms: f64,
    decode_s: f64,
    files: usize,
    mb: f64,
    sample_s: f64,
    bands: u32,
    geom_s: f64,
}

impl TileLine {
    /// Print with the write's wall time and the tile's wall time on its
    /// compute thread.
    fn print(&self, write_ns: u64, total_ns: u64) {
        crate::diag::out(&format!(
            "[Rust] tile {}: {} sources, header {:.1}s (open {:.1} ms, read {:.1} ms avg), \
             decode {:.1}s ({} files, {:.1} MB read), sample {:.1}s, bands {}, geom {:.1}s, \
             write {:.1}s, total {:.1}s",
            self.name,
            self.sources,
            self.header_s,
            self.open_ms,
            self.read_ms,
            self.decode_s,
            self.files,
            self.mb,
            self.sample_s,
            self.bands,
            self.geom_s,
            write_ns as f64 / 1e9,
            total_ns as f64 / 1e9,
        ));
    }
}

/// A sampled tile, ready to be written: its 44-byte header and payload.
struct ReadyTile {
    dst: PathBuf,
    header: Vec<u8>,
    body: TileBody,
    stats: TileStats,
    /// The tile's diagnostic line, printed once the tile is written.
    line: TileLine,
    /// When the tile started on its compute thread.
    t0: Clock,
    /// The ENOSPC test hook, read on the thread that sampled the tile.
    #[cfg(test)]
    fail_after: Option<usize>,
}

/// Write *tile* to `<dst>.tmp-<pid>` and rename it over `dst` ([`TileFile`]),
/// creating the output directory first. Any error removes the temp file and
/// leaves `dst` untouched.
fn write_tile(tile: &ReadyTile) -> Result<()> {
    if let Some(dir) = tile.dst.parent() {
        fs::create_dir_all(dir)
            .with_context(|| format!("creating output directory {:?}", dir))?;
    }
    // Written to `<output>.tmp-<pid>` beside the output and renamed over it
    // only once flushed and synced ([`TileFile`]): a failed write (a full
    // disk) leaves no truncated tile under the final name, and an existing
    // tile is replaced by a new inode instead of being truncated in place —
    // views hardlink pool tiles, and their inode stays intact.
    let mut w = TileFile::open_temp(
        &tile.dst,
        #[cfg(test)]
        tile.fail_after,
    )?;
    w.write_all(&tile.header)?;
    match &tile.body {
        TileBody::Rows { buf, size, pad } => {
            // Legacy R16SINT writing with 256-byte pitch padding
            let padding_buf = vec![0u8; *pad];
            for chunk in buf.chunks(*size) {
                let ptr = chunk.as_ptr() as *const u8;
                let len = chunk.len() * 2;
                let slice = unsafe { std::slice::from_raw_parts(ptr, len) };
                w.write_all(slice)?;
                if *pad > 0 {
                    w.write_all(&padding_buf)?;
                }
            }
        }
        TileBody::Raw(bytes) => w.write_all(bytes)?,
    }
    w.commit()
}

/// One tile for a [`TileWriter`]: the bytes, and what to do with the
/// write's outcome.
struct WriteJob {
    tile: ReadyTile,
    /// Runs with the write's result, the tile's line and the write's wall
    /// time.
    then: Box<dyn FnOnce(Result<()>, TileLine, u64) + Send>,
}

/// Write one job and report it. A panic while writing becomes the tile's
/// error; a panic in the report itself propagates (it kills this writer
/// thread and [`TileWriter::finish`] re-raises it, as a panic on a compute
/// thread takes the batch down).
fn run_write_job(job: WriteJob, spares: &Spares) {
    let WriteJob { tile, then } = job;
    let w = Clock::now();
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| write_tile(&tile)))
        .unwrap_or_else(|_| Err(anyhow::anyhow!("writing output tile {:?} panicked", tile.dst)));
    let write_ns = w.ns();
    if let TileBody::Rows { buf, .. } = tile.body {
        spares.give(buf);
    }
    then(r, tile.line, write_ns);
}

/// Output buffers of written tiles, for the next tiles to sample into: a
/// 2 m tile's buffer is 62 MB, and a fresh one costs ~15 000 page faults
/// (most of a tile's) and an allocator round trip to the OS.
struct Spares {
    bufs: std::sync::Mutex<Vec<Vec<i16>>>,
    keep: usize,
}

impl Spares {
    fn give(&self, buf: Vec<i16>) {
        let mut g = self.bufs.lock().unwrap_or_else(|p| p.into_inner());
        if g.len() < self.keep {
            g.push(buf);
        }
    }

    fn take(&self) -> Option<Vec<i16>> {
        self.bufs.lock().unwrap_or_else(|p| p.into_inner()).pop()
    }
}

/// Dedicated threads that write a batch's tiles (changelog 37): each file's
/// create, write, flush, `sync_data`, close and rename runs here, never on a
/// compute thread. A compute thread waits only when every writer is busy
/// and the queue (one slot per writer) is full — the back-pressure that
/// bounds the finished tiles held in memory. If no thread can be started,
/// or the writers are gone, a job is written inline on the submitting
/// thread instead: slower, never lost.
pub struct TileWriter {
    tx: Option<std::sync::mpsc::SyncSender<WriteJob>>,
    workers: Vec<std::thread::JoinHandle<()>>,
    spares: Arc<Spares>,
}

impl TileWriter {
    /// A writer with *threads* writer threads (at least one) that keeps up
    /// to *spare_buffers* written tiles' buffers for reuse (one per compute
    /// thread is enough).
    ///
    /// The hand-off is a rendezvous: a tile is held by its compute thread
    /// until a writer takes it, so a batch holds at most one finished tile
    /// per writer beyond the one each compute thread works on.
    pub fn new(threads: usize, spare_buffers: usize) -> TileWriter {
        let threads = threads.max(1);
        let spares = Arc::new(Spares { bufs: std::sync::Mutex::new(Vec::new()), keep: spare_buffers });
        let (tx, rx) = std::sync::mpsc::sync_channel::<WriteJob>(0);
        let rx = Arc::new(std::sync::Mutex::new(rx));
        let mut workers = Vec::with_capacity(threads);
        for i in 0..threads {
            let (rx, spares) = (rx.clone(), spares.clone());
            let spawned = std::thread::Builder::new().name(format!("aether-write-{i}")).spawn(move || loop {
                // The lock is held while waiting, so idle writers queue on it
                // and each job goes to exactly one of them.
                let job = rx.lock().unwrap_or_else(|p| p.into_inner()).recv();
                match job {
                    Ok(job) => run_write_job(job, &spares),
                    Err(_) => return, // every sender is gone: the batch is done
                }
            });
            match spawned {
                Ok(h) => workers.push(h),
                Err(_) => break,
            }
        }
        TileWriter { tx: (!workers.is_empty()).then_some(tx), workers, spares }
    }

    fn submit(&self, job: WriteJob) {
        match &self.tx {
            Some(tx) => {
                if let Err(std::sync::mpsc::SendError(job)) = tx.send(job) {
                    run_write_job(job, &self.spares);
                }
            }
            None => run_write_job(job, &self.spares),
        }
    }

    /// A written tile's output buffer to sample the next tile into.
    fn spare_buffer(&self) -> Option<Vec<i16>> {
        self.spares.take()
    }

    /// Wait until every submitted tile is written and reported. Re-raises a
    /// panic of a writer thread.
    pub fn finish(mut self) {
        self.shut_down();
    }

    fn shut_down(&mut self) {
        drop(self.tx.take());
        let mut panic = None;
        for h in self.workers.drain(..) {
            if let Err(p) = h.join() {
                panic.get_or_insert(p);
            }
        }
        if let Some(p) = panic {
            if !std::thread::panicking() {
                std::panic::resume_unwind(p);
            }
        }
    }
}

impl Drop for TileWriter {
    fn drop(&mut self) {
        self.shut_down();
    }
}

/// Everything of [`process_tile_opts`] but the file write.
fn sample_tile(
    job: IngestJob,
    cache_arc: ImageCache,
    pbf_buildings: Option<&PbfBuildingSet>,
    opts: &IngestOptions,
    spare: Option<Vec<i16>>,
) -> Result<ReadyTile> {
    // The tile's start on this thread (`total` of the `[Rust] tile` line).
    let t0 = Clock::now();
    // 0. Normalize the legacy aliases into sources[] — ONE code path from here.
    let sources = job.effective_sources()?;

    let chunk_cache: Arc<ChunkCache> = match &opts.chunk_cache {
        Some(c) => c.clone(),
        None => Arc::new(ChunkCache::new(DEFAULT_CHUNK_CACHE_BYTES)),
    };
    let chunk_cache = &chunk_cache;

    // 1.-2. Resolve every source's header (pixels come later, per band). A
    // listed source that cannot be opened or parsed is FATAL — even one this
    // tile never samples: writing the tile without it would silently drop
    // terrain and the run would still report success. The headers not yet
    // cached are read on the I/O threads, all at once (each is an open and a
    // positioned read or two); the error reported is still the first in
    // source order.
    // Where this tile's time goes, for the `[Rust] tile …` line (changelog 31).
    let diag = TileDiag::default();
    let hdr_clock = Clock::now();
    enum Header {
        Ready(Src),
        Reading(Arc<Ticket<Result<(Arc<FileSource>, IoDelta)>>>),
    }
    let pending: Vec<Header> = sources
        .iter()
        .map(|s| {
            let injected = cache_arc.lock().unwrap().get(&cache_key(s)).cloned();
            if let Some(img) = injected {
                return Header::Ready(Src::Mem(img));
            }
            if let Some(meta) = chunk_cache.meta_cached(s) {
                return Header::Ready(Src::File(meta));
            }
            let (cache, spec) = (chunk_cache.clone(), s.clone());
            Header::Reading(io_pool().submit(true, move || {
                // Measured on the I/O thread that does the read.
                let mark = IoMark::now();
                let meta = cache.meta(&spec)?;
                Ok((meta, mark.since()))
            }))
        })
        .collect();
    let mut srcs: Vec<Src> = Vec::with_capacity(sources.len());
    for (h, s) in pending.into_iter().zip(&sources) {
        srcs.push(match h {
            Header::Ready(src) => src,
            Header::Reading(t) => {
                let (meta, io) = t.take().with_context(|| format!("loading source {:?}", s.path))?;
                diag.header(io);
                Src::File(meta)
            }
        });
    }
    let hdr_ns = hdr_clock.ns();
    let geoms: Vec<SourceGeom> = srcs
        .iter()
        .map(|s| match s {
            Src::Mem(img) => img.geom(),
            Src::File(f) => f.geom.clone(),
        })
        .collect();

    // Per-source Proj objects are built once per job and cached by CRS string;
    // sources sharing a CRS share one projection group (and, later, one set of
    // per-row transforms).
    let wgs84 = Proj::from_proj_string("+proj=longlat +datum=WGS84 +no_defs")
        .expect("the WGS84 proj string is a constant and always parses");
    let mut proj_cache: HashMap<String, (usize, Arc<Proj>)> = HashMap::new();
    let mut groups: Vec<ProjGroup> = Vec::new();
    let mut kinds: Vec<SourceKind> = Vec::with_capacity(sources.len());
    // The proj string of each projected source (`None` for geographic ones).
    let mut proj_strings: Vec<Option<String>> = Vec::with_capacity(sources.len());
    for (spec, img) in sources.iter().zip(&geoms) {
        // An .abt source is self-describing: geographic degrees by format
        // definition. A crs field on it is refused rather than ignored — the
        // caller is asserting something the format cannot honor. (The nodata
        // twin of this error lives in open_source.)
        if img.is_abt {
            if spec.crs.is_some() {
                anyhow::bail!(
                    "source {:?}: .abt sources are self-describing — remove \
                     \"crs\" from this source (the header fixes the geometry \
                     to geographic WGS84 degrees)",
                    spec.path
                );
            }
            kinds.push(SourceKind::Geographic);
            proj_strings.push(None);
            continue;
        }
        let crs = match (&spec.crs, &img.embedded_crs) {
            (Some(c), _) => c.clone(),
            (None, Some(c)) => c.clone(),
            (None, None) => anyhow::bail!(
                "source {:?}: no CRS — the job does not set one and the file's \
                 GeoKeyDirectory carries no usable EPSG code (absent or \
                 user-defined/32767). Add \"crs\": \"EPSG:nnnn\" (or a proj \
                 string starting with '+') to this source.",
                spec.path
            ),
        };
        let proj_string = crs_to_proj_string(&crs, &spec.path)?;
        let kind = match proj_cache.get(&proj_string) {
            Some((group_idx, _)) => SourceKind::Projected(*group_idx),
            None => {
                let proj = Proj::from_proj_string(&proj_string).map_err(|e| {
                    anyhow::anyhow!(
                        "source {:?}: crs {:?} did not parse as a projection: {e}",
                        spec.path, crs
                    )
                })?;
                if proj.is_latlong() {
                    SourceKind::Geographic
                } else {
                    let group_idx = groups.len();
                    let proj = Arc::new(proj);
                    proj_cache.insert(proj_string.clone(), (group_idx, proj.clone()));
                    groups.push(ProjGroup { proj, min_scale: img.scale_x.min(img.scale_y) });
                    SourceKind::Projected(group_idx)
                }
            }
        };
        if let SourceKind::Projected(gi) = kind {
            groups[gi].min_scale = groups[gi].min_scale.min(img.scale_x).min(img.scale_y);
        }
        proj_strings.push(matches!(kind, SourceKind::Projected(_)).then_some(proj_string));
        kinds.push(kind);
    }

    // Each file source's WGS84 bounding box, for the per-band shadowing
    // test below: geographic extents as they are, projected ones from the
    // header-time box when it was computed for this same CRS.
    let bbox_of = |i: usize| -> Option<[f64; 4]> {
        let Src::File(f) = &srcs[i] else { return None };
        match (&kinds[i], &proj_strings[i]) {
            (SourceKind::Geographic, _) => Some(geographic_bbox(&geoms[i])),
            (SourceKind::Projected(gi), Some(ps)) => match &f.wgs84_bbox {
                Some((cached, bb)) if cached == ps => *bb,
                _ => wgs84_bbox_of_projected(&geoms[i], &groups[*gi].proj, &wgs84),
            },
            _ => None,
        }
    };
    #[cfg(feature = "native")]
    let bboxes: Vec<Option<[f64; 4]>> = (0..srcs.len()).into_par_iter().map(bbox_of).collect();
    #[cfg(not(feature = "native"))]
    let bboxes: Vec<Option<[f64; 4]>> = (0..srcs.len()).map(bbox_of).collect();

    let deg_per_meter = 1.0 / 111111.0;
    let pixel_deg = job.resolution_m * deg_per_meter;
    // Cell-CENTRE grid (CONTRACT §6: a pixel averages the ground footprint it
    // stands for). Anchoring the ±half-pixel window on the NW corner instead
    // sampled everything half an output pixel to the north-west.
    let ul_lon_c = job.ul_lon + 0.5 * pixel_deg;
    let ul_lat_c = job.ul_lat - 0.5 * pixel_deg;
    let out_size = job.size_px as usize;
    let total_pixels = out_size * out_size;
    // Every pixel is written by the sampling loop below (each band covers
    // whole rows, each row every column) before anything reads the buffer,
    // so a written tile's buffer is reused as it is — no zero fill, no fresh
    // pages. Only a buffer that is too short grows (zero-filling the growth).
    let mut buffer = match spare {
        Some(mut b) if b.capacity() >= total_pixels && b.capacity() / 4 <= total_pixels => {
            if b.len() >= total_pixels {
                b.truncate(total_pixels);
            } else {
                b.resize(total_pixels, 0);
            }
            b
        }
        _ => vec![0i16; total_pixels],
    };

    // What a pixel no source covers becomes: the void sentinel, or the job's
    // requested fill elevation (round half away from zero, saturating i16).
    let void_fill: i16 = match job.void_fill_m {
        Some(m) => {
            let half_metres = (m * 2.0).round();
            if half_metres >= f64::from(i16::MAX) {
                i16::MAX
            } else if half_metres <= f64::from(i16::MIN) {
                i16::MIN
            } else {
                half_metres as i16
            }
        }
        None => VOID_ELEV,
    };

    // Per-column source spans for each *geographic* source, computed once for
    // the whole tile: such a source is axis-aligned in degrees, so a column's
    // footprint is the same on every row. `(0, 0)` means the column falls
    // outside the source. (This is the historical base_tif fast path,
    // byte-identical.)
    let geo_x_spans: Vec<Option<Vec<(u32, u32)>>> = kinds
        .iter()
        .zip(&geoms)
        .map(|(kind, img)| match kind {
            SourceKind::Projected(_) => None,
            SourceKind::Geographic => {
                let half = 0.5 * pixel_deg / img.scale_x;
                Some(
                    (0..out_size)
                        .map(|x| {
                            let pixel_lon = ul_lon_c + (x as f64 * pixel_deg);
                            if pixel_lon >= img.origin_e && pixel_lon < img.limit_e {
                                let px_f = (pixel_lon - img.origin_e) / img.scale_x;
                                if px_f >= 0.0 && (px_f as u32) < img.width {
                                    return sample_span(px_f - half, px_f + half, img.width);
                                }
                            }
                            (0, 0)
                        })
                        .collect(),
                )
            }
        })
        .collect();
    // The columns a geographic source can be read at, over the whole tile —
    // exact, straight from the spans above.
    let geo_x_rect: Vec<Option<(u32, u32)>> = geo_x_spans
        .iter()
        .map(|spans| {
            let spans = spans.as_ref()?;
            let (mut lo, mut hi) = (u32::MAX, 0u32);
            for &(a, b) in spans {
                if a < b {
                    lo = lo.min(a);
                    hi = hi.max(b);
                }
            }
            (lo < hi).then_some((lo, hi))
        })
        .collect();

    // Caller-supplied images are their own window, whole and uncopied.
    let mem_windows: Vec<Option<Window>> = srcs
        .iter()
        .map(|s| match s {
            Src::Mem(img) => Some(Window {
                buf: WinBuf::Img(img.clone()),
                x0: 0,
                y0: 0,
                x1: img.width,
                y1: img.height,
                stride: img.width as usize,
            }),
            Src::File(_) => None,
        })
        .collect();

    /// One row-ready source, in job priority order.
    // `lazy` is `Some(source index)` while a shadowed source's window is not
    // yet fetched for this row (`win` is then empty).
    enum RowSrc<'a> {
        Geo { win: WinView<'a>, lazy: Option<usize>, spans: &'a [(u32, u32)], py0: u32, py1: u32 },
        // `inv_x` / `inv_y` carry 1/scale per axis so the per-pixel footprint
        // maths is multiplies, not a division per pixel per candidate image.
        Prj { img: &'a SourceGeom, win: WinView<'a>, lazy: Option<usize>, inv_x: f64, inv_y: f64, geom_idx: usize },
    }

    // Pixels a source actually reached, folded once per row. Counted before
    // `void_fill` rewrites the holes, which is the only place the answer still
    // exists: after the fill, an uncovered tile and a real sea-level one are
    // the same bytes.
    let covered_px = AtomicU64::new(0);
    let window_miss = std::sync::atomic::AtomicBool::new(false);
    let hold: TileHold = std::sync::Mutex::new(HashMap::new());
    let mut peak_window_bytes: usize = 0;
    let mut bands: u32 = 0;
    // Bytes read for this tile's windows, and the sources whose windows had
    // to be read from disk at all (not all from cache).
    let reads = TileReads::new(srcs.len());
    let win_pool: WinPool = std::sync::Mutex::new(Vec::new());
    let mut sample_ns = 0u64;
    let row_lat_of = |y: usize| ul_lat_c - (y as f64 * pixel_deg);

    // The projected geometry of each band row, one per CRS group
    // ([`band_row_geoms`]). `row_lat` is the row's cell-centre parallel; the
    // second latitude one pixel south gets its own transforms because the
    // cell footprint (±half a pixel around the centre) needs a northing
    // extent, and a projected northing of a WGS84 parallel drifts along the
    // row.

    /// One planned row band.
    struct BandPlan {
        r0: usize,
        r1: usize,
        geoms: Vec<Vec<Option<RowGeom>>>,
        rects: Vec<Option<(u32, u32, u32, u32)>>,
        est: usize,
        shadowed: Vec<bool>,
        /// The reads issued for each eager window.
        ahead: Vec<Option<Vec<RunRead>>>,
    }

    // Plan the band that starts at row `r0` and is at most `band_rows` tall
    // (thinner when its windows would exceed the band budget), and issue its
    // windows' reads: `urgent` for a band that loads right away, else as
    // reads ahead, read while the band before it samples.
    let plan_band = |r0: usize, mut band_rows: usize, urgent: bool| -> BandPlan {
        // Row geometry and each row's footprint extent, per CRS group:
        // computed once for the widest band tried — a band re-planned
        // thinner keeps the prefix it already has.
        let mut band_geoms: Vec<Vec<Option<RowGeom>>> = Vec::new();
        let mut band_exts: Vec<Vec<Option<RowExt>>> = Vec::new();
        loop {
            let r1 = (r0 + band_rows).min(out_size);
            if band_geoms.len() > r1 - r0 {
                band_geoms.truncate(r1 - r0);
                band_exts.truncate(r1 - r0);
            } else if band_geoms.len() < r1 - r0 {
                let mut cols: Vec<std::vec::IntoIter<Option<RowGeom>>> = groups
                    .iter()
                    .map(|g| {
                        band_row_geoms(&wgs84, &g.proj, g.min_scale, (r0, r1), &row_lat_of, ul_lon_c, pixel_deg, out_size)
                            .into_iter()
                    })
                    .collect();
                band_geoms = (r0..r1).map(|_| cols.iter_mut().map(|c| c.next().unwrap()).collect()).collect();
                band_exts = band_geoms
                    .iter()
                    .map(|row| row.iter().map(|g| g.as_ref().map(|g| row_extent(g, out_size))).collect())
                    .collect();
            }

            // The window of each file source this band can read.
            let rects: Vec<Option<(u32, u32, u32, u32)>> = srcs
                .iter()
                .enumerate()
                .map(|(i, src)| {
                    if let Src::Mem(_) = src {
                        return None;
                    }
                    let g = &geoms[i];
                    match kinds[i] {
                        SourceKind::Geographic => {
                            let (x0, x1) = geo_x_rect[i]?;
                            let (mut y0, mut y1) = (u32::MAX, 0u32);
                            for y in r0..r1 {
                                if let Some((a, b)) = geo_row_span(g, row_lat_of(y), pixel_deg) {
                                    y0 = y0.min(a);
                                    y1 = y1.max(b);
                                }
                            }
                            (y0 < y1).then_some((x0, x1, y0, y1))
                        }
                        SourceKind::Projected(gi) => {
                            let mut ext = RowExt::EMPTY;
                            for (row, exts) in band_geoms.iter().zip(&band_exts) {
                                if let (Some(geom), Some(row_ext)) = (&row[gi], &exts[gi]) {
                                    if prj_row_candidate(g, geom) {
                                        ext.union(row_ext);
                                    }
                                }
                            }
                            if ext.is_empty() {
                                return None;
                            }
                            let (inv_x, inv_y) = (1.0 / g.scale_x, 1.0 / g.scale_y);
                            let x0 = px_lo((ext.e_min - g.origin_e) * inv_x, g.width);
                            let x1 = px_hi((ext.e_max - g.origin_e) * inv_x, g.width);
                            let y0 = px_lo((g.origin_n - ext.n_max) * inv_y, g.height);
                            let y1 = px_hi((g.origin_n - ext.n_min) * inv_y, g.height);
                            (x0 < x1 && y0 < y1).then_some((x0, x1, y0, y1))
                        }
                    }
                })
                .collect();
            let est: usize = rects
                .iter()
                .flatten()
                .map(|&(x0, x1, y0, y1)| (x1 - x0) as usize * (y1 - y0) as usize * 2)
                .sum();
            if est > opts.band_budget_bytes && r1 - r0 > 1 {
                band_rows = ((r1 - r0) / 2).max(1);
                continue; // re-plan a thinner band
            }

            // Shadowing: a candidate whose WGS84 box, clipped to the band, lies
            // inside the box of a higher-priority candidate is probably never
            // sampled — the sampler stops at the first source with ground. Its
            // window is decoded only if the sampling loop actually falls through
            // to it (a void in the source above, a gap the box overstates). A
            // window never sampled was never needed, so the bytes do not change;
            // a mosaic of side-by-side tiles shadows nothing and loads eagerly.
            let band_box = [
                job.ul_lon,
                job.ul_lat - r1 as f64 * pixel_deg,
                job.ul_lon + out_size as f64 * pixel_deg,
                job.ul_lat - r0 as f64 * pixel_deg,
            ];
            let shadowed = shadowed_sources(&rects, &bboxes, band_box);

            // The reads of the eager windows' chunks the cache does not hold.
            let mut budget = READ_AHEAD_BYTES;
            let ahead = (0..srcs.len())
                .map(|i| match (&srcs[i], rects[i]) {
                    (Src::File(f), Some(rect)) if !shadowed[i] => {
                        read_window_ahead(f, i, rect, chunk_cache, &hold, urgent, &mut budget, &reads)
                    }
                    _ => None,
                })
                .collect();
            return BandPlan { r0, r1, geoms: std::mem::take(&mut band_geoms), rects, est, shadowed, ahead };
        }
    };

    // Row geometry and planning (`geom` of the `[Rust] tile` line).
    let mut geom_ns = 0u64;
    let plan_clock = Clock::now();
    let mut next = Some(plan_band(0, out_size.clamp(1, MAX_BAND_ROWS), true));
    geom_ns += plan_clock.ns();
    while let Some(BandPlan { r0, r1, geoms: band_geoms, rects, est, shadowed, ahead }) = next.take() {
        // Decode the band's unshadowed windows, sources in parallel.
        let load = |i: usize, ahead: Option<Vec<RunRead>>| -> Result<Option<Window>> {
            match (&srcs[i], rects[i]) {
                (Src::File(f), Some(rect)) => {
                    let mark = IoMark::now();
                    let w = load_window(f, i, rect, chunk_cache, &hold, ahead, &reads, &win_pool);
                    // Reads made on this thread (the `tiff`-decoder paths);
                    // the I/O threads count theirs in `reads`.
                    let (opens, _, bytes, _) = mark.since();
                    diag.dec_bytes.fetch_add(bytes, Ordering::Relaxed);
                    if opens > 0 {
                        reads.files[i].store(true, Ordering::Relaxed);
                    }
                    w.map(Some).with_context(|| format!("loading source {:?}", f.path))
                }
                _ => Ok(None),
            }
        };
        let eager = |(i, ahead): (usize, Option<Vec<RunRead>>)| if shadowed[i] { Ok(None) } else { load(i, ahead) };
        let dec_clock = Clock::now();
        #[cfg(feature = "native")]
        let loaded: Vec<Result<Option<Window>>> = ahead.into_par_iter().enumerate().map(eager).collect();
        #[cfg(not(feature = "native"))]
        let loaded: Vec<Result<Option<Window>>> = ahead.into_iter().enumerate().map(eager).collect();
        diag.dec_ns.fetch_add(dec_clock.ns(), Ordering::Relaxed);
        let mut windows: Vec<Option<Window>> = Vec::with_capacity(loaded.len());
        for w in loaded {
            windows.push(w?);
        }

        // Plan the next band now: its reads go out while this one samples.
        // It is sized from this band's measured density, aiming just under
        // the budget.
        if r1 < out_size {
            let band_rows = if est == 0 {
                MAX_BAND_ROWS
            } else {
                let scaled = (r1 - r0) as f64 * (opts.band_budget_bytes as f64 / est as f64) * 0.9;
                (scaled as usize).clamp(1, MAX_BAND_ROWS)
            };
            let plan_clock = Clock::now();
            next = Some(plan_band(r1, band_rows, false));
            geom_ns += plan_clock.ns();
        }
        // Sources this band read and the next does not: this tile is done
        // with them (bands run north to south), so their open handles go
        // first when the cache needs room.
        chunk_cache.spare_files((0..srcs.len()).filter_map(|i| match &srcs[i] {
            Src::File(f) if rects[i].is_some() && next.as_ref().map_or(true, |p| p.rects[i].is_none()) => Some(f.id),
            _ => None,
        }));

        // Shadowed windows, decoded once on first use by whichever row needs
        // them (other rows wait for that one decode).
        let lazy: Vec<OnceLock<Result<Option<Window>>>> = (0..srcs.len()).map(|_| OnceLock::new()).collect();
        let lazy_failed = AtomicBool::new(false);
        let lazy_view = |i: usize| -> WinView<'_> {
            let loaded = lazy[i].get_or_init(|| {
                let t = Clock::now();
                let w = load(i, None);
                diag.lazy_ns.fetch_add(t.ns(), Ordering::Relaxed);
                w
            });
            match loaded {
                Ok(Some(w)) => w.view(),
                Ok(None) => WinView::EMPTY,
                Err(_) => {
                    lazy_failed.store(true, Ordering::Relaxed);
                    WinView::EMPTY
                }
            }
        };

        let views: Vec<WinView> = windows
            .iter()
            .zip(&mem_windows)
            .map(|(w, m)| match (w, m) {
                (Some(w), _) | (None, Some(w)) => w.view(),
                (None, None) => WinView::EMPTY,
            })
            .collect();

        // Terrain Rasterization
        let sample_clock = Clock::now();
        let band_buf = &mut buffer[r0 * out_size..r1 * out_size];
        #[cfg(feature = "native")]
        let iter = band_buf.par_chunks_mut(out_size);
        #[cfg(not(feature = "native"))]
        let iter = band_buf.chunks_mut(out_size);
        iter.enumerate().for_each(|(k, row_buffer)| {
            let y = r0 + k;
            let mut row_covered: u64 = 0;
            let mut row_miss = false;
            let row_lat = row_lat_of(y);
            let row_geoms = &band_geoms[k];

            // Row candidates, in source priority order. A shadowed source
            // carries its index in `lazy` until the row first needs it.
            let mut row_srcs: Vec<RowSrc> = Vec::with_capacity(sources.len());
            for (i, (kind, img)) in kinds.iter().zip(&geoms).enumerate() {
                let lazy = shadowed[i].then_some(i);
                match kind {
                    SourceKind::Geographic => {
                        if let Some((py0, py1)) = geo_row_span(img, row_lat, pixel_deg) {
                            row_srcs.push(RowSrc::Geo {
                                win: views[i],
                                lazy,
                                spans: geo_x_spans[i].as_deref().unwrap(),
                                py0,
                                py1,
                            });
                        }
                    }
                    SourceKind::Projected(gi) => {
                        if let Some(geom) = &row_geoms[*gi] {
                            if prj_row_candidate(img, geom) {
                                row_srcs.push(RowSrc::Prj {
                                    img,
                                    win: views[i],
                                    lazy,
                                    inv_x: 1.0 / img.scale_x,
                                    inv_y: 1.0 / img.scale_y,
                                    geom_idx: *gi,
                                });
                            }
                        }
                    }
                }
            }

            // Per block of 64 pixels (one geometry segment), the candidates
            // that can hold any of its cell centres. Within a segment a
            // centre coordinate is `c0 + step * lx` — monotone in `lx` — so
            // its extremes over the block are at the block's first and last
            // pixel, evaluated with the loop's own expressions; a projected
            // candidate whose rectangle those extremes miss fails the
            // containment test at every pixel of the block and is skipped
            // there. The rest are scanned in priority order exactly as
            // before, so the first source with ground wins at every pixel.
            let block = 1usize << CHUNK_SHIFT;
            let n_blocks = out_size.div_ceil(block);
            let mut picks: Vec<u32> = Vec::with_capacity(row_srcs.len() * 2);
            let mut starts: Vec<u32> = Vec::with_capacity(n_blocks + 1);
            let mut centres: Vec<Option<(f64, f64, f64, f64)>> = vec![None; row_geoms.len()];
            for b in 0..n_blocks {
                starts.push(picks.len() as u32);
                let (bx0, bx1) = (b * block, ((b + 1) * block).min(out_size));
                for (slot, geom) in centres.iter_mut().zip(row_geoms) {
                    *slot = geom.as_ref().map(|g| centre_bounds(g, bx0, bx1 - 1));
                }
                for (j, src) in row_srcs.iter().enumerate() {
                    let keep = match src {
                        RowSrc::Geo { .. } => true,
                        RowSrc::Prj { img, geom_idx, .. } => centres[*geom_idx].map_or(true, |c| may_hold(img, c)),
                    };
                    if keep {
                        picks.push(j as u32);
                    }
                }
            }
            starts.push(picks.len() as u32);

            for b in 0..n_blocks {
                let cands = &picks[starts[b] as usize..starts[b + 1] as usize];
                let (bx0, bx1) = (b * block, ((b + 1) * block).min(out_size));

                // Fast path: one projected candidate for the whole block — the
                // inside of a source file, where at ratio ~1 every cell is one
                // sample. The same arithmetic per pixel as the loop below,
                // in the same order, without the candidate scan: the block's
                // segment is fixed, and the footprint edges are formed only
                // when the centre is inside the file.
                if let [j] = *cands {
                    if let RowSrc::Prj { img, win, lazy, inv_x, inv_y, geom_idx } = &mut row_srcs[j as usize] {
                        let geom = row_geoms[*geom_idx].as_ref().unwrap();
                        let seg = &geom.segs[bx0 >> geom.shift];
                        for (x, out_pixel) in (bx0..bx1).zip(&mut row_buffer[bx0..bx1]) {
                            let lx = (x & geom.mask) as f64;
                            let e = seg.e0 + seg.step_e * lx;
                            let n = seg.n0 + seg.step_n * lx;
                            let mut val = VOID_ELEV;
                            if n <= img.origin_n && n >= img.limit_n && e >= img.origin_e && e < img.limit_e {
                                let e_lo = e - seg.step_e * 0.5;
                                let e_hi = e + seg.step_e * 0.5;
                                let half_n = (n - (seg.nlo0 + seg.step_nlo * lx)) * 0.5;
                                let (n_hi, n_lo) = (n + half_n, n - half_n);
                                let (px0, px1) = sample_span(
                                    (e_lo - img.origin_e) * *inv_x,
                                    (e_hi - img.origin_e) * *inv_x,
                                    img.width,
                                );
                                let (py0, py1) = sample_span(
                                    (img.origin_n - n_hi) * *inv_y,
                                    (img.origin_n - n_lo) * *inv_y,
                                    img.height,
                                );
                                if let Some(i) = *lazy {
                                    *lazy = None;
                                    *win = lazy_view(i);
                                }
                                if let Some(v) = sample_cell(win, px0, px1, py0, py1, &mut row_miss) {
                                    val = v;
                                }
                            }
                            if val == VOID_ELEV {
                                *out_pixel = void_fill;
                            } else {
                                row_covered += 1;
                                *out_pixel = val;
                            }
                        }
                        continue;
                    }
                }

                for x in bx0..bx1 {
                    let out_pixel = &mut row_buffer[x];
                    let mut val = VOID_ELEV;
                    // Consecutive candidates in the same CRS reuse the pixel's
                    // transformed cell footprint — one lerp per pixel per CRS, so a
                    // stack of same-CRS tiles costs what the old single-CRS loop cost.
                    let mut cached_geom = usize::MAX;
                    let mut cell = (0.0f64, 0.0f64, 0.0f64, 0.0f64, 0.0f64, 0.0f64);

                    for &j in cands {
                        match &mut row_srcs[j as usize] {
                            RowSrc::Geo { win, lazy, spans, py0, py1 } => {
                                let (px0, px1) = spans[x];
                                if px0 < px1 {
                                    if let Some(i) = *lazy {
                                        *lazy = None;
                                        *win = lazy_view(i);
                                    }
                                    if let Some(v) = sample_cell(win, px0, px1, *py0, *py1, &mut row_miss) {
                                        val = v;
                                        break;
                                    }
                                }
                            }
                            RowSrc::Prj { img, win, lazy, inv_x, inv_y, geom_idx } => {
                                if *geom_idx != cached_geom {
                                    // The cell this output pixel stands for: one
                                    // output pixel wide and one output row tall,
                                    // centred on the cell's own centre (e, n) — the
                                    // ground footprint CONTRACT §6 promises the
                                    // average is taken over.
                                    let geom = row_geoms[*geom_idx].as_ref().unwrap();
                                    let seg = &geom.segs[x >> geom.shift];
                                    let lx = (x & geom.mask) as f64;
                                    let e = seg.e0 + seg.step_e * lx;
                                    let n = seg.n0 + seg.step_n * lx;
                                    let e_lo = e - seg.step_e * 0.5;
                                    let e_hi = e + seg.step_e * 0.5;
                                    let half_n = (n - (seg.nlo0 + seg.step_nlo * lx)) * 0.5;
                                    cell = (e, n, e_lo, e_hi, n + half_n, n - half_n);
                                    cached_geom = *geom_idx;
                                }
                                let (e, n, e_lo, e_hi, n_hi, n_lo) = cell;
                                if n <= img.origin_n && n >= img.limit_n
                                    && e >= img.origin_e && e < img.limit_e
                                {
                                    let (px0, px1) = sample_span(
                                        (e_lo - img.origin_e) * *inv_x,
                                        (e_hi - img.origin_e) * *inv_x,
                                        img.width,
                                    );
                                    let (py0, py1) = sample_span(
                                        (img.origin_n - n_hi) * *inv_y,
                                        (img.origin_n - n_lo) * *inv_y,
                                        img.height,
                                    );
                                    // First source that has real ground under the cell
                                    // wins, as before. The footprint is clipped to that
                                    // one image, so a cell straddling two source tiles
                                    // averages the part inside the tile its corner
                                    // landed in — still an average of real terrain.
                                    if let Some(i) = *lazy {
                                        *lazy = None;
                                        *win = lazy_view(i);
                                    }
                                    if let Some(v) = sample_cell(win, px0, px1, py0, py1, &mut row_miss) {
                                        val = v;
                                        break;
                                    }
                                }
                            }
                        }
                    }

                    if val == VOID_ELEV {
                        *out_pixel = void_fill;
                    } else {
                        row_covered += 1;
                        *out_pixel = val;
                    }
                }
            }
            covered_px.fetch_add(row_covered, Ordering::Relaxed);
            if row_miss {
                window_miss.store(true, Ordering::Relaxed);
            }
        });
        sample_ns += sample_clock.ns();

        let lazy_bytes: usize = lazy
            .iter()
            .filter_map(|c| match c.get() {
                Some(Ok(Some(w))) => Some(w.owned_bytes()),
                _ => None,
            })
            .sum();
        let held: usize = hold.lock().unwrap().values().map(|d| d.len() * 2).sum();
        let band_bytes = windows.iter().flatten().map(Window::owned_bytes).sum::<usize>() + lazy_bytes + held;
        peak_window_bytes = peak_window_bytes.max(band_bytes);

        // A shadowed window that failed to decode fails the tile exactly as
        // an eager one does (first failure in source order).
        if lazy_failed.load(Ordering::Relaxed) {
            for cell in lazy {
                if let Some(Err(e)) = cell.into_inner() {
                    return Err(e);
                }
            }
            anyhow::bail!("internal error converting {:?}: a deferred source window failed without an error", job.output_path);
        }

        if window_miss.load(Ordering::Relaxed) {
            anyhow::bail!(
                "internal error converting {:?}: rows {}..{} read source pixels outside \
                 the window planned for them; refusing to write a possibly wrong tile \
                 (please report this with the job file)",
                job.output_path, r0, r1
            );
        }
        // The band's window buffers go back for the next band's windows.
        let lazy_windows = lazy.into_iter().filter_map(|c| c.into_inner().and_then(Result::ok).flatten());
        for w in windows.into_iter().flatten().chain(lazy_windows) {
            if let WinBuf::Own(v) = w.buf {
                give_buf(&win_pool, v, KEEP_WIN_BYTES.min(opts.band_budget_bytes));
            }
        }
        bands += 1;
    }
    drop(hold);

    // The per-tile diagnostic line (changelog 31/37, not parsed). Header
    // averages are over the headers this tile actually read (cache misses);
    // deferred window loads run inside sampling and are counted as decode.
    // Printed once the tile is written (or has failed after sampling).
    let line = {
        let opens = diag.hdr_opens.load(Ordering::Relaxed);
        let avg_ms = |ns: &AtomicU64| {
            if opens > 0 { ns.load(Ordering::Relaxed) as f64 / opens as f64 / 1e6 } else { 0.0 }
        };
        let lazy_ns = diag.lazy_ns.load(Ordering::Relaxed);
        TileLine {
            name: job.output_path.file_name().map_or_else(
                || job.output_path.display().to_string(),
                |n| n.to_string_lossy().into_owned(),
            ),
            sources: sources.len(),
            header_s: hdr_ns as f64 / 1e9,
            open_ms: avg_ms(&diag.hdr_open_ns),
            read_ms: avg_ms(&diag.hdr_read_ns),
            decode_s: (diag.dec_ns.load(Ordering::Relaxed) + lazy_ns) as f64 / 1e9,
            files: reads.files.iter().filter(|f| f.load(Ordering::Relaxed)).count(),
            mb: (diag.dec_bytes.load(Ordering::Relaxed) + reads.bytes.load(Ordering::Relaxed)) as f64
                / (1024.0 * 1024.0),
            sample_s: sample_ns.saturating_sub(lazy_ns) as f64 / 1e9,
            bands,
            geom_s: geom_ns as f64 / 1e9,
        }
    };

    let stats = TileStats {
        covered_px: covered_px.load(Ordering::Relaxed),
        total_px: total_pixels as u64,
        peak_window_bytes: peak_window_bytes as u64,
        bands,
    };

    // Everything after sampling. A failure here still prints the tile's
    // diagnostic line (its terrain was sampled), then fails the tile.
    let finished = (|| -> Result<(Vec<u8>, TileBody)> {
        let mut buffer = buffer;
        // Building Rasterization
        if let Some(fgb) = &job.buildings_file {
            // A buildings source that cannot be read fails the tile, exactly as an
            // unusable `buildings_pbf_dir` does below. This used to warn and exit 0,
            // which wrote building-less terrain under an identity that claims
            // buildings — a cache hit forever, for every later run.
            if let Err(e) = apply_buildings(&job, &mut buffer, fgb, pixel_deg) {
                anyhow::bail!(
                    "failed to apply buildings from buildings_file {:?} to {:?}: {:#}; \
                     refusing to write a building-less tile",
                    fgb,
                    job.output_path,
                    e
                );
            }
        }
        match (&job.buildings_pbf_dir, pbf_buildings) {
            (Some(_), Some(set)) => apply_buildings_pbf(&job, &mut buffer, set, pixel_deg),
            (Some(dir), None) => anyhow::bail!(
                "buildings_pbf_dir {:?} was requested but no decoded building set was \
                 supplied for {:?}; refusing to write a building-less tile",
                dir,
                job.output_path
            ),
            (None, _) => {}
        }

        let format_str = job.format.as_deref().unwrap_or("r16sint");
        let is_bc6h = format_str.eq_ignore_ascii_case("bc6h");

        #[cfg(not(feature = "bc6h"))]
        if is_bc6h {
            return Err(anyhow::anyhow!("BC6H format requested but binary was built without the 'bc6h' feature"));
        }

        // Dynamic BaseElev calculation for BC6H
        let mut base_elev = 0i16;
        if is_bc6h {
            let mut min_elev = i16::MAX;
            for &v in buffer.iter() {
                if v > -5000 && v < min_elev {
                    min_elev = v;
                }
            }
            if min_elev == i16::MAX { min_elev = 0; }
            base_elev = min_elev;
        }

        // The 44-byte header (contract §6), assembled here and written with the
        // payload by `write_tile`.
        let mut w: Vec<u8> = Vec::with_capacity(ABT_HEADER_BYTES as usize);

        w.write_all(b"AETH")?;

        // Version 2 for BC6H, Version 1 for legacy R16SINT
        let version = if is_bc6h { 2u16 } else { 1u16 };
        w.write_u16::<LittleEndian>(version)?;
        w.write_u16::<LittleEndian>(job.size_px as u16)?;
        w.write_f64::<LittleEndian>(job.ul_lat)?;
        w.write_f64::<LittleEndian>(job.ul_lon)?;
        w.write_f64::<LittleEndian>(pixel_deg)?;
        w.write_f64::<LittleEndian>(pixel_deg)?;
        w.write_i16::<LittleEndian>(base_elev)?;

        let bytes_per_row_r16 = job.size_px as u32 * 2;
        let aligned_stride_r16 = (bytes_per_row_r16 + 255) & !255;

        // Calculate final physical row stride (to skip padding during decode)
        let final_stride = if is_bc6h {
            // BC6H block is 16 bytes. Row stride is the width of blocks * 16.
            ((job.size_px + 3) / 4 * 16) as u16
        } else {
            aligned_stride_r16 as u16
        };
        w.write_u16::<LittleEndian>(final_stride)?;
        let header = w;

        #[cfg(feature = "bc6h")]
        if is_bc6h {
             // 1. Map terrain to 0-aligned RGBA float buffer
            let mut rgba = vec![0.0f32; total_pixels * 4];
            for (i, &v) in buffer.iter().enumerate() {
                let real_val = if v > -5000 {
                    (v - base_elev) as f32 * 0.5 + 1.0
                } else {
                    0.0
                };
                rgba[i * 4] = real_val;
                rgba[i * 4 + 1] = real_val;
                rgba[i * 4 + 2] = real_val;
                rgba[i * 4 + 3] = 1.0;
            }

            // ═════════════════════════════════════════════════════════════════════
            // [BC6H-DEBUG] Sample values before encoding
            // ═════════════════════════════════════════════════════════════════════
            let sample_positions: Vec<(usize, usize)> = vec![
                (0, 0), (1, 0), (2, 0), (3, 0),
                (0, 1), (1, 1), (2, 1), (3, 1),
                (out_size/2, out_size/2),
                (out_size/2+1, out_size/2),
                (out_size-1, out_size-1),
            ];
            eprintln!("[BC6H-DEBUG] PRE-ENCODE sample values:");
            for &(x, y) in &sample_positions {
                if x < out_size && y < out_size {
                    let idx = y * out_size + x;
                    eprintln!("[BC6H-DEBUG]   px({:4},{:4}) raw_i16={:6} → R={:12.4}",
                              x, y, buffer[idx], rgba[idx * 4]);
                }
            }

            // 2. Encode
            let surface = SurfaceRgba32Float {
                width: job.size_px,
                height: job.size_px,
                depth: 1,
                layers: 1,
                mipmaps: 1,
                data: rgba.clone(),
            };

            eprintln!("[BC6H-DEBUG] Calling image_dds encode(BC6Sfloat, Fast) ...");
            let dds_surface = surface.encode(
                ImageFormat::BC6Ufloat,
                Quality::Fast,
                Mipmaps::Disabled
            ).map_err(|e| anyhow::anyhow!("BC6H Encoding failed: {:?}", e))?;

            return Ok((header, TileBody::Raw(dds_surface.data)));
        }

        let padding_bytes = aligned_stride_r16 - bytes_per_row_r16;
        Ok((header, TileBody::Rows { buf: buffer, size: job.size_px as usize, pad: padding_bytes as usize }))
    })();
    match finished {
        Ok((header, body)) => Ok(ReadyTile {
            dst: job.output_path,
            header,
            body,
            stats,
            line,
            t0,
            #[cfg(test)]
            fail_after: FAIL_TILE_WRITES_AFTER.with(|c| c.get()),
        }),
        Err(e) => {
            line.print(0, t0.ns());
            Err(e)
        }
    }
}

#[cfg(test)]
thread_local! {
    /// Test hook: when set, the next [`TileFile`]s created on this thread
    /// fail like a full disk once this many bytes have reached the file.
    static FAIL_TILE_WRITES_AFTER: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
}

/// The file under a [`TileFile`]'s buffer.
struct TileSink {
    file: File,
    /// Bytes left before writes fail with a simulated ENOSPC (tests only).
    #[cfg(test)]
    fail_after: Option<usize>,
}

impl Write for TileSink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        #[cfg(test)]
        if let Some(left) = self.fail_after.as_mut() {
            if *left == 0 {
                return Err(std::io::Error::other("No space left on device (simulated ENOSPC)"));
            }
            let n = self.file.write(&buf[..buf.len().min(*left)])?;
            *left -= n;
            return Ok(n);
        }
        self.file.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
}

/// An output tile being written. Bytes go to `<output>.tmp-<pid>` in the
/// output's own directory; [`TileFile::commit`] flushes, `sync_data`s, closes
/// and only then renames the temp file over `<output>` (atomic on one
/// volume). Dropped uncommitted — any `?` between create and commit — it
/// removes the temp file. So a write that fails part-way (ENOSPC, an I/O
/// error, a failed final flush, which `BufWriter`'s drop used to swallow)
/// never leaves a truncated tile under the final name, and a tile that
/// already exists keeps its old inode: it is replaced by rename, never
/// truncated in place, so hardlinks to it (views of pool tiles) keep the old
/// bytes.
pub(crate) struct TileFile {
    w: Option<BufWriter<TileSink>>,
    tmp: PathBuf,
    dst: PathBuf,
    done: bool,
}

impl TileFile {
    /// `<output>.tmp-<pid>`, beside the output so the rename stays on one
    /// volume.
    pub(crate) fn temp_path(dst: &Path) -> PathBuf {
        let mut s = dst.as_os_str().to_owned();
        s.push(format!(".tmp-{}", std::process::id()));
        PathBuf::from(s)
    }

    #[allow(dead_code)]
    pub(crate) fn create(dst: &Path) -> Result<TileFile> {
        Self::open_temp(
            dst,
            #[cfg(test)]
            FAIL_TILE_WRITES_AFTER.with(|c| c.get()),
        )
    }

    /// [`TileFile::create`] with the test hook passed in rather than read
    /// from this thread (the tile may be written on another one).
    fn open_temp(dst: &Path, #[cfg(test)] fail_after: Option<usize>) -> Result<TileFile> {
        let tmp = Self::temp_path(dst);
        let file = File::create(&tmp)
            .with_context(|| format!("creating output tile {:?} (as {:?})", dst, tmp))?;
        let sink = TileSink {
            file,
            #[cfg(test)]
            fail_after,
        };
        Ok(TileFile {
            w: Some(BufWriter::with_capacity(1024 * 1024, sink)),
            tmp,
            dst: dst.to_path_buf(),
            done: false,
        })
    }

    /// Flush, sync and close the temp file, then rename it over the output.
    /// On any error the temp file is removed and the output is untouched.
    pub(crate) fn commit(mut self) -> Result<()> {
        let mut w = self.w.take().expect("a TileFile is committed once");
        w.flush().with_context(|| format!("writing output tile {:?}", self.tmp))?;
        // Durability where the volume offers it: a filesystem without fsync
        // support (network and exFAT mounts, macOS F_FULLFSYNC on SMB) must
        // not fail every tile — the rename below still replaces the output
        // atomically. Any other sync error is still the tile's error.
        if let Err(e) = w.get_ref().file.sync_data() {
            if !sync_unsupported(&e) {
                return Err(e).with_context(|| format!("syncing output tile {:?}", self.tmp));
            }
        }
        drop(w); // closed before the rename: an open handle can fail it on Windows
        rename_with_retry(&self.tmp, &self.dst)
            .with_context(|| format!("renaming {:?} over output tile {:?}", self.tmp, self.dst))?;
        self.done = true;
        Ok(())
    }
}

/// `sync_data` errors that mean "this volume cannot sync" rather than "the
/// data did not reach the disk": `Unsupported` (ENOTSUP / EOPNOTSUPP) and
/// `InvalidInput` (EINVAL, which some network and exFAT mounts answer).
fn sync_unsupported(e: &std::io::Error) -> bool {
    matches!(e.kind(), std::io::ErrorKind::Unsupported | std::io::ErrorKind::InvalidInput)
}

/// A rename refused for a moment: on Windows a file a scanner (Defender's
/// on-close scan) or an indexer still holds open answers "access denied" or
/// a sharing violation (os errors 5 and 32) until it lets go. POSIX renames
/// never fail that way, so the loop runs once there.
fn rename_transient(e: &std::io::Error) -> bool {
    e.kind() == std::io::ErrorKind::PermissionDenied
        || (cfg!(windows) && matches!(e.raw_os_error(), Some(5) | Some(32)))
}

/// `fs::rename` that waits out a transient refusal ([`rename_transient`]):
/// up to ten attempts 100 ms apart, then the last error. Single attempt on
/// wasm32 (no sleeping there).
fn rename_with_retry(from: &Path, to: &Path) -> std::io::Result<()> {
    let retries = if cfg!(target_arch = "wasm32") { 0 } else { 9 };
    let mut attempt = 0;
    loop {
        match fs::rename(from, to) {
            Ok(()) => return Ok(()),
            Err(e) if attempt < retries && rename_transient(&e) => {
                attempt += 1;
                #[cfg(not(target_arch = "wasm32"))]
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            Err(e) => return Err(e),
        }
    }
}

impl Write for TileFile {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.w.as_mut().expect("a TileFile is not written after commit").write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.w.as_mut().expect("a TileFile is not written after commit").flush()
    }
}

impl Drop for TileFile {
    fn drop(&mut self) {
        if !self.done {
            drop(self.w.take()); // close first: Windows cannot remove an open file
            let _ = fs::remove_file(&self.tmp);
        }
    }
}

#[cfg(test)]
mod tile_file_tests {
    use super::*;

    /// A one-source geographic job over an in-memory image: `(job, cache)`.
    fn job(dir: &Path, size_px: u32) -> (IngestJob, ImageCache) {
        let src = dir.join("src.tif");
        let job: IngestJob = serde_json::from_value(serde_json::json!({
            "output_path": dir.join("tile.abt"),
            "format": "r16sint",
            "ul_lat": 47.0, "ul_lon": 8.0, "resolution_m": 10.0, "size_px": size_px,
            "sources": [{"path": src, "crs": "EPSG:4326"}],
        }))
        .unwrap();
        let (w, h, scale) = (2400u32, 2400u32, 0.0001);
        let img = LoadedImage {
            width: w,
            height: h,
            data: (0..w * h).map(|i| (i % 977) as i16).collect(),
            origin_e: 7.99,
            origin_n: 47.01,
            limit_e: 7.99 + w as f64 * scale,
            limit_n: 47.01 - h as f64 * scale,
            scale_x: scale,
            scale_y: scale,
            name: "mem".into(),
            embedded_crs: None,
            is_abt: false,
        };
        let mut cache = HashMap::new();
        cache.insert((src, None), Arc::new(img));
        (job, Arc::new(std::sync::Mutex::new(cache)))
    }

    fn convert(dir: &Path, size_px: u32, fail_after: Option<usize>) -> Result<TileStats> {
        let (job, cache) = job(dir, size_px);
        FAIL_TILE_WRITES_AFTER.with(|c| c.set(fail_after));
        let r = process_tile(job, cache, None);
        FAIL_TILE_WRITES_AFTER.with(|c| c.set(None));
        r
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> =
            fs::read_dir(dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        v.sort();
        v
    }

    #[test]
    fn a_full_disk_leaves_no_tile_and_no_temp_file() {
        // 16 px: the whole tile fits the 1 MiB buffer, so the failure surfaces
        // at the final flush — the error BufWriter's drop used to swallow,
        // leaving a truncated tile and an exit 0. 1024 px (2 MiB): it
        // surfaces in the middle of the payload writes.
        for (size, fail_after) in [(16u32, 0usize), (16, 100), (1024, 1 << 20), (1024, 0)] {
            let dir = tempfile::tempdir().unwrap();
            let err = convert(dir.path(), size, Some(fail_after)).unwrap_err();
            assert!(format!("{err:#}").contains("No space left on device"), "{err:#}");
            assert_eq!(names(dir.path()), Vec::<String>::new(), "size {size}, fail after {fail_after}");
        }
    }

    #[test]
    fn a_failed_rewrite_keeps_the_old_tile_and_its_hardlinks() {
        let dir = tempfile::tempdir().unwrap();
        let tile = dir.path().join("tile.abt");
        fs::write(&tile, b"OLD TILE").unwrap();
        fs::hard_link(&tile, dir.path().join("view.abt")).unwrap();
        assert!(convert(dir.path(), 16, Some(10)).is_err());
        assert_eq!(fs::read(&tile).unwrap(), b"OLD TILE");
        assert_eq!(fs::read(dir.path().join("view.abt")).unwrap(), b"OLD TILE");
        assert_eq!(names(dir.path()), vec!["tile.abt", "view.abt"]);
    }

    #[test]
    fn a_successful_rewrite_replaces_by_rename_and_leaves_hardlinks_alone() {
        let fresh = tempfile::tempdir().unwrap();
        convert(fresh.path(), 16, None).unwrap();
        let expected = fs::read(fresh.path().join("tile.abt")).unwrap();
        assert_eq!(expected.len(), 44 + 16 * 256);

        let dir = tempfile::tempdir().unwrap();
        let tile = dir.path().join("tile.abt");
        fs::write(&tile, b"OLD TILE").unwrap();
        fs::hard_link(&tile, dir.path().join("view.abt")).unwrap();
        convert(dir.path(), 16, None).unwrap();
        assert_eq!(fs::read(&tile).unwrap(), expected, "the new tile, byte for byte");
        // The view still holds the old inode: the tile was replaced, never
        // truncated and rewritten in place.
        assert_eq!(fs::read(dir.path().join("view.abt")).unwrap(), b"OLD TILE");
        assert_eq!(names(dir.path()), vec!["tile.abt", "view.abt"]);
    }

    #[test]
    fn the_temp_file_sits_beside_the_output_and_names_the_process() {
        let p = TileFile::temp_path(Path::new("/x/out/tile_N47.00E8.00_10m.abt"));
        assert_eq!(p, PathBuf::from(format!("/x/out/tile_N47.00E8.00_10m.abt.tmp-{}", std::process::id())));
    }
}

/// Burn a `buildings_file` — one `.fgb`, or a directory of them — into *buffer*.
///
/// Errors here are fatal to the tile. A source that cannot be opened, parsed or
/// scanned used to be a warning and an exit 0, which handed the caller a
/// building-*less* tile under an identity claiming buildings; the caller then
/// cached it and every later run was a hit. Finding **no** buildings over this
/// particular tile is not that: it is the ordinary fate of an edge tile, so it
/// is reported and the terrain is written.
fn apply_buildings(job: &IngestJob, buffer: &mut [i16], fgb_target: &Path, px_deg: f64) -> Result<()> {
    let mut files_to_process = Vec::new();
    if fgb_target.is_dir() {
        let entries = fs::read_dir(fgb_target)
            .with_context(|| format!("buildings_file directory {:?} cannot be read", fgb_target))?;
        for entry in entries.flatten() {
            if entry.path().extension().map_or(false, |ext| ext == "fgb") {
                files_to_process.push(entry.path());
            }
        }
        if files_to_process.is_empty() {
            anyhow::bail!("buildings_file directory {:?} holds no .fgb file", fgb_target);
        }
        // Directory order is not defined; sorting makes the reported counts
        // reproducible run to run. The drawn surface never depended on it —
        // `max` compositing is order-independent.
        files_to_process.sort();
    } else {
        files_to_process.push(fgb_target.to_path_buf());
    }

    let lr_lat = job.ul_lat - (job.size_px as f64 * px_deg);
    let lr_lon = job.ul_lon + (job.size_px as f64 * px_deg);
    let pad_deg = 20.0 / 111111.0;

    let min_lon = job.ul_lon.min(lr_lon) - pad_deg;
    let max_lon = job.ul_lon.max(lr_lon) + pad_deg;
    let min_lat = job.ul_lat.min(lr_lat) - pad_deg;
    let max_lat = job.ul_lat.max(lr_lat) + pad_deg;

    let mut all_buildings: Vec<Building> = Vec::new();
    let mut features_seen = 0usize;

    for fgb_path in files_to_process {
        let file = File::open(&fgb_path)
            .with_context(|| format!("buildings_file {:?} cannot be opened", fgb_path))?;
        let fgb = FgbReader::open(BufReader::new(file))
            .with_context(|| format!("buildings_file {:?} is not a readable FlatGeobuf", fgb_path))?;

        if let Some(env) = fgb.header().envelope() {
            if env.get(0) > max_lon || env.get(2) < min_lon || env.get(1) > max_lat || env.get(3) < min_lat {
                continue;
            }
        }

        // A FlatGeobuf writes the geometry type ONCE, in the header; a feature
        // repeats it only in a mixed-type dataset, where the header says
        // `Unknown`. Reading it off the feature alone — which this did — makes
        // every feature of an ordinary single-type file look like `Unknown`
        // and drops the lot. This is the resolution order the format's own
        // reader uses (`flatgeobuf::geometry_reader::read_geometry`).
        let header_type = fgb.header().geometry_type();

        let mut features = fgb
            .select_bbox(min_lon, min_lat, max_lon, max_lat)
            .with_context(|| format!("buildings_file {:?} cannot be scanned", fgb_path))?;
        while let Some(feature) = features.next()? {
            let Some(geo) = feature.geometry() else { continue };
            let g_type = match geo.type_() {
                GeometryType::Unknown => header_type,
                t => t,
            };
            // TIN and PolyhedralSurface are polygon collections (the shape
            // GDAL writes for swissBUILDINGS3D 3.0 solids/roofs); a Triangle
            // is a 3-point ring. All walk through `collect_fgb_buildings`
            // exactly like a MultiPolygon — parts recurse, rings split on
            // `ends` — so refusing them dropped whole national datasets on
            // the floor with nothing but the "no footprint to draw" line.
            if !matches!(
                g_type,
                GeometryType::Polygon
                    | GeometryType::MultiPolygon
                    | GeometryType::PolyhedralSurface
                    | GeometryType::TIN
                    | GeometryType::Triangle
            ) {
                continue;
            }
            features_seen += 1;

            // Read every attribute column once, then walk the shared ladder
            // over them. This is only consulted for a geometry with no Z —
            // see `collect_fgb_buildings` — but resolving it here keeps it one
            // property scan per feature rather than one per ring.
            let props = feature.properties().unwrap_or_default();
            let fallback = resolve_building_height(None, |key| {
                props.get(key).map(String::as_str).and_then(parse_leading_metres)
            });

            collect_fgb_buildings(&geo, fallback, &mut all_buildings);
        }
    }

    if all_buildings.is_empty() {
        // Not an error — an edge tile with no buildings over it is ordinary —
        // but never again silent: this line is the only thing separating a
        // burn that drew nothing from one that never ran.
        println!(
            "[Buildings] {}: {} polygon feature(s) in this tile's extent, \
             no footprint to draw",
            fgb_target.display(),
            features_seen
        );
        return Ok(());
    }

    let tile = TileRef {
        ul_lat: job.ul_lat,
        ul_lon: job.ul_lon,
        scale_x: px_deg,
        scale_y: px_deg,
        size_px: job.size_px,
    };
    let mut grid = I16Grid { buf: buffer, size: job.size_px };
    // Truncation, not rounding: this path has always truncated `z * 2.0`, and
    // keeping that keeps previously generated .abt tiles byte-identical.
    let opts = RasterOpts { rounding: Rounding::Truncate, ..Default::default() };
    let stats = rasterize_buildings(&mut grid, &tile, &all_buildings, &opts);

    let measured = all_buildings.iter().filter(|b| b.source.is_measured()).count();
    println!(
        "[Buildings] {}: {} polygon feature(s) → {} footprint(s), {} with a height \
         from the data ({} at the {} m default), {} drawn, {} px raised{}",
        fgb_target.display(),
        features_seen,
        all_buildings.len(),
        measured,
        all_buildings.len() - measured,
        DEFAULT_HEIGHT_M,
        stats.buildings_hit,
        stats.pixels_modified,
        if stats.pixels_modified == 0 {
            " — the surface is unchanged"
        } else {
            ""
        },
    );

    if stats.datum_suspect {
        eprintln!(
            "[WARN] buildings: roofs sit a median {:.1} m from the terrain — the \
             source may be above-ground heights labelled as absolute, or use a \
             different vertical datum than the terrain.",
            stats.median_roof_above_terrain.unwrap_or(0.0)
        );
    }

    Ok(())
}

/// Parse the `{z}_{x}_{y}.pbf` tile coordinates out of a file name.
pub fn parse_pbf_tile_name(name: &str) -> Option<(u32, u32, u32)> {
    let stem = name.strip_suffix(".pbf").or_else(|| name.strip_suffix(".mvt"))?;
    let parts: Vec<&str> = stem.split('_').collect();
    if parts.len() != 3 {
        return None;
    }
    Some((parts[0].parse().ok()?, parts[1].parse().ok()?, parts[2].parse().ok()?))
}

/// Rasterize an already-decoded vector-tile building set onto an ingest tile.
///
/// Shares the decoder and the write rule with the WASM pipeline, so the native
/// converter and the browser produce the same surface from the same tiles.
///
/// *set* is decoded once per run by [`load_pbf_building_dir`]: it is a whole
/// directory scan plus a PBF decode of every file in it, and its result is the
/// same for every output tile. This function is what runs per tile.
fn apply_buildings_pbf(
    job: &IngestJob,
    buffer: &mut [i16],
    set: &PbfBuildingSet,
    px_deg: f64,
) {
    let tiles_read = set.tiles_read;

    if set.buildings.is_empty() {
        println!("[Info] buildings_pbf_dir: {tiles_read} tile(s), no buildings in extent");
        return;
    }

    let tile = TileRef {
        ul_lat: job.ul_lat,
        ul_lon: job.ul_lon,
        scale_x: px_deg,
        scale_y: px_deg,
        size_px: job.size_px,
    };
    let mut grid = I16Grid { buf: buffer, size: job.size_px };
    let stats = rasterize_buildings(&mut grid, &tile, &set.buildings, &RasterOpts::default());

    println!(
        "[Info] buildings_pbf_dir: {tiles_read} tile(s), {} building(s) \
         ({} after dedup), {} drawn, {} pixel(s) raised",
        set.decoded, set.buildings.len(), stats.buildings_hit, stats.pixels_modified
    );
}

/// Collect building rings from a FlatGeobuf geometry into the shared model.
///
/// The roof comes off the geometry Z when there is one — an **absolute**
/// elevation, which is what this path has always used and still the top rung
/// of [`resolve_building_height`]. A 2D FlatGeobuf has no Z at all, and used to
/// return here on its first line: every feature was dropped, the building list
/// came back empty and the burn was a silent no-op. Such a geometry now falls
/// back to *fallback* — the height the caller resolved from this feature's
/// attribute columns, always **above ground** — so a 2D file with `height` or
/// `levels` columns draws the buildings it has always described.
///
/// *fallback* is resolved once per feature, not per ring: a multi-polygon's
/// parts are one building's outline and share its attributes. Each part still
/// takes its own Z, exactly as before.
fn collect_fgb_buildings(
    geo: &flatgeobuf::Geometry,
    fallback: (BuildingHeight, HeightSource),
    out: &mut Vec<Building>,
) {
    if let Some(parts) = geo.parts() {
        if parts.len() > 0 {
            for i in 0..parts.len() {
                collect_fgb_buildings(&parts.get(i), fallback, out);
            }
            return;
        }
    }

    let xy = match geo.xy() { Some(v) => v, None => return };

    let (height, source) = match geo.z() {
        Some(z_vals) => {
            let mut max_z: f64 = -1000.0;
            for z in z_vals {
                if z > max_z { max_z = z; }
            }
            // Preserved from the original write loop: a roof that lands below
            // zero in half-metre units is dropped rather than drawn. This is a
            // property of an *absolute* roof only — an above-ground height is
            // screened by the rasterizer instead, against real terrain.
            if (max_z * 2.0).trunc() < 0.0 { return; }
            (BuildingHeight::Absolute(max_z), HeightSource::AbsoluteZ)
        }
        None => fallback,
    };

    let mut push_ring = |stop_idx: usize, start_idx: usize, out: &mut Vec<Building>| {
        let count = (stop_idx - start_idx) / 2;
        if count < 3 { return; }
        let mut coords: Vec<(f64, f64)> = Vec::with_capacity(count);
        let mut i = start_idx;
        while i < stop_idx {
            coords.push((xy.get(i), xy.get(i + 1)));
            i += 2;
        }
        out.push(Building { coords, height, source });
    };

    // `ends` counts POINTS, not `xy` slots — the format's own reader shifts
    // each entry left by one to index `xy` (`geometry_reader::read_polygon`).
    // Taking them as `xy` offsets, as this did, read the first half of each
    // ring and closed the footprint through the middle of the building.
    if let Some(ends_vec) = geo.ends() {
        let mut start = 0;
        for end in ends_vec {
            let stop = (end as usize) * 2;
            push_ring(stop, start, out);
            start = stop;
        }
    } else {
        push_ring(xy.len(), 0, out);
    }
}

pub fn point_in_poly(x: f64, y: f64, poly: &[(f64, f64)]) -> bool {
    let mut inside = false;
    let mut j = poly.len() - 1;
    for i in 0..poly.len() {
        let (xi, yi) = poly[i];
        let (xj, yj) = poly[j];
        let intersect = ((yi > y) != (yj > y)) && (x < (xj - xi) * (y - yi) / (yj - yi) + xi);
        if intersect { inside = !inside; }
        j = i;
    }
    inside
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── DEM sample decoding ────────────────────────────────────────────────

    #[test]
    fn the_casts_that_used_to_decode_dem_samples_map_nodata_to_zero() {
        // Not a test of our code — a test of the language rule the bug rested
        // on, so the reason for the two helpers stays visible. Both of these
        // are 0, i.e. sea level, and both used to reach the .abt that way.
        assert_eq!(f32::NAN as i16, 0, "NaN is GDAL's default Float32 nodata");
        assert_eq!(i32::MIN as i16, 0, "an int-to-int cast keeps the low 16 bits");
        assert_eq!(65536i32 as i16, 0);
    }

    #[test]
    fn float32_nodata_becomes_a_void_not_sea_level() {
        assert_eq!(f32_sample_to_half_metres(f32::NAN), VOID_ELEV);
        assert_eq!(f32_sample_to_half_metres(f32::INFINITY), VOID_ELEV);
        assert_eq!(f32_sample_to_half_metres(f32::NEG_INFINITY), VOID_ELEV);
    }

    #[test]
    fn float32_elevations_decode_exactly_as_before() {
        // Half-metre units, truncating toward zero, saturating at the bounds —
        // unchanged for every finite sample, so real terrain keeps its bytes.
        assert_eq!(f32_sample_to_half_metres(0.0), 0);
        assert_eq!(f32_sample_to_half_metres(100.0), 200);
        assert_eq!(f32_sample_to_half_metres(-430.5), -861); // Dead Sea shore
        assert_eq!(f32_sample_to_half_metres(8848.9), 17697); // 17697.8 truncates
        assert_eq!(f32_sample_to_half_metres(-0.4), 0);
        assert_eq!(f32_sample_to_half_metres(1.0e9), i16::MAX);
        assert_eq!(f32_sample_to_half_metres(-1.0e9), i16::MIN);
    }

    #[test]
    fn int32_nodata_becomes_a_void_not_sea_level() {
        // i32::MIN is the classic Int32 nodata. Its low 16 bits are zero, so
        // the old `(x as i16)` narrowing produced 0 m.
        assert_eq!(i32_sample_to_half_metres(i32::MIN), VOID_ELEV);
        assert_eq!(i32_sample_to_half_metres(-32768), VOID_ELEV);
        assert_eq!(i32_sample_to_half_metres(-9999), VOID_ELEV);
        // 65536 m is not an elevation either, but the truncating cast made it 0.
        assert_ne!(i32_sample_to_half_metres(65536), 0);
    }

    #[test]
    fn int32_elevations_saturate_instead_of_truncating() {
        assert_eq!(i32_sample_to_half_metres(0), 0);
        assert_eq!(i32_sample_to_half_metres(100), 200);
        assert_eq!(i32_sample_to_half_metres(-430), -860);
        assert_eq!(i32_sample_to_half_metres(8849), 17698);
        assert_eq!(i32_sample_to_half_metres(i32::MAX), i16::MAX);
        // The whole i16 range is reachable and nothing wraps.
        for m in [-4000i32, -2000, -1, 1, 16000, 16383] {
            assert_eq!(i32_sample_to_half_metres(m), (m * 2) as i16, "m = {m}");
        }
    }

    #[test]
    fn a_void_is_rejected_by_the_validity_test_that_a_zero_would_pass() {
        // This is why the fix matters: the samplers in this file accept any
        // value above -5000 *half-metres* (-2500 m) as real ground.
        assert!(VOID_ELEV <= -5000, "the sentinel must read as no-data");
        assert!(0 > -5000, "sea level reads as valid ground — as it should");
    }

    // ── Area-averaged resampling ───────────────────────────────────────────

    /// A source image in the base DEM's frame: degrees, origin at the tile's
    /// upper-left, `scale` degrees per pixel.
    fn img(w: u32, h: u32, origin_e: f64, origin_n: f64, scale: f64, data: Vec<i16>) -> LoadedImage {
        assert_eq!(data.len(), (w * h) as usize);
        LoadedImage {
            width: w,
            height: h,
            data,
            origin_e,
            origin_n,
            limit_e: origin_e + w as f64 * scale,
            limit_n: origin_n - h as f64 * scale,
            scale_x: scale,
            scale_y: scale,
            name: "test".into(),
            embedded_crs: None,
            is_abt: false,
        }
    }

    /// Convert one tile from an in-memory base DEM and read the payload back.
    fn convert_with_base(mut job: IngestJob, base: LoadedImage) -> Vec<Vec<i16>> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("base.tif");
        job.output_path = dir.path().join("tile.abt");
        // Exercises the deprecated base_tif shim: the shim maps it to a
        // trailing EPSG:4326 source, whose geographic sampling path must stay
        // byte-identical to the historical base path these tests pin.
        job.base_tif = Some(path.clone());
        let mut cache = HashMap::new();
        cache.insert((path, None), Arc::new(base));
        let out = job.output_path.clone();
        let size = job.size_px as usize;
        process_tile(job, Arc::new(std::sync::Mutex::new(cache)), None).unwrap();

        let bytes = fs::read(&out).unwrap();
        let stride = ((size * 2) + 255) & !255;
        (0..size)
            .map(|y| {
                (0..size)
                    .map(|x| {
                        let o = 44 + y * stride + x * 2;
                        i16::from_le_bytes([bytes[o], bytes[o + 1]])
                    })
                    .collect()
            })
            .collect()
    }

    fn ramp(w: u32, h: u32) -> Vec<i16> {
        (0..w * h).map(|i| (i % w) as i16 * 10 + (i / w) as i16).collect()
    }

    #[test]
    fn an_output_cell_is_the_mean_of_the_source_block_under_it() {
        // Base three times finer than the target: every output pixel owns the
        // clean 3x3 block that IS its ground footprint — cell (x,y) covers
        // source pixels [3x, 3x+3) x [3y, 3y+3), centred on (3x+1, 3y+1).
        let mut job = empty_job(PathBuf::new());
        job.size_px = 4;
        let pixel_deg = job.resolution_m / 111111.0;
        let src = img(12, 12, job.ul_lon, job.ul_lat, pixel_deg / 3.0, ramp(12, 12));

        let block_mean = |cx: usize, cy: usize| -> i16 {
            let mut s = 0i32;
            for y in cy - 1..=cy + 1 {
                for x in cx - 1..=cx + 1 {
                    s += src.data[y * 12 + x] as i32;
                }
            }
            ((s * 2 + 9) / 18) as i16 // round half away from zero, 9 samples
        };
        let expect: Vec<(usize, usize, i16)> = (0..4)
            .flat_map(|y| (0..4).map(move |x| (x, y, 0)))
            .map(|(x, y, _)| (x, y, block_mean(3 * x + 1, 3 * y + 1)))
            .collect();

        let out = convert_with_base(job, src);
        for (x, y, want) in expect {
            assert_eq!(out[y][x], want, "pixel ({x},{y})");
        }
    }

    #[test]
    fn voids_are_left_out_of_the_average_instead_of_dragging_the_cell_down() {
        // The DEM-edge / coastline case: averaging the sentinel in would pull a
        // 1000 m cell to -1200 m and bury the coast.
        let src = &[
            2000, 2000, 2000, //
            2000, VOID_ELEV, 2000, //
            2000, 2000, VOID_ELEV,
        ];
        // 7 valid samples of 2000, 2 voids -> exactly 2000, not 2000*7/9.
        assert_eq!(mean_valid(src, 3, 0, 3, 0, 3), Some(2000));

        // A mixed block averages only the real ground in it.
        let src = &[10i16, 20, VOID_ELEV, 40];
        assert_eq!(mean_valid(src, 4, 0, 4, 0, 1), Some(23)); // (10+20+40)/3 = 23.33
    }

    #[test]
    fn a_cell_with_nothing_but_voids_under_it_stays_void() {
        let src = &[VOID_ELEV; 9];
        assert_eq!(mean_valid(src, 3, 0, 3, 0, 3), None);

        // End to end: a base DEM of pure voids must not become sea level.
        let mut job = empty_job(PathBuf::new());
        job.size_px = 4;
        let pixel_deg = job.resolution_m / 111111.0;
        let src = img(12, 12, job.ul_lon, job.ul_lat, pixel_deg / 3.0, vec![VOID_ELEV; 144]);
        let out = convert_with_base(job, src);
        for row in &out {
            for &v in row {
                assert_eq!(v, VOID_ELEV);
            }
        }
    }

    #[test]
    fn a_source_coarser_than_the_target_still_gives_every_cell_a_value() {
        // Ratio < 1: the cell holds no source-pixel centre at all, so there is
        // nothing to average. It must not divide by zero or fall through as a
        // void — it takes the pixel it sits in, exactly as before.
        let mut job = empty_job(PathBuf::new());
        job.size_px = 8;
        let pixel_deg = job.resolution_m / 111111.0;
        // One source pixel per four output pixels.
        let scale = pixel_deg * 4.0;
        let src = img(4, 4, job.ul_lon, job.ul_lat, scale, ramp(4, 4));
        // The fallback takes the source pixel the cell's CENTRE sits in,
        // spelled with the same truncation the code uses, so the comparison
        // holds on the cells where the arithmetic lands a hair either side of
        // a source-pixel boundary.
        let expect: Vec<Vec<i16>> = (0..8)
            .map(|y| {
                let py = (((y as f64 + 0.5) * pixel_deg) / scale) as usize;
                (0..8)
                    .map(|x| {
                        let px = (((x as f64 + 0.5) * pixel_deg) / scale) as usize;
                        src.data[py * 4 + px]
                    })
                    .collect()
            })
            .collect();

        let out = convert_with_base(job, src);
        assert_eq!(out, expect);
    }

    #[test]
    fn the_span_never_empties_and_never_divides_by_zero_at_any_ratio() {
        for &ratio in &[0.01f64, 0.5, 0.999, 1.0, 1.001, 3.26, 60.0] {
            for step in 0..7 {
                let centre = 3.0 + step as f64 * 0.137;
                let (a, b) = sample_span(centre - ratio / 2.0, centre + ratio / 2.0, 16);
                assert!(a < b, "ratio {ratio} centre {centre} gave an empty span");
                assert!(b <= 16);
            }
        }
        // A cell entirely off the low edge still resolves to a real pixel.
        let (a, b) = sample_span(-8.0, -7.0, 16);
        assert!(a < b && b <= 16);
    }

    #[test]
    fn a_non_integer_ratio_uses_every_source_pixel_exactly_once() {
        // 3.26 source pixels per output cell — the ratio that exposed the bug.
        // Nearest-neighbour keeps pixel `int(3.26 * x)` and drops the other
        // 2.26, so a feature in a dropped pixel is invisible and which pixels
        // survive follows a 3,3,3,4 beat. The spans must instead tile the
        // source: contiguous, no gaps, no pixel counted twice.
        const RATIO: f64 = 3.26;
        let n = 326u32;
        let mut next = None;
        // From 1: cell 0's footprint runs off the raster's edge and is clipped.
        for x in 1..99u32 {
            let centre = RATIO * x as f64;
            let (a, b) = sample_span(centre - RATIO / 2.0, centre + RATIO / 2.0, n);
            assert!(b - a >= 3 && b - a <= 4, "x={x} took {} pixels", b - a);
            if let Some(prev_end) = next {
                assert_eq!(a, prev_end, "gap or overlap before output cell {x}");
            }
            next = Some(b);
        }
    }

    #[test]
    fn a_feature_the_old_sampler_skipped_now_reaches_the_output() {
        // The same 3.26 ratio, end to end. A spike sits on a source pixel the
        // truncating index never reads; with area-averaging it lifts its cell.
        let mut job = empty_job(PathBuf::new());
        job.size_px = 8;
        let pixel_deg = job.resolution_m / 111111.0;
        let w = 40u32;
        let flat = vec![1000i16; (w * w) as usize];

        let out_flat = convert_with_base(job.clone(), img(w, w, job.ul_lon, job.ul_lat, pixel_deg / 3.26, flat.clone()));
        assert_eq!(out_flat[4][4], 1000);

        // Pixel (4*3.26 = 13.04 -> the old code read column 13); put the spike
        // on column 14, which it never reads, and keep row 13 so only the
        // column moves.
        let mut spiked = flat.clone();
        spiked[13 * w as usize + 14] = 1000 + 900; // +450 m mast
        let old_sample = spiked[13 * w as usize + 13];
        assert_eq!(old_sample, 1000, "the old point sample is blind to the spike");

        let out = convert_with_base(job.clone(), img(w, w, job.ul_lon, job.ul_lat, pixel_deg / 3.26, spiked));
        assert!(
            out[4][4] > out_flat[4][4],
            "the spike must raise the cell it stands in: {} vs {}",
            out[4][4], out_flat[4][4]
        );
    }

    // ── .abt writer ────────────────────────────────────────────────────────

    fn empty_job(out: PathBuf) -> IngestJob {
        IngestJob {
            output_path: out,
            format: None,
            ul_lat: 47.5,
            ul_lon: 8.25,
            resolution_m: 10.0,
            size_px: 8,
            sources: Vec::new(),
            void_fill_m: None,
            base_tif: None,
            swiss_tifs: Vec::new(),
            buildings_file: None,
            buildings_pbf_dir: None,
        }
    }

    #[test]
    fn the_abt_writer_emits_the_44_byte_header_and_padded_rows() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("tile.abt");
        let job = empty_job(out.clone());
        let pixel_deg = job.resolution_m / 111111.0;
        let size = job.size_px as usize;

        process_tile_with_cache(job, Arc::new(std::sync::Mutex::new(HashMap::new()))).unwrap();

        let bytes = fs::read(&out).unwrap();
        let stride = ((size * 2) + 255) & !255;
        assert_eq!(bytes.len(), 44 + stride * size);

        let u16at = |o: usize| u16::from_le_bytes([bytes[o], bytes[o + 1]]);
        let f64at = |o: usize| f64::from_le_bytes(bytes[o..o + 8].try_into().unwrap());
        assert_eq!(&bytes[0..4], b"AETH");
        assert_eq!(u16at(4), 1, "version 1 = R16SINT");
        assert_eq!(u16at(6), 8, "width");
        assert_eq!(f64at(8), 47.5);
        assert_eq!(f64at(16), 8.25);
        assert_eq!(f64at(24), pixel_deg);
        assert_eq!(f64at(32), pixel_deg);
        assert_eq!(i16::from_le_bytes([bytes[40], bytes[41]]), 0, "base_elev");
        assert_eq!(u16at(42) as usize, stride);
    }

    #[test]
    fn a_tile_with_no_terrain_source_is_all_void_not_all_sea_level() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("tile.abt");
        let job = empty_job(out.clone());
        let size = job.size_px as usize;

        process_tile_with_cache(job, Arc::new(std::sync::Mutex::new(HashMap::new()))).unwrap();

        let bytes = fs::read(&out).unwrap();
        let stride = ((size * 2) + 255) & !255;
        for y in 0..size {
            for x in 0..size {
                let o = 44 + y * stride + x * 2;
                assert_eq!(
                    i16::from_le_bytes([bytes[o], bytes[o + 1]]),
                    VOID_ELEV,
                    "pixel ({x},{y})"
                );
            }
            // Row padding is zero-filled, per the 256-byte alignment rule.
            assert!(bytes[44 + y * stride + size * 2..44 + (y + 1) * stride].iter().all(|&b| b == 0));
        }
    }

    #[test]
    fn a_job_with_a_pbf_dir_but_no_decoded_set_is_refused() {
        // Guards the E1 hoist: a batch caller that forgets to pass the set gets
        // an error, not silently building-less tiles.
        let dir = tempfile::tempdir().unwrap();
        let mut job = empty_job(dir.path().join("tile.abt"));
        job.buildings_pbf_dir = Some(dir.path().to_path_buf());

        let err = process_tile(job, Arc::new(std::sync::Mutex::new(HashMap::new())), None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("no decoded building set"), "got {err:?}");
    }

    // ── Chunk cache ────────────────────────────────────────────────────────

    #[test]
    fn the_chunk_cache_evicts_least_recently_used_and_never_exceeds_its_budget() {
        let chunk = |v: i16| Arc::new(vec![v; 512]); // 1 KiB each
        let cache = ChunkCache::new(2560); // room for two
        cache.insert_many(vec![((1, 0), chunk(1)), ((1, 1), chunk(2))]);
        // Touch chunk 0, so chunk 1 is now the least recently used.
        assert!(cache.get_many(&[(1, 0)])[0].is_some());
        cache.insert_many(vec![((1, 2), chunk(3))]);
        let got = cache.get_many(&[(1, 0), (1, 1), (1, 2)]);
        assert!(got[0].is_some(), "the recently used chunk must survive");
        assert!(got[1].is_none(), "the least recently used chunk must be evicted");
        assert!(got[2].is_some());
        assert_eq!(cache.bytes(), 2048);
        assert!(cache.peak_bytes() <= cache.budget());
        // A chunk over half the budget is not admitted at all.
        assert!(!cache.admits(1281) && cache.admits(1280));
    }

    #[test]
    fn gdal_metadata_scale_offset_parsing() {
        let p = Path::new("x.tif");
        let md = r#"<GDALMetadata>
  <Item name="DESCRIPTION" sample="0" role="description">h</Item>
  <Item name="SCALE" sample="0" role="scale">0.1</Item>
  <Item name='OFFSET' sample='0' role='offset'> -10 </Item>
  <Item name="SCALE" sample="1" role="scale">7</Item>
  <Item name="AREA_OR_POINT">Area</Item>
</GDALMetadata>"#;
        assert_eq!(gdal_scale_offset(md, p).unwrap(), Some((0.1, -10.0)));
        assert_eq!(gdal_scale_offset("<GDALMetadata/>", p).unwrap(), None);
        let id = r#"<GDALMetadata><Item name="SCALE" sample="0" role="scale">1</Item></GDALMetadata>"#;
        assert_eq!(gdal_scale_offset(id, p).unwrap(), None);
        let bad = r#"<GDALMetadata><Item name="OFFSET" sample="0" role="offset">nan</Item></GDALMetadata>"#;
        assert!(gdal_scale_offset(bad, p).is_err());
    }

    // ── Point-in-polygon ───────────────────────────────────────────────────

    #[test]
    fn point_in_poly_classifies_inside_outside_and_the_edges() {
        let sq = [(0.0, 0.0), (4.0, 0.0), (4.0, 4.0), (0.0, 4.0)];
        assert!(point_in_poly(2.0, 2.0, &sq));
        assert!(!point_in_poly(5.0, 2.0, &sq));
        assert!(!point_in_poly(-1.0, 2.0, &sq));
        assert!(!point_in_poly(2.0, -1.0, &sq));
        assert!(!point_in_poly(2.0, 5.0, &sq));
        // Half-open on purpose: the low edge is in, the high edge is out, so
        // pixel centres on a shared boundary belong to exactly one polygon.
        assert!(point_in_poly(0.0, 2.0, &sq));
        assert!(!point_in_poly(4.0, 2.0, &sq));
    }

    #[test]
    fn point_in_poly_handles_a_concave_ring() {
        // A "U": the notch between the arms must read as outside.
        let u = [
            (0.0, 0.0),
            (6.0, 0.0),
            (6.0, 6.0),
            (4.0, 6.0),
            (4.0, 2.0),
            (2.0, 2.0),
            (2.0, 6.0),
            (0.0, 6.0),
        ];
        assert!(point_in_poly(1.0, 4.0, &u));
        assert!(point_in_poly(5.0, 4.0, &u));
        assert!(!point_in_poly(3.0, 4.0, &u), "the notch is outside");
        assert!(point_in_poly(3.0, 1.0, &u), "the base is inside");
    }

    // ── Streaming reads ────────────────────────────────────────────────────

    #[test]
    fn a_truncated_abt_strip_fails_naming_its_first_incomplete_row() {
        // 300 x 300 rows of 600 bytes in a 768-byte stride, cut 100 bytes
        // into row 280 (the second 256-row strip).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cut.abt");
        let (size, stride) = (300usize, 768usize);
        let mut buf: Vec<u8> = b"AETH".to_vec();
        buf.write_u16::<LittleEndian>(1).unwrap();
        buf.write_u16::<LittleEndian>(size as u16).unwrap();
        for v in [46.0f64, 7.0, 1e-4, 1e-4] {
            buf.write_f64::<LittleEndian>(v).unwrap();
        }
        buf.write_i16::<LittleEndian>(0).unwrap();
        buf.write_u16::<LittleEndian>(stride as u16).unwrap();
        for r in 0..size {
            let mut row = vec![0u8; stride];
            for c in 0..size {
                row[2 * c..2 * c + 2].copy_from_slice(&((r + c) as i16).to_le_bytes());
            }
            buf.extend_from_slice(&row);
        }
        buf.truncate(44 + 280 * stride + 100);
        std::fs::write(&path, &buf).unwrap();

        let src = Arc::new(read_file_source(&path, None).unwrap());
        let cache = Arc::new(ChunkCache::new(1 << 20));
        let hold: TileHold = std::sync::Mutex::new(HashMap::new());
        let reads = TileReads::new(1);
        let pool: WinPool = std::sync::Mutex::new(Vec::new());
        // Rows of the intact first strip load, and hold the stored values.
        let w = load_window(&src, 0, (5, 300, 10, 100), &cache, &hold, None, &reads, &pool).unwrap();
        assert_eq!(w.data()[0], 15);
        assert_eq!(w.data()[(99 - 10) * w.stride + (299 - 5)], 398);
        let err = load_window(&src, 0, (0, 300, 250, 290), &cache, &hold, None, &reads, &pool).err().unwrap();
        assert!(
            format!("{err:#}").contains("truncated .abt payload (row 280 of 300 is incomplete)"),
            "got {err:#}"
        );
    }

    #[test]
    fn a_projected_source_gets_its_wgs84_box() {
        // LV95 (2600000, 1200000) is the Bern origin, 46.95108 N 7.43864 E;
        // a 1 km tile east/south of it spans ~0.0131 deg of longitude and
        // ~0.0090 deg of latitude.
        let proj = Proj::from_proj_string(&crs_to_proj_string("EPSG:2056", Path::new("t")).unwrap()).unwrap();
        let g = SourceGeom {
            width: 500,
            height: 500,
            origin_e: 2_600_000.0,
            origin_n: 1_200_000.0,
            limit_e: 2_601_000.0,
            limit_n: 1_199_000.0,
            scale_x: 2.0,
            scale_y: 2.0,
            embedded_crs: None,
            is_abt: false,
        };
        let [w, s, e, n] = wgs84_bbox_of_projected(&g, &proj, wgs84()).unwrap();
        assert!((w - 7.43864).abs() < 1e-3 && (n - 46.95108).abs() < 1e-3, "NW corner {w} {n}");
        assert!((e - w - 0.0131).abs() < 5e-4 && (n - s - 0.0090).abs() < 5e-4, "span {} x {}", e - w, n - s);
    }

    /// Every float of a row's geometry, as bits.
    fn geom_bits(g: &Option<RowGeom>) -> Option<Vec<u64>> {
        let g = g.as_ref()?;
        let mut v = vec![g.shift as u64, g.mask as u64, g.min_e.to_bits(), g.max_e.to_bits(), g.min_n.to_bits(), g.max_n.to_bits()];
        for s in &g.segs {
            v.extend([s.e0, s.n0, s.nlo0, s.step_e, s.step_n, s.step_nlo].map(f64::to_bits));
        }
        Some(v)
    }

    #[test]
    fn shared_row_geometry_is_identical_to_the_per_row_one() {
        // LV95 (chunked rows) at 2 m and 10 m, UTM 32N, and a tiny tile
        // whose rows take the single-segment path; runs of shared rows
        // against make_row_geom row by row, bit for bit.
        let cases = [
            ("EPSG:2056", 47.0, 7.5, 2.0, 5556usize, 2.0),
            ("EPSG:2056", 46.9, 7.8, 10.0, 2780, 2.0),
            ("EPSG:32632", 47.25, 8.25, 10.0, 2780, 1.0),
            ("EPSG:2056", 47.0, 7.5, 2.0, 24, 2.0),
        ];
        for (crs, ul_lat, ul_lon, res, size, min_scale) in cases {
            let dst = Proj::from_proj_string(&crs_to_proj_string(crs, Path::new("t")).unwrap()).unwrap();
            let pd = res * (1.0 / 111111.0);
            let (lat_c, lon_c) = (ul_lat - 0.5 * pd, ul_lon + 0.5 * pd);
            let row_lat_of = |y: usize| lat_c - (y as f64 * pd);
            let rows = 300.min(size);
            let shared = band_row_geoms(wgs84(), &dst, min_scale, (3, 3 + rows), &row_lat_of, lon_c, pd, size);
            let mut reused = 0;
            for (k, got) in shared.iter().enumerate() {
                let y = 3 + k;
                let want = make_row_geom(wgs84(), &dst, row_lat_of(y), row_lat_of(y) - pd, lon_c, pd, size, min_scale);
                assert!(want.is_some(), "{crs} row {y} must transform");
                assert_eq!(geom_bits(got), geom_bits(&want), "{crs} {res} m, {size} px: row {y}");
                if k % ROW_RUN != 0 && (row_lat_of(y - 1) - pd).to_bits() == row_lat_of(y).to_bits() {
                    reused += 1;
                }
            }
            assert!(reused > rows / 3, "{crs}: the runs must actually share ({reused} of {rows})");
        }
    }

    #[test]
    fn only_a_candidate_inside_a_higher_priority_box_is_shadowed() {
        let band = [7.0, 46.0, 8.0, 47.0];
        let r = Some((0u32, 1u32, 0u32, 1u32));
        // Side by side (a mosaic): nothing is shadowed.
        let mosaic = [Some([7.0, 46.0, 7.5, 47.0]), Some([7.5, 46.0, 8.0, 47.0]), Some([7.2, 46.2, 7.8, 46.8])];
        assert_eq!(shadowed_sources(&[r, r, r], &mosaic, band), [false, false, false]);
        // A big box first hides a small one, clipped to the band (the small
        // one reaches past the band's south edge); not the other way round.
        let stack = [Some([7.05, 45.95, 7.9, 47.1]), Some([7.1, 45.5, 7.3, 46.5]), Some([6.0, 45.0, 9.0, 48.0])];
        assert_eq!(shadowed_sources(&[r, r, r], &stack, band), [false, true, false]);
        // No window this band, or no box: never shadowed, shadows nothing.
        assert_eq!(shadowed_sources(&[None, r], &stack[..2], band), [false, false]);
        assert_eq!(shadowed_sources(&[r, r], &[None, stack[1]], band), [false, false]);
        // A box that misses the band entirely is deferred.
        assert_eq!(shadowed_sources(&[r], &[Some([9.0, 46.0, 9.5, 47.0])], band), [true]);
    }
}
