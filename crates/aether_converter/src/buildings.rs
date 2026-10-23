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

use std::fs;
use std::path::Path;

use anyhow::Result;

use crate::ingest::point_in_poly;

/// How a source expresses a building's height.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum BuildingHeight {
    /// Roof elevation, metres above mean sea level (e.g. FlatGeobuf geometry Z).
    Absolute(f64),
    /// Height in metres above the ground beneath the footprint (e.g. OSM).
    AboveGround(f64),
}

impl BuildingHeight {
    /// The metre value, whatever its datum.
    ///
    /// For a caller that already knows which variant it asked for — the datum
    /// is the whole point of the enum, so do not use this to compare two
    /// heights of different provenance.
    #[inline]
    pub fn metres(self) -> f64 {
        match self {
            BuildingHeight::Absolute(v) | BuildingHeight::AboveGround(v) => v,
        }
    }
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

impl HeightSource {
    /// True when the value came off the data rather than out of the default.
    #[inline]
    pub fn is_measured(self) -> bool {
        self != HeightSource::Default
    }
}

// ── The height ladder, shared by every building source ────────────────────

/// Attribute names carrying an explicit height in **metres above ground**,
/// in the order the ladder tries them.
///
/// `height` is the OSM spelling, `render_height` the one Planetiler-derived
/// vector tiles use, `building:height` a common variant. All three mean the
/// same thing: metres from the ground to the top of the building — never an
/// elevation above sea level.
pub const HEIGHT_ATTRS: [&str; 3] = ["height", "render_height", "building:height"];

/// Attribute names carrying a storey count, in the order the ladder tries them.
pub const LEVEL_ATTRS: [&str; 3] = ["levels", "building:levels", "building_levels"];

/// Metres of height one storey contributes.
pub const METRES_PER_LEVEL: f64 = 3.0;

/// Height applied when no rung of the ladder yielded anything usable.
pub const DEFAULT_HEIGHT_M: f64 = 6.0;

/// The leading float of an attribute value, if it has one.
///
/// Attribute heights arrive as free text far more often than as numbers —
/// `"12"`, `"12.5"`, `"12 m"` all occur in OSM — so the parse stops at the
/// first character that cannot continue a decimal number instead of demanding
/// the whole string be one. Anything with no leading digits (`""`, `"tall"`,
/// `"~"`) and anything non-finite is rejected; the sign is kept so a negative
/// value can be rejected by the caller rather than silently becoming positive.
pub fn parse_leading_metres(raw: &str) -> Option<f64> {
    let s = raw.trim();
    let bytes = s.as_bytes();
    let mut end = 0usize;
    let mut seen_digit = false;
    let mut seen_dot = false;
    while end < bytes.len() {
        match bytes[end] {
            b'0'..=b'9' => seen_digit = true,
            b'.' if !seen_dot => seen_dot = true,
            b'+' | b'-' if end == 0 => {}
            _ => break,
        }
        end += 1;
    }
    if !seen_digit {
        return None;
    }
    let v: f64 = s[..end].parse().ok()?;
    v.is_finite().then_some(v)
}

/// Resolve one building's height, from whatever the source could supply.
///
/// This is the **single** height ladder. Every source funnels into it so that
/// a FlatGeobuf and a vector tile carrying the same building end up at the
/// same roof, and so there is one place to read when the answer surprises
/// someone. In order:
///
/// 1. `absolute_z` — a roof elevation read from the geometry Z, metres above
///    mean sea level. The only rung that is *absolute*; when a source has it,
///    it is surveyed truth and nothing below can improve on it.
/// 2. An explicit height attribute ([`HEIGHT_ATTRS`]), **metres above ground**.
/// 3. A storey count ([`LEVEL_ATTRS`]) × [`METRES_PER_LEVEL`], above ground.
/// 4. [`DEFAULT_HEIGHT_M`] above ground.
///
/// # Attribute heights are never absolute
///
/// Rungs 2–4 are above-ground by definition and are returned as
/// [`BuildingHeight::AboveGround`] so [`rasterize_buildings`] resolves them
/// against the terrain under the footprint. Treating them as absolute would
/// put a 12 m building 528 m below the ground it stands on in Bern, and every
/// such building would then be dropped as being under the surface — silently,
/// because a roof below the terrain simply loses the `max`.
///
/// *lookup* returns the numeric value of an attribute, already parsed out of
/// whatever the container stores (a FlatGeobuf column, an MVT tag). A value it
/// returns is used only if it is finite and strictly positive; otherwise the
/// ladder moves on to the next rung.
pub fn resolve_building_height<F>(
    absolute_z: Option<f64>,
    mut lookup: F,
) -> (BuildingHeight, HeightSource)
where
    F: FnMut(&str) -> Option<f64>,
{
    if let Some(z) = absolute_z {
        return (BuildingHeight::Absolute(z), HeightSource::AbsoluteZ);
    }
    let usable = |v: Option<f64>| v.filter(|v| v.is_finite() && *v > 0.0);

    for key in HEIGHT_ATTRS {
        if let Some(m) = usable(lookup(key)) {
            return (BuildingHeight::AboveGround(m), HeightSource::ExplicitHeight);
        }
    }
    for key in LEVEL_ATTRS {
        if let Some(n) = usable(lookup(key)) {
            return (
                BuildingHeight::AboveGround(n * METRES_PER_LEVEL),
                HeightSource::Levels,
            );
        }
    }
    (
        BuildingHeight::AboveGround(DEFAULT_HEIGHT_M),
        HeightSource::Default,
    )
}

/// One building footprint in WGS84, with its height.
///
/// The footprint is the exterior ring **minus** its interior rings: a
/// courtyard belongs to its building as a hole, never as a building of its
/// own, so [`rasterize_buildings`] leaves it at the terrain.
#[derive(Clone, Debug)]
pub struct Building {
    /// Exterior ring vertices as (lon, lat).
    pub coords: Vec<(f64, f64)>,
    /// Interior rings (courtyards), vertices as (lon, lat). A pixel whose
    /// centre lies in one is not part of the footprint.
    pub holes: Vec<Vec<(f64, f64)>>,
    pub height: BuildingHeight,
    pub source: HeightSource,
}

impl Building {
    /// This building's identity for the buffer-zone duplicate filter.
    pub(crate) fn duplicate_key(&self) -> Option<DuplicateKey> {
        DuplicateKey::new(&self.coords, &self.holes, self.height)
    }
}

/// What makes two decoded vector-tile buildings **the same** building: every
/// vertex of every ring — exterior first, then the holes — rounded to 1e-6°,
/// in order, plus the height bit for bit.
///
/// Neighbouring vector tiles overlap in their buffer zones, so a building near
/// a tile edge arrives once per tile. This key drops only those true
/// duplicates. The first vertex alone, which the filter used to key on, also
/// matched every *distinct* building whose ring merely starts at a corner it
/// shares with a neighbour — on live OpenFreeMap tiles that deleted 7.3 % of
/// the footprint in Bern's old town and 4.1 % at Zürich HB.
///
/// A building that two tiles clip *differently* is not a duplicate under this
/// key and is rasterised once per copy. That is harmless: every roof is
/// resolved against the terrain before any is written, and the `max` write
/// rule never stacks one copy on the other.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct DuplicateKey {
    rings: Vec<Vec<(i64, i64)>>,
    /// The datum (0 absolute, 1 above ground) and the metres' bit pattern.
    height: (u8, u64),
}

