//! CLI `download`, changelog 28: the parent fill fetches each ancestor once
//! per run, every source tile is capped at one wall-clock deadline across all
//! its attempts, and `repair_source_tiles` patches listed tiles into the
//! existing `.abt` files.
//!
//! Contract surface: `docs/CONTRACT.md` §1.2, §1.5, §9b, changelog 28. The
//! tile source is a loopback HTTP server that counts every request, so these
//! tests are hermetic and assert on what went over the wire.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::{Ipv4Addr, TcpListener};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use aether_converter::download::{lat2ty, lon2tx, tx2lon, ty2lat};

const Z: u32 = 12;

enum Reply {
    Png(Vec<u8>),
    Status(u16),
    /// 200 with a huge Content-Length, then one byte a second, forever: a
    /// body whose gaps never reach the 15 s read-idle timeout.
    Trickle,
}

type Counts = Arc<Mutex<HashMap<(u32, u32, u32), usize>>>;

/// Loopback tile server; `route(z, x, y)` answers, every request is counted.
fn serve<F>(route: F) -> (u16, Counts)
where
    F: Fn(u32, u32, u32) -> Reply + Send + Sync + 'static,
{
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let route = Arc::new(route);
    let counts: Counts = Arc::default();
    let c2 = counts.clone();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let (route, counts) = (route.clone(), c2.clone());
            thread::spawn(move || {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut request = String::new();
                if reader.read_line(&mut request).unwrap_or(0) == 0 { return; }
                let mut line = String::new();
                while reader.read_line(&mut line).unwrap_or(0) > 0 && line != "\r\n" { line.clear(); }
                let path = request.split_whitespace().nth(1).unwrap_or("/").to_owned();
                let zxy: Vec<u32> = path.trim_start_matches('/').trim_end_matches(".png")
                    .split('/').filter_map(|p| p.parse().ok()).collect();
                if zxy.len() != 3 { return; }
                *counts.lock().unwrap().entry((zxy[0], zxy[1], zxy[2])).or_insert(0) += 1;
                match route(zxy[0], zxy[1], zxy[2]) {
                    Reply::Png(body) => {
                        let _ = write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\
                                                Connection: close\r\n\r\n", body.len());
                        let _ = stream.write_all(&body);
                    }
                    Reply::Status(code) => {
                        let _ = write!(stream, "HTTP/1.1 {code} X\r\nContent-Length: 0\r\n\
                                                Connection: close\r\n\r\n");
                    }
                    Reply::Trickle => {
                        let _ = write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: 100000000\r\n\
                                                Connection: close\r\n\r\n");
                        for _ in 0..3600 {
                            if stream.write_all(&[0u8]).is_err() || stream.flush().is_err() { return; }
                            thread::sleep(Duration::from_secs(1));
                        }
                    }
                }
            });
        }
    });
    (port, counts)
}

/// A 256 px Terrarium PNG whose sample `(px, py)` is `f(px, py)` metres
/// (integers, so the encoding is exact).
fn terrarium(f: impl Fn(usize, usize) -> i32) -> Vec<u8> {
    let mut px = Vec::with_capacity(256 * 256 * 3);
    for py in 0..256 {
        for x in 0..256 {
            let t = (f(x, py) + 32768) as u32;
            px.extend_from_slice(&[(t >> 8) as u8, (t & 255) as u8, 0]);
        }
    }
    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, 256, 256);
        enc.set_color(png::ColorType::Rgb);
        enc.set_depth(png::BitDepth::Eight);
        enc.write_header().unwrap().write_image_data(&px).unwrap();
    }
    out
}

/// Real-looking terrain for tile `(z, x, y)`: unique per tile and varying
/// per sample, so a misplaced or wrongly upsampled sample shows.
fn ground(z: u32, x: u32, y: u32) -> impl Fn(usize, usize) -> i32 {
    let base = 200 + (z as i32) * 300 + ((x % 9) as i32) * 37 + ((y % 11) as i32) * 23;
    move |px, py| base + (px as i32 * 3) % 97 + (py as i32 * 5) % 89
}

