//! Streaming ingest (CONTRACT changelog 23): row bands + chunk-wise decode
//! must give the SAME bytes as the whole-file path (`load_source_to_ram`, the
//! historical decoder, handed in as caller-supplied images), with decoded
//! memory bounded by the band and cache budgets.

mod common;

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use aether_converter::ingest::{
    cache_key, load_source_to_ram, process_tile_opts, ChunkCache, ImageCache, IngestJob,
    IngestOptions, LoadedImage, VOID_ELEV,
};
use common::*;

// ── Runners ───────────────────────────────────────────────────────────────

fn empty_cache() -> ImageCache {
    Arc::new(Mutex::new(HashMap::new()))
}

/// Run *json* with *cache* and *opts*; return the `.abt` bytes and the stats.
fn run(
    json: &serde_json::Value,
    cache: ImageCache,
    opts: &IngestOptions,
) -> anyhow::Result<(Vec<u8>, aether_converter::ingest::TileStats)> {
    let job: IngestJob = serde_json::from_value(json.clone()).unwrap();
    let out = job.output_path.clone();
    let stats = process_tile_opts(job, cache, None, opts)?;
    Ok((std::fs::read(out).unwrap(), stats))
}

/// The historical whole-file path: every source decoded WHOLE up front by
/// `load_source_to_ram` and handed in as a caller-supplied image.
fn whole_file_cache(json: &serde_json::Value) -> ImageCache {
    let job: IngestJob = serde_json::from_value(json.clone()).unwrap();
    let mut map: HashMap<_, Arc<LoadedImage>> = HashMap::new();
    for s in job.effective_sources().unwrap() {
        let img = load_source_to_ram(&s.path, s.nodata).unwrap();
        map.insert(cache_key(&s), img);
    }
    Arc::new(Mutex::new(map))
}

fn opts(band: usize, cache: usize) -> IngestOptions {
    IngestOptions { band_budget_bytes: band, chunk_cache: Some(Arc::new(ChunkCache::new(cache))) }
}

/// Run the real CLI over *json*; `(exit ok, stdout + stderr)`.
// ── Mixed fixture set ─────────────────────────────────────────────────────

/// Tile: 96 px at 10 m, NW corner near Bern.
const UL_LAT: f64 = 46.952;
const UL_LON: f64 = 7.436;
const SIZE: usize = 96;
const RES_M: f64 = 10.0;

/// Approximate LV95 (EPSG:2056) coordinates of the tile's NW corner — the
/// projected fixtures are placed relative to it (exactness is irrelevant:
/// the test compares two decode paths, not absolute positions).
const E0: f64 = 2_599_800.0;
const N0: f64 = 1_200_100.0;

/// Four sources of every shape the streaming path must reproduce:
/// A — LV95 Int16, 2.5 m, Deflate tiles 16x16 with ragged edges, GDAL_NODATA
///     hole; B — LV95 Float32, 3 m, 7-row Deflate strips, NaN voids, overlaps
///     A; C — WGS84 UInt16, non-square pixels, 32x32 tiles, explicit nodata
///     override, misses the SE corner; D — a coarse `.abt` pool tile last.
fn mixed_sources(dir: &Path) -> serde_json::Value {
    let a = dir.join("a_lv95_i16_tiled.tif");
    let (w, h) = (203u32, 197u32);
    let mut va: Vec<i16> = (0..w * h).map(|i| (1000 + (i % w) + 3 * (i / w)) as i16).collect();
    for y in 60..90 {
        for x in 80..120 {
            va[(y * w + x) as usize] = -32768; // nodata hole -> falls through to B/C
        }
    }
    write_tif(
        &a,
        &Tif {
            w,
            h,
            px: Px::I16(va),
            layout: Layout::Tiles(16, 16),
            compression: 8,
            geo: Geo::TieScale(E0 - 100.0, N0 + 50.0, 2.5, 2.5),
            geokeys: &[(3072, 2056)],
            nodata: Some("-32768"),
            gdal_metadata: None,
        },
    );

    let b = dir.join("b_lv95_f32_strips.tif");
    let (w, h) = (230u32, 200u32);
    let vb: Vec<f32> = (0..w * h)
        .map(|i| {
            let (x, y) = (i % w, i / w);
            if (x / 9 + y / 11) % 7 == 0 { f32::NAN } else { 2000.0 + x as f32 * 0.75 - y as f32 * 0.4 }
        })
        .collect();
    write_tif(
        &b,
        &Tif {
            w,
            h,
            px: Px::F32(vb),
            layout: Layout::Strips(7),
            compression: 8,
            geo: Geo::TieScale(E0 + 250.0, N0 - 150.0, 3.0, 3.0),
            geokeys: &[(3072, 2056)],
            nodata: None,
            gdal_metadata: None,
        },
    );

    let c = dir.join("c_wgs84_u16_nonsquare.tif");
    let (w, h) = (100u32, 140u32);
    let sx = 0.00008;
    let sy = 0.00005;
    let vc: Vec<u16> = (0..w * h)
        .map(|i| if i % 97 == 0 { 65535 } else { 3000 + (i % w) as u16 * 2 + (i / w) as u16 })
        .collect();
    write_tif(
        &c,
        &Tif {
            w,
            h,
            px: Px::U16(vc),
            layout: Layout::Tiles(32, 32),
            compression: 1,
            // Ends short of the tile's east and south edges: voids remain.
            geo: Geo::TieScale(UL_LON - 0.0005, UL_LAT + 0.0004, sx, sy),
            geokeys: &[(2048, 4326)],
            nodata: None,
            gdal_metadata: None,
        },
    );

    let d = dir.join("d_pool.abt");
    let pd = 30.0 / 111111.0;
    let vd: Vec<i16> = (0..20 * 20).map(|i| if i % 13 == 0 { VOID_ELEV } else { 9000 + i as i16 }).collect();
    write_abt(&d, 20, UL_LAT - 0.002, UL_LON + 0.004, pd, &vd);

    serde_json::json!([
        {"path": a},
        {"path": b},
        {"path": c, "nodata": 65535.0},
        {"path": d},
    ])
}

