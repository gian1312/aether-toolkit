use aether_converter::geo::{wgs84_to_lv95, is_in_swiss_bounds, SwissCoord};

#[test]
fn test_bern_federal_building() {
    // Bern federal building: known LV95 ~ E=2600070, N=1199520
    // The swisstopo approximate formula has limited precision; use +-1000m tolerance.
    let c = wgs84_to_lv95(46.9481, 7.4474);
    assert!(
        (c.e - 2600070.0).abs() < 1000.0,
        "Bern easting: expected ~2600070, got {}",
        c.e
    );
    assert!(
        (c.n - 1199520.0).abs() < 1000.0,
        "Bern northing: expected ~1199520, got {}",
        c.n
    );
}

#[test]
fn test_zurich_hb() {
    // Zurich HB: known LV95 ~ E=2683100, N=1247900
    // The swisstopo approximate formula has limited precision; use +-1000m tolerance.
    let c = wgs84_to_lv95(47.3769, 8.5417);
    assert!(
        (c.e - 2683100.0).abs() < 1000.0,
        "Zurich easting: expected ~2683100, got {}",
        c.e
    );
    assert!(
        (c.n - 1247900.0).abs() < 1000.0,
        "Zurich northing: expected ~1247900, got {}",
        c.n
    );
}

#[test]
fn test_geneva_inside_swiss_bounds() {
    let c = wgs84_to_lv95(46.2044, 6.1432);
    assert!(
        is_in_swiss_bounds(&c),
        "Geneva should be inside Swiss bounds, got E={}, N={}",
        c.e,
        c.n
    );
}

#[test]
fn test_paris_outside_swiss_bounds() {
    let c = wgs84_to_lv95(48.8566, 2.3522);
    assert!(
        !is_in_swiss_bounds(&c),
        "Paris should be outside Swiss bounds, got E={}, N={}",
        c.e,
        c.n
    );
}

#[test]
fn test_is_in_swiss_bounds_inside() {
    let c = SwissCoord { e: 2600000.0, n: 1200000.0 };
    assert!(is_in_swiss_bounds(&c), "Center of Switzerland should be inside bounds");
}

#[test]
fn test_is_in_swiss_bounds_outside_west() {
    let c = SwissCoord { e: 2400000.0, n: 1200000.0 };
    assert!(!is_in_swiss_bounds(&c), "E=2400000 is west of Swiss bounds");
}

#[test]
fn test_is_in_swiss_bounds_outside_north() {
    let c = SwissCoord { e: 2600000.0, n: 1350000.0 };
    assert!(!is_in_swiss_bounds(&c), "N=1350000 is north of Swiss bounds");
}

#[test]
fn test_is_in_swiss_bounds_exact_boundary() {
    let c = SwissCoord { e: 2480000.0, n: 1070000.0 };
    assert!(is_in_swiss_bounds(&c), "Exact SW corner boundary should be inclusive");
}

#[test]
fn test_is_in_swiss_bounds_just_outside() {
    let c = SwissCoord { e: 2479999.0, n: 1200000.0 };
    assert!(!is_in_swiss_bounds(&c), "E=2479999 is just outside the western boundary");
}
