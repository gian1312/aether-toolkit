//! `buildings_pbf_list` (CONTRACT §9a/§9b, changelog 51): a job names, in a
//! list file, the building tiles it needs out of a store directory far
//! larger than that — the Waveshed plugin's `<cache>/pool/<md5>/` — and the
//! converter reads exactly those files from `buildings_pbf_dir`, never
//! listing the directory. The list is also the ready signal: missing when
//! the converter looks, it is waited for exactly as `buildings_pbf_ready`'s
//! marker is.
//!
//! The acceptance test, for a batch `ingest` and a CLI `download`: a job
//! with a list burns byte for byte what the same job burns from a
//! directory holding exactly the listed tiles (the old way) — and not what
//! the store's other tiles would add: the store holds a needed tile the list
//! leaves out (which changes the bytes when burnt), an undecodable tile (a
//! `[Warn]` when read) and a tile of another zoom (which fails any decode of
//! the whole store). Then: a list written after launch (the wait lines, the
//! same bytes), a list that never comes and a listed tile not in the store
//! (an error, nothing written), an empty list (an empty directory's bytes
//! and lines, never an error), bad fields and a missing store (refused
//! before anything runs), the single-job form (refused), and a repair job,
//! which honours the list exactly as a first pass does: the same selection
//! (the bytes of a repair over a directory of exactly the listed tiles), the
//! same wait, the same failures. Every path a line names — the list in the wait line,
//! the store in the decode's start line — is the job's spelling, verbatim:
//! the store is spelt `<dir>/cache//./pool\0a1b2c`, which neither a
//! canonical form nor Rust's `Debug` form would print.
//!
//! Hermetic: terrain from an `.abt` source (ingest) or a loopback XYZ
//! server (download), building tiles written by the test.

mod common;

use std::collections::BTreeSet;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use aether_converter::download::{lat2ty, lon2tx};
use common::{mvt_building_tile, read_dir_bytes, serve_tiles, terrarium_png, tile_ground, write_abt, TileCounts};
use serde_json::json;

/// Zoom of the building tiles (OpenFreeMap's).
const BZ: u32 = 14;
const WAIT_LINE: &str = "[Buildings] waiting for the building tiles (";
const READY_LINE: &str = "[Buildings] building tiles ready after ";
const DIR_DECODE: &str = "[Rust] Decoding building tiles in ";
const DECODED: &str = "[Rust] Building tiles decoded: ";
/// A run that does not end by then is killed and reported, never hung.
const LIMIT: Duration = Duration::from_secs(180);

// ── Building tiles and the store ────────────────────────────────────────────

/// One z14 building tile at `(x, y)`: a 4 × 4 block of houses of varying
/// size and height spread over the tile.
fn building_tile(x: u32, y: u32) -> Vec<u8> {
    let mut features = Vec::new();
    for j in 0..4i32 {
        for i in 0..4i32 {
            let (bx, by) = (300 + i * 950, 250 + j * 950);
            let h = 8.0 + ((x * 7 + y * 5 + (i * 4 + j) as u32 * 3) % 33) as f64;
            let (ex, ey) = (bx + 300 + i * 37, by + 260 + j * 41);
            features.push((h, vec![(bx, by), (ex, by), (ex, ey), (bx, ey)]));
        }
    }
    mvt_building_tile(&features)
}

fn tile_name((x, y): (u32, u32)) -> String {
    format!("{BZ}_{x}_{y}.pbf")
}

/// The z14 tiles under the box `(north, west, south, east)`.
fn tiles_under(n: f64, w: f64, s: f64, e: f64) -> Vec<(u32, u32)> {
    let mut v = Vec::new();
    for y in lat2ty(n, BZ)..=lat2ty(s, BZ) {
        for x in lon2tx(w, BZ)..=lon2tx(e, BZ) {
            v.push((x, y));
        }
    }
    v
}

/// Writes *tiles* into *dir* (created).
fn write_tiles(dir: &Path, tiles: &[(u32, u32)]) {
    std::fs::create_dir_all(dir).unwrap();
    for &(x, y) in tiles {
        std::fs::write(dir.join(tile_name((x, y))), building_tile(x, y)).unwrap();
    }
}

/// The plugin's store as a job names it: `<dir>/cache/pool\0a1b2c` — on
/// Linux one directory whose name holds a backslash, on Windows two — spelt
/// `<dir>/cache//./pool\0a1b2c`. Every line naming it must give that
/// spelling exactly as the job gives it: no canonical form, and no `Debug`
/// form either, which would double the backslash.
fn store_paths(dir: &Path) -> (PathBuf, String) {
    let real = dir.join("cache").join("pool\\0a1b2c");
    let spelt = format!("{}//./pool\\0a1b2c", dir.join("cache").display());
    (real, spelt)
}

/// The building tiles of a job over the box of *needed* tiles, of which the
/// list leaves out *left_out* (a tile under the outputs, so that burning it
/// changes their bytes):
/// * the store ([`store_paths`]) — every needed tile, a z14 tile far away,
///   an undecodable z14 tile (a `[Warn]` if read), a z15 tile (a
///   whole-store decode fails: mixed zooms), a README and, on unix, a FIFO
///   named like a z14 tile ([`unlisted_fifo`]: a run that opened it would
///   block until killed);
/// * `only/` — exactly the listed tiles (the old way);
/// * `all/` — every needed tile.
///
/// Returns `(store as the jobs spell it, only, all, listed)`.
fn building_dirs(dir: &Path, needed: &[(u32, u32)], left_out: (u32, u32)) -> (String, PathBuf, PathBuf, Vec<(u32, u32)>) {
    assert!(needed.len() >= 4 && needed.contains(&left_out), "{needed:?}");
    let listed: Vec<(u32, u32)> = needed.iter().copied().filter(|&t| t != left_out).collect();
    let ((store, spelt), only, all) = (store_paths(dir), dir.join("only"), dir.join("all"));
    write_tiles(&store, needed);
    write_tiles(&store, &[(9000, 6000)]);
    std::fs::write(store.join("14_9001_6000.pbf"), b"\x1a\xff\xff\xff\x0fnot a vector tile").unwrap();
    std::fs::write(store.join("15_18000_12000.pbf"), building_tile(18000, 12000)).unwrap();
    std::fs::write(store.join("README.txt"), b"the plugin's tile store").unwrap();
    unlisted_fifo(&store.join("14_9002_6000.pbf"));
    write_tiles(&only, &listed);
    write_tiles(&all, needed);
    (spelt, only, all, listed)
}

/// A named pipe at *path*: opening it for reading blocks until a writer
/// comes — none ever does — so a run that opens an unlisted file of the
/// store hangs until [`LIMIT`] kills it and the test fails. Proof that the
/// listed files, and only they, are opened (unix; elsewhere nothing).
fn unlisted_fifo(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let c = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        // SAFETY: a NUL-terminated path; mkfifo reads it and nothing else.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o644) }, 0, "mkfifo {path:?}");
    }
    #[cfg(not(unix))]
    let _ = path;
}

/// The list text for *tiles*: CRLF lines, a comment, a blank line, and the
/// first tile listed twice — as a caller may write it.
fn list_text(tiles: &[(u32, u32)]) -> String {
    let mut t = String::from("# building tiles of one converter batch\r\n\r\n");
    for &tile in tiles.iter().rev() {
        t += &format!("{}\r\n", tile_name(tile));
    }
    t + &format!("{}\r\n", tile_name(tiles[0]))
}

/// A list path in *dir* spelt as no canonical form is: `<dir>/<sub>//./<name>`
/// — the lines that name the list must give it exactly so. Returns the path
/// and its spelling.
fn odd_list_path(dir: &Path, sub: &str, name: &str) -> (PathBuf, String) {
    let spelt = format!("{}//./{name}", dir.join(sub).display());
    (PathBuf::from(&spelt), spelt)
}