fn mixed_job(dir: &Path, name: &str) -> serde_json::Value {
    serde_json::json!({
        "output_path": dir.join(name),
        "format": "r16sint",
        "ul_lat": UL_LAT, "ul_lon": UL_LON,
        "resolution_m": RES_M, "size_px": SIZE,
        "sources": mixed_sources(dir),
    })
}

#[test]
fn streaming_is_byte_identical_to_the_whole_file_path_on_mixed_sources() {
    let dir = tempfile::tempdir().unwrap();
    let job = mixed_job(dir.path(), "mixed.abt");

    let (reference, ref_stats) = run(&job, whole_file_cache(&job), &IngestOptions::default()).unwrap();
    let px = payload(&reference, SIZE);
    // The fixture must exercise every source, priority fall-through and voids,
    // or identity would prove little.
    let has = |lo: i16, hi: i16| px.iter().any(|&v| v >= lo && v < hi);
    assert!(has(2000, 3000), "A (Int16 x2 = 2000..2999 half-metres) never won a pixel");
    assert!(has(4000, 5000), "B (Float32 ~2000 m) never won a pixel");
    assert!(has(6000, 7200), "C (UInt16 ~3000 m) never won a pixel");
    assert!(has(9000, 11000), "D (.abt) never won a pixel");
    assert!(px.contains(&VOID_ELEV), "no void left: the fixture must leave an uncovered corner");
    assert_eq!(ref_stats.peak_window_bytes, 0, "the reference must not have streamed anything");

    // Every band size, from one output row per band to the whole tile, and
    // every cache budget, down to 0 (nothing admitted; per-tile hold only).
    for (band, cache) in [
        (1usize, 0usize),
        (1, 1 << 20),
        (20_000, 64 << 10),
        (200_000, 0),
        (usize::MAX, 64 << 20),
    ] {
        let o = opts(band, cache);
        let (bytes, stats) = run(&job, empty_cache(), &o).unwrap();
        assert!(
            bytes == reference,
            "band budget {band} / cache {cache}: streamed bytes differ from the whole-file path \
             ({} of {} pixels)",
            payload(&bytes, SIZE).iter().zip(&px).filter(|(a, b)| a != b).count(),
            SIZE * SIZE
        );
        assert_eq!(stats.covered_px, ref_stats.covered_px);
        assert!(stats.peak_window_bytes > 0, "band {band}: nothing was streamed");
        if band == 1 {
            // One row per band wherever a row needs any source pixels (the
            // all-void south strip needs none and goes in one band).
            assert!(stats.bands as usize > SIZE * 3 / 4, "only {} bands", stats.bands);
        }
        let cache = o.chunk_cache.unwrap();
        assert!(cache.peak_bytes() <= cache.budget(), "chunk cache over budget");
    }

    // A shared cache reused across tiles (the batch shape) changes nothing.
    let shared = opts(50_000, 1 << 20);
    for _ in 0..2 {
        let (bytes, _) = run(&job, empty_cache(), &shared).unwrap();
        assert!(bytes == reference, "a warm shared chunk cache changed the bytes");
    }
}

/// A deflate-tiled geographic source of `n x n` Int16 pixels, `src_deg` per
/// pixel, NW corner at the test tile's.
fn big_tiled_source(path: &Path, n: u32, src_deg: f64) {
    let v: Vec<i16> = (0..n * n).map(|i| ((i % n) / 3 + (i / n) / 5) as i16).collect();
    write_tif(
        path,
        &Tif {
            w: n,
            h: n,
            px: Px::I16(v),
            layout: Layout::Tiles(128, 128),
            compression: 8,
            geo: Geo::TieScale(8.0, 47.0, src_deg, src_deg),
            geokeys: &[(2048, 4326)],
            nodata: None,
            gdal_metadata: None,
        },
    );
}

