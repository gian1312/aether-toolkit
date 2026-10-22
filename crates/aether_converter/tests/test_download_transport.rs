//! How the CLI `download` moves bytes: retries decided by error type,
//! read-idle (not total) timeouts, STALL reporting, the adaptive connection
//! window, strips assembled in completion order, the per-strip diagnostic
//! lines' sink and the open-file limit.
//!
//! Contract surface: `docs/CONTRACT.md` §1.2 (`[Download] STALL`, `[Stats]
//! STALL`, `[Stats] Connection window`, `[Stats] Tile latency`), §9b
//! (`max_connections`), changelog 27 and 36. The tile source is a loopback
//! HTTP server, so these tests are hermetic.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::{Ipv4Addr, TcpListener};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use aether_converter::download::{lat2ty, lon2tx, tx2lon, ty2lat};

/// What the server does with one request for tile `(x, y)`, the `n`-th
/// request for that tile (0-based).
enum Reply {
    Png(Vec<u8>, u64),
    Close,
    Status(u16),
}

fn serve<F>(route: F) -> u16
where
    F: Fn(u32, u32, usize) -> Reply + Send + Sync + 'static,
{
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let route = Arc::new(route);
    let seen: Arc<Mutex<HashMap<(u32, u32), usize>>> = Arc::default();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let (route, seen) = (route.clone(), seen.clone());
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
                let n = {
                    let mut s = seen.lock().unwrap();
                    let c = s.entry((zxy[1], zxy[2])).or_insert(0);
                    *c += 1;
                    *c - 1
                };
                let (status, body) = match route(zxy[1], zxy[2], n) {
                    Reply::Close => return,
                    Reply::Status(code) => (format!("{code} X"), Vec::new()),
                    Reply::Png(png, delay_ms) => {
                        thread::sleep(Duration::from_millis(delay_ms));
                        ("200 OK".to_string(), png)
                    }
                };
                let _ = write!(stream, "HTTP/1.1 {status}\r\nContent-Length: {}\r\n\
                                        Connection: close\r\n\r\n", body.len());
                let _ = stream.write_all(&body);
            });
        }
    });
    port
}

/// A constant-elevation 256 px Terrarium tile, encoded once per value (the
/// tests run unoptimised, and encoding inside the server would swamp the
/// delays the strip-order test relies on).
fn tile(elev_m: f64) -> Vec<u8> {
    static CACHE: Mutex<Option<HashMap<u64, Vec<u8>>>> = Mutex::new(None);
    let key = elev_m.to_bits();
    if let Some(t) = CACHE.lock().unwrap().get_or_insert_with(HashMap::new).get(&key) {
        return t.clone();
    }
    let t = encode_tile(elev_m);
    CACHE.lock().unwrap().as_mut().unwrap().insert(key, t.clone());
    t
}

fn encode_tile(elev_m: f64) -> Vec<u8> {
    let t = elev_m + 32768.0;
    let (r, g) = ((t / 256.0).floor(), t % 256.0);
    let px: Vec<u8> = [r as u8, g as u8, 0].iter().copied().cycle().take(256 * 256 * 3).collect();
    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, 256, 256);
        enc.set_color(png::ColorType::Rgb);
        enc.set_depth(png::BitDepth::Eight);
        enc.write_header().unwrap().write_image_data(&px).unwrap();
    }
    out
}

const Z: u32 = 12;

/// Elevation of tile `(x, y)`: unique per tile, so a misplaced tile shows.
fn elev(x: u32, y: u32) -> f64 { ((x % 7) * 100 + (y % 50) * 2) as f64 }

