// MVT (Mapbox Vector Tile) decoder for building extraction.
//
// Decodes PBF-encoded vector tiles and extracts building polygons with
// geographic coordinates + heights.  Used by both the native converter
// (for PBF tile files) and the WASM pipeline (for OpenFreeMap tiles).

use anyhow::{anyhow, Result};
use prost::Message;

use crate::buildings::{Building, BuildingHeight, HeightSource};

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
    /// Outer ring as (lon, lat) pairs in WGS84.
    pub coords: Vec<(f64, f64)>,
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
            height: BuildingHeight::AboveGround(self.height_m),
            source: self.source,
        }
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
    let tile_w = tile_to_lon(tile_x + 1, z) - tile_to_lon(tile_x, z);
    let tile_n = tile_to_lat(tile_y, z);
    let tile_s = tile_to_lat(tile_y + 1, z);
    let tile_origin_lon = tile_to_lon(tile_x, z);
    let ext = extent as f64;

    let mut rings: Vec<Vec<(f64, f64)>> = Vec::new();
    let mut ring: Vec<(f64, f64)> = Vec::new();
    let mut cx: i32 = 0;
    let mut cy: i32 = 0;
    let mut i = 0;

    while i < cmds.len() {
        let cmd_int = cmds[i];
        let cmd_id = cmd_int & 0x7;
        let cmd_count = (cmd_int >> 3) as usize;
        i += 1;

        match cmd_id {
            1 => {
                // MoveTo — start new ring
                if ring.len() >= 3 { rings.push(std::mem::take(&mut ring)); }
                else { ring.clear(); }

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
                }
            }
            7 => {
                // ClosePath — close current ring
                if ring.len() >= 3 {
                    rings.push(std::mem::take(&mut ring));
                } else {
                    ring.clear();
                }
            }
            _ => { i += cmd_count * 2; } // skip unknown
        }
    }
    if ring.len() >= 3 { rings.push(ring); }

    rings
}

#[inline]
fn zigzag(n: u32) -> i32 {
    ((n >> 1) as i32) ^ -((n & 1) as i32)
}

// ── Building extraction ────────────────────────────────────────

const DEFAULT_HEIGHT: f64 = 6.0;

fn resolve_height(feature: &Feature, layer: &Layer) -> f64 {
    resolve_height_detailed(feature, layer).0
}

/// Returns (height_m, source). The ladder is: an explicit `render_height`, then
/// a storey count via `building:levels`, then [`DEFAULT_HEIGHT`]. The source is
/// carried out so callers can report how much of a result was guessed.
fn resolve_height_detailed(feature: &Feature, layer: &Layer) -> (f64, HeightSource) {
    let mut i = 0;
    while i + 1 < feature.tags.len() {
        let key_idx = feature.tags[i] as usize;
        let val_idx = feature.tags[i + 1] as usize;
        i += 2;

        if key_idx >= layer.keys.len() || val_idx >= layer.values.len() { continue; }
        let key = &layer.keys[key_idx];
        let val = &layer.values[val_idx];

        if key == "render_height" {
            if let Some(v) = val.float_val { if v > 0.0 { return (v as f64, HeightSource::ExplicitHeight); } }
            if let Some(v) = val.double_val { if v > 0.0 { return (v, HeightSource::ExplicitHeight); } }
            if let Some(v) = val.int_val { if v > 0 { return (v as f64, HeightSource::ExplicitHeight); } }
            if let Some(v) = val.uint_val { if v > 0 { return (v as f64, HeightSource::ExplicitHeight); } }
            if let Some(v) = val.sint_val { if v > 0 { return (v as f64, HeightSource::ExplicitHeight); } }
        }
        if key == "building:levels" {
            if let Some(v) = val.int_val { if v > 0 { return (v as f64 * 3.0, HeightSource::Levels); } }
            if let Some(v) = val.uint_val { if v > 0 { return (v as f64 * 3.0, HeightSource::Levels); } }
            if let Some(v) = val.sint_val { if v > 0 { return (v as f64 * 3.0, HeightSource::Levels); } }
            if let Some(ref s) = val.string_val {
                if let Ok(n) = s.parse::<f64>() { if n > 0.0 { return (n * 3.0, HeightSource::Levels); } }
            }
        }
    }
    (DEFAULT_HEIGHT, HeightSource::Default)
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
            let rings = decode_geometry(&feature.geometry, extent, tile_x, tile_y, z);

            // Each feature can be a multi-polygon: Planetiler merges individual
            // buildings into single features. Each ring is a separate building.
            // Outer rings (clockwise in tile space) are buildings; counter-clockwise
            // rings are holes (courtyards). At z14, most rings are outer = buildings.
            if rings.is_empty() {
                stats.features_no_rings += 1;
            } else {
                if height_source == HeightSource::Default {
                    stats.height_default_count += 1;
                } else {
                    stats.height_explicit_count += 1;
                }
                let mut ring_count = 0;
                for ring in rings {
                    if ring.len() < 3 {
                        stats.features_short_ring += 1;
                        continue;
                    }
                    // Check winding: signed area > 0 = CCW in WGS84 = outer ring
                    // (MVT CW in tile-space becomes CCW after lat-flip to WGS84)
                    let signed_area: f64 = ring.windows(2)
                        .map(|w| w[0].0 * w[1].1 - w[1].0 * w[0].1)
                        .sum();
                    if signed_area.abs() < 1e-14 { continue; } // degenerate
                    // Take all rings as buildings — holes are rare at z14
                    // and their small negative area is harmless for LOS
                    stats.height_sum += height;
                    if height < stats.height_min { stats.height_min = height; }
                    if height > stats.height_max { stats.height_max = height; }
                    buildings.push(BuildingPolygon { coords: ring, height_m: height, source: height_source });
                    ring_count += 1;
                }
                if ring_count == 0 { stats.features_short_ring += 1; }
            }
        }
    }

    stats.buildings_out = buildings.len();
    (buildings, stats)
}

// ── Batch building application to .abt tiles ──────────────────────

use crate::canopy::{apply_surface_to_abt_tiles, SurfaceOpts};
use std::collections::HashSet;

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

    // Dedup
    {
        let mut seen = HashSet::new();
        all_buildings.retain(|b| {
            if b.coords.is_empty() { return false; }
            let (lon, lat) = b.coords[0];
            let key = ((lat * 1_000_000.0).round() as i64, (lon * 1_000_000.0).round() as i64);
            seen.insert(key)
        });
    }

    let mut features = String::from("[");
    for (i, b) in all_buildings.iter().enumerate() {
        if i > 0 { features.push(','); }
        features.push_str("{\"type\":\"Feature\",\"properties\":{\"height\":");
        features.push_str(&format!("{:.1}", b.height_m));
        features.push_str("},\"geometry\":{\"type\":\"Polygon\",\"coordinates\":[[");
        for (j, &(lon, lat)) in b.coords.iter().enumerate() {
            if j > 0 { features.push(','); }
            features.push_str(&format!("[{:.7},{:.7}]", lon, lat));
        }
        if let Some(&(lon, lat)) = b.coords.first() {
            features.push_str(&format!(",[{:.7},{:.7}]", lon, lat));
        }
        features.push_str("]]}}");
    }
    features.push(']');
    Ok(format!("{{\"type\":\"FeatureCollection\",\"features\":{}}}", features))
}
