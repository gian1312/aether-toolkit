//! Honest memory budgets (CONTRACT §1.2, changelog 52; informational lines,
//! none parsed). A CLI `download`'s `[Download] RAM budget: <mb> MB` is what
//! the whole process takes — strips, assembly, connections, decoding, the
//! allocator's share and the base — so its `[Stats] Peak RSS:` lands near
//! it; a pass whose share cannot hold its connections beside the rest runs
//! fewer (`[Download] connections <a> → <b>: …`), never fails and writes the
//! same bytes. A batch `ingest` counts the building set it decodes: the
//! `[Rust] Rayon Pool:` line carries its estimate from the tiles' bytes on
//! disk, fewer threads run for it, `[Rust] Building tiles decoded: …` ends
//! with the set's memory, and a set decoded late that leaves room for fewer
//! tiles lowers the tiles sampled at once (`[Rust] Tiles at once: …`).
//!
//! Hermetic: terrain from a loopback XYZ server (`common::serve_tiles`), the
//! building tiles are copies of the OpenFreeMap fixture tile, and the
//! machine's available memory is imposed through `AETHER_TEST_AVAILABLE_MB`
//! (tests only) so the figures do not depend on the machine's RAM.

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;

use aether_converter::download::{lat2ty, lon2tx, tx2lon, ty2lat};
use common::{read_dir_bytes, serve_tiles, terrarium_png, tile_ground};
use serde_json::json;

const Z: u32 = 12;
const AVAIL_VAR: &str = "AETHER_TEST_AVAILABLE_MB";

/// Run the CLI `<sub> --job-file <job>` with `AETHER_TEST_AVAILABLE_MB` set to
/// *avail_mb* (or removed); `(exit ok, stdout, stderr)`.
fn run(sub: &str, job: &Path, avail_mb: Option<u64>) -> (bool, String, String) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_aether_converter"));
    cmd.args([sub, "--job-file"]).arg(job).env_remove("AETHER_DIAG_FILE");
    match avail_mb {
        Some(mb) => cmd.env(AVAIL_VAR, mb.to_string()),
        None => cmd.env_remove(AVAIL_VAR),
    };
    let out = cmd.output().expect("spawn aether_converter");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// The number after *prefix* up to *end* in the first line that has it.
fn number_after(text: &str, prefix: &str, end: &str) -> Option<f64> {
    text.lines().find_map(|l| {
        let rest = l.split_once(prefix)?.1;
        rest.split_once(end)?.0.trim().parse().ok()
    })
}

/// A z12 terrain server: every tile at Z, nothing else.
fn terrain() -> u16 {
    serve_tiles(|z, x, y| (z == Z).then(|| terrarium_png(tile_ground(z, x, y)))).0
}

/// A download job of *specs* (`(name, ul_lat, ul_lon, size_px, side_deg)`).
fn download_job(dir: &Path, name: &str, port: u16, conns: usize, specs: &[(String, f64, f64, u32, f64)]) -> (PathBuf, PathBuf) {
    download_job_with(dir, name, port, conns, specs, json!({}))
}

/// [`download_job`] with the fields of *extra* on top.
fn download_job_with(
    dir: &Path,
    name: &str,
    port: u16,
    conns: usize,
    specs: &[(String, f64, f64, u32, f64)],
    extra: serde_json::Value,
) -> (PathBuf, PathBuf) {
    let out = dir.join(format!("{name}_out"));
    let tiles: Vec<serde_json::Value> = specs
        .iter()
        .map(|(f, lat, lon, px, side)| {
            json!({ "filename": f, "ul_lat": lat, "ul_lon": lon, "size_px": px,
                    "resolution_m": side / *px as f64 * 111_111.0 })
        })
        .collect();
    let mut job = json!({
        "url_template": format!("http://127.0.0.1:{port}/{{z}}/{{x}}/{{y}}.png"),
        "encoding": "terrarium", "output_dir": out, "zoom": Z,
        "max_connections": conns, "tiles": tiles,
    });
    for (k, v) in extra.as_object().unwrap() {
        job[k] = v.clone();
    }
    let path = dir.join(format!("{name}.json"));
    std::fs::write(&path, serde_json::to_vec_pretty(&job).unwrap()).unwrap();
    (path, out)
}

// ── download ────────────────────────────────────────────────────────────────

