//! The pure-Rust CRS stack (proj4rs + crs-definitions) that sources[] ingest
//! stands on. Pins the properties the converter relies on: EPSG:2056 resolves
//! and parses, its somerc forward transform agrees with the approximate
//! CH1903+ polynomial the converter used before sources[] landed (retired in
//! contract v2.0), and a plain UTM zone behaves.

use proj4rs::Proj;
use proj4rs::transform::transform;

/// The retired approximate polynomial (federal mapping agency formulas),
/// kept here only as the historical reference the parity bound is stated
/// against. It no longer exists in the converter.
fn wgs84_to_epsg2056_polynomial(lat: f64, lon: f64) -> (f64, f64) {
    let phi = (lat * 3600.0 - 169028.66) / 10000.0;
    let lam = (lon * 3600.0 - 26782.5) / 10000.0;
    let e = 2600072.37 + 211455.93 * lam - 10938.51 * lam * phi - 0.36 * lam * phi * phi
        - 44.54 * lam * lam * lam;
    let n = 1200147.07 + 308807.95 * phi + 3745.25 * lam * lam + 76.63 * phi * phi
        - 194.56 * lam * lam * phi + 119.79 * phi * phi * phi;
    (e, n)
}

fn wgs84() -> Proj {
    Proj::from_proj_string("+proj=longlat +datum=WGS84 +no_defs").unwrap()
}

#[test]
fn epsg_2056_definition_exists_and_parses() {
    let def = crs_definitions::from_code(2056).expect("EPSG:2056 in crs-definitions");
    Proj::from_proj_string(def.proj4).expect("proj4rs parses EPSG:2056 (somerc)");
}

#[test]
fn epsg_2056_matches_the_retired_polynomial_within_2m() {
    // Contract v2.0 records the ingest switch from the polynomial to the real
    // projection as a <= 1 m shift; the worst deviation measured on this grid
    // is ~0.80 m. 2 m is the alarm threshold, not the expectation.
    let def = crs_definitions::from_code(2056).unwrap();
    let dst = Proj::from_proj_string(def.proj4).unwrap();
    let src = wgs84();

    let mut worst = 0.0f64;
    let mut worst_at = (0.0, 0.0);
    // Grid over the projection's home range: lat 45.8..47.8, lon 6.0..10.5.
    for i in 0..=20 {
        for j in 0..=20 {
            let lat = 45.8 + (47.8 - 45.8) * (i as f64) / 20.0;
            let lon = 6.0 + (10.5 - 6.0) * (j as f64) / 20.0;
            let (pe, pn) = wgs84_to_epsg2056_polynomial(lat, lon);

            let mut pt = (lon.to_radians(), lat.to_radians(), 0.0);
            transform(&src, &dst, &mut pt).expect("transform ok");

            let d = ((pt.0 - pe).powi(2) + (pt.1 - pn).powi(2)).sqrt();
            if d > worst {
                worst = d;
                worst_at = (lat, lon);
            }
        }
    }
    assert!(worst < 2.0, "deviation {worst} m at {worst_at:?} exceeds 2 m");
}

#[test]
fn epsg_2056_projection_center_maps_to_false_origin() {
    // Definitional for somerc: (lat_0, lon_0) -> (x_0, y_0) = (2600000, 1200000)
    // exactly (the Bern reference point, federal mapping agency values).
    let def = crs_definitions::from_code(2056).unwrap();
    let dst = Proj::from_proj_string(def.proj4).unwrap();
    let (lat, lon): (f64, f64) = (46.9510827861504654, 7.4386324175346402);
    let mut pt = (lon.to_radians(), lat.to_radians(), 0.0);
    transform(&wgs84(), &dst, &mut pt).unwrap();
    let de = (pt.0 - 2_600_000.0).abs();
    let dn = (pt.1 - 1_200_000.0).abs();
    assert!(de < 2.0 && dn < 2.0, "reference point off by ({de}, {dn}) m");
}

#[test]
fn a_utm_zone_works_too() {
    // Generic check beyond EPSG:2056: EPSG:32632 (UTM 32N, WGS84).
    // lon 9 is the central meridian -> E 500000 exactly.
    let def = crs_definitions::from_code(32632).expect("EPSG:32632 known");
    let utm = Proj::from_proj_string(def.proj4).unwrap();
    let mut pt = (9.0f64.to_radians(), 47.0f64.to_radians(), 0.0);
    transform(&wgs84(), &utm, &mut pt).unwrap();
    assert!((pt.0 - 500_000.0).abs() < 0.5, "central meridian easting");
    assert!(pt.1 > 5_190_000.0 && pt.1 < 5_220_000.0, "northing plausible");
}
