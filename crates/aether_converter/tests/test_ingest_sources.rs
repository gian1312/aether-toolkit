//! sources[] ingest: CRS resolution (explicit + GeoKeys), the legacy
//! `base_tif`/`swiss_tifs` shim, per-source nodata, `void_fill_m`, priority
//! order, and the hard-error surface — all against real GeoTIFF files.
//!
//! The files are written by a minimal hand-crafted uncompressed-GeoTIFF
//! writer, because the `tiff` crate's encoder cannot write the Geo tags.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use aether_converter::ingest::{process_tile_with_cache, IngestJob, VOID_ELEV};

// ── Minimal uncompressed GeoTIFF writer ───────────────────────────────────

enum Samples {
    I16(Vec<i16>),
    F32(Vec<f32>),
}

struct TiffSpec<'a> {
    w: u32,
    h: u32,
    /// (origin_e, origin_n, scale) — written as ModelTiepoint + ModelPixelScale.
    /// `None` writes a file with NO geotransform (the hard-error case).
    geotransform: Option<(f64, f64, f64)>,
    /// GeoKeyDirectory entry `(key_id, code)`, e.g. `(3072, 2056)` for a
    /// projected CRS or `(2048, 4326)` for a geographic one. `None` omits the
    /// tag entirely.
    geokey: Option<(u16, u16)>,
    /// GDAL_NODATA ascii tag content.
    nodata: Option<&'a str>,
    samples: Samples,
}

fn write_geotiff(path: &Path, spec: &TiffSpec) {
    const T_SHORT: u16 = 3;
    const T_LONG: u16 = 4;
    const T_ASCII: u16 = 2;
    const T_DOUBLE: u16 = 12;

    let (bps, sample_format, data): (u32, u32, Vec<u8>) = match &spec.samples {
        Samples::I16(v) => {
            assert_eq!(v.len(), (spec.w * spec.h) as usize);
            (16, 2, v.iter().flat_map(|x| x.to_le_bytes()).collect())
        }
        Samples::F32(v) => {
            assert_eq!(v.len(), (spec.w * spec.h) as usize);
            (32, 3, v.iter().flat_map(|x| x.to_le_bytes()).collect())
        }
    };

    // GDAL_NODATA: count must exceed 4 bytes so the value is stored via
    // offset (the reader takes <=4-byte values inline from the offset field).
    let nodata_bytes: Option<Vec<u8>> = spec.nodata.map(|s| {
        let mut b = s.as_bytes().to_vec();
        b.push(0);
        while b.len() < 5 {
            b.push(0);
        }
        b
    });
    // GeoKeyDirectory: header (version, revision, minor, key count) + 1 key
    // entry (key_id, tag_location=0, count=1, value).
    let geokeys: Option<Vec<u16>> =
        spec.geokey.map(|(key, code)| vec![1, 1, 0, 1, key, 0, 1, code]);

    let n_entries = 10
        + if spec.geotransform.is_some() { 2 } else { 0 }
        + if geokeys.is_some() { 1 } else { 0 }
        + if nodata_bytes.is_some() { 1 } else { 0 };

    let ifd_off = 8 + data.len();
    let mut ext_off = ifd_off + 2 + n_entries * 12 + 4;
    let mut take = |len: usize| {
        let o = ext_off;
        ext_off += len;
        o as u32
    };
    let (scale_off, tie_off) = if spec.geotransform.is_some() {
        (take(24), take(48))
    } else {
        (0, 0)
    };
    let geokey_off = geokeys.as_ref().map(|g| take(g.len() * 2));
    let nodata_off = nodata_bytes.as_ref().map(|b| take(b.len()));

    let mut buf: Vec<u8> = Vec::new();
    buf.extend_from_slice(b"II");
    buf.extend_from_slice(&42u16.to_le_bytes());
    buf.extend_from_slice(&(ifd_off as u32).to_le_bytes());
    buf.extend_from_slice(&data);

    buf.extend_from_slice(&(n_entries as u16).to_le_bytes());
    let mut entry = |tag: u16, typ: u16, count: u32, value: u32| {
        buf.extend_from_slice(&tag.to_le_bytes());
        buf.extend_from_slice(&typ.to_le_bytes());
        buf.extend_from_slice(&count.to_le_bytes());
        buf.extend_from_slice(&value.to_le_bytes());
    };
    entry(256, T_LONG, 1, spec.w); // ImageWidth
    entry(257, T_LONG, 1, spec.h); // ImageLength
    entry(258, T_SHORT, 1, bps); // BitsPerSample
    entry(259, T_SHORT, 1, 1); // Compression: none
    entry(262, T_SHORT, 1, 1); // Photometric: BlackIsZero
    entry(273, T_LONG, 1, 8); // StripOffsets: data starts right after header
    entry(277, T_SHORT, 1, 1); // SamplesPerPixel
    entry(278, T_LONG, 1, spec.h); // RowsPerStrip: single strip
    entry(279, T_LONG, 1, data.len() as u32); // StripByteCounts
    entry(339, T_SHORT, 1, sample_format); // SampleFormat
    if spec.geotransform.is_some() {
        entry(33550, T_DOUBLE, 3, scale_off); // ModelPixelScale
        entry(33922, T_DOUBLE, 6, tie_off); // ModelTiepoint
    }
    if let Some(g) = &geokeys {
        entry(34735, T_SHORT, g.len() as u32, geokey_off.unwrap()); // GeoKeyDirectory
    }
    if let Some(b) = &nodata_bytes {
        entry(42113, T_ASCII, b.len() as u32, nodata_off.unwrap()); // GDAL_NODATA
    }
    buf.extend_from_slice(&0u32.to_le_bytes()); // next IFD: none

    if let Some((origin_e, origin_n, scale)) = spec.geotransform {
        for v in [scale, scale, 0.0] {
            buf.extend_from_slice(&v.to_le_bytes());
        }
        for v in [0.0, 0.0, 0.0, origin_e, origin_n, 0.0] {
            buf.extend_from_slice(&v.to_le_bytes());
        }
    }
    if let Some(g) = &geokeys {
        for v in g {
            buf.extend_from_slice(&v.to_le_bytes());
        }
    }
    if let Some(b) = &nodata_bytes {
        buf.extend_from_slice(b);
    }

    std::fs::write(path, buf).unwrap();
}