/// `rows` stacked `.abt` specs, each one tile row tall and `cols` tiles wide
/// (inset half a pixel so a spec never touches the next tile row). The job's
/// fetch rectangle is then `cols × rows` tiles — `rows > 64` gives 3 strips.
fn stacked_job(dir: &Path, port: u16, cols: u32, rows: u32, conns: usize) -> (PathBuf, PathBuf, u32, u32) {
    let (tx0, ty0) = (lon2tx(8.0, Z), lat2ty(47.0, Z));
    let size = 16u32;
    let mut tiles = Vec::new();
    for r in 0..rows {
        let (n, s) = (ty2lat(ty0 + r, Z), ty2lat(ty0 + r + 1, Z));
        let w = tx2lon(tx0 + cols, Z) - tx2lon(tx0, Z);
        // Square specs side by side spanning `cols` tiles exactly (0.1 % inset
        // each side), each at most 90 % of a tile row tall.
        let per = (w / ((n - s) * 0.9)).ceil() as u32;
        let pd = w * 0.998 / (per * size) as f64;
        for c in 0..per {
            tiles.push(serde_json::json!({
                "filename": format!("r{r:03}_c{c:03}.abt"),
                "ul_lat": n - (n - s) * 0.05,
                "ul_lon": tx2lon(tx0, Z) + w * 0.001 + (c * size) as f64 * pd,
                "size_px": size,
                "resolution_m": pd * 111_111.0,
            }));
        }
    }
    let out_dir = dir.join("out");
    let job = serde_json::json!({
        "url_template": format!("http://127.0.0.1:{port}/{{z}}/{{x}}/{{y}}.png"),
        "encoding": "terrarium", "output_dir": out_dir, "zoom": Z,
        "max_connections": conns, "tiles": tiles,
    });
    let path = dir.join("job.json");
    std::fs::write(&path, serde_json::to_vec_pretty(&job).unwrap()).unwrap();
    (path, out_dir, tx0, ty0)
}

fn run_cli(job: &Path) -> (bool, String) {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_aether_converter"))
        .args(["download", "--job-file"]).arg(job).output().unwrap();
    (out.status.success(), String::from_utf8_lossy(&out.stderr).into_owned())
}

fn read_dir_bytes(dir: &Path) -> Vec<(String, Vec<u8>)> {
    let mut v: Vec<_> = std::fs::read_dir(dir).unwrap().filter_map(|e| e.ok())
        .map(|e| (e.file_name().to_string_lossy().into_owned(), std::fs::read(e.path()).unwrap()))
        .collect();
    v.sort();
    v
}

#[test]
fn strips_assembled_in_completion_order_write_the_same_bytes() {
    // 2 × 70 tiles = 3 strips. Run A delays tiles by their row, so strips
    // finish in index order; run B delays the FIRST rows most, so strip 3
    // finishes first and is assembled first. The output must not change.
    let (cols, rows) = (2u32, 70u32);
    let mut outs = Vec::new();
    for reverse in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let (tx0, ty0) = (lon2tx(8.0, Z), lat2ty(47.0, Z));
        for x in tx0..tx0 + cols { for y in ty0..ty0 + rows { tile(elev(x, y)); } }
        let port = serve(move |x, y, _| {
            let r = (y - ty0) as u64;
            let delay = if reverse { (rows as u64 - 1 - r) * 40 } else { r * 40 };
            Reply::Png(tile(elev(x, y)), delay)
        });
        let (job, out_dir, tx0, _) = stacked_job(dir.path(), port, cols, rows, 256);
        let (ok, err) = run_cli(&job);
        assert!(ok, "{err}");
        assert!(err.contains("3 strips"), "{err}");
        let first_asm = err.lines().find(|l| l.starts_with("[Strip ") && l.contains("setup="))
            .unwrap_or_default().to_owned();
        assert!(first_asm.starts_with(if reverse { "[Strip 3/3]" } else { "[Strip 1/3]" }),
                "assembly order (reverse={reverse}): {first_asm}\n{err}");
        let files = read_dir_bytes(&out_dir);
        // Spot-check placement: row 40's first pixel is tile (tx0, ty0+40).
        let (_, b) = files.iter().find(|(n, _)| n == "r040_c000.abt").unwrap();
        let v = i16::from_le_bytes([b[44], b[45]]);
        assert_eq!(v as f64, elev(tx0, ty0 + 40) * 2.0, "half-metres of tile (tx0, ty0+40)");
        outs.push(files);
    }
    assert_eq!(outs[0].len(), outs[1].len());
    for ((na, a), (nb, b)) in outs[0].iter().zip(&outs[1]) {
        assert_eq!(na, nb);
        assert!(a == b, "{na} differs between in-order and completion-order assembly");
    }
}

