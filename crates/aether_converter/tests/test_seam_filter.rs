//! The seam filter (CONTRACT §9b `seam_filter`, changelog 49) on the real
//! AWS Terrarium tiles of the 10°E seam, through the real CLI `download`.
//!
//! The fixtures (`tests/fixtures/terrarium_seam_10e/<z>_<x>_<y>.png`) are the
//! service's bytes of `s3.amazonaws.com/elevation-tiles-prod/terrarium`,
//! fetched 2026-11-05: z15 x 17292 … 17295 × y 11552 … 11553 (x 17294 holds
//! the seam — y 11552 in pixel columns 52 and 53, −9,313 … −8,999 m and
//! −573 … −141 m, y 11553 in column 53 only; the other six are blank,
//! −32768 m everywhere), z14 8646/5776 (blank) and 8647/5776 (the seam as
//! column 26, −4,165 … −3,782 m), z13 4323/2888 (clean, 2,030 – 2,913 m).
//! Everything else answers 404. In these tiles of 1,500 – 3,000 m terrain
//! every seam sample — and nothing else — is below 0 m, so the test tells
//! the seam by its value alone, without the rule it checks.
//!
//! The filter must be exactly "these samples are voids": a run with it
//! (default) must write the bytes a run without it writes over the same
//! tiles with those samples blanked to Terrarium's void — fetched, from the
//! raw tile cache, and derived from cached finer tiles alike.

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;

use common::{payload, read_dir_bytes, serve_tiles, TileCounts};
use serde_json::json;

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/terrarium_seam_10e");
const DIAG_ENV: &str = "AETHER_DIAG_FILE";

/// The fixture tile `(z, x, y)`, as the service sent it.
fn fixture(z: u32, x: u32, y: u32) -> Option<Vec<u8>> {
    std::fs::read(format!("{FIXTURES}/{z}_{x}_{y}.png")).ok()
}

/// `png` with every sample below 0 m written as Terrarium's void `0, 0, 0`.
fn blank_below_zero(png: &[u8]) -> Vec<u8> {
    let mut reader = png::Decoder::new(std::io::Cursor::new(png)).read_info().unwrap();
    let mut buf = vec![0u8; reader.output_buffer_size()];
    let frame = reader.next_frame(&mut buf).unwrap();
    let ch = if frame.color_type == png::ColorType::Rgba { 4 } else { 3 };
    let (w, h) = (frame.width, frame.height);
    let mut rgb = Vec::with_capacity((w * h * 3) as usize);
    for p in buf[..frame.buffer_size()].chunks_exact(ch) {
        // Terrarium: v = R·256 + G + B/256 − 32768, so v < 0 ⟺ R < 128.
        rgb.extend_from_slice(if p[0] < 128 { &[0, 0, 0] } else { &p[..3] });
    }
    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, w, h);
        enc.set_color(png::ColorType::Rgb);
        enc.set_depth(png::BitDepth::Eight);
        enc.write_header().unwrap().write_image_data(&rgb).unwrap();
    }
    out
}

/// The loopback service of the fixtures; with `blank_seam`, their seam
/// samples blanked (`blank_below_zero`).
fn service(blank_seam: bool) -> (u16, TileCounts) {
    serve_tiles(move |z, x, y| fixture(z, x, y).map(|png| if blank_seam { blank_below_zero(&png) } else { png }))
}

/// z15 at 5 m: three 320 px tiles side by side whose sources are exactly
/// the eight z15 fixtures (lon 9.9757 … 10.0189, lat 46.7999 … 46.7855).
fn z15_tiles() -> serde_json::Value {
    let pd = 5.0 / 111_111.0;
    json!((0..3).map(|i| json!({
        "filename": format!("z15_{i}.abt"), "ul_lat": 46.7999, "ul_lon": 9.9757 + i as f64 * 320.0 * pd,
        "size_px": 320, "resolution_m": 5.0,
    })).collect::<Vec<_>>())
}