#[test]
fn a_256_connection_download_peaks_within_15_percent_of_its_printed_budget() {
    // 146 squares of 0.24° side by side along 47° N, 35° of longitude: a
    // band ~400 z12 tiles wide and 5 deep, ~2,000 tiles (~500 MB decoded)
    // beside a 100 MB assembly grid — a pass whose overhead a budget of the
    // strips alone would leave out (it would be ~25 % short). The whole job
    // is one strip, so every tile is held until the strip is in: the pass
    // reaches its budget whatever the machine's speed. 8 GB "available": a
    // 2048 MB share, on any machine.
    let dir = tempfile::tempdir().unwrap();
    let port = terrain();
    let specs: Vec<_> = (0..146)
        .map(|i| (format!("s{i:03}.abt"), 47.0, -15.0 + 0.24 * i as f64, 64u32, 0.24))
        .collect();
    let (job, _) = download_job(dir.path(), "big", port, 256, &specs);
    let (ok, _, err) = run("download", &job, Some(8192));
    assert!(ok, "{err}");
    let header = err.lines().find(|l| l.starts_with("[Download] z=")).unwrap();
    println!("{header}");
    let budget_line = err.lines().find(|l| l.starts_with("[Download] RAM budget: ")).unwrap();
    println!("{budget_line}");
    // The breakdown follows the line's unchanged text.
    assert!(budget_line.contains(" GB available at start; fixed for the run) (strips "), "{budget_line}");
    assert!(budget_line.contains(" + assembly 1 × "), "one strip, one assembly: {budget_line}");
    assert!(budget_line.contains(" + 256 connections "), "{budget_line}");
    assert!(!err.contains("[Download] connections "), "a 2048 MB share holds 256 connections:\n{err}");
    let budget = number_after(&err, "[Download] RAM budget: ", " MB").unwrap();
    let peak_line = err.lines().find(|l| l.starts_with("[Stats] Peak RSS: ")).unwrap();
    println!("{peak_line}");
    let peak = number_after(&err, "[Stats] Peak RSS: ", " MB").unwrap();
    assert!(peak_line.ends_with(&format!(" MB (RAM budget {budget} MB)")), "{peak_line}");
    let flight: Option<usize> =
        err.lines().find_map(|l| l.strip_prefix("[Stats] Peak concurrent requests: ")?.trim().parse().ok());
    println!("peak concurrent requests: {flight:?}");
    assert!(flight.is_some_and(|f| f > 64), "the connections were used: {flight:?}\n{err}");
    // The point: the process peaks at what the line said, within 15 % —
    // and not at what the strips alone hold, which it exceeds by more.
    let ratio = peak / budget;
    assert!((0.85..=1.15).contains(&ratio), "peak {peak} MB against a budget of {budget} MB ({ratio:.3})");
    let strips = number_after(budget_line, "(strips ", " MB").unwrap();
    assert!(peak > 1.15 * strips, "the strips alone ({strips} MB) are not the peak ({peak} MB)");
}

/// The specs of a pass 180 z12 tiles wide and 2 tile-rows deep: 90 squares
/// of 16 px, each over 2 × 2 tiles, along one latitude.
fn wide_flat_specs() -> Vec<(String, f64, f64, u32, f64)> {
    let x0 = lon2tx(-10.0, Z);
    let y0 = lat2ty(47.0, Z);
    // Each square starts in the middle of tile (x0 + 2i, y0) and, 0.7 of a
    // tile's width on a side, reaches into the next column (half a width
    // away) and the next row (a row is 0.68 of a width tall here) — not
    // beyond either.
    let side = 0.7 * (tx2lon(x0 + 1, Z) - tx2lon(x0, Z));
    let mid = (ty2lat(y0, Z) + ty2lat(y0 + 1, Z)) / 2.0;
    (0..90)
        .map(|i| {
            let x = x0 + 2 * i;
            (format!("w{i:02}.abt"), mid, (tx2lon(x, Z) + tx2lon(x + 1, Z)) / 2.0, 16u32, side)
        })
        .collect()
}

