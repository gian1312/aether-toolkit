//! Batch `ingest` with `buildings_pbf_ready` (CONTRACT §9a, changelog 48): a
//! caller fetches the building tiles WHILE the batch samples its terrain and
//! declares the directory complete with a marker file. A marker there at
//! launch runs exactly as a batch without one. A marker that comes later is
//! watched for: the tiles whose terrain is done before it are held (past the
//! hold budget, they wait on their workers), none is written, and once the
//! directory is decoded the batch writes exactly what a batch whose
//! directory was complete at launch writes. A marker that never comes, or a
//! directory that does not decode, fails the batch with no tile written.
//! Without the field nothing changes; a single job refuses it.
//!
//! Hermetic: the terrain is an `.abt` source, the building tiles are MVT
//! written by the test.

mod common;

use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use aether_converter::download::{lat2ty, lon2tx};
use common::{mvt_building_tile, read_dir_bytes, write_abt};
use serde_json::json;

/// Zoom of the building tiles (OpenFreeMap's).
const BZ: u32 = 14;
const MARKER: &str = "_ready";
const WAIT_LINE: &str = "[Buildings] waiting for the building tiles (";
const READY_LINE: &str = "[Buildings] building tiles ready after ";
const DECODE_START: &str = "[Rust] Decoding building tiles in ";
const DECODED: &str = "[Rust] Building tiles decoded: ";
const FAIL_TEXT: &str = "Building tiles not ready: the marker file ";
/// A run that does not end by then is killed and reported, never hung.
const LIMIT: Duration = Duration::from_secs(180);

// ── The batch ───────────────────────────────────────────────────────────────

/// Output tiles: a COLS × ROWS grid of SIZE px at RES m.
const SIZE: usize = 128;
const RES: f64 = 10.0;
const PD: f64 = RES / 111_111.0;
const COLS: usize = 4;
const ROWS: usize = 3;
const TILES: usize = COLS * ROWS;
const UL: (f64, f64) = (47.01, 8.003); // (lat, lon)
/// The samples a held tile keeps.
const TILE_BYTES: usize = SIZE * SIZE * 2;