/// z14 at 10 m: three 160 px tiles over the same ground, whose sources are
/// exactly the two z14 tiles 8646/5776 and 8647/5776.
fn z14_tiles() -> serde_json::Value {
    let pd = 10.0 / 111_111.0;
    json!((0..3).map(|i| json!({
        "filename": format!("z14_{i}.abt"), "ul_lat": 46.7999, "ul_lon": 9.9757 + i as f64 * 160.0 * pd,
        "size_px": 160, "resolution_m": 10.0,
    })).collect::<Vec<_>>())
}

/// Write a download job (`<dir>/<name>.json`, output `<dir>/<name>/`).
fn job(dir: &Path, port: u16, name: &str, zoom: u32, tiles: serde_json::Value, extra: serde_json::Value) -> (PathBuf, PathBuf) {
    let out = dir.join(name);
    let mut j = json!({
        "url_template": format!("http://127.0.0.1:{port}/{{z}}/{{x}}/{{y}}.png"),
        "encoding": "terrarium", "output_dir": out, "zoom": zoom, "max_connections": 8, "tiles": tiles,
    });
    for (k, v) in extra.as_object().unwrap() {
        j[k] = v.clone();
    }
    let path = dir.join(format!("{name}.json"));
    std::fs::write(&path, serde_json::to_vec_pretty(&j).unwrap()).unwrap();
    (path, out)
}

/// Run the CLI `download`, `AETHER_DIAG_FILE` set to `diag` or unset;
/// returns stderr (the run must succeed).
fn download(job_file: &Path, diag: Option<&Path>) -> String {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_aether_converter"));
    cmd.args(["download", "--job-file"]).arg(job_file);
    match diag {
        Some(p) => cmd.env(DIAG_ENV, p),
        None => cmd.env_remove(DIAG_ENV),
    };
    let out = cmd.output().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(out.status.success(), "{stderr}");
    stderr
}

/// The run's `[Stats] Backfill:` line.
fn backfill(stderr: &str) -> &str {
    let mut lines = stderr.lines().filter(|l| l.starts_with("[Stats] Backfill: "));
    let line = lines.next().unwrap_or_else(|| panic!("no [Stats] Backfill line:\n{stderr}"));
    assert!(lines.next().is_none(), "one line per pass:\n{stderr}");
    line
}

fn requests_at(counts: &TileCounts, zoom: u32) -> usize {
    counts.lock().unwrap().iter().filter(|((z, _, _), _)| *z == zoom).map(|(_, n)| n).sum()
}

/// Every sample of every `.abt` in `out`, half-metres.
fn all_samples(out: &Path) -> Vec<i16> {
    read_dir_bytes(out).iter().flat_map(|(_, b)| payload(b, u16::from_le_bytes([b[6], b[7]]) as usize)).collect()
}

#[test]
fn the_fixtures_hold_the_seam_where_the_test_says() {
    // The premise of every test below, checked on the bytes: the seam is
    // the samples below 0 m, and there are this many of them.
    let below_zero = |z, x, y| {
        let png = fixture(z, x, y).unwrap();
        let mut reader = png::Decoder::new(std::io::Cursor::new(png)).read_info().unwrap();
        let mut buf = vec![0u8; reader.output_buffer_size()];
        let frame = reader.next_frame(&mut buf).unwrap();
        let ch = if frame.color_type == png::ColorType::Rgba { 4 } else { 3 };
        let px: Vec<&[u8]> = buf[..frame.buffer_size()].chunks_exact(ch).collect();
        let blank = px.iter().filter(|p| p[..3] == [0, 0, 0]).count();
        let cols: std::collections::BTreeSet<usize> =
            px.iter().enumerate().filter(|(_, p)| p[0] < 128 && p[..3] != [0, 0, 0]).map(|(i, _)| i % 256).collect();
        let n = px.iter().filter(|p| p[0] < 128 && p[..3] != [0, 0, 0]).count();
        (blank, n, cols.into_iter().collect::<Vec<_>>())
    };
    assert_eq!(below_zero(15, 17294, 11552), (0, 512, vec![52, 53]));
    assert_eq!(below_zero(15, 17294, 11553), (0, 256, vec![53]));
    assert_eq!(below_zero(14, 8647, 5776), (0, 256, vec![26]));
    assert_eq!(below_zero(13, 4323, 2888), (0, 0, vec![]));
    for (z, x, y) in [(15, 17292, 11552), (15, 17293, 11553), (15, 17295, 11552), (14, 8646, 5776)] {
        assert_eq!(below_zero(z, x, y), (65536, 0, vec![]), "z{z}/{x}/{y} is blank");
    }
}