#[test]
fn a_memory_short_download_runs_fewer_connections_and_writes_the_same_bytes() {
    // 180 tiles wide: two assembly grids of 45 MB and a strips' floor of
    // two 45 MB tile-rows. 1000 MB "available" leaves a 256 MB share (the
    // floor of min(available / 4, 2 GiB)), which cannot hold 256
    // connections beside them: the pass runs fewer, says so once, and
    // succeeds. The bytes are a run's with the machine's own memory.
    let dir = tempfile::tempdir().unwrap();
    let port = terrain();
    let specs = wide_flat_specs();
    let (job, out_short) = download_job(dir.path(), "short", port, 256, &specs);
    let (ok, _, err) = run("download", &job, Some(1000));
    assert!(ok, "{err}");
    let shrink: Vec<&str> = err.lines().filter(|l| l.starts_with("[Download] connections ")).collect();
    assert_eq!(shrink.len(), 1, "one line:\n{err}");
    println!("{}", shrink[0]);
    let to: usize = shrink[0]
        .strip_prefix("[Download] connections 256 → ")
        .and_then(|r| r.split_once(": 256 MB available for this pass; the assembly (2 × "))
        .and_then(|(n, _)| n.parse().ok())
        .unwrap_or_else(|| panic!("{}", shrink[0]));
    assert!((16..256).contains(&to), "{}", shrink[0]);
    // Before the budget line, which prices the connections it kept.
    let at = |p: &str| err.lines().position(|l| l.starts_with(p)).unwrap();
    assert!(at("[Download] connections ") < at("[Download] RAM budget: "), "{err}");
    let budget_line = err.lines().find(|l| l.starts_with("[Download] RAM budget: ")).unwrap();
    println!("{budget_line}");
    assert!(budget_line.contains(&format!(" + {to} connections ")), "{budget_line}");
    // The window runs at the new ceiling: it starts there, never above it.
    let window = err.lines().find(|l| l.starts_with("[Stats] Connection window: ")).unwrap();
    assert!(window.starts_with(&format!("[Stats] Connection window: start {to}, ")), "{window}");
    assert!(window.ends_with(&format!("..={to}, 0 back-off(s))")), "{window}");
    let flight: usize = err
        .lines()
        .find_map(|l| l.strip_prefix("[Stats] Peak concurrent requests: ")?.trim().parse().ok())
        .unwrap();
    assert!(flight <= to, "{flight} requests at once with a ceiling of {to}");
    assert!(err.contains("[Stats] Tiles: 360/360 OK"), "{err}");

    // The machine's own memory: the bytes do not depend on the budget.
    let (job, out_free) = download_job(dir.path(), "free", port, 256, &specs);
    let (ok, _, err) = run("download", &job, None);
    assert!(ok, "{err}");
    let (a, b) = (read_dir_bytes(&out_short), read_dir_bytes(&out_free));
    assert_eq!(a.len(), 90);
    assert!(a == b, "the memory-short pass wrote other bytes");
}

#[test]
fn a_download_prices_its_buildings_post_pass_when_its_tiles_are_there_and_says_when_they_are_not() {
    // A small terrain pass (one 64 px tile) whose post-pass decodes 200
    // copies of the fixture tile (12.1 MB on disk, ~36 MB expected): the
    // post-pass is the larger phase, and the budget is its price.
    let dir = tempfile::tempdir().unwrap();
    let port = terrain();
    let store = dir.path().join("pbf");
    let names = building_store(&store, 200);
    let specs = vec![("b.abt".to_string(), 47.0, 8.0, 64u32, 0.05)];
    let (job, _) = download_job_with(dir.path(), "there", port, 16, &specs, json!({ "buildings_pbf_dir": store }));
    let (ok, _, err) = run("download", &job, Some(8192));
    assert!(ok, "{err}");
    let line = err.lines().find(|l| l.starts_with("[Download] RAM budget: ")).unwrap();
    println!("{line}");
    let budget = number_after(line, "[Download] RAM budget: ", " MB").unwrap();
    let post = number_after(line, "; buildings post-pass ", " MB)").unwrap_or_else(|| panic!("{line}"));
    assert_eq!(budget, post, "the post-pass is the larger phase: {line}");
    // 200 × 60,516 bytes × 3, at the decode's peak (× 1.4), with the
    // allocator's share and the base: well above the terrain's few MB.
    assert!(post > 60.0, "{line}");
    assert!(err.contains("[Buildings] 200 pbf tile(s) → "), "{err}");

    // The list comes once the budget is drawn — the run waits for it, as
    // for the plugin's — so there is nothing to estimate then, and the line
    // says so.
    let list = dir.path().join("late.txt");
    let (job, _) = download_job_with(
        dir.path(),
        "later",
        port,
        16,
        &specs,
        json!({ "buildings_pbf_dir": store, "buildings_pbf_list": list, "buildings_pbf_wait_s": 60 }),
    );
    let mut child = Command::new(env!("CARGO_BIN_EXE_aether_converter"))
        .args(["download", "--job-file"])
        .arg(&job)
        .env(AVAIL_VAR, "8192")
        .env_remove("AETHER_DIAG_FILE")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn aether_converter");
    let mut err = String::new();
    for l in std::io::BufRead::lines(std::io::BufReader::new(child.stderr.take().unwrap())) {
        let l = l.unwrap();
        if l.starts_with("[Download] RAM budget: ") {
            let part = list.with_extension("part");
            std::fs::write(&part, names.join("\n")).unwrap();
            std::fs::rename(&part, &list).unwrap();
        }
        err.push_str(&l);
        err.push('\n');
    }
    assert!(child.wait().unwrap().success(), "{err}");
    let line = err.lines().find(|l| l.starts_with("[Download] RAM budget: ")).unwrap();
    println!("{line}");
    assert!(line.ends_with(" MB base; buildings post-pass not counted, its tiles are not in yet)"), "{line}");
    assert!(err.contains("[Buildings] 200 pbf tile(s) → "), "{err}");
}