#[test]
fn connection_resets_on_a_png_url_are_retried_not_degraded() {
    // Every tile's first request is dropped unanswered ("connection closed
    // before message completed"). The URL ends in .png — which the old
    // message-substring rule read as a decode error and never retried, so
    // each reset became a [Degraded] tile and a full plugin re-download.
    let dir = tempfile::tempdir().unwrap();
    let port = serve(|x, y, n| if n == 0 { Reply::Close } else { Reply::Png(tile(elev(x, y)), 0) });
    let (job, _, _, _) = stacked_job(dir.path(), port, 2, 2, 16);
    let (ok, err) = run_cli(&job);
    assert!(ok, "{err}");
    assert!(!err.contains("[Degraded]"), "{err}");
    assert!(err.lines().any(|l| l == "[Stats] Tiles: 4/4 OK (100.0% success)"), "{err}");
    assert!(err.lines().any(|l| l == "[Stats] Retries: 4 (up to 5 per failed tile)"), "{err}");
}

#[test]
fn a_tile_waiting_past_five_seconds_is_a_stall_and_the_window_is_reported() {
    let dir = tempfile::tempdir().unwrap();
    let ty0 = lat2ty(47.0, Z);
    let tx0 = lon2tx(8.0, Z);
    let port = serve(move |x, y, _| {
        let d = if (x, y) == (tx0, ty0) { 6000 } else { 0 };
        Reply::Png(tile(elev(x, y)), d)
    });
    let (job, _, _, _) = stacked_job(dir.path(), port, 2, 2, 16);
    let (ok, err) = run_cli(&job);
    assert!(ok, "{err}");
    let stall = format!("[Download] STALL tile z={Z}/x={tx0}/y={ty0}: first byte after 6.");
    assert!(err.lines().any(|l| l.starts_with(&stall)), "want `{stall}…`; stderr:\n{err}");
    assert!(err.lines().any(|l| l.starts_with("[Stats] STALL: 1 of 4 tile(s) waited > 5 s")), "{err}");
    assert!(!err.contains("SLOW"), "{err}");
    // Changelog 36: the 6 s that tile waited for its response head shows in
    // the mean first-byte time of the run's 4 attempts (>= 6000 / 4 ms).
    let lat = err.lines().find(|l| l.starts_with("[Stats] Tile latency: "))
        .unwrap_or_else(|| panic!("no latency line; stderr:\n{err}"));
    let fb: u64 = lat.split(" first-byte avg=").nth(1).and_then(|v| v.strip_suffix("ms"))
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| panic!("want `… first-byte avg=<n>ms`: {lat}"));
    assert!(fb >= 1400, "{lat}");
    assert!(err.lines().any(|l| l.starts_with("[Stats] Connection window: start 16, min 16, final 16 ")),
            "a clean run never leaves its configured window; stderr:\n{err}");
}

#[test]
fn a_429_storm_shrinks_the_window_and_the_run_still_completes() {
    // 8 × 8 tiles at 64 connections; each tile is refused twice with 429.
    let dir = tempfile::tempdir().unwrap();
    let port = serve(|x, y, n| if n < 2 { Reply::Status(429) } else { Reply::Png(tile(elev(x, y)), 0) });
    let (job, _, _, _) = stacked_job(dir.path(), port, 8, 8, 64);
    let (ok, err) = run_cli(&job);
    assert!(ok, "{err}");
    assert!(err.lines().any(|l| l == "[Stats] Tiles: 64/64 OK (100.0% success)"), "{err}");
    let line = err.lines().find(|l| l.starts_with("[Stats] Connection window: start 64, min "))
        .unwrap_or_else(|| panic!("no window line; stderr:\n{err}"));
    let min: usize = line.split("min ").nth(1).unwrap().split(',').next().unwrap().parse().unwrap();
    assert!(min < 64, "the window must have backed off: {line}");
}