/// Writes the list *path* the way a caller must: complete under another
/// name, then renamed into place — the converter reads it once it exists.
fn publish_list(path: &Path, text: &str) {
    let tmp = path.with_extension("part");
    std::fs::write(&tmp, text).unwrap();
    std::fs::rename(&tmp, path).unwrap();
}

/// The files of *out* (none when it does not exist).
fn files_in(out: &Path) -> Vec<String> {
    match std::fs::read_dir(out) {
        Ok(rd) => rd.filter_map(|e| e.ok()).map(|e| e.file_name().to_string_lossy().into_owned()).collect(),
        Err(_) => Vec::new(),
    }
}

fn count(lines: &[String], needle: &str) -> usize {
    lines.iter().filter(|l| l.contains(needle)).count()
}

// ── Running the CLI ─────────────────────────────────────────────────────────

/// A running CLI whose lines (stdout for `ingest`, stderr for `download`)
/// are read as they are printed; the other stream is collected.
struct Running {
    child: Child,
    lines: mpsc::Receiver<(Instant, String)>,
    other: thread::JoinHandle<String>,
    t0: Instant,
    seen: Vec<String>,
}

fn launch(subcommand: &str, job: &Path) -> Running {
    let t0 = Instant::now();
    let mut child = Command::new(env!("CARGO_BIN_EXE_aether_converter"))
        .args([subcommand, "--job-file"])
        .arg(job)
        .env_remove("AETHER_DIAG_FILE")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let (out, err) = (child.stdout.take().unwrap(), child.stderr.take().unwrap());
    let (watched, mut other): (Box<dyn Read + Send>, Box<dyn Read + Send>) =
        if subcommand == "download" { (Box::new(err), Box::new(out)) } else { (Box::new(out), Box::new(err)) };
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        for line in BufReader::new(watched).lines().map_while(Result::ok) {
            if tx.send((Instant::now(), line)).is_err() {
                break;
            }
        }
    });
    let other = thread::spawn(move || {
        let mut s = String::new();
        let _ = other.read_to_string(&mut s);
        s
    });
    Running { child, lines: rx, other, t0, seen: Vec::new() }
}

impl Running {
    /// The moment a line containing *needle* was read (all lines kept), or
    /// `None` when the stream ends or [`LIMIT`] passes first.
    fn wait_for(&mut self, needle: &str) -> Option<Instant> {
        loop {
            let left = LIMIT.saturating_sub(self.t0.elapsed());
            let (at, line) = self.lines.recv_timeout(left).ok()?;
            let hit = line.contains(needle);
            self.seen.push(line);
            if hit {
                return Some(at);
            }
        }
    }

    /// Wait for the exit (kill after [`LIMIT`]): `(exit ok, every line of
    /// the watched stream, the other stream)`.
    fn finish(mut self) -> (bool, Vec<String>, String) {
        let ok = loop {
            if let Some(st) = self.child.try_wait().unwrap() {
                break st.success();
            }
            if self.t0.elapsed() > LIMIT {
                let _ = self.child.kill();
                let _ = self.child.wait();
                panic!("still running after {LIMIT:?} — killed. Lines:\n{}", self.seen.join("\n"));
            }
            while let Ok((_, l)) = self.lines.try_recv() {
                self.seen.push(l);
            }
            thread::sleep(Duration::from_millis(20));
        };
        // The reader ends at EOF, after the exit.
        self.seen.extend(self.lines.iter().map(|(_, l)| l));
        (ok, self.seen, self.other.join().unwrap())
    }
}

/// Run a batch `ingest`: `(exit ok, stdout lines, stderr)`.
fn ingest(job: &Path) -> (bool, Vec<String>, String) {
    launch("ingest", job).finish()
}

/// Run a CLI `download`: `(exit ok, stderr lines)`.
fn download(job: &Path) -> (bool, Vec<String>) {
    let (ok, lines, _) = launch("download", job).finish();
    (ok, lines)
}

// ── The ingest batch ────────────────────────────────────────────────────────

/// Output tiles: a COLS × ROWS grid of SIZE px at RES m.
const SIZE: usize = 128;
const RES: f64 = 10.0;
const PD: f64 = RES / 111_111.0;
const COLS: usize = 4;
const ROWS: usize = 3;
const TILES: usize = COLS * ROWS;
const UL: (f64, f64) = (47.01, 8.003); // (lat, lon)

/// The z14 tiles under the batch, and the one at its centre (left out of
/// the list).
fn ingest_tiles() -> (Vec<(u32, u32)>, (u32, u32)) {
    let (w, h) = ((COLS * SIZE) as f64 * PD, (ROWS * SIZE) as f64 * PD);
    let needed = tiles_under(UL.0, UL.1, UL.0 - h, UL.1 + w);
    let centre = (lon2tx(UL.1 + w / 2.0, BZ), lat2ty(UL.0 - h / 2.0, BZ));
    (needed, centre)
}

/// The terrain source under the whole grid: a sloped, rippled `.abt`.
fn write_terrain(dir: &Path) -> PathBuf {
    let n = 600usize;
    let data: Vec<i16> = (0..n * n)
        .map(|i| {
            let (x, y) = (i % n, i / n);
            (900 + x + 2 * y + (x * 7 + y * 3) % 23) as i16
        })
        .collect();
    let p = dir.join("terrain.abt");
    write_abt(&p, n as u16, UL.0 + 0.005, UL.1 - 0.005, 0.0001, &data);
    p
}

/// Writes the batch `<dir>/<name>.json` — outputs in `<dir>/<name>/` — with
/// *extra* merged into every job.
fn write_batch(dir: &Path, name: &str, extra: serde_json::Value) -> (PathBuf, PathBuf) {
    let out = dir.join(name);
    let src = dir.join("terrain.abt");
    let mut jobs = Vec::new();
    for r in 0..ROWS {
        for c in 0..COLS {
            let i = r * COLS + c;
            let mut j = json!({
                "output_path": out.join(format!("t{i:02}.abt")), "format": "r16sint",
                "ul_lat": UL.0 - (r * SIZE) as f64 * PD, "ul_lon": UL.1 + (c * SIZE) as f64 * PD,
                "resolution_m": RES, "size_px": SIZE, "sources": [{"path": src}],
            });
            for (k, v) in extra.as_object().unwrap() {
                j[k] = v.clone();
            }
            jobs.push(j);
        }
    }
    let path = dir.join(format!("{name}.json"));
    std::fs::write(&path, serde_json::to_vec_pretty(&jobs).unwrap()).unwrap();
    (path, out)
}

/// stdout as two runs of one batch must agree on it: without the timing
/// lines and numbers, the per-tile lines (completion order) as a sorted set
/// after the rest (in order).
fn normalized(lines: &[String]) -> Vec<String> {
    let kept = lines
        .iter()
        .filter(|l| !["[Rust] tile ", "[Rust] run: ", "[Rust] Rayon Pool: "].iter().any(|p| l.starts_with(p)))
        .map(|l| match l.strip_prefix(DECODED) {
            Some(r) => format!("{DECODED}{}", r.rsplit_once(" in ").expect("… in <s.s> s").0),
            None => l.clone(),
        });
    let (mut per_tile, rest): (Vec<String>, Vec<String>) =
        kept.partition(|l| l.starts_with("[Tile] ") || l.starts_with("[Info] buildings_pbf_dir:"));
    per_tile.sort();
    rest.into_iter().chain(per_tile).collect()
}

/// The list run's header line for *n* tiles from *store*, spelt as the job
/// spells it.
fn list_decode_line(n: usize, store: &str) -> String {
    format!("[Rust] Decoding {n} listed building tiles from \"{store}\" (once for the run)...")
}

