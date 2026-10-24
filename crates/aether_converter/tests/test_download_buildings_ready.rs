//! CLI `download` with `buildings_pbf_ready` (CONTRACT §9b, changelog 45): a
//! caller fetches the building tiles WHILE the download runs and declares the
//! directory complete with a marker file. The run reads nothing in the
//! directory before its buildings post-pass, waits there for the marker
//! (announcing the wait and its end), burns exactly what a job whose
//! directory was complete at launch burns, and fails — never runs without
//! buildings — when the marker does not come within `buildings_pbf_wait_s`.
//! A job without the field is unchanged.
//!
//! Hermetic: terrain comes from a loopback XYZ server (`common::serve_tiles`),
//! the building tiles are MVT written by the test.

mod common;

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use aether_converter::download::{lat2ty, lon2tx};
use aether_converter::mvt::{Feature, Layer, Tile, Value};
use common::{read_dir_bytes, serve_tiles, terrarium_png, tile_ground, TileCounts};
use prost::Message;
use serde_json::json;

/// Terrain source zoom.
const Z: u32 = 12;
/// Zoom of the building tiles (OpenFreeMap's).
const BZ: u32 = 14;
const MARKER: &str = "_ready";
const WAIT_LINE: &str = "[Buildings] waiting for the building tiles (";
const READY_LINE: &str = "[Buildings] building tiles ready after ";
/// The post-pass's success line; the hand-over must not change it.
const BURN_LINE: &str = "pbf tile(s) → ";
const FAIL_TEXT: &str = "Building tiles not ready: the marker file ";
/// A run that does not end by then is killed and reported, never hung.
const LIMIT: Duration = Duration::from_secs(120);

// ── Building tiles ──────────────────────────────────────────────────────────

fn zz(n: i32) -> u32 {
    ((n << 1) ^ (n >> 31)) as u32
}

/// MVT command stream of one closed ring.
fn ring_geometry(r: &[(i32, i32)]) -> Vec<u32> {
    let mut out = vec![(1 << 3) | 1, zz(r[0].0), zz(r[0].1)];
    out.push((((r.len() - 1) as u32) << 3) | 2);
    let mut c = r[0];
    for &p in &r[1..] {
        out.extend([zz(p.0 - c.0), zz(p.1 - c.1)]);
        c = p;
    }
    out.push((1 << 3) | 7);
    out
}

/// A `building` layer: one rectangle (clockwise with y down) per
/// `(render_height, (x0, y0, x1, y1))`, in tile units.
fn building_tile(features: &[(f64, (i32, i32, i32, i32))]) -> Vec<u8> {
    let mut values = Vec::new();
    let mut feats = Vec::new();
    for (i, (h, (x0, y0, x1, y1))) in features.iter().enumerate() {
        values.push(Value { double_val: Some(*h), ..Default::default() });
        feats.push(Feature {
            id: Some(i as u64 + 1),
            tags: vec![0, i as u32],
            r#type: Some(3),
            geometry: ring_geometry(&[(*x0, *y0), (*x1, *y0), (*x1, *y1), (*x0, *y1)]),
        });
    }
    Tile {
        layers: vec![Layer {
            name: "building".into(),
            features: feats,
            keys: vec!["render_height".into()],
            values,
            extent: Some(4096),
            version: Some(2),
        }],
    }
    .encode_to_vec()
}

// ── The job ─────────────────────────────────────────────────────────────────

/// Two 128 px output tiles at 20 m, side by side.
const PD: f64 = 20.0 / 111_111.0;
const UL: (f64, f64) = (47.01, 8.003); // (lat, lon)

fn specs() -> Vec<serde_json::Value> {
    (0..2)
        .map(|i| {
            json!({
                "filename": format!("t{i}.abt"),
                "ul_lat": UL.0, "ul_lon": UL.1 + (i * 128) as f64 * PD,
                "size_px": 128, "resolution_m": 20.0,
            })
        })
        .collect()
}