#[test]
fn a_tile_never_holds_more_decoded_source_than_its_budgets() {
    // 2048^2 Int16 = 8 MiB decoded — the whole-file path holds all of it.
    // With a 512 KiB band budget and a 256 KiB chunk cache the streamed tile
    // must stay inside both, and still produce the identical bytes.
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("big.tif");
    let n = 2048u32;
    let out_px = 128usize;
    let pixel_deg = 30.0 / 111111.0;
    big_tiled_source(&src, n, pixel_deg / 16.0);
    let job = serde_json::json!({
        "output_path": dir.path().join("big.abt"),
        "ul_lat": 47.0, "ul_lon": 8.0,
        "resolution_m": 30.0, "size_px": out_px,
        "sources": [{"path": src}],
    });

    let band_budget = 512 << 10;
    let cache_budget = 256 << 10;
    let o = opts(band_budget, cache_budget);
    let (bytes, stats) = run(&job, empty_cache(), &o).unwrap();
    let cache = o.chunk_cache.unwrap();
    let decoded = (n as u64) * (n as u64) * 2;
    println!(
        "decoded source {} KiB; peak band windows {} KiB in {} bands; peak chunk cache {} KiB",
        decoded >> 10,
        stats.peak_window_bytes >> 10,
        stats.bands,
        cache.peak_bytes() >> 10
    );
    assert!(stats.peak_window_bytes <= band_budget as u64, "band windows over budget");
    assert!(cache.peak_bytes() <= cache_budget, "chunk cache over budget");
    assert!(stats.bands >= 16, "the budget must have forced banding ({} bands)", stats.bands);
    assert!(stats.peak_window_bytes + (cache_budget as u64) < decoded / 8);
    assert_eq!(stats.covered_px, (out_px * out_px) as u64);

    let (reference, _) = run(&job, whole_file_cache(&job), &IngestOptions::default()).unwrap();
    assert!(bytes == reference, "banded output differs from the whole-file path");
}

/// A stack whose top source covers the whole tile but has void holes: an
/// `.abt` of 300 rows (two 256-row strips on the streaming path) above an
/// LV95 tile that lies inside it and a geographic base under both. The LV95
/// tile is shadowed — its window is decoded only when the sampler falls
/// through a hole to it — and the `.abt` is read strip by strip. Neither may
/// change a byte against the whole-file path.
#[test]
fn a_shadowed_stack_streams_byte_identical_to_the_whole_file_path() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();

    let top = d.join("top.abt");
    let n = 300usize;
    let deg = 5.0 / 111111.0;
    let mut vt: Vec<i16> = (0..n * n).map(|i| 9000 + (i % n) as i16 + (i / n) as i16).collect();
    for (r0, r1, c0, c1) in [(20, 60, 10, 60), (150, 170, 150, 200), (0, n, 120, 121)] {
        for r in r0..r1 {
            for c in c0..c1 {
                vt[r * n + c] = VOID_ELEV;
            }
        }
    }
    write_abt(&top, n as u16, UL_LAT + 0.0002, UL_LON - 0.0002, deg, &vt);

    let mid = d.join("mid_lv95_i16.tif");
    let (w, h) = (203u32, 197u32);
    let vm: Vec<i16> = (0..w * h).map(|i| (1000 + (i % w) + 3 * (i / w)) as i16).collect();
    write_tif(
        &mid,
        &Tif {
            w,
            h,
            px: Px::I16(vm),
            layout: Layout::Tiles(16, 16),
            compression: 8,
            geo: Geo::TieScale(E0 - 100.0, N0 + 50.0, 2.5, 2.5),
            geokeys: &[(3072, 2056)],
            nodata: None,
            gdal_metadata: None,
        },
    );

    let base = d.join("base_wgs84_u16.tif");
    let (w, h) = (120u32, 120u32);
    let vb: Vec<u16> = (0..w * h).map(|i| 3000 + (i % w) as u16 + (i / w) as u16).collect();
    write_tif(
        &base,
        &Tif {
            w,
            h,
            px: Px::U16(vb),
            layout: Layout::Strips(9),
            compression: 1,
            geo: Geo::TieScale(UL_LON - 0.001, UL_LAT + 0.001, 0.0001, 0.0001),
            geokeys: &[(2048, 4326)],
            nodata: None,
            gdal_metadata: None,
        },
    );

    let job = serde_json::json!({
        "output_path": d.join("stack.abt"),
        "format": "r16sint",
        "ul_lat": UL_LAT, "ul_lon": UL_LON,
        "resolution_m": RES_M, "size_px": SIZE,
        "sources": [{"path": top}, {"path": mid}, {"path": base}],
    });
    let (reference, _) = run(&job, whole_file_cache(&job), &IngestOptions::default()).unwrap();
    let px = payload(&reference, SIZE);
    let has = |lo: i16, hi: i16| px.iter().any(|&v| v >= lo && v < hi);
    assert!(has(9000, 10000), "the .abt never won a pixel");
    assert!(has(2000, 3000), "the shadowed LV95 tile never won a pixel through a hole");
    assert!(has(6000, 7000), "the base never won a pixel");

    for (band, cache) in [(1usize, 0usize), (30_000, 64 << 10), (usize::MAX, 64 << 20)] {
        let (bytes, _) = run(&job, empty_cache(), &opts(band, cache)).unwrap();
        assert!(bytes == reference, "band budget {band} / cache {cache}: streamed stack differs");
    }
}