/// The top-left z12 tile of the test area, aligned to 4 so a 4 × 4 block
/// shares one z10 ancestor (and one z9 … z6).
fn origin() -> (u32, u32) {
    (lon2tx(8.0, Z) & !3, lat2ty(47.0, Z) & !3)
}

/// `rows` rows of square `.abt` specs, each inside one tile row (5 % inset
/// top and bottom) and spanning `cols` tiles (0.1 % inset left and right).
fn write_job(dir: &Path, port: u16, cols: u32, rows: u32, extra: serde_json::Value) -> (PathBuf, PathBuf) {
    let (tx0, ty0) = origin();
    let size = 64u32;
    let mut tiles = Vec::new();
    for r in 0..rows {
        let (n, s) = (ty2lat(ty0 + r, Z), ty2lat(ty0 + r + 1, Z));
        let w = tx2lon(tx0 + cols, Z) - tx2lon(tx0, Z);
        let per = (w / ((n - s) * 0.9)).ceil() as u32;
        let pd = w * 0.998 / (per * size) as f64;
        for c in 0..per {
            tiles.push(serde_json::json!({
                "filename": format!("r{r:02}_c{c:02}.abt"),
                "ul_lat": n - (n - s) * 0.05,
                "ul_lon": tx2lon(tx0, Z) + w * 0.001 + (c * size) as f64 * pd,
                "size_px": size,
                "resolution_m": pd * 111_111.0,
            }));
        }
    }
    let out_dir = dir.join("out");
    let mut job = serde_json::json!({
        "url_template": format!("http://127.0.0.1:{port}/{{z}}/{{x}}/{{y}}.png"),
        "encoding": "terrarium", "output_dir": out_dir, "zoom": Z,
        "max_connections": 32, "tiles": tiles,
    });
    if let (Some(j), Some(e)) = (job.as_object_mut(), extra.as_object()) {
        for (k, v) in e { j.insert(k.clone(), v.clone()); }
    }
    let path = dir.join("job.json");
    std::fs::write(&path, serde_json::to_vec_pretty(&job).unwrap()).unwrap();
    (path, out_dir)
}

/// Run the real binary; kill it after `limit` (a run that never ends is a
/// failure to report, not a hung test suite). Returns (finished ok, stderr, secs).
fn run_cli(job: &Path, limit: Duration) -> (bool, String, f64) {
    let t0 = Instant::now();
    let mut child = Command::new(env!("CARGO_BIN_EXE_aether_converter"))
        .args(["download", "--job-file"]).arg(job)
        .stdout(Stdio::null()).stderr(Stdio::piped()).spawn().unwrap();
    let mut err = child.stderr.take().unwrap();
    let reader = thread::spawn(move || {
        let mut s = String::new();
        let _ = std::io::Read::read_to_string(&mut err, &mut s);
        s
    });
    let ok = loop {
        if let Some(st) = child.try_wait().unwrap() { break st.success(); }
        if t0.elapsed() > limit {
            let _ = child.kill();
            let _ = child.wait();
            let s = reader.join().unwrap();
            panic!("download still running after {:?} — killed. stderr:\n{s}", limit);
        }
        thread::sleep(Duration::from_millis(50));
    };
    (ok, reader.join().unwrap(), t0.elapsed().as_secs_f64())
}

fn read_dir_bytes(dir: &Path) -> Vec<(String, Vec<u8>)> {
    let mut v: Vec<_> = std::fs::read_dir(dir).unwrap().filter_map(|e| e.ok())
        .map(|e| (e.file_name().to_string_lossy().into_owned(), std::fs::read(e.path()).unwrap()))
        .collect();
    v.sort();
    v
}

fn count(c: &Counts, z: u32) -> Vec<usize> {
    let mut v: Vec<usize> = c.lock().unwrap().iter().filter(|(k, _)| k.0 == z).map(|(_, n)| *n).collect();
    v.sort();
    v
}