// ── Job runner helpers ────────────────────────────────────────────────────

/// Deserialize a job from JSON (the real ingest surface), run it with a fresh
/// cache, and return the produced `.abt` bytes.
fn run_job(json: serde_json::Value) -> anyhow::Result<Vec<u8>> {
    let job: IngestJob = serde_json::from_value(json).unwrap();
    let out = job.output_path.clone();
    process_tile_with_cache(job, Arc::new(Mutex::new(HashMap::new())))?;
    Ok(std::fs::read(out).unwrap())
}

/// The payload as rows of i16, past the 44-byte header and row padding.
fn payload(bytes: &[u8], size: usize) -> Vec<Vec<i16>> {
    let stride = (size * 2 + 255) & !255;
    (0..size)
        .map(|y| {
            (0..size)
                .map(|x| {
                    let o = 44 + y * stride + x * 2;
                    i16::from_le_bytes([bytes[o], bytes[o + 1]])
                })
                .collect()
        })
        .collect()
}

/// The retired CH1903+ polynomial, kept in the tests as the reference the
/// proj4rs parity bound is stated against (see tests/test_crs.rs).
fn wgs84_to_epsg2056_polynomial(lat: f64, lon: f64) -> (f64, f64) {
    let phi = (lat * 3600.0 - 169028.66) / 10000.0;
    let lam = (lon * 3600.0 - 26782.5) / 10000.0;
    let e = 2600072.37 + 211455.93 * lam - 10938.51 * lam * phi - 0.36 * lam * phi * phi
        - 44.54 * lam * lam * lam;
    let n = 1200147.07 + 308807.95 * phi + 3745.25 * lam * lam + 76.63 * phi * phi
        - 194.56 * lam * lam * phi + 119.79 * phi * phi * phi;
    (e, n)
}

