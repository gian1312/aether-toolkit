//! Shared building model and the single building rasterizer.
//!
//! Building data reaches us from several sources with different conventions —
//! FlatGeobuf carries an absolute roof elevation in the geometry Z, OSM-derived
//! vector tiles carry a height above ground — and each used to bring its own
//! rasterizer along. That produced two different write rules, only one of which
//! was correct:
//!
//! | | FlatGeobuf | vector tiles (old) |
//! |---|---|---|
//! | rule | `if roof > cur { cur = roof }` | `cur += height` |
//! | roof on a slope | flat | draped over the terrain |
//! | applied twice | idempotent | doubles every building |
//! | overlapping footprints | safe | stack on each other |
//!
//! This module keeps one rule — take the maximum of the current surface and an
//! **absolute** roof elevation — and makes every source normalise into it.
//! Heights that arrive above-ground are resolved against the terrain under
//! their own footprint before anything is written.

use crate::ingest::point_in_poly;

/// How a source expresses a building's height.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum BuildingHeight {
    /// Roof elevation, metres above mean sea level (e.g. FlatGeobuf geometry Z).
    Absolute(f64),
    /// Height in metres above the ground beneath the footprint (e.g. OSM).
    AboveGround(f64),
}

/// Which rung of the height ladder a value came from.
///
/// Carried through so callers can report confidence instead of presenting a
/// 6 m guess and a surveyed roof as though they were the same thing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HeightSource {
    /// Absolute roof elevation read from the geometry.
    AbsoluteZ,
    /// An explicit height in metres (`height`, `render_height`).
    ExplicitHeight,
    /// Derived from a storey count (`building:levels`).
    Levels,
    /// Nothing usable was found; a configured default was applied.
    Default,
}

/// One building footprint ring in WGS84, with its height.
#[derive(Clone, Debug)]
pub struct Building {
    /// Ring vertices as (lon, lat).
    pub coords: Vec<(f64, f64)>,
    pub height: BuildingHeight,
    pub source: HeightSource,
}

/// Which terrain statistic under a footprint counts as its ground level.
///
/// OSM heights are nominally measured from the lowest ground, which argues for
/// [`GroundRef::Min`] — but a single nodata pixel, a cliff edge or a river
/// clipping the footprint drags the raw minimum down and floats the whole
/// building. [`GroundRef::P10`] keeps the same intent while shrugging off
/// outliers, so it is the default.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum GroundRef {
    Min,
    #[default]
    P10,
    Median,
}

/// How a metre value is converted to the `.abt` half-metre i16 unit.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Rounding {
    /// Round to nearest. Correct, and the default for new callers.
    #[default]
    Nearest,
    /// Truncate toward zero.
    ///
    /// Only for the FlatGeobuf path, which has always truncated: keeping it
    /// means existing `.abt` output stays byte-for-byte identical.
    Truncate,
}

impl Rounding {
    #[inline]
    fn to_half_metres(self, metres: f64) -> i16 {
        let v = metres * 2.0;
        let v = match self {
            Rounding::Nearest => v.round(),
            Rounding::Truncate => v.trunc(),
        };
        v.clamp(i16::MIN as f64, i16::MAX as f64) as i16
    }
}

/// Geo-referencing for one elevation grid.
#[derive(Clone, Copy, Debug)]
pub struct TileRef {
    pub ul_lat: f64,
    pub ul_lon: f64,
    /// Degrees of longitude per pixel, increasing east.
    pub scale_x: f64,
    /// Degrees of latitude per pixel, increasing south.
    pub scale_y: f64,
    pub size_px: u32,
}

/// A mutable elevation grid addressed in pixels, valued in half-metre i16 units.
///
/// Implemented for both shapes we rasterize into: the flat `i16` grid the
/// ingest pipeline builds before writing a tile, and the strided byte buffer of
/// an already-written `.abt`.
pub trait ElevGrid {
    fn size_px(&self) -> u32;
    fn get(&self, x: u32, y: u32) -> Option<i16>;
    fn set(&mut self, x: u32, y: u32, v: i16);
}

