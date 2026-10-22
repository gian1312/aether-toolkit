//! CLI `download`, changelog 30: only the source tiles that touch a requested
//! output tile are fetched, and strips are sized from a RAM budget — neither
//! may change a byte of any `.abt` the run produces. Changelog 36: the strips
//! are sized from the real tile edge, learnt from the first tile before any
//! strip is sized, and that tile is not fetched twice.
//!
//! Contract surface: `docs/CONTRACT.md` §1.2 (`[Download] z=… tiles=…`,
//! `[Stats] Tiles:`, the progress line's trailing `window` field, `[Stats]
//! Tile latency:`). The tile source is a loopback HTTP server that counts
//! every request, so these tests are hermetic and assert on what went over
//! the wire.

mod common;

use std::path::{Path, PathBuf};

use aether_converter::download::{
    lat2ty, lon2tx, run_download_tuned, tx2lon, ty2lat, DownloadTuning,
};
use common::{
    read_dir_bytes, run_download_cli, serve_tiles, serve_tiles_delayed, terrarium_png, tile_ground,
    TileCounts,
};

const Z: u32 = 12;

fn origin() -> (u32, u32) {
    (lon2tx(8.0, Z), lat2ty(47.0, Z))
}

/// A square 64 px spec whose NW corner is `(lat, lon)` and whose side is
/// `side` degrees.
fn spec(name: &str, lat: f64, lon: f64, side: f64) -> serde_json::Value {
    let pd = side / 64.0;
    serde_json::json!({
        "filename": name, "ul_lat": lat, "ul_lon": lon,
        "size_px": 64, "resolution_m": pd * 111_111.0,
    })
}

fn write_job(dir: &Path, port: u16, out: &str, tiles: &[serde_json::Value]) -> (PathBuf, PathBuf) {
    let out_dir = dir.join(out);
    let job = serde_json::json!({
        "url_template": format!("http://127.0.0.1:{port}/{{z}}/{{x}}/{{y}}.png"),
        "encoding": "terrarium", "output_dir": out_dir, "zoom": Z,
        "max_connections": 64, "tiles": tiles,
    });
    let path = dir.join(format!("{out}.json"));
    std::fs::write(&path, serde_json::to_vec_pretty(&job).unwrap()).unwrap();
    (path, out_dir)
}

fn fetched(counts: &TileCounts) -> Vec<(u32, u32)> {
    let mut v: Vec<(u32, u32)> = counts.lock().unwrap().iter()
        .map(|(&(z, x, y), &n)| {
            assert_eq!((z, n), (Z, 1), "tile {z}/{x}/{y} requested {n} time(s)");
            (x, y)
        })
        .collect();
    v.sort();
    v
}

fn server() -> (u16, TileCounts) {
    serve_tiles(|z, x, y| (z == Z).then(|| terrarium_png(tile_ground(z, x, y))))
}

#[test]
fn two_far_corners_of_a_bbox_fetch_only_their_own_tiles_and_write_the_same_bytes() {
    // Spec A starts at the centre of tile (tx0, ty0) and spans 0.8 tile
    // widths: it touches tx0..=tx0+1 × ty0..=ty0+1. Spec B does the same from
    // tile (tx0+4, ty0+3). Their bounding rectangle is 6 × 5 = 30 tiles, of
    // which 8 touch a spec. Every build before changelog 30 fetched all 30.
    let (tx0, ty0) = origin();
    let w = tx2lon(tx0 + 1, Z) - tx2lon(tx0, Z);
    let centre = |x: u32, y: u32| ((ty2lat(y, Z) + ty2lat(y + 1, Z)) / 2.0,
                                   (tx2lon(x, Z) + tx2lon(x + 1, Z)) / 2.0);
    let (la, oa) = centre(tx0, ty0);
    let (lb, ob) = centre(tx0 + 4, ty0 + 3);
    let tiles = [spec("a.abt", la, oa, 0.8 * w), spec("b.abt", lb, ob, 0.8 * w)];
    let mut want: Vec<(u32, u32)> = Vec::new();
    for (x, y) in [(tx0, ty0), (tx0 + 4, ty0 + 3)] {
        for dx in 0..2 { for dy in 0..2 { want.push((x + dx, y + dy)); } }
    }
    want.sort();

    // The real CLI, default behaviour: the 8 tiles, each once.
    let dir = tempfile::tempdir().unwrap();
    let (port, counts) = server();
    let (job, out_filtered) = write_job(dir.path(), port, "filtered", &tiles);
    let (ok, err) = run_download_cli(&job);
    assert!(ok, "{err}");
    assert_eq!(fetched(&counts), want, "only the tiles that touch a spec go over the wire");
    // The header's 4th number is what the run fetches (the plugin's pass total).
    assert!(err.lines().any(|l| l.starts_with(&format!("[Download] z={Z} tiles=6x5=8 "))), "{err}");
    assert!(err.lines().any(|l| l == "[Stats] Tiles: 8/8 OK (100.0% success)"), "{err}");
    let last = err.lines().filter(|l| l.starts_with("[Download] 100% (8/8) — ")).last()
        .unwrap_or_else(|| panic!("no final progress line; stderr:\n{err}"));
    assert!(last.contains(" in-flight, window 64"), "{last}");
    // Changelog 36: the latency line gains the mean wait for a response head.
    let lat = err.lines().find(|l| l.starts_with("[Stats] Tile latency: min="))
        .unwrap_or_else(|| panic!("no latency line; stderr:\n{err}"));
    let fb = lat.split(" first-byte avg=").nth(1)
        .and_then(|v| v.strip_suffix("ms")).and_then(|v| v.parse::<u64>().ok());
    assert!(fb.is_some(), "want `… max=<n>ms first-byte avg=<n>ms`: {lat}");

    // The whole rectangle, as before (comparison run): 30 tiles, same bytes.
    let (port, counts) = server();
    let (job, out_whole) = write_job(dir.path(), port, "whole", &tiles);
    let report = run_download_tuned(&job, DownloadTuning { fetch_whole_rect: true, ..Default::default() })
        .unwrap();
    assert_eq!(report.tiles_fetched, 30);
    assert_eq!(fetched(&counts).len(), 30);

    let (a, b) = (read_dir_bytes(&out_filtered), read_dir_bytes(&out_whole));
    assert_eq!(a.len(), 2);
    assert_eq!(a.len(), b.len());
    for ((na, ba), (nb, bb)) in a.iter().zip(&b) {
        assert_eq!(na, nb);
        assert!(ba == bb, "{na}: the filtered fetch changed the output");
    }
    // Sanity: real terrain, not void.
    let v = i16::from_le_bytes([a[0].1[44], a[0].1[45]]);
    assert!(v > 0, "first pixel of {} is {v}", a[0].0);
}