/// A projected (EPSG:2056) source amply covering the test tile at 10 m.
fn projected_ramp(path: &Path) {
    let (w, h) = (300u32, 300u32);
    // Column-index ramp: the stored value tells which source column a cell
    // averaged, which is what the parity test measures.
    let data: Vec<i16> = (0..w * h).map(|i| (i % w) as i16).collect();
    write_geotiff(
        path,
        &TiffSpec {
            w,
            h,
            geotransform: Some((2_598_500.0, 1_206_000.0, 10.0)),
            geokey: Some((3072, 2056)),
            nodata: None,
            samples: Samples::I16(data),
        },
    );
}

/// A geographic (EPSG:4326) source covering the test tile: constant 300 m.
fn geographic_base(path: &Path) {
    let (w, h) = (60u32, 60u32);
    write_geotiff(
        path,
        &TiffSpec {
            w,
            h,
            geotransform: Some((7.40, 47.00, 0.001)),
            geokey: Some((2048, 4326)),
            nodata: None,
            samples: Samples::I16(vec![300; (w * h) as usize]),
        },
    );
}

fn tile_json(dir: &Path, name: &str, size_px: u32) -> serde_json::Value {
    serde_json::json!({
        "output_path": dir.join(name),
        "format": "r16sint",
        "ul_lat": 46.999,
        "ul_lon": 7.425,
        "resolution_m": 30.0,
        "size_px": size_px,
    })
}

// ── Shim equivalence & CRS resolution ─────────────────────────────────────

#[test]
fn legacy_and_sources_jobs_produce_identical_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let prj = dir.path().join("overlay_2599-1204.tif");
    let geo = dir.path().join("base.tif");
    // A PARTIAL projected overlay (covers only E >= 2599200, N >= 1205000 of
    // the tile's ~2598962..2599442 x ~1204847..1205327 footprint), so the
    // geographic base must fill the rest — the equivalence exercises both
    // sampling paths and the fall-through between them.
    let (w, h) = (100u32, 100u32);
    let ramp: Vec<i16> = (0..w * h).map(|i| (i % w) as i16).collect();
    write_geotiff(
        &prj,
        &TiffSpec {
            w,
            h,
            geotransform: Some((2_599_200.0, 1_206_000.0, 10.0)),
            geokey: Some((3072, 2056)),
            nodata: None,
            samples: Samples::I16(ramp),
        },
    );
    geographic_base(&geo);

    // The projected source only covers E >= 2598500 with data columns, and the
    // ramp values differ from the base's constant 600, so both sources must
    // contribute pixels for the equivalence to mean anything.
    let mut legacy = tile_json(dir.path(), "legacy.abt", 16);
    legacy["swiss_tifs"] = serde_json::json!([prj]);
    legacy["base_tif"] = serde_json::json!(geo);

    let mut new = tile_json(dir.path(), "new.abt", 16);
    new["sources"] = serde_json::json!([
        {"path": prj, "crs": "EPSG:2056"},
        {"path": geo, "crs": "EPSG:4326"},
    ]);

    // Third form: no explicit crs at all — resolved from each file's GeoKeys.
    let mut geokeys_only = tile_json(dir.path(), "geokeys.abt", 16);
    geokeys_only["sources"] = serde_json::json!([{"path": prj}, {"path": geo}]);

    let a = run_job(legacy).unwrap();
    let b = run_job(new).unwrap();
    let c = run_job(geokeys_only).unwrap();
    assert_eq!(a, b, "legacy-shim job must be byte-identical to the sources[] job");
    assert_eq!(b, c, "GeoKey-resolved CRS must be byte-identical to explicit crs");

    let px = payload(&a, 16);
    let flat: Vec<i16> = px.iter().flatten().copied().collect();
    assert!(flat.iter().any(|&v| v == 600), "base pixels missing (300 m = 600 half-m)");
    assert!(
        flat.iter().any(|&v| v != 600 && v != VOID_ELEV),
        "projected-source pixels missing"
    );
    assert!(flat.iter().all(|&v| v != VOID_ELEV), "both sources together cover the tile");
}

