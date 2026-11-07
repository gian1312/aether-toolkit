//! The converter's peak resident memory at the end of a run (CONTRACT §1.2,
//! changelog 50; informational, never parsed). `ingest`'s `[Rust] run:` line
//! keeps its fields and ends with `, peak rss <mb> MB`; every CLI `download`
//! pass — a full run, a repair, a run that fails — prints `[Stats] Peak RSS:
//! <mb> MB` exactly once in its `[Stats]` block, with `(RAM budget <mb> MB)`
//! for a full run as its `[Download] RAM budget:` line printed it, after
//! every other line of the block but the cache lines, and keeps it in
//! `AETHER_DIAG_FILE` like the rest of the block. The number is the
//! converter's own: this process holds more memory than any converter it
//! starts uses, which a peak inherited from the parent — Linux's
//! `ru_maxrss` — would report.

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use aether_converter::download::{lat2ty, lon2tx};
use common::{serve_tiles, terrarium_png, tile_ground};

/// What this process holds, resident, while it starts converters: more than
/// any of them uses.
const HELD_MB: usize = 256;

fn hold_memory() {
    static BLOCK: OnceLock<Vec<u8>> = OnceLock::new();
    // Written byte by byte, so every page is resident.
    BLOCK.get_or_init(|| vec![1u8; HELD_MB << 20]);
}

/// Run the converter with `args` and the job file *job*, `AETHER_DIAG_FILE`
/// set to *diag* or removed; `(exit ok, stdout, stderr)`.
fn run(args: &[&str], job: &Path, diag: Option<&Path>) -> (bool, String, String) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_aether_converter"));
    cmd.args(args).arg(job);
    match diag {
        Some(p) => cmd.env("AETHER_DIAG_FILE", p),
        None => cmd.env_remove("AETHER_DIAG_FILE"),
    };
    let out = cmd.output().expect("spawn aether_converter");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// A peak in MB is the converter's: above 1 MB, below the machine's RAM and
/// below what this process — its parent — holds.
fn assert_plausible(mb: f64, line: &str) {
    let mut sys = sysinfo::System::new();
    sys.refresh_memory();
    let ram_mb = sys.total_memory() as f64 / (1u64 << 20) as f64;
    assert!(mb > 1.0, "{line}");
    assert!(mb < ram_mb, "above the machine's {ram_mb:.0} MB: {line}");
    assert!(mb < HELD_MB as f64, "the parent's {HELD_MB} MB are in the converter's peak: {line}");
}

/// `<d>.<d>` — a number with exactly one decimal.
fn one_decimal(s: &str) -> bool {
    s.split_once('.').is_some_and(|(i, d)| {
        !i.is_empty() && i.bytes().all(|b| b.is_ascii_digit()) && d.len() == 1 && d.bytes().all(|b| b.is_ascii_digit())
    })
}

// ── ingest ──────────────────────────────────────────────────────────────────

