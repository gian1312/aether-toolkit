//! `[Tile] <name> voids=<n>` (CONTRACT §1.2, changelog 46): every output tile
//! a CLI `download` writes, and every tile a batch `ingest` writes, is
//! reported with the number of void samples the file holds when the run ends
//! — samples at or below the validity floor of -5000 half-metres (-2500 m):
//! the `-9999` sentinel and deep water alike — after the strip assembly and
//! the buildings post-pass (download), after `void_fill_m` and the burns
//! (ingest), so a consumer need not read every tile back to find its voids.
//! A repair pass prints no such line. Each count here is checked against an
//! independent count of the written file.
//!
//! Hermetic: terrain from a loopback XYZ server (`common::serve_tiles`) or
//! `.abt` sources written by the test; building tiles are MVT written here.

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use aether_converter::download::{lat2ty, lon2tx, tx2lon};
use aether_converter::mvt::{Feature, Layer, Tile, Value};
use common::{payload, run_download_cli, serve_tiles, terrarium_png, tile_ground, write_abt};
use prost::Message;
use serde_json::json;

const VOID: i16 = -9999;
/// At or below this (half-metres) a sample is no ground.
const FLOOR: i16 = -5000;

/// The samples of the `.abt` at *path*, read back from the file.
fn samples(path: &Path) -> Vec<i16> {
    let bytes = std::fs::read(path).unwrap();
    let size = u16::from_le_bytes([bytes[6], bytes[7]]) as usize;
    payload(&bytes, size)
}

/// The void samples of the `.abt` at *path* (at or below [`FLOOR`]), read
/// back from the file.
fn voids_in_file(path: &Path) -> u64 {
    samples(path).iter().filter(|&&v| v <= FLOOR).count() as u64
}

/// How many samples of the `.abt` at *path* are exactly *v*.
fn count_of(path: &Path, v: i16) -> usize {
    samples(path).iter().filter(|&&s| s == v).count()
}

