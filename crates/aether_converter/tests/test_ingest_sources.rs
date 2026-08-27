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
    U16(Vec<u16>),
    F64(Vec<f64>),
}

struct TiffSpec<'a> {
    w: u32,
    h: u32,
    /// (origin_e, origin_n, scale) — written as ModelTiepoint + ModelPixelScale.
    /// `None` writes a file with NO geotransform (the hard-error case).
    geotransform: Option<(f64, f64, f64)>,
    /// GeoKeyDirectory entries `(key_id, code)`, e.g. `(3072, 2056)` for a
    /// projected CRS or `(2048, 4326)` for a geographic one. Empty omits the
    /// tag entirely.
    geokeys: &'a [(u16, u16)],
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
        Samples::U16(v) => {
            assert_eq!(v.len(), (spec.w * spec.h) as usize);
            (16, 1, v.iter().flat_map(|x| x.to_le_bytes()).collect())
        }
        Samples::F64(v) => {
            assert_eq!(v.len(), (spec.w * spec.h) as usize);
            (64, 3, v.iter().flat_map(|x| x.to_le_bytes()).collect())
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
    // GeoKeyDirectory: header (version, revision, minor, key count) + one
    // 4-u16 entry per key (key_id, tag_location=0, count=1, value).
    let geokeys: Option<Vec<u16>> = (!spec.geokeys.is_empty()).then(|| {
        let mut dir = vec![1, 1, 0, spec.geokeys.len() as u16];
        for &(key, code) in spec.geokeys {
            dir.extend_from_slice(&[key, 0, 1, code]);
        }
        dir
    });

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
            geokeys: &[(3072, 2056)],
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
            geokeys: &[(2048, 4326)],
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
            geokeys: &[(3072, 2056)],
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
            geokeys: &[(2048, 4326)],
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
            geokeys: &[(2048, 4326)],
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
            geokeys: &[(2048, 4326)],
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
            geokeys: &[],
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
            geokeys: &[(3072, 32767)], // user-defined
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
fn a_pixel_is_point_tiepoint_is_shifted_to_the_pixel_corner() {
    // GTRasterType (1025) = 2, RasterPixelIsPoint: the tiepoint names the
    // CENTRE of pixel (0,0) — Copernicus and SRTM ship this way. The same
    // ground written both ways must produce byte-identical tiles; ignoring
    // the key sampled everything half a source pixel to the north-west.
    let dir = tempfile::tempdir().unwrap();
    let (w, h, s) = (40u32, 40u32, 0.001f64);
    let data: Vec<i16> = (0..w * h).map(|i| (i * 7 % 500) as i16).collect();
    let area = dir.path().join("area.tif");
    write_geotiff(
        &area,
        &TiffSpec {
            w,
            h,
            geotransform: Some((7.40, 47.00, s)), // NW corner of pixel (0,0)
            geokeys: &[(2048, 4326)],
            nodata: None,
            samples: Samples::I16(data.clone()),
        },
    );
    let point = dir.path().join("point.tif");
    write_geotiff(
        &point,
        &TiffSpec {
            w,
            h,
            // Centre of pixel (0,0): half a pixel in from the corner.
            geotransform: Some((7.40 + 0.5 * s, 47.00 - 0.5 * s, s)),
            geokeys: &[(2048, 4326), (1025, 2)],
            nodata: None,
            samples: Samples::I16(data),
        },
    );
    let mut job_a = tile_json(dir.path(), "area.abt", 16);
    job_a["sources"] = serde_json::json!([{"path": area}]);
    let mut job_p = tile_json(dir.path(), "point.abt", 16);
    job_p["sources"] = serde_json::json!([{"path": point}]);
    let bytes_a = run_job(job_a).unwrap();
    let bytes_p = run_job(job_p).unwrap();
    assert!(payload(&bytes_a, 16).iter().flatten().any(|&v| v != VOID_ELEV));
    assert_eq!(payload(&bytes_a, 16), payload(&bytes_p, 16));
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
            geokeys: &[(3072, 2056)],
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

// ── .abt files as ingest sources (Addendum A1) ────────────────────────────

/// Hand-crafted `.abt` writer: 44-byte header + 256-aligned stride-padded
/// rows of raw i16 half-metres (contract §6). `legacy_span_scale` writes the
/// tile's WHOLE SPAN into scale_x — the old-build quirk the reader must
/// divide back out (mirroring the plugin's waveshed/core/abt.py).
fn write_abt(
    path: &Path,
    size: u16,
    ul_lat: f64,
    ul_lon: f64,
    pixel_deg: f64,
    data: &[i16],
    legacy_span_scale: bool,
    version: u16,
) {
    assert_eq!(data.len(), size as usize * size as usize);
    let row_bytes = size as usize * 2;
    let stride = (row_bytes + 255) & !255;
    let scale_x = if legacy_span_scale { pixel_deg * size as f64 } else { pixel_deg };

    let mut buf: Vec<u8> = Vec::with_capacity(44 + stride * size as usize);
    buf.extend_from_slice(b"AETH");
    buf.extend_from_slice(&version.to_le_bytes());
    buf.extend_from_slice(&size.to_le_bytes());
    buf.extend_from_slice(&ul_lat.to_le_bytes());
    buf.extend_from_slice(&ul_lon.to_le_bytes());
    buf.extend_from_slice(&pixel_deg.to_le_bytes()); // scale_y (ignored by readers)
    buf.extend_from_slice(&scale_x.to_le_bytes());
    buf.extend_from_slice(&0i16.to_le_bytes()); // base_elev
    buf.extend_from_slice(&(stride as u16).to_le_bytes());
    for row in data.chunks(size as usize) {
        for v in row {
            buf.extend_from_slice(&v.to_le_bytes());
        }
        buf.resize(buf.len() + (stride - row_bytes), 0);
    }
    std::fs::write(path, buf).unwrap();
}

#[test]
fn an_abt_source_is_loaded_raw_and_its_voids_fall_through() {
    // .abt values are ALREADY half-metres: 500 must come out as 500 (250 m),
    // never doubled. Void pixels fall through to the next source exactly like
    // a GeoTIFF's no-data. Mixed .abt + GeoTIFF priority in one job.
    let dir = tempfile::tempdir().unwrap();
    let abt = dir.path().join("pool_tile.bin"); // deliberately NOT .abt — magic, not extension
    let w = 40u16;
    // West half 500 half-metres, east half void.
    let data: Vec<i16> = (0..w as u32 * w as u32)
        .map(|i| if i % (w as u32) < 20 { 500 } else { VOID_ELEV })
        .collect();
    write_abt(&abt, w, 47.00, 7.40, 0.001, &data, false, 1);
    // The stacked pair's B (a GeoTIFF, 222 m everywhere) backs the .abt.
    let (_a, tif) = stacked_pair(dir.path());

    let mut job = straddle_json(dir.path(), "mixed.abt.out");
    job["sources"] = serde_json::json!([{"path": abt}, {"path": tif}]);
    let px = payload(&run_job(job).unwrap(), 16);
    let flat: Vec<i16> = px.iter().flatten().copied().collect();
    assert!(flat.contains(&500), "raw half-metre values must survive undoubled");
    assert!(flat.contains(&444), "the GeoTIFF must fill the .abt's void half");
    assert!(flat.iter().all(|&v| v == 500 || v == 444));
}

#[test]
fn an_abt_source_area_averages_like_a_geographic_geotiff() {
    // Ratio > 1: source three times finer than the target; an interior output
    // cell is the mean of its 3x3 block — the same rule the lib tests pin for
    // geographic GeoTIFFs, with NO unit conversion on the way in.
    let dir = tempfile::tempdir().unwrap();
    let abt = dir.path().join("fine.abt");
    let out_pixel_deg = 30.0 / 111111.0;
    let src_deg = out_pixel_deg / 3.0;
    let (w, h) = (12u16, 12u16);
    let ramp: Vec<i16> = (0..w as u32 * h as u32)
        .map(|i| (i % w as u32) as i16 * 10 + (i / w as u32) as i16)
        .collect();
    write_abt(&abt, w, 47.5, 8.25, src_deg, &ramp, false, 1);

    let job = serde_json::json!({
        "output_path": dir.path().join("t.abt.out"),
        "ul_lat": 47.5, "ul_lon": 8.25,
        "resolution_m": 30.0, "size_px": 4,
        "sources": [{"path": abt}],
    });
    let px = payload(&run_job(job).unwrap(), 4);

    let block_mean = |cx: usize, cy: usize| -> i16 {
        let mut s = 0i32;
        for y in cy - 1..=cy + 1 {
            for x in cx - 1..=cx + 1 {
                s += ramp[y * 12 + x] as i32;
            }
        }
        ((s * 2 + 9) / 18) as i16 // round half away from zero, 9 samples
    };
    for y in 1..4 {
        for x in 1..4 {
            assert_eq!(px[y][x], block_mean(3 * x + 1, 3 * y + 1), "pixel ({x},{y})");
        }
    }
}

#[test]
fn a_legacy_span_scale_abt_header_reads_identically() {
    // Old builds wrote the tile's whole span in scale_x; the reader divides it
    // back out (same rule as the plugin's abt.py), so the two header variants
    // must produce byte-identical output.
    let dir = tempfile::tempdir().unwrap();
    let modern = dir.path().join("modern.abt");
    let legacy = dir.path().join("legacy.abt");
    let (w, _) = (40u16, ());
    let data: Vec<i16> = (0..w as u32 * w as u32).map(|i| (i % 97) as i16 * 3).collect();
    write_abt(&modern, w, 47.00, 7.40, 0.001, &data, false, 1);
    write_abt(&legacy, w, 47.00, 7.40, 0.001, &data, true, 1);

    let mut a = straddle_json(dir.path(), "modern.out");
    a["sources"] = serde_json::json!([{"path": modern}]);
    let mut b = straddle_json(dir.path(), "legacy.out");
    b["sources"] = serde_json::json!([{"path": legacy}]);
    assert_eq!(run_job(a).unwrap(), run_job(b).unwrap());
}

#[test]
fn crs_on_an_abt_source_is_refused_as_self_describing() {
    let dir = tempfile::tempdir().unwrap();
    let abt = dir.path().join("tile.abt");
    write_abt(&abt, 8, 47.00, 7.40, 0.001, &vec![100; 64], false, 1);
    let mut job = straddle_json(dir.path(), "x.out");
    job["sources"] = serde_json::json!([{"path": abt, "crs": "EPSG:4326"}]);
    let err = run_job(job).unwrap_err().to_string();
    assert!(err.contains("tile.abt"), "must name the file: {err:?}");
    assert!(err.contains("self-describing") && err.contains("crs"), "got {err:?}");
}

#[test]
fn nodata_on_an_abt_source_is_refused_as_self_describing() {
    let dir = tempfile::tempdir().unwrap();
    let abt = dir.path().join("tile.abt");
    write_abt(&abt, 8, 47.00, 7.40, 0.001, &vec![100; 64], false, 1);
    let mut job = straddle_json(dir.path(), "x.out");
    job["sources"] = serde_json::json!([{"path": abt, "nodata": -9999.0}]);
    let err = format!("{:?}", run_job(job).unwrap_err());
    assert!(err.contains("tile.abt"), "must name the file: {err:?}");
    assert!(err.contains("self-describing") && err.contains("nodata"), "got {err:?}");
}

#[test]
fn a_truncated_abt_header_is_refused_naming_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let abt = dir.path().join("stub.abt");
    std::fs::write(&abt, b"AETH\x01\x00\x10\x00short").unwrap();
    let mut job = straddle_json(dir.path(), "x.out");
    job["sources"] = serde_json::json!([{"path": abt}]);
    let err = format!("{:?}", run_job(job).unwrap_err());
    assert!(err.contains("stub.abt"), "must name the file: {err:?}");
    assert!(err.contains("truncated or corrupt .abt header"), "got {err:?}");
}

#[test]
fn a_bc6h_abt_tile_is_refused_as_a_source() {
    // Version 2 payloads are BC6H blocks, not i16 rows — decoding them as
    // rows would be silent garbage, so the version is a hard error.
    let dir = tempfile::tempdir().unwrap();
    let abt = dir.path().join("bc6h.abt");
    write_abt(&abt, 8, 47.00, 7.40, 0.001, &vec![100; 64], false, 2);
    let mut job = straddle_json(dir.path(), "x.out");
    job["sources"] = serde_json::json!([{"path": abt}]);
    let err = format!("{:?}", run_job(job).unwrap_err());
    assert!(err.contains("bc6h.abt") && err.contains("version 2"), "got {err:?}");
}

#[test]
fn a_missing_source_file_is_fatal_not_a_warning() {
    let dir = tempfile::tempdir().unwrap();
    let mut job = straddle_json(dir.path(), "x.abt");
    job["sources"] = serde_json::json!([{"path": dir.path().join("nope.tif")}]);
    let err = format!("{:?}", run_job(job).unwrap_err());
    assert!(err.contains("nope.tif"), "must name the file: {err:?}");
}

// ── Coverage: a run that produced no terrain must not exit 0 ──────────────
//
// The library call keeps writing the tile — a partially covered tile is
// normal, and `void_fill_m` is a legitimate way to ask for 0 m outside the
// data. The refusal is the CLI's, over the WHOLE run, because that is the only
// level at which "no source reached anything" is unambiguous. These tests
// therefore drive the binary, not `process_tile_with_cache`.

/// A 4x4 source of flat 55 m ground at *(lon, lat)*, one arc-second per pixel.
fn ground_at(dir: &Path, name: &str, lon: f64, lat: f64) -> PathBuf {
    let p = dir.join(name);
    write_geotiff(
        &p,
        &TiffSpec {
            w: 4,
            h: 4,
            geotransform: Some((lon, lat, 0.001)),
            geokeys: &[(2048, 4326)],
            nodata: None,
            samples: Samples::I16(vec![55; 16]),
        },
    );
    p
}

/// Run the real CLI over *json*; `(exit ok, stdout + stderr)`.
fn run_cli(dir: &Path, json: serde_json::Value) -> (bool, String) {
    let job_file = dir.join("job.json");
    std::fs::write(&job_file, serde_json::to_vec(&json).unwrap()).unwrap();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_aether_converter"))
        .args(["ingest", "--job-file"])
        .arg(&job_file)
        .output()
        .unwrap();
    let said = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.success(), said)
}