#[test]
fn the_ingest_run_line_keeps_its_fields_and_ends_with_the_peak_rss() {
    hold_memory();
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src.abt");
    let data: Vec<i16> = (0..200 * 200).map(|i| (2000 + i % 200 + 2 * (i / 200)) as i16).collect();
    common::write_abt(&src, 200, 47.01, 7.99, 0.0001, &data);
    let job = |name: &str, lon: f64| {
        serde_json::json!({
            "output_path": dir.path().join(name), "format": "r16sint",
            "ul_lat": 47.005, "ul_lon": lon, "resolution_m": 10.0, "size_px": 16,
            "sources": [{"path": src}],
        })
    };
    let batch = dir.path().join("batch.json");
    std::fs::write(&batch, serde_json::to_vec(&[job("a.abt", 8.0), job("b.abt", 8.002)]).unwrap()).unwrap();
    let single = dir.path().join("single.json");
    std::fs::write(&single, serde_json::to_vec(&job("c.abt", 8.0)).unwrap()).unwrap();

    for (file, is_batch) in [(&batch, true), (&single, false)] {
        let (ok, stdout, stderr) = run(&["ingest", "--job-file"], file, None);
        assert!(ok, "stdout:\n{stdout}\nstderr:\n{stderr}");
        let runs: Vec<&str> = stdout.lines().filter(|l| l.starts_with("[Rust] run: ")).collect();
        assert_eq!(runs.len(), 1, "{stdout}");
        let line = runs[0];
        println!("{line}");
        assert_eq!(stdout.lines().last(), Some(line), "still the last stdout line");
        // The old fields, in their old order and shape (changelog 31, 37) …
        let f: Vec<&str> = line["[Rust] run: ".len()..].split(", ").collect();
        assert_eq!(f.len(), if is_batch { 6 } else { 5 }, "{line}");
        let wall = f[0].strip_prefix("wall ").and_then(|v| v.strip_suffix('s'));
        assert!(wall.is_some_and(one_decimal), "{line}");
        let cpu = f[1].strip_prefix("cpu user ").and_then(|v| v.strip_suffix('s')).and_then(|v| v.split_once("s sys "));
        assert!(cpu.is_some_and(|(u, s)| one_decimal(u) && one_decimal(s)), "{line}");
        let faults = f[2].strip_prefix("page faults ");
        assert!(faults.is_some_and(|n| n.parse::<u64>().is_ok_and(|n| n > 0)), "{line}");
        if is_batch {
            assert!(f[3].strip_prefix("threads ").is_some_and(|n| n.parse::<usize>().is_ok_and(|n| n > 0)), "{line}");
        }
        let blocked = f[f.len() - 2].strip_prefix("blocked ~").and_then(|v| v.strip_suffix('s'));
        assert!(blocked.is_some_and(one_decimal), "{line}");
        // … then the peak, last (changelog 50).
        let peak = f[f.len() - 1].strip_prefix("peak rss ").and_then(|v| v.strip_suffix(" MB"));
        assert!(peak.is_some_and(one_decimal), "{line}");
        assert_plausible(peak.unwrap().parse().unwrap(), line);
        assert_eq!(stdout.matches("peak rss").count(), 1, "{stdout}");
        assert!(!stderr.contains("peak rss") && !stderr.contains("Peak RSS"), "{stderr}");
    }
}

// ── download ────────────────────────────────────────────────────────────────

const Z: u32 = 12;
/// Two 256 px tiles at 60 m from (47.01, 8.03): a 4 × 3 block of z12 tiles.
const UL: (f64, f64) = (47.01, 8.03);
const SIZE: usize = 256;
const RES_M: f64 = 60.0;

/// Write the download job *name* of the two tiles over the source at *port*,
/// plus the fields of *extra*.
fn download_job(dir: &Path, name: &str, port: u16, extra: serde_json::Value) -> PathBuf {
    let pd = RES_M / 111_111.0;
    let tiles: Vec<serde_json::Value> = (0..2)
        .map(|i| {
            serde_json::json!({
                "filename": format!("t{i}.abt"), "ul_lat": UL.0, "ul_lon": UL.1 + (i * SIZE) as f64 * pd,
                "size_px": SIZE, "resolution_m": RES_M,
            })
        })
        .collect();
    let mut job = serde_json::json!({
        "url_template": format!("http://127.0.0.1:{port}/{{z}}/{{x}}/{{y}}.png"),
        "encoding": "terrarium", "output_dir": dir.join("out"), "zoom": Z,
        "max_connections": 16, "tiles": tiles,
    });
    for (k, v) in extra.as_object().unwrap() {
        job[k] = v.clone();
    }
    let path = dir.join(name);
    std::fs::write(&path, serde_json::to_vec(&job).unwrap()).unwrap();
    path
}