/// The references, the old way: the batch over `only/` (exactly the listed
/// tiles) — its `.abt` files and normalized stdout — after checking that
/// the batch over `all/` (the left-out tile too) writes other bytes, so
/// that equal bytes below mean the left-out tile was not burnt.
struct IngestRef {
    store: String,
    listed: Vec<(u32, u32)>,
    bytes: Vec<(String, Vec<u8>)>,
    stdout: Vec<String>,
    only: PathBuf,
}

fn ingest_reference(dir: &Path) -> IngestRef {
    write_terrain(dir);
    let (needed, centre) = ingest_tiles();
    let (store, only, all, listed) = building_dirs(dir, &needed, centre);

    let (job, out) = write_batch(dir, "reference", json!({ "buildings_pbf_dir": only }));
    let (ok, lines, stderr) = ingest(&job);
    assert!(ok, "reference run failed:\n{}\n{stderr}", lines.join("\n"));
    assert!(stderr.is_empty(), "{stderr}");
    let bytes = read_dir_bytes(&out);
    assert_eq!(bytes.len(), TILES);

    let (job, out) = write_batch(dir, "reference_all", json!({ "buildings_pbf_dir": all }));
    let (ok, lines_all, _) = ingest(&job);
    assert!(ok, "{}", lines_all.join("\n"));
    let differ = read_dir_bytes(&out).iter().zip(&bytes).filter(|((_, a), (_, b))| a != b).count();
    assert!(differ >= 2, "the left-out tile must change the bytes when burnt ({differ} tiles differ)");

    let (job, out) = write_batch(dir, "terrain_only", json!({}));
    let (ok, _, _) = ingest(&job);
    assert!(ok);
    let differ = read_dir_bytes(&out).iter().zip(&bytes).filter(|((_, a), (_, b))| a != b).count();
    assert!(differ >= TILES / 2, "the listed tiles must raise pixels in most tiles ({differ} of {TILES})");

    IngestRef { store, listed, bytes, stdout: normalized(&lines), only }
}

impl IngestRef {
    /// *lines* of a list run, its header replaced by the reference's —
    /// after checking it names the store and the listed count, once.
    fn as_reference(&self, lines: &[String]) -> Vec<String> {
        let header = list_decode_line(self.listed.len(), &self.store);
        assert_eq!(count(lines, "listed building tiles from"), 1, "{}", lines.join("\n"));
        assert!(lines.contains(&header), "want {header:?}:\n{}", lines.join("\n"));
        let dir_header = format!("{DIR_DECODE}{:?} (once for the run)...", self.only);
        lines.iter().map(|l| if *l == header { dir_header.clone() } else { l.clone() }).collect()
    }
}

#[test]
fn ingest_a_list_burns_exactly_what_a_directory_of_its_tiles_burns_and_nothing_else_of_the_store() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let r = ingest_reference(dir);
    let list = dir.join("lists").join("batch_1.txt");
    std::fs::create_dir_all(list.parent().unwrap()).unwrap();
    publish_list(&list, &list_text(&r.listed));

    let (job, out) = write_batch(dir, "listed", json!({ "buildings_pbf_dir": r.store, "buildings_pbf_list": list }));
    let (ok, lines, stderr) = ingest(&job);
    let all = format!("{}\n{stderr}", lines.join("\n"));
    assert!(ok, "{all}");
    // THE acceptance: the bytes of the batch over a directory holding exactly
    // the listed tiles (and not those of `all/`, checked in the reference).
    assert!(read_dir_bytes(&out) == r.bytes, ".abt bytes must equal the old way's");
    // The store's undecodable tile was never read (no warning), nor its z15
    // tile (no mixed-zoom error); the list was there: no wait.
    assert!(stderr.is_empty(), "nothing of the store but the listed tiles is read:\n{stderr}");
    assert_eq!(count(&lines, "[Buildings]"), 0, "{all}");
    // Every line as the old way's, but the decode header; the counts are the
    // listed tiles'.
    let n = r.listed.len();
    assert!(lines.iter().any(|l| l.starts_with(&format!("{DECODED}{n} tile(s), "))), "{all}");
    assert_eq!(count(&lines, &format!("[Info] buildings_pbf_dir: {n} tile(s), ")), TILES, "{all}");
    assert_eq!(normalized(&r.as_reference(&lines)), r.stdout);
    // The decode comes before any tile, as for a directory.
    let at = |p: &str| lines.iter().position(|l| l.starts_with(p)).unwrap();
    assert!(at("[Rust] Decoding ") < at(DECODED) && at(DECODED) < at("[Tile] "), "{all}");
}

#[test]
fn ingest_a_list_written_after_launch_is_waited_for_and_burns_the_same_bytes() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let r = ingest_reference(dir);
    // The list's directory does not even exist at launch.
    let (list, spelt) = odd_list_path(dir, "lists late", "batch 2.txt");
    let (job, out) = write_batch(dir, "late", json!({
        "buildings_pbf_dir": r.store, "buildings_pbf_list": spelt, "buildings_pbf_wait_s": 120,
    }));
    let mut running = launch("ingest", &job);
    let t0 = running.t0;
    let announced = running.wait_for(WAIT_LINE);
    let announced = announced.unwrap_or_else(|| panic!("no wait line:\n{}", running.seen.join("\n")));
    // 2 s after launch the caller has its tiles and writes the list. Until
    // then no tile is written.
    thread::sleep(Duration::from_secs(2).saturating_sub(t0.elapsed()));
    assert!(files_in(&out).is_empty(), "written before the list: {:?}", files_in(&out));
    std::fs::create_dir_all(list.parent().unwrap()).unwrap();
    publish_list(&list, &list_text(&r.listed));
    let listed_at = Instant::now();
    let (ok, lines, stderr) = running.finish();
    let all = format!("{}\n{stderr}", lines.join("\n"));
    assert!(ok, "{all}");
    assert!(stderr.is_empty(), "{stderr}");
    assert!(read_dir_bytes(&out) == r.bytes, ".abt bytes must equal the old way's");

    // The marker's lines, naming the list exactly as the job does.
    assert_eq!(count(&lines, WAIT_LINE), 1, "{all}");
    assert!(lines.contains(&format!("{WAIT_LINE}{spelt})")), "want the list as given, {spelt:?}:\n{all}");
    let ready: Vec<&String> = lines.iter().filter(|l| l.starts_with(READY_LINE)).collect();
    assert_eq!(ready.len(), 1, "{all}");
    let secs: f64 = ready[0][READY_LINE.len()..].strip_suffix(" s").expect("<s.s> s").parse().unwrap();
    let gap = listed_at.duration_since(announced).as_secs_f64();
    assert!(secs + 0.06 >= gap && secs <= gap + 1.5, "waited {secs} s, list {gap:.2} s after the wait line");
    let held = lines.iter().position(|l| l.contains(" tile(s) held (")).unwrap_or_else(|| panic!("no held line:\n{all}"));
    // Wait, the decode of the listed tiles, ready, held — then the tiles.
    let at = |p: &str| lines.iter().position(|l| l.starts_with(p)).unwrap_or_else(|| panic!("no {p:?}:\n{all}"));
    assert!(at(WAIT_LINE) < at("[Rust] Decoding ") && at(DECODED) < at(READY_LINE) && at(READY_LINE) < held, "{all}");
    let first_tile = lines.iter().position(|l| l.starts_with("[Tile] ") || l.starts_with("[Rust] Progress:")).unwrap();
    assert!(held < first_tile, "{all}");
    // Otherwise the old way's lines.
    let rest: Vec<String> = lines.iter().filter(|l| !l.starts_with("[Buildings] ")).cloned().collect();
    let mut mine = normalized(&r.as_reference(&rest));
    let mut theirs = r.stdout.clone();
    mine.sort();
    theirs.sort();
    assert_eq!(mine, theirs);
}