#[test]
fn the_seam_is_voided_and_backfilled_exactly_like_a_void() {
    let dir = tempfile::tempdir().unwrap();
    let (port, counts) = service(false);
    let (blank_port, _) = service(true);

    // With the filter (the default) over the service's tiles…
    let (j, with) = job(dir.path(), port, "with", 15, z15_tiles(), json!({}));
    let err_with = download(&j, None);
    // …and without it, over the same tiles with the seam blanked.
    let (j, blanked) = job(dir.path(), blank_port, "blanked", 15, z15_tiles(), json!({ "seam_filter": false }));
    let err_blanked = download(&j, None);
    assert_eq!(read_dir_bytes(&with), read_dir_bytes(&blanked), "the filter is: these samples are voids");

    // 768 seam samples, six blank tiles: all filled from z13 (z14 is blank
    // or the seam there too), none left; and the line says so.
    assert_eq!(
        backfill(&err_with),
        "[Stats] Backfill: 393984 px in 8 tile(s) filled from coarser zooms (768 px seam samples)"
    );
    assert_eq!(
        backfill(&err_blanked),
        "[Stats] Backfill: 393984 px in 8 tile(s) filled from coarser zooms (0 px seam samples)"
    );
    // The z14 and z13 parents were asked once each — the seam's parent too.
    assert_eq!(requests_at(&counts, 14), 2);
    assert_eq!(requests_at(&counts, 13), 1);
    // Plausible ground everywhere: nothing below 1,500 m, no void sample.
    let s = all_samples(&with);
    assert_eq!(s.len(), 3 * 320 * 320);
    let min = *s.iter().min().unwrap();
    assert!(min >= 3000, "lowest sample {} m", min as f64 / 2.0);
    for i in 0..3 {
        assert!(err_with.lines().any(|l| l == format!("[Tile] z15_{i}.abt voids=0")), "{err_with}");
    }
    // The per-tile lines never reach the console.
    assert!(!err_with.contains("[Backfill]"), "{err_with}");
}

#[test]
fn with_the_filter_off_the_seam_reaches_the_output() {
    let dir = tempfile::tempdir().unwrap();
    let (port, _) = service(false);
    let (j, on) = job(dir.path(), port, "on", 15, z15_tiles(), json!({ "seam_filter": true }));
    download(&j, None);
    let (j, off) = job(dir.path(), port, "off", 15, z15_tiles(), json!({ "seam_filter": false }));
    let err = download(&j, None);
    // Only the blank tiles were filled; the seam passed as terrain.
    assert_eq!(
        backfill(&err),
        "[Stats] Backfill: 393216 px in 6 tile(s) filled from coarser zooms (0 px seam samples)"
    );
    let (a, b) = (all_samples(&on), all_samples(&off));
    let changed: Vec<(i16, i16)> = a.iter().zip(&b).filter(|(x, y)| x != y).map(|(x, y)| (*x, *y)).collect();
    assert!(!changed.is_empty());
    // Every sample the filter changed was a seam value (below 0 m) and is
    // ground now; at least one was the −9 km column.
    assert!(changed.iter().all(|&(with, without)| without < 0 && with >= 3000), "{changed:?}");
    assert!(changed.iter().any(|&(_, without)| without < -16000), "{changed:?}");
    // The −9 km samples are below the validity floor: counted as voids
    // without the filter, in the tile over the seam.
    let voids: u64 = err
        .lines()
        .filter_map(|l| l.strip_prefix("[Tile] ")?.rsplit_once(" voids=")?.1.parse::<u64>().ok())
        .sum();
    assert!(voids > 0, "{err}");
}