// ── 1. Ancestor de-duplication ──────────────────────────────────────────────

#[test]
fn each_ancestor_is_fetched_once_for_all_its_descendants() {
    // 4 × 4 z12 tiles, every one 404 (a bounded source that has no z12 here),
    // z11 dead too (503, the degraded path's ancestor failure), z10 live.
    // Each tile used to fetch all six of its ancestors itself: 16 requests
    // for the four z11 parents, 16 for the one z10 ancestor, 16 each for
    // z9…z6. Now every distinct ancestor is asked exactly once, the dead z11
    // ones included.
    let dir = tempfile::tempdir().unwrap();
    let (port, counts) = serve(|z, x, y| match z {
        12 => Reply::Status(404),
        11 => Reply::Status(503),
        10 => Reply::Png(terrarium(ground(z, x, y))),
        _ => Reply::Status(404),
    });
    let (job, _) = write_job(dir.path(), port, 4, 4, serde_json::json!({}));
    let (ok, err, _) = run_cli(&job, Duration::from_secs(120));
    assert!(ok, "{err}");
    assert!(err.lines().any(|l| l == "[Stats] Tiles: 16/16 OK (100.0% success)"), "{err}");
    assert!(err.lines().any(|l| l.starts_with("[Stats] Repaired from coarser parent tiles: 16")), "{err}");
    assert_eq!(count(&counts, 12), vec![1; 16], "each z12 tile once (404 is final)");
    assert_eq!(count(&counts, 11), vec![1; 4], "four dead z11 parents, each asked once");
    for z in 6..=10 {
        assert_eq!(count(&counts, z), vec![1], "one z{z} ancestor, asked once");
    }
}

#[test]
fn a_tile_cut_from_a_shared_ancestor_is_byte_identical_to_that_terrain_served_directly() {
    // Run A: z12 all 404, z11 serves terrain — every tile is cut from its
    // (shared, once-fetched) parent. Run B: the server hands out exactly
    // those cut-outs as z12 tiles. The `.abt` bytes must be identical: the
    // shared cache changes how often an ancestor is fetched, not one pixel.
    let (cols, rows) = (4u32, 2u32);
    let upsampled = |x: u32, y: u32| {
        let parent = ground(11, x >> 1, y >> 1);
        move |px: usize, py: usize| {
            let apx = ((x as usize * 256 + px) >> 1) - (x as usize >> 1) * 256;
            let apy = ((y as usize * 256 + py) >> 1) - (y as usize >> 1) * 256;
            parent(apx, apy)
        }
    };
    let mut outs = Vec::new();
    for direct in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let (port, counts) = serve(move |z, x, y| match (z, direct) {
            (12, true) => Reply::Png(terrarium(upsampled(x, y))),
            (12, false) => Reply::Status(404),
            (11, false) => Reply::Png(terrarium(ground(11, x, y))),
            _ => Reply::Status(404),
        });
        let (job, out) = write_job(dir.path(), port, cols, rows, serde_json::json!({}));
        let (ok, err, _) = run_cli(&job, Duration::from_secs(120));
        assert!(ok, "{err}");
        if !direct {
            assert_eq!(count(&counts, 11), vec![1; 2], "two z11 parents for 4 × 2 tiles, once each");
        }
        outs.push(read_dir_bytes(&out));
    }
    assert!(!outs[0].is_empty());
    assert_eq!(outs[0].len(), outs[1].len());
    for ((na, a), (nb, b)) in outs[0].iter().zip(&outs[1]) {
        assert_eq!(na, nb);
        assert!(a == b, "{na}: parent-filled bytes differ from the same terrain served directly");
    }
}

// ── 2. Per-tile deadline ────────────────────────────────────────────────────