#[test]
fn ingest_a_list_that_never_comes_fails_the_batch_and_writes_no_tile() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    write_terrain(dir);
    let (needed, centre) = ingest_tiles();
    let (store, _, _, _) = building_dirs(dir, &needed, centre);
    let (_, list) = odd_list_path(dir, "lists", "never.txt");
    let (job, out) = write_batch(dir, "never", json!({
        "buildings_pbf_dir": store, "buildings_pbf_list": list, "buildings_pbf_wait_s": 1,
    }));
    let t = Instant::now();
    let (ok, lines, stderr) = ingest(&job);
    let secs = t.elapsed().as_secs_f64();
    let all = format!("{}\n{stderr}", lines.join("\n"));
    assert!(!ok, "{all}");
    let want = format!(
        "Error: Building tiles not ready: the list file {list} (buildings_pbf_list) did not appear within 1 s \
         (buildings_pbf_wait_s); the ingest fails rather than write tiles without their buildings"
    );
    assert!(lines.contains(&format!("{WAIT_LINE}{list})")), "{all}");
    assert!(stderr.lines().any(|l| l == want), "want {want:?}:\n{all}");
    assert!(secs >= 1.0, "it waited 1 s first ({secs:.2} s)");
    assert_eq!(count(&lines, WAIT_LINE), 1, "{all}");
    assert_eq!(count(&lines, READY_LINE) + count(&lines, "[Rust] Decoding "), 0, "{all}");
    assert_eq!(count(&lines, "[Tile] ") + count(&lines, "[Rust] Progress:") + count(&lines, "[Error]"), 0, "{all}");
    assert!(files_in(&out).is_empty(), "no tile — nor temp file — may be left: {:?}", files_in(&out));
}

#[test]
fn ingest_a_listed_tile_missing_from_the_store_fails_the_batch_and_writes_no_tile() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    write_terrain(dir);
    let (needed, centre) = ingest_tiles();
    let (store, _, _, listed) = building_dirs(dir, &needed, centre);
    // The plugin guarantees presence; a missing file is a bug or a cache
    // cleared under the run. Two are missing: the first in name order is
    // named.
    let mut names = listed.clone();
    names.extend([(8500, 5700), (8499, 5700)]);
    let missing = tile_name((8499, 5700));

    // (a) The list there at launch: refused before any tile.
    let list = dir.join("missing.txt");
    publish_list(&list, &list_text(&names));
    let (job, out) = write_batch(dir, "missing", json!({ "buildings_pbf_dir": store, "buildings_pbf_list": list }));
    let (ok, lines, stderr) = ingest(&job);
    let all = format!("{}\n{stderr}", lines.join("\n"));
    assert!(!ok, "{all}");
    let want = format!("Error: Building tile {missing} listed in {} is not in {store}", list.display());
    assert_eq!(stderr.lines().next(), Some(want.as_str()), "{all}");
    assert_eq!(stderr.lines().count(), 1, "one line:\n{stderr}");
    assert_eq!(count(&lines, "[Tile] ") + count(&lines, DECODED) + count(&lines, "[Rust] Progress:"), 0, "{all}");
    assert!(files_in(&out).is_empty(), "{:?}", files_in(&out));

    // (b) The list written after launch: the batch has held its tiles —
    // and drops them.
    let late = dir.join("missing_late.txt");
    let (job, out) = write_batch(dir, "missing_late", json!({ "buildings_pbf_dir": store, "buildings_pbf_list": late }));
    let mut running = launch("ingest", &job);
    assert!(running.wait_for(WAIT_LINE).is_some(), "no wait line:\n{}", running.seen.join("\n"));
    publish_list(&late, &list_text(&names));
    let (ok, lines, stderr) = running.finish();
    let all = format!("{}\n{stderr}", lines.join("\n"));
    assert!(!ok, "{all}");
    let want = format!("Error: Building tile {missing} listed in {} is not in {store}", late.display());
    assert!(stderr.lines().any(|l| l == want), "want {want:?}:\n{all}");
    assert_eq!(count(&lines, "[Tile] ") + count(&lines, READY_LINE) + count(&lines, "[Rust] Progress:"), 0, "{all}");
    assert!(files_in(&out).is_empty(), "{:?}", files_in(&out));

    // (c) A name with a directory part: the list is refused, naming the line.
    let bad = dir.join("bad.txt");
    publish_list(&bad, &format!("{}\r\npool/{}\r\n", tile_name(listed[0]), tile_name(listed[1])));
    let (job, out) = write_batch(dir, "bad_name", json!({ "buildings_pbf_dir": store, "buildings_pbf_list": bad }));
    let (ok, lines, stderr) = ingest(&job);
    assert!(!ok);
    let want = format!(
        "Error: buildings_pbf_list {}: line 2: \"pool/{}\" has a directory part; list the tile file names \
         inside buildings_pbf_dir ({{z}}_{{x}}_{{y}}.pbf), one per line",
        bad.display(),
        tile_name(listed[1])
    );
    assert!(stderr.lines().any(|l| l == want), "want {want:?}:\n{stderr}");
    assert_eq!(count(&lines, "[Tile] "), 0);
    assert!(files_in(&out).is_empty());
}

#[test]
fn ingest_jobs_naming_two_lists_decode_each_once() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let r = ingest_reference(dir);
    let (a, b) = (dir.join("a.txt"), dir.join("b.txt"));
    publish_list(&a, &list_text(&r.listed));
    publish_list(&b, &list_text(&r.listed));
    let (job, out) = write_batch(dir, "two_lists", json!({ "buildings_pbf_dir": r.store }));
    let mut jobs: Vec<serde_json::Value> = serde_json::from_slice(&std::fs::read(&job).unwrap()).unwrap();
    for (i, j) in jobs.iter_mut().enumerate() {
        j["buildings_pbf_list"] = json!(if i % 3 == 0 { &a } else { &b });
    }
    std::fs::write(&job, serde_json::to_vec(&jobs).unwrap()).unwrap();
    let (ok, lines, stderr) = ingest(&job);
    let all = format!("{}\n{stderr}", lines.join("\n"));
    assert!(ok, "{all}");
    assert_eq!(count(&lines, &list_decode_line(r.listed.len(), &r.store)), 2, "one decode per list:\n{all}");
    assert_eq!(count(&lines, DECODED), 2, "{all}");
    assert!(read_dir_bytes(&out) == r.bytes, ".abt bytes must equal the old way's");
}