/// `(ul_lat, ul_lon)` of every output tile.
fn grid() -> Vec<(f64, f64)> {
    let mut v = Vec::new();
    for r in 0..ROWS {
        for c in 0..COLS {
            v.push((UL.0 - (r * SIZE) as f64 * PD, UL.1 + (c * SIZE) as f64 * PD));
        }
    }
    v
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

/// Writes one building tile per z14 tile under the grid — a 4 × 4 block of
/// houses of varying size and height each — into *dir* (created); returns
/// how many.
fn write_building_tiles(dir: &Path) -> usize {
    std::fs::create_dir_all(dir).unwrap();
    let (x0, x1) = (lon2tx(UL.1, BZ), lon2tx(UL.1 + (COLS * SIZE) as f64 * PD, BZ));
    let (y0, y1) = (lat2ty(UL.0, BZ), lat2ty(UL.0 - (ROWS * SIZE) as f64 * PD, BZ));
    let mut n = 0;
    for y in y0..=y1 {
        for x in x0..=x1 {
            let mut features = Vec::new();
            for j in 0..4i32 {
                for i in 0..4i32 {
                    let (bx, by) = (300 + i * 950, 250 + j * 950);
                    let h = 8.0 + ((x * 7 + y * 5 + (i * 4 + j) as u32 * 3) % 33) as f64;
                    let (ex, ey) = (bx + 300 + i * 37, by + 260 + j * 41);
                    features.push((h, vec![(bx, by), (ex, by), (ex, ey), (bx, ey)]));
                }
            }
            std::fs::write(dir.join(format!("{BZ}_{x}_{y}.pbf")), mvt_building_tile(&features)).unwrap();
            n += 1;
        }
    }
    assert!(n >= 4, "the grid must span several building tiles, got {n}");
    n
}

/// Writes the batch `<dir>/<name>.json` — outputs in `<dir>/<name>/`, the
/// building directory *pbf* (if any), the *extra* fields on every job.
fn write_jobs(dir: &Path, name: &str, pbf: Option<&Path>, extra: serde_json::Value) -> (PathBuf, PathBuf) {
    let out = dir.join(name);
    let src = dir.join("terrain.abt");
    let jobs: Vec<serde_json::Value> = grid()
        .iter()
        .enumerate()
        .map(|(i, &(lat, lon))| {
            let mut j = json!({
                "output_path": out.join(format!("t{i:02}.abt")), "format": "r16sint",
                "ul_lat": lat, "ul_lon": lon, "resolution_m": RES, "size_px": SIZE,
                "sources": [{"path": src}],
            });
            if let Some(p) = pbf {
                j["buildings_pbf_dir"] = json!(p);
            }
            if let (Some(o), Some(e)) = (j.as_object_mut(), extra.as_object()) {
                for (k, v) in e {
                    o.insert(k.clone(), v.clone());
                }
            }
            j
        })
        .collect();
    let path = dir.join(format!("{name}.json"));
    std::fs::write(&path, serde_json::to_vec_pretty(&jobs).unwrap()).unwrap();
    (path, out)
}

/// The files of *out* (none when it does not exist).
fn files_in(out: &Path) -> Vec<String> {
    match std::fs::read_dir(out) {
        Ok(rd) => rd.filter_map(|e| e.ok()).map(|e| e.file_name().to_string_lossy().into_owned()).collect(),
        Err(_) => Vec::new(),
    }
}

// ── Running the CLI ─────────────────────────────────────────────────────────

/// A running batch `ingest` whose stdout lines are read as they are printed.
struct Running {
    child: Child,
    lines: mpsc::Receiver<(Instant, String)>,
    stderr: thread::JoinHandle<String>,
    t0: Instant,
    seen: Vec<String>,
}

fn launch(job: &Path) -> Running {
    let t0 = Instant::now();
    let mut child = Command::new(env!("CARGO_BIN_EXE_aether_converter"))
        .args(["ingest", "--job-file"])
        .arg(job)
        .env_remove("AETHER_DIAG_FILE")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let out = child.stdout.take().unwrap();
    let mut err = child.stderr.take().unwrap();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        for line in BufReader::new(out).lines().map_while(Result::ok) {
            if tx.send((Instant::now(), line)).is_err() {
                break;
            }
        }
    });
    let stderr = thread::spawn(move || {
        let mut s = String::new();
        let _ = err.read_to_string(&mut s);
        s
    });
    Running { child, lines: rx, stderr, t0, seen: Vec::new() }
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

    /// Wait for the exit (kill after [`LIMIT`]): `(exit ok, every stdout
    /// line, stderr)`.
    fn finish(mut self) -> (bool, Vec<String>, String) {
        let ok = loop {
            if let Some(st) = self.child.try_wait().unwrap() {
                break st.success();
            }
            if self.t0.elapsed() > LIMIT {
                let _ = self.child.kill();
                let _ = self.child.wait();
                panic!("ingest still running after {LIMIT:?} — killed. stdout:\n{}", self.seen.join("\n"));
            }
            while let Ok((_, l)) = self.lines.try_recv() {
                self.seen.push(l);
            }
            thread::sleep(Duration::from_millis(20));
        };
        // The reader ends at EOF, after the exit.
        self.seen.extend(self.lines.iter().map(|(_, l)| l));
        (ok, self.seen, self.stderr.join().unwrap())
    }
}

fn run(job: &Path) -> (bool, Vec<String>, String) {
    launch(job).finish()
}