/// A 256 px Terrarium PNG whose sample `(px, py)` is `f(px, py)` metres, to
/// 1/256 m (`R·65536 + G·256 + B = (v + 32768)·256`).
fn terrarium_png_m(f: impl Fn(usize, usize) -> f64) -> Vec<u8> {
    let mut px = Vec::with_capacity(256 * 256 * 3);
    for py in 0..256 {
        for x in 0..256 {
            let t = ((f(x, py) + 32768.0) * 256.0).round() as u32;
            px.extend_from_slice(&[(t >> 16) as u8, (t >> 8) as u8, t as u8]);
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

/// `(name, n)` of every `[Tile] <name> voids=<n>` line, in order.
fn void_lines(text: &str) -> Vec<(String, u64)> {
    text.lines()
        .filter_map(|l| {
            let rest = l.strip_prefix("[Tile] ")?;
            let (name, n) = rest.rsplit_once(" voids=")?;
            Some((name.to_owned(), n.parse().ok()?))
        })
        .collect()
}

// ── Building tiles (MVT) ────────────────────────────────────────────────────

const BZ: u32 = 14;

fn zz(n: i32) -> u32 {
    ((n << 1) ^ (n >> 31)) as u32
}

/// A `building` layer: one ring (tile units, y down) per
/// `(render_height, ring)`.
fn building_tile(features: &[(f64, Vec<(i32, i32)>)]) -> Vec<u8> {
    let mut values = Vec::new();
    let mut feats = Vec::new();
    for (i, (h, r)) in features.iter().enumerate() {
        values.push(Value { double_val: Some(*h), ..Default::default() });
        let mut geometry = vec![(1 << 3) | 1, zz(r[0].0), zz(r[0].1), (((r.len() - 1) as u32) << 3) | 2];
        let mut c = r[0];
        for &p in &r[1..] {
            geometry.extend([zz(p.0 - c.0), zz(p.1 - c.1)]);
            c = p;
        }
        geometry.push((1 << 3) | 7);
        feats.push(Feature { id: Some(i as u64 + 1), tags: vec![0, i as u32], r#type: Some(3), geometry });
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

/// Fractional z14 tile coordinates of a point (the decoder's own Mercator).
fn tile_xy(lon: f64, lat: f64) -> (f64, f64) {
    let n = (1u32 << BZ) as f64;
    let x = (lon + 180.0) / 360.0 * n;
    let y = (1.0 - lat.to_radians().tan().asinh() / std::f64::consts::PI) / 2.0 * n;
    (x, y)
}

/// Writes one z14 building tile per house — a rectangle from (`w`, `n`) to
/// (`e`, `s`) in degrees, its ring in the units of the tile holding its
/// north-west corner — into *dir* (created).
fn write_houses(dir: &Path, houses: &[((f64, f64, f64, f64), f64)]) {
    std::fs::create_dir_all(dir).unwrap();
    let mut tiles: std::collections::BTreeMap<(u32, u32), Vec<(f64, Vec<(i32, i32)>)>> = Default::default();
    for &((w, n, e, s), h) in houses {
        let (x0, y0) = tile_xy(w, n);
        let (x1, y1) = tile_xy(e, s);
        let (tx, ty) = (x0.floor(), y0.floor());
        let u = |v: f64, t: f64| ((v - t) * 4096.0).round() as i32;
        let ring = vec![(u(x0, tx), u(y0, ty)), (u(x1, tx), u(y0, ty)), (u(x1, tx), u(y1, ty)), (u(x0, tx), u(y1, ty))];
        tiles.entry((tx as u32, ty as u32)).or_default().push((h, ring));
    }
    for ((x, y), features) in tiles {
        std::fs::write(dir.join(format!("{BZ}_{x}_{y}.pbf")), building_tile(&features)).unwrap();
    }
}

// ── download ────────────────────────────────────────────────────────────────

/// Terrain source zoom.
const Z: u32 = 12;
/// Four 128 px output tiles at 30 m, side by side.
const SIZE: usize = 128;
const RES: f64 = 30.0;
const PD: f64 = RES / 111_111.0;
const UL: (f64, f64) = (47.01, 8.03); // (lat, lon)

fn specs() -> Vec<serde_json::Value> {
    (0..4)
        .map(|i| {
            json!({
                "filename": format!("t{i}.abt"),
                "ul_lat": UL.0, "ul_lon": UL.1 + (i * SIZE) as f64 * PD,
                "size_px": SIZE, "resolution_m": RES,
            })
        })
        .collect()
}

/// The source column under the third output tile's centre: the column the
/// server answers 404 for, at every zoom, so nothing fills it.
fn missing_x() -> u32 {
    lon2tx(UL.1 + 2.5 * SIZE as f64 * PD, Z)
}

/// The loopback terrain: every tile at zoom [`Z`] but column [`missing_x`]
/// (while *hole* is set), nothing at any other zoom (no parent fill).
fn terrain_server(hole: Arc<AtomicBool>) -> u16 {
    let miss = missing_x();
    serve_tiles(move |z, x, y| {
        (z == Z && !(x == miss && hole.load(Ordering::Relaxed))).then(|| terrarium_png(tile_ground(z, x, y)))
    })
    .0
}

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

/// The void lines of a finished download: one per output, in job order,
/// before the `[Stats]` block, each the file's own count. Returns the counts.
fn check_download_lines(stderr: &str, out_dir: &Path) -> Vec<u64> {
    let lines = void_lines(stderr);
    let names: Vec<String> = (0..4).map(|i| format!("t{i}.abt")).collect();
    assert_eq!(lines.iter().map(|(n, _)| n.clone()).collect::<Vec<_>>(), names, "{stderr}");
    let all: Vec<&str> = stderr.lines().collect();
    let first_stats = all.iter().position(|l| l.starts_with("[Stats] ")).expect("a [Stats] block");
    let last_tile = all.iter().rposition(|l| l.starts_with("[Tile] ")).unwrap();
    assert!(last_tile < first_stats, "the [Tile] lines come before the [Stats] block:\n{stderr}");
    for (name, n) in &lines {
        assert_eq!(*n, voids_in_file(&out_dir.join(name)), "{name}: line and file disagree\n{stderr}");
    }
    lines.into_iter().map(|(_, n)| n).collect()
}

#[test]
fn a_download_reports_each_outputs_void_samples_as_the_file_holds_them() {
    let dir = tempfile::tempdir().unwrap();
    let port = terrain_server(Arc::new(AtomicBool::new(true)));
    let (job, out) = write_job(dir.path(), port, "out", json!({}));
    let (ok, stderr) = run_download_cli(&job);
    assert!(ok, "{stderr}");
    assert!(stderr.contains("[Stats] NO-DATA (HTTP 404): "), "the missing column must 404:\n{stderr}");

    let voids = check_download_lines(&stderr, &out);
    let total = (SIZE * SIZE) as u64;
    // The geometry: t0 lies west of the missing column, t2 inside it.
    assert_eq!(voids[0], 0, "{voids:?}");
    assert!(voids[2] > 0, "{voids:?}");
    assert!(voids.iter().any(|&n| n > 0 && n < total), "one tile is part void: {voids:?}");
    // The format, exactly: `[Tile] <name> voids=<n>`, nothing after.
    assert!(stderr.lines().any(|l| l == "[Tile] t0.abt voids=0"), "{stderr}");
}

#[test]
fn a_download_with_buildings_counts_the_voids_its_burn_leaves() {
    let dir = tempfile::tempdir().unwrap();
    let port = terrain_server(Arc::new(AtomicBool::new(true)));
    // A house on terrain that reaches two pixels past the missing column's
    // west edge: its ground reference (the P10 under it) is terrain, so its
    // roof lands on the void pixels it covers and lifts them above the floor.
    // One wholly inside the column (its ground is the sentinel, its roof
    // `-9999` + height stays below the floor: still void), one on terrain.
    let edge = tx2lon(missing_x(), Z);
    let lat = UL.0 - 40.0 * PD;
    let d = 12.0 * PD;
    let houses = [
        ((edge - 28.0 * PD, lat, edge + 2.0 * PD, lat - 10.0 * PD), 15.0),
        ((edge + 4.0 * d, lat, edge + 6.0 * d, lat - 2.0 * d), 9.0),
        ((UL.1 + 20.0 * PD, lat, UL.1 + 20.0 * PD + d, lat - d), 12.0),
    ];
    let pbf = dir.path().join("pbf");
    write_houses(&pbf, &houses);

    let (job, out_with) = write_job(dir.path(), port, "with", json!({ "buildings_pbf_dir": pbf }));
    let (ok, stderr) = run_download_cli(&job);
    assert!(ok, "{stderr}");
    assert!(stderr.contains(" pbf tile(s) → 3 building(s)"), "{stderr}");
    let with = check_download_lines(&stderr, &out_with);

    let (job, out) = write_job(dir.path(), port, "without", json!({}));
    let (ok, stderr) = run_download_cli(&job);
    assert!(ok, "{stderr}");
    let without = check_download_lines(&stderr, &out);
    assert!(
        with.iter().sum::<u64>() < without.iter().sum::<u64>(),
        "the burn must have lifted void samples, or this proves nothing: {with:?} vs {without:?}"
    );
    // The house inside the column wrote `-9999` + 18 there: no longer the
    // sentinel, still void.
    let inside = (count_of(&out_with.join("t2.abt"), VOID + 18) as u64, with[2], without[2]);
    assert!(inside.0 > 0, "the inside house must have drawn: {inside:?}");
    assert_eq!(inside.1, inside.2, "a roof below the floor leaves the count alone: {inside:?}");
}

#[test]
fn a_sample_at_or_below_the_floor_is_void_and_one_above_it_is_not() {
    // One output tile inside one source tile whose west half is -3000 m
    // (-6000 half-metres: deep water, no ground for a consumer) and whose
    // east half is -2499.5 m (-4999: the deepest valid ground).
    let (tx, ty) = (lon2tx(UL.1, Z), lat2ty(UL.0, Z));
    let (w, e) = (tx2lon(tx, Z), tx2lon(tx + 1, Z));
    let n = aether_converter::download::ty2lat(ty, Z);
    let s = aether_converter::download::ty2lat(ty + 1, Z);
    let (port, _) = serve_tiles(move |z, x, y| {
        (z == Z && (x, y) == (tx, ty)).then(|| terrarium_png_m(|px, _| if px < 128 { -3000.0 } else { -2499.5 }))
    });
    let ul = (n - 0.005, w + 0.4 * (e - w));
    assert!(ul.1 + 64.0 * PD < e && ul.0 - 64.0 * PD > s, "the output must lie in the one tile");
    let dir = tempfile::tempdir().unwrap();
    let (job, out) = write_job(dir.path(), port, "deep", json!({
        "tiles": [{ "filename": "deep.abt", "ul_lat": ul.0, "ul_lon": ul.1, "size_px": 64, "resolution_m": RES }],
    }));
    let (ok, stderr) = run_download_cli(&job);
    assert!(ok, "{stderr}");
    let file = out.join("deep.abt");
    let lines = void_lines(&stderr);
    assert_eq!(lines.len(), 1, "{stderr}");
    let (deep, valid) = (count_of(&file, -6000), count_of(&file, -4999));
    assert!(deep > 0 && valid > 0, "both halves must be in the tile: {deep} / {valid}");
    assert_eq!(count_of(&file, VOID), 0, "nothing is the sentinel here");
    assert_eq!(lines[0].1, voids_in_file(&file));
    // -6000 (and the -5500 average at the seam) counts, -4999 does not.
    assert_eq!(lines[0].1 as usize, 64 * 64 - samples(&file).iter().filter(|&&v| v > FLOOR).count());
    assert!(lines[0].1 as usize >= deep && (lines[0].1 as usize) < 64 * 64 - valid + 1);

    // Ingest: a -6000 source sample is no ground to the sampler — the pixel
    // is not covered — and the fill decides: the sentinel and a -3000 m fill
    // count, a -2499.5 m fill does not; -4999 is ground and never counts.
    let src = dir.path().join("deep_src.abt");
    let data: Vec<i16> = (0..128 * 128).map(|i| if i % 128 < 64 { -6000 } else { -4999 }).collect();
    write_abt(&src, 128, IN_UL.0, IN_UL.1, IN_DEG / 2.0, &data);
    let o = |name: &str| dir.path().join("deep_out").join(format!("{name}.abt"));
    let jobs = json!([
        ingest_job(&o("bare"), IN_UL, &[&src], json!({})),
        ingest_job(&o("deep_fill"), IN_UL, &[&src], json!({ "void_fill_m": -3000.0 })),
        ingest_job(&o("valid_fill"), IN_UL, &[&src], json!({ "void_fill_m": -2499.5 })),
    ]);
    let (ok, stdout, stderr) = ingest(dir.path(), "deep_batch", &jobs);
    assert!(ok, "{stdout}\n{stderr}");
    let half = (IN_SIZE * IN_SIZE / 2) as u64;
    assert_eq!(tile_report(&stdout, "bare.abt"), (half, 2 * half, half));
    assert_eq!((count_of(&o("bare"), VOID), count_of(&o("bare"), -4999)), (half as usize, half as usize));
    assert_eq!(tile_report(&stdout, "deep_fill.abt"), (half, 2 * half, half));
    assert_eq!((count_of(&o("deep_fill"), -6000), count_of(&o("deep_fill"), -4999)), (half as usize, half as usize));
    assert_eq!(tile_report(&stdout, "valid_fill.abt"), (half, 2 * half, 0));
    assert_eq!(count_of(&o("valid_fill"), -4999), 2 * half as usize);
    for name in ["bare", "deep_fill", "valid_fill"] {
        assert_eq!(tile_report(&stdout, &format!("{name}.abt")).2, voids_in_file(&o(name)), "{name}");
    }
}

#[test]
fn a_repair_pass_prints_no_void_line() {
    let dir = tempfile::tempdir().unwrap();
    let hole = Arc::new(AtomicBool::new(true));
    let port = terrain_server(hole.clone());
    let (job, out) = write_job(dir.path(), port, "out", json!({}));
    let (ok, stderr) = run_download_cli(&job);
    assert!(ok, "{stderr}");
    let before = check_download_lines(&stderr, &out);
    assert!(before[2] > 0);

    // The column is served now; the repair patches it into the files.
    hole.store(false, Ordering::Relaxed);
    let y = lat2ty(UL.0 - 0.5 * SIZE as f64 * PD, Z);
    let (job, _) = write_job(dir.path(), port, "out", json!({ "repair_source_tiles": [[Z, missing_x(), y]] }));
    let (ok, stderr) = run_download_cli(&job);
    assert!(ok, "{stderr}");
    assert!(stderr.contains("[Download] repair: "), "{stderr}");
    assert!(stderr.contains("[Stats] Tiles: 1/1 OK"), "{stderr}");
    assert!(!stderr.lines().any(|l| l.starts_with("[Tile]")), "a repair prints no [Tile] line:\n{stderr}");
    assert!(voids_in_file(&out.join("t2.abt")) < before[2], "the repair must have filled voids");
}

// ── ingest ──────────────────────────────────────────────────────────────────

/// Output tiles: 64 px at 10 m.
const IN_SIZE: usize = 64;
const IN_DEG: f64 = 10.0 / 111_111.0;
const IN_UL: (f64, f64) = (47.005, 8.0);

/// A source `.abt` over the north-west of the output tile at (47.005, 8.0)
/// — 80 px of half an output pixel, so about 40 × 40 output pixels — with a
/// 12 × 12 hole of void samples inside: a sampled void is no coverage.
fn partial_source(dir: &Path) -> PathBuf {
    let n = 80usize;
    let data: Vec<i16> = (0..n * n)
        .map(|i| {
            let (x, y) = (i % n, i / n);
            if (30..42).contains(&x) && (30..42).contains(&y) { VOID } else { 1000 + (x + 2 * y) as i16 }
        })
        .collect();
    let p = dir.join("partial.abt");
    write_abt(&p, n as u16, IN_UL.0, IN_UL.1, IN_DEG / 2.0, &data);
    p
}

/// A source covering every tile of these tests.
fn full_source(dir: &Path) -> PathBuf {
    let n = 400usize;
    let data: Vec<i16> = (0..n * n).map(|i| 900 + ((i % n) + (i / n)) as i16).collect();
    let p = dir.join("full.abt");
    write_abt(&p, n as u16, IN_UL.0 + 0.001, IN_UL.1 - 0.001, IN_DEG / 2.0, &data);
    p
}

fn ingest_job(out: &Path, ul: (f64, f64), sources: &[&Path], extra: serde_json::Value) -> serde_json::Value {
    let mut j = json!({
        "output_path": out, "format": "r16sint",
        "ul_lat": ul.0, "ul_lon": ul.1, "resolution_m": 10.0, "size_px": IN_SIZE,
        "sources": sources.iter().map(|p| json!({ "path": p })).collect::<Vec<_>>(),
    });
    if let (Some(o), Some(e)) = (j.as_object_mut(), extra.as_object()) {
        for (k, v) in e {
            o.insert(k.clone(), v.clone());
        }
    }
    j
}

/// Run `ingest` on *jobs* (written to `<dir>/<name>.json`): `(ok, stdout, stderr)`.
fn ingest(dir: &Path, name: &str, jobs: &serde_json::Value) -> (bool, String, String) {
    let job_file = dir.join(format!("{name}.json"));
    std::fs::write(&job_file, serde_json::to_vec(jobs).unwrap()).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_aether_converter"))
        .args(["ingest", "--job-file"])
        .arg(&job_file)
        .env_remove("AETHER_DIAG_FILE")
        .output()
        .unwrap();
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// `(covered, total, voids)` of tile *name* from a batch's stdout: its
/// covered line exactly as before (nothing after `covered=<a>/<b>`), the
/// voids line right after it.
fn tile_report(stdout: &str, name: &str) -> (u64, u64, u64) {
    let lines: Vec<&str> = stdout.lines().collect();
    let head = format!("[Tile] {name} covered=");
    let at = lines.iter().position(|l| l.starts_with(&head)).unwrap_or_else(|| panic!("no covered line for {name}:\n{stdout}"));
    let (a, b) = lines[at][head.len()..].split_once('/').expect("covered=<a>/<b>");
    let (a, b): (u64, u64) = (a.parse().expect("<a>"), b.parse().expect("<b> ends the line"));
    let voids = lines[at + 1]
        .strip_prefix(&format!("[Tile] {name} voids="))
        .unwrap_or_else(|| panic!("the voids line must follow the covered line:\n{stdout}"));
    (a, b, voids.parse().expect("voids=<n> ends the line"))
}

#[test]
fn an_ingest_batch_reports_the_voids_after_its_fill() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let (part, full) = (partial_source(dir), full_source(dir));
    let out = |n: &str| dir.join("out").join(format!("{n}.abt"));
    let jobs = json!([
        ingest_job(&out("partial"), IN_UL, &[&part], json!({})),
        ingest_job(&out("filled"), IN_UL, &[&part], json!({ "void_fill_m": 0.0 })),
        ingest_job(&out("full"), IN_UL, &[&full], json!({})),
        ingest_job(&out("nothing"), (IN_UL.0 - 0.1, IN_UL.1 + 0.1), &[&part], json!({})),
    ]);
    let (ok, stdout, stderr) = ingest(dir, "batch", &jobs);
    assert!(ok, "{stdout}\n{stderr}");
    let total = (IN_SIZE * IN_SIZE) as u64;

    // Partial coverage, no fill: every pixel no source covered is void —
    // the hole in the source included — and only those.
    let (c, t, v) = tile_report(&stdout, "partial.abt");
    assert_eq!(t, total);
    assert!(c > 0 && c < t, "partly covered: {c}/{t}");
    assert_eq!(v, t - c);
    assert_eq!(v, voids_in_file(&out("partial")));

    // The same coverage with `void_fill_m`: no void left, coverage unchanged.
    let (c2, t2, v2) = tile_report(&stdout, "filled.abt");
    assert_eq!((c2, t2, v2), (c, t, 0));
    assert_eq!(voids_in_file(&out("filled")), 0);

    let (c3, t3, v3) = tile_report(&stdout, "full.abt");
    assert_eq!((c3, t3, v3), (total, total, 0));
    let (c4, t4, v4) = tile_report(&stdout, "nothing.abt");
    assert_eq!((c4, t4, v4), (0, total, total));
    assert_eq!(voids_in_file(&out("nothing")), total);

    // One covered and one voids line per tile, nothing else that starts so.
    assert_eq!(stdout.lines().filter(|l| l.starts_with("[Tile] ")).count(), 8, "{stdout}");

    // A single job (a JSON object) prints neither, as before.
    let (ok, stdout, stderr) = ingest(dir, "single", &jobs[0]);
    assert!(ok, "{stdout}\n{stderr}");
    assert!(!stdout.contains("[Tile]"), "{stdout}");
}

#[test]
fn an_ingest_burn_onto_void_samples_is_counted() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let part = partial_source(dir);
    // The source reaches 40 output pixels east and south. A house on terrain
    // whose last column (40) is past that edge — ground reference terrain, so
    // its roof lifts that column above the floor —, one wholly in the void
    // beyond (roof `-9999` + height, still void), one on the terrain.
    let px = |x: f64, y: f64| (IN_UL.1 + x * IN_DEG, IN_UL.0 - y * IN_DEG);
    let house = |(x0, y0): (f64, f64), (x1, y1): (f64, f64)| {
        let ((w, n), (e, s)) = (px(x0, y0), px(x1, y1));
        (w, n, e, s)
    };
    let pbf = dir.join("pbf");
    write_houses(&pbf, &[
        (house((22.0, 2.0), (41.0, 12.0)), 15.0),
        (house((44.0, 2.0), (50.0, 8.0)), 9.0),
        (house((5.0, 25.0), (11.0, 31.0)), 12.0),
    ]);
    let out = |n: &str| dir.join("out").join(format!("{n}.abt"));
    let jobs = json!([
        ingest_job(&out("bare"), IN_UL, &[&part], json!({})),
        ingest_job(&out("built"), IN_UL, &[&part], json!({ "buildings_pbf_dir": pbf })),
        ingest_job(&out("built_filled"), IN_UL, &[&part], json!({ "buildings_pbf_dir": pbf, "void_fill_m": 0.0 })),
    ]);
    let (ok, stdout, stderr) = ingest(dir, "batch", &jobs);
    assert!(ok, "{stdout}\n{stderr}");

    let (c, t, bare) = tile_report(&stdout, "bare.abt");
    assert_eq!(bare, t - c);
    let (c_b, _, built) = tile_report(&stdout, "built.abt");
    assert_eq!(c_b, c, "coverage is counted before the burn");
    assert_eq!(built, voids_in_file(&out("built")), "the line is the file's count");
    assert!(built < bare, "the burn must have lifted void samples: {built} vs {bare}");
    assert_eq!(bare - built, 10, "the house lifted its one void column, 10 rows");
    assert!(count_of(&out("built"), VOID + 18) > 0, "the house in the void drew below the floor");
    let (_, _, filled) = tile_report(&stdout, "built_filled.abt");
    assert_eq!((filled, voids_in_file(&out("built_filled"))), (0, 0));
}