impl DuplicateKey {
    /// `None` for a building without a single exterior vertex — such a
    /// building is dropped by [`drop_duplicate_buildings`], as it always was.
    pub(crate) fn new(
        exterior: &[(f64, f64)],
        holes: &[Vec<(f64, f64)>],
        height: BuildingHeight,
    ) -> Option<Self> {
        if exterior.is_empty() {
            return None;
        }
        let quantise = |ring: &[(f64, f64)]| -> Vec<(i64, i64)> {
            ring.iter()
                .map(|&(lon, lat)| {
                    ((lon * 1_000_000.0).round() as i64, (lat * 1_000_000.0).round() as i64)
                })
                .collect()
        };
        let mut rings = Vec::with_capacity(1 + holes.len());
        rings.push(quantise(exterior));
        rings.extend(holes.iter().map(|h| quantise(h)));
        let height = match height {
            BuildingHeight::Absolute(v) => (0, v.to_bits()),
            BuildingHeight::AboveGround(v) => (1, v.to_bits()),
        };
        Some(Self { rings, height })
    }
}

/// Keep the first of every group of buildings sharing a [`DuplicateKey`], in
/// order, and drop buildings that have no key. One rule for every place that
/// merges vector tiles: [`load_pbf_building_dir`], the WASM surface pass and
/// the GeoJSON export.
pub(crate) fn drop_duplicate_buildings<T>(
    buildings: &mut Vec<T>,
    key: impl Fn(&T) -> Option<DuplicateKey>,
) {
    let mut seen = std::collections::HashSet::new();
    buildings.retain(|b| key(b).is_some_and(|k| seen.insert(k)));
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
    pub(crate) fn to_half_metres(self, metres: f64) -> i16 {
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

/// Georeferencing and row stride of an already-written `.abt` buffer.
///
/// The 44-byte header is: `AETH`, `u16` version, `u16` size_px, then `f64`
/// ul_lat / ul_lon / scale_y / scale_x, an `i16` base elevation and the `u16`
/// row stride in bytes. Returns `None` for anything too short to hold one —
/// callers treat that as "not a tile" and leave the buffer alone.
pub fn parse_abt_header(buf: &[u8]) -> Option<(TileRef, usize)> {
    use byteorder::{LittleEndian, ReadBytesExt};
    use std::io::Cursor;

    if buf.len() < 44 {
        return None;
    }
    let mut c = Cursor::new(&buf[4..44]);
    let _version = c.read_u16::<LittleEndian>().ok()?;
    let size_px = c.read_u16::<LittleEndian>().ok()? as u32;
    let ul_lat = c.read_f64::<LittleEndian>().ok()?;
    let ul_lon = c.read_f64::<LittleEndian>().ok()?;
    let scale_y = c.read_f64::<LittleEndian>().ok()?;
    let scale_x = c.read_f64::<LittleEndian>().ok()?;
    let _base_elev = c.read_i16::<LittleEndian>().ok()?;
    let stride = c.read_u16::<LittleEndian>().ok()? as usize;

    Some((TileRef { ul_lat, ul_lon, scale_x, scale_y, size_px }, stride))
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

/// Pixels of *grid* whose centre falls inside *ring* and outside every one of
/// *holes*, as (x, y).
///
/// Per-pixel point-in-polygon, so a hole is simply subtracted: a centre counts
/// when it is in the exterior and in no interior ring. The scan stays inside
/// the exterior's bounding box, and a building without holes takes exactly the
/// path it always took.
fn footprint_pixels<G: ElevGrid>(
    grid: &G,
    tile: &TileRef,
    ring: &[(f64, f64)],
    holes: &[Vec<(f64, f64)>],
) -> Vec<(u32, u32)> {
    let size = grid.size_px() as f64;
    let to_px = |&(lon, lat): &(f64, f64)| {
        ((lon - tile.ul_lon) / tile.scale_x, (tile.ul_lat - lat) / tile.scale_y)
    };
    let mut verts: Vec<(f64, f64)> = Vec::with_capacity(ring.len());
    let (mut min_x, mut max_x) = (size, 0.0f64);
    let (mut min_y, mut max_y) = (size, 0.0f64);

    for v in ring {
        let (px, py) = to_px(v);
        min_x = min_x.min(px);
        max_x = max_x.max(px);
        min_y = min_y.min(py);
        max_y = max_y.max(py);
        verts.push((px, py));
    }

    if max_x < 0.0 || min_x >= size || max_y < 0.0 || min_y >= size {
        return Vec::new();
    }

    // A ring of fewer than three vertices encloses nothing (and an empty one
    // would underflow `point_in_poly`'s index).
    let hole_verts: Vec<Vec<(f64, f64)>> = holes
        .iter()
        .filter(|h| h.len() >= 3)
        .map(|h| h.iter().map(to_px).collect())
        .collect();

    let start_x = min_x.floor().max(0.0) as u32;
    let end_x = max_x.ceil().min(size) as u32;
    let start_y = min_y.floor().max(0.0) as u32;
    let end_y = max_y.ceil().min(size) as u32;

    let mut out = Vec::new();
    for y in start_y..end_y {
        let cy = y as f64 + 0.5;
        for x in start_x..end_x {
            let cx = x as f64 + 0.5;
            if point_in_poly(cx, cy, &verts)
                && !hole_verts.iter().any(|h| point_in_poly(cx, cy, h))
            {
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
/// A building's footprint is its exterior ring minus its [`Building::holes`]:
/// a courtyard is neither raised nor sampled as the building's ground.
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
        let pixels = footprint_pixels(grid, tile, &bldg.coords, &bldg.holes);
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
        for (x, y) in footprint_pixels(grid, tile, &bldg.coords, &bldg.holes) {
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

/// Every building in a directory of `{z}_{x}_{y}.pbf` vector tiles, decoded once.
///
/// The set does not depend on which output tile it will be drawn onto, so both
/// pipelines that consume such a directory — `ingest`'s per-tile rasterization
/// and `download`'s post-pass — load it once per run and share it across tiles.
#[derive(Debug, Default)]
pub struct PbfBuildingSet {
    /// Deduplicated buildings, in tile-file **name** order (and, within a
    /// tile, in the tile's own feature order).
    pub buildings: Vec<Building>,
    /// `{z}_{x}_{y}` tile files in the directory, decoded or skipped.
    pub tiles_found: usize,
    /// PBF tiles that decoded successfully.
    pub tiles_read: usize,
    /// Buildings decoded, before duplicate footprints were dropped.
    pub decoded: usize,
    /// The zoom level every tile in the directory shared, or `None` when no
    /// `{z}_{x}_{y}` file was found.
    pub zoom: Option<u32>,
}

impl PbfBuildingSet {
    /// How many decoded buildings were dropped as duplicates.
    pub fn duplicates_dropped(&self) -> usize {
        self.decoded - self.buildings.len()
    }
}

/// Decode every `{z}_{x}_{y}.pbf` tile in *pbf_dir* into one shared building set.
///
/// [`load_pbf_building_dir_with_progress`] without the progress callback.
pub fn load_pbf_building_dir(pbf_dir: &Path) -> Result<PbfBuildingSet> {
    load_pbf_building_dir_with_progress(pbf_dir, &|_, _| {})
}

/// One `{z}_{x}_{y}` tile file of a building directory.
struct PbfTileFile {
    name: String,
    path: std::path::PathBuf,
    z: u32,
    x: u32,
    y: u32,
}

/// What reading and decoding one tile file came to.
enum PbfTileOutcome {
    Decoded(Vec<Building>),
    /// Skipped with a `[Warn] buildings <name>: <error>` line.
    Undecodable(anyhow::Error),
    /// The read failed, which fails the whole load.
    Unreadable(std::io::Error),
    /// Not read: a tile before it (in name order) was unreadable, so the
    /// load fails at that one and never looks at this.
    NotReached,
}

/// Read and decode one tile file. Never fails the load by itself: the
/// caller decides, in name order, what an outcome means.
fn decode_pbf_tile_file(f: &PbfTileFile) -> PbfTileOutcome {
    let raw = match fs::read(&f.path) {
        Ok(raw) => raw,
        Err(e) => return PbfTileOutcome::Unreadable(e),
    };
    let bytes = crate::mvt::maybe_gunzip(&raw);
    match crate::mvt::extract_buildings_from_pbf(&bytes, f.x, f.y, f.z) {
        Ok((buildings, _)) => {
            PbfTileOutcome::Decoded(buildings.iter().map(|b| b.to_building()).collect())
        }
        Err(e) => PbfTileOutcome::Undecodable(e),
    }
}

/// Decode every `{z}_{x}_{y}.pbf` tile in *pbf_dir* into one shared building
/// set, reporting each finished tile to *on_progress*.
///
/// Cost is `O(pbf tiles)` **per run**, not per output tile. Callers must load
/// once and rasterize the same set onto every tile; re-reading the directory
/// inside a tile loop turns one directory scan into one scan per `.abt` and
/// produces exactly the same buildings.
///
/// # Parallel, and the same result as one tile after the other
///
/// The tile files are taken in **name order** (byte-wise: the order NTFS lists
/// such names in; other file systems list them in an order of their own, ext4
/// by hash) and read and decoded in parallel on the current rayon pool — the
/// batch's pool when called inside its `install`, else the global one (the
/// `native` feature; without it, one after the other). The results are then
/// taken in name order, exactly as a sequential walk would take them: the
/// building list, so the duplicate filter's choices and every byte drawn from
/// the set; `tiles_read`; the `[Warn] buildings <name>: <error>` line of each
/// tile that does not decode (skipped, as before — printed in name order once
/// every tile is decoded, never from two threads at once); and the error of
/// the first tile, in name order, that cannot be read at all, which fails the
/// load as it always did (a tile after it is not read once that is known).
///
/// *on_progress(done, total)* is called once per tile file, as it finishes
/// (decoded, skipped or failed): `done` runs 1, 2, …, `total`, the calls never
/// overlap and arrive in that order. `total` is the directory's tile count
/// ([`PbfBuildingSet::tiles_found`]); a directory without tiles never calls it.
///
/// # Mixed zooms are refused
///
/// [`rasterize_buildings`] resolves each above-ground height against the terrain
/// it reads *before* it writes any roof, so one pass over a tile is
/// self-consistent — but a second pass would measure new roofs against the roofs
/// the first pass laid down. Two zoom levels covering the same ground are two
/// passes over the same buildings, so a mixed-zoom directory is refused rather
/// than silently compounded — before any tile is read.
pub fn load_pbf_building_dir_with_progress(
    pbf_dir: &Path,
    on_progress: &(dyn Fn(usize, usize) + Sync),
) -> Result<PbfBuildingSet> {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    if !pbf_dir.is_dir() {
        anyhow::bail!("buildings_pbf_dir {:?} is not a directory", pbf_dir);
    }

    let mut files: Vec<PbfTileFile> = Vec::new();
    for entry in fs::read_dir(pbf_dir)?.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else { continue };
        let Some((z, x, y)) = crate::ingest::parse_pbf_tile_name(name) else { continue };
        files.push(PbfTileFile { name: name.to_owned(), path, z, x, y });
    }
    files.sort_unstable_by(|a, b| a.name.cmp(&b.name));

    let mut set = PbfBuildingSet { tiles_found: files.len(), ..Default::default() };
    for f in &files {
        match set.zoom {
            None => set.zoom = Some(f.z),
            Some(z0) if z0 != f.z => anyhow::bail!(
                "buildings_pbf_dir mixes zoom levels ({z0} and {}); expected exactly one",
                f.z
            ),
            _ => {}
        }
    }

    let total = files.len();
    let done = Mutex::new(0usize);
    // The lowest index whose read failed so far. A tile past it is never
    // looked at by the walk below, so it need not be read; every tile before
    // the first failure in name order still is (this only ever holds the
    // index of a real failure, never one below the first).
    let first_unreadable = AtomicUsize::new(usize::MAX);
    let work = |(i, f): (usize, &PbfTileFile)| -> PbfTileOutcome {
        let outcome = if i > first_unreadable.load(Ordering::Relaxed) {
            PbfTileOutcome::NotReached
        } else {
            let outcome = decode_pbf_tile_file(f);
            if matches!(outcome, PbfTileOutcome::Unreadable(_)) {
                first_unreadable.fetch_min(i, Ordering::Relaxed);
            }
            outcome
        };
        // Counted and reported under one lock, so the calls come in order.
        let mut n = done.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        *n += 1;
        on_progress(*n, total);
        outcome
    };
    // An indexed `collect` keeps the input order whatever the thread timing.
    #[cfg(feature = "native")]
    let outcomes: Vec<PbfTileOutcome> = {
        use rayon::prelude::*;
        files.par_iter().enumerate().map(work).collect()
    };
    #[cfg(not(feature = "native"))]
    let outcomes: Vec<PbfTileOutcome> = files.iter().enumerate().map(work).collect();

    let decoded_total: usize = outcomes
        .iter()
        .map(|o| match o {
            PbfTileOutcome::Decoded(b) => b.len(),
            _ => 0,
        })
        .sum();
    set.buildings.reserve_exact(decoded_total);
    for (f, outcome) in files.iter().zip(outcomes) {
        match outcome {
            PbfTileOutcome::Decoded(buildings) => {
                set.tiles_read += 1;
                set.buildings.extend(buildings);
            }
            PbfTileOutcome::Undecodable(e) => eprintln!("[Warn] buildings {}: {e}", f.name),
            PbfTileOutcome::Unreadable(e) => return Err(e.into()),
            PbfTileOutcome::NotReached => {
                unreachable!("a tile past an unreadable one is never walked to")
            }
        }
    }

    // Tiles overlap in their buffer zones, so the same building arrives more
    // than once. The `max` write rule makes that harmless, but dropping the
    // duplicates saves rasterizing identical footprints repeatedly. Only a
    // building identical in every ring and in height is one (`DuplicateKey`);
    // one that two tiles clipped differently is drawn once per copy, which
    // `max` makes harmless too. `decoded - buildings.len()` is what the
    // `[Info] buildings_pbf_dir` / `[Buildings]` lines report as dropped.
    set.decoded = set.buildings.len();
    drop_duplicate_buildings(&mut set.buildings, Building::duplicate_key);

    Ok(set)
}

#[cfg(test)]
pub(crate) mod tests {
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
            holes: Vec::new(),
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
                    holes: Vec::new(),
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

    // ── Shared PBF directory loader ────────────────────────────────────────

    /// MVT zig-zag encoding of one geometry delta.
    fn zz(n: i32) -> u32 {
        ((n << 1) ^ (n >> 31)) as u32
    }

    /// A minimal but real MVT tile: one `building` layer, one square polygon.
    pub(crate) fn synthetic_building_tile() -> Vec<u8> {
        use crate::mvt::{Feature, Layer, Tile, Value};
        use prost::Message;

        // MoveTo(1) then LineTo(3) then ClosePath, in 0..4096 tile space.
        let geometry = vec![
            (1 << 3) | 1,
            zz(100),
            zz(100), // MoveTo 100,100
            (3 << 3) | 2,
            zz(400),
            zz(0), // LineTo +400,0
            zz(0),
            zz(400), // LineTo 0,+400
            zz(-400),
            zz(0), // LineTo -400,0
            (1 << 3) | 7, // ClosePath
        ];

        Tile {
            layers: vec![Layer {
                name: "building".into(),
                features: vec![Feature {
                    id: Some(1),
                    tags: vec![0, 0],
                    r#type: Some(3), // Polygon
                    geometry,
                }],
                keys: vec!["render_height".into()],
                values: vec![Value { double_val: Some(12.0), ..Default::default() }],
                extent: Some(4096),
                version: Some(2),
            }],
        }
        .encode_to_vec()
    }

    /// MVT command stream for one polygon feature: every ring, in order.
    pub(crate) fn poly_geometry(rings: &[Vec<(i32, i32)>]) -> Vec<u32> {
        let mut out = Vec::new();
        let (mut cx, mut cy) = (0i32, 0i32);
        for r in rings {
            out.extend([(1 << 3) | 1, zz(r[0].0 - cx), zz(r[0].1 - cy)]);
            (cx, cy) = r[0];
            out.push((((r.len() - 1) as u32) << 3) | 2);
            for &(x, y) in &r[1..] {
                out.extend([zz(x - cx), zz(y - cy)]);
                (cx, cy) = (x, y);
            }
            out.push((1 << 3) | 7);
        }
        out
    }

    /// An exterior ring in tile coordinates: clockwise with y down, i.e. a
    /// positive surveyor's-formula area, as MVT spec 2.1 defines one.
    pub(crate) fn exterior(x0: i32, y0: i32, x1: i32, y1: i32) -> Vec<(i32, i32)> {
        vec![(x0, y0), (x1, y0), (x1, y1), (x0, y1)]
    }

    /// An interior ring: the same rectangle wound the other way.
    pub(crate) fn interior(x0: i32, y0: i32, x1: i32, y1: i32) -> Vec<(i32, i32)> {
        vec![(x0, y0), (x0, y1), (x1, y1), (x1, y0)]
    }

    /// A `building` layer tile: one polygon feature per `(render_height, rings)`.
    pub(crate) fn building_tile(features: &[(f64, Vec<Vec<(i32, i32)>>)]) -> Vec<u8> {
        use crate::mvt::{Feature, Layer, Tile, Value};
        use prost::Message;

        let mut values = Vec::new();
        let mut feats = Vec::new();
        for (i, (height, rings)) in features.iter().enumerate() {
            values.push(Value { double_val: Some(*height), ..Default::default() });
            feats.push(Feature {
                id: Some(i as u64 + 1),
                tags: vec![0, i as u32],
                r#type: Some(3),
                geometry: poly_geometry(rings),
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

    /// A `TileRef` covering exactly the XYZ tile *(z, x, y)*, at `size` px.
    pub(crate) fn tile_ref_for(z: u32, x: u32, y: u32, size: u32) -> TileRef {
        use crate::download::{tx2lon, ty2lat};
        let (ul_lon, ul_lat) = (tx2lon(x, z), ty2lat(y, z));
        let (lr_lon, lr_lat) = (tx2lon(x + 1, z), ty2lat(y + 1, z));
        TileRef {
            ul_lat,
            ul_lon,
            scale_x: (lr_lon - ul_lon) / size as f64,
            scale_y: (ul_lat - lr_lat) / size as f64,
            size_px: size,
        }
    }

    /// Two same-zoom tiles with one building each, plus a file the scan ignores.
    fn pbf_dir_with_two_tiles() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("14_8531_5752.pbf"), synthetic_building_tile()).unwrap();
        std::fs::write(dir.path().join("14_8532_5752.pbf"), synthetic_building_tile()).unwrap();
        std::fs::write(dir.path().join("README.txt"), b"not a tile").unwrap();
        dir
    }

    #[test]
    fn a_pbf_directory_decodes_to_buildings() {
        let dir = pbf_dir_with_two_tiles();
        let set = load_pbf_building_dir(dir.path()).unwrap();
        assert_eq!(set.tiles_read, 2, "the non-tile file must be skipped, not read");
        assert_eq!(set.zoom, Some(14));
        assert_eq!(set.buildings.len(), 2);
        // The height ladder's top rung: an explicit render_height, above ground.
        assert_eq!(set.buildings[0].height, BuildingHeight::AboveGround(12.0));
        assert_eq!(set.duplicates_dropped(), 0);
    }

    #[test]
    fn mixed_zoom_pbf_directories_are_refused() {
        // Two zooms covering the same ground would apply every above-ground
        // height twice — the second pass measuring from the first pass's roofs.
        // Ingest used to accept this silently; both paths now refuse it.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("14_8531_5752.pbf"), synthetic_building_tile()).unwrap();
        std::fs::write(dir.path().join("15_17062_11504.pbf"), synthetic_building_tile()).unwrap();

        let err = load_pbf_building_dir(dir.path()).unwrap_err().to_string();
        assert!(
            err.contains("buildings_pbf_dir mixes zoom levels"),
            "the contract freezes this message; got {err:?}"
        );
    }

    #[test]
    fn a_missing_pbf_directory_is_an_error_not_an_empty_set() {
        let dir = tempfile::tempdir().unwrap();
        let err = load_pbf_building_dir(&dir.path().join("nope")).unwrap_err().to_string();
        assert!(err.contains("is not a directory"), "got {err:?}");
    }

    #[test]
    fn loading_a_pbf_directory_is_deterministic() {
        // The hoist in E1 is only safe if the decode is a pure function of the
        // directory — same files in, same buildings in the same order out.
        let dir = pbf_dir_with_two_tiles();
        let a = load_pbf_building_dir(dir.path()).unwrap();
        let b = load_pbf_building_dir(dir.path()).unwrap();

        let key = |s: &PbfBuildingSet| -> Vec<(Vec<(f64, f64)>, BuildingHeight)> {
            s.buildings.iter().map(|b| (b.coords.clone(), b.height)).collect()
        };
        assert_eq!(key(&a), key(&b));
        assert_eq!((a.tiles_read, a.decoded, a.zoom), (b.tiles_read, b.decoded, b.zoom));
    }

    #[test]
    fn hoisting_the_pbf_decode_out_of_the_tile_loop_is_pixel_identical() {
        // E1: `apply_buildings_pbf` used to re-scan and re-decode the whole PBF
        // directory once per output .abt. This pins that moving the decode out
        // of that loop changes nothing but the number of decodes.
        let dir = pbf_dir_with_two_tiles();
        let size = 64u32;
        let opts = RasterOpts::default();
        let tiles: Vec<TileRef> = (0..3)
            .map(|i| tile_ref_for(14, 8531 + i % 2, 5752, size))
            .collect();

        // Old shape: decode inside the loop, once per output tile.
        let per_tile: Vec<Vec<i16>> = tiles
            .iter()
            .map(|t| {
                let set = load_pbf_building_dir(dir.path()).unwrap();
                let mut buf = vec![200i16; (size * size) as usize];
                let mut g = I16Grid { buf: &mut buf, size };
                rasterize_buildings(&mut g, t, &set.buildings, &opts);
                buf
            })
            .collect();

        // New shape: decode once for the run, share the set across tiles.
        let shared = load_pbf_building_dir(dir.path()).unwrap();
        let hoisted: Vec<Vec<i16>> = tiles
            .iter()
            .map(|t| {
                let mut buf = vec![200i16; (size * size) as usize];
                let mut g = I16Grid { buf: &mut buf, size };
                rasterize_buildings(&mut g, t, &shared.buildings, &opts);
                buf
            })
            .collect();

        assert_eq!(per_tile, hoisted);
        assert!(
            per_tile[0].iter().any(|&v| v != 200),
            "the fixture must actually draw a building, or this proves nothing"
        );
    }

    // ── The parallel decode (changelog 44) ─────────────────────────────────

    /// The loader as it was before its decode went parallel, verbatim but for
    /// its walk order: it takes the tile files in the order *cmp* puts their
    /// paths in instead of `read_dir` order. With [`by_name`] that is the
    /// order the parallel loader fixes on every platform, and the one NTFS
    /// lists these names in (see the test below).
    fn load_pbf_building_dir_sequential(
        pbf_dir: &Path,
        cmp: impl FnMut(&std::path::PathBuf, &std::path::PathBuf) -> std::cmp::Ordering,
    ) -> Result<PbfBuildingSet> {
        if !pbf_dir.is_dir() {
            anyhow::bail!("buildings_pbf_dir {:?} is not a directory", pbf_dir);
        }

        let mut set = PbfBuildingSet::default();
        let mut paths: Vec<std::path::PathBuf> =
            fs::read_dir(pbf_dir)?.flatten().map(|e| e.path()).collect();
        paths.sort_by(cmp);
        for path in paths {
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else { continue };
            let Some((z, x, y)) = crate::ingest::parse_pbf_tile_name(name) else { continue };
            match set.zoom {
                None => set.zoom = Some(z),
                Some(z0) if z0 != z => anyhow::bail!(
                    "buildings_pbf_dir mixes zoom levels ({z0} and {z}); expected exactly one"
                ),
                _ => {}
            }
            let raw = fs::read(&path)?;
            let bytes = crate::mvt::maybe_gunzip(&raw);
            match crate::mvt::extract_buildings_from_pbf(&bytes, x, y, z) {
                Ok((buildings, _)) => {
                    set.tiles_read += 1;
                    set.buildings.extend(buildings.iter().map(|b| b.to_building()));
                }
                Err(e) => eprintln!("[Warn] buildings {name}: {e}"),
            }
        }

        set.decoded = set.buildings.len();
        drop_duplicate_buildings(&mut set.buildings, Building::duplicate_key);

        Ok(set)
    }

    fn by_name(a: &std::path::PathBuf, b: &std::path::PathBuf) -> std::cmp::Ordering {
        a.file_name().cmp(&b.file_name())
    }

    /// Bit pattern of a ring.
    type RingBits = Vec<(u64, u64)>;

    /// Every building of *set*, everything it carries bit for bit, in order.
    fn building_bits(set: &PbfBuildingSet) -> Vec<(RingBits, Vec<RingBits>, (bool, u64), HeightSource)> {
        let ring = |r: &[(f64, f64)]| -> RingBits {
            r.iter().map(|&(lon, lat)| (lon.to_bits(), lat.to_bits())).collect()
        };
        set.buildings
            .iter()
            .map(|b| {
                let height = match b.height {
                    BuildingHeight::Absolute(v) => (true, v.to_bits()),
                    BuildingHeight::AboveGround(v) => (false, v.to_bits()),
                };
                (ring(&b.coords), b.holes.iter().map(|h| ring(h)).collect(), height, b.source)
            })
            .collect()
    }

    /// The `(x, y)` of tile *i* of [`many_tile_dir`]: rows of 40 at z14, x
    /// crossing 9999 → 10000, so name order ("14_10000_…" < "14_9980_…") is
    /// neither numeric nor creation order.
    fn many_tile_xy(i: usize) -> (u32, u32) {
        (9980 + (i % 40) as u32, 5700 + (i / 40) as u32)
    }

    /// A z14 directory of *n* building tiles (see [`many_tile_xy`]) holding
    /// what a real fetch holds: a house at each tile's east edge and its copy
    /// in the east neighbour's buffer zone (the duplicate filter's case,
    /// across tiles), a building with a courtyard in every other tile, every
    /// third tile gzipped, and a file the scan ignores. The tiles listed in
    /// *bad* are written as bytes that do not decode.
    fn many_tile_dir(n: usize, bad: &[usize]) -> tempfile::TempDir {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        // The east-edge house of tile i: its ring at the east edge, and the
        // same ground 4096 units further west, as the east neighbour has it.
        let house = |i: usize, dx: i32| -> (f64, Vec<Vec<(i32, i32)>>) {
            let k = (i % 50) as i32;
            (10.0 + (i % 7) as f64, vec![exterior(4000 - dx, 1000 + k, 4090 - dx, 1090 + k)])
        };
        for i in 0..n {
            let (x, y) = many_tile_xy(i);
            let bytes = if bad.contains(&i) {
                b"\x1a\xff\xff\xff\x0fnot a vector tile".to_vec()
            } else {
                let mut features = vec![house(i, 0)];
                if i % 40 != 0 {
                    features.push(house(i - 1, 4096));
                }
                if i % 2 == 0 {
                    let k = (i % 300) as i32;
                    features.push((
                        6.0 + (i % 5) as f64,
                        vec![exterior(500, 500, 1500 + k, 1500), interior(800, 800, 1000, 1000)],
                    ));
                }
                let tile = building_tile(&features);
                if i % 3 == 0 {
                    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
                    gz.write_all(&tile).unwrap();
                    gz.finish().unwrap()
                } else {
                    tile
                }
            };
            std::fs::write(dir.path().join(format!("14_{x}_{y}.pbf")), bytes).unwrap();
        }
        std::fs::write(dir.path().join("README.txt"), b"not a tile").unwrap();
        dir
    }

    #[cfg(feature = "native")]
    fn pool(threads: usize) -> rayon::ThreadPool {
        rayon::ThreadPoolBuilder::new().num_threads(threads).build().unwrap()
    }

    #[cfg(feature = "native")]
    #[test]
    fn the_parallel_decode_is_the_sequential_walk_in_name_order() {
        let n = 1500;
        let dir = many_tile_dir(n, &[17, 1203]);
        let reference = load_pbf_building_dir_sequential(dir.path(), by_name).unwrap();
        let expected = building_bits(&reference);
        assert_eq!(reference.tiles_read, n - 2);
        assert!(
            reference.duplicates_dropped() > 1000,
            "the buffer-zone copies must reach the duplicate filter: {} dropped",
            reference.duplicates_dropped()
        );

        // Any pool size, and rayon's global pool: the same list, bit for bit.
        for threads in [1, 3, 8] {
            let set = pool(threads).install(|| load_pbf_building_dir(dir.path())).unwrap();
            assert!(building_bits(&set) == expected, "{threads} thread(s): building list differs");
            assert_eq!(
                (set.tiles_found, set.tiles_read, set.decoded, set.zoom),
                (n, reference.tiles_read, reference.decoded, reference.zoom),
                "{threads} thread(s)"
            );
        }
        let global = load_pbf_building_dir(dir.path()).unwrap();
        assert!(building_bits(&global) == expected, "global pool: building list differs");

        // Name order indeed: the first building is the east-edge house of
        // 14_10000_5700, the first name in byte order — not of 14_9980_5700,
        // the first tile written and the lowest x.
        let lon = global.buildings[0].coords[0].0;
        let (w, e) = (crate::download::tx2lon(10000, 14), crate::download::tx2lon(10001, 14));
        assert!(lon > w && lon < e, "first building at lon {lon}, not in tile x=10000 ({w}..{e})");

        // And the order matters: a walk in numeric order keeps the other
        // copy of each house at the 9999|10000 seam and lists the tiles
        // differently, so this comparison would see a wrong order.
        let numeric = load_pbf_building_dir_sequential(dir.path(), |a, b| {
            let xy = |p: &std::path::PathBuf| {
                let (_, x, y) = crate::ingest::parse_pbf_tile_name(p.file_name()?.to_str()?)?;
                Some((y, x))
            };
            xy(a).cmp(&xy(b))
        })
        .unwrap();
        assert!(building_bits(&numeric) != expected);
    }

    #[cfg(feature = "native")]
    #[test]
    fn the_parallel_decode_burns_the_same_bytes_as_the_sequential_walk() {
        // What ingest does with the set, over the 9999|10000 seam where the
        // name order picks which copy of a house survives: every byte and
        // every count the `[Info] buildings_pbf_dir` line reports.
        let dir = many_tile_dir(400, &[123]);
        let reference = load_pbf_building_dir_sequential(dir.path(), by_name).unwrap();
        let parallel = pool(8).install(|| load_pbf_building_dir(dir.path())).unwrap();

        let size = 1024u32;
        let (ul_lon, lr_lon) = (crate::download::tx2lon(9994, 14), crate::download::tx2lon(10006, 14));
        let (ul_lat, lr_lat) = (crate::download::ty2lat(5700, 14), crate::download::ty2lat(5710, 14));
        let tile = TileRef {
            ul_lat,
            ul_lon,
            scale_x: (lr_lon - ul_lon) / size as f64,
            scale_y: (ul_lat - lr_lat) / size as f64,
            size_px: size,
        };
        let terrain: Vec<i16> =
            (0..size * size).map(|i| 1000 + (((i % size) * 7 + (i / size) * 13) % 211) as i16).collect();
        let burn = |set: &PbfBuildingSet| {
            let mut buf = terrain.clone();
            let mut g = I16Grid { buf: &mut buf, size };
            let s = rasterize_buildings(&mut g, &tile, &set.buildings, &RasterOpts::default());
            (buf, (s.buildings_hit, s.pixels_modified, s.buildings_off_tile))
        };
        let (want, want_stats) = burn(&reference);
        let (got, got_stats) = burn(&parallel);
        assert!(got == want, "the burnt surface differs");
        assert_eq!(got_stats, want_stats);
        assert!(want_stats.0 > 100, "the fixture must draw buildings: {want_stats:?}");
        assert_eq!(
            (parallel.tiles_read, parallel.decoded, parallel.buildings.len()),
            (reference.tiles_read, reference.decoded, reference.buildings.len())
        );
    }

    #[cfg(feature = "native")]
    #[test]
    fn the_decode_reports_every_tile_once_and_in_order() {
        let n = 1200;
        let dir = many_tile_dir(n, &[5]);
        let calls = std::sync::Mutex::new(Vec::new());
        let set = pool(8)
            .install(|| {
                load_pbf_building_dir_with_progress(dir.path(), &|done, total| {
                    calls.lock().unwrap().push((done, total))
                })
            })
            .unwrap();
        // Every tile file counts, the undecodable one included; the README
        // is no tile and does not.
        assert_eq!(calls.into_inner().unwrap(), (1..=n).map(|d| (d, n)).collect::<Vec<_>>());
        assert_eq!((set.tiles_found, set.tiles_read), (n, n - 1));

        let empty = tempfile::tempdir().unwrap();
        let set = load_pbf_building_dir_with_progress(empty.path(), &|_, _| panic!("no tiles"))
            .unwrap();
        assert_eq!((set.tiles_found, set.tiles_read, set.zoom), (0, 0, None));
    }

    #[test]
    fn an_undecodable_tile_is_skipped_and_the_rest_decode() {
        let dir = pbf_dir_with_two_tiles();
        std::fs::write(dir.path().join("14_8533_5752.pbf"), b"\x1a\xff\xff\xff\x0fnot a tile").unwrap();
        let set = load_pbf_building_dir(dir.path()).unwrap();
        let reference = load_pbf_building_dir_sequential(dir.path(), by_name).unwrap();
        assert_eq!((set.tiles_found, set.tiles_read, set.buildings.len()), (3, 2, 2));
        assert!(building_bits(&set) == building_bits(&reference));
    }

    #[test]
    fn an_unreadable_tile_still_fails_the_load() {
        // A directory where a tile file should be: listed, but no read of it
        // succeeds. That fails the run, as it always has, with the same error.
        let dir = many_tile_dir(60, &[]);
        std::fs::create_dir(dir.path().join("14_9990_5799.pbf")).unwrap();
        let err = load_pbf_building_dir(dir.path()).unwrap_err().to_string();
        let want = load_pbf_building_dir_sequential(dir.path(), by_name).unwrap_err().to_string();
        assert_eq!(err, want);

        // A mixed-zoom directory is refused before any tile is read, so its
        // error wins over the unreadable tile that sorts ahead of the odd
        // zoom (the sequential walk failed on whichever it met first).
        std::fs::write(dir.path().join("15_19980_11400.pbf"), synthetic_building_tile()).unwrap();
        let err = load_pbf_building_dir(dir.path()).unwrap_err().to_string();
        assert!(err.contains("buildings_pbf_dir mixes zoom levels (14 and 15)"), "got {err:?}");
    }

    #[test]
    fn name_order_is_the_order_ntfs_lists_tile_names_in() {
        // Why the name order changes no byte on Windows: NTFS lists a
        // directory in the order of its index — names compared as UTF-16
        // code units after upcasing — so `read_dir`, which the sequential
        // loader walked, already handed it tiles in byte order there. The
        // two orders differ only where a letter meets a non-letter, which
        // two accepted names never do (they part at a digit, `+`, `_` or
        // `.`, and their extensions are lowercase `pbf`/`mvt`). ext4 and
        // APFS list in an order of their own, which the old walk followed.
        let mut names = Vec::new();
        for z in [0u32, 3, 9, 13, 14, 15, 17, 20] {
            let n = 1u64 << z;
            for (x, y) in [(0, 0), (1, 10), (9, 99), (10, 9), (99, 100), (n - 1, n / 3), (n / 7, n - 1)] {
                for ext in ["pbf", "mvt"] {
                    names.push(format!("{z}_{x}_{y}.{ext}"));
                    names.push(format!("{z}_{x:05}_{y}.{ext}"));
                    names.push(format!("+{z}_+{x}_{y}.{ext}"));
                }
            }
        }
        for name in &names {
            assert!(crate::ingest::parse_pbf_tile_name(name).is_some(), "{name} must be a tile name");
        }
        let ntfs_key = |s: &String| -> Vec<u16> { s.to_ascii_uppercase().encode_utf16().collect() };
        let mut by_bytes = names.clone();
        by_bytes.sort();
        let mut by_ntfs = names.clone();
        by_ntfs.sort_by_key(ntfs_key);
        assert_eq!(by_bytes, by_ntfs);
    }

    /// Container-measured decode time of 2,000 tiles, sequential vs parallel.
    /// `cargo test --release -p aether_converter --lib -- --ignored
    /// --nocapture bench_pbf_decode`. `AETHER_PBF_BENCH_DIR` puts the tiles
    /// in a directory of your choice (another disk), else a temp dir.
    #[cfg(feature = "native")]
    #[test]
    #[ignore = "timing: [AETHER_PBF_BENCH_DIR=<dir on another disk>] cargo test --release -p aether_converter --lib bench_pbf_decode -- --ignored --nocapture"]
    fn bench_pbf_decode_2000_tiles_sequential_vs_parallel() {
        use std::io::Write;
        use std::time::Instant;
        let n = 2000;
        let real = std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/ofm_z14_8651_5782.pbf"))
            .expect("fixture ofm_z14_8651_5782.pbf");
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(&real).unwrap();
        let real_gz = gz.finish().unwrap();
        let cpus = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(0);
        let root = std::env::var_os("AETHER_PBF_BENCH_DIR").map(std::path::PathBuf::from);
        let threads = [2usize, 4, 8];

        for (what, content) in [
            ("OpenFreeMap tile 14/8651/5782 (470 buildings), gzipped", Some(real_gz)),
            ("synthetic tile (2-3 buildings)", None),
        ] {
            let tmp = match &root {
                Some(r) => tempfile::tempdir_in(r).unwrap(),
                None => tempfile::tempdir().unwrap(),
            };
            match &content {
                Some(bytes) => {
                    for i in 0..n {
                        let (x, y) = many_tile_xy(i);
                        std::fs::write(tmp.path().join(format!("14_{x}_{y}.pbf")), bytes).unwrap();
                    }
                }
                None => {
                    let src = many_tile_dir(n, &[]);
                    for e in std::fs::read_dir(src.path()).unwrap().flatten() {
                        std::fs::copy(e.path(), tmp.path().join(e.file_name())).unwrap();
                    }
                }
            }
            let p: &Path = tmp.path();
            let time = |f: &dyn Fn() -> PbfBuildingSet| -> (f64, PbfBuildingSet) {
                let t0 = Instant::now();
                let set = f();
                (t0.elapsed().as_secs_f64(), set)
            };
            let seq = || load_pbf_building_dir_sequential(p, by_name).unwrap();
            let pools = threads.map(pool);
            // A first pass (cold unless the files are still cached from
            // writing them), then the best of three each.
            let (cold_seq, reference) = time(&seq);
            let mut best = [f64::MAX; 4];
            for _ in 0..3 {
                for slot in 0..4 {
                    let (s, set) = match slot {
                        0 => time(&seq),
                        _ => time(&|| pools[slot - 1].install(|| load_pbf_building_dir(p).unwrap())),
                    };
                    assert!(building_bits(&set) == building_bits(&reference));
                    best[slot] = best[slot].min(s);
                }
            }
            // The duplicate filter runs after the decode, on one thread, in
            // both: its share bounds what the threads can win.
            let mut all = reference.buildings.clone();
            let t0 = Instant::now();
            drop_duplicate_buildings(&mut all, Building::duplicate_key);
            let filter_s = t0.elapsed().as_secs_f64();
            let par: Vec<String> = threads
                .iter()
                .enumerate()
                .map(|(i, t)| format!("{t} threads {:.3} s ({:.1}x)", best[i + 1], best[0] / best[i + 1]))
                .collect();
            println!(
                "[bench, container-measured, {cpus} CPU(s) available] {n} x {what}: {} buildings; \
                 sequential {:.3} s (first pass {cold_seq:.3} s), of which the duplicate \
                 filter ~{filter_s:.3} s; parallel {}",
                reference.decoded,
                best[0],
                par.join(", "),
            );
        }
    }

    // ── The buffer-zone duplicate filter ───────────────────────────────────

    /// A directory holding *tiles*, each `((x, y), its building features)` at z14.
    fn pbf_dir_of(tiles: &[((u32, u32), Vec<(f64, Vec<Vec<(i32, i32)>>)>)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for ((x, y), features) in tiles {
            std::fs::write(dir.path().join(format!("14_{x}_{y}.pbf")), building_tile(features))
                .unwrap();
        }
        dir
    }

    #[test]
    fn distinct_buildings_sharing_a_first_vertex_both_survive() {
        // Two neighbours whose rings both start at the corner they share —
        // ordinary in a block of terraced houses. The filter used to key on
        // the first vertex alone and deleted the second one (Bern's old town:
        // -7.3 % of the footprint on live OpenFreeMap tiles).
        let east = exterior(1000, 1000, 2000, 2000);
        let west = vec![(1000, 1000), (1000, 2000), (0, 2000), (0, 1000)];
        let dir = pbf_dir_of(&[((8531, 5752), vec![(12.0, vec![east]), (12.0, vec![west])])]);

        let set = load_pbf_building_dir(dir.path()).unwrap();
        assert_eq!(set.buildings[0].coords[0], set.buildings[1].coords[0], "fixture: one first vertex");
        assert_eq!((set.decoded, set.buildings.len(), set.duplicates_dropped()), (2, 2, 0));

        let size = 64u32;
        let mut buf = vec![200i16; (size * size) as usize];
        let mut g = I16Grid { buf: &mut buf, size };
        let stats = rasterize_buildings(
            &mut g,
            &tile_ref_for(14, 8531, 5752, size),
            &set.buildings,
            &RasterOpts::default(),
        );
        assert_eq!(stats.buildings_hit, 2, "both houses are drawn");
    }

    #[test]
    fn a_building_arriving_twice_through_the_buffer_is_kept_once() {
        // The same house near the seam of 8531|8532: x 4000..4090 of the west
        // tile is x -96..-6 in the east tile's buffer zone — the same ground,
        // decoded from two tiles.
        let copy_w = exterior(4000, 1000, 4090, 1090);
        let copy_e = exterior(-96, 1000, -6, 1090);
        let dir = pbf_dir_of(&[
            ((8531, 5752), vec![(12.0, vec![copy_w.clone()])]),
            ((8532, 5752), vec![(12.0, vec![copy_e.clone()])]),
        ]);
        let set = load_pbf_building_dir(dir.path()).unwrap();
        assert_eq!((set.decoded, set.buildings.len(), set.duplicates_dropped()), (2, 1, 1));

        // Not a duplicate: the same ring with another height, or the same
        // exterior with a courtyard the other copy lacks.
        let dir = pbf_dir_of(&[
            ((8531, 5752), vec![(12.0, vec![copy_w.clone()])]),
            ((8532, 5752), vec![(15.0, vec![copy_e.clone()])]),
        ]);
        assert_eq!(load_pbf_building_dir(dir.path()).unwrap().buildings.len(), 2);
        let dir = pbf_dir_of(&[
            ((8531, 5752), vec![(12.0, vec![copy_w])]),
            ((8532, 5752), vec![(12.0, vec![copy_e, interior(-80, 1020, -20, 1070)])]),
        ]);
        assert_eq!(load_pbf_building_dir(dir.path()).unwrap().buildings.len(), 2);
    }

    #[test]
    fn the_duplicate_key_is_every_vertex_in_order_and_the_height() {
        let ring = vec![(7.45, 46.95), (7.46, 46.95), (7.46, 46.94), (7.45, 46.94)];
        let key = |r: &[(f64, f64)], h: BuildingHeight| DuplicateKey::new(r, &[], h);
        let h = BuildingHeight::AboveGround(12.0);

        // Below the 1e-6° quantum is the same ring; one quantum off is not.
        let mut nudged = ring.clone();
        nudged[2].0 += 2.0e-7;
        assert_eq!(key(&ring, h), key(&nudged, h));
        nudged[2].0 += 1.0e-6;
        assert_ne!(key(&ring, h), key(&nudged, h));

        // Order counts, and so does the datum of the height.
        let mut rotated = ring.clone();
        rotated.rotate_left(1);
        assert_ne!(key(&ring, h), key(&rotated, h));
        assert_ne!(key(&ring, h), key(&ring, BuildingHeight::Absolute(12.0)));

        // No exterior vertex, no key: such a building is dropped, as before.
        assert_eq!(key(&[], h), None);
        let mut v = vec![1, 2, 1, 3];
        drop_duplicate_buildings(&mut v, |n| {
            (*n != 3).then(|| key(&ring, BuildingHeight::AboveGround(*n as f64))).flatten()
        });
        assert_eq!(v, vec![1, 2], "first of each kept, in order; keyless dropped");
    }

    // ── The shared height ladder ────────────────────────────────────────────

    /// The ladder over a fixed attribute table, as both call sites see it.
    fn ladder(z: Option<f64>, attrs: &[(&str, &str)]) -> (BuildingHeight, HeightSource) {
        resolve_building_height(z, |key| {
            attrs
                .iter()
                .find(|(k, _)| *k == key)
                .and_then(|(_, v)| parse_leading_metres(v))
        })
    }

    #[test]
    fn a_leading_float_is_parsed_out_of_whatever_the_attribute_says() {
        assert_eq!(parse_leading_metres("12"), Some(12.0));
        assert_eq!(parse_leading_metres("12.5"), Some(12.5));
        assert_eq!(parse_leading_metres("12 m"), Some(12.0));
        assert_eq!(parse_leading_metres(" 12.5m "), Some(12.5));
        assert_eq!(parse_leading_metres("3.5"), Some(3.5));
        // The sign survives the parse so the ladder can reject it, rather than
        // "-2" silently becoming a two-storey building.
        assert_eq!(parse_leading_metres("-2"), Some(-2.0));
        assert_eq!(parse_leading_metres(""), None);
        assert_eq!(parse_leading_metres("tall"), None);
        assert_eq!(parse_leading_metres("."), None);
        assert_eq!(parse_leading_metres("~"), None);
    }

    #[test]
    fn geometry_z_is_the_top_rung_and_is_absolute() {
        // Nothing below Z can improve on it, and its datum is sea level — the
        // one rung the rasterizer must NOT add to the terrain.
        let (h, src) = ladder(Some(560.0), &[("height", "12"), ("levels", "40")]);
        assert_eq!(h, BuildingHeight::Absolute(560.0));
        assert_eq!(src, HeightSource::AbsoluteZ);
    }

    #[test]
    fn every_rung_below_z_is_above_ground() {
        // The distinction is the whole point: a 12 m *absolute* roof on Bern's
        // 540 m terrain is underground, and every such building would be
        // dropped by the `max` composite without a word.
        for attrs in [
            vec![("height", "12")],
            vec![("render_height", "12")],
            vec![("building:height", "12")],
            vec![("levels", "4")],
            vec![("building:levels", "4")],
            vec![("building_levels", "4")],
            vec![],
        ] {
            let (h, _) = ladder(None, &attrs);
            assert!(
                matches!(h, BuildingHeight::AboveGround(_)),
                "{attrs:?} must be above-ground, got {h:?}"
            );
        }
    }

    #[test]
    fn the_ladder_is_tried_in_order() {
        // A measured height outranks a storey count outranks a guess, whatever
        // order the attributes happen to sit in on the feature.
        assert_eq!(
            ladder(None, &[("levels", "20"), ("height", "9")]),
            (BuildingHeight::AboveGround(9.0), HeightSource::ExplicitHeight)
        );
        assert_eq!(
            ladder(None, &[("height", "9"), ("render_height", "30")]),
            (BuildingHeight::AboveGround(9.0), HeightSource::ExplicitHeight)
        );
        assert_eq!(
            ladder(None, &[("building:levels", "4")]),
            (BuildingHeight::AboveGround(12.0), HeightSource::Levels)
        );
        assert_eq!(
            ladder(None, &[]),
            (BuildingHeight::AboveGround(DEFAULT_HEIGHT_M), HeightSource::Default)
        );
    }

    #[test]
    fn a_non_positive_or_unparseable_value_falls_to_the_next_rung() {
        // Not to zero, and not to a dropped building: an unusable value on one
        // rung says nothing about the next one.
        assert_eq!(
            ladder(None, &[("height", "0"), ("levels", "3")]),
            (BuildingHeight::AboveGround(9.0), HeightSource::Levels)
        );
        assert_eq!(
            ladder(None, &[("height", "-4"), ("levels", "3")]),
            (BuildingHeight::AboveGround(9.0), HeightSource::Levels)
        );
        assert_eq!(
            ladder(None, &[("height", "tall")]),
            (BuildingHeight::AboveGround(DEFAULT_HEIGHT_M), HeightSource::Default)
        );
        assert_eq!(
            ladder(None, &[("height", ""), ("levels", "")]),
            (BuildingHeight::AboveGround(DEFAULT_HEIGHT_M), HeightSource::Default)
        );
    }

    #[test]
    fn only_the_default_rung_is_unmeasured() {
        assert!(!HeightSource::Default.is_measured());
        for s in [HeightSource::AbsoluteZ, HeightSource::ExplicitHeight, HeightSource::Levels] {
            assert!(s.is_measured(), "{s:?}");
        }
    }

    #[test]
    fn a_vector_tile_reads_the_same_ladder_as_a_flatgeobuf() {
        // `HeightSource::ExplicitHeight` has always documented `height` as one
        // of the names it covers, while the vector-tile path only ever looked
        // at `render_height`. Both now walk the one ladder, so a tile keyed
        // `height` resolves exactly as the same attribute would off a
        // FlatGeobuf column — including a string value with a unit suffix.
        use crate::mvt::{extract_buildings_from_pbf, Feature, Layer, Tile, Value};
        use prost::Message;

        let tile_with = |key: &str, value: Value| {
            let geometry = vec![
                (1 << 3) | 1, zz(100), zz(100),
                (3 << 3) | 2, zz(400), zz(0), zz(0), zz(400), zz(-400), zz(0),
                (1 << 3) | 7,
            ];
            Tile {
                layers: vec![Layer {
                    name: "building".into(),
                    features: vec![Feature {
                        id: Some(1),
                        tags: vec![0, 0],
                        r#type: Some(3),
                        geometry,
                    }],
                    keys: vec![key.into()],
                    values: vec![value],
                    extent: Some(4096),
                    version: Some(2),
                }],
            }
            .encode_to_vec()
        };

        let height_of = |bytes: Vec<u8>| {
            let (b, _) = extract_buildings_from_pbf(&bytes, 8531, 5752, 14).unwrap();
            (b[0].height_m, b[0].source)
        };

        let string_val = |s: &str| Value { string_val: Some(s.into()), ..Default::default() };
        let double_val = |v: f64| Value { double_val: Some(v), ..Default::default() };

        assert_eq!(height_of(tile_with("height", double_val(12.0))),
                   (12.0, HeightSource::ExplicitHeight));
        assert_eq!(height_of(tile_with("height", string_val("12.5 m"))),
                   (12.5, HeightSource::ExplicitHeight));
        assert_eq!(height_of(tile_with("render_height", double_val(12.0))),
                   (12.0, HeightSource::ExplicitHeight));
        assert_eq!(height_of(tile_with("building:levels", string_val("4"))),
                   (12.0, HeightSource::Levels));
        assert_eq!(height_of(tile_with("levels", string_val("4"))),
                   (12.0, HeightSource::Levels));
        assert_eq!(height_of(tile_with("colour", string_val("red"))),
                   (DEFAULT_HEIGHT_M, HeightSource::Default));
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
