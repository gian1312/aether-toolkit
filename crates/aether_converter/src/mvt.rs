// MVT (Mapbox Vector Tile) decoder for building extraction.
//
// Decodes PBF-encoded vector tiles and extracts building polygons with
// geographic coordinates + heights.  Used by both the native converter
// (for PBF tile files) and the WASM pipeline (for OpenFreeMap tiles).

use anyhow::{anyhow, Result};
use prost::Message;

use crate::buildings::{
    drop_duplicate_buildings, parse_leading_metres, resolve_building_height, Building,
    BuildingHeight, DuplicateKey, HeightSource,
};

// ── Protobuf message definitions (MVT spec v2.1) ──────────────

#[derive(Clone, Message)]
pub struct Tile {
    #[prost(message, repeated, tag = "3")]
    pub layers: Vec<Layer>,
}

#[derive(Clone, Message)]
pub struct Layer {
    #[prost(string, tag = "1")]
    pub name: String,
    #[prost(message, repeated, tag = "2")]
    pub features: Vec<Feature>,
    #[prost(string, repeated, tag = "3")]
    pub keys: Vec<String>,
    #[prost(message, repeated, tag = "4")]
    pub values: Vec<Value>,
    #[prost(uint32, optional, tag = "5")]
    pub extent: Option<u32>,
    #[prost(uint32, optional, tag = "15")]
    pub version: Option<u32>,
}

#[derive(Clone, Message)]
pub struct Feature {
    #[prost(uint64, optional, tag = "1")]
    pub id: Option<u64>,
    #[prost(uint32, repeated, packed, tag = "2")]
    pub tags: Vec<u32>,
    #[prost(enumeration = "GeomType", optional, tag = "3")]
    pub r#type: Option<i32>,
    #[prost(uint32, repeated, packed, tag = "4")]
    pub geometry: Vec<u32>,
}

#[derive(Clone, Message)]
pub struct Value {
    #[prost(string, optional, tag = "1")]
    pub string_val: Option<String>,
    #[prost(float, optional, tag = "2")]
    pub float_val: Option<f32>,
    #[prost(double, optional, tag = "3")]
    pub double_val: Option<f64>,
    #[prost(int64, optional, tag = "4")]
    pub int_val: Option<i64>,
    #[prost(uint64, optional, tag = "5")]
    pub uint_val: Option<u64>,
    #[prost(sint64, optional, tag = "6")]
    pub sint_val: Option<i64>,
    #[prost(bool, optional, tag = "7")]
    pub bool_val: Option<bool>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[repr(i32)]
pub enum GeomType {
    #[default]
    Unknown = 0,
    Point = 1,
    LineString = 2,
    Polygon = 3,
}
impl GeomType {
    pub fn from_i32(v: i32) -> Self {
        match v { 1 => Self::Point, 2 => Self::LineString, 3 => Self::Polygon, _ => Self::Unknown }
    }
}
impl TryFrom<i32> for GeomType {
    type Error = i32;
    fn try_from(v: i32) -> Result<Self, i32> {
        match v { 0 => Ok(Self::Unknown), 1 => Ok(Self::Point), 2 => Ok(Self::LineString), 3 => Ok(Self::Polygon), _ => Err(v) }
    }
}

// ── Extracted building ─────────────────────────────────────────

pub struct BuildingPolygon {
    /// Exterior ring as (lon, lat) pairs in WGS84.
    pub coords: Vec<(f64, f64)>,
    /// The exterior's interior rings (courtyards), as (lon, lat) pairs.
    pub holes: Vec<Vec<(f64, f64)>>,
    /// Building height above ground in meters.
    pub height_m: f64,
    /// Which rung of the height ladder `height_m` came from.
    pub source: HeightSource,
}

impl BuildingPolygon {
    /// Convert into the shared building model.
    ///
    /// Vector tiles always carry a height *above ground*, so the rasterizer has
    /// to resolve it against the terrain under the footprint before writing.
    pub fn to_building(&self) -> Building {
        Building {
            coords: self.coords.clone(),
            holes: self.holes.clone(),
            height: BuildingHeight::AboveGround(self.height_m),
            source: self.source,
        }
    }