fn count(lines: &[String], needle: &str) -> usize {
    lines.iter().filter(|l| l.contains(needle)).count()
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

/// `(held tiles, "<m.m>", tiles that waited on their worker)` from the one
/// `[Buildings] <n> tile(s) held (<m.m> MB) …` line.
fn held_line(lines: &[String]) -> (usize, String, Option<usize>) {
    let found: Vec<&String> = lines.iter().filter(|l| l.contains(" tile(s) held (")).collect();
    assert_eq!(found.len(), 1, "one held line expected:\n{}", lines.join("\n"));
    let rest = found[0].strip_prefix("[Buildings] ").expect("[Buildings] prefix");
    let (n, rest) = rest.split_once(" tile(s) held (").unwrap();
    let (mb, rest) = rest.split_once(" MB) until the building tiles were ready").expect("the held line's text");
    let blocked = rest.strip_prefix("; ").map(|r| {
        let (k, tail) = r.split_once(" more waited on their worker threads (hold budget ").expect("the waiting part");
        assert!(tail.ends_with(" MB full)"), "{r}");
        k.parse().unwrap()
    });
    if blocked.is_none() {
        assert!(rest.is_empty(), "{rest:?}");
    }
    (n.parse().unwrap(), mb.to_owned(), blocked)
}

/// The reference: the batch without the field, its directory complete at
/// launch (`<dir>/pbf`, written here). Checks that the burn raised pixels
/// (bytes differ from the terrain alone), so equal bytes below mean the
/// same burn. Returns its `.abt` files and its normalized stdout.
fn reference(dir: &Path) -> (Vec<(String, Vec<u8>)>, Vec<String>) {
    write_terrain(dir);
    write_building_tiles(&dir.join("pbf"));
    let (job, out) = write_jobs(dir, "reference", Some(&dir.join("pbf")), json!({}));
    let (ok, lines, stderr) = run(&job);
    assert!(ok, "reference run failed:\n{}\n{stderr}", lines.join("\n"));
    assert!(stderr.is_empty(), "{stderr}");
    assert_eq!(count(&lines, "[Buildings]"), 0, "{}", lines.join("\n"));
    let with = read_dir_bytes(&out);
    assert_eq!(with.len(), TILES);

    let (job, out) = write_jobs(dir, "terrain_only", None, json!({}));
    let (ok, lines_t, _) = run(&job);
    assert!(ok, "terrain-only run failed:\n{}", lines_t.join("\n"));
    let without = read_dir_bytes(&out);
    assert_eq!(without.len(), TILES);
    let differ = with.iter().zip(&without).filter(|((_, a), (_, b))| a != b).count();
    assert!(differ >= TILES / 2, "the buildings must have raised pixels in most tiles ({differ} of {TILES})");
    (with, normalized(&lines))
}

// ── 1. Without the field: unchanged ─────────────────────────────────────────

#[test]
fn the_wait_and_hold_fields_alone_change_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let (want, stdout) = reference(dir);
    // Not read without `buildings_pbf_ready`: not even checked.
    let (job, out) = write_jobs(dir, "fields_only", Some(&dir.join("pbf")), json!({
        "buildings_pbf_wait_s": -1, "buildings_hold_mb": -5,
    }));
    let (ok, lines, stderr) = run(&job);
    assert!(ok, "{}\n{stderr}", lines.join("\n"));
    assert!(read_dir_bytes(&out) == want, "the bytes of the batch without the fields");
    assert_eq!(normalized(&lines), stdout);
    assert!(stderr.is_empty(), "{stderr}");
}

// ── 2. Marker there at launch ───────────────────────────────────────────────

