//! `AETHER_DIAG_FILE` (CONTRACT §1.2, changelog 37): the per-tile
//! `[Rust] tile …` diagnostic lines go to the named file (appended) instead
//! of stdout; every parsed line (`[Tile]`, `[Rust] Progress:`) and the
//! one-per-run `[Rust] run:` summary stay on stdout. Unset, the lines are on
//! stdout as before; a file that cannot be opened costs one `[Warn]` and the
//! lines fall back to the console.

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
    assert_eq!(stdout.lines().filter(|l| l.starts_with("[Tile] ")).count(), 2, "{stdout}");
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
    assert_eq!(tile_lines(&std::fs::read_to_string(&diag).unwrap()).len(), 3);
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