#[test]
fn ingest_bad_list_fields_fail_before_any_tile() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    write_terrain(dir);
    // Not created yet: the store's check comes after the fields' (the last
    // case), and the list is never read before the batch starts.
    let (store, list) = (dir.join("store"), dir.join("list.txt"));
    let cases = [
        (
            json!({ "buildings_pbf_list": list }),
            format!("buildings_pbf_list {list:?} needs buildings_pbf_dir: the listed tile names are files inside that directory"),
        ),
        (
            json!({ "buildings_pbf_dir": store, "buildings_pbf_list": list, "buildings_pbf_ready": "_ready" }),
            "buildings_pbf_list and buildings_pbf_ready exclude each other: the list file is itself the ready \
             signal (it is read once it exists); give buildings_pbf_list alone"
                .to_owned(),
        ),
        (
            json!({ "buildings_pbf_dir": store, "buildings_pbf_list": "lists/batch.txt" }),
            "buildings_pbf_list must be the absolute path of a file, got \"lists/batch.txt\"".to_owned(),
        ),
        (
            json!({ "buildings_pbf_dir": store, "buildings_pbf_list": list, "buildings_pbf_wait_s": -1 }),
            "buildings_pbf_wait_s must be a number of seconds from 0 to 86400, got -1".to_owned(),
        ),
        (
            json!({ "buildings_pbf_dir": store, "buildings_pbf_list": list, "buildings_hold_mb": -1 }),
            "buildings_hold_mb must be a number of megabytes from 0 to 1048576, got -1".to_owned(),
        ),
        // The store must be there at launch (the caller creates it first).
        (
            json!({ "buildings_pbf_dir": store, "buildings_pbf_list": list }),
            format!(
                "buildings_pbf_dir {store:?} is not a directory; with buildings_pbf_list it must exist when the \
                 job starts — the listed tiles are read from it"
            ),
        ),
    ];
    for (i, (extra, msg)) in cases.into_iter().enumerate() {
        let (job, out) = write_batch(dir, &format!("bad{i}"), extra);
        let (ok, lines, stderr) = ingest(&job);
        let all = format!("{}\n{stderr}", lines.join("\n"));
        assert!(!ok, "case {i} must fail:\n{all}");
        let first = out.join("t00.abt");
        let want = format!("Error: ingest job for {first:?}: {msg}");
        assert!(stderr.lines().any(|l| l == want), "case {i}: want {want:?}:\n{all}");
        assert!(!out.exists(), "case {i}: nothing is written");
        assert_eq!(count(&lines, "[Rust] Batch processing"), 0, "case {i}: refused before the batch starts:\n{all}");
    }
    // One list, two directories: refused (the list is decoded once).
    std::fs::create_dir(&store).unwrap();
    let (job, out) = write_batch(dir, "two_dirs", json!({ "buildings_pbf_dir": store, "buildings_pbf_list": list }));
    let mut jobs: Vec<serde_json::Value> = serde_json::from_slice(&std::fs::read(&job).unwrap()).unwrap();
    jobs[5]["buildings_pbf_dir"] = json!(dir.join("other"));
    std::fs::write(&job, serde_json::to_vec(&jobs).unwrap()).unwrap();
    let (ok, _, stderr) = ingest(&job);
    assert!(!ok);
    let want = format!(
        "Error: buildings_pbf_list {list:?}: the jobs for {:?} and {:?} give it different buildings_pbf_dir \
         ({store:?} against {:?}); the list is decoded once for the run, so every job naming it must give the same",
        out.join("t00.abt"),
        out.join("t05.abt"),
        dir.join("other")
    );
    assert!(stderr.lines().any(|l| l == want), "want {want:?}:\n{stderr}");
    assert!(!out.exists());
}

#[test]
fn ingest_a_single_job_refuses_the_list() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    write_terrain(dir);
    let (needed, centre) = ingest_tiles();
    let (store, _, _, listed) = building_dirs(dir, &needed, centre);
    let list = dir.join("single.txt");
    publish_list(&list, &list_text(&listed));
    let (batch, out) = write_batch(dir, "single", json!({ "buildings_pbf_dir": store, "buildings_pbf_list": list }));
    let jobs: Vec<serde_json::Value> = serde_json::from_slice(&std::fs::read(&batch).unwrap()).unwrap();
    let job = dir.join("single_job.json");
    std::fs::write(&job, serde_json::to_vec(&jobs[0]).unwrap()).unwrap();
    let (ok, lines, stderr) = ingest(&job);
    assert!(!ok, "{}\n{stderr}", lines.join("\n"));
    let want = format!(
        "Error: buildings_pbf_list {list:?} is read by a batch ingest only (a JSON array of jobs): a single job \
         decodes its whole buildings_pbf_dir before its tile, as it finds it, and would ignore the list — pass \
         the job as a one-job array"
    );
    assert!(stderr.lines().any(|l| l == want), "want {want:?}:\n{stderr}");
    assert!(!out.exists(), "nothing is written");

    // The same job as a one-job array takes it.
    std::fs::write(&job, serde_json::to_vec(&[&jobs[0]]).unwrap()).unwrap();
    let (ok, lines, stderr) = ingest(&job);
    assert!(ok, "{}\n{stderr}", lines.join("\n"));
    assert!(lines.contains(&list_decode_line(listed.len(), &store)), "{}", lines.join("\n"));
    assert_eq!(files_in(&out), ["t00.abt"]);
}

// ── The download job ────────────────────────────────────────────────────────

/// Terrain source zoom.
const Z: u32 = 12;
/// Two 128 px output tiles at 20 m, side by side.
const DPD: f64 = 20.0 / 111_111.0;

/// The z14 tiles under the download's outputs, and the one at their centre.
fn download_tiles() -> (Vec<(u32, u32)>, (u32, u32)) {
    let needed = tiles_under(UL.0, UL.1, UL.0 - 128.0 * DPD, UL.1 + 256.0 * DPD);
    let centre = (lon2tx(UL.1 + 128.0 * DPD, BZ), lat2ty(UL.0 - 64.0 * DPD, BZ));
    (needed, centre)
}

fn terrain_server() -> (u16, TileCounts) {
    serve_tiles(|z, x, y| Some(terrarium_png(tile_ground(z, x, y))))
}

/// Writes `<dir>/<name>.json`, output directory `<dir>/<name>`, with the
/// *extra* fields merged in.
fn write_download(dir: &Path, port: u16, name: &str, extra: serde_json::Value) -> (PathBuf, PathBuf) {
    let out_dir = dir.join(name);
    let tiles: Vec<serde_json::Value> = (0..2)
        .map(|i| {
            json!({
                "filename": format!("t{i}.abt"), "ul_lat": UL.0, "ul_lon": UL.1 + (i * 128) as f64 * DPD,
                "size_px": 128, "resolution_m": 20.0,
            })
        })
        .collect();
    let mut job = json!({
        "url_template": format!("http://127.0.0.1:{port}/{{z}}/{{x}}/{{y}}.png"),
        "encoding": "terrarium", "output_dir": out_dir, "zoom": Z, "max_connections": 16, "tiles": tiles,
    });
    for (k, v) in extra.as_object().unwrap() {
        job[k] = v.clone();
    }
    let path = dir.join(format!("{name}.json"));
    std::fs::write(&path, serde_json::to_vec_pretty(&job).unwrap()).unwrap();
    (path, out_dir)
}

/// The post-pass's `[Buildings] <n> pbf tile(s) → …` line without its timing.
fn burn_line(lines: &[String]) -> String {
    let burns: Vec<&String> = lines.iter().filter(|l| l.starts_with("[Buildings] ") && l.contains(" pbf tile(s) ")).collect();
    assert_eq!(burns.len(), 1, "one burn line expected:\n{}", lines.join("\n"));
    burns[0].rsplit_once(" in ").expect("the burn line ends in its timing").0.to_owned()
}

/// The reference, the old way: the job over `only/` — its `.abt` files and
/// burn line — after checking that the job over `all/` writes other bytes.
struct DownloadRef {
    store: String,
    listed: Vec<(u32, u32)>,
    bytes: Vec<(String, Vec<u8>)>,
    burn: String,
}

fn download_reference(dir: &Path, port: u16) -> DownloadRef {
    let (needed, centre) = download_tiles();
    let (store, only, all, listed) = building_dirs(dir, &needed, centre);
    let (job, out) = write_download(dir, port, "reference", json!({ "buildings_pbf_dir": only }));
    let (ok, lines) = download(&job);
    assert!(ok, "reference run failed:\n{}", lines.join("\n"));
    let bytes = read_dir_bytes(&out);
    assert_eq!(bytes.len(), 2);
    let burn = burn_line(&lines);
    let n = listed.len();
    assert!(burn.starts_with(&format!("[Buildings] {n} pbf tile(s) → {} building(s) ", n * 16)), "{burn}");

    let (job, out) = write_download(dir, port, "reference_all", json!({ "buildings_pbf_dir": all }));
    let (ok, lines) = download(&job);
    assert!(ok, "{}", lines.join("\n"));
    assert!(read_dir_bytes(&out) != bytes, "the left-out tile must change the bytes when burnt");
    DownloadRef { store, listed, bytes, burn }
}

