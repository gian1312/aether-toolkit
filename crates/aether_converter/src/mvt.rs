// MVT (Mapbox Vector Tile) decoder for building extraction.
//
// Decodes PBF-encoded vector tiles and extracts building polygons with
// geographic coordinates + heights.  Used by both the native converter
// (for PBF tile files) and the WASM pipeline (for OpenFreeMap tiles).

use anyhow::{anyhow, Result};
use prost::Message;

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

fn decode_geometry(cmds: &[u32], extent: u32, tile_x: u32, tile_y: u32, z: u32) -> Vec<Vec<(f64, f64)>> {
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
    // Look for render_height or building:levels in tags
    let mut i = 0;
    while i + 1 < feature.tags.len() {
        let key_idx = feature.tags[i] as usize;
        let val_idx = feature.tags[i + 1] as usize;
        i += 2;

        if key_idx >= layer.keys.len() || val_idx >= layer.values.len() { continue; }
        let key = &layer.keys[key_idx];
        let val = &layer.values[val_idx];

        if key == "render_height" {
            if let Some(v) = val.float_val { if v > 0.0 { return v as f64; } }
            if let Some(v) = val.double_val { if v > 0.0 { return v; } }
            if let Some(v) = val.int_val { if v > 0 { return v as f64; } }
            if let Some(v) = val.uint_val { if v > 0 { return v as f64; } }
        }
        if key == "building:levels" {
            if let Some(v) = val.int_val { if v > 0 { return v as f64 * 3.0; } }
            if let Some(v) = val.uint_val { if v > 0 { return v as f64 * 3.0; } }
            if let Some(ref s) = val.string_val {
                if let Ok(n) = s.parse::<f64>() { if n > 0.0 { return n * 3.0; } }
            }
        }
    }
    DEFAULT_HEIGHT
}

/// Decode a PBF vector tile and extract building polygons with heights.
///
/// Returns building polygons in WGS84 coordinates with height above ground.
/// `tile_x`, `tile_y`, `z` are the XYZ tile coordinates for geo-referencing.
pub fn extract_buildings_from_pbf(
    pbf_data: &[u8],
    tile_x: u32,
    tile_y: u32,
    z: u32,
) -> Result<Vec<BuildingPolygon>> {
    let tile = Tile::decode(pbf_data).map_err(|e| anyhow!("PBF decode error: {e}"))?;

    let mut buildings = Vec::new();

    for layer in &tile.layers {
        if layer.name != "building" { continue; }

        let extent = layer.extent.unwrap_or(4096);

        for feature in &layer.features {
            let geom_type = feature.r#type.map(GeomType::from_i32).unwrap_or(GeomType::Unknown);
            if geom_type != GeomType::Polygon { continue; }

            let height = resolve_height(feature, layer);
            let rings = decode_geometry(&feature.geometry, extent, tile_x, tile_y, z);

            // First ring is outer, rest are holes (we take outer only)
            if let Some(outer) = rings.into_iter().next() {
                if outer.len() >= 3 {
                    buildings.push(BuildingPolygon { coords: outer, height_m: height });
                }
            }
        }
    }

    Ok(buildings)
}
