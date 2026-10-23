//! `buildings_pbf_dir` in a batch ingest, through the real CLI: the
//! directory decodes on the batch's pool with a progress line every 1,000
//! tiles and a summary line (CONTRACT §9a, changelog 44), an undecodable tile
//! is still skipped with its warning, and every tile comes out as the decoded
//! set burnt onto the same batch's building-less terrain.

mod common;

use std::path::Path;
use std::process::Command;

use aether_converter::buildings::{load_pbf_building_dir, rasterize_buildings, AbtGrid, RasterOpts, TileRef};
use aether_converter::download::{tx2lon, ty2lat};
use aether_converter::mvt::{Feature, Layer, Tile, Value};
use common::{write_tif, Geo, Layout, Px, Tif};
use prost::Message;

const Z: u32 = 14;
/// 50 × 42 = 2,100 building tiles, x 8520..8569, y 5740..5781.
const X0: u32 = 8520;
const Y0: u32 = 5740;
const COLS: u32 = 50;
const ROWS: u32 = 42;
const TILES: usize = (COLS * ROWS) as usize;
/// The one tile written as bytes that do not decode.
const BAD: (u32, u32) = (8545, 5760);

fn zz(n: i32) -> u32 {
    ((n << 1) ^ (n >> 31)) as u32
}

/// MVT command stream of one closed ring.
fn ring_geometry(r: &[(i32, i32)]) -> Vec<u32> {
    let mut out = vec![(1 << 3) | 1, zz(r[0].0), zz(r[0].1)];
    out.push((((r.len() - 1) as u32) << 3) | 2);
    let mut c = r[0];
    for &p in &r[1..] {
        out.extend([zz(p.0 - c.0), zz(p.1 - c.1)]);
        c = p;
    }
    out.push((1 << 3) | 7);
    out
}

/// Exterior ring (clockwise with y down) of a tile-space rectangle.
fn rect(x0: i32, y0: i32, x1: i32, y1: i32) -> Vec<(i32, i32)> {
    vec![(x0, y0), (x1, y0), (x1, y1), (x0, y1)]
}