#[test]
fn download_a_list_burns_exactly_what_a_directory_of_its_tiles_burns_and_nothing_else_of_the_store() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let (port, _) = terrain_server();
    let r = download_reference(dir, port);
    let list = dir.join("batch.txt");
    publish_list(&list, &list_text(&r.listed));
    let (job, out) = write_download(dir, port, "listed", json!({ "buildings_pbf_dir": r.store, "buildings_pbf_list": list }));
    let (ok, lines) = download(&job);
    let all = lines.join("\n");
    assert!(ok, "{all}");
    assert!(read_dir_bytes(&out) == r.bytes, ".abt bytes must equal the old way's");
    // `[Buildings] <n> pbf tile(s) → …`: the listed tiles, as the old way's.
    assert_eq!(burn_line(&lines), r.burn);
    assert_eq!(count(&lines, WAIT_LINE) + count(&lines, READY_LINE), 0, "the list was there:\n{all}");
    assert_eq!(count(&lines, "[Warn]"), 0, "the store's undecodable tile is never read:\n{all}");
    assert_eq!(count(&lines, "[Tile] t"), 2, "{all}");
}

#[test]
fn download_a_list_written_after_launch_is_waited_for_and_burns_the_same_bytes() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let (port, _) = terrain_server();
    let r = download_reference(dir, port);
    let (list, spelt) = odd_list_path(dir, "late lists", "batch 9.txt");
    let (job, out) = write_download(dir, port, "late_out", json!({
        "buildings_pbf_dir": r.store, "buildings_pbf_list": spelt, "buildings_pbf_wait_s": 60,
    }));
    let mut running = launch("download", &job);
    let announced = running.wait_for(WAIT_LINE);
    let announced = announced.unwrap_or_else(|| panic!("no wait line:\n{}", running.seen.join("\n")));
    thread::sleep(Duration::from_millis(1500));
    std::fs::create_dir_all(list.parent().unwrap()).unwrap();
    publish_list(&list, &list_text(&r.listed));
    let listed_at = Instant::now();
    let (ok, lines, _) = running.finish();
    let all = lines.join("\n");
    assert!(ok, "{all}");
    assert_eq!(count(&lines, WAIT_LINE), 1, "{all}");
    assert!(lines.contains(&format!("{WAIT_LINE}{spelt})")), "want the list as given, {spelt:?}:\n{all}");
    let ready: Vec<&String> = lines.iter().filter(|l| l.starts_with(READY_LINE)).collect();
    assert_eq!(ready.len(), 1, "{all}");
    let secs: f64 = ready[0][READY_LINE.len()..].strip_suffix(" s").expect("<s.s> s").parse().unwrap();
    let gap = listed_at.duration_since(announced).as_secs_f64();
    assert!(secs + 0.06 >= gap && secs <= gap + 1.5, "waited {secs} s, list {gap:.2} s after the wait line");
    let at = |needle: &str| lines.iter().position(|l| l.contains(needle)).unwrap();
    assert!(at(WAIT_LINE) < at(READY_LINE) && at(READY_LINE) < at(" pbf tile(s) "), "{all}");
    assert_eq!(burn_line(&lines), r.burn);
    assert!(read_dir_bytes(&out) == r.bytes, ".abt bytes must equal the old way's");
}

#[test]
fn download_a_list_that_never_comes_or_names_a_missing_tile_fails_and_removes_the_outputs() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let (port, _) = terrain_server();
    let (needed, centre) = download_tiles();
    let (store, _, _, listed) = building_dirs(dir, &needed, centre);

    // Never comes.
    let (_, list) = odd_list_path(dir, "lists", "never.txt");
    let (job, out) = write_download(dir, port, "never", json!({
        "buildings_pbf_dir": store, "buildings_pbf_list": list, "buildings_pbf_wait_s": 1,
    }));
    let (ok, lines) = download(&job);
    let all = lines.join("\n");
    assert!(!ok, "{all}");
    let want = format!(
        "Error: Building tiles not ready: the list file {list} (buildings_pbf_list) did not appear within 1 s \
         (buildings_pbf_wait_s); the download fails rather than leave terrain without its buildings"
    );
    assert!(lines.iter().any(|l| *l == want), "want {want:?}:\n{all}");
    assert!(lines.contains(&format!("{WAIT_LINE}{list})")), "{all}");
    assert_eq!(count(&lines, READY_LINE) + count(&lines, " pbf tile(s) "), 0, "{all}");
    assert!(lines.iter().any(|l| l.starts_with("[Download] Removed 2 incomplete .abt file(s)")), "{all}");
    assert!(read_dir_bytes(&out).is_empty(), "the outputs must be removed");

    // Names a tile the store does not hold.
    let missing = tile_name((8000, 5800));
    let list = dir.join("missing.txt");
    let mut names = listed.clone();
    names.push((8000, 5800));
    publish_list(&list, &list_text(&names));
    let (job, out) = write_download(dir, port, "missing", json!({ "buildings_pbf_dir": store, "buildings_pbf_list": list }));
    let (ok, lines) = download(&job);
    let all = lines.join("\n");
    assert!(!ok, "{all}");
    let want = format!("Error: Building tile {missing} listed in {} is not in {store}", list.display());
    assert!(lines.iter().any(|l| *l == want), "want {want:?}:\n{all}");
    assert_eq!(count(&lines, " pbf tile(s) ") + count(&lines, "[Stats]"), 0, "{all}");
    assert!(lines.iter().any(|l| l.starts_with("[Download] Removed 2 incomplete .abt file(s)")), "{all}");
    assert!(read_dir_bytes(&out).is_empty(), "the outputs must be removed");
}

#[test]
fn download_bad_list_fields_fail_before_any_request() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let (port, counts) = terrain_server();
    let (store, list) = (dir.join("store"), dir.join("list.txt"));
    let cases = [
        (
            json!({ "buildings_pbf_list": list }),
            format!("buildings_pbf_list {list:?} needs buildings_pbf_dir: the listed tile names are files inside that directory"),
        ),
        (
            json!({ "buildings_pbf_dir": store, "buildings_pbf_list": list, "buildings_pbf_ready": "_ready" }),
            "buildings_pbf_list and buildings_pbf_ready exclude each other: the list file is itself the ready \
             signal (it is read once it exists); give buildings_pbf_list alone"
                .to_owned(),
        ),
        (
            json!({ "buildings_pbf_dir": store, "buildings_pbf_list": "batch.txt" }),
            "buildings_pbf_list must be the absolute path of a file, got \"batch.txt\"".to_owned(),
        ),
        (
            json!({ "buildings_pbf_dir": store, "buildings_pbf_list": list, "buildings_pbf_wait_s": 1e6 }),
            "buildings_pbf_wait_s must be a number of seconds from 0 to 86400, got 1000000".to_owned(),
        ),
        // The store must be there at launch (the caller creates it first).
        (
            json!({ "buildings_pbf_dir": store, "buildings_pbf_list": list }),
            format!(
                "buildings_pbf_dir {store:?} is not a directory; with buildings_pbf_list it must exist when the \
                 job starts — the listed tiles are read from it"
            ),
        ),
    ];
    for (i, (extra, msg)) in cases.into_iter().enumerate() {
        let (job, out) = write_download(dir, port, &format!("bad{i}"), extra);
        let (ok, lines) = download(&job);
        let all = lines.join("\n");
        assert!(!ok, "case {i} must fail:\n{all}");
        assert!(lines.iter().any(|l| *l == format!("Error: {msg}")), "case {i}: want {msg:?}:\n{all}");
        assert!(!out.exists(), "case {i}: nothing is written");
    }
    assert!(counts.lock().unwrap().is_empty(), "no request may go out");
}

// ── Repair jobs honour the list ──────────────────────────────────────────────

/// The source tile a repair refetches: the z12 tile under the outputs — it
/// holds every output pixel, so the repair rewrites them all, the left-out
/// tile's ground included.
fn repair_tile() -> serde_json::Value {
    json!([[Z, lon2tx(UL.1 + 10.0 * DPD, Z), lat2ty(UL.0 - 10.0 * DPD, Z)]])
}

