//! Header-level ingest fixes (CONTRACT changelog 23): GDAL scale/offset,
//! the rotated-geotransform refusal, named codec refusals, ZSTD decode, and
//! the full cause chain on the batch `[Error]` line. Only the long-standing
//! API (`process_tile_with_cache`, `load_source_to_ram`, the CLI) is used, so
//! these compile — and fail — against the pre-change converter too.

mod common;

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use aether_converter::ingest::{load_source_to_ram, process_tile_with_cache, IngestJob, VOID_ELEV};
use common::*;

/// Run a job with a fresh cache; return the `.abt` bytes.
fn run(json: &serde_json::Value) -> anyhow::Result<Vec<u8>> {
    let job: IngestJob = serde_json::from_value(json.clone()).unwrap();
    let out = job.output_path.clone();
    process_tile_with_cache(job, Arc::new(Mutex::new(HashMap::new())))?;
    Ok(std::fs::read(out).unwrap())
}

// ── GDAL scale / offset ───────────────────────────────────────────────────

fn small_geo_tif(path: &Path, px: Px, md: Option<&str>, nodata: Option<&str>, geo: Geo, compression: u16) {
    write_tif(
        path,
        &Tif {
            w: 8,
            h: 8,
            px,
            layout: Layout::Strips(3),
            compression,
            geo,
            geokeys: &[(2048, 4326)],
            nodata,
            gdal_metadata: md,
        },
    );
}

const NORTH_UP: Geo = Geo::TieScale(8.0, 47.0, 0.0005, 0.0005);

fn small_job(dir: &Path, src: &Path, out: &str) -> serde_json::Value {
    serde_json::json!({
        "output_path": dir.join(out),
        "ul_lat": 47.0, "ul_lon": 8.0,
        "resolution_m": 30.0, "size_px": 4,
        "sources": [{"path": src}],
    })
}

#[test]
fn gdal_scale_and_offset_are_applied_before_the_half_metre_conversion() {
    let dir = tempfile::tempdir().unwrap();
    // Int16 decimetres: 5000 * 0.1 = 500 m = 1000 half-metres (was 10000).
    let p = dir.path().join("dm.tif");
    let md = r#"<GDALMetadata>
  <Item name="SCALE" sample="0" role="scale">0.1</Item>
  <Item name="OFFSET" sample="0" role="offset">0</Item>
</GDALMetadata>"#;
    small_geo_tif(&p, Px::I16(vec![5000; 64]), Some(md), None, NORTH_UP, 1);
    let bytes = run(&small_job(dir.path(), &p, "dm.abt")).unwrap();
    assert!(payload(&bytes, 4).iter().all(|&v| v == 1000), "got {:?}", payload(&bytes, 4));

    // With an offset, and nodata matched on the RAW value (GDAL's rule):
    // 4000 * 0.1 + 100 = 500 m; raw -1 is void, though -1*0.1+100 is not.
    let p2 = dir.path().join("dm_off.tif");
    let md2 = r#"<GDALMetadata><Item name="OFFSET" sample="0" role="offset">100</Item><Item name="SCALE" sample="0" role="scale">0.1</Item><Item name="DESCRIPTION" sample="0">x</Item></GDALMetadata>"#;
    let mut v = vec![4000i16; 64];
    for x in v.iter_mut().take(16) {
        *x = -1;
    }
    small_geo_tif(&p2, Px::I16(v), Some(md2), Some("-1"), NORTH_UP, 1);
    let bytes = run(&small_job(dir.path(), &p2, "off.abt")).unwrap();
    let px = payload(&bytes, 4);
    assert!(px.iter().all(|&v| v == 1000 || v == VOID_ELEV), "got {px:?}");
    assert!(px.contains(&VOID_ELEV), "raw nodata must void the pixel before scaling");

    // The whole-file reader applies the same transform.
    let img = load_source_to_ram(&p, None).unwrap();
    assert!(img.data.iter().all(|&v| v == 1000));

    // An identity pair (and a dataset-level item without sample=) changes nothing.
    let p3 = dir.path().join("identity.tif");
    let md3 = r#"<GDALMetadata><Item name="SCALE">0.5</Item><Item name="SCALE" sample="0" role="scale">1</Item></GDALMetadata>"#;
    small_geo_tif(&p3, Px::I16(vec![123; 64]), Some(md3), None, NORTH_UP, 1);
    let bytes = run(&small_job(dir.path(), &p3, "id.abt")).unwrap();
    assert!(payload(&bytes, 4).iter().all(|&v| v == 246));
}

#[test]
fn an_unparseable_gdal_scale_is_refused_naming_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("badscale.tif");
    let md = r#"<GDALMetadata><Item name="SCALE" sample="0" role="scale">zero point one</Item></GDALMetadata>"#;
    small_geo_tif(&p, Px::I16(vec![5000; 64]), Some(md), None, NORTH_UP, 1);
    let err = run(&small_job(dir.path(), &p, "x.abt")).unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("badscale.tif") && msg.contains("GDAL_METADATA SCALE"), "{msg}");
}

// ── Rotated / sheared geotransform ────────────────────────────────────────