#[test]
fn a_tile_no_source_covered_is_refused_instead_of_exiting_0() {
    let dir = tempfile::tempdir().unwrap();
    // Ground in the Pacific; the tile is over Bern.
    let far = ground_at(dir.path(), "far.tif", -150.0, 20.0);
    let mut job = straddle_json(dir.path(), "void.abt");
    job["sources"] = serde_json::json!([{"path": far}]);
    let (ok, said) = run_cli(dir.path(), job);
    assert!(!ok, "an all-void tile must not exit 0: {said}");
    assert!(said.contains("void.abt"), "must name the tile: {said}");
    assert!(said.to_lowercase().contains("void"), "must say why: {said}");
}

#[test]
fn void_fill_does_not_disguise_a_tile_no_source_covered() {
    // The evidence is destroyed on the way to disk — with void_fill_m every
    // pixel is a plausible 0 m — so coverage is counted before the fill.
    let dir = tempfile::tempdir().unwrap();
    let far = ground_at(dir.path(), "far.tif", -150.0, 20.0);
    let mut job = straddle_json(dir.path(), "filled.abt");
    job["sources"] = serde_json::json!([{"path": far}]);
    job["void_fill_m"] = serde_json::json!(0.0);
    let (ok, said) = run_cli(dir.path(), job);
    assert!(!ok, "a filled all-void tile must not exit 0 either: {said}");
}