#[test]
fn a_marker_present_at_launch_runs_as_a_batch_without_one() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let (want, stdout) = reference(dir);
    // The same directory, now with its marker: the marker is no tile.
    std::fs::write(dir.join("pbf").join(MARKER), b"").unwrap();
    let (job, out) = write_jobs(dir, "marked", Some(&dir.join("pbf")), json!({ "buildings_pbf_ready": MARKER }));
    let (ok, lines, stderr) = run(&job);
    assert!(ok, "{}\n{stderr}", lines.join("\n"));
    assert_eq!(count(&lines, "[Buildings]"), 0, "no wait when the marker is there:\n{}", lines.join("\n"));
    assert!(read_dir_bytes(&out) == want, ".abt bytes must equal the reference's");
    // The same lines: the decode before any tile, every tile's lines.
    assert_eq!(normalized(&lines), stdout);
    let at = |p: &str| lines.iter().position(|l| l.starts_with(p)).unwrap();
    assert!(at(DECODED) < at("[Tile] "), "{}", lines.join("\n"));
    assert!(stderr.is_empty(), "{stderr}");
}

// ── 3. Marker written while the batch runs ──────────────────────────────────

#[test]
fn a_marker_written_after_launch_holds_the_finished_tiles_and_burns_the_same_bytes() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let (want, stdout) = reference(dir);

    // The directory does not exist at launch: nothing may look at it before
    // the marker (an early read would fail the batch).
    let pbf = dir.join("pbf_late");
    let marker = pbf.join(MARKER);
    let (job, out) = write_jobs(dir, "late", Some(&pbf), json!({
        "buildings_pbf_ready": MARKER, "buildings_pbf_wait_s": 120,
    }));
    let mut running = launch(&job);
    let t0 = running.t0;
    // A tile's terrain was done before the marker: the batch says it waits.
    let announced = running.wait_for(WAIT_LINE);
    let announced = announced.unwrap_or_else(|| panic!("no wait line:\n{}", running.seen.join("\n")));
    // The test's fetch: 2 s after launch, the tiles, then the marker. Until
    // then no tile is written — none has its buildings yet.
    thread::sleep(Duration::from_secs(2).saturating_sub(t0.elapsed()));
    let early = files_in(&out);
    assert!(early.is_empty(), "written before the building tiles were ready: {early:?}");
    write_building_tiles(&pbf);
    std::fs::write(&marker, b"").unwrap();
    let marked = Instant::now();
    let (ok, lines, stderr) = running.finish();
    let all = lines.join("\n");
    assert!(ok, "{all}\n{stderr}");
    assert!(stderr.is_empty(), "{stderr}");

    // The bytes of the batch whose directory was complete at launch.
    assert!(read_dir_bytes(&out) == want, ".abt bytes must equal the reference's");

    // One wait line naming the marker, one ready line, one held line.
    assert_eq!(count(&lines, WAIT_LINE), 1, "{all}");
    assert!(lines.contains(&format!("{WAIT_LINE}{})", marker.display())), "{all}");
    let ready: Vec<&String> = lines.iter().filter(|l| l.starts_with(READY_LINE)).collect();
    assert_eq!(ready.len(), 1, "{all}");
    let secs: f64 = ready[0][READY_LINE.len()..].strip_suffix(" s").expect("<s.s> s").parse().unwrap();
    // Waited from the wait line (before `announced`) until the set was
    // decoded (after `marked`): at least that gap, within a few polls of it.
    let gap = marked.duration_since(announced).as_secs_f64();
    assert!(secs + 0.06 >= gap && secs <= gap + 1.5, "waited {secs} s, marker {gap:.2} s after the wait line");
    let (held, mb, blocked) = held_line(&lines);
    assert!((1..=TILES).contains(&held), "{all}");
    assert_eq!(mb, format!("{:.1}", (held * TILE_BYTES) as f64 / 1048576.0));
    assert_eq!(blocked, None, "the default hold takes these small tiles:\n{all}");

    // Order: wait, then the decode the marker allowed, ready, held — and
    // only then any tile.
    let at = |p: &str| lines.iter().position(|l| l.starts_with(p)).unwrap_or_else(|| panic!("no {p:?}:\n{all}"));
    let held_at = lines.iter().position(|l| l.contains(" tile(s) held (")).unwrap();
    assert!(at(WAIT_LINE) < at(DECODE_START), "{all}");
    assert!(at(DECODE_START) < at(DECODED) && at(DECODED) < at(READY_LINE) && at(READY_LINE) < held_at, "{all}");
    let first_tile = lines
        .iter()
        .position(|l| l.starts_with("[Tile] ") || l.starts_with("[Info] buildings_pbf_dir:") || l.starts_with("[Rust] Progress:"))
        .unwrap();
    assert!(held_at < first_tile, "{all}");
    // Otherwise the batch's lines: every tile's, the same decode summary.
    let without_wait: Vec<String> = lines.iter().filter(|l| !l.starts_with("[Buildings] ")).cloned().collect();
    let theirs: Vec<String> = stdout
        .iter()
        .map(|l| l.replace(&format!("{:?}", dir.join("pbf")), &format!("{:?}", pbf)))
        .collect();
    let mut mine = normalized(&without_wait);
    let mut theirs = theirs;
    mine.sort();
    theirs.sort();
    assert_eq!(mine, theirs);
}