#[test]
fn a_trickling_tile_is_cut_at_the_job_deadline_and_reported_as_timeout() {
    // One tile trickles a byte a second — never idle 15 s, so the read-idle
    // timeout never fires and the tile used to hold the run forever. With
    // `tile_deadline_s: 4` it stops at 4 s as a timeout; no ancestor exists,
    // so it is an ordinary (not degraded, not fatal) failure.
    let dir = tempfile::tempdir().unwrap();
    let (tx0, ty0) = origin();
    let (port, _) = serve(move |z, x, y| match (z, x, y) {
        (12, x, y) if (x, y) == (tx0 + 1, ty0) => Reply::Trickle,
        (12, x, y) => Reply::Png(terrarium(ground(12, x, y))),
        _ => Reply::Status(404),
    });
    let (job, _) = write_job(dir.path(), port, 4, 1, serde_json::json!({"tile_deadline_s": 4}));
    let (ok, err, secs) = run_cli(&job, Duration::from_secs(60));
    assert!(ok, "{err}");
    assert!(secs < 15.0, "a 4 s cap took {secs:.1}s. stderr:\n{err}");
    assert!(err.lines().any(|l| l == "[Stats] Tiles: 3/4 OK (75.0% success)"), "{err}");
    assert!(err.lines().any(|l| l.starts_with("[Stats] ERRORS (1): timeout=1")), "{err}");
    assert!(err.lines().any(|l| l == format!("[Download] first timeout: z={Z}/x={}/y={ty0}", tx0 + 1)),
            "{err}");
}

#[test]
fn the_default_deadline_frees_a_stalled_tile_within_a_minute_and_degrades_it() {
    // Same trickle, no job field: the built-in 60 s cap applies. Its z11
    // parent is live, so the tile is filled and reported DEGRADED with
    // reason=timeout — not stuck, not silently OK.
    let dir = tempfile::tempdir().unwrap();
    let (tx0, ty0) = origin();
    let slow = (tx0 + 1, ty0);
    let (port, _) = serve(move |z, x, y| match (z, x, y) {
        (12, x, y) if (x, y) == slow => Reply::Trickle,
        (12, x, y) => Reply::Png(terrarium(ground(12, x, y))),
        (11, x, y) if (x, y) == (slow.0 >> 1, slow.1 >> 1) => Reply::Png(terrarium(ground(11, x, y))),
        _ => Reply::Status(404),
    });
    let (job, _) = write_job(dir.path(), port, 4, 1, serde_json::json!({}));
    let (ok, err, secs) = run_cli(&job, Duration::from_secs(150));
    assert!(ok, "{err}");
    assert!((55.0..80.0).contains(&secs), "want the 60 s cap (+ fill); took {secs:.1}s. stderr:\n{err}");
    let line = format!("[Degraded] z={Z} x={} y={} reason=timeout filled_from=z11", slow.0, slow.1);
    assert!(err.lines().any(|l| l == line), "want `{line}`; stderr:\n{err}");
    assert!(err.lines().any(|l| l == "[Stats] Tiles: 3/4 OK (75.0% success)"), "{err}");
}

#[test]
fn a_bad_tile_deadline_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let (port, counts) = serve(|z, x, y| Reply::Png(terrarium(ground(z, x, y))));
    let (job, _) = write_job(dir.path(), port, 1, 1, serde_json::json!({"tile_deadline_s": 0}));
    let (ok, err, _) = run_cli(&job, Duration::from_secs(60));
    assert!(!ok, "{err}");
    assert!(err.contains("tile_deadline_s must be a positive number of seconds"), "{err}");
    assert!(counts.lock().unwrap().is_empty(), "refused before any request");
}

// ── 3. repair_source_tiles ──────────────────────────────────────────────────

/// Output pixel `(x, y)` of spec `s`: its longitude span and centre latitude.
struct Px { lon0: f64, lon1: f64, lat: f64 }

fn pixels(spec: &serde_json::Value) -> (usize, f64, f64, f64) {
    let pd = spec["resolution_m"].as_f64().unwrap() / 111_111.0;
    (spec["size_px"].as_u64().unwrap() as usize, spec["ul_lat"].as_f64().unwrap(),
     spec["ul_lon"].as_f64().unwrap(), pd)
}