#[test]
fn a_tile_the_source_does_cover_still_exits_0() {
    let dir = tempfile::tempdir().unwrap();
    let near = ground_at(dir.path(), "near.tif", 7.4177, 46.9995);
    let mut job = straddle_json(dir.path(), "real.abt");
    job["sources"] = serde_json::json!([{"path": near}]);
    let (ok, said) = run_cli(dir.path(), job);
    assert!(ok, "a covered tile must convert: {said}");
}

#[test]
fn a_batch_keeps_its_uncovered_edge_tiles() {
    // The refusal is run-wide on purpose: the edge tiles of any area fall
    // outside the sources, and failing per tile would refuse ordinary runs.
    let dir = tempfile::tempdir().unwrap();
    let near = ground_at(dir.path(), "near.tif", 7.4177, 46.9995);
    let mut covered = straddle_json(dir.path(), "covered.abt");
    covered["sources"] = serde_json::json!([{"path": near}]);
    let mut edge = straddle_json(dir.path(), "edge.abt");
    edge["ul_lon"] = serde_json::json!(20.0);
    edge["sources"] = serde_json::json!([{"path": near}]);
    let (ok, said) = run_cli(dir.path(), serde_json::json!([covered, edge]));
    assert!(ok, "one covered tile is enough for the batch: {said}");
    assert!(dir.path().join("edge.abt").exists(), "the edge tile is still written");
}