#[test]
fn strips_of_32_3_and_1_tile_rows_write_the_same_bytes() {
    // Nine square specs stacked down one column, each 1.1 tile widths tall
    // (~1.6 tile-rows), 0.7 tile-row apart: 8 tile-rows, every strip boundary
    // crossed by an output tile, so the 3-row run writes those tiles from two
    // strips each.
    let (tx0, ty0) = origin();
    let w = tx2lon(tx0 + 1, Z) - tx2lon(tx0, Z);
    let row_h = ty2lat(ty0, Z) - ty2lat(ty0 + 1, Z);
    let tiles: Vec<serde_json::Value> = (0..9)
        .map(|k| spec(&format!("s{k}.abt"), ty2lat(ty0, Z) - 0.01 * row_h - k as f64 * 0.7 * row_h,
                      tx2lon(tx0, Z) + 0.1 * w, 1.1 * w))
        .collect();

    let dir = tempfile::tempdir().unwrap();
    let mut outs = Vec::new();
    for (i, cap) in [None, Some(3), Some(1)].into_iter().enumerate() {
        let (port, counts) = server();
        let (job, out) = write_job(dir.path(), port, &format!("run{i}"), &tiles);
        let report = run_download_tuned(&job, DownloadTuning { max_strip_rows: cap, ..Default::default() })
            .unwrap();
        let rows = fetched(&counts).iter().map(|t| t.1).collect::<std::collections::BTreeSet<_>>();
        assert_eq!(rows.len(), 8, "the job spans 8 tile-rows");
        match cap {
            None => assert_eq!((report.strips, report.max_strip_rows), (1, 8), "{report:?}"),
            Some(3) => assert_eq!((report.strips, report.max_strip_rows), (3, 3), "{report:?}"),
            _ => assert_eq!((report.strips, report.max_strip_rows), (8, 1), "{report:?}"),
        }
        outs.push(read_dir_bytes(&out));
    }
    assert_eq!(outs[0].len(), 9);
    for run in &outs[1..] {
        assert_eq!(run.len(), outs[0].len());
        for ((na, a), (nb, b)) in outs[0].iter().zip(run) {
            assert_eq!(na, nb);
            assert!(a == b, "{na}: strip boundaries changed the output");
        }
    }
}

/// Specs covering exactly the tiles `tx0..tx0+4 × ty0..ty0+16`: 64 px squares
/// 3.8 tile widths wide, stacked down the column, the last one pulled up so
/// it ends inside tile-row 15.
fn four_by_sixteen() -> Vec<serde_json::Value> {
    let (tx0, ty0) = origin();
    let w = tx2lon(tx0 + 1, Z) - tx2lon(tx0, Z);
    let side = 3.8 * w;
    let (top, bot) = (ty2lat(ty0, Z), ty2lat(ty0 + 16, Z));
    let margin = 0.01 * (ty2lat(ty0 + 15, Z) - bot);
    let lon = tx2lon(tx0, Z) + 0.1 * w;
    let mut tiles = Vec::new();
    let mut lat = top - margin;
    while lat - side > bot + margin {
        tiles.push(spec(&format!("s{}.abt", tiles.len()), lat, lon, side));
        lat -= side;
    }
    tiles.push(spec(&format!("s{}.abt", tiles.len()), bot + margin + side, lon, side));
    tiles
}