/// Writes one building tile per z14 tile under the outputs — a 4 × 4 block
/// of houses of varying size and height each — into *dir* (created).
/// Returns how many tiles it wrote.
fn write_building_tiles(dir: &Path) -> usize {
    std::fs::create_dir_all(dir).unwrap();
    let (x0, x1) = (lon2tx(UL.1, BZ), lon2tx(UL.1 + 256.0 * PD, BZ));
    let (y0, y1) = (lat2ty(UL.0, BZ), lat2ty(UL.0 - 128.0 * PD, BZ));
    let mut n = 0;
    for y in y0..=y1 {
        for x in x0..=x1 {
            let mut features = Vec::new();
            for j in 0..4 {
                for i in 0..4 {
                    let (bx, by) = (300 + i * 950, 250 + j * 950);
                    let h = 8.0 + ((x * 7 + y * 5 + (i * 4 + j) as u32 * 3) % 33) as f64;
                    features.push((h, (bx, by, bx + 300 + i * 37, by + 260 + j * 41)));
                }
            }
            std::fs::write(dir.join(format!("{BZ}_{x}_{y}.pbf")), building_tile(&features)).unwrap();
            n += 1;
        }
    }
    assert!(n >= 4, "the outputs must span several building tiles, got {n}");
    n
}

/// The loopback terrain source: every tile at every zoom, unique per tile
/// and per sample.
fn terrain_server() -> (u16, TileCounts) {
    serve_tiles(|z, x, y| Some(terrarium_png(tile_ground(z, x, y))))
}

/// Writes `<dir>/<name>.json`, output directory `<dir>/<name>`, with the
/// *extra* fields merged in.
fn write_job(dir: &Path, port: u16, name: &str, extra: serde_json::Value) -> (PathBuf, PathBuf) {
    let out_dir = dir.join(name);
    let mut job = json!({
        "url_template": format!("http://127.0.0.1:{port}/{{z}}/{{x}}/{{y}}.png"),
        "encoding": "terrarium", "output_dir": out_dir, "zoom": Z,
        "max_connections": 16, "tiles": specs(),
    });
    if let (Some(j), Some(e)) = (job.as_object_mut(), extra.as_object()) {
        for (k, v) in e {
            j.insert(k.clone(), v.clone());
        }
    }
    let path = dir.join(format!("{name}.json"));
    std::fs::write(&path, serde_json::to_vec_pretty(&job).unwrap()).unwrap();
    (path, out_dir)
}

// ── Running the CLI ─────────────────────────────────────────────────────────

/// A running CLI `download` whose stderr lines are read as they are printed.
struct Running {
    child: Child,
    lines: mpsc::Receiver<(Instant, String)>,
    t0: Instant,
    seen: Vec<String>,
}

fn launch(job: &Path) -> Running {
    let t0 = Instant::now();
    let mut child = Command::new(env!("CARGO_BIN_EXE_aether_converter"))
        .args(["download", "--job-file"])
        .arg(job)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let err = child.stderr.take().unwrap();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        for line in BufReader::new(err).lines().map_while(Result::ok) {
            if tx.send((Instant::now(), line)).is_err() {
                break;
            }
        }
    });
    Running { child, lines: rx, t0, seen: Vec::new() }
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

    /// Wait for the exit (kill after [`LIMIT`]): `(exit ok, every stderr line)`.
    fn finish(mut self) -> (bool, Vec<String>) {
        let ok = loop {
            if let Some(st) = self.child.try_wait().unwrap() {
                break st.success();
            }
            if self.t0.elapsed() > LIMIT {
                let _ = self.child.kill();
                let _ = self.child.wait();
                panic!("download still running after {LIMIT:?} — killed. stderr:\n{}", self.seen.join("\n"));
            }
            while let Ok((_, l)) = self.lines.try_recv() {
                self.seen.push(l);
            }
            thread::sleep(Duration::from_millis(20));
        };
        // The reader ends at EOF, after the exit.
        self.seen.extend(self.lines.iter().map(|(_, l)| l));
        (ok, self.seen)
    }
}

fn run(job: &Path) -> (bool, Vec<String>) {
    launch(job).finish()
}

fn count(lines: &[String], needle: &str) -> usize {
    lines.iter().filter(|l| l.contains(needle)).count()
}

/// The post-pass's `[Buildings] … pbf tile(s) → …` line without its timing.
fn burn_line(lines: &[String]) -> String {
    let burns: Vec<&String> = lines.iter().filter(|l| l.starts_with("[Buildings] ") && l.contains(BURN_LINE)).collect();
    assert_eq!(burns.len(), 1, "one burn line expected:\n{}", lines.join("\n"));
    burns[0].rsplit_once(" in ").expect("the burn line ends in its timing").0.to_owned()
}