#[test]
fn a_geographic_source_reproduces_the_base_tif_path_bytes() {
    // The geographic sampling path must stay byte-identical to the historical
    // base_tif path: same file, legacy form vs sources form.
    let dir = tempfile::tempdir().unwrap();
    let geo = dir.path().join("base.tif");
    geographic_base(&geo);

    let mut legacy = tile_json(dir.path(), "legacy.abt", 16);
    legacy["base_tif"] = serde_json::json!(geo);
    legacy["swiss_tifs"] = serde_json::json!([]);

    let mut new = tile_json(dir.path(), "new.abt", 16);
    new["sources"] = serde_json::json!([{"path": geo, "crs": "EPSG:4326"}]);

    let a = run_job(legacy).unwrap();
    let b = run_job(new).unwrap();
    assert_eq!(a, b);
    assert!(payload(&a, 16).iter().flatten().all(|&v| v == 600));
}

#[test]
fn epsg_2056_sampling_lands_within_one_source_pixel_of_the_old_polynomial() {
    let dir = tempfile::tempdir().unwrap();
    let prj = dir.path().join("ramp.tif");
    projected_ramp(&prj);

    let mut job = tile_json(dir.path(), "tile.abt", 32);
    job["sources"] = serde_json::json!([{"path": prj}]);

    let px = payload(&run_job(job).unwrap(), 32);
    let pixel_deg = 30.0 / 111111.0;
    for (y, row) in px.iter().enumerate() {
        let lat = 46.999 - y as f64 * pixel_deg;
        for (x, &v) in row.iter().enumerate() {
            let lon = 7.425 + x as f64 * pixel_deg;
            let (e, _n) = wgs84_to_epsg2056_polynomial(lat, lon);
            let expected_col = (e - 2_598_500.0) / 10.0;
            let got_col = f64::from(v) / 2.0; // stored value = 2 * column index
            assert!(
                (got_col - expected_col).abs() <= 1.0,
                "pixel ({x},{y}): sampled column {got_col:.2}, polynomial column {expected_col:.2}"
            );
        }
    }
}

// ── Priority, nodata, void_fill ───────────────────────────────────────────

/// Two stacked geographic sources over lon 7.40..7.44: A is 111 m on its west
/// half and Float32-NaN on its east half; B is 222 m everywhere.
fn stacked_pair(dir: &Path) -> (PathBuf, PathBuf) {
    let a = dir.join("a.tif");
    let b = dir.join("b.tif");
    let (w, h) = (40u32, 40u32);
    let data_a: Vec<f32> = (0..w * h)
        .map(|i| if i % w < 20 { 111.0 } else { f32::NAN })
        .collect();
    write_geotiff(
        &a,
        &TiffSpec {
            w,
            h,
            geotransform: Some((7.40, 47.00, 0.001)),
            geokey: Some((2048, 4326)),
            nodata: None,
            samples: Samples::F32(data_a),
        },
    );
    write_geotiff(
        &b,
        &TiffSpec {
            w,
            h,
            geotransform: Some((7.40, 47.00, 0.001)),
            geokey: Some((2048, 4326)),
            nodata: None,
            samples: Samples::I16(vec![222; (w * h) as usize]),
        },
    );
    (a, b)
}

fn straddle_json(dir: &Path, name: &str) -> serde_json::Value {
    // 16 px at 30 m starting at lon 7.4177: source columns ~17.7..22, so the
    // tile straddles A's valid/NaN boundary at column 20 (lon 7.42).
    serde_json::json!({
        "output_path": dir.join(name),
        "ul_lat": 46.9995,
        "ul_lon": 7.4177,
        "resolution_m": 30.0,
        "size_px": 16,
    })
}

#[test]
fn priority_is_array_order_first_valid_sample_wins() {
    let dir = tempfile::tempdir().unwrap();
    let (a, b) = stacked_pair(dir.path());

    let mut ab = straddle_json(dir.path(), "ab.abt");
    ab["sources"] = serde_json::json!([{"path": a}, {"path": b}]);
    let px = payload(&run_job(ab).unwrap(), 16);
    let flat: Vec<i16> = px.iter().flatten().copied().collect();
    // West of the boundary A wins (111 m); east its NaN falls through to B.
    assert!(flat.contains(&222), "A's west half must win where it has data");
    assert!(flat.contains(&444), "B must fill A's NaN half");
    assert!(flat.iter().all(|&v| v == 222 || v == 444));

    let mut ba = straddle_json(dir.path(), "ba.abt");
    ba["sources"] = serde_json::json!([{"path": b}, {"path": a}]);
    let px = payload(&run_job(ba).unwrap(), 16);
    assert!(
        px.iter().flatten().all(|&v| v == 444),
        "with B first, A must never be consulted"
    );
}

