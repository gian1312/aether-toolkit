//! Forest canopy as an obstruction surface.
//!
//! Line-of-sight treats trees the way it treats buildings: opaque, and standing
//! on the terrain. So instead of a second raster and a second engine concept,
//! this module *raises the ground* under mapped forest by the canopy height.
//! What the viewshed kernel then occludes with is the surface it already knows.
//!
//! Three things separate it from [`crate::buildings`]:
//!
//! | | buildings | canopy |
//! |---|---|---|
//! | write rule | one flat roof per footprint | `ground + h·α`, draped per pixel |
//! | rings | every ring is its own footprint | all rings of a feature, **even-odd** |
//! | fill | point-in-polygon per pixel | scanline spans |
//!
//! Neither difference is cosmetic. A forest feature is routinely a multipolygon
//! whose inner rings are the clearings, the village and the lake inside it;
//! filling each ring on its own — which is what the building path does, and is
//! right for buildings — would plant trees in all of them. And forest polygons
//! are valley-sized with thousands of vertices, of which a 30 km run sees six
//! figures, so the per-pixel point-in-polygon loop that suits a 12 m house is
//! orders of magnitude too slow here.
//!
//! The forest mask itself comes from the same OpenMapTiles vector tiles the
//! building path already fetches: layer `landcover` with `class = wood`, plus
//! layer `park` with a managed-forest class. See [`extract_canopy_from_pbf`].

use anyhow::{anyhow, Result};
use prost::Message;
use serde::Deserialize;

use crate::buildings::{
    parse_abt_header, rasterize_buildings, AbtGrid, Building, ElevGrid, RasterOpts, Rounding,
    TileRef,
};
use crate::ingest::VOID_ELEV;
use crate::mvt::{
    decode_geometry, extract_buildings_from_tile, maybe_gunzip, BuildingPolygon, Feature, GeomType,
    Layer, Tile,
};

// ── The forest mask ────────────────────────────────────────────────────────

/// Which mapping convention put a polygon into the forest mask.
///
/// Carried out of the decoder so a caller can report *why* an area counted as
/// forest — the two rungs have very different precision.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CanopyKind {
    /// `landcover` / `class = wood`: actual mapped tree cover.
    Wood,
    /// `park` / `class ∈ {national_forest, state_forest, forest}`: a managed
    /// forest boundary. Coarser — the whole administrative area counts as
    /// wooded — but in the US it is often the only signal there is, because
    /// tree cover inside a national forest is frequently not mapped separately.
    /// Measured recall on the Hoosier National Forest: 0 % without it, 97 % with.
    ManagedForest,
}

/// One forest feature: **all** of its rings, in WGS84 (lon, lat).
///
/// Outer rings and holes stay together deliberately — the rasteriser fills the
/// whole set even-odd, which is the only way a clearing inside a wood stays
/// clear. Splitting a feature into one polygon per ring loses that.
#[derive(Clone, Debug)]
pub struct CanopyPolygon {
    pub rings: Vec<Vec<(f64, f64)>>,
    pub kind: CanopyKind,
}

/// `park` classes that stand in for tree cover the map does not carry.
const MANAGED_FOREST_CLASSES: [&str; 3] = ["national_forest", "state_forest", "forest"];

/// Diagnostic counters from one [`extract_canopy_from_pbf`] pass.
#[derive(Clone, Debug, Default)]
pub struct CanopyExtractStats {
    pub layers_total: usize,
    pub landcover_layer_found: bool,
    pub park_layer_found: bool,
    /// Features seen in the two candidate layers, before the class filter.
    pub features_total: usize,
    pub features_wood: usize,
    pub features_managed_forest: usize,
    /// Features in a candidate layer whose `class` was something else.
    pub features_other_class: usize,
    /// Features that were not polygons (labels, boundaries).
    pub features_other_type: usize,
    /// Features that passed the class filter but decoded to no usable ring.
    pub features_no_rings: usize,
    pub polygons_out: usize,
    pub rings_out: usize,
    pub points_out: usize,
}

/// The value of a feature's `class` attribute, if it has one.
///
/// Walks the `tags` key/value index pairs against the layer dictionaries, the
/// same way `mvt::resolve_height_detailed` reads a building's height.
fn feature_class<'a>(feature: &Feature, layer: &'a Layer) -> Option<&'a str> {
    let mut i = 0;
    while i + 1 < feature.tags.len() {
        let key_idx = feature.tags[i] as usize;
        let val_idx = feature.tags[i + 1] as usize;
        i += 2;
        if key_idx >= layer.keys.len() || val_idx >= layer.values.len() {
            continue;
        }
        if layer.keys[key_idx] == "class" {
            if let Some(ref s) = layer.values[val_idx].string_val {
                return Some(s.as_str());
            }
        }
    }
    None
}

/// Extract the forest mask from an already-decoded vector tile.
///
/// Split out from [`extract_canopy_from_pbf`] so the surface pipeline can decode
/// each PBF **once** and read both buildings and canopy out of the same `Tile`.
pub fn extract_canopy_from_tile(
    tile: &Tile,
    tile_x: u32,
    tile_y: u32,
    z: u32,
) -> (Vec<CanopyPolygon>, CanopyExtractStats) {
    let mut out: Vec<CanopyPolygon> = Vec::new();
    let mut stats = CanopyExtractStats { layers_total: tile.layers.len(), ..Default::default() };

    for layer in &tile.layers {
        let wants_wood = match layer.name.as_str() {
            "landcover" => {
                stats.landcover_layer_found = true;
                true
            }
            "park" => {
                stats.park_layer_found = true;
                false
            }
            _ => continue,
        };

        let extent = layer.extent.unwrap_or(4096);
        stats.features_total += layer.features.len();

        for feature in &layer.features {
            let geom_type = feature.r#type.map(GeomType::from_i32).unwrap_or(GeomType::Unknown);
            if geom_type != GeomType::Polygon || feature.geometry.is_empty() {
                stats.features_other_type += 1;
                continue;
            }

            let class = feature_class(feature, layer);
            let kind = match (wants_wood, class) {
                (true, Some("wood")) => CanopyKind::Wood,
                (false, Some(c)) if MANAGED_FOREST_CLASSES.contains(&c) => CanopyKind::ManagedForest,
                _ => {
                    stats.features_other_class += 1;
                    continue;
                }
            };
            match kind {
                CanopyKind::Wood => stats.features_wood += 1,
                CanopyKind::ManagedForest => stats.features_managed_forest += 1,
            }

            // Every ring of the feature, kept together: the rasteriser needs
            // the holes to cancel the outer rings.
            let rings = decode_geometry(&feature.geometry, extent, tile_x, tile_y, z);
            if rings.is_empty() {
                stats.features_no_rings += 1;
                continue;
            }
            stats.rings_out += rings.len();
            stats.points_out += rings.iter().map(|r| r.len()).sum::<usize>();
            out.push(CanopyPolygon { rings, kind });
        }
    }

    stats.polygons_out = out.len();
    (out, stats)
}