/// The reference: the same job without the field, its building directory
/// complete at launch — today's post-pass. Checks that it burnt something
/// (its bytes differ from the terrain alone), so equal bytes below mean the
/// same burn. Returns its `.abt` files, its burn line and the number of
/// building tiles.
fn reference(dir: &Path, port: u16) -> (Vec<(String, Vec<u8>)>, String, usize) {
    let pbf = dir.join("pbf_reference");
    let tiles = write_building_tiles(&pbf);
    let (job, out) = write_job(dir, port, "reference", json!({ "buildings_pbf_dir": pbf }));
    let (ok, lines) = run(&job);
    assert!(ok, "reference run failed:\n{}", lines.join("\n"));
    assert_eq!(count(&lines, WAIT_LINE) + count(&lines, READY_LINE), 0, "{}", lines.join("\n"));
    let with = read_dir_bytes(&out);
    assert_eq!(with.len(), 2);

    let (job, out) = write_job(dir, port, "terrain_only", json!({}));
    let (ok, lines_t) = run(&job);
    assert!(ok, "terrain-only run failed:\n{}", lines_t.join("\n"));
    let without = read_dir_bytes(&out);
    for ((name, a), (_, b)) in with.iter().zip(&without) {
        assert_ne!(a, b, "{name}: the buildings must have raised pixels");
        assert_eq!(a.len(), b.len());
    }
    (with, burn_line(&lines), tiles)
}

// ── 1. Without the field: unchanged ─────────────────────────────────────────

#[test]
fn a_job_without_the_marker_burns_its_directory_as_before() {
    let dir = tempfile::tempdir().unwrap();
    let (port, _) = terrain_server();
    let (want, burn, tiles) = reference(dir.path(), port);
    // Today's line, with today's counts: every tile read, 16 houses each.
    assert!(
        burn.starts_with(&format!("[Buildings] {tiles} pbf tile(s) → {} building(s) (0 dup dropped) → 2/2 abt tile(s), ", tiles * 16)),
        "{burn}"
    );

    // `buildings_pbf_wait_s` alone is not read: the same run, the same bytes.
    let pbf = dir.path().join("pbf_wait_only");
    write_building_tiles(&pbf);
    let (job, out) = write_job(dir.path(), port, "wait_only", json!({
        "buildings_pbf_dir": pbf, "buildings_pbf_wait_s": 0.5,
    }));
    let (ok, lines) = run(&job);
    assert!(ok, "{}", lines.join("\n"));
    assert_eq!(count(&lines, WAIT_LINE) + count(&lines, READY_LINE), 0, "{}", lines.join("\n"));
    assert_eq!(burn_line(&lines), burn);
    assert!(read_dir_bytes(&out) == want, "a job without buildings_pbf_ready must burn exactly as before");
}

// ── 2. Marker already there at launch ───────────────────────────────────────

#[test]
fn a_marker_present_at_launch_burns_at_once_with_the_same_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let (port, _) = terrain_server();
    let (want, burn, _) = reference(dir.path(), port);

    let pbf = dir.path().join("pbf");
    write_building_tiles(&pbf);
    std::fs::write(pbf.join(MARKER), b"").unwrap();
    let (job, out) = write_job(dir.path(), port, "marked", json!({
        "buildings_pbf_dir": pbf, "buildings_pbf_ready": MARKER,
    }));
    let (ok, lines) = run(&job);
    assert!(ok, "{}", lines.join("\n"));
    assert_eq!(count(&lines, WAIT_LINE), 0, "no wait when the marker is there:\n{}", lines.join("\n"));
    assert_eq!(count(&lines, READY_LINE), 0, "{}", lines.join("\n"));
    assert_eq!(burn_line(&lines), burn, "the marker file is not read as a building tile");
    assert!(read_dir_bytes(&out) == want, ".abt bytes must equal the reference's");
}

// ── 3. Marker written while the job runs ────────────────────────────────────