// ── 4. A hold of one tile: the workers wait ─────────────────────────────────

#[test]
fn a_hold_of_one_tile_makes_the_workers_wait_and_changes_no_byte() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let (want, _) = reference(dir);

    let pbf = dir.join("pbf_late");
    let one_tile_mb = TILE_BYTES as f64 / 1048576.0;
    let (job, out) = write_jobs(dir, "held_one", Some(&pbf), json!({
        "buildings_pbf_ready": MARKER, "buildings_hold_mb": one_tile_mb,
    }));
    let mut running = launch(&job);
    let t0 = running.t0;
    assert!(running.wait_for(WAIT_LINE).is_some(), "no wait line:\n{}", running.seen.join("\n"));
    // Time for a worker to finish a second tile, which no longer fits.
    thread::sleep(Duration::from_secs(2).saturating_sub(t0.elapsed()));
    assert!(files_in(&out).is_empty());
    write_building_tiles(&pbf);
    std::fs::write(pbf.join(MARKER), b"").unwrap();
    let (ok, lines, stderr) = running.finish();
    let all = lines.join("\n");
    assert!(ok, "{all}\n{stderr}");

    let (held, mb, blocked) = held_line(&lines);
    assert_eq!((held, mb.as_str()), (1, "0.0"), "one tile fits the hold:\n{all}");
    let blocked = blocked.unwrap_or_else(|| panic!("the next tiles must have waited on their workers:\n{all}"));
    assert!(blocked >= 1 && held + blocked <= TILES, "{all}");
    assert!(read_dir_bytes(&out) == want, ".abt bytes must equal the reference's");
    assert_eq!(count(&lines, "[Tile] "), 2 * TILES, "{all}");
    assert!(lines.contains(&format!("[Rust] Progress: {TILES}/{TILES}")), "{all}");
}

// ── 5. Marker never comes ───────────────────────────────────────────────────

#[test]
fn a_marker_that_never_comes_fails_the_batch_and_writes_no_tile() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    write_terrain(dir);
    // Every tile is there, but nobody declared the directory complete.
    let pbf = dir.join("pbf");
    write_building_tiles(&pbf);
    let (job, out) = write_jobs(dir, "never", Some(&pbf), json!({
        "buildings_pbf_ready": MARKER, "buildings_pbf_wait_s": 1,
    }));
    let t = Instant::now();
    let (ok, lines, stderr) = run(&job);
    let secs = t.elapsed().as_secs_f64();
    let all = format!("{}\n{stderr}", lines.join("\n"));
    assert!(!ok, "the batch must fail:\n{all}");
    let want = format!(
        "Error: {FAIL_TEXT}{} (buildings_pbf_ready) did not appear within 1 s (buildings_pbf_wait_s); \
         the ingest fails rather than write tiles without their buildings",
        pbf.join(MARKER).display()
    );
    assert!(stderr.lines().any(|l| l == want), "{all}");
    assert!(secs >= 1.0, "it waited 1 s first ({secs:.2} s)");
    // The tiles were sampled and held — and dropped: none written, none
    // reported.
    assert_eq!(count(&lines, WAIT_LINE), 1, "{all}");
    assert_eq!(count(&lines, READY_LINE) + count(&lines, DECODE_START), 0, "{all}");
    assert_eq!(count(&lines, "[Tile] ") + count(&lines, "[Rust] Progress:") + count(&lines, "[Error]"), 0, "{all}");
    assert!(files_in(&out).is_empty(), "no tile — nor temp file — may be left: {:?}", files_in(&out));
}