#[test]
fn strip_lines_go_to_the_diag_file_and_the_parsed_lines_stay_on_stderr() {
    // Changelog 36/37: the per-strip `[Strip k/n] …` lines are diagnostics —
    // with AETHER_DIAG_FILE set they go to that file; the progress, `[Stats]`
    // and one-per-run `[Download]` lines stay on stderr for their parsers.
    let dir = tempfile::tempdir().unwrap();
    let port = serve(|x, y, _| Reply::Png(tile(elev(x, y)), 0));
    let (job, _, _, _) = stacked_job(dir.path(), port, 2, 2, 16);
    let diag = dir.path().join("diag.log");
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_aether_converter"))
        .env("AETHER_DIAG_FILE", &diag)
        .args(["download", "--job-file"]).arg(&job).output().unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{err}");
    let file = std::fs::read_to_string(&diag).unwrap_or_default();
    assert!(file.lines().any(|l| l.starts_with("[Strip 1/1] setup=")), "diag file:\n{file}\nstderr:\n{err}");
    assert!(!err.contains("[Strip "), "{err}");
    assert!(err.lines().any(|l| l.starts_with("[Download] 100% (4/4) — ")), "{err}");
    assert!(err.lines().any(|l| l == "[Stats] Tiles: 4/4 OK (100.0% success)"), "{err}");
    assert!(err.lines().any(|l| l.starts_with("[Download] RAM budget: ")), "{err}");
}

#[cfg(unix)]
#[test]
fn a_low_open_file_limit_is_raised_or_the_connections_are_fitted_under_it() {
    // 150 tiles, each answered after 300 ms, at 100 connections: about 100
    // sockets open at once. (1) Soft limit 64, hard limit as it is: the
    // converter raises its own soft limit and runs clean — no retries (each
    // EMFILE would have been a failed connect, retried). (2) Hard limit 100
    // as well, so nothing can be raised: it runs (100 - 64) / 2 = 18
    // connections, says so, and still runs clean. (Kept small: this test's
    // own server holds a socket per connection, and macOS starts a test
    // process at a soft limit of 256.)
    let dir = tempfile::tempdir().unwrap();
    let (cols, rows) = (10u32, 15u32);
    let (tx0, ty0) = (lon2tx(8.0, Z), lat2ty(47.0, Z));
    for x in tx0..tx0 + cols { for y in ty0..ty0 + rows { tile(elev(x, y)); } }
    let port = serve(|x, y, _| Reply::Png(tile(elev(x, y)), 300));
    let (job, _, _, _) = stacked_job(dir.path(), port, cols, rows, 100);
    let capped = "[Download] open-file limit 100: 2 connections instead of max_connections 100 \
                  (296 descriptors would not fit)";
    for (ulimit, want_cap) in [("-S -n 64", false), ("-n 100", true)] {
        let out = std::process::Command::new("sh")
            .arg("-c").arg(format!("ulimit {ulimit} && exec \"$0\" download --job-file \"$1\""))
            .arg(env!("CARGO_BIN_EXE_aether_converter")).arg(&job)
            .output().unwrap();
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "ulimit {ulimit}: {err}");
        assert!(err.lines().any(|l| l == "[Stats] Tiles: 150/150 OK (100.0% success)"), "ulimit {ulimit}: {err}");
        assert!(err.lines().any(|l| l == "[Stats] Retries: 0 (up to 5 per failed tile)"), "ulimit {ulimit}: {err}");
        assert_eq!(err.lines().any(|l| l == capped), want_cap, "ulimit {ulimit}: {err}");
        let start = if want_cap { 2 } else { 100 };
        assert!(err.lines().any(|l| l.starts_with(&format!("[Stats] Connection window: start {start}, "))),
                "ulimit {ulimit}: {err}");
    }
}