/// A first pass without buildings into `<dir>/<name>` — what a repair then
/// patches — returning the output directory and its bytes.
fn terrain_pass(dir: &Path, port: u16, name: &str) -> (PathBuf, Vec<(String, Vec<u8>)>) {
    let (job, out) = write_download(dir, port, name, json!({}));
    let (ok, lines) = download(&job);
    assert!(ok, "{}", lines.join("\n"));
    let bytes = read_dir_bytes(&out);
    assert_eq!(bytes.len(), 2);
    (out, bytes)
}

/// The repair of [`repair_tile`] over the outputs of `<dir>/<name>`, with
/// *extra*: `(exit ok, stderr lines)`.
fn repair(dir: &Path, port: u16, name: &str, extra: serde_json::Value) -> (bool, Vec<String>) {
    let mut extra = extra;
    extra["repair_source_tiles"] = repair_tile();
    let (job, _) = write_download(dir, port, name, extra);
    download(&job)
}

/// The repair's `[Buildings] repair: <b> building(s), <p> repaired px raised`.
fn repair_burn_line(lines: &[String]) -> String {
    let found: Vec<&String> = lines.iter().filter(|l| l.starts_with("[Buildings] repair: ")).collect();
    assert_eq!(found.len(), 1, "one repair burn line expected:\n{}", lines.join("\n"));
    found[0].clone()
}

#[test]
fn download_a_repair_with_the_list_burns_what_a_repair_with_a_directory_of_its_tiles_burns() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let (port, _) = terrain_server();
    let (needed, centre) = download_tiles();
    let (store, only, all, listed) = building_dirs(dir, &needed, centre);
    let list = dir.join("batch.txt");
    publish_list(&list, &list_text(&listed));
    // Three copies of a first pass without buildings: the repair's burn is
    // all the buildings they get.
    let (out_only, terrain) = terrain_pass(dir, port, "rep_only");
    let (out_list, _) = terrain_pass(dir, port, "rep_list");
    let (out_all, _) = terrain_pass(dir, port, "rep_all");

    // The old way: a directory holding exactly the listed tiles.
    let (ok, want) = repair(dir, port, "rep_only", json!({ "buildings_pbf_dir": only }));
    assert!(ok, "{}", want.join("\n"));
    let want_bytes = read_dir_bytes(&out_only);
    assert!(want_bytes != terrain, "the repair must have burnt buildings");

    // The list over the store: the same burn line, the same bytes — and the
    // store's undecodable tile, z15 tile and FIFO never touched.
    let (ok, lines) = repair(dir, port, "rep_list", json!({ "buildings_pbf_dir": store, "buildings_pbf_list": list }));
    let all_lines = lines.join("\n");
    assert!(ok, "{all_lines}");
    assert_eq!(repair_burn_line(&lines), repair_burn_line(&want));
    let n = listed.len();
    assert!(repair_burn_line(&lines).starts_with(&format!("[Buildings] repair: {} building(s), ", n * 16)), "{all_lines}");
    assert!(read_dir_bytes(&out_list) == want_bytes, ".abt bytes must equal the old way's repair");
    assert_eq!(count(&lines, "[Warn]") + count(&lines, WAIT_LINE) + count(&lines, READY_LINE), 0, "{all_lines}");
    assert_eq!(count(&lines, "[Download] repair: "), 2, "{all_lines}");

    // With the left-out tile too, the repair burns other bytes: the list's
    // selection is what made them equal.
    let (ok, lines) = repair(dir, port, "rep_all", json!({ "buildings_pbf_dir": all }));
    assert!(ok, "{}", lines.join("\n"));
    assert!(read_dir_bytes(&out_all) != want_bytes, "the left-out tile changes the repaired bytes");
}

#[test]
fn download_a_repair_waits_for_its_list_like_a_first_pass_and_fails_without_it() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let (port, counts) = terrain_server();
    let (needed, centre) = download_tiles();
    let (store, only, _, listed) = building_dirs(dir, &needed, centre);
    let (out_only, terrain) = terrain_pass(dir, port, "rep_only");
    let (ok, want) = repair(dir, port, "rep_only", json!({ "buildings_pbf_dir": only }));
    assert!(ok, "{}", want.join("\n"));
    let want_bytes = read_dir_bytes(&out_only);

    // The list written after launch: the first pass's wait lines, naming the
    // list as the job spells it, then the burn — the same bytes.
    let (out, _) = terrain_pass(dir, port, "rep_late");
    let (list, spelt) = odd_list_path(dir, "repair lists", "batch 4.txt");
    let mut extra = json!({ "buildings_pbf_dir": store, "buildings_pbf_list": spelt, "buildings_pbf_wait_s": 60 });
    extra["repair_source_tiles"] = repair_tile();
    let (job, _) = write_download(dir, port, "rep_late", extra);
    let mut running = launch("download", &job);
    let announced = running.wait_for(WAIT_LINE);
    let announced = announced.unwrap_or_else(|| panic!("no wait line:\n{}", running.seen.join("\n")));
    thread::sleep(Duration::from_millis(1500));
    assert!(read_dir_bytes(&out) == terrain, "nothing is written before the list");
    std::fs::create_dir_all(list.parent().unwrap()).unwrap();
    publish_list(&list, &list_text(&listed));
    let listed_at = Instant::now();
    let (ok, lines, _) = running.finish();
    let all = lines.join("\n");
    assert!(ok, "{all}");
    assert!(lines.contains(&format!("{WAIT_LINE}{spelt})")), "want the list as given, {spelt:?}:\n{all}");
    let ready: Vec<&String> = lines.iter().filter(|l| l.starts_with(READY_LINE)).collect();
    assert_eq!(ready.len(), 1, "{all}");
    let secs: f64 = ready[0][READY_LINE.len()..].strip_suffix(" s").expect("<s.s> s").parse().unwrap();
    let gap = listed_at.duration_since(announced).as_secs_f64();
    assert!(secs + 0.06 >= gap && secs <= gap + 1.5, "waited {secs} s, list {gap:.2} s after the wait line");
    let at = |p: &str| lines.iter().position(|l| l.starts_with(p)).unwrap_or_else(|| panic!("no {p:?}:\n{all}"));
    assert!(at(WAIT_LINE) < at(READY_LINE) && at(READY_LINE) < at("[Buildings] repair: "), "{all}");
    assert_eq!(repair_burn_line(&lines), repair_burn_line(&want));
    assert!(read_dir_bytes(&out) == want_bytes, ".abt bytes must equal the old way's repair");

    // A list that never comes fails the repair; a repair never removes a
    // file, and this one wrote none.
    let (out, _) = terrain_pass(dir, port, "rep_never");
    let (_, never) = odd_list_path(dir, "repair lists", "never.txt");
    let (ok, lines) = repair(dir, port, "rep_never", json!({
        "buildings_pbf_dir": store, "buildings_pbf_list": never, "buildings_pbf_wait_s": 1,
    }));
    let all = lines.join("\n");
    assert!(!ok, "{all}");
    let want_err = format!(
        "Error: Building tiles not ready: the list file {never} (buildings_pbf_list) did not appear within 1 s \
         (buildings_pbf_wait_s); the download fails rather than leave terrain without its buildings"
    );
    assert!(lines.iter().any(|l| *l == want_err), "want {want_err:?}:\n{all}");
    assert!(lines.contains(&format!("{WAIT_LINE}{never})")), "{all}");
    assert_eq!(count(&lines, "[Buildings] repair: ") + count(&lines, "px rewritten"), 0, "{all}");
    assert!(read_dir_bytes(&out) == terrain, "the existing files are unchanged");

    // A listed tile the store does not hold: the same error as a first pass.
    let (out, _) = terrain_pass(dir, port, "rep_missing");
    let missing_list = dir.join("missing.txt");
    let mut names = listed.clone();
    names.push((8000, 5800));
    publish_list(&missing_list, &list_text(&names));
    let (ok, lines) = repair(dir, port, "rep_missing", json!({ "buildings_pbf_dir": store, "buildings_pbf_list": missing_list }));
    let all = lines.join("\n");
    assert!(!ok, "{all}");
    let want_err = format!(
        "Error: Building tile {} listed in {} is not in {store}",
        tile_name((8000, 5800)),
        missing_list.display()
    );
    assert!(lines.iter().any(|l| *l == want_err), "want {want_err:?}:\n{all}");
    assert!(read_dir_bytes(&out) == terrain, "the existing files are unchanged");

    // Bad list fields: refused before any request, as on a first pass.
    let (out, _) = terrain_pass(dir, port, "rep_bad");
    let good_list = dir.join("good.txt");
    publish_list(&good_list, &list_text(&listed));
    let cases = [
        (
            json!({ "buildings_pbf_dir": store, "buildings_pbf_list": good_list, "buildings_pbf_ready": "_ready" }),
            "Error: buildings_pbf_list and buildings_pbf_ready exclude each other: the list file is itself the ready \
             signal (it is read once it exists); give buildings_pbf_list alone"
                .to_owned(),
        ),
        (
            json!({ "buildings_pbf_dir": store, "buildings_pbf_list": "good.txt" }),
            "Error: buildings_pbf_list must be the absolute path of a file, got \"good.txt\"".to_owned(),
        ),
        (
            json!({ "buildings_pbf_dir": dir.join("no_store"), "buildings_pbf_list": good_list }),
            format!(
                "Error: buildings_pbf_dir {:?} is not a directory; with buildings_pbf_list it must exist when the \
                 job starts — the listed tiles are read from it",
                dir.join("no_store")
            ),
        ),
    ];
    for (i, (extra, msg)) in cases.into_iter().enumerate() {
        let before: usize = counts.lock().unwrap().values().sum();
        let (ok, lines) = repair(dir, port, "rep_bad", extra);
        let all = lines.join("\n");
        assert!(!ok, "case {i}:\n{all}");
        assert!(lines.iter().any(|l| *l == msg), "case {i}: want {msg:?}:\n{all}");
        let after: usize = counts.lock().unwrap().values().sum();
        assert_eq!(after, before, "case {i}: no request may go out");
        assert!(read_dir_bytes(&out) == terrain, "case {i}: the existing files are unchanged");
    }
}