/// Remove the zoom-`zoom` packs of every service under the raw tile cache
/// `cache`, so its tiles of that zoom have no entry (they must be derived).
fn drop_zoom(cache: &Path, zoom: u32) {
    let mut n = 0;
    for svc in std::fs::read_dir(cache).unwrap().flatten() {
        let d = svc.path().join(format!("z{zoom}"));
        if d.is_dir() {
            std::fs::remove_dir_all(&d).unwrap();
            n += 1;
        }
    }
    assert_eq!(n, 1, "one service, one z{zoom} directory");
}

#[test]
fn cached_and_derived_tiles_are_filtered_like_fetched_ones() {
    let dir = tempfile::tempdir().unwrap();
    let (port, counts) = service(false);
    let (blank_port, _) = service(true);
    let cache = dir.path().join("cache");
    let blank_cache = dir.path().join("blank_cache");

    // The z15 pass fills the cache; the same job again reads every tile
    // from it — no request — and writes the same bytes and the same line.
    let (j, fetched) = job(dir.path(), port, "z15_fetched", 15, z15_tiles(), json!({ "cache_dir": cache }));
    let first = download(&j, None);
    let n15 = requests_at(&counts, 15);
    let (j, hits) = job(dir.path(), port, "z15_hits", 15, z15_tiles(), json!({ "cache_dir": cache }));
    let second = download(&j, None);
    assert_eq!(requests_at(&counts, 15), n15, "every z15 tile from the cache");
    assert!(second.contains("[Download] cache: 8 of 8 source tiles from cache (0 derived)"), "{second}");
    assert_eq!(read_dir_bytes(&fetched), read_dir_bytes(&hits));
    assert_eq!(backfill(&first), backfill(&second));

    // z14, fetched from the service: its seam column is voided at z14 (256
    // samples), 8646 is blank; all filled from z13 — the bytes of a run
    // without the filter over the blanked tiles.
    let (j, z14_fetched) = job(dir.path(), port, "z14_fetched", 14, z14_tiles(), json!({}));
    let err_fetched = download(&j, None);
    assert_eq!(
        backfill(&err_fetched),
        "[Stats] Backfill: 65792 px in 2 tile(s) filled from coarser zooms (256 px seam samples)"
    );
    let (j, z14_blank) = job(dir.path(), blank_port, "z14_fetched_blank", 14, z14_tiles(), json!({ "seam_filter": false }));
    download(&j, None);
    assert_eq!(read_dir_bytes(&z14_fetched), read_dir_bytes(&z14_blank));

    // z14 from the cache: the z15 pass's parent fill stored the service's
    // z14 tiles, so they are hits — filtered like the fetched ones.
    let z14_requests = requests_at(&counts, 14);
    let (j, z14_hits) = job(dir.path(), port, "z14_hits", 14, z14_tiles(), json!({ "cache_dir": cache }));
    let err_hits = download(&j, None);
    assert!(err_hits.contains("[Download] cache: 2 of 2 source tiles from cache (0 derived)"), "{err_hits}");
    assert_eq!(requests_at(&counts, 14), z14_requests);
    assert_eq!(backfill(&err_hits), backfill(&err_fetched));
    assert_eq!(read_dir_bytes(&z14_hits), read_dir_bytes(&z14_fetched));

    // Without its z14 entries the cache derives both z14 tiles from the
    // z15 ones — no z14 request. The seam reaches z14 voided (128 samples:
    // column 26 of the rows over y 11552; over y 11553 column 52 was
    // terrain), the blank z15 tiles as voids (all of 8646, the east half of
    // 8647); all of it filled from z13.
    drop_zoom(&cache, 14);
    let (j, derived) = job(dir.path(), port, "z14_derived", 14, z14_tiles(), json!({ "cache_dir": cache }));
    let err = download(&j, None);
    assert!(err.contains("[Download] cache: 2 of 2 source tiles from cache (2 derived)"), "{err}");
    assert_eq!(requests_at(&counts, 14), z14_requests, "derived, not fetched");
    assert_eq!(
        backfill(&err),
        "[Stats] Backfill: 98432 px in 2 tile(s) filled from coarser zooms (128 px seam samples)"
    );
    // The same from blanked tiles without the filter: identical bytes.
    let (j, _) = job(dir.path(), blank_port, "z15_blank", 15, z15_tiles(),
        json!({ "cache_dir": blank_cache, "seam_filter": false }));
    download(&j, None);
    drop_zoom(&blank_cache, 14);
    let (j, derived_blank) = job(dir.path(), blank_port, "z14_derived_blank", 14, z14_tiles(),
        json!({ "cache_dir": blank_cache, "seam_filter": false }));
    let err_blank = download(&j, None);
    assert!(err_blank.contains("(2 derived)"), "{err_blank}");
    assert_eq!(read_dir_bytes(&derived), read_dir_bytes(&derived_blank));

    // A later z14 run reads the derived tiles the first one stored — their
    // seam voids kept as seam voids (RGB 0, 0, 1) — and writes the same.
    let (j, again) = job(dir.path(), port, "z14_again", 14, z14_tiles(), json!({ "cache_dir": cache }));
    let err_again = download(&j, None);
    assert!(err_again.contains("[Download] cache: 2 of 2 source tiles from cache (0 derived)"), "{err_again}");
    assert_eq!(read_dir_bytes(&derived), read_dir_bytes(&again));
    assert_eq!(backfill(&err), backfill(&err_again));
}