// ── ingest ──────────────────────────────────────────────────────────────────

/// *n* copies of the OpenFreeMap fixture tile (Zernez, 470 buildings,
/// 60,516 bytes) at their own z14 addresses — distinct ground, so no copy
/// is a duplicate of another.
fn building_store(dir: &Path, n: u32) -> Vec<String> {
    let tile = std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/ofm_z14_8651_5782.pbf")).unwrap();
    std::fs::create_dir_all(dir).unwrap();
    (0..n)
        .map(|i| {
            let name = format!("14_{}_5782.pbf", 8651 + i);
            std::fs::write(dir.join(&name), &tile).unwrap();
            name
        })
        .collect()
}

/// A batch of three 64 px tiles over a small `.abt` source, each with
/// *extra* fields.
fn ingest_batch(dir: &Path, name: &str, extra: serde_json::Value) -> PathBuf {
    let src = dir.join("src.abt");
    if !src.exists() {
        let data: Vec<i16> = (0..200 * 200).map(|i| (2000 + i % 200) as i16).collect();
        common::write_abt(&src, 200, 47.01, 7.99, 0.0001, &data);
    }
    let jobs: Vec<serde_json::Value> = (0..3)
        .map(|i| {
            let mut j = json!({
                "output_path": dir.join(format!("{name}_out")).join(format!("t{i}.abt")), "format": "r16sint",
                "ul_lat": 47.005, "ul_lon": 8.0 + 0.002 * i as f64, "resolution_m": 2.0, "size_px": 64,
                "sources": [{"path": src}],
            });
            for (k, v) in extra.as_object().unwrap() {
                j[k] = v.clone();
            }
            j
        })
        .collect();
    let path = dir.join(format!("{name}.json"));
    std::fs::write(&path, serde_json::to_vec_pretty(&jobs).unwrap()).unwrap();
    path
}

/// The batch's pool size from its `[Rust] Rayon Pool:` line.
fn pool_threads(stdout: &str) -> usize {
    number_after(stdout, "[Rust] Rayon Pool: ", " threads").unwrap() as usize
}

/// 721 MB "available": 80 % of it less the 192 MB chunk cache is 384.8 MB,
/// two tiles of 192.008 MB (64 px tiles: 8 KB of samples, the 128 MB band
/// budget, 64 MB) and 0.77 MB to spare — a building set of more than that
/// leaves room for one.
const TIGHT_MB: u64 = 721;

fn cores() -> usize {
    std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4)
}

