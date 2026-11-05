//! `AETHER_DIAG_FILE` (CONTRACT §1.2, changelog 37): the per-tile
//! `[Rust] tile …` diagnostic lines go to the named file (appended) instead
//! of stdout; every parsed line (`[Tile]`, `[Rust] Progress:`) and the
//! one-per-run `[Rust] run:` summary stay on stdout. Unset, the lines are on
//! stdout as before; a file that cannot be opened costs one `[Warn]` and the
//! lines fall back to the console. A `download`'s progress record —
//! `[Download] <pct>%`, `STALL`, `first …`, `[Degraded]` and the `[Stats]`
//! block — stays on stderr and is ALSO kept in the file (changelog 46), and
//! so are the `[Tile] <name> voids=<n>` lines of a download (stderr) and of
//! a batch ingest (stdout; changelog 48) — not the ingest's covered lines.

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;

const ENV: &str = "AETHER_DIAG_FILE";

/// A geographic R16SINT `.abt` source (§6): `size`² px of `deg` degrees from
/// (`ul_lat`, `ul_lon`), values a gentle ramp in half-metres.
fn write_abt(path: &Path, size: u16, ul_lat: f64, ul_lon: f64, deg: f64) {
    let stride = (size as usize * 2 + 255) & !255;
    let mut b: Vec<u8> = b"AETH".to_vec();
    b.extend_from_slice(&1u16.to_le_bytes());
    b.extend_from_slice(&size.to_le_bytes());
    for v in [ul_lat, ul_lon, deg, deg] {
        b.extend_from_slice(&v.to_le_bytes());
    }
    b.extend_from_slice(&0i16.to_le_bytes());
    b.extend_from_slice(&(stride as u16).to_le_bytes());
    for r in 0..size as usize {
        let mut row = vec![0u8; stride];
        for c in 0..size as usize {
            row[2 * c..2 * c + 2].copy_from_slice(&((1000 + r + 2 * c) as i16).to_le_bytes());
        }
        b.extend_from_slice(&row);
    }
    std::fs::write(path, b).unwrap();
}

/// Two 16 px tiles at 10 m inside one `.abt` source, as a batch (array) job
/// file; returns the job file and the two output paths.
fn batch_job(dir: &Path) -> (PathBuf, Vec<PathBuf>) {
    let src = dir.join("src.abt");
    write_abt(&src, 200, 47.01, 7.99, 0.0001);
    let outs: Vec<PathBuf> = ["a", "b"].iter().map(|n| dir.join("out").join(format!("tile_{n}.abt"))).collect();
    let jobs: Vec<serde_json::Value> = outs
        .iter()
        .zip([8.0, 8.002])
        .map(|(out, lon)| {
            serde_json::json!({
                "output_path": out, "format": "r16sint",
                "ul_lat": 47.005, "ul_lon": lon, "resolution_m": 10.0, "size_px": 16,
                "sources": [{"path": src}],
            })
        })
        .collect();
    let job_file = dir.join("batch.json");
    std::fs::write(&job_file, serde_json::to_vec(&jobs).unwrap()).unwrap();
    (job_file, outs)
}

/// Run `ingest` on *job_file* with `AETHER_DIAG_FILE` set to *diag* (or
/// removed); `(exit ok, stdout, stderr)`.
fn ingest(job_file: &Path, diag: Option<&Path>) -> (bool, String, String) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_aether_converter"));
    cmd.args(["ingest", "--job-file"]).arg(job_file);
    match diag {
        Some(p) => cmd.env(ENV, p),
        None => cmd.env_remove(ENV),
    };
    let out = cmd.output().expect("spawn aether_converter");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn tile_lines(text: &str) -> Vec<&str> {
    text.lines().filter(|l| l.starts_with("[Rust] tile ")).collect()
}