/// Flat `size × size` i16 grid, as used while ingesting a tile.
pub struct I16Grid<'a> {
    pub buf: &'a mut [i16],
    pub size: u32,
}

impl ElevGrid for I16Grid<'_> {
    #[inline]
    fn size_px(&self) -> u32 {
        self.size
    }

    #[inline]
    fn get(&self, x: u32, y: u32) -> Option<i16> {
        self.buf.get((y * self.size + x) as usize).copied()
    }

    #[inline]
    fn set(&mut self, x: u32, y: u32, v: i16) {
        if let Some(slot) = self.buf.get_mut((y * self.size + x) as usize) {
            *slot = v;
        }
    }
}

/// An already-written `.abt` byte buffer: 44-byte header, then strided rows.
pub struct AbtGrid<'a> {
    pub buf: &'a mut [u8],
    pub size: u32,
    pub stride: usize,
}

impl AbtGrid<'_> {
    #[inline]
    fn offset(&self, x: u32, y: u32) -> Option<usize> {
        let off = 44 + y as usize * self.stride + x as usize * 2;
        if off + 1 < self.buf.len() {
            Some(off)
        } else {
            None
        }
    }
}

impl ElevGrid for AbtGrid<'_> {
    #[inline]
    fn size_px(&self) -> u32 {
        self.size
    }

    #[inline]
    fn get(&self, x: u32, y: u32) -> Option<i16> {
        let off = self.offset(x, y)?;
        Some(i16::from_le_bytes([self.buf[off], self.buf[off + 1]]))
    }

    #[inline]
    fn set(&mut self, x: u32, y: u32, v: i16) {
        if let Some(off) = self.offset(x, y) {
            self.buf[off..off + 2].copy_from_slice(&v.to_le_bytes());
        }
    }
}

/// Knobs for [`rasterize_buildings`].
#[derive(Clone, Copy, Debug)]
pub struct RasterOpts {
    pub ground_ref: GroundRef,
    pub rounding: Rounding,
    /// Plausible span, in metres, for `roof - terrain` on an absolute source.
    /// Used only to flag a suspect vertical datum; never to alter geometry.
    pub plausible_roof_agl: (f64, f64),
}

impl Default for RasterOpts {
    fn default() -> Self {
        Self {
            ground_ref: GroundRef::default(),
            rounding: Rounding::default(),
            plausible_roof_agl: (1.0, 300.0),
        }
    }
}

/// What one rasterize pass did, and whether the input looked trustworthy.
#[derive(Clone, Debug, Default)]
pub struct RasterizeStats {
    pub buildings_hit: u32,
    pub pixels_modified: u32,
    /// Buildings whose footprint fell entirely outside this tile.
    pub buildings_off_tile: u32,
    /// True when most absolute-height buildings sit at an implausible height
    /// above the terrain — usually a mislabelled AGL source or a mismatched
    /// vertical datum (ellipsoidal vs geoid is a ~50 m bias in Europe).
    pub datum_suspect: bool,
    /// Median of `roof - terrain` over absolute-height buildings, when known.
    /// A large constant offset here is the signature of a datum mismatch.
    pub median_roof_above_terrain: Option<f64>,
}