/// A geographic source that is 55 m on its west half and 77 m on its east
/// half, with a GDAL_NODATA tag of "77".
fn nodata_tagged(dir: &Path) -> PathBuf {
    let c = dir.join("c.tif");
    let (w, h) = (40u32, 40u32);
    let data: Vec<i16> = (0..w * h).map(|i| if i % w < 20 { 55 } else { 77 }).collect();
    write_geotiff(
        &c,
        &TiffSpec {
            w,
            h,
            geotransform: Some((7.40, 47.00, 0.001)),
            geokey: Some((2048, 4326)),
            nodata: Some("77"),
            samples: Samples::I16(data),
        },
    );
    c
}

#[test]
fn the_gdal_nodata_tag_maps_matching_samples_to_void_at_load_time() {
    let dir = tempfile::tempdir().unwrap();
    let c = nodata_tagged(dir.path());

    let mut job = straddle_json(dir.path(), "tag.abt");
    job["sources"] = serde_json::json!([{"path": c}]);
    let px = payload(&run_job(job).unwrap(), 16);
    let flat: Vec<i16> = px.iter().flatten().copied().collect();
    assert!(flat.contains(&110), "valid 55 m samples survive");
    assert!(flat.contains(&VOID_ELEV), "tagged 77 samples become voids");
    assert!(flat.iter().all(|&v| v == 110 || v == VOID_ELEV));
}

#[test]
fn an_explicit_nodata_field_overrides_the_gdal_nodata_tag() {
    let dir = tempfile::tempdir().unwrap();
    let c = nodata_tagged(dir.path());

    let mut job = straddle_json(dir.path(), "explicit.abt");
    job["sources"] = serde_json::json!([{"path": c, "nodata": 55.0}]);
    let px = payload(&run_job(job).unwrap(), 16);
    let flat: Vec<i16> = px.iter().flatten().copied().collect();
    // With nodata=55 the tag's 77 is real terrain again and 55 is the void.
    assert!(flat.contains(&154), "77 m samples survive under the override");
    assert!(flat.contains(&VOID_ELEV), "55 m samples become voids");
    assert!(flat.iter().all(|&v| v == 154 || v == VOID_ELEV));
}

#[test]
fn void_fill_m_replaces_the_sentinel_for_uncovered_pixels_only() {
    let dir = tempfile::tempdir().unwrap();
    let c = nodata_tagged(dir.path());

    let mut job = straddle_json(dir.path(), "fill.abt");
    job["sources"] = serde_json::json!([{"path": c}]);
    job["void_fill_m"] = serde_json::json!(12.3); // round(24.6) = 25 half-metres
    let px = payload(&run_job(job).unwrap(), 16);
    let flat: Vec<i16> = px.iter().flatten().copied().collect();
    assert!(flat.contains(&110), "covered pixels keep their terrain");
    assert!(flat.contains(&25), "uncovered pixels take round(void_fill_m * 2)");
    assert!(!flat.contains(&VOID_ELEV), "no sentinel survives when void_fill_m is set");
}

#[test]
fn a_tile_with_no_sources_and_a_void_fill_is_flat_fill() {
    let dir = tempfile::tempdir().unwrap();
    let mut job = straddle_json(dir.path(), "flat.abt");
    job["sources"] = serde_json::json!([]);
    job["void_fill_m"] = serde_json::json!(0.0);
    let px = payload(&run_job(job).unwrap(), 16);
    assert!(px.iter().flatten().all(|&v| v == 0), "0 m ground everywhere");
}

// ── Hard errors ───────────────────────────────────────────────────────────