/// Decode one PBF vector tile into forest polygons in WGS84.
///
/// Forest is `landcover`/`wood` plus the managed-forest `park` classes; see
/// [`CanopyKind`] for why the second source is in.
///
/// *pbf_bytes* must already be plain protobuf — gunzip is the caller's job, as
/// on the building path (`mvt::maybe_gunzip`), so that a batch decompresses each
/// tile once for both layers.
pub fn extract_canopy_from_pbf(
    pbf_bytes: &[u8],
    tile_x: u32,
    tile_y: u32,
    z: u32,
) -> Result<(Vec<CanopyPolygon>, CanopyExtractStats)> {
    let tile = Tile::decode(pbf_bytes).map_err(|e| anyhow!("PBF decode error: {e}"))?;
    Ok(extract_canopy_from_tile(&tile, tile_x, tile_y, z))
}

// ── Rasteriser ─────────────────────────────────────────────────────────────

/// A patch of ground the canopy is not allowed to cover.
///
/// The observer's own pixel: someone standing in a wood is standing on the
/// forest floor, not on the treetops, and an antenna height AGL is measured
/// from there. Without this the eye starts at canopy level and the surrounding
/// trees stop obstructing anything.
#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(default)]
pub struct Carveout {
    pub lon: f64,
    pub lat: f64,
    /// Chebyshev radius in pixels. `1` keeps a 3×3 patch on the ground, so the
    /// first canopy pixel is immediately adjacent and blocks at once.
    pub radius_px: u32,
}

impl Default for Carveout {
    fn default() -> Self {
        Self { lon: 0.0, lat: 0.0, radius_px: 1 }
    }
}

/// Knobs for [`rasterize_canopy`].
#[derive(Clone, Copy, Debug, Default, Deserialize)]
#[serde(default)]
pub struct CanopyOpts {
    /// Canopy height above ground, in metres. `0` (the default) is a no-op —
    /// a caller that forgets the field gets no canopy rather than a silent
    /// guess at what the trees might be.
    pub height_m: f64,
    /// `false`: binary mask, a pixel is in or out by its centre.
    /// `true`: coverage α ∈ [0,1] from 4 sub-scanlines × exact span fractions,
    /// so a polygon edge ramps instead of stepping.
    pub antialias: bool,
    pub carveout: Option<Carveout>,
}

/// What one [`rasterize_canopy`] pass did.
#[derive(Clone, Debug, Default)]
pub struct CanopyStats {
    /// Polygons that raised at least one pixel.
    pub polygons_hit: u32,
    /// Writes that raised the surface. A pixel covered by two overlapping
    /// polygons is counted once per raising write.
    pub pixels_raised: u32,
    /// Writes skipped because the pixel fell in the observer carve-out, counted
    /// the same way as `pixels_raised`.
    pub pixels_carved: u32,
    /// Polygons whose bounding box missed this tile entirely.
    pub polygons_off_tile: u32,
}

/// Where the *pristine* ground under a pixel is read from.
///
/// The canopy rule is `ground + h`, and `ground` must be terrain — not a roof,
/// and not canopy from an earlier pass. Which of the two variants is correct is
/// decided by what has already been written into the grid; see
/// [`apply_surface_to_abt_tiles`].
#[derive(Clone, Copy, Debug)]
pub enum CanopyGround<'a> {
    /// The grid still holds bare terrain, so it *is* the ground. The rasteriser
    /// snapshots it on entry — reading it back live would let the first of two
    /// overlapping forest polygons become the second one's ground and stack the
    /// canopy, which is exactly what happens wherever vector-tile buffer zones
    /// deliver the same wood twice.
    Current,
    /// A row-major `size × size` snapshot taken before anything was written.
    /// Used when buildings have already gone down on the grid.
    Snapshot(&'a [i16]),
}

/// Read a whole [`ElevGrid`] into a row-major buffer.
fn snapshot_of<G: ElevGrid>(grid: &G) -> Vec<i16> {
    let n = grid.size_px() as usize;
    let mut out = vec![0i16; n * n];
    for y in 0..n {
        for x in 0..n {
            if let Some(v) = grid.get(x as u32, y as u32) {
                out[y * n + x] = v;
            }
        }
    }
    out
}

/// Half-metre values at or below this are "no terrain".
///
/// [`VOID_ELEV`] is the sentinel a converter writes, but every sampler in
/// [`crate::ingest`] tests `> -5000` (−2500 m) rather than equality, so the
/// same floor is used here. Raising a void would turn a hole in the DEM into a
/// 15 m tree standing at −5000 m, which occludes nothing but is still a lie.
const GROUND_FLOOR: i16 = -5000;

#[inline]
fn is_void(v: i16) -> bool {
    debug_assert!(VOID_ELEV <= GROUND_FLOOR);
    v <= GROUND_FLOOR
}

/// Sub-scanlines per pixel row when `antialias` is on.
const SUBSAMPLES: usize = 4;

/// One non-horizontal polygon edge, normalised so `y` runs downward.
struct Edge {
    y_lo: f64,
    y_hi: f64,
    /// x where the edge crosses `y_lo`.
    x_lo: f64,
    /// dx per unit y.
    dxdy: f64,
}

#[inline]
fn to_px(p: (f64, f64), tile: &TileRef) -> (f64, f64) {
    ((p.0 - tile.ul_lon) / tile.scale_x, (tile.ul_lat - p.1) / tile.scale_y)
}

/// Mark every pixel whose **centre** lies in `[xa, xb)`.
fn fill_binary(cov: &mut [f32], x0: u32, x1: u32, xa: f64, xb: f64) {
    let s = (xa - 0.5).ceil().max(x0 as f64);
    let e = (xb - 0.5).ceil().min(x1 as f64);
    if !(e > s) {
        return;
    }
    for x in (s as u32)..(e as u32) {
        cov[(x - x0) as usize] = 1.0;
    }
}

/// Add the span's exact horizontal overlap with each pixel, weighted by `w`.
fn fill_coverage(cov: &mut [f32], x0: u32, x1: u32, xa: f64, xb: f64, w: f32) {
    let a = xa.max(x0 as f64);
    let b = xb.min(x1 as f64);
    if !(b > a) {
        return;
    }
    let sx = a.floor() as u32;
    let ex = (b.ceil() as u32).min(x1);
    for x in sx..ex {
        let l = (x as f64).max(a);
        let r = ((x + 1) as f64).min(b);
        if r > l {
            cov[(x - x0) as usize] += (r - l) as f32 * w;
        }
    }
}