/// Pixels of *grid* whose centre falls inside *ring*, as (x, y).
fn footprint_pixels<G: ElevGrid>(grid: &G, tile: &TileRef, ring: &[(f64, f64)]) -> Vec<(u32, u32)> {
    let size = grid.size_px() as f64;
    let mut verts: Vec<(f64, f64)> = Vec::with_capacity(ring.len());
    let (mut min_x, mut max_x) = (size, 0.0f64);
    let (mut min_y, mut max_y) = (size, 0.0f64);

    for &(lon, lat) in ring {
        let px = (lon - tile.ul_lon) / tile.scale_x;
        let py = (tile.ul_lat - lat) / tile.scale_y;
        min_x = min_x.min(px);
        max_x = max_x.max(px);
        min_y = min_y.min(py);
        max_y = max_y.max(py);
        verts.push((px, py));
    }

    if max_x < 0.0 || min_x >= size || max_y < 0.0 || min_y >= size {
        return Vec::new();
    }

    let start_x = min_x.floor().max(0.0) as u32;
    let end_x = max_x.ceil().min(size) as u32;
    let start_y = min_y.floor().max(0.0) as u32;
    let end_y = max_y.ceil().min(size) as u32;

    let mut out = Vec::new();
    for y in start_y..end_y {
        let cy = y as f64 + 0.5;
        for x in start_x..end_x {
            if point_in_poly(x as f64 + 0.5, cy, &verts) {
                out.push((x, y));
            }
        }
    }
    out
}

/// The chosen ground statistic over a set of terrain samples (half-metre units).
fn ground_level(samples: &mut [i16], how: GroundRef) -> Option<i16> {
    if samples.is_empty() {
        return None;
    }
    samples.sort_unstable();
    let n = samples.len();
    let idx = match how {
        GroundRef::Min => 0,
        // Nearest-rank percentile; saturates to the minimum for tiny footprints.
        GroundRef::P10 => n / 10,
        GroundRef::Median => n / 2,
    };
    Some(samples[idx.min(n - 1)])
}