#[test]
fn a_marker_written_after_launch_is_waited_for_and_burns_the_same_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let (port, _) = terrain_server();
    let (want, burn, _) = reference(dir.path(), port);

    // The directory does not exist at launch: the job must not look at it
    // before the burn stage (an early check would fail the run).
    let pbf = dir.path().join("pbf_late");
    let marker = pbf.join(MARKER);
    let (job, out) = write_job(dir.path(), port, "late", json!({
        "buildings_pbf_dir": pbf, "buildings_pbf_ready": MARKER, "buildings_pbf_wait_s": 60,
    }));
    let mut running = launch(&job);
    let t0 = running.t0;
    // The test's fetch: 2 s after launch — and never before the job says it
    // waits, so the wait is certain however slow this machine is — the tiles,
    // then the marker.
    let (go, wait_seen) = mpsc::channel::<()>();
    let writer = {
        let (pbf, marker) = (pbf.clone(), marker.clone());
        thread::spawn(move || {
            thread::sleep(Duration::from_secs(2).saturating_sub(t0.elapsed()));
            wait_seen.recv().expect("the job announced its wait");
            write_building_tiles(&pbf);
            std::fs::write(&marker, b"").unwrap();
            Instant::now()
        })
    };
    let announced = running.wait_for(WAIT_LINE);
    let announced = announced.unwrap_or_else(|| panic!("no wait line:\n{}", running.seen.join("\n")));
    go.send(()).unwrap();
    let marked = writer.join().unwrap();
    let (ok, lines) = running.finish();
    let all = lines.join("\n");
    assert!(ok, "{all}");

    assert_eq!(count(&lines, WAIT_LINE), 1, "{all}");
    assert!(lines.contains(&format!("{WAIT_LINE}{})", marker.display())), "{all}");
    let ready: Vec<&String> = lines.iter().filter(|l| l.starts_with(READY_LINE)).collect();
    assert_eq!(ready.len(), 1, "{all}");
    let secs: f64 = ready[0][READY_LINE.len()..].strip_suffix(" s").expect("<s.s> s").parse().unwrap();
    // The job waited from its announcement (before `announced`) until a poll
    // after `marked`: at least that gap, and within a few polls of it.
    let gap = marked.duration_since(announced).as_secs_f64();
    assert!(secs + 0.06 >= gap && secs <= gap + 1.5, "waited {secs} s, marker {gap:.2} s after the wait line");
    assert!(marked.duration_since(t0) >= Duration::from_secs(2));

    // Order: wait, ready, then the burn — the same burn as the reference's.
    let at = |needle: &str| lines.iter().position(|l| l.contains(needle)).unwrap();
    assert!(at(WAIT_LINE) < at(READY_LINE) && at(READY_LINE) < at(BURN_LINE), "{all}");
    assert_eq!(burn_line(&lines), burn);
    assert!(read_dir_bytes(&out) == want, ".abt bytes must equal the reference's");
}

// ── 4. Marker never comes ───────────────────────────────────────────────────

#[test]
fn a_marker_that_never_comes_fails_the_run_without_burning() {
    let dir = tempfile::tempdir().unwrap();
    let (port, _) = terrain_server();
    // Every tile is there, but nobody declared the directory complete.
    let pbf = dir.path().join("pbf");
    write_building_tiles(&pbf);
    let (job, out) = write_job(dir.path(), port, "never", json!({
        "buildings_pbf_dir": pbf, "buildings_pbf_ready": MARKER, "buildings_pbf_wait_s": 1,
    }));
    let t = Instant::now();
    let (ok, lines) = run(&job);
    let secs = t.elapsed().as_secs_f64();
    let all = lines.join("\n");
    assert!(!ok, "the run must fail:\n{all}");

    assert!(lines.contains(&format!("{WAIT_LINE}{})", pbf.join(MARKER).display())), "{all}");
    let want = format!(
        "{FAIL_TEXT}{} (buildings_pbf_ready) did not appear within 1 s (buildings_pbf_wait_s); \
         the download fails rather than leave terrain without its buildings",
        pbf.join(MARKER).display()
    );
    assert!(lines.iter().any(|l| l == &format!("Error: {want}")), "{all}");
    assert_eq!(count(&lines, READY_LINE), 0, "{all}");
    assert_eq!(count(&lines, BURN_LINE), 0, "no burn line on a failed wait:\n{all}");
    assert!(secs >= 1.0, "it waited 1 s first ({secs:.2} s)");
    // A failed run leaves no terrain to pool as "with buildings".
    assert!(lines.iter().any(|l| l.starts_with("[Download] Removed 2 incomplete .abt file(s)")), "{all}");
    assert!(read_dir_bytes(&out).is_empty(), "the outputs must be removed");
}