/// A 16 MiB budget: a strip may hold 8 MiB — 8 tile-rows of four 256 px
/// tiles, or 2 of four 512 px ones.
const BUDGET: usize = 16 << 20;

fn same_bytes(a: &Path, b: &Path) {
    let (a, b) = (read_dir_bytes(a), read_dir_bytes(b));
    assert!(!a.is_empty());
    assert_eq!(a.len(), b.len());
    for ((na, ba), (nb, bb)) in a.iter().zip(&b) {
        assert_eq!(na, nb);
        assert!(ba == bb, "{na}: strip sizing changed the output");
    }
}

#[test]
fn the_first_tile_sets_the_edge_before_any_strip_is_sized() {
    // Every build before changelog 36 sized strips for 512 px tiles until a
    // tile had decoded: 2 rows for the first strip here, 8 only for the later
    // ones. The first tile is now fetched alone before any strip is sized, so
    // the first strip already gets the 8 rows 256 px tiles allow — and that
    // tile goes to its strip instead of being fetched again.
    let tiles = four_by_sixteen();
    let dir = tempfile::tempdir().unwrap();
    let (port, counts) = server();
    let (job, out) = write_job(dir.path(), port, "probed", &tiles);
    let report = run_download_tuned(&job, DownloadTuning { ram_budget: Some(BUDGET), ..Default::default() })
        .unwrap();
    assert_eq!(report.tiles_fetched, 64, "{report:?}");
    assert_eq!((report.first_strip_rows, report.strips, report.max_strip_rows), (8, 2, 8),
               "both strips sized from the probed 256 px edge: {report:?}");
    let got = fetched(&counts); // asserts every tile was requested exactly once
    assert_eq!(got.len(), 64);
    assert_eq!(got.iter().map(|t| t.1).collect::<std::collections::BTreeSet<_>>().len(), 16);

    let (port, _) = server();
    let (job1, out1) = write_job(dir.path(), port, "rows1", &tiles);
    run_download_tuned(&job1, DownloadTuning { max_strip_rows: Some(1), ..Default::default() }).unwrap();
    same_bytes(&out, &out1);
}

#[test]
fn a_failed_or_slow_edge_probe_sizes_the_first_strip_for_512_px_and_keeps_its_tile() {
    // The probe is the first tile of the first tile-row. (1) It 404s at every
    // zoom: no edge comes from it. (2) It answers after 6 s, past the 5 s the
    // run waits for it. Either way the first strip is sized as before
    // changelog 36 — for 512 px tiles, 2 rows — the run completes, every
    // tile is requested once (the slow probe is not re-fetched by its strip),
    // and the bytes are those of 1-row strips over the same answers.
    let (tx0, ty0) = origin();
    let tiles = four_by_sixteen();
    for (label, lost, delay_ms) in [("lost", true, 0u64), ("slow", false, 6000)] {
        let dir = tempfile::tempdir().unwrap();
        let route = move |z: u32, x: u32, y: u32, delay: u64| {
            let probe = (x, y) == (tx0, ty0);
            (z == Z && !(lost && probe)).then(|| {
                (terrarium_png(tile_ground(z, x, y)), if probe { delay } else { 0 })
            })
        };
        let (port, counts) = serve_tiles_delayed(move |z, x, y| route(z, x, y, delay_ms));
        let (job, out) = write_job(dir.path(), port, label, &tiles);
        let t0 = std::time::Instant::now();
        let report = run_download_tuned(&job, DownloadTuning { ram_budget: Some(BUDGET), ..Default::default() })
            .unwrap();
        assert_eq!(report.first_strip_rows, 2, "{label}: sized for 512 px: {report:?}");
        let at_z: Vec<usize> = counts.lock().unwrap().iter()
            .filter(|((z, _, _), _)| *z == Z).map(|(_, &n)| n).collect();
        assert_eq!(at_z.len(), 64, "{label}");
        assert!(at_z.iter().all(|&n| n == 1), "{label}: a tile was requested twice: {at_z:?}");
        if delay_ms > 0 {
            assert!(t0.elapsed().as_millis() as u64 >= delay_ms, "{label}: the probe's tile was not waited for");
        }

        let (port, _) = serve_tiles_delayed(move |z, x, y| route(z, x, y, 0));
        let (job1, out1) = write_job(dir.path(), port, &format!("{label}_rows1"), &tiles);
        run_download_tuned(&job1, DownloadTuning { max_strip_rows: Some(1), ..Default::default() }).unwrap();
        same_bytes(&out, &out1);
    }
}