    /// This building's identity for the buffer-zone duplicate filter — the
    /// same key [`Building::duplicate_key`] gives its converted self.
    pub(crate) fn duplicate_key(&self) -> Option<DuplicateKey> {
        DuplicateKey::new(&self.coords, &self.holes, BuildingHeight::AboveGround(self.height_m))
    }
}

/// Diagnostic stats from PBF building extraction.
#[derive(Default)]
pub struct ExtractStats {
    pub layers_total: usize,
    pub building_layer_found: bool,
    pub features_total: usize,
    pub features_polygon: usize,
    pub features_other_type: usize,
    pub features_empty_geom: usize,
    pub features_no_rings: usize,
    pub features_short_ring: usize,
    /// Interior rings attached to their exterior as holes (courtyards) —
    /// before 0.2.9 each of these was drawn as a building of its own.
    pub rings_interior: usize,
    /// Rings of zero area in tile coordinates, skipped as degenerate.
    pub rings_degenerate: usize,
    /// Buildings out: one per exterior ring, holes included.
    pub buildings_out: usize,
    pub height_min: f64,
    pub height_max: f64,
    pub height_sum: f64,
    pub height_default_count: usize,
    pub height_explicit_count: usize,
}

// ── Tile coordinate conversion ─────────────────────────────────

fn tile_to_lon(x: u32, z: u32) -> f64 {
    x as f64 / (1u64 << z) as f64 * 360.0 - 180.0
}
fn tile_to_lat(y: u32, z: u32) -> f64 {
    let n = std::f64::consts::PI - 2.0 * std::f64::consts::PI * y as f64 / (1u64 << z) as f64;
    (0.5 * (n.exp() - (-n).exp())).atan().to_degrees()
}

// ── MVT geometry decoding ──────────────────────────────────────
// Commands: MoveTo=1, LineTo=2, ClosePath=7
// Each command word: (id & 0x7) = command, (id >> 3) = count
// Coordinates are zigzag-encoded deltas in tile-local space (0..extent).

pub(crate) fn decode_geometry(cmds: &[u32], extent: u32, tile_x: u32, tile_y: u32, z: u32) -> Vec<Vec<(f64, f64)>> {
    decode_rings(cmds, extent, tile_x, tile_y, z).into_iter().map(|r| r.coords).collect()
}

/// One decoded ring: its WGS84 vertices and its winding.
pub(crate) struct DecodedRing {
    /// Vertices as (lon, lat); the closing vertex is not repeated.
    pub coords: Vec<(f64, f64)>,
    /// Twice the signed area by the surveyor's formula over the ring's
    /// **integer tile coordinates**, closing edge included — exact, and the
    /// quantity MVT spec 2.1 §4.3.4.4 defines winding by: positive (clockwise
    /// with y down) is an exterior ring, negative an interior ring, zero
    /// degenerate.
    pub area2: i128,
}

/// [`decode_geometry`], keeping each ring's winding.
pub(crate) fn decode_rings(cmds: &[u32], extent: u32, tile_x: u32, tile_y: u32, z: u32) -> Vec<DecodedRing> {
    let tile_w = tile_to_lon(tile_x + 1, z) - tile_to_lon(tile_x, z);
    let tile_n = tile_to_lat(tile_y, z);
    let tile_s = tile_to_lat(tile_y + 1, z);
    let tile_origin_lon = tile_to_lon(tile_x, z);
    let ext = extent as f64;

    let mut rings: Vec<DecodedRing> = Vec::new();
    let mut ring: Vec<(f64, f64)> = Vec::new();
    // The same vertices in tile coordinates, for an exact winding.
    let mut ring_px: Vec<(i64, i64)> = Vec::new();
    let mut cx: i32 = 0;
    let mut cy: i32 = 0;
    let mut i = 0;

    // A ring of three or more vertices is kept, anything shorter dropped —
    // on MoveTo, on ClosePath and at the end of the stream, as before.
    let finish = |rings: &mut Vec<DecodedRing>, ring: &mut Vec<(f64, f64)>, px: &mut Vec<(i64, i64)>| {
        if ring.len() >= 3 {
            let n = px.len();
            let area2 = (0..n)
                .map(|k| {
                    let (x0, y0) = px[k];
                    let (x1, y1) = px[(k + 1) % n];
                    (x0 as i128) * (y1 as i128) - (x1 as i128) * (y0 as i128)
                })
                .sum();
            rings.push(DecodedRing { coords: std::mem::take(ring), area2 });
        } else {
            ring.clear();
        }
        px.clear();
    };

    while i < cmds.len() {
        let cmd_int = cmds[i];
        let cmd_id = cmd_int & 0x7;
        let cmd_count = (cmd_int >> 3) as usize;
        i += 1;

        match cmd_id {
            1 => {
                // MoveTo — start new ring
                finish(&mut rings, &mut ring, &mut ring_px);

                for _ in 0..cmd_count {
                    if i + 1 >= cmds.len() { break; }
                    let dx = zigzag(cmds[i]);
                    let dy = zigzag(cmds[i + 1]);
                    cx += dx;
                    cy += dy;
                    i += 2;

                    let lon = tile_origin_lon + (cx as f64 / ext) * tile_w;
                    // Latitude: linear interpolation in tile space between N and S edges
                    let lat = tile_n + (cy as f64 / ext) * (tile_s - tile_n);
                    ring.push((lon, lat));
                    ring_px.push((cx as i64, cy as i64));
                }
            }
            2 => {
                // LineTo
                for _ in 0..cmd_count {
                    if i + 1 >= cmds.len() { break; }
                    let dx = zigzag(cmds[i]);
                    let dy = zigzag(cmds[i + 1]);
                    cx += dx;
                    cy += dy;
                    i += 2;

                    let lon = tile_origin_lon + (cx as f64 / ext) * tile_w;
                    let lat = tile_n + (cy as f64 / ext) * (tile_s - tile_n);
                    ring.push((lon, lat));
                    ring_px.push((cx as i64, cy as i64));
                }
            }
            7 => {
                // ClosePath — close current ring
                finish(&mut rings, &mut ring, &mut ring_px);
            }
            _ => { i += cmd_count * 2; } // skip unknown
        }
    }
    finish(&mut rings, &mut ring, &mut ring_px);

    rings
}

#[inline]
fn zigzag(n: u32) -> i32 {
    ((n >> 1) as i32) ^ -((n & 1) as i32)
}

// ── Building extraction ────────────────────────────────────────

/// The numeric value of the tag named *key* on *feature*, whatever protobuf
/// wire type it arrived in.
///
/// A string value goes through [`parse_leading_metres`], so `"12 m"` reads the
/// same off an MVT tag as it does off a FlatGeobuf attribute column. The sign
/// is preserved: rejecting non-positive values is the ladder's job, not this
/// function's, so that a `render_height` of `0` falls through to the next rung
/// instead of being mistaken for an absent tag.
fn tag_number(feature: &Feature, layer: &Layer, key: &str) -> Option<f64> {
    let mut i = 0;
    while i + 1 < feature.tags.len() {
        let key_idx = feature.tags[i] as usize;
        let val_idx = feature.tags[i + 1] as usize;
        i += 2;

        if key_idx >= layer.keys.len() || val_idx >= layer.values.len() { continue; }
        if layer.keys[key_idx] != key { continue; }

        let val = &layer.values[val_idx];
        let n = val
            .float_val
            .map(f64::from)
            .or(val.double_val)
            .or(val.int_val.map(|v| v as f64))
            .or(val.uint_val.map(|v| v as f64))
            .or(val.sint_val.map(|v| v as f64))
            .or_else(|| val.string_val.as_deref().and_then(parse_leading_metres));
        if n.is_some() {
            return n;
        }
    }
    None
}

/// This feature's height above ground, and which rung of the ladder it is from.
///
/// The ladder itself lives in [`crate::buildings::resolve_building_height`] and
/// is shared with the FlatGeobuf reader; all this does is tell it how to read a
/// tag off an MVT feature. Vector tiles carry no geometry Z, so the top rung
/// (an absolute roof) never applies here — every value out of this is metres
/// **above ground**, which is why [`BuildingPolygon::to_building`] can wrap it
/// in [`BuildingHeight::AboveGround`] unconditionally.
fn resolve_height_detailed(feature: &Feature, layer: &Layer) -> (f64, HeightSource) {
    let (height, source) =
        resolve_building_height(None, |key| tag_number(feature, layer, key));
    (height.metres(), source)
}

/// Count building features using raw protobuf wire parsing (no prost).
/// For diagnostic comparison with prost-based extraction.
pub fn count_buildings_raw(pbf_data: &[u8]) -> Result<(usize, Vec<String>)> {
    // Minimal protobuf wire-format parser
    fn read_varint(data: &[u8], pos: &mut usize) -> u64 {
        let mut result: u64 = 0;
        let mut shift = 0;
        while *pos < data.len() {
            let b = data[*pos];
            *pos += 1;
            result |= ((b & 0x7f) as u64) << shift;
            if b & 0x80 == 0 { break; }
            shift += 7;
        }
        result
    }

    // Parse top-level Tile: find Layer messages (tag 3)
    let mut layers_info = Vec::new();
    let mut building_features = 0usize;
    let mut pos = 0;
    while pos < pbf_data.len() {
        let tag = read_varint(pbf_data, &mut pos);
        let field = tag >> 3;
        let wtype = tag & 7;
        match wtype {
            0 => { read_varint(pbf_data, &mut pos); }
            2 => {
                let len = read_varint(pbf_data, &mut pos) as usize;
                let end = pos + len;
                if field == 3 {
                    // Layer message — parse to find name and feature count
                    let mut lpos = pos;
                    let mut name = String::new();
                    let mut feat_count = 0usize;
                    while lpos < end {
                        let ltag = read_varint(pbf_data, &mut lpos);
                        let lfield = ltag >> 3;
                        let lwtype = ltag & 7;
                        match lwtype {
                            0 => { read_varint(pbf_data, &mut lpos); }
                            2 => {
                                let llen = read_varint(pbf_data, &mut lpos) as usize;
                                if lfield == 1 {
                                    name = String::from_utf8_lossy(&pbf_data[lpos..lpos+llen]).to_string();
                                }
                                if lfield == 2 { feat_count += 1; }
                                lpos += llen;
                            }
                            5 => { lpos += 4; }
                            1 => { lpos += 8; }
                            _ => break,
                        }
                    }
                    if name == "building" {
                        building_features += feat_count;
                    }
                    layers_info.push(format!("{}:{}", name, feat_count));
                }
                pos = end;
            }
            5 => { pos += 4; }
            1 => { pos += 8; }
            _ => break,
        }
    }
    Ok((building_features, layers_info))
}

/// Decode a PBF vector tile and extract building polygons with heights.
///
/// Returns building polygons in WGS84 coordinates with height above ground,
/// plus diagnostic stats for debugging extraction yield.
/// `tile_x`, `tile_y`, `z` are the XYZ tile coordinates for geo-referencing.
pub fn extract_buildings_from_pbf(
    pbf_data: &[u8],
    tile_x: u32,
    tile_y: u32,
    z: u32,
) -> Result<(Vec<BuildingPolygon>, ExtractStats)> {
    let tile = Tile::decode(pbf_data).map_err(|e| anyhow!("PBF decode error: {e}"))?;
    Ok(extract_buildings_from_tile(&tile, tile_x, tile_y, z))
}

/// Extract buildings from an already-decoded vector tile.
///
/// Split out of [`extract_buildings_from_pbf`] so a pipeline that also wants the
/// canopy mask (`crate::canopy::extract_canopy_from_tile`) can decode the
/// protobuf once and read both layers out of the same `Tile`.
pub fn extract_buildings_from_tile(
    tile: &Tile,
    tile_x: u32,
    tile_y: u32,
    z: u32,
) -> (Vec<BuildingPolygon>, ExtractStats) {
    let mut buildings = Vec::new();
    let mut stats = ExtractStats {
        layers_total: tile.layers.len(),
        height_min: f64::MAX,
        height_max: f64::MIN,
        ..Default::default()
    };

    for layer in &tile.layers {
        if layer.name != "building" { continue; }
        stats.building_layer_found = true;

        let extent = layer.extent.unwrap_or(4096);
        stats.features_total += layer.features.len();

        for feature in &layer.features {
            let geom_type = feature.r#type.map(GeomType::from_i32).unwrap_or(GeomType::Unknown);

            if feature.geometry.is_empty() {
                stats.features_empty_geom += 1;
                continue;
            }

            if geom_type != GeomType::Polygon {
                stats.features_other_type += 1;
                continue;
            }
            stats.features_polygon += 1;

            let (height, height_source) = resolve_height_detailed(feature, layer);
            let rings = decode_rings(&feature.geometry, extent, tile_x, tile_y, z);

            // A feature can be a multipolygon — Planetiler merges buildings into
            // single features — and a polygon can have courtyards. MVT spec 2.1
            // §4.3.4.4: every exterior ring begins a new polygon and its interior
            // rings follow it, and winding tells them apart (exterior: positive
            // area in tile coordinates, clockwise with y down). Each exterior is
            // one building; each interior ring is a hole of the exterior before
            // it, never a building of its own — drawing it as one raised every
            // courtyard to its building's roof. Classified the way the reference
            // decoder (vector-tile-js / MapLibre `classifyRings`) does it: the
            // first ring's winding is the exterior winding, which on a
            // spec-conformant tile is the positive one, and a tile written with
            // the opposite convention still keeps its courtyards with their
            // buildings.
            if rings.is_empty() {
                stats.features_no_rings += 1;
            } else {
                if height_source == HeightSource::Default {
                    stats.height_default_count += 1;
                } else {
                    stats.height_explicit_count += 1;
                }
                let mut exterior_positive: Option<bool> = None;
                let first_out = buildings.len();
                for ring in rings {
                    if ring.coords.len() < 3 {
                        stats.features_short_ring += 1;
                        continue;
                    }
                    if ring.area2 == 0 {
                        stats.rings_degenerate += 1;
                        continue;
                    }
                    let positive = ring.area2 > 0;
                    if *exterior_positive.get_or_insert(positive) == positive {
                        stats.height_sum += height;
                        if height < stats.height_min { stats.height_min = height; }
                        if height > stats.height_max { stats.height_max = height; }
                        buildings.push(BuildingPolygon {
                            coords: ring.coords,
                            holes: Vec::new(),
                            height_m: height,
                            source: height_source,
                        });
                    } else if let Some(owner) = buildings[first_out..].last_mut() {
                        // Always taken: the first kept ring set the exterior
                        // winding, so this feature has an exterior by now.
                        owner.holes.push(ring.coords);
                        stats.rings_interior += 1;
                    }
                }
                if buildings.len() == first_out { stats.features_short_ring += 1; }
            }
        }
    }

    stats.buildings_out = buildings.len();
    (buildings, stats)
}

// ── Batch building application to .abt tiles ──────────────────────

use crate::canopy::{apply_surface_to_abt_tiles, SurfaceOpts};

/// Result of applying buildings to a set of .abt tiles.
pub struct ApplyBuildingsResult {
    /// Modified .abt tile buffers (same order as input).
    pub tiles: Vec<Vec<u8>>,
    /// Total buildings decoded (before dedup).
    pub buildings_decoded: usize,
    /// Buildings after dedup.
    pub buildings_after_dedup: usize,
    /// Per-tile: (buildings_hit, pixels_modified).
    pub per_tile: Vec<(u32, u32)>,
}

/// Decompress gzip if magic bytes present, otherwise return as-is.
pub fn maybe_gunzip(data: &[u8]) -> Vec<u8> {
    if data.len() >= 2 && data[0] == 0x1f && data[1] == 0x8b {
        use std::io::Read;
        let mut decoder = flate2::read::GzDecoder::new(data);
        let mut decompressed = Vec::new();
        if decoder.read_to_end(&mut decompressed).is_ok() {
            return decompressed;
        }
    }
    data.to_vec()
}

/// Decode PBF building tiles, deduplicate, and rasterize onto .abt tile buffers.
///
/// This is the core building integration pipeline. The WASM wrapper calls this
/// after converting JS types to Rust types.
///
/// The write rule lives in `buildings::rasterize_buildings`, shared with the
/// FlatGeobuf path: resolve each above-ground height against the terrain under
/// its own footprint, then composite the resulting absolute roof with `max`.
/// This replaced a per-pixel `+= height`, which draped roofs over slopes,
/// stacked overlapping footprints and doubled on re-application.
///
/// Buildings are one of two layers that can be burned into a tile; the body is
/// [`crate::canopy::apply_surface_to_abt_tiles`] with the canopy switched off,
/// so "buildings only" cannot drift away from "buildings and canopy".
///
/// * `abt_bufs` — mutable .abt tile buffers (44-byte header + i16 elevation data)
/// * `pbf_tiles` — raw PBF tile bytes (one per vector tile)
/// * `pbf_xs`, `pbf_ys` — tile x/y coordinates for each PBF tile
/// * `pbf_zoom` — zoom level of the PBF tiles
pub fn apply_buildings_to_abt_tiles(
    abt_bufs: &mut [Vec<u8>],
    pbf_tiles: &[Vec<u8>],
    pbf_xs: &[u32],
    pbf_ys: &[u32],
    pbf_zoom: u32,
) -> ApplyBuildingsResult {
    let opts = SurfaceOpts { buildings: true, canopy: None };
    let result =
        apply_surface_to_abt_tiles(abt_bufs, pbf_tiles, pbf_xs, pbf_ys, pbf_zoom, &opts);

    ApplyBuildingsResult {
        tiles: Vec::new(), // caller already has the mutated bufs
        buildings_decoded: result.buildings_decoded,
        buildings_after_dedup: result.buildings_after_dedup,
        per_tile: result
            .per_tile
            .iter()
            .map(|t| (t.buildings_hit, t.building_pixels))
            .collect(),
    }
}

/// Decode PBF tiles and return buildings as a GeoJSON FeatureCollection string.
pub fn decode_buildings_to_geojson(
    pbf_tiles: &[Vec<u8>],
    pbf_xs: &[u32],
    pbf_ys: &[u32],
    pbf_zoom: u32,
) -> Result<String> {
    let mut all_buildings: Vec<BuildingPolygon> = Vec::new();
    for i in 0..pbf_tiles.len() {
        let pbf_bytes = maybe_gunzip(&pbf_tiles[i]);
        if let Ok((buildings, _)) = extract_buildings_from_pbf(&pbf_bytes, pbf_xs[i], pbf_ys[i], pbf_zoom) {
            all_buildings.extend(buildings);
        }
    }

    // Buffer-zone duplicates: the same whole-ring rule as the rasterising paths.
    drop_duplicate_buildings(&mut all_buildings, BuildingPolygon::duplicate_key);

    // One closed GeoJSON ring: the first vertex repeated at the end.
    let push_ring = |out: &mut String, ring: &[(f64, f64)]| {
        out.push('[');
        for (j, &(lon, lat)) in ring.iter().enumerate() {
            if j > 0 { out.push(','); }
            out.push_str(&format!("[{:.7},{:.7}]", lon, lat));
        }
        if let Some(&(lon, lat)) = ring.first() {
            out.push_str(&format!(",[{:.7},{:.7}]", lon, lat));
        }
        out.push(']');
    };

    let mut features = String::from("[");
    for (i, b) in all_buildings.iter().enumerate() {
        if i > 0 { features.push(','); }
        features.push_str("{\"type\":\"Feature\",\"properties\":{\"height\":");
        features.push_str(&format!("{:.1}", b.height_m));
        features.push_str("},\"geometry\":{\"type\":\"Polygon\",\"coordinates\":[");
        // The exterior first, then its courtyards as the Polygon's interior
        // rings (RFC 7946 §3.1.6) — no longer as buildings of their own.
        push_ring(&mut features, &b.coords);
        for hole in &b.holes {
            features.push(',');
            push_ring(&mut features, hole);
        }
        features.push_str("]}}");
    }
    features.push(']');
    Ok(format!("{{\"type\":\"FeatureCollection\",\"features\":{}}}", features))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buildings::tests::{building_tile, exterior, interior, tile_ref_for};
    use crate::buildings::{rasterize_buildings, I16Grid, RasterOpts};

    const SIZE: u32 = 64;
    const GROUND: i16 = 200; // 100 m in half-metres
    const ROOF: i16 = 224; // + 12 m

    /// Extract one z14 tile's buildings and burn them into flat terrain.
    /// Tile space 0..4096 maps to 0..64 px, so `/64` converts.
    fn burn(features: &[(f64, Vec<Vec<(i32, i32)>>)]) -> (Vec<BuildingPolygon>, ExtractStats, Vec<i16>, u32) {
        let (polys, stats) = extract_buildings_from_pbf(&building_tile(features), 8531, 5752, 14).unwrap();
        let model: Vec<Building> = polys.iter().map(|p| p.to_building()).collect();
        let mut buf = vec![GROUND; (SIZE * SIZE) as usize];
        let mut g = I16Grid { buf: &mut buf, size: SIZE };
        let rs = rasterize_buildings(&mut g, &tile_ref_for(14, 8531, 5752, SIZE), &model, &RasterOpts::default());
        (polys, stats, buf, rs.pixels_modified)
    }

    fn px(buf: &[i16], x: u32, y: u32) -> i16 {
        buf[(y * SIZE + x) as usize]
    }

    #[test]
    fn a_square_with_a_square_hole_raises_the_ring_not_the_courtyard() {
        // Exterior px 16..48, courtyard px 24..40. Every ring used to be drawn
        // as a building of its own, so the courtyard went up to the roof too:
        // 1024 px raised instead of 32² - 16² = 768.
        let (polys, stats, buf, raised) = burn(&[(
            12.0,
            vec![exterior(1024, 1024, 3072, 3072), interior(1536, 1536, 2560, 2560)],
        )]);
        assert_eq!(polys.len(), 1, "one building, not two");
        assert_eq!(polys[0].holes.len(), 1);
        assert_eq!((stats.buildings_out, stats.rings_interior), (1, 1));

        assert_eq!(px(&buf, 20, 20), ROOF, "the ring is the building");
        assert_eq!(px(&buf, 32, 32), GROUND, "the courtyard stays at the terrain");
        assert_eq!(px(&buf, 24, 24), GROUND, "the hole's low edge is the hole's");
        assert_eq!(px(&buf, 40, 40), ROOF, "its high edge is the building's");
        assert_eq!(raised, 768);
    }

    #[test]
    fn a_feature_with_two_exteriors_and_one_interior_is_two_buildings() {
        // A: px 4..20 with a courtyard px 8..16; B: px 32..48, whole.
        let a = exterior(256, 256, 1280, 1280);
        let hole = interior(512, 512, 1024, 1024);
        let b = exterior(2048, 2048, 3072, 3072);
        let check = |polys: &[BuildingPolygon], buf: &[i16], raised: u32, holed: usize| {
            assert_eq!(polys.len(), 2);
            assert_eq!(polys[holed].holes.len(), 1, "the hole belongs to the exterior before it");
            assert_eq!(polys[1 - holed].holes.len(), 0);
            assert_eq!(raised, (16 * 16 - 8 * 8) + 16 * 16);
            assert_eq!((px(buf, 5, 5), px(buf, 12, 12), px(buf, 40, 40)), (ROOF, GROUND, ROOF));
        };

        // Spec order: each exterior followed by its own interior rings.
        let (polys, _, buf, raised) = burn(&[(12.0, vec![a.clone(), hole.clone(), b.clone()])]);
        check(&polys, &buf, raised, 0);

        // A writer with the opposite winding convention: the first ring sets
        // what an exterior looks like, so the courtyard still pairs up.
        let rev = |r: &Vec<(i32, i32)>| r.iter().rev().copied().collect::<Vec<_>>();
        let (polys, _, buf, raised) = burn(&[(12.0, vec![rev(&a), rev(&hole), rev(&b)])]);
        check(&polys, &buf, raised, 0);

        // With the interior ring after B it is B's: the spec pairs a hole with
        // the exterior before it, not with the one it happens to lie in.
        let (polys, _, _, _) = burn(&[(12.0, vec![a, b, hole])]);
        assert_eq!((polys[0].holes.len(), polys[1].holes.len()), (0, 1));
    }

    #[test]
    fn a_degenerate_ring_is_skipped_and_sets_no_winding() {
        // A zero-area first ring must neither become a building nor decide
        // which winding is "exterior" for the rings after it.
        let flat = vec![(100, 100), (300, 100), (200, 100)];
        let (polys, stats, buf, _) = burn(&[(
            12.0,
            vec![flat, exterior(1024, 1024, 3072, 3072), interior(1536, 1536, 2560, 2560)],
        )]);
        assert_eq!(stats.rings_degenerate, 1);
        assert_eq!((polys.len(), polys[0].holes.len()), (1, 1));
        assert_eq!((px(&buf, 20, 20), px(&buf, 32, 32)), (ROOF, GROUND));
    }

    #[test]
    fn the_geojson_export_carries_a_courtyard_as_an_interior_ring() {
        let tile = building_tile(&[(
            12.0,
            vec![exterior(1024, 1024, 3072, 3072), interior(1536, 1536, 2560, 2560)],
        )]);
        let json = decode_buildings_to_geojson(&[tile], &[8531], &[5752], 14).unwrap();
        let doc: serde_json::Value = serde_json::from_str(&json).unwrap();
        let features = doc["features"].as_array().unwrap();
        assert_eq!(features.len(), 1, "the courtyard is no feature of its own");
        let rings = features[0]["geometry"]["coordinates"].as_array().unwrap();
        assert_eq!(rings.len(), 2, "exterior, then the interior ring");
        for ring in rings {
            let ring = ring.as_array().unwrap();
            assert_eq!(ring.len(), 5, "four corners, closed");
            assert_eq!(ring[0], ring[4]);
        }
    }
}