// ── 5. Bad fields fail before anything happens ──────────────────────────────

#[test]
fn bad_marker_fields_fail_before_any_request() {
    let dir = tempfile::tempdir().unwrap();
    let (port, counts) = terrain_server();
    let pbf = dir.path().join("pbf");
    let cases = [
        (json!({ "buildings_pbf_ready": MARKER }), "buildings_pbf_ready \"_ready\" needs buildings_pbf_dir"),
        (json!({ "buildings_pbf_dir": pbf, "buildings_pbf_ready": "" }), "buildings_pbf_ready must be a file name inside buildings_pbf_dir, got \"\""),
        (json!({ "buildings_pbf_dir": pbf, "buildings_pbf_ready": "sub/_ready" }), "must be a file name inside buildings_pbf_dir"),
        (json!({ "buildings_pbf_dir": pbf, "buildings_pbf_ready": "..\\_ready" }), "must be a file name inside buildings_pbf_dir"),
        (json!({ "buildings_pbf_dir": pbf, "buildings_pbf_ready": ".." }), "must be a file name inside buildings_pbf_dir"),
        (json!({ "buildings_pbf_dir": pbf, "buildings_pbf_ready": "14_1_2.pbf" }), "is named like a building tile"),
        (json!({ "buildings_pbf_dir": pbf, "buildings_pbf_ready": MARKER, "buildings_pbf_wait_s": -1 }), "buildings_pbf_wait_s must be a number of seconds from 0 to 86400, got -1"),
        (json!({ "buildings_pbf_dir": pbf, "buildings_pbf_ready": MARKER, "buildings_pbf_wait_s": 1e6 }), "buildings_pbf_wait_s must be a number of seconds from 0 to 86400"),
    ];
    for (i, (extra, msg)) in cases.into_iter().enumerate() {
        let (job, out) = write_job(dir.path(), port, &format!("bad{i}"), extra);
        let (ok, lines) = run(&job);
        let all = lines.join("\n");
        assert!(!ok, "case {i} must fail:\n{all}");
        assert!(lines.iter().any(|l| l.starts_with("Error: ") && l.contains(msg)), "case {i}: want {msg:?}:\n{all}");
        assert!(!out.exists(), "case {i}: nothing is written");
    }
    assert!(counts.lock().unwrap().is_empty(), "no request may go out");
    assert!(!pbf.exists());
}

// ── 6. Repair jobs ignore the field ─────────────────────────────────────────

#[test]
fn a_repair_job_does_not_wait_for_the_marker() {
    let dir = tempfile::tempdir().unwrap();
    let (port, _) = terrain_server();
    let pbf = dir.path().join("pbf");
    write_building_tiles(&pbf);
    std::fs::write(pbf.join(MARKER), b"").unwrap();
    let marked = json!({ "buildings_pbf_dir": pbf, "buildings_pbf_ready": MARKER });
    let (job, out) = write_job(dir.path(), port, "out", marked.clone());
    let (ok, lines) = run(&job);
    assert!(ok, "{}", lines.join("\n"));
    let before = read_dir_bytes(&out);

    // The marker is gone, yet the repair of one source tile under the
    // outputs neither waits nor fails: it reads the directory as it is.
    std::fs::remove_file(pbf.join(MARKER)).unwrap();
    let (tx, ty) = (lon2tx(UL.1 + 10.0 * PD, Z), lat2ty(UL.0 - 10.0 * PD, Z));
    let mut extra = marked;
    extra["buildings_pbf_wait_s"] = json!(0);
    extra["repair_source_tiles"] = json!([[Z, tx, ty]]);
    let (job, _) = write_job(dir.path(), port, "out", extra);
    let (ok, lines) = run(&job);
    let all = lines.join("\n");
    assert!(ok, "{all}");
    assert_eq!(count(&lines, WAIT_LINE) + count(&lines, READY_LINE), 0, "{all}");
    assert_eq!(count(&lines, "[Download] repair: "), 2, "{all}");
    assert_eq!(count(&lines, "[Buildings] repair: "), 1, "the repair burns at once:\n{all}");
    assert_eq!(read_dir_bytes(&out).len(), before.len());
}