#[test]
fn each_backfilled_tile_has_a_line_in_the_diagnostic_file_only() {
    let dir = tempfile::tempdir().unwrap();
    let (port, _) = service(false);
    let diag = dir.path().join("diag.log");
    let (j, _) = job(dir.path(), port, "out", 15, z15_tiles(), json!({}));
    let err = download(&j, Some(&diag));
    let file = std::fs::read_to_string(&diag).unwrap();
    let mut lines: Vec<&str> = file.lines().filter(|l| l.starts_with("[Backfill] ")).collect();
    lines.sort_unstable();
    assert_eq!(lines, [
        "[Backfill] z=15 x=17292 y=11552: 65536 void px (0 seam), 65536 filled (z13 65536), 0 left void",
        "[Backfill] z=15 x=17292 y=11553: 65536 void px (0 seam), 65536 filled (z13 65536), 0 left void",
        "[Backfill] z=15 x=17293 y=11552: 65536 void px (0 seam), 65536 filled (z13 65536), 0 left void",
        "[Backfill] z=15 x=17293 y=11553: 65536 void px (0 seam), 65536 filled (z13 65536), 0 left void",
        "[Backfill] z=15 x=17294 y=11552: 512 void px (512 seam), 512 filled (z13 512), 0 left void",
        "[Backfill] z=15 x=17294 y=11553: 256 void px (256 seam), 256 filled (z13 256), 0 left void",
        "[Backfill] z=15 x=17295 y=11552: 65536 void px (0 seam), 65536 filled (z14 65536), 0 left void",
        "[Backfill] z=15 x=17295 y=11553: 65536 void px (0 seam), 65536 filled (z14 65536), 0 left void",
    ]);
    // Not on the console; the [Stats] line is on both, like the whole block.
    assert!(!err.contains("[Backfill]"), "{err}");
    let stats = backfill(&err);
    assert!(file.lines().any(|l| l == stats), "{file}");
}