/// Raise *grid* under the forest mask: `z = max(z, ground + h·α)`.
///
/// # The write rule
///
/// Per masked pixel, with coverage α (always 1 for the binary mask):
///
/// ```text
/// z_new = max(z_cur, ground + round(h · α))          [half-metre units]
/// ```
///
/// `max` rather than `+=` for the same reasons as the building rasteriser:
/// overlapping polygons must not stack, and a roof already standing higher than
/// the trees must stay a roof. The addition is *per pixel*, so the canopy drapes
/// over the terrain instead of forming a flat lid the way a roof does — a wood
/// on a hillside rises with the hillside.
///
/// Three kinds of pixel are skipped: those in the observer [`Carveout`], voids
/// (see [`is_void`]), and any whose raise rounds to zero half-metres.
///
/// # Precondition
///
/// `ground` must be **canopy-free terrain**. The rule is idempotent against
/// itself only while that holds: re-running with [`CanopyGround::Current`] on a
/// grid that already carries canopy reads treetops as ground and grows the trees
/// by `h` again. This is the same precondition
/// [`crate::buildings::rasterize_buildings`] documents for above-ground heights,
/// and the web pipeline satisfies it by caching pristine tiles and applying the
/// surface to a copy.
///
/// # Cost
///
/// Scanline, not point-in-polygon: an active-edge table over the polygon's
/// bounding box ∩ tile. Off-tile polygons cost one pass over their vertices.
pub fn rasterize_canopy<G: ElevGrid>(
    grid: &mut G,
    ground: CanopyGround<'_>,
    tile: &TileRef,
    polys: &[CanopyPolygon],
    opts: &CanopyOpts,
) -> CanopyStats {
    let mut stats = CanopyStats::default();
    let size = grid.size_px();
    if size == 0 || polys.is_empty() || !(opts.height_m > 0.0) {
        return stats;
    }
    let sizef = size as f64;

    // Freeze the ground for the whole pass. Without this, two forest polygons
    // covering the same pixel — the normal case at a vector-tile seam — would
    // raise it twice, the second reading the first one's treetops as terrain.
    let owned;
    let base: &[i16] = match ground {
        CanopyGround::Snapshot(s) => s,
        CanopyGround::Current => {
            owned = snapshot_of(grid);
            &owned
        }
    };

    // The observer's pixel, if the carve-out centre is finite in this frame.
    let carve = opts.carveout.and_then(|c| {
        let (px, py) = to_px((c.lon, c.lat), tile);
        (px.is_finite() && py.is_finite())
            .then(|| (px.floor(), py.floor(), c.radius_px as f64))
    });

    let mut edges: Vec<Edge> = Vec::new();
    let mut order: Vec<u32> = Vec::new();
    let mut active: Vec<u32> = Vec::new();
    let mut xs: Vec<f64> = Vec::new();
    let mut cov: Vec<f32> = Vec::new();

    for poly in polys {
        // ── Edge list in pixel space, all rings of the feature together ──
        edges.clear();
        let (mut min_x, mut max_x) = (f64::MAX, f64::MIN);
        let (mut min_y, mut max_y) = (f64::MAX, f64::MIN);

        for ring in &poly.rings {
            if ring.len() < 3 {
                continue;
            }
            let n = ring.len();
            // Close the ring explicitly. A ring that already repeats its first
            // vertex just contributes a zero-length edge, which is dropped.
            let mut prev = to_px(ring[n - 1], tile);
            for &v in ring.iter() {
                let cur = to_px(v, tile);
                min_x = min_x.min(cur.0);
                max_x = max_x.max(cur.0);
                min_y = min_y.min(cur.1);
                max_y = max_y.max(cur.1);
                if prev.1 != cur.1 {
                    let (lo, hi) = if prev.1 < cur.1 { (prev, cur) } else { (cur, prev) };
                    edges.push(Edge {
                        y_lo: lo.1,
                        y_hi: hi.1,
                        x_lo: lo.0,
                        dxdy: (hi.0 - lo.0) / (hi.1 - lo.1),
                    });
                }
                prev = cur;
            }
        }

        if edges.is_empty() {
            continue; // degenerate: no ring with vertical extent
        }
        if !(max_x >= 0.0 && min_x < sizef && max_y >= 0.0 && min_y < sizef) {
            stats.polygons_off_tile += 1;
            continue;
        }

        let x0 = min_x.floor().max(0.0) as u32;
        let x1 = (max_x.ceil().min(sizef) as u32).max(x0);
        let y0 = min_y.floor().max(0.0) as u32;
        let y1 = (max_y.ceil().min(sizef) as u32).max(y0);
        if x1 == x0 || y1 == y0 {
            stats.polygons_off_tile += 1;
            continue;
        }
        let w = (x1 - x0) as usize;

        // ── Active-edge table, so wide polygons stay linear in edges ──
        order.clear();
        order.extend(0..edges.len() as u32);
        order.sort_unstable_by(|a, b| {
            edges[*a as usize]
                .y_lo
                .partial_cmp(&edges[*b as usize].y_lo)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        active.clear();
        let mut next = 0usize;

        cov.clear();
        cov.resize(w, 0.0);

        let mut any = false;
        for y in y0..y1 {
            let row_top = y as f64;
            let row_bot = row_top + 1.0;
            while next < order.len() && edges[order[next] as usize].y_lo < row_bot {
                active.push(order[next]);
                next += 1;
            }
            active.retain(|&e| edges[e as usize].y_hi > row_top);
            if active.is_empty() {
                continue;
            }

            cov[..w].fill(0.0);
            let (n_sub, weight) =
                if opts.antialias { (SUBSAMPLES, 1.0 / SUBSAMPLES as f32) } else { (1, 1.0) };

            for s in 0..n_sub {
                let sy = row_top + (s as f64 + 0.5) / n_sub as f64;
                xs.clear();
                for &e in &active {
                    let e = &edges[e as usize];
                    if e.y_lo <= sy && sy < e.y_hi {
                        xs.push(e.x_lo + (sy - e.y_lo) * e.dxdy);
                    }
                }
                if xs.len() < 2 {
                    continue;
                }
                xs.sort_unstable_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
                // Even-odd: fill between crossing 0-1, 2-3, … across *all*
                // rings of the feature, so holes cancel their outer ring.
                for pair in xs.chunks_exact(2) {
                    if opts.antialias {
                        fill_coverage(&mut cov, x0, x1, pair[0], pair[1], weight);
                    } else {
                        fill_binary(&mut cov, x0, x1, pair[0], pair[1]);
                    }
                }
            }

            // ── Write ──
            for x in x0..x1 {
                let alpha = cov[(x - x0) as usize];
                if alpha <= 0.0 {
                    continue;
                }
                if let Some((ox, oy, r)) = carve {
                    if (x as f64 - ox).abs().max((y as f64 - oy).abs()) <= r {
                        stats.pixels_carved += 1;
                        continue;
                    }
                }
                let Some(cur) = grid.get(x, y) else { continue };
                let Some(&g) = base.get((y as usize) * size as usize + x as usize) else {
                    continue;
                };
                if is_void(g) {
                    continue;
                }
                let delta =
                    Rounding::Nearest.to_half_metres(opts.height_m * (alpha.min(1.0) as f64));
                if delta <= 0 {
                    continue;
                }
                let top = g.saturating_add(delta);
                if top > cur {
                    grid.set(x, y, top);
                    stats.pixels_raised += 1;
                    any = true;
                }
            }
        }
        if any {
            stats.polygons_hit += 1;
        }
    }

    stats
}

// ── Composition: one surface out of buildings and canopy ───────────────────

/// Which layers [`apply_surface_to_abt_tiles`] burns into the terrain.
#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(default)]
pub struct SurfaceOpts {
    pub buildings: bool,
    /// `None` (or JSON `null`/absent) leaves the terrain treeless.
    pub canopy: Option<CanopyOpts>,
}

impl Default for SurfaceOpts {
    fn default() -> Self {
        Self { buildings: true, canopy: None }
    }
}

/// What one tile got.
#[derive(Clone, Copy, Debug, Default)]
pub struct SurfaceTileStats {
    pub buildings_hit: u32,
    pub building_pixels: u32,
    pub canopy_polygons_hit: u32,
    pub canopy_pixels: u32,
    pub canopy_pixels_carved: u32,
}

/// Totals for one [`apply_surface_to_abt_tiles`] batch.
#[derive(Clone, Debug, Default)]
pub struct SurfaceResult {
    /// Always empty: the caller already owns the mutated buffers. Kept so the
    /// shape mirrors `mvt::ApplyBuildingsResult`.
    pub tiles: Vec<Vec<u8>>,
    pub buildings_decoded: usize,
    pub buildings_after_dedup: usize,
    pub canopy_polygons: usize,
    pub canopy_rings: usize,
    pub per_tile: Vec<SurfaceTileStats>,
}

/// Burn buildings **and** canopy into `.abt` tiles from one set of vector tiles.
///
/// # Why one function and not two calls
///
/// The surface that is wanted is
///
/// ```text
/// max(pristine_ground + canopy·α,  roof resolved against pristine_ground)
/// ```
///
/// and neither ordering of two independent passes produces it:
///
/// * **canopy, then buildings** — `rasterize_buildings` reads its ground
///   reference (the P10 of the terrain under the footprint) out of the grid it
///   is about to write. After a canopy pass that grid is treetops, so a house in
///   a wood is placed at *ground + 15 m + 6 m* and pokes out of the forest.
/// * **buildings, then canopy** — the canopy step would read roofs as ground and
///   grow trees out of them.
///
/// So when both layers are on, the pristine grid is snapshotted first, buildings
/// go down while the grid is still bare terrain, and the canopy pass takes its
/// ground from the snapshot. With only canopy requested nothing has been written
/// and [`CanopyGround::Current`] is already pristine — no snapshot needed.
///
/// Each PBF tile is decoded once and both layers are read out of the same
/// `Tile`. With `canopy: None` this is exactly `mvt::apply_buildings_to_abt_tiles`
/// — that function delegates here, so the two cannot drift.
///
/// # Precondition
///
/// `abt_bufs` must hold **bare terrain**: no buildings, no canopy. Both write
/// rules resolve a height against what they find underneath, and neither can be
/// undone once baked. Callers that cache `.abt` tiles must key "with surface"
/// separately from "without".
pub fn apply_surface_to_abt_tiles(
    abt_bufs: &mut [Vec<u8>],
    pbf_tiles: &[Vec<u8>],
    pbf_xs: &[u32],
    pbf_ys: &[u32],
    pbf_zoom: u32,
    opts: &SurfaceOpts,
) -> SurfaceResult {
    // ── 1. Decode every PBF tile once, for both layers ───────────
    let want_canopy = opts.canopy.is_some();
    let mut all_buildings: Vec<BuildingPolygon> = Vec::new();
    let mut all_canopy: Vec<CanopyPolygon> = Vec::new();

    for i in 0..pbf_tiles.len() {
        let pbf_bytes = maybe_gunzip(&pbf_tiles[i]);
        let Ok(vt) = Tile::decode(&pbf_bytes[..]) else { continue };
        if opts.buildings {
            let (b, _) = extract_buildings_from_tile(&vt, pbf_xs[i], pbf_ys[i], pbf_zoom);
            all_buildings.extend(b);
        }
        if want_canopy {
            let (c, _) = extract_canopy_from_tile(&vt, pbf_xs[i], pbf_ys[i], pbf_zoom);
            all_canopy.extend(c);
        }
    }

    let buildings_decoded = all_buildings.len();

    // ── 2. Drop buffer-zone duplicate buildings ──────────────────
    // Canopy gets no equivalent pass on purpose: a forest clipped by two tile
    // buffers arrives as two *different* polygons, so keying on the first
    // vertex would not match them anyway — and `max` makes the overlap a no-op
    // rather than a double raise, which is what the dedup buys for buildings.
    {
        let mut seen = std::collections::HashSet::new();
        all_buildings.retain(|b| {
            if b.coords.is_empty() {
                return false;
            }
            let (lon, lat) = b.coords[0];
            let key = ((lat * 1_000_000.0).round() as i64, (lon * 1_000_000.0).round() as i64);
            seen.insert(key)
        });
    }

    let buildings_after_dedup = all_buildings.len();
    let canopy_rings: usize = all_canopy.iter().map(|p| p.rings.len()).sum();

    // ── 3. Rasterize onto each .abt tile ─────────────────────────
    let model: Vec<Building> = all_buildings.iter().map(|b| b.to_building()).collect();
    let bopts = RasterOpts::default();
    let mut per_tile = Vec::with_capacity(abt_bufs.len());

    for buf in abt_bufs.iter_mut() {
        let header = if model.is_empty() && all_canopy.is_empty() {
            None
        } else {
            parse_abt_header(buf)
        };
        let Some((tile, stride)) = header else {
            per_tile.push(SurfaceTileStats::default());
            continue;
        };
        let size = tile.size_px;

        // The canopy pass needs a *whole* pristine grid, and it sizes that grid
        // from the header. A header claiming more rows than its buffer holds
        // would therefore ask for an allocation the buffer never justified, so
        // such a tile gets no canopy rather than a speculative gigabyte. The
        // building pass is left exactly as it was: it only ever touches pixels
        // through the bounds-checked `AbtGrid`.
        let complete = buf.len() >= 44usize.saturating_add(stride.saturating_mul(size as usize));

        // Pristine terrain, kept only while buildings would otherwise poison
        // the canopy's ground reference.
        let snapshot: Option<Vec<i16>> = if !model.is_empty() && want_canopy && complete {
            Some(snapshot_of(&AbtGrid { buf, size, stride }))
        } else {
            None
        };

        let mut st = SurfaceTileStats::default();

        if !model.is_empty() {
            let mut grid = AbtGrid { buf, size, stride };
            let bs = rasterize_buildings(&mut grid, &tile, &model, &bopts);
            st.buildings_hit = bs.buildings_hit;
            st.building_pixels = bs.pixels_modified;
        }

        if let Some(copts) = opts.canopy.as_ref().filter(|_| complete) {
            let ground = match snapshot.as_deref() {
                Some(s) => CanopyGround::Snapshot(s),
                None => CanopyGround::Current,
            };
            let mut grid = AbtGrid { buf, size, stride };
            let cs = rasterize_canopy(&mut grid, ground, &tile, &all_canopy, copts);
            st.canopy_polygons_hit = cs.polygons_hit;
            st.canopy_pixels = cs.pixels_raised;
            st.canopy_pixels_carved = cs.pixels_carved;
        }

        per_tile.push(st);
    }

    SurfaceResult {
        tiles: Vec::new(),
        buildings_decoded,
        buildings_after_dedup,
        canopy_polygons: all_canopy.len(),
        canopy_rings,
        per_tile,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buildings::I16Grid;
    use crate::download::{tx2lon, ty2lat};
    use crate::mvt::{Layer, Value};

    // ── Synthetic geometry ─────────────────────────────────────────────────

    const SIZE: u32 = 64;
    /// 1/32° per pixel: exactly representable, so a polygon placed on a pixel
    /// boundary really lands on it and the counts below are exact, not ±1.
    const SCALE: f64 = 0.03125;

    fn tile() -> TileRef {
        TileRef { ul_lat: 48.0, ul_lon: 8.0, scale_x: SCALE, scale_y: SCALE, size_px: SIZE }
    }

    /// The lon/lat of pixel-space coordinate *px*, *py* on [`tile`].
    fn at(px: f64, py: f64) -> (f64, f64) {
        (8.0 + px * SCALE, 48.0 - py * SCALE)
    }

    /// An axis-aligned ring spanning pixels `[x0, x1) × [y0, y1)`.
    fn ring(x0: f64, y0: f64, x1: f64, y1: f64) -> Vec<(f64, f64)> {
        vec![at(x0, y0), at(x1, y0), at(x1, y1), at(x0, y1)]
    }

    fn wood(rings: Vec<Vec<(f64, f64)>>) -> CanopyPolygon {
        CanopyPolygon { rings, kind: CanopyKind::Wood }
    }

    /// A 20×20-pixel square of forest at pixels 10..30.
    fn square() -> CanopyPolygon {
        wood(vec![ring(10.0, 10.0, 30.0, 30.0)])
    }

    fn opts(height_m: f64) -> CanopyOpts {
        CanopyOpts { height_m, antialias: false, carveout: None }
    }

    fn flat(value: i16) -> Vec<i16> {
        vec![value; (SIZE * SIZE) as usize]
    }

    // ── The write rule ─────────────────────────────────────────────────────

    #[test]
    fn a_square_of_forest_raises_exactly_its_own_pixels_by_the_canopy_height() {
        let mut buf = flat(200); // 100 m
        let stats = {
            let mut g = I16Grid { buf: &mut buf, size: SIZE };
            rasterize_canopy(&mut g, CanopyGround::Current, &tile(), &[square()], &opts(15.0))
        };

        assert_eq!(stats.polygons_hit, 1);
        assert_eq!(stats.pixels_raised, 400, "20 x 20 pixels, edges on pixel boundaries");
        // Raised by exactly 2 * h half-metres; every other pixel bit-identical.
        assert_eq!(buf.iter().filter(|&&v| v == 230).count(), 400);
        assert_eq!(buf.iter().filter(|&&v| v == 200).count(), (SIZE * SIZE) as usize - 400);
    }

    #[test]
    fn a_clearing_inside_a_wood_stays_clear() {
        // Even-odd across *all* rings of one feature. This is the difference
        // from the building path, which treats every ring as its own footprint
        // and would fill the clearing with trees.
        let poly = wood(vec![ring(10.0, 10.0, 30.0, 30.0), ring(15.0, 15.0, 25.0, 25.0)]);

        let mut buf = flat(200);
        let stats = {
            let mut g = I16Grid { buf: &mut buf, size: SIZE };
            rasterize_canopy(&mut g, CanopyGround::Current, &tile(), &[poly], &opts(15.0))
        };

        assert_eq!(stats.pixels_raised, 400 - 100, "the 10 x 10 hole must stay unraised");
        for y in 15..25u32 {
            for x in 15..25u32 {
                assert_eq!(buf[(y * SIZE + x) as usize], 200, "hole pixel ({x},{y})");
            }
        }
        // …while the ring around it is forest.
        assert_eq!(buf[(12 * SIZE + 12) as usize], 230);
    }

    #[test]
    fn the_observer_keeps_a_patch_of_bare_ground_under_their_feet() {
        // Standing in a wood means standing on the forest floor: the antenna's
        // height AGL is measured from the ground, and the neighbouring trees
        // block immediately.
        let (lon, lat) = at(20.5, 20.5); // centre of pixel (20, 20)
        let o = CanopyOpts {
            height_m: 15.0,
            antialias: false,
            carveout: Some(Carveout { lon, lat, radius_px: 1 }),
        };

        let mut buf = flat(200);
        let stats = {
            let mut g = I16Grid { buf: &mut buf, size: SIZE };
            rasterize_canopy(&mut g, CanopyGround::Current, &tile(), &[square()], &o)
        };

        assert_eq!(stats.pixels_carved, 9, "a radius of 1 keeps 3 x 3 pixels");
        assert_eq!(stats.pixels_raised, 400 - 9);
        for y in 19..=21u32 {
            for x in 19..=21u32 {
                assert_eq!(buf[(y * SIZE + x) as usize], 200, "carved pixel ({x},{y})");
            }
        }
        // The ring immediately around the carve-out is canopy, so the observer
        // is boxed in rather than looking out over the treetops.
        assert_eq!(buf[(18 * SIZE + 20) as usize], 230);
        assert_eq!(buf[(22 * SIZE + 20) as usize], 230);
    }

    #[test]
    fn voids_are_left_as_voids_rather_than_grown_into_trees() {
        let mut buf = flat(200);
        buf[(20 * SIZE + 20) as usize] = VOID_ELEV;
        buf[(21 * SIZE + 20) as usize] = -6000; // below the -5000 no-data floor

        {
            let mut g = I16Grid { buf: &mut buf, size: SIZE };
            rasterize_canopy(&mut g, CanopyGround::Current, &tile(), &[square()], &opts(15.0));
        }

        assert_eq!(buf[(20 * SIZE + 20) as usize], VOID_ELEV);
        assert_eq!(buf[(21 * SIZE + 20) as usize], -6000);
        assert_eq!(buf[(22 * SIZE + 20) as usize], 230, "real ground beside it is raised");
    }

    #[test]
    fn the_canopy_drapes_over_a_slope_instead_of_capping_it() {
        // A roof is flat across sloping terrain (see `buildings.rs`); a canopy
        // is not — it rides the hillside, `ground + h` per pixel.
        let mut buf = vec![0i16; (SIZE * SIZE) as usize];
        for y in 0..SIZE {
            for x in 0..SIZE {
                buf[(y * SIZE + x) as usize] = 200 + x as i16 * 4;
            }
        }
        let terrain = buf.clone();

        {
            let mut g = I16Grid { buf: &mut buf, size: SIZE };
            rasterize_canopy(&mut g, CanopyGround::Current, &tile(), &[square()], &opts(15.0));
        }

        for y in 10..30u32 {
            for x in 10..30u32 {
                let i = (y * SIZE + x) as usize;
                assert_eq!(buf[i], terrain[i] + 30, "pixel ({x},{y}) must follow the slope");
            }
        }
    }

    #[test]
    fn a_polygon_edge_through_the_pixel_centres_is_half_covered() {
        // Anti-aliased: the left column is half inside, so it rises by h/2.
        let poly = wood(vec![ring(10.5, 10.0, 30.5, 30.0)]);
        let o = CanopyOpts { height_m: 15.0, antialias: true, carveout: None };

        let mut buf = flat(200);
        {
            let mut g = I16Grid { buf: &mut buf, size: SIZE };
            rasterize_canopy(&mut g, CanopyGround::Current, &tile(), &[poly], &o);
        }

        for y in 10..30u32 {
            let edge = buf[(y * SIZE + 10) as usize] - 200;
            let full = buf[(y * SIZE + 20) as usize] - 200;
            assert!((12..=18).contains(&edge), "row {y}: half-covered edge got {edge}");
            assert_eq!(full, 30, "row {y}: the interior must be fully covered");
            let right = buf[(y * SIZE + 30) as usize] - 200;
            assert!((12..=18).contains(&right), "row {y}: right edge got {right}");
        }
        assert_eq!(buf[(10 * SIZE + 9) as usize], 200, "outside stays untouched");
    }

    #[test]
    fn a_zero_height_canopy_is_a_no_op() {
        let mut buf = flat(200);
        let stats = {
            let mut g = I16Grid { buf: &mut buf, size: SIZE };
            rasterize_canopy(&mut g, CanopyGround::Current, &tile(), &[square()], &opts(0.0))
        };
        assert_eq!(stats.pixels_raised, 0);
        assert!(buf.iter().all(|&v| v == 200));
    }

    #[test]
    fn polygons_off_the_tile_are_counted_not_drawn() {
        let poly = wood(vec![vec![(50.0, 10.0), (50.5, 10.0), (50.5, 9.5), (50.0, 9.5)]]);
        let mut buf = flat(200);
        let stats = {
            let mut g = I16Grid { buf: &mut buf, size: SIZE };
            rasterize_canopy(&mut g, CanopyGround::Current, &tile(), &[poly], &opts(15.0))
        };
        assert_eq!(stats.polygons_off_tile, 1);
        assert_eq!(stats.pixels_raised, 0);
        assert!(buf.iter().all(|&v| v == 200));
    }

    #[test]
    fn overlapping_woods_do_not_stack() {
        // Vector-tile buffer zones hand the same wood to us more than once,
        // clipped differently each time, so this is the normal case and not an
        // edge case: the ground reference has to stay frozen for the pass.
        let mut buf = flat(200);
        let stats = {
            let mut g = I16Grid { buf: &mut buf, size: SIZE };
            rasterize_canopy(
                &mut g,
                CanopyGround::Current,
                &tile(),
                &[square(), square()],
                &opts(15.0),
            )
        };
        assert_eq!(buf.iter().copied().max().unwrap(), 230, "h once, not twice");
        assert_eq!(stats.pixels_raised, 400, "the second copy must change nothing");
    }

    // ── MVT extraction ─────────────────────────────────────────────────────

    fn zz(n: i32) -> u32 {
        ((n << 1) ^ (n >> 31)) as u32
    }

    /// MVT command stream for one polygon feature: every ring, in order.
    fn poly_geom(rings: &[Vec<(i32, i32)>]) -> Vec<u32> {
        let mut out = Vec::new();
        let (mut cx, mut cy) = (0i32, 0i32);
        for r in rings {
            out.push((1 << 3) | 1);
            out.push(zz(r[0].0 - cx));
            out.push(zz(r[0].1 - cy));
            cx = r[0].0;
            cy = r[0].1;
            out.push((((r.len() - 1) as u32) << 3) | 2);
            for p in &r[1..] {
                out.push(zz(p.0 - cx));
                out.push(zz(p.1 - cy));
                cx = p.0;
                cy = p.1;
            }
            out.push((1 << 3) | 7);
        }
        out
    }

    fn tile_rect(x0: i32, y0: i32, x1: i32, y1: i32) -> Vec<(i32, i32)> {
        vec![(x0, y0), (x1, y0), (x1, y1), (x0, y1)]
    }

    /// A layer of polygon features, each tagged `class = <its class>`.
    fn class_layer(name: &str, features: &[(&str, Vec<Vec<(i32, i32)>>)]) -> Layer {
        let mut values: Vec<Value> = Vec::new();
        let mut feats = Vec::new();
        for (class, rings) in features {
            let vi = values.len() as u32;
            values.push(Value { string_val: Some((*class).into()), ..Default::default() });
            feats.push(Feature {
                id: Some(vi as u64 + 1),
                tags: vec![0, vi],
                r#type: Some(3),
                geometry: poly_geom(rings),
            });
        }
        Layer {
            name: name.into(),
            features: feats,
            keys: vec!["class".into()],
            values,
            extent: Some(4096),
            version: Some(2),
        }
    }

    fn building_layer(features: &[(f64, Vec<Vec<(i32, i32)>>)]) -> Layer {
        let mut values: Vec<Value> = Vec::new();
        let mut feats = Vec::new();
        for (height, rings) in features {
            let vi = values.len() as u32;
            values.push(Value { double_val: Some(*height), ..Default::default() });
            feats.push(Feature {
                id: Some(vi as u64 + 1),
                tags: vec![0, vi],
                r#type: Some(3),
                geometry: poly_geom(rings),
            });
        }
        Layer {
            name: "building".into(),
            features: feats,
            keys: vec!["render_height".into()],
            values,
            extent: Some(4096),
            version: Some(2),
        }
    }

    #[test]
    fn only_wood_and_managed_forest_classes_become_canopy() {
        let vt = Tile {
            layers: vec![
                class_layer(
                    "landcover",
                    &[
                        ("wood", vec![tile_rect(0, 0, 1000, 1000)]),
                        ("grass", vec![tile_rect(2000, 0, 3000, 1000)]),
                        ("ice", vec![tile_rect(3000, 0, 4000, 1000)]),
                    ],
                ),
                class_layer(
                    "park",
                    &[
                        ("national_forest", vec![tile_rect(0, 2000, 1000, 3000)]),
                        ("state_forest", vec![tile_rect(1000, 2000, 2000, 3000)]),
                        ("forest", vec![tile_rect(2000, 2000, 3000, 3000)]),
                        ("public_park", vec![tile_rect(3000, 2000, 4000, 3000)]),
                    ],
                ),
                class_layer("water", &[("ocean", vec![tile_rect(0, 3000, 4000, 4000)])]),
            ],
        };

        let (polys, stats) = extract_canopy_from_tile(&vt, 8531, 5752, 14);
        assert_eq!(stats.features_wood, 1);
        assert_eq!(stats.features_managed_forest, 3);
        assert_eq!(stats.features_other_class, 3, "grass, ice, public_park");
        assert_eq!(polys.len(), 4);
        assert_eq!(polys[0].kind, CanopyKind::Wood);
        assert!(polys[1..].iter().all(|p| p.kind == CanopyKind::ManagedForest));
        assert!(stats.landcover_layer_found && stats.park_layer_found);
    }

    #[test]
    fn a_multipolygon_feature_keeps_all_its_rings_together() {
        // The whole point of `CanopyPolygon`: the outer ring and its holes must
        // reach the rasteriser as one feature, or the even-odd fill cannot
        // cancel them.
        let vt = Tile {
            layers: vec![class_layer(
                "landcover",
                &[("wood", vec![tile_rect(0, 0, 2000, 2000), tile_rect(500, 500, 1500, 1500)])],
            )],
        };
        let bytes = prost::Message::encode_to_vec(&vt);
        let (polys, stats) = extract_canopy_from_pbf(&bytes, 8531, 5752, 14).unwrap();
        assert_eq!(polys.len(), 1);
        assert_eq!(polys[0].rings.len(), 2);
        assert_eq!(stats.rings_out, 2);
    }

    // ── Composition with buildings ─────────────────────────────────────────

    fn tile_ref_for(z: u32, x: u32, y: u32, size: u32) -> TileRef {
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

    /// An `.abt` buffer covering exactly XYZ tile *(z, x, y)*, filled flat.
    fn abt_for(z: u32, x: u32, y: u32, size: u32, fill: i16) -> Vec<u8> {
        let t = tile_ref_for(z, x, y, size);
        let stride = (size as usize * 2 + 255) & !255;
        let mut buf = vec![0u8; 44 + stride * size as usize];
        buf[0..4].copy_from_slice(b"AETH");
        buf[4..6].copy_from_slice(&1u16.to_le_bytes());
        buf[6..8].copy_from_slice(&(size as u16).to_le_bytes());
        buf[8..16].copy_from_slice(&t.ul_lat.to_le_bytes());
        buf[16..24].copy_from_slice(&t.ul_lon.to_le_bytes());
        buf[24..32].copy_from_slice(&t.scale_y.to_le_bytes());
        buf[32..40].copy_from_slice(&t.scale_x.to_le_bytes());
        buf[40..42].copy_from_slice(&0i16.to_le_bytes());
        buf[42..44].copy_from_slice(&(stride as u16).to_le_bytes());
        for yy in 0..size as usize {
            for xx in 0..size as usize {
                let off = 44 + yy * stride + xx * 2;
                buf[off..off + 2].copy_from_slice(&fill.to_le_bytes());
            }
        }
        buf
    }

    fn abt_get(buf: &[u8], x: u32, y: u32) -> i16 {
        let stride = u16::from_le_bytes([buf[42], buf[43]]) as usize;
        let off = 44 + y as usize * stride + x as usize * 2;
        i16::from_le_bytes([buf[off], buf[off + 1]])
    }

    /// One vector tile with a wood, a house inside it and a house outside it.
    ///
    /// Tile-space 0..4096 maps to 0..64 px, so `/64` converts: the wood covers
    /// px 16..48, the inner house px 24..28 and the outer house px 56..60.
    fn forest_and_houses(inner_height: f64) -> Vec<u8> {
        let vt = Tile {
            layers: vec![
                class_layer("landcover", &[("wood", vec![tile_rect(1024, 1024, 3072, 3072)])]),
                building_layer(&[
                    (inner_height, vec![tile_rect(1536, 1536, 1792, 1792)]),
                    (6.0, vec![tile_rect(3584, 3584, 3840, 3840)]),
                ]),
            ],
        };
        prost::Message::encode_to_vec(&vt)
    }

    /// (wood only, wood + house, house only, bare ground)
    fn probe(buf: &[u8]) -> (i16, i16, i16, i16) {
        (abt_get(buf, 40, 40), abt_get(buf, 26, 26), abt_get(buf, 58, 58), abt_get(buf, 4, 4))
    }

    #[test]
    fn a_house_in_a_wood_is_measured_from_the_ground_not_from_the_treetops() {
        // Ground 100 m (200 half-metres), canopy 15 m, house 6 m.
        // Right: max(ground + 15, ground + 6) = ground + 15.
        // Wrong (canopy first, then buildings): ground + 15 + 6 = ground + 21.
        let mut bufs = vec![abt_for(14, 8531, 5752, 64, 200)];
        let pbf = vec![forest_and_houses(6.0)];
        let o = SurfaceOpts { buildings: true, canopy: Some(opts(15.0)) };
        let r = apply_surface_to_abt_tiles(&mut bufs, &pbf, &[8531], &[5752], 14, &o);

        assert_eq!(r.canopy_polygons, 1);
        assert_eq!(r.buildings_after_dedup, 2);
        let (wood_only, wood_house, house_only, bare) = probe(&bufs[0]);
        assert_eq!(wood_only, 230, "wood on 100 m ground");
        assert_eq!(wood_house, 230, "a 6 m house under a 15 m canopy stays hidden");
        assert_eq!(house_only, 212, "a house outside the wood is ground + 6 m");
        assert_eq!(bare, 200);
        assert!(r.per_tile[0].buildings_hit > 0 && r.per_tile[0].canopy_polygons_hit == 1);
    }

    #[test]
    fn a_tower_in_a_wood_still_stands_above_the_canopy() {
        let mut bufs = vec![abt_for(14, 8531, 5752, 64, 200)];
        let pbf = vec![forest_and_houses(25.0)];
        let o = SurfaceOpts { buildings: true, canopy: Some(opts(15.0)) };
        apply_surface_to_abt_tiles(&mut bufs, &pbf, &[8531], &[5752], 14, &o);

        let (wood_only, wood_house, house_only, _) = probe(&bufs[0]);
        assert_eq!(wood_only, 230);
        assert_eq!(wood_house, 250, "25 m building beats a 15 m canopy");
        assert_eq!(house_only, 212);
    }

    #[test]
    fn canopy_without_buildings_leaves_the_houses_alone() {
        let mut bufs = vec![abt_for(14, 8531, 5752, 64, 200)];
        let pbf = vec![forest_and_houses(6.0)];
        let o = SurfaceOpts { buildings: false, canopy: Some(opts(15.0)) };
        let r = apply_surface_to_abt_tiles(&mut bufs, &pbf, &[8531], &[5752], 14, &o);

        assert_eq!(r.buildings_decoded, 0, "the building layer must not even be decoded");
        let (wood_only, wood_house, house_only, bare) = probe(&bufs[0]);
        assert_eq!(wood_only, 230);
        assert_eq!(wood_house, 230);
        assert_eq!(house_only, 200);
        assert_eq!(bare, 200);
    }

    #[test]
    fn the_surface_function_with_no_canopy_is_the_buildings_function() {
        // `mvt::apply_buildings_to_abt_tiles` delegates here, so this pins that
        // the delegation did not change a byte of the PBF building path.
        let pbf = vec![forest_and_houses(6.0)];
        let mut via_surface = vec![abt_for(14, 8531, 5752, 64, 200)];
        let mut via_buildings = via_surface.clone();

        let o = SurfaceOpts { buildings: true, canopy: None };
        let a = apply_surface_to_abt_tiles(&mut via_surface, &pbf, &[8531], &[5752], 14, &o);
        let b = crate::mvt::apply_buildings_to_abt_tiles(
            &mut via_buildings,
            &pbf,
            &[8531],
            &[5752],
            14,
        );

        assert_eq!(via_surface, via_buildings);
        assert_eq!(a.buildings_decoded, b.buildings_decoded);
        assert_eq!(a.buildings_after_dedup, b.buildings_after_dedup);
        assert_eq!(b.per_tile, vec![(a.per_tile[0].buildings_hit, a.per_tile[0].building_pixels)]);
        assert_eq!(abt_get(&via_surface[0], 26, 26), 212, "the house is still drawn");
        assert_eq!(abt_get(&via_surface[0], 40, 40), 200, "and the wood is not");
    }

    #[test]
    fn a_tile_whose_header_over_claims_gets_no_canopy() {
        // The canopy pass sizes its pristine snapshot from the header, so a
        // header promising more rows than the buffer carries must not be taken
        // at its word.
        let mut truncated = abt_for(14, 8531, 5752, 64, 200);
        truncated.truncate(truncated.len() - 1024);
        let before = truncated.clone();

        let mut bufs = vec![truncated];
        let pbf = vec![forest_and_houses(6.0)];
        let o = SurfaceOpts { buildings: false, canopy: Some(opts(15.0)) };
        let r = apply_surface_to_abt_tiles(&mut bufs, &pbf, &[8531], &[5752], 14, &o);

        assert_eq!(bufs[0], before, "left alone rather than snapshotted");
        assert_eq!(r.per_tile[0].canopy_pixels, 0);
    }

    #[test]
    fn a_tile_that_is_not_an_abt_is_left_alone() {
        let mut bufs = vec![vec![0u8; 10]];
        let pbf = vec![forest_and_houses(6.0)];
        let o = SurfaceOpts { buildings: true, canopy: Some(opts(15.0)) };
        let r = apply_surface_to_abt_tiles(&mut bufs, &pbf, &[8531], &[5752], 14, &o);
        assert_eq!(bufs[0], vec![0u8; 10]);
        assert_eq!(r.per_tile.len(), 1);
        assert_eq!(r.per_tile[0].canopy_pixels, 0);
    }

    // ── Options ────────────────────────────────────────────────────────────

    #[test]
    fn surface_options_parse_from_the_wasm_json() {
        let o: SurfaceOpts = serde_json::from_str(
            r#"{"buildings": true, "canopy": {"height_m": 15, "antialias": false,
                "carveout": {"lon": 10.09, "lat": 46.7, "radius_px": 1}}, "future": 3}"#,
        )
        .unwrap();
        assert!(o.buildings);
        let c = o.canopy.unwrap();
        assert_eq!(c.height_m, 15.0);
        assert!(!c.antialias);
        let cv = c.carveout.unwrap();
        assert_eq!((cv.lon, cv.lat, cv.radius_px), (10.09, 46.7, 1));

        // An absent or null canopy means buildings only.
        let o: SurfaceOpts = serde_json::from_str(r#"{"buildings": true}"#).unwrap();
        assert!(o.canopy.is_none());
        let o: SurfaceOpts = serde_json::from_str(r#"{"canopy": null}"#).unwrap();
        assert!(o.buildings, "buildings default to on");
        assert!(o.canopy.is_none());

        // A canopy with no height is a visible no-op, not a guessed height.
        let o: SurfaceOpts = serde_json::from_str(r#"{"canopy": {}}"#).unwrap();
        let c = o.canopy.unwrap();
        assert_eq!(c.height_m, 0.0);
        assert_eq!(c.carveout.map(|c| c.radius_px), None);

        // A carve-out with no radius keeps the observer's own pixel clear.
        let o: SurfaceOpts =
            serde_json::from_str(r#"{"canopy": {"height_m": 12, "carveout": {"lon": 1}}}"#)
                .unwrap();
        assert_eq!(o.canopy.unwrap().carveout.unwrap().radius_px, 1);
    }

    // ── Real data ──────────────────────────────────────────────────────────

    #[test]
    fn the_zernez_tile_matches_the_javascript_reference() {
        // OpenFreeMap z14 8651/5782 over Zernez (CH). The browser-side
        // reference (`web/src/lib/utils/canopy-dem.ts`) fills the same two MVT
        // layers into a canvas and raises 35.1 % of this tile's pixels — every
        // pixel the anti-aliased fill *touches*, not just the ones whose centre
        // is inside. Both readings are checked here, because only the second is
        // comparable to that number.
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/ofm_z14_8651_5782.pbf");
        let raw = std::fs::read(path).expect("fixture ofm_z14_8651_5782.pbf");
        let bytes = maybe_gunzip(&raw);
        let (polys, stats) = extract_canopy_from_pbf(&bytes, 8651, 5782, 14).unwrap();
        assert!(stats.features_wood > 0, "the fixture must contain mapped wood");

        let size = 256u32;
        let t = tile_ref_for(14, 8651, 5782, size);
        let cover = |antialias: bool| {
            let mut buf = vec![2000i16; (size * size) as usize];
            {
                let mut g = I16Grid { buf: &mut buf, size };
                let o = CanopyOpts { height_m: 15.0, antialias, carveout: None };
                rasterize_canopy(&mut g, CanopyGround::Current, &t, &polys, &o);
            }
            let touched = buf.iter().filter(|&&v| v != 2000).count();
            (touched as f64 / (size * size) as f64, buf)
        };

        let (binary, buf) = cover(false);
        assert!(
            (0.33..0.37).contains(&binary),
            "expected ~1/3 of the tile under canopy, got {:.1} % ({} polygons)",
            binary * 100.0,
            polys.len()
        );
        assert!(
            buf.iter().all(|&v| v == 2000 || v == 2030),
            "the binary mask raises by 0 or h, nothing in between"
        );

        let (feathered, _) = cover(true);
        assert!(
            (0.345..0.355).contains(&feathered),
            "the anti-aliased mask must reproduce the JS reference's 35.1 %, got {:.2} %",
            feathered * 100.0
        );
    }
}
