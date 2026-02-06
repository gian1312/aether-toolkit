// Approximate conversion WGS84 (Lat/Lon) -> LV95 (Swiss Coordinate System)
// Source: swisstopo formulas (accurate to <1m for general usage)

pub struct SwissCoord {
    pub e: f64, // Easting
    pub n: f64, // Northing
}

pub fn wgs84_to_lv95(lat: f64, lon: f64) -> SwissCoord {
    // Reference constants
    let phi = (lat * 3600.0 - 169028.66) / 10000.0;
    let lam = (lon * 3600.0 - 26782.5) / 10000.0;

    let e = 2600072.37
        + 211455.93 * lam
        - 10938.51 * lam * phi
        - 0.36 * lam * phi * phi
        - 44.54 * lam * lam * lam;

    let n = 1200147.07
        + 308807.95 * phi
        + 3745.25 * lam * lam
        + 76.63 * phi * phi
        - 194.56 * lam * lam * phi
        + 119.79 * phi * phi * phi;

    SwissCoord { e, n }
}

pub fn is_in_swiss_bounds(c: &SwissCoord) -> bool {
    // Approximate bounding box of Switzerland in LV95
    c.e >= 2480000.0 && c.e <= 2840000.0 && c.n >= 1070000.0 && c.n <= 1300000.0
}