// ── 6. A directory that does not decode ─────────────────────────────────────

#[test]
fn a_directory_that_does_not_decode_after_its_marker_fails_the_batch() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    write_terrain(dir);
    let pbf = dir.join("pbf_mixed");
    let (job, out) = write_jobs(dir, "mixed", Some(&pbf), json!({ "buildings_pbf_ready": MARKER }));
    let mut running = launch(&job);
    assert!(running.wait_for(WAIT_LINE).is_some(), "no wait line:\n{}", running.seen.join("\n"));
    // Two zooms in one directory: refused when it is decoded.
    write_building_tiles(&pbf);
    std::fs::write(pbf.join("15_17300_11570.pbf"), mvt_building_tile(&[])).unwrap();
    std::fs::write(pbf.join(MARKER), b"").unwrap();
    let (ok, lines, stderr) = running.finish();
    let all = format!("{}\n{stderr}", lines.join("\n"));
    assert!(!ok, "{all}");
    assert!(
        stderr.lines().any(|l| l == "Error: buildings_pbf_dir mixes zoom levels (14 and 15); expected exactly one"),
        "{all}"
    );
    assert_eq!(count(&lines, DECODE_START), 1, "{all}");
    assert_eq!(count(&lines, DECODED) + count(&lines, READY_LINE) + count(&lines, "[Tile] "), 0, "{all}");
    assert!(files_in(&out).is_empty(), "{:?}", files_in(&out));
}

// ── 7. Bad fields fail before any tile ──────────────────────────────────────