// ── An empty list: no building tile has data ─────────────────────────────────

/// The empty lists a caller may write when none of its tiles has data: no
/// byte at all, and blank and comment lines only.
const EMPTY_LISTS: [&str; 2] = ["", "# no listed tile has data\r\n\r\n   \r\n"];

#[test]
fn ingest_an_empty_list_burns_nothing_exactly_as_an_empty_directory_does() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    write_terrain(dir);
    let (needed, centre) = ingest_tiles();
    // The store holds tiles under the batch; the list names none of them.
    let (store, _, _, _) = building_dirs(dir, &needed, centre);
    // The old way: an empty directory.
    let empty = dir.join("empty");
    std::fs::create_dir(&empty).unwrap();
    let (job, out) = write_batch(dir, "empty_dir", json!({ "buildings_pbf_dir": empty }));
    let (ok, want, stderr) = ingest(&job);
    assert!(ok && stderr.is_empty(), "{}\n{stderr}", want.join("\n"));
    let want_bytes = read_dir_bytes(&out);
    assert_eq!(count(&want, "[Info] buildings_pbf_dir: 0 tile(s), no buildings in extent"), TILES);
    let (job, out) = write_batch(dir, "terrain_only", json!({}));
    assert!(ingest(&job).0);
    assert!(read_dir_bytes(&out) == want_bytes, "an empty directory burns nothing");

    for (i, text) in EMPTY_LISTS.iter().enumerate() {
        let list = dir.join(format!("empty_{i}.txt"));
        publish_list(&list, text);
        let (job, out) = write_batch(dir, &format!("listed_{i}"), json!({ "buildings_pbf_dir": store, "buildings_pbf_list": list }));
        let (ok, lines, stderr) = ingest(&job);
        let all = format!("{}\n{stderr}", lines.join("\n"));
        assert!(ok, "case {i}: an empty list is never an error:\n{all}");
        assert!(stderr.is_empty(), "case {i}: {stderr}");
        assert!(read_dir_bytes(&out) == want_bytes, "case {i}: the bytes of the empty directory");
        // The empty directory's lines — per tile `[Info] buildings_pbf_dir:
        // 0 tile(s), no buildings in extent`, the decoded line with 0 tiles,
        // no progress line — but the header.
        let header = list_decode_line(0, &store);
        assert_eq!(count(&lines, &header), 1, "case {i}: want {header:?}:\n{all}");
        let dir_header = format!("{DIR_DECODE}{empty:?} (once for the run)...");
        let mine: Vec<String> = lines.iter().map(|l| if *l == header { dir_header.clone() } else { l.clone() }).collect();
        assert_eq!(normalized(&mine), normalized(&want), "case {i}");
        assert_eq!(count(&lines, "[Info] buildings_pbf_dir: 0 tile(s), no buildings in extent"), TILES, "case {i}");
        assert!(lines.iter().any(|l| l.starts_with(&format!("{DECODED}0 tile(s), 0 building(s) in "))), "case {i}:\n{all}");
        assert_eq!(count(&lines, "[Rust] Decoding building tiles: "), 0, "case {i}: no tile, no progress line");
    }
}

#[test]
fn download_an_empty_list_burns_nothing_exactly_as_an_empty_directory_does() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let (port, _) = terrain_server();
    let (needed, centre) = download_tiles();
    let (store, _, _, _) = building_dirs(dir, &needed, centre);
    let empty = dir.join("empty");
    std::fs::create_dir(&empty).unwrap();
    let (job, out) = write_download(dir, port, "empty_dir", json!({ "buildings_pbf_dir": empty }));
    let (ok, want) = download(&job);
    assert!(ok, "{}", want.join("\n"));
    let want_bytes = read_dir_bytes(&out);
    const NONE: &str = "[Buildings] 0 pbf tile(s), no buildings in extent";
    let exactly = |lines: &[String], l: &str| lines.iter().filter(|x| *x == l).count();
    assert_eq!(exactly(&want, NONE), 1, "{}", want.join("\n"));
    let tile_lines = |lines: &[String]| -> Vec<String> { lines.iter().filter(|l| l.starts_with("[Tile] ")).cloned().collect() };

    for (i, text) in EMPTY_LISTS.iter().enumerate() {
        let list = dir.join(format!("empty_{i}.txt"));
        publish_list(&list, text);
        let (job, out) =
            write_download(dir, port, &format!("listed_{i}"), json!({ "buildings_pbf_dir": store, "buildings_pbf_list": list }));
        let (ok, lines) = download(&job);
        let all = lines.join("\n");
        assert!(ok, "case {i}: an empty list is never an error:\n{all}");
        assert!(read_dir_bytes(&out) == want_bytes, "case {i}: the bytes of the empty directory");
        assert_eq!(exactly(&lines, NONE), 1, "case {i}: want {NONE:?}:\n{all}");
        assert_eq!(count(&lines, " pbf tile(s)"), 1, "case {i}:\n{all}");
        assert_eq!(tile_lines(&lines), tile_lines(&want), "case {i}");
        assert_eq!(count(&lines, WAIT_LINE) + count(&lines, "[Warn]"), 0, "case {i}:\n{all}");
    }
}

/// The list names each needed tile once, whatever the duplicates: the
/// decode counts each once (a guard on the fixtures above).
#[test]
fn the_fixture_lists_name_distinct_tiles() {
    let (needed, centre) = ingest_tiles();
    let set: BTreeSet<(u32, u32)> = needed.iter().copied().collect();
    assert_eq!(set.len(), needed.len());
    assert!(set.contains(&centre));
    let (needed, centre) = download_tiles();
    assert!(needed.len() >= 4 && needed.contains(&centre), "{needed:?}");
}