#[test]
fn a_file_without_geokeys_and_without_crs_is_refused_naming_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("no_geokeys.tif");
    write_geotiff(
        &p,
        &TiffSpec {
            w: 4,
            h: 4,
            geotransform: Some((7.40, 47.00, 0.001)),
            geokey: None,
            nodata: None,
            samples: Samples::I16(vec![1; 16]),
        },
    );
    let mut job = straddle_json(dir.path(), "x.abt");
    job["sources"] = serde_json::json!([{"path": p}]);
    let err = run_job(job).unwrap_err().to_string();
    assert!(err.contains("no_geokeys.tif"), "must name the file: {err:?}");
    assert!(err.contains("crs"), "must tell the user to add \"crs\": {err:?}");
}

#[test]
fn a_user_defined_projection_geokey_is_refused_like_an_absent_one() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("user_defined.tif");
    write_geotiff(
        &p,
        &TiffSpec {
            w: 4,
            h: 4,
            geotransform: Some((7.40, 47.00, 0.001)),
            geokey: Some((3072, 32767)), // user-defined
            nodata: None,
            samples: Samples::I16(vec![1; 16]),
        },
    );
    let mut job = straddle_json(dir.path(), "x.abt");
    job["sources"] = serde_json::json!([{"path": p}]);
    let err = run_job(job).unwrap_err().to_string();
    assert!(err.contains("user_defined.tif") && err.contains("crs"), "got {err:?}");
}

#[test]
fn an_unknown_epsg_code_is_refused_naming_the_code() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("fine.tif");
    geographic_base(&p);
    let mut job = straddle_json(dir.path(), "x.abt");
    job["sources"] = serde_json::json!([{"path": p, "crs": "EPSG:65432"}]);
    let err = run_job(job).unwrap_err().to_string();
    assert!(err.contains("65432"), "must name the unknown code: {err:?}");
    assert!(err.contains("proj string"), "must suggest a proj string: {err:?}");
}

#[test]
fn a_crs_that_is_neither_epsg_nor_proj_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("fine.tif");
    geographic_base(&p);
    let mut job = straddle_json(dir.path(), "x.abt");
    job["sources"] = serde_json::json!([{"path": p, "crs": "utm32"}]);
    let err = run_job(job).unwrap_err().to_string();
    assert!(err.contains("utm32") && err.contains("EPSG:nnnn"), "got {err:?}");
}

#[test]
fn mixing_sources_with_the_legacy_fields_is_refused_as_ambiguous() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("fine.tif");
    geographic_base(&p);
    let mut job = straddle_json(dir.path(), "x.abt");
    job["sources"] = serde_json::json!([{"path": p}]);
    job["base_tif"] = serde_json::json!(p);
    let err = run_job(job).unwrap_err().to_string();
    assert!(
        err.contains("sources") && err.contains("base_tif"),
        "must name both surfaces: {err:?}"
    );
}

#[test]
fn a_file_without_a_geotransform_is_refused_naming_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("bare_2599-1201.tif"); // a name the old parser "understood"
    write_geotiff(
        &p,
        &TiffSpec {
            w: 4,
            h: 4,
            geotransform: None,
            geokey: Some((3072, 2056)),
            nodata: None,
            samples: Samples::I16(vec![1; 16]),
        },
    );
    let mut job = straddle_json(dir.path(), "x.abt");
    job["sources"] = serde_json::json!([{"path": p}]);
    let err = format!("{:?}", run_job(job).unwrap_err());
    assert!(err.contains("bare_2599-1201.tif"), "must name the file: {err:?}");
    assert!(err.contains("no geotransform"), "got {err:?}");
    // Filename-derived georeferencing is gone: the coordinates in the file
    // name must NOT rescue the job.
    assert!(err.contains("never"), "must state filenames are never parsed: {err:?}");
}

#[test]
fn a_missing_source_file_is_fatal_not_a_warning() {
    let dir = tempfile::tempdir().unwrap();
    let mut job = straddle_json(dir.path(), "x.abt");
    job["sources"] = serde_json::json!([{"path": dir.path().join("nope.tif")}]);
    let err = format!("{:?}", run_job(job).unwrap_err());
    assert!(err.contains("nope.tif"), "must name the file: {err:?}");
}