#[test]
fn a_repair_rewrites_only_the_listed_tiles_footprint_with_fresh_download_bytes() {
    let (cols, rows) = (4u32, 2u32);
    let (tx0, ty0) = origin();
    // Listed: two neighbours in row 0 (their shared border is inside the
    // repair) and one tile in row 1.
    let listed = [(tx0 + 1, ty0), (tx0 + 2, ty0), (tx0 + 3, ty0 + 1)];

    // "fresh": a full download from a healthy source.
    let fresh_dir = tempfile::tempdir().unwrap();
    let (good, _) = serve(|z, x, y| Reply::Png(terrarium(ground(z, x, y))));
    let (job, out) = write_job(fresh_dir.path(), good, cols, rows, serde_json::json!({}));
    let (ok, err, _) = run_cli(&job, Duration::from_secs(120));
    assert!(ok, "{err}");
    let fresh = read_dir_bytes(&out);

    // "old": the same job, the listed tiles 403 and filled from z11 (degraded).
    let dir = tempfile::tempdir().unwrap();
    let (bad, _) = serve(move |z, x, y| {
        if z == 12 && listed.contains(&(x, y)) { Reply::Status(403) }
        else { Reply::Png(terrarium(ground(z, x, y))) }
    });
    let (job, out) = write_job(dir.path(), bad, cols, rows, serde_json::json!({}));
    let (ok, err, _) = run_cli(&job, Duration::from_secs(120));
    assert!(ok, "{err}");
    assert_eq!(err.lines().filter(|l| l.starts_with("[Degraded]")).count(), 3, "{err}");
    let old = read_dir_bytes(&out);

    // Repair against the healthy source, into the old files.
    let (good2, counts) = serve(|z, x, y| Reply::Png(terrarium(ground(z, x, y))));
    let list: Vec<[u32; 3]> = listed.iter().map(|&(x, y)| [Z, x, y]).collect();
    let (rjob, rout) = write_job(dir.path(), good2, cols, rows,
                                 serde_json::json!({"repair_source_tiles": list}));
    assert_eq!(rout, out);
    let (ok, err, _) = run_cli(&rjob, Duration::from_secs(120));
    assert!(ok, "{err}");
    assert!(err.lines().any(|l| l == "[Stats] Tiles: 3/3 OK (100.0% success)"), "{err}");
    let mut fetched: Vec<(u32, u32, u32)> = counts.lock().unwrap().keys().copied().collect();
    fetched.sort();
    let mut want: Vec<(u32, u32, u32)> = listed.iter().map(|&(x, y)| (Z, x, y)).collect();
    want.sort();
    assert_eq!(fetched, want, "only the listed tiles are fetched");
    let repaired = read_dir_bytes(&out);

    // Classify every pixel by its footprint and check it against old/fresh.
    let jobv: serde_json::Value = serde_json::from_slice(&std::fs::read(&rjob).unwrap()).unwrap();
    let specs = jobv["tiles"].as_array().unwrap();
    let in_listed = |lon: f64, lat: f64| {
        listed.iter().any(|&(x, y)| lon >= tx2lon(x, Z) && lon < tx2lon(x + 1, Z)
                                   && lat <= ty2lat(y, Z) && lat > ty2lat(y + 1, Z))
    };
    let (mut inside, mut outside, mut border, mut border_fresh) = (0, 0, 0, 0);
    for (si, spec) in specs.iter().enumerate() {
        let name = spec["filename"].as_str().unwrap();
        let (o, f, r) = (&old[si], &fresh[si], &repaired[si]);
        assert_eq!((o.0.as_str(), f.0.as_str(), r.0.as_str()), (name, name, name));
        let (o, f, r) = (&o.1, &f.1, &r.1);
        assert_eq!(r.len(), o.len());
        assert_eq!(r[..44], o[..44], "{name}: header untouched");
        let (sz, ul_lat, ul_lon, pd) = pixels(spec);
        let stride = (sz * 2 + 255) & !255;
        for y in 0..sz {
            let row = 44 + y * stride;
            assert_eq!(r[row + sz * 2..row + stride], o[row + sz * 2..row + stride], "{name}: padding");
            for x in 0..sz {
                let p = Px { lon0: ul_lon + x as f64 * pd, lon1: ul_lon + (x + 1) as f64 * pd,
                             lat: ul_lat - (y as f64 + 0.5) * pd };
                let eps = pd * 1e-6;
                let a = in_listed(p.lon0 + eps, p.lat);
                let b = in_listed(p.lon1 - eps, p.lat);
                let i = row + x * 2;
                let (rv, ov, fv) = (&r[i..i + 2], &o[i..i + 2], &f[i..i + 2]);
                if a && b {
                    inside += 1;
                    assert_eq!(rv, fv, "{name} ({x},{y}): inside the repair, want the fresh bytes");
                } else if !a && !b {
                    outside += 1;
                    assert_eq!(rv, ov, "{name} ({x},{y}): outside the repair, want the old bytes");
                } else {
                    border += 1;
                    assert!(rv == ov || rv == fv, "{name} ({x},{y}): border pixel is old or fresh");
                    if rv == fv && rv != ov { border_fresh += 1; }
                }
            }
        }
    }
    assert!(inside > 1000 && outside > 1000, "inside={inside} outside={outside}");
    // Old and fresh really differ inside (degraded z11 vs z12 terrain), so
    // `inside` matching fresh is the repair, not a coincidence.
    let differs = specs.iter().enumerate().any(|(si, _)| old[si].1 != fresh[si].1);
    assert!(differs);
    eprintln!("inside={inside} outside={outside} border={border} (fresh {border_fresh})");
}