#[test]
fn a_building_set_there_at_launch_is_estimated_counted_in_the_pool_and_reported_decoded() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("pbf");
    building_store(&store, 6);
    // Without buildings: two threads (one on a one-core machine).
    let plain = ingest_batch(dir.path(), "plain", json!({}));
    let (ok, stdout, _) = run("ingest", &plain, Some(TIGHT_MB));
    assert!(ok, "{stdout}");
    let pool = stdout.lines().find(|l| l.starts_with("[Rust] Rayon Pool: ")).unwrap();
    assert!(pool.ends_with(" MB per tile)"), "no building set, nothing appended: {pool}");
    assert_eq!(pool_threads(&stdout), cores().min(2), "{pool}");

    // With them: 6 × 60,516 bytes on disk × 3 = 1.04 MB expected, more
    // than the 0.77 MB to spare — one thread.
    let with = ingest_batch(dir.path(), "with", json!({ "buildings_pbf_dir": store }));
    let (ok, stdout, err) = run("ingest", &with, Some(TIGHT_MB));
    assert!(ok, "{stdout}\n{err}");
    let pool = stdout.lines().find(|l| l.starts_with("[Rust] Rayon Pool: ")).unwrap();
    println!("{pool}");
    assert!(pool.ends_with(", ~192 MB per tile, building set ~1 MB)"), "{pool}");
    assert_eq!(pool_threads(&stdout), 1, "{pool}");
    // The decode's line ends with the set's memory, MB with one decimal:
    // 2,820 buildings of 5 – 30 vertices, 0.4 – 0.6 MB.
    let decoded = stdout.lines().find(|l| l.starts_with("[Rust] Building tiles decoded: ")).unwrap();
    println!("{decoded}");
    let (head, mb) = decoded.rsplit_once(" s, ").unwrap();
    assert!(head.starts_with("[Rust] Building tiles decoded: 6 tile(s), 2820 building(s) in "), "{decoded}");
    let mb = mb.strip_suffix(" MB").unwrap();
    assert!(mb.split_once('.').is_some_and(|(_, d)| d.len() == 1), "one decimal: {decoded}");
    let mb: f64 = mb.parse().unwrap();
    assert!((0.3..0.8).contains(&mb), "{decoded}");
    // The set's own size is below the estimate: nothing more to lower.
    assert!(!stdout.contains("[Rust] Tiles at once: "), "{stdout}");
    assert_eq!(stdout.matches("[Tile] ").count(), 6, "three covered and three voids lines:\n{stdout}");
}

#[test]
fn a_building_set_decoded_late_lowers_the_tiles_at_once_when_it_leaves_room_for_fewer() {
    // The list names 12 copies (1.0 MB decoded, more than the 0.77 MB to
    // spare) and is written after the batch has looked for it: the pool
    // cannot count it, its line says so, and once decoded the set leaves
    // room for one tile at a time — said in one line, before the held tiles
    // are written.
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("store");
    let names = building_store(&store, 12);
    let list = dir.path().join("lists").join("batch.txt");
    std::fs::create_dir_all(list.parent().unwrap()).unwrap();
    let job = ingest_batch(
        dir.path(),
        "late",
        json!({ "buildings_pbf_dir": store, "buildings_pbf_list": list, "buildings_pbf_wait_s": 60 }),
    );
    // Written once the pool line is out: the batch has looked for it then.
    let mut child = Command::new(env!("CARGO_BIN_EXE_aether_converter"))
        .args(["ingest", "--job-file"])
        .arg(&job)
        .env(AVAIL_VAR, TIGHT_MB.to_string())
        .env_remove("AETHER_DIAG_FILE")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn aether_converter");
    let mut stdout = String::new();
    for l in std::io::BufRead::lines(std::io::BufReader::new(child.stdout.take().unwrap())) {
        let l = l.unwrap();
        if l.starts_with("[Rust] Rayon Pool: ") {
            let part = list.with_extension("part");
            std::fs::write(&part, names.join("\n")).unwrap();
            std::fs::rename(&part, &list).unwrap();
        }
        stdout.push_str(&l);
        stdout.push('\n');
    }
    assert!(child.wait().unwrap().success(), "{stdout}");
    let pool = stdout.lines().find(|l| l.starts_with("[Rust] Rayon Pool: ")).unwrap();
    println!("{pool}");
    assert!(pool.ends_with(", ~192 MB per tile, building set: 1 source(s) decoded later)"), "{pool}");
    let threads = pool_threads(&stdout);
    assert_eq!(threads, cores().min(2), "{pool}");
    let decoded = stdout.lines().find(|l| l.starts_with("[Rust] Building tiles decoded: ")).unwrap();
    println!("{decoded}");
    let at_once: Vec<&str> = stdout.lines().filter(|l| l.starts_with("[Rust] Tiles at once: ")).collect();
    if threads == 2 {
        assert_eq!(at_once.len(), 1, "{stdout}");
        println!("{}", at_once[0]);
        assert!(
            at_once[0].starts_with("[Rust] Tiles at once: 1 of 2 threads from here on (building sets "),
            "{}",
            at_once[0]
        );
        assert!(at_once[0].ends_with(" MB; ~192 MB per tile, 0 GB available at start)"), "{}", at_once[0]);
        // Right after the decode's line.
        let at = |l: &str| stdout.lines().position(|x| x == l).unwrap();
        assert_eq!(at(at_once[0]), at(decoded) + 1, "{stdout}");
    } else {
        assert!(at_once.is_empty(), "one thread runs one tile already: {stdout}");
    }
    assert_eq!(stdout.matches("[Tile] ").count(), 6, "{stdout}");
}