/// The one `[Stats] Peak RSS:` line of a pass's stderr, checked: printed
/// once, `<mb>` with one decimal and plausible, `(RAM budget <m> MB)` with
/// the budget of the pass's `[Download] RAM budget:` line when it had one,
/// nothing after `MB` when not (a repair), after every other `[Stats]` line
/// and followed by exactly the block's cache lines (none without a cache).
fn checked_peak_line(stderr: &str) -> &str {
    let lines: Vec<&str> = stderr.lines().filter(|l| l.contains("Peak RSS")).collect();
    assert_eq!(lines.len(), 1, "one Peak RSS line per pass:\n{stderr}");
    let line = lines[0];
    let rest = line.strip_prefix("[Stats] Peak RSS: ").unwrap_or_else(|| panic!("{line}"));
    let budget = stderr
        .lines()
        .find_map(|l| l.strip_prefix("[Download] RAM budget: "))
        .and_then(|r| r.split_once(" MB, "))
        .map(|(mb, _)| mb);
    let peak = match budget {
        Some(b) => rest.strip_suffix(&format!(" MB (RAM budget {b} MB)")),
        None => rest.strip_suffix(" MB"),
    };
    assert!(peak.is_some_and(one_decimal), "{line} (budget {budget:?})");
    assert_plausible(peak.unwrap().parse().unwrap(), line);
    println!("{line}");
    let stats: Vec<&str> = stderr.lines().filter(|l| l.starts_with("[Stats] ")).collect();
    let at = stats.iter().position(|l| *l == line).unwrap();
    assert!(stats[0].starts_with("[Stats] Tiles: ") && at > 0, "{stats:#?}");
    let cache: Vec<&str> = stats.iter().copied().filter(|l| l.starts_with("[Stats] Cache")).collect();
    assert_eq!(stats[at + 1..], cache[..], "after it the cache lines, all of them, and nothing else: {stats:#?}");
    line
}

#[test]
fn a_full_download_and_a_repair_print_their_peak_rss_once_each_and_keep_it_in_the_diag_file() {
    hold_memory();
    let (port, _) = serve_tiles(|z, x, y| Some(terrarium_png(tile_ground(z, x, y))));
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("cache");

    let job = download_job(dir.path(), "job.json", port, serde_json::json!({ "cache_dir": cache }));
    let diag = dir.path().join("diag.log");
    let (ok, stdout, stderr) = run(&["download", "--job-file"], &job, Some(&diag));
    assert!(ok, "{stderr}");
    let line = checked_peak_line(&stderr);
    assert!(line.contains(" (RAM budget "), "a full run names its budget: {line}");
    assert!(stderr.lines().any(|l| l.starts_with("[Stats] Cache: ")), "a run with a cache prints its lines:\n{stderr}");
    // Kept in the diagnostic file like the whole block (changelog 46).
    let logged = std::fs::read_to_string(&diag).unwrap();
    assert_eq!(logged.lines().filter(|l| l.contains("Peak RSS")).collect::<Vec<_>>(), [line], "{logged}");
    assert!(!stdout.contains("Peak RSS") && !stdout.contains("[Rust] run:"), "{stdout}");

    // A repair of one source tile into those files: its own pass, its own
    // line — without a budget, which a repair does not have.
    let first = [Z, lon2tx(UL.1, Z), lat2ty(UL.0, Z)];
    let repair = download_job(
        dir.path(),
        "repair.json",
        port,
        serde_json::json!({ "cache_dir": cache, "repair_source_tiles": [first] }),
    );
    let diag = dir.path().join("diag_repair.log");
    let (ok, _, stderr) = run(&["download", "--job-file"], &repair, Some(&diag));
    assert!(ok, "{stderr}");
    assert!(stderr.lines().any(|l| l == "[Stats] Tiles: 1/1 OK (100.0% success)"), "{stderr}");
    let line = checked_peak_line(&stderr);
    assert!(!line.contains("RAM budget"), "{line}");
    let logged = std::fs::read_to_string(&diag).unwrap();
    assert_eq!(logged.lines().filter(|l| l.contains("Peak RSS")).collect::<Vec<_>>(), [line], "{logged}");
}

#[test]
fn a_download_that_fails_still_prints_its_peak_rss_once() {
    hold_memory();
    // Not one z12 tile decodes and no coarser zoom exists: the run is fatal.
    let (port, _) = serve_tiles(|z, _, _| (z == Z).then(|| b"not a tile".to_vec()));
    let dir = tempfile::tempdir().unwrap();
    let job = download_job(dir.path(), "job.json", port, serde_json::json!({}));
    let (ok, _, stderr) = run(&["download", "--job-file"], &job, None);
    assert!(!ok, "{stderr}");
    assert!(stderr.contains("Tile download failed: "), "{stderr}");
    let line = checked_peak_line(&stderr);
    assert!(line.contains(" (RAM budget "), "{line}");
}