// ── Sample formats ────────────────────────────────────────────────────────

/// A 4x4 source of flat 100 m ground over the straddle tile, in *samples*.
fn ground_typed(dir: &Path, name: &str, samples: Samples) -> PathBuf {
    let p = dir.join(name);
    write_geotiff(
        &p,
        &TiffSpec {
            w: 4,
            h: 4,
            geotransform: Some((7.4177, 46.9995, 0.001)),
            geokeys: &[(2048, 4326)],
            nodata: None,
            samples,
        },
    );
    p
}

#[test]
fn every_sample_format_the_decoder_returns_is_terrain() {
    // The reader used to handle I16/I32/F32 and answer "Unsupported TIF
    // format" to everything else — including UInt16, the commonest national
    // DEM type, and Float64, which is what QGIS's own raster writer hands the
    // converter for an ASCII grid. All four must decode to the same ground.
    let dir = tempfile::tempdir().unwrap();
    let cases = [
        ("i16.tif", Samples::I16(vec![100; 16])),
        ("f32.tif", Samples::F32(vec![100.0; 16])),
        ("u16.tif", Samples::U16(vec![100; 16])),
        ("f64.tif", Samples::F64(vec![100.0; 16])),
    ];
    for (name, samples) in cases {
        let src = ground_typed(dir.path(), name, samples);
        let mut job = straddle_json(dir.path(), &format!("{name}.abt"));
        job["sources"] = serde_json::json!([{"path": src}]);
        let px = payload(&run_job(job).unwrap(), 16);
        let flat: Vec<i16> = px.iter().flatten().copied().collect();
        assert!(flat.contains(&200), "{name}: 100 m must read as 200 half-metres");
    }
}

#[test]
fn an_rgba_picture_is_refused_as_imagery() {
    // A rendered basemap/hillshade export is a picture, not terrain. Reading
    // its colour bytes as metres used to "succeed" (and the 4x sample count
    // then sheared the grid); it must be a hard error naming the file,
    // before a single sample is decoded.
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("basemap.tif");
    let file = std::fs::File::create(&p).unwrap();
    let mut enc = tiff::encoder::TiffEncoder::new(file).unwrap();
    enc.write_image::<tiff::encoder::colortype::RGBA8>(4, 4, &[255u8; 64]).unwrap();
    let mut job = straddle_json(dir.path(), "x.abt");
    job["sources"] = serde_json::json!([{"path": p, "crs": "EPSG:4326"}]);
    let err = format!("{:#}", run_job(job).unwrap_err());
    assert!(err.contains("basemap.tif"), "must name the file: {err:?}");
    assert!(err.contains("not an elevation raster"), "got {err:?}");
}