#[test]
fn a_rotated_geotransform_is_refused_naming_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("rotated.tif");
    let mut m = [0.0; 16];
    m[0] = 0.0005;
    m[1] = 0.0001; // rotation / shear
    m[3] = 8.0;
    m[5] = -0.0005;
    m[7] = 47.0;
    m[15] = 1.0;
    small_geo_tif(&p, Px::I16(vec![100; 64]), None, None, Geo::Matrix(m), 1);
    let err = run(&small_job(dir.path(), &p, "rot.abt"))
        .expect_err("a rotated grid must not be sampled as north-up");
    let msg = format!("{err:#}");
    assert!(msg.contains("rotated.tif"), "must name the file: {msg}");
    assert!(
        msg.contains("rotated/sheared geotransform is not supported — warp it to a north-up grid first"),
        "{msg}"
    );
    // The whole-file reader refuses it too.
    assert!(load_source_to_ram(&p, None).is_err());

    // Shear in the other term alone is refused as well.
    m[1] = 0.0;
    m[4] = -0.00002;
    let p2 = dir.path().join("sheared.tif");
    small_geo_tif(&p2, Px::I16(vec![100; 64]), None, None, Geo::Matrix(m), 1);
    let msg = format!("{:#}", run(&small_job(dir.path(), &p2, "sh.abt")).unwrap_err());
    assert!(msg.contains("sheared.tif") && msg.contains("rotated/sheared"), "{msg}");

    // A north-up ModelTransformation still converts.
    m[4] = 0.0;
    let p3 = dir.path().join("northup_matrix.tif");
    small_geo_tif(&p3, Px::I16(vec![100; 64]), None, None, Geo::Matrix(m), 1);
    let bytes = run(&small_job(dir.path(), &p3, "nu.abt")).unwrap();
    assert!(payload(&bytes, 4).iter().all(|&v| v == 200));
}

// ── Compression ───────────────────────────────────────────────────────────

#[test]
fn an_undecodable_compression_is_refused_naming_file_and_codec() {
    let dir = tempfile::tempdir().unwrap();
    for (code, name) in [(34887u16, "LERC"), (50002, "JXL"), (34925, "LZMA")] {
        let p = dir.path().join(format!("codec_{code}.tif"));
        small_geo_tif(&p, Px::I16(vec![100; 64]), None, None, NORTH_UP, code);
        let err = run(&small_job(dir.path(), &p, "c.abt"))
            .expect_err("an undecodable codec must fail the tile");
        let msg = format!("{err:#}");
        assert!(msg.contains(&format!("codec_{code}.tif")), "must name the file: {msg}");
        assert!(msg.contains(&format!("unsupported compression {name} ({code})")), "{msg}");
        // Refused at header time, on the whole-file path too.
        let msg = format!("{:#}", load_source_to_ram(&p, None).err().expect("must be refused"));
        assert!(msg.contains(&format!("unsupported compression {name} ({code})")), "{msg}");
    }
}

#[test]
fn zstd_and_deflate_sources_decode_like_uncompressed_ones() {
    let dir = tempfile::tempdir().unwrap();
    let v: Vec<f32> = (0..64).map(|i| 400.0 + i as f32 * 1.25).collect();
    let mut outs = Vec::new();
    for code in [1u16, 8, 50000] {
        let p = dir.path().join(format!("z_{code}.tif"));
        small_geo_tif(&p, Px::F32(v.clone()), None, None, NORTH_UP, code);
        let bytes = run(&small_job(dir.path(), &p, &format!("z{code}.abt")))
            .unwrap_or_else(|e| panic!("compression {code} must decode: {e:#}"));
        outs.push(bytes);
    }
    assert!(outs[0] == outs[1], "Deflate differs from uncompressed");
    assert!(outs[0] == outs[2], "ZSTD differs from uncompressed");
}

// ── Batch error line carries the cause ────────────────────────────────────

#[test]
fn the_batch_error_line_carries_the_whole_cause_chain() {
    let dir = tempfile::tempdir().unwrap();
    let good = dir.path().join("good.tif");
    small_geo_tif(&good, Px::I16(vec![100; 64]), None, None, NORTH_UP, 1);
    let lerc = dir.path().join("lerc.tif");
    small_geo_tif(&lerc, Px::I16(vec![100; 64]), None, None, NORTH_UP, 34887);
    let jobs = serde_json::json!([
        small_job(dir.path(), &good, "ok.abt"),
        small_job(dir.path(), &dir.path().join("missing.tif"), "m.abt"),
        small_job(dir.path(), &lerc, "l.abt"),
    ]);
    let (ok, said) = run_cli(dir.path(), jobs);
    assert!(!ok, "a batch with failed tiles must exit non-zero:\n{said}");
    let errors: Vec<&str> = said.lines().filter(|l| l.starts_with("[Error] Failed to process tile:")).collect();
    assert_eq!(errors.len(), 2, "{said}");
    // The outer context names the file; the cause says what is wrong with it.
    let missing = errors.iter().find(|l| l.contains("missing.tif")).expect(&said);
    assert!(missing.contains("loading source") && missing.contains("Opening"), "{missing}");
    let lerc_line = errors.iter().find(|l| l.contains("lerc.tif")).expect(&said);
    assert!(lerc_line.contains("unsupported compression LERC (34887)"), "{lerc_line}");
}