/// The parsed and summary lines a batch must keep on stdout.
fn assert_stdout_keeps_the_parsed_lines(stdout: &str) {
    // Per tile its covered line and (changelog 46) its voids line.
    assert_eq!(stdout.lines().filter(|l| l.starts_with("[Tile] ") && l.contains(" covered=")).count(), 2, "{stdout}");
    assert_eq!(stdout.lines().filter(|l| l.starts_with("[Tile] ") && l.contains(" voids=")).count(), 2, "{stdout}");
    assert!(stdout.lines().any(|l| l == "[Rust] Progress: 2/2"), "{stdout}");
    assert!(stdout.lines().any(|l| l.starts_with("[Rust] Batch processing 2 tiles")), "{stdout}");
    assert!(stdout.lines().any(|l| l.starts_with("[Rust] Rayon Pool: ")), "{stdout}");
    let run: Vec<&str> = stdout.lines().filter(|l| l.starts_with("[Rust] run: ")).collect();
    assert_eq!(run.len(), 1, "{stdout}");
    assert!(run[0].contains(", threads ") && run[0].contains(", blocked ~"), "{}", run[0]);
}

#[test]
fn diag_lines_go_to_the_file_and_parsed_lines_stay_on_stdout() {
    let dir = tempfile::tempdir().unwrap();
    let (job_file, outs) = batch_job(dir.path());
    let diag = dir.path().join("diag.log");
    // Appended, never truncated: an earlier line survives.
    std::fs::write(&diag, "earlier line\n").unwrap();

    let (ok, stdout, stderr) = ingest(&job_file, Some(&diag));
    assert!(ok, "stdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(tile_lines(&stdout).is_empty(), "tile lines leaked to stdout:\n{stdout}");
    assert!(!stderr.contains("[Rust] tile "), "{stderr}");
    assert!(!stderr.contains("[Warn]"), "{stderr}");
    assert_stdout_keeps_the_parsed_lines(&stdout);

    let logged = std::fs::read_to_string(&diag).unwrap();
    assert!(logged.starts_with("earlier line\n"), "{logged}");
    // The voids lines are kept in the file as well (changelog 48): byte for
    // byte stdout's, in stdout's order; the covered lines are not.
    let voids = |text: &str| -> Vec<String> {
        text.lines().filter(|l| l.starts_with("[Tile] ") && l.contains(" voids=")).map(str::to_owned).collect()
    };
    assert_eq!(voids(&stdout).len(), 2, "{stdout}");
    assert_eq!(voids(&logged), voids(&stdout), "the diag file must keep the voids lines:\n{logged}");
    assert!(!logged.contains(" covered="), "{logged}");
    let lines = tile_lines(&logged);
    assert_eq!(lines.len(), 2, "{logged}");
    for l in &lines {
        // changelog 31 fields, then the three added by 37
        for field in [" sources, header ", " decode ", " sample ", ", bands ", ", geom ", "s, write ", "s, total "] {
            assert!(l.contains(field), "{field:?} missing in {l}");
        }
        assert!(l.ends_with('s'), "{l}");
    }
    assert!(lines.iter().any(|l| l.starts_with("[Rust] tile tile_a.abt: 1 sources")), "{logged}");
    for out in &outs {
        assert!(out.exists(), "{out:?} was not written");
    }

    // A single job (a JSON object) sends its line there too.
    let single = dir.path().join("single.json");
    let one: Vec<serde_json::Value> = serde_json::from_slice(&std::fs::read(&job_file).unwrap()).unwrap();
    std::fs::write(&single, serde_json::to_vec(&one[0]).unwrap()).unwrap();
    let (ok, stdout, stderr) = ingest(&single, Some(&diag));
    assert!(ok, "stdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(tile_lines(&stdout).is_empty(), "{stdout}");
    let run: Vec<&str> = stdout.lines().filter(|l| l.starts_with("[Rust] run: ")).collect();
    assert_eq!(run.len(), 1, "{stdout}");
    assert!(!run[0].contains(", threads ") && run[0].contains(", blocked ~"), "{}", run[0]);
    let logged = std::fs::read_to_string(&diag).unwrap();
    assert_eq!(tile_lines(&logged).len(), 3);
    // The single job prints no `[Tile]` line, so keeps none.
    assert_eq!(voids(&logged).len(), 2, "{logged}");
}

#[test]
fn unset_the_diag_lines_stay_on_stdout() {
    let dir = tempfile::tempdir().unwrap();
    let (job_file, _) = batch_job(dir.path());
    let (ok, stdout, stderr) = ingest(&job_file, None);
    assert!(ok, "stdout:\n{stdout}\nstderr:\n{stderr}");
    assert_eq!(tile_lines(&stdout).len(), 2, "{stdout}");
    assert!(!stderr.contains("[Warn]"), "{stderr}");
    assert_stdout_keeps_the_parsed_lines(&stdout);
}

#[test]
fn an_unopenable_diag_file_warns_once_and_falls_back_to_the_console() {
    let dir = tempfile::tempdir().unwrap();
    let (job_file, outs) = batch_job(dir.path());
    // A path under a directory that does not exist cannot be created.
    let diag = dir.path().join("no").join("such").join("dir").join("diag.log");
    let (ok, stdout, stderr) = ingest(&job_file, Some(&diag));
    assert!(ok, "an unusable diag file must not fail the run:\n{stdout}\n{stderr}");
    let warns: Vec<&str> = stderr.lines().filter(|l| l.starts_with("[Warn]") && l.contains(ENV)).collect();
    assert_eq!(warns.len(), 1, "{stderr}");
    assert_eq!(tile_lines(&stdout).len(), 2, "{stdout}");
    assert_stdout_keeps_the_parsed_lines(&stdout);
    assert!(!diag.exists());
    for out in &outs {
        assert!(out.exists(), "{out:?} was not written");
    }
}

// ── download: the progress record is kept in the file too (changelog 46) ────

/// Whether *line* is one a `download` keeps in the diag file as well.
fn kept_by_download(line: &str) -> bool {
    let progress = line.strip_prefix("[Download] ").and_then(|r| r.split_once("% (")).is_some_and(|(pct, _)| pct.parse::<u32>().is_ok());
    progress
        || line.starts_with("[Download] STALL ")
        || line.starts_with("[Download] (suppressing further stall warnings")
        || line.starts_with("[Download] first ")
        || line.starts_with("[Degraded] ")
        || line.starts_with("[Stats] ")
        // changelog 48
        || (line.starts_with("[Tile] ") && line.contains(" voids="))
}

#[test]
fn a_downloads_progress_record_is_also_appended_to_the_diag_file() {
    use aether_converter::download::{lat2ty, lon2tx};
    use common::{serve_tiles_delayed, terrarium_png, tile_ground};
    const Z: u32 = 12;
    // Two 256 px tiles at 60 m: a 4 x 3 block of z12 source tiles.
    let (ul_lat, ul_lon, size, res) = (47.01, 8.03, 256usize, 60.0);
    let pd = res / 111_111.0;
    let (x0, x1) = (lon2tx(ul_lon, Z), lon2tx(ul_lon + 2.0 * size as f64 * pd, Z));
    let (y0, y1) = (lat2ty(ul_lat, Z), lat2ty(ul_lat - size as f64 * pd, Z));
    assert!(x1 > x0 && y1 > y0, "the outputs must span several source tiles");
    // Three tiles that are not the run's first (the edge probe): one that
    // does not decode, one 5.5 s late (a STALL), one silent past the 8 s
    // deadline (a timeout); their z11 parents fill the two that fail.
    let (bad, slow, dead) = ((x1, y1), (x1, y0), (x0, y1));
    let (port, _) = serve_tiles_delayed(move |z, x, y| match z {
        11 => Some((terrarium_png(tile_ground(z, x, y)), 0)),
        Z if (x, y) == bad => Some((b"not a tile".to_vec(), 0)),
        Z if (x, y) == slow => Some((terrarium_png(tile_ground(z, x, y)), 5500)),
        Z if (x, y) == dead => Some((terrarium_png(tile_ground(z, x, y)), 60_000)),
        Z => Some((terrarium_png(tile_ground(z, x, y)), 0)),
        _ => None,
    });
    let dir = tempfile::tempdir().unwrap();
    let tiles: Vec<serde_json::Value> = (0..2)
        .map(|i| serde_json::json!({
            "filename": format!("t{i}.abt"), "ul_lat": ul_lat, "ul_lon": ul_lon + (i * size) as f64 * pd,
            "size_px": size, "resolution_m": res,
        }))
        .collect();
    let job = dir.path().join("job.json");
    std::fs::write(&job, serde_json::to_vec(&serde_json::json!({
        "url_template": format!("http://127.0.0.1:{port}/{{z}}/{{x}}/{{y}}.png"),
        "encoding": "terrarium", "output_dir": dir.path().join("out"), "zoom": Z,
        "max_connections": 16, "tile_deadline_s": 8, "tiles": tiles,
    })).unwrap()).unwrap();
    let diag = dir.path().join("diag.log");
    std::fs::write(&diag, "earlier line\n").unwrap();

    let out = Command::new(env!("CARGO_BIN_EXE_aether_converter"))
        .args(["download", "--job-file"]).arg(&job).env(ENV, &diag).output().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(out.status.success(), "{stderr}");
    let logged = std::fs::read_to_string(&diag).unwrap();
    assert!(logged.starts_with("earlier line\n"), "appended, never truncated:\n{logged}");

    // stderr is what it was: every whole percent, then the summary.
    let total = (x1 - x0 + 1) as usize * (y1 - y0 + 1) as usize;
    let progress: Vec<&str> = stderr.lines().filter(|l| kept_by_download(l) && l.contains("% (")).collect();
    let due = (1..=total).filter(|&d| d * 100 / total > (d - 1) * 100 / total || d == total).count();
    assert_eq!(progress.len(), due, "{stderr}");
    assert!(progress.last().unwrap().starts_with(&format!("[Download] 100% ({total}/{total}) — ")), "{stderr}");
    for needle in [
        format!("[Download] STALL tile z={Z}/x={}/y={}: ", slow.0, slow.1),
        format!("[Download] first timeout: z={Z}/x={}/y={}", dead.0, dead.1),
        format!("[Download] first decode error: z={Z}/x={}/y={} — ", bad.0, bad.1),
        format!("[Degraded] z={Z} x={} y={} reason=timeout filled_from=z11", dead.0, dead.1),
        format!("[Degraded] z={Z} x={} y={} reason=decode filled_from=z11", bad.0, bad.1),
        format!("[Stats] Tiles: {}/{total} OK", total - 2),
        format!("[Stats] DEGRADED: 2 of {total} source tile(s)"),
        // The slow tile, and the silent one: it waited its 8 s for a first byte.
        format!("[Download] STALL tile z={Z}/x={}/y={}: first byte after ", dead.0, dead.1),
        format!("[Stats] STALL: 2 of {total} tile(s)"),
    ] {
        assert!(stderr.lines().any(|l| l.starts_with(&needle)), "{needle:?} missing on stderr:\n{stderr}");
    }

    // One voids line per output on stderr (changelog 46), before the
    // `[Stats]` block — and in the file (changelog 48).
    let voids: Vec<&str> = stderr.lines().filter(|l| l.starts_with("[Tile] ") && l.contains(" voids=")).collect();
    assert_eq!(voids.len(), 2, "{stderr}");
    assert!(voids[0].starts_with("[Tile] t0.abt voids=") && voids[1].starts_with("[Tile] t1.abt voids="), "{voids:?}");
    assert_eq!(logged.lines().filter(|l| voids.contains(l)).count(), 2, "{logged}");

    // The file keeps exactly those lines too, byte for byte, in stderr's
    // order — and besides them only the diagnostics it always had.
    let mine: Vec<&str> = stderr.lines().filter(|l| kept_by_download(l)).collect();
    let theirs: Vec<&str> = logged.lines().skip(1).filter(|l| kept_by_download(l)).collect();
    assert!(mine.len() > total, "{stderr}");
    assert_eq!(theirs, mine, "the diag file must hold the same lines in the same order");
    for l in logged.lines().skip(1).filter(|l| !kept_by_download(l)) {
        assert!(
            l.starts_with("[Strip ") || l.starts_with("[StripMem ") || l.starts_with("[Download] launch gate closed for "),
            "unexpected line in the diag file: {l:?}"
        );
        assert!(!stderr.contains(l), "a diagnostic line is in the file instead of on stderr: {l:?}");
    }
    assert!(logged.contains("[Download] launch gate closed for "), "{logged}");
}