#[test]
fn a_repair_into_a_missing_output_file_is_an_error_and_fetches_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let (tx0, ty0) = origin();
    let (port, counts) = serve(|z, x, y| Reply::Png(terrarium(ground(z, x, y))));
    let (job, out) = write_job(dir.path(), port, 2, 1,
                               serde_json::json!({"repair_source_tiles": [[Z, tx0, ty0]]}));
    let (ok, err, _) = run_cli(&job, Duration::from_secs(60));
    assert!(!ok, "a repair needs the files of a finished download; stderr:\n{err}");
    assert!(err.contains("repair_source_tiles: output tile") && err.contains("does not exist"), "{err}");
    assert!(counts.lock().unwrap().is_empty(), "validated before any request");
    assert!(!out.exists() || std::fs::read_dir(&out).unwrap().next().is_none(), "nothing created");
}

#[test]
fn a_repair_tile_outside_the_job_or_at_another_zoom_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let (tx0, ty0) = origin();
    let (port, counts) = serve(|z, x, y| Reply::Png(terrarium(ground(z, x, y))));
    let (job, _) = write_job(dir.path(), port, 2, 1, serde_json::json!({}));
    let (ok, err, _) = run_cli(&job, Duration::from_secs(60));
    assert!(ok, "{err}");
    counts.lock().unwrap().clear();
    for (list, want) in [
        (serde_json::json!([[Z, tx0 + 50, ty0]]), "does not intersect any output tile of this job"),
        (serde_json::json!([[Z - 1, tx0 >> 1, ty0 >> 1]]), "is not at the job's zoom"),
        (serde_json::json!([]), "repair_source_tiles is empty"),
    ] {
        let (rjob, _) = write_job(dir.path(), port, 2, 1, serde_json::json!({"repair_source_tiles": list}));
        let (ok, err, _) = run_cli(&rjob, Duration::from_secs(60));
        assert!(!ok, "{list}: {err}");
        assert!(err.contains(want), "{list}: want `{want}`; stderr:\n{err}");
    }
    assert!(counts.lock().unwrap().is_empty(), "refused before any request");
}