/// Rasterize *buildings* onto *grid*, compositing each roof with `max`.
///
/// Runs in two passes so that overlapping footprints cannot contaminate one
/// another: every ground reference is read while the surface is still pure
/// terrain, and only then is anything written. A single pass would let the
/// first building's roof masquerade as ground for the next.
///
/// # Precondition
///
/// *grid* must hold **building-free terrain**.
///
/// [`BuildingHeight::Absolute`] roofs do not care — writing them is idempotent,
/// because the value does not depend on what is underneath. But an
/// [`BuildingHeight::AboveGround`] height is resolved *against the surface it
/// is written onto*, so applying it twice measures the second building from the
/// first one's roof and the building grows. No rasterizer can undo that: once a
/// roof is baked into the surface, the terrain beneath it is gone.
///
/// Both call sites satisfy this by construction — the ingest pipeline rasterizes
/// onto terrain it has just built, and the tile pipeline onto tiles it has just
/// fetched. Callers that cache terrain must keep "with buildings" and "without
/// buildings" in separate cache entries so a baked tile is never re-fed here.
pub fn rasterize_buildings<G: ElevGrid>(
    grid: &mut G,
    tile: &TileRef,
    buildings: &[Building],
    opts: &RasterOpts,
) -> RasterizeStats {
    let mut stats = RasterizeStats::default();

    // ── Pass 1: resolve every height to an absolute roof, reading only ──
    let mut resolved: Vec<Option<i16>> = Vec::with_capacity(buildings.len());
    let mut roof_above_terrain: Vec<f64> = Vec::new();

    for bldg in buildings {
        if bldg.coords.len() < 3 {
            resolved.push(None);
            continue;
        }
        let pixels = footprint_pixels(grid, tile, &bldg.coords);
        if pixels.is_empty() {
            stats.buildings_off_tile += 1;
            resolved.push(None);
            continue;
        }

        let mut samples: Vec<i16> = pixels.iter().filter_map(|&(x, y)| grid.get(x, y)).collect();
        let ground = ground_level(&mut samples, opts.ground_ref);

        let roof = match bldg.height {
            BuildingHeight::Absolute(z) => {
                if let Some(g) = ground {
                    roof_above_terrain.push(z - g as f64 / 2.0);
                }
                opts.rounding.to_half_metres(z)
            }
            BuildingHeight::AboveGround(h) => {
                // Without terrain underneath there is no datum to add to, so
                // the building is skipped rather than placed at sea level.
                let Some(g) = ground else {
                    resolved.push(None);
                    continue;
                };
                let delta = opts.rounding.to_half_metres(h);
                if delta <= 0 {
                    resolved.push(None);
                    continue;
                }
                g.saturating_add(delta)
            }
        };
        resolved.push(Some(roof));
    }

    // A mislabelled or wrongly-datumed absolute source shows up as a roof that
    // sits implausibly far from the ground for most of its buildings.
    if !roof_above_terrain.is_empty() {
        roof_above_terrain.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let median = roof_above_terrain[roof_above_terrain.len() / 2];
        stats.median_roof_above_terrain = Some(median);
        let (lo, hi) = opts.plausible_roof_agl;
        let implausible = roof_above_terrain.iter().filter(|d| **d < lo || **d > hi).count();
        stats.datum_suspect = implausible * 2 > roof_above_terrain.len();
    }

    // ── Pass 2: write. `max` keeps this idempotent and overlap-safe ──
    for (bldg, roof) in buildings.iter().zip(resolved) {
        let Some(roof) = roof else { continue };
        let mut any = false;
        for (x, y) in footprint_pixels(grid, tile, &bldg.coords) {
            if let Some(cur) = grid.get(x, y) {
                if roof > cur {
                    grid.set(x, y, roof);
                    stats.pixels_modified += 1;
                    any = true;
                }
            }
        }
        if any {
            stats.buildings_hit += 1;
        }
    }

    stats
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tile() -> TileRef {
        // 10x10 px, 1 degree per 10 px, upper-left at (48N, 8E).
        TileRef { ul_lat: 48.0, ul_lon: 8.0, scale_x: 0.1, scale_y: 0.1, size_px: 10 }
    }

    /// A square covering roughly pixels x=2..5, y=2..5.
    fn square(source: HeightSource, height: BuildingHeight) -> Building {
        Building {
            coords: vec![
                (8.25, 47.75),
                (8.55, 47.75),
                (8.55, 47.45),
                (8.25, 47.45),
                (8.25, 47.75),
            ],
            height,
            source,
        }
    }

    fn flat_grid(value: i16) -> Vec<i16> {
        vec![value; 100]
    }

    #[test]
    fn above_ground_height_is_added_to_the_terrain_under_it() {
        // Terrain at 100 m (200 half-metres) + a 10 m building = 110 m.
        let mut buf = flat_grid(200);
        let mut g = I16Grid { buf: &mut buf, size: 10 };
        let b = square(HeightSource::ExplicitHeight, BuildingHeight::AboveGround(10.0));
        let stats = rasterize_buildings(&mut g, &tile(), &[b], &RasterOpts::default());

        assert!(stats.buildings_hit == 1);
        assert!(stats.pixels_modified > 0);
        assert_eq!(buf.iter().copied().max().unwrap(), 220);
    }

    #[test]
    fn absolute_height_ignores_the_terrain_under_it() {
        let mut buf = flat_grid(200);
        let mut g = I16Grid { buf: &mut buf, size: 10 };
        let b = square(HeightSource::AbsoluteZ, BuildingHeight::Absolute(150.0));
        rasterize_buildings(&mut g, &tile(), &[b], &RasterOpts::default());
        assert_eq!(buf.iter().copied().max().unwrap(), 300);
    }

    #[test]
    fn absolute_heights_are_idempotent() {
        // The old vector-tile rule added, so a second pass doubled every
        // building. An absolute roof does not depend on what is underneath,
        // so `max` makes re-application a no-op.
        let opts = RasterOpts::default();
        let b = square(HeightSource::AbsoluteZ, BuildingHeight::Absolute(150.0));

        let mut buf = flat_grid(200);
        {
            let mut g = I16Grid { buf: &mut buf, size: 10 };
            rasterize_buildings(&mut g, &tile(), std::slice::from_ref(&b), &opts);
        }
        let after_once = buf.clone();
        {
            let mut g = I16Grid { buf: &mut buf, size: 10 };
            rasterize_buildings(&mut g, &tile(), std::slice::from_ref(&b), &opts);
        }
        assert_eq!(buf, after_once);
    }

    #[test]
    fn above_ground_heights_need_building_free_terrain() {
        // Documents the precondition on `rasterize_buildings`: an AGL height is
        // measured from whatever surface it lands on, so re-applying it to
        // already-built terrain stacks. This is why callers must never feed a
        // baked tile back in — see the cache-key note on the function.
        let opts = RasterOpts::default();
        let b = square(HeightSource::ExplicitHeight, BuildingHeight::AboveGround(10.0));

        let mut buf = flat_grid(200);
        {
            let mut g = I16Grid { buf: &mut buf, size: 10 };
            rasterize_buildings(&mut g, &tile(), std::slice::from_ref(&b), &opts);
        }
        assert_eq!(buf.iter().copied().max().unwrap(), 220);
        {
            let mut g = I16Grid { buf: &mut buf, size: 10 };
            rasterize_buildings(&mut g, &tile(), std::slice::from_ref(&b), &opts);
        }
        assert_eq!(
            buf.iter().copied().max().unwrap(),
            240,
            "AGL re-application stacks; the guarantee is a caller precondition"
        );
    }

    #[test]
    fn overlapping_footprints_do_not_stack() {
        let opts = RasterOpts::default();
        let a = square(HeightSource::ExplicitHeight, BuildingHeight::AboveGround(10.0));
        let b = a.clone();

        let mut buf = flat_grid(200);
        let mut g = I16Grid { buf: &mut buf, size: 10 };
        rasterize_buildings(&mut g, &tile(), &[a, b], &opts);

        // 100 m + 10 m, not 100 m + 20 m.
        assert_eq!(buf.iter().copied().max().unwrap(), 220);
    }

    #[test]
    fn a_roof_is_flat_across_sloping_terrain() {
        // Terrain ramps west→east; the roof must still be one elevation.
        let mut buf = vec![0i16; 100];
        for y in 0..10 {
            for x in 0..10 {
                buf[y * 10 + x] = 200 + (x as i16 * 10);
            }
        }
        let mut g = I16Grid { buf: &mut buf, size: 10 };
        let b = square(HeightSource::ExplicitHeight, BuildingHeight::AboveGround(20.0));
        rasterize_buildings(&mut g, &tile(), &[b], &RasterOpts::default());

        // Collect the values that were actually raised above their terrain.
        let roofs: Vec<i16> = (0..10)
            .flat_map(|y| (0..10).map(move |x| (x, y)))
            .filter(|&(x, y)| buf[y * 10 + x] != 200 + (x as i16 * 10))
            .map(|(x, y)| buf[y * 10 + x])
            .collect();
        assert!(!roofs.is_empty());
        assert!(
            roofs.iter().all(|r| *r == roofs[0]),
            "roof should be flat, got {roofs:?}"
        );
    }

    #[test]
    fn ground_reference_shrugs_off_a_single_outlier() {
        // One pit pixel must not drag the whole building down.
        let mut samples = vec![200i16; 20];
        samples[0] = -400;
        let mut copy = samples.clone();
        assert_eq!(ground_level(&mut copy, GroundRef::Min), Some(-400));
        assert_eq!(ground_level(&mut samples.clone(), GroundRef::P10), Some(200));
        assert_eq!(ground_level(&mut samples, GroundRef::Median), Some(200));
    }

    #[test]
    fn truncation_matches_the_legacy_flatgeobuf_conversion() {
        // FlatGeobuf has always truncated `z * 2.0`; keeping that keeps
        // existing .abt bytes identical.
        assert_eq!(Rounding::Truncate.to_half_metres(10.3), 20);
        assert_eq!(Rounding::Nearest.to_half_metres(10.3), 21);
        assert_eq!(Rounding::Truncate.to_half_metres(10.0), 20);
        assert_eq!(Rounding::Nearest.to_half_metres(10.0), 20);
    }

    #[test]
    fn half_metre_conversion_saturates_instead_of_wrapping() {
        assert_eq!(Rounding::Nearest.to_half_metres(1.0e9), i16::MAX);
        assert_eq!(Rounding::Nearest.to_half_metres(-1.0e9), i16::MIN);
    }

    #[test]
    fn a_mislabelled_agl_source_is_flagged() {
        // Terrain at 500 m, but "absolute" roofs of ~12 m: clearly AGL values
        // wearing an absolute label.
        let mut buf = flat_grid(1000);
        let mut g = I16Grid { buf: &mut buf, size: 10 };
        let b = square(HeightSource::AbsoluteZ, BuildingHeight::Absolute(12.0));
        let stats = rasterize_buildings(&mut g, &tile(), &[b], &RasterOpts::default());
        assert!(stats.datum_suspect);
        assert_eq!(stats.median_roof_above_terrain, Some(12.0 - 500.0));
    }

    #[test]
    fn a_plausible_absolute_source_is_not_flagged() {
        let mut buf = flat_grid(1000); // 500 m
        let mut g = I16Grid { buf: &mut buf, size: 10 };
        let b = square(HeightSource::AbsoluteZ, BuildingHeight::Absolute(512.0));
        let stats = rasterize_buildings(&mut g, &tile(), &[b], &RasterOpts::default());
        assert!(!stats.datum_suspect);
        assert_eq!(stats.median_roof_above_terrain, Some(12.0));
    }

    #[test]
    fn buildings_outside_the_tile_are_counted_not_drawn() {
        let mut buf = flat_grid(200);
        let mut g = I16Grid { buf: &mut buf, size: 10 };
        let mut b = square(HeightSource::ExplicitHeight, BuildingHeight::AboveGround(10.0));
        b.coords = b.coords.iter().map(|&(lon, lat)| (lon + 50.0, lat)).collect();
        let stats = rasterize_buildings(&mut g, &tile(), &[b], &RasterOpts::default());
        assert_eq!(stats.buildings_off_tile, 1);
        assert_eq!(stats.pixels_modified, 0);
        assert!(buf.iter().all(|v| *v == 200));
    }

    /// The FlatGeobuf write loop exactly as it stood before this module existed.
    ///
    /// Kept verbatim so the port can be proved equivalent rather than asserted
    /// to be. Any divergence here is a real change to previously written .abt
    /// tiles.
    fn legacy_fgb_rasterize(
        buffer: &mut [i16],
        size: u32,
        ul_lon: f64,
        ul_lat: f64,
        px_deg: f64,
        ring: &[(f64, f64)],
        max_z: f64,
    ) {
        let roof_val = (max_z * 2.0) as i16;
        if roof_val < 0 {
            return;
        }
        if ring.len() < 3 {
            return;
        }

        let mut vertices: Vec<(f64, f64)> = Vec::with_capacity(ring.len());
        let mut min_x = size as f64;
        let mut max_x = 0.0f64;
        let mut min_y = size as f64;
        let mut max_y = 0.0f64;

        for &(lon, lat) in ring {
            let px = (lon - ul_lon) / px_deg;
            let py = (ul_lat - lat) / px_deg;
            if px < min_x { min_x = px; }
            if px > max_x { max_x = px; }
            if py < min_y { min_y = py; }
            if py > max_y { max_y = py; }
            vertices.push((px, py));
        }

        let start_x = min_x.floor().max(0.0) as u32;
        let end_x = max_x.ceil().min(size as f64) as u32;
        let start_y = min_y.floor().max(0.0) as u32;
        let end_y = max_y.ceil().min(size as f64) as u32;

        for y in start_y..end_y {
            let py_center = y as f64 + 0.5;
            for x in start_x..end_x {
                if point_in_poly(x as f64 + 0.5, py_center, &vertices) {
                    let idx = (y * size + x) as usize;
                    if idx < buffer.len() {
                        let current_h = buffer[idx];
                        if roof_val > current_h {
                            buffer[idx] = roof_val;
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn flatgeobuf_output_is_unchanged_by_the_port() {
        // Deterministic sweep over footprints, roof heights and terrain: the
        // shared rasterizer in Absolute+Truncate mode must reproduce the legacy
        // FlatGeobuf loop byte for byte.
        let size = 24u32;
        let t = TileRef {
            ul_lat: 48.0,
            ul_lon: 8.0,
            scale_x: 0.01,
            scale_y: 0.01,
            size_px: size,
        };
        let opts = RasterOpts { rounding: Rounding::Truncate, ..Default::default() };

        // Simple LCG so the cases are varied but reproducible.
        let mut seed = 0x2545F491u64;
        let mut next = move || {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            ((seed >> 33) as f64) / ((1u64 << 31) as f64)
        };

        for case in 0..200 {
            // Terrain: a ramp plus a per-case offset, sometimes below sea level.
            let base = if case % 5 == 0 { -60i16 } else { 40i16 };
            let terrain: Vec<i16> = (0..size * size)
                .map(|i| base + (i % 17) as i16 * 3)
                .collect();

            // A quadrilateral somewhere on (or off) the tile.
            let ox = next() * 1.4 - 0.2;
            let oy = next() * 1.4 - 0.2;
            let w = 0.02 + next() * 0.12;
            let h = 0.02 + next() * 0.12;
            let lon0 = t.ul_lon + ox * size as f64 * t.scale_x;
            let lat0 = t.ul_lat - oy * size as f64 * t.scale_y;
            let ring = vec![
                (lon0, lat0),
                (lon0 + w, lat0),
                (lon0 + w, lat0 - h),
                (lon0, lat0 - h),
                (lon0, lat0),
            ];
            // Roof heights straddling zero and with fractional half-metres, so
            // truncation and the negative-skip both get exercised.
            let max_z = next() * 120.0 - 20.0;

            let mut legacy = terrain.clone();
            legacy_fgb_rasterize(&mut legacy, size, t.ul_lon, t.ul_lat, t.scale_x, &ring, max_z);

            let mut ported = terrain.clone();
            // Mirrors collect_fgb_buildings: a roof truncating below zero is
            // dropped at the source rather than in the rasterizer.
            if (max_z * 2.0).trunc() >= 0.0 {
                let b = Building {
                    coords: ring.clone(),
                    height: BuildingHeight::Absolute(max_z),
                    source: HeightSource::AbsoluteZ,
                };
                let mut g = I16Grid { buf: &mut ported, size };
                rasterize_buildings(&mut g, &t, &[b], &opts);
            }

            assert_eq!(ported, legacy, "case {case} diverged (max_z={max_z})");
        }
    }

    #[test]
    fn pbf_tile_names_round_trip() {
        use crate::ingest::parse_pbf_tile_name;
        assert_eq!(parse_pbf_tile_name("14_8531_5752.pbf"), Some((14, 8531, 5752)));
        assert_eq!(parse_pbf_tile_name("14_8531_5752.mvt"), Some((14, 8531, 5752)));
        // Anything that is not exactly z_x_y is ignored rather than guessed at.
        assert_eq!(parse_pbf_tile_name("14_8531.pbf"), None);
        assert_eq!(parse_pbf_tile_name("14_8531_5752_extra.pbf"), None);
        assert_eq!(parse_pbf_tile_name("readme.txt"), None);
        assert_eq!(parse_pbf_tile_name("a_b_c.pbf"), None);
        assert_eq!(parse_pbf_tile_name("-1_2_3.pbf"), None);
    }

    #[test]
    fn the_abt_grid_reads_and_writes_through_its_stride() {
        // stride deliberately wider than size*2 to catch row-shear bugs.
        let size = 4u32;
        let stride = 16usize;
        let mut bytes = vec![0u8; 44 + stride * size as usize];
        {
            let mut g = AbtGrid { buf: &mut bytes, size, stride };
            g.set(3, 2, 1234);
            assert_eq!(g.get(3, 2), Some(1234));
            assert_eq!(g.get(0, 0), Some(0));
        }
        let off = 44 + 2 * stride + 3 * 2;
        assert_eq!(i16::from_le_bytes([bytes[off], bytes[off + 1]]), 1234);
    }
}