#[test]
fn bad_marker_fields_fail_before_any_tile() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    write_terrain(dir);
    // Never created: a check that read it would fail differently.
    let pbf = dir.join("pbf");
    let cases = [
        (json!({ "buildings_pbf_ready": MARKER }), None, "buildings_pbf_ready \"_ready\" needs buildings_pbf_dir"),
        (json!({ "buildings_pbf_ready": "" }), Some(&pbf), "buildings_pbf_ready must be a file name inside buildings_pbf_dir, got \"\""),
        (json!({ "buildings_pbf_ready": "sub/_ready" }), Some(&pbf), "must be a file name inside buildings_pbf_dir"),
        (json!({ "buildings_pbf_ready": ".." }), Some(&pbf), "must be a file name inside buildings_pbf_dir"),
        (json!({ "buildings_pbf_ready": "14_1_2.pbf" }), Some(&pbf), "is named like a building tile"),
        (json!({ "buildings_pbf_ready": MARKER, "buildings_pbf_wait_s": -1 }), Some(&pbf), "buildings_pbf_wait_s must be a number of seconds from 0 to 86400, got -1"),
        (json!({ "buildings_pbf_ready": MARKER, "buildings_hold_mb": -1 }), Some(&pbf), "buildings_hold_mb must be a number of megabytes from 0 to 1048576, got -1"),
        (json!({ "buildings_pbf_ready": MARKER, "buildings_hold_mb": 1e7 }), Some(&pbf), "buildings_hold_mb must be a number of megabytes from 0 to 1048576, got 10000000"),
    ];
    for (i, (extra, dir_of, msg)) in cases.into_iter().enumerate() {
        let (job, out) = write_jobs(dir, &format!("bad{i}"), dir_of.map(PathBuf::as_path), extra);
        let (ok, lines, stderr) = run(&job);
        let all = format!("{}\n{stderr}", lines.join("\n"));
        assert!(!ok, "case {i} must fail:\n{all}");
        let first = dir.join(format!("bad{i}")).join("t00.abt");
        let err = stderr.lines().find(|l| l.starts_with("Error: ")).unwrap_or_else(|| panic!("case {i}:\n{all}"));
        assert!(err.starts_with(&format!("Error: ingest job for {first:?}: ")) && err.contains(msg), "case {i}: want {msg:?}:\n{all}");
        assert!(!out.exists(), "case {i}: nothing is written");
        assert_eq!(count(&lines, "[Rust] Batch processing"), 0, "case {i}: refused before the batch starts:\n{all}");
    }
    // One directory, two hand-overs: half the jobs give the marker.
    let (job, out) = write_jobs(dir, "disagree", Some(&pbf), json!({}));
    let mut jobs: Vec<serde_json::Value> = serde_json::from_slice(&std::fs::read(&job).unwrap()).unwrap();
    for j in jobs.iter_mut().step_by(2) {
        j["buildings_pbf_ready"] = json!(MARKER);
    }
    std::fs::write(&job, serde_json::to_vec(&jobs).unwrap()).unwrap();
    let (ok, lines, stderr) = run(&job);
    assert!(!ok, "{}\n{stderr}", lines.join("\n"));
    assert!(
        stderr.contains(&format!(
            "Error: buildings_pbf_dir {pbf:?}: the jobs for {:?} and {:?} give it different \
             buildings_pbf_ready / buildings_pbf_wait_s (marker \"_ready\", 1800 s against no marker)",
            out.join("t00.abt"),
            out.join("t01.abt")
        )),
        "{stderr}"
    );
    assert!(!out.exists());
    assert!(!pbf.exists());
}

// ── 8. The single-job form refuses the field ────────────────────────────────

#[test]
fn a_single_job_refuses_the_marker_and_ignores_the_other_fields() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let (want, _) = reference(dir);
    let (batch, _) = write_jobs(dir, "single", Some(&dir.join("pbf")), json!({}));
    let jobs: Vec<serde_json::Value> = serde_json::from_slice(&std::fs::read(&batch).unwrap()).unwrap();

    // A single job decodes its directory before its tile, as it finds it:
    // the hand-over is refused before anything is read.
    std::fs::write(dir.join("pbf").join(MARKER), b"").unwrap();
    let mut one = jobs[0].clone();
    one["buildings_pbf_ready"] = json!(MARKER);
    let job = dir.join("single_marked.json");
    std::fs::write(&job, serde_json::to_vec(&one).unwrap()).unwrap();
    let (ok, lines, stderr) = run(&job);
    assert!(!ok, "{}\n{stderr}", lines.join("\n"));
    assert!(
        stderr.lines().any(|l| l.starts_with(
            "Error: buildings_pbf_ready \"_ready\" is read by a batch ingest only (a JSON array of jobs)"
        )),
        "{stderr}"
    );
    assert!(!dir.join("single").exists(), "nothing is written");

    // `buildings_pbf_wait_s` / `buildings_hold_mb` alone are not read: the
    // tile is the batch's.
    let mut one = jobs[0].clone();
    one["buildings_pbf_wait_s"] = json!(-1);
    one["buildings_hold_mb"] = json!(-1);
    std::fs::write(&job, serde_json::to_vec(&one).unwrap()).unwrap();
    let (ok, lines, stderr) = run(&job);
    assert!(ok, "{}\n{stderr}", lines.join("\n"));
    let got = std::fs::read(dir.join("single").join("t00.abt")).unwrap();
    assert!(got == want[0].1, "the single job's tile must be the batch's");
}