/// A `building` layer: one polygon per `(render_height, ring)`.
fn building_tile(features: &[(f64, Vec<(i32, i32)>)]) -> Vec<u8> {
    let mut values = Vec::new();
    let mut feats = Vec::new();
    for (i, (h, ring)) in features.iter().enumerate() {
        values.push(Value { double_val: Some(*h), ..Default::default() });
        feats.push(Feature {
            id: Some(i as u64 + 1),
            tags: vec![0, i as u32],
            r#type: Some(3),
            geometry: ring_geometry(ring),
        });
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

/// Writes the building directory; returns the buildings its good tiles hold.
///
/// Each tile holds a house at its east edge, the copy of its west
/// neighbour's house in its buffer zone (the duplicate filter's case), and a
/// larger building whose size and height vary per tile.
fn write_pbf_dir(dir: &Path) -> usize {
    std::fs::create_dir_all(dir).unwrap();
    let house = |x: u32, y: u32, dx: i32| -> (f64, Vec<(i32, i32)>) {
        let k = ((x * 7 + y * 3) % 60) as i32;
        (9.0 + ((x + y) % 5) as f64, rect(4000 - dx, 900 + k, 4090 - dx, 990 + k))
    };
    let mut buildings = 0;
    for y in Y0..Y0 + ROWS {
        for x in X0..X0 + COLS {
            let name = dir.join(format!("{Z}_{x}_{y}.pbf"));
            if (x, y) == BAD {
                std::fs::write(name, b"\x1a\xff\xff\xff\x0fnot a vector tile").unwrap();
                continue;
            }
            let mut features = vec![house(x, y, 0)];
            if x > X0 {
                features.push(house(x - 1, y, 4096));
            }
            let k = ((x * 13 + y * 29) % 900) as i32;
            features.push((5.0 + ((x * y) % 30) as f64, rect(600 + k / 3, 1800, 1400 + k, 2600 + k / 2)));
            buildings += features.len();
            std::fs::write(name, building_tile(&features)).unwrap();
        }
    }
    std::fs::write(dir.join("README.txt"), b"not a tile").unwrap();
    buildings
}

/// Terrain over both output tiles: a sloped, rippled Float32 EPSG:4326
/// GeoTIFF, so above-ground heights resolve against ground that varies.
fn write_terrain(path: &Path) -> (f64, f64) {
    let (ul_lon, ul_lat) = (tx2lon(8529, Z), ty2lat(5749, Z));
    let step = 0.0002;
    let (w, h) = (800u32, 600u32);
    let px: Vec<f32> = (0..w * h)
        .map(|i| {
            let (x, y) = ((i % w) as f32, (i / w) as f32);
            420.0 + 0.35 * x - 0.2 * y + ((i % w + 3 * (i / w)) % 17) as f32
        })
        .collect();
    write_tif(
        path,
        &Tif {
            w,
            h,
            px: Px::F32(px),
            layout: Layout::Strips(16),
            compression: 8,
            geo: Geo::TieScale(ul_lon, ul_lat, step, step),
            geokeys: &[(2048, 4326)],
            nodata: None,
            gdal_metadata: None,
        },
    );
    (ul_lon, ul_lat)
}

const RES_M: f64 = 30.0;
const SIZE: u32 = 256;

/// Two output tiles inside the terrain, each over several building tiles.
fn jobs(dir: &Path, terrain: &Path, pbf: Option<&Path>, tag: &str) -> serde_json::Value {
    let corners = [(tx2lon(8530, Z), ty2lat(5750, Z)), (tx2lon(8532, Z) + 0.003, ty2lat(5751, Z) - 0.002)];
    let jobs: Vec<serde_json::Value> = corners
        .iter()
        .enumerate()
        .map(|(i, &(ul_lon, ul_lat))| {
            let mut j = serde_json::json!({
                "output_path": dir.join(format!("{tag}_{i}.abt")),
                "format": "r16sint",
                "ul_lat": ul_lat,
                "ul_lon": ul_lon,
                "resolution_m": RES_M,
                "size_px": SIZE,
                "sources": [{"path": terrain}],
            });
            if let Some(p) = pbf {
                j["buildings_pbf_dir"] = serde_json::json!(p);
            }
            j
        })
        .collect();
    serde_json::Value::Array(jobs)
}

/// The real CLI over *json*: `(exit ok, stdout, stderr)`.
fn ingest(dir: &Path, name: &str, json: &serde_json::Value) -> (bool, String, String) {
    let job_file = dir.join(name);
    std::fs::write(&job_file, serde_json::to_vec(json).unwrap()).unwrap();
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

#[test]
fn a_batch_decodes_its_building_tiles_with_progress_and_burns_the_decoded_set() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let pbf = dir.join("pbf");
    let buildings = write_pbf_dir(&pbf);
    let terrain = dir.join("terrain.tif");
    write_terrain(&terrain);

    let (ok, stdout, stderr) = ingest(dir, "with.json", &jobs(dir, &terrain, Some(&pbf), "with"));
    assert!(ok, "the batch must succeed\nstdout:\n{stdout}\nstderr:\n{stderr}");
    let lines: Vec<&str> = stdout.lines().collect();
    let at = |pred: &dyn Fn(&str) -> bool| lines.iter().position(|l| pred(l));

    // The decode's lines, in this order, before any tile is reported.
    let start = at(&|l| l.starts_with("[Rust] Decoding building tiles in ")).expect("decode start line");
    let progress: Vec<(usize, &str)> = lines
        .iter()
        .enumerate()
        .filter(|(_, l)| l.starts_with("[Rust] Decoding building tiles: "))
        .map(|(i, l)| (i, *l))
        .collect();
    assert_eq!(
        progress.iter().map(|(_, l)| *l).collect::<Vec<_>>(),
        [
            "[Rust] Decoding building tiles: 1000/2100",
            "[Rust] Decoding building tiles: 2000/2100",
            "[Rust] Decoding building tiles: 2100/2100",
        ]
    );
    let done = at(&|l| l.starts_with("[Rust] Building tiles decoded: ")).expect("decode summary line");
    let first_tile = at(&|l| {
        ["[Tile] ", "[Rust] Progress: ", "[Info] buildings_pbf_dir:"].iter().any(|p| l.starts_with(p))
    })
    .expect("tile lines");
    assert!(start < progress[0].0 && progress[2].0 < done && done < first_tile, "{stdout}");

    // `<M> tile(s), <B> building(s) in <S.S> s`: every tile file, the
    // undecodable one included; every building decoded, before the
    // duplicate filter.
    let summary = lines[done];
    let head = format!("[Rust] Building tiles decoded: {TILES} tile(s), {buildings} building(s) in ");
    let secs = summary.strip_prefix(&head).and_then(|r| r.strip_suffix(" s"));
    let secs = secs.unwrap_or_else(|| panic!("{summary:?} is not {head:?}<S.S> s"));
    assert!(secs.parse::<f64>().is_ok() && secs.split('.').nth(1).map(str::len) == Some(1), "{summary:?}");

    // The undecodable tile: skipped with its warning, once, on stderr.
    let warns: Vec<&str> = stderr.lines().filter(|l| l.starts_with("[Warn] buildings ")).collect();
    assert_eq!(warns.len(), 1, "{stderr}");
    assert!(warns[0].starts_with("[Warn] buildings 14_8545_5760.pbf: PBF decode error: "), "{}", warns[0]);
    let info: Vec<&str> = lines.iter().copied().filter(|l| l.starts_with("[Info] buildings_pbf_dir:")).collect();
    assert_eq!(info.len(), 2, "{stdout}");
    let info_head = format!("[Info] buildings_pbf_dir: {} tile(s), {buildings} building(s) (", TILES - 1);
    for l in &info {
        assert!(l.starts_with(&info_head), "{l}");
    }

    // Nothing new looks like output-tile progress to the plugin, whose
    // regex is `Progress:\s*(\d+)/(\d+)` searched anywhere in a line.
    for l in stdout.lines().chain(stderr.lines()) {
        if l.contains("Progress:") {
            assert!(l.starts_with("[Rust] Progress: "), "{l:?}");
        }
    }

    // The tiles are the decoded set burnt onto the batch's own terrain.
    let (ok, out, err) = ingest(dir, "without.json", &jobs(dir, &terrain, None, "without"));
    assert!(ok, "{out}\n{err}");
    let set = load_pbf_building_dir(&pbf).unwrap();
    assert_eq!((set.tiles_found, set.tiles_read, set.decoded), (TILES, TILES - 1, buildings));
    assert!(set.duplicates_dropped() > 1000, "the buffer-zone copies must be exercised");
    let pixel_deg = RES_M * (1.0 / 111111.0);
    let json = jobs(dir, &terrain, None, "without");
    let mut raised = 0usize;
    for (i, j) in json.as_array().unwrap().iter().enumerate() {
        let got = std::fs::read(dir.join(format!("with_{i}.abt"))).unwrap();
        let mut want = std::fs::read(dir.join(format!("without_{i}.abt"))).unwrap();
        let bare = want.clone();
        let tile = TileRef {
            ul_lat: j["ul_lat"].as_f64().unwrap(),
            ul_lon: j["ul_lon"].as_f64().unwrap(),
            scale_x: pixel_deg,
            scale_y: pixel_deg,
            size_px: SIZE,
        };
        let stride = (SIZE as usize * 2 + 255) & !255;
        let mut grid = AbtGrid { buf: &mut want, size: SIZE, stride };
        let stats = rasterize_buildings(&mut grid, &tile, &set.buildings, &RasterOpts::default());
        assert!(got == want, "tile {i}: not the decoded set burnt onto the terrain");
        assert!(stats.buildings_hit > 20, "tile {i} must draw buildings: {stats:?}");
        raised += got.iter().zip(&bare).filter(|(a, b)| a != b).count();
    }
    assert!(raised > 0);
}
