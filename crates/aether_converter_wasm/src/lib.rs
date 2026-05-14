use wasm_bindgen::prelude::*;
use aether_converter::download::{
    DownloadJob, run_download_mem, lon2tx, lat2ty, ty2lat, tx2lon,
};
use aether_converter::mvt;
use byteorder::{LittleEndian, ReadBytesExt, WriteBytesExt};
use serde::Deserialize;
use std::io::{Cursor, Write};

/// Download terrain tiles and convert to .abt format in memory.
/// job_json: JSON string matching DownloadJob schema.
/// on_progress: optional JS callback `(phase, done, total)` called during
///   download (phase 0, every 5 %) and decode (phase 1, every 5 %).
/// Returns: JS object mapping filename → Uint8Array of .abt bytes.
#[wasm_bindgen]
pub async fn download_terrain(
    job_json: &str,
    on_progress: Option<js_sys::Function>,
) -> Result<JsValue, JsValue> {
    let job: DownloadJob = serde_json::from_str(job_json)
        .map_err(|e| JsValue::from_str(&format!("Invalid job JSON: {e}")))?;

    let client = reqwest::Client::builder()
        .build()
        .map_err(|e| JsValue::from_str(&format!("HTTP client error: {e}")))?;

    let result = run_download_mem(
        &job,
        &client,
        None,                    // Rust callback unused — js_progress handles everything
        on_progress.as_ref(),    // raw JS function passed directly to fetch pool + decode loop
    )
    .await
    .map_err(|e| JsValue::from_str(&format!("Download failed: {e}")))?;

    let obj = js_sys::Object::new();
    for (name, data) in result {
        let arr = js_sys::Uint8Array::from(&data[..]);
        js_sys::Reflect::set(&obj, &JsValue::from_str(&name), &arr.into())
            .map_err(|e| JsValue::from_str(&format!("JS error: {e:?}")))?;
    }
    Ok(obj.into())
}

// ── Assemble pre-decoded tiles into .abt buffers ─────────────

/// Assemble terrain from pre-decoded f32 elevation tiles (from Web Workers)
/// into .abt binary buffers.  Runs only the resampling / assembly step —
/// no network I/O, no PNG decode.
///
/// * `job_json`  — Same DownloadJob JSON as `download_terrain`.
/// * `tile_xs`, `tile_ys` — Parallel arrays of XYZ tile x/y coordinates.
/// * `tile_data` — Array of Float32Array(65536) per tile (256×256 elevations).
/// * `on_progress` — Optional `(done, total)` callback for assembly progress.
///
/// Returns: JS object mapping filename → Uint8Array of .abt bytes (same
///          format as `download_terrain`).
#[wasm_bindgen]
pub fn assemble_terrain(
    job_json: &str,
    tile_xs: &[u32],
    tile_ys: &[u32],
    tile_data: Vec<js_sys::Float32Array>,
    on_progress: Option<js_sys::Function>,
) -> Result<JsValue, JsValue> {
    let job: DownloadJob = serde_json::from_str(job_json)
        .map_err(|e| JsValue::from_str(&format!("Invalid job JSON: {e}")))?;

    if tile_xs.len() != tile_ys.len() || tile_xs.len() != tile_data.len() {
        return Err(JsValue::from_str("tile_xs/tile_ys/tile_data length mismatch"));
    }

    // 1. Full bbox across all output sub-tiles.
    let (mut bb_s, mut bb_n, mut bb_w, mut bb_e) = (f64::MAX, f64::MIN, f64::MAX, f64::MIN);
    for t in &job.tiles {
        let pd = t.resolution_m / 111_111.0;
        let sp = t.size_px as f64 * pd;
        bb_s = bb_s.min(t.ul_lat - sp);
        bb_n = bb_n.max(t.ul_lat);
        bb_w = bb_w.min(t.ul_lon);
        bb_e = bb_e.max(t.ul_lon + sp);
    }

    // 2. Tile range.
    let zoom = job.zoom;
    let (x0, x1) = (lon2tx(bb_w, zoom), lon2tx(bb_e, zoom));
    let (y0, y1) = (lat2ty(bb_n, zoom), lat2ty(bb_s, zoom));
    let nx = (x1 - x0 + 1) as usize;
    let gul_lon = tx2lon(x0, zoom);
    let glr_lon = tx2lon(x1 + 1, zoom);
    let gw = nx * 256;
    let gpx = (glr_lon - gul_lon) / gw as f64;

    // 3. Index decoded tiles by (x, y).
    let mut tile_map: std::collections::HashMap<(u32, u32), Vec<f32>> =
        std::collections::HashMap::with_capacity(tile_xs.len());
    for i in 0..tile_xs.len() {
        tile_map.insert((tile_xs[i], tile_ys[i]), tile_data[i].to_vec());
    }

    // 4. Prepare in-memory .abt buffers.
    struct MemAbt {
        filename: String,
        buf: Vec<u8>,
        size_px: u32,
        stride: usize,
        ul_lat: f64,
        ul_lon: f64,
        pd: f64,
    }
    let mut abt_bufs: Vec<MemAbt> = Vec::with_capacity(job.tiles.len());
    for spec in &job.tiles {
        let pd = spec.resolution_m / 111_111.0;
        let bpr = spec.size_px as usize * 2;
        let stride = (bpr + 255) & !255;
        let total_bytes = 44 + stride * spec.size_px as usize;
        let mut buf = vec![0u8; total_bytes];
        // 44-byte .abt header
        {
            use byteorder::WriteBytesExt;
            let mut c = Cursor::new(&mut buf[..44]);
            c.write_all(b"AETH").unwrap();
            c.write_u16::<LittleEndian>(1).unwrap();
            c.write_u16::<LittleEndian>(spec.size_px as u16).unwrap();
            c.write_f64::<LittleEndian>(spec.ul_lat).unwrap();
            c.write_f64::<LittleEndian>(spec.ul_lon).unwrap();
            c.write_f64::<LittleEndian>(pd).unwrap();
            c.write_f64::<LittleEndian>(pd).unwrap();
            c.write_i16::<LittleEndian>(0).unwrap();
            c.write_u16::<LittleEndian>(stride as u16).unwrap();
        }
        abt_bufs.push(MemAbt {
            filename: spec.filename.clone(), buf, size_px: spec.size_px,
            stride, ul_lat: spec.ul_lat, ul_lon: spec.ul_lon, pd,
        });
    }

    // 5. Pre-compute x-lookup tables.
    let x_luts: Vec<Vec<usize>> = abt_bufs.iter().map(|abt| {
        let sz = abt.size_px as usize;
        (0..sz).map(|x| {
            let gc = ((abt.ul_lon + (x as f64 + 0.5) * abt.pd - gul_lon) / gpx).round() as isize;
            if gc >= 0 && (gc as usize) < gw { gc as usize } else { usize::MAX }
        }).collect()
    }).collect();

    // 6. Build strip ranges.
    let strip_rows: u32 = 32;
    let mut strips: Vec<(u32, u32)> = Vec::new();
    {
        let mut cur = y0;
        while cur <= y1 {
            let end = (cur + strip_rows - 1).min(y1);
            strips.push((cur, end));
            cur = end + 1;
        }
    }
    let total_strips = strips.len();

    // 7. Resample decoded tiles into .abt buffers (strip-by-strip).
    let mut mini_grid = vec![0.0f32; gw * 256];

    for (strip_idx, &(sy0, sy1)) in strips.iter().enumerate() {
        let sny = (sy1 - sy0 + 1) as usize;

        // Group tiles by row within the strip.
        let mut tiles_by_row: Vec<Vec<(u32, &[f32])>> = vec![Vec::new(); sny];
        for ty in sy0..=sy1 {
            for tx in x0..=x1 {
                if let Some(elev) = tile_map.get(&(tx, ty)) {
                    tiles_by_row[(ty - sy0) as usize].push((tx, elev.as_slice()));
                }
            }
        }

        // Process one tile-row at a time.
        for tr in 0..sny {
            let ty = sy0 + tr as u32;
            mini_grid.fill(0.0);

            for &(tx, elev) in &tiles_by_row[tr] {
                let col = (tx - x0) as usize * 256;
                for py in 0..256usize {
                    let cw = 256.min(gw - col);
                    mini_grid[py * gw + col..py * gw + col + cw]
                        .copy_from_slice(&elev[py * 256..py * 256 + cw]);
                }
            }

            let tr_top = ty2lat(ty, zoom);
            let tr_bot = ty2lat(ty + 1, zoom);
            let tr_spy = (tr_top - tr_bot) / 256.0;

            for (sti, abt) in abt_bufs.iter_mut().enumerate() {
                let sz = abt.size_px as usize;
                let x_lut = &x_luts[sti];
                let spec_bot = abt.ul_lat - sz as f64 * abt.pd;
                if abt.ul_lat <= tr_bot || spec_bot >= tr_top { continue; }

                for y in 0..sz {
                    let lat = abt.ul_lat - (y as f64 + 0.5) * abt.pd;
                    if lat > tr_top || lat <= tr_bot { continue; }
                    let gr = (((tr_top - lat) / tr_spy).round() as usize).min(255);
                    let row_off = gr * gw;
                    let buf_offset = 44 + y * abt.stride;

                    for x in 0..sz {
                        let gc = x_lut[x];
                        let val: i16 = if gc < gw {
                            (mini_grid[row_off + gc] * 2.0).round() as i16
                        } else { 0 };
                        let byte_off = buf_offset + x * 2;
                        abt.buf[byte_off..byte_off + 2]
                            .copy_from_slice(&val.to_le_bytes());
                    }
                }
            }
        }

        // Report assembly progress per strip.
        if let Some(ref f) = on_progress {
            let _ = f.call2(
                &JsValue::NULL,
                &JsValue::from((strip_idx + 1) as u32),
                &JsValue::from(total_strips as u32),
            );
        }
    }

    // 8. Build result object.
    let obj = js_sys::Object::new();
    for abt in abt_bufs {
        let arr = js_sys::Uint8Array::from(&abt.buf[..]);
        js_sys::Reflect::set(&obj, &JsValue::from_str(&abt.filename), &arr.into())
            .map_err(|e| JsValue::from_str(&format!("JS error: {e:?}")))?;
    }
    Ok(obj.into())
}

/// Get tile coordinate range for a bounding box at a given zoom level.
/// Returns [x_min, y_min, x_max, y_max] as u32 array.
#[wasm_bindgen]
pub fn tile_range(south: f64, north: f64, west: f64, east: f64, zoom: u32) -> Vec<u32> {
    vec![
        lon2tx(west, zoom),
        lat2ty(north, zoom),
        lon2tx(east, zoom),
        lat2ty(south, zoom),
    ]
}

/// Convert resolution in meters to optimal zoom level.
/// Capped at zoom 15 (Terrarium max).
#[wasm_bindgen]
pub fn zoom_for_resolution(resolution_m: f64, lat: f64) -> u32 {
    ((40_075_000.0 * lat.to_radians().cos()) / (resolution_m * 256.0))
        .log2()
        .ceil()
        .min(15.0) as u32
}

/// Convert tile Y to latitude (north edge of tile).
#[wasm_bindgen]
pub fn tile_y_to_lat(y: u32, z: u32) -> f64 {
    ty2lat(y, z)
}

/// Convert tile X to longitude (west edge of tile).
#[wasm_bindgen]
pub fn tile_x_to_lon(x: u32, z: u32) -> f64 {
    tx2lon(x, z)
}

// ── Building rasterization (ported from ingest.rs) ──────────

#[derive(Deserialize)]
struct BuildingPoly {
    /// Outer ring as flat [lon, lat, lon, lat, ...] array
    coords: Vec<f64>,
    /// Building roof height in meters AMSL
    height_m: f64,
}

/// Apply building heights to an .abt tile buffer in-place.
///
/// Reads the 44-byte .abt header to determine tile bounds and resolution,
/// then rasterizes each building polygon onto the i16 elevation grid
/// using the same point-in-polygon scanline logic as aether_converter's
/// native `process_geometry_wgs84` in ingest.rs.
///
/// buildings_json: JSON array of `{ coords: [lon,lat,...], height_m: f64 }`
/// Returns the MODIFIED tile data (wasm_bindgen copies &mut [u8] in, never copies back).
#[wasm_bindgen]
pub fn apply_buildings(
    tile_data: &[u8],
    buildings_json: &str,
) -> Result<js_sys::Uint8Array, JsValue> {
    let buildings: Vec<BuildingPoly> = serde_json::from_str(buildings_json)
        .map_err(|e| JsValue::from_str(&format!("Invalid buildings JSON: {e}")))?;

    if buildings.is_empty() || tile_data.len() < 44 {
        return Ok(js_sys::Uint8Array::from(tile_data));
    }

    let mut buf = tile_data.to_vec();

    // Parse .abt header
    let mut c = Cursor::new(&tile_data[4..44]);
    let _version = c.read_u16::<LittleEndian>().unwrap();
    let size_px = c.read_u16::<LittleEndian>().unwrap() as u32;
    let ul_lat = c.read_f64::<LittleEndian>().unwrap();
    let ul_lon = c.read_f64::<LittleEndian>().unwrap();
    let scale_y = c.read_f64::<LittleEndian>().unwrap();
    let _scale_x = c.read_f64::<LittleEndian>().unwrap();
    let _base_elev = c.read_i16::<LittleEndian>().unwrap();
    let stride = c.read_u16::<LittleEndian>().unwrap() as usize;

    let px_deg = scale_y; // degrees per pixel
    let tile_south = ul_lat - px_deg * size_px as f64;
    let tile_east = ul_lon + px_deg * size_px as f64;

    let mut total_modified: u32 = 0;

    for bldg in &buildings {
        let n_verts = bldg.coords.len() / 2;
        if n_verts < 3 { continue; }

        // Convert to pixel coords + compute bbox
        let mut vertices: Vec<(f64, f64)> = Vec::with_capacity(n_verts);
        let mut min_x = size_px as f64;
        let mut max_x = 0.0f64;
        let mut min_y = size_px as f64;
        let mut max_y = 0.0f64;

        for i in 0..n_verts {
            let lon = bldg.coords[i * 2];
            let lat = bldg.coords[i * 2 + 1];
            let px = (lon - ul_lon) / px_deg;
            let py = (ul_lat - lat) / px_deg;
            min_x = min_x.min(px);
            max_x = max_x.max(px);
            min_y = min_y.min(py);
            max_y = max_y.max(py);
            vertices.push((px, py));
        }

        // Quick bbox rejection
        if max_x < 0.0 || min_x >= size_px as f64 || max_y < 0.0 || min_y >= size_px as f64 {
            continue;
        }

        let start_x = (min_x.floor().max(0.0)) as u32;
        let end_x = (max_x.ceil().min(size_px as f64)) as u32;
        let start_y = (min_y.floor().max(0.0)) as u32;
        let end_y = (max_y.ceil().min(size_px as f64)) as u32;

        // height_m is building height ABOVE GROUND (from OSM tags).
        // Add it on top of the current terrain elevation (i16 = meters × 2).
        let bldg_h_i16 = (bldg.height_m * 2.0).round() as i16;
        if bldg_h_i16 <= 0 { continue; }

        for y in start_y..end_y {
            let py_center = y as f64 + 0.5;
            for x in start_x..end_x {
                if point_in_poly(x as f64 + 0.5, py_center, &vertices) {
                    let offset = 44 + y as usize * stride + x as usize * 2;
                    if offset + 1 < buf.len() {
                        let current = i16::from_le_bytes([buf[offset], buf[offset + 1]]);
                        let with_building = current.saturating_add(bldg_h_i16);
                        buf[offset..offset + 2].copy_from_slice(&with_building.to_le_bytes());
                        total_modified += 1;
                    }
                }
            }
        }
    }

    if total_modified > 0 {
        web_sys::console::log_1(
            &format!("[AETHER] apply_buildings: {} pixels modified across {} buildings",
                     total_modified, buildings.len()).into(),
        );
    }

    Ok(js_sys::Uint8Array::from(&buf[..]))
}

fn point_in_poly(x: f64, y: f64, poly: &[(f64, f64)]) -> bool {
    let mut inside = false;
    let mut j = poly.len() - 1;
    for i in 0..poly.len() {
        let (xi, yi) = poly[i];
        let (xj, yj) = poly[j];
        let intersect = ((yi > y) != (yj > y)) && (x < (xj - xi) * (y - yi) / (yj - yi) + xi);
        if intersect { inside = !inside; }
        j = i;
    }
    inside
}

// ── PBF-based building application ─────────────────────────────

/// Apply buildings from OpenFreeMap PBF vector tiles to an .abt tile buffer.
///
/// Decodes the `building` layer from each PBF tile, extracts polygon geometry
/// + heights, and rasterizes onto the .abt elevation grid.  All decoding and
/// rasterization happens in Rust — no JS building decode needed.
///
/// * `tile_data` — .abt tile buffer (44-byte header + i16 elevation grid)
/// * `pbf_tiles` — Array of raw PBF tile bytes (one per vector tile)
/// * `pbf_xs`, `pbf_ys` — Tile x/y coordinates for each PBF tile
/// * `pbf_zoom` — Zoom level of the PBF tiles (typically 14)
///
/// Returns the modified .abt tile buffer.
#[wasm_bindgen]
pub fn apply_buildings_pbf(
    tile_data: &[u8],
    pbf_tiles: Vec<js_sys::Uint8Array>,
    pbf_xs: &[u32],
    pbf_ys: &[u32],
    pbf_zoom: u32,
) -> Result<js_sys::Uint8Array, JsValue> {
    if tile_data.len() < 44 {
        return Ok(js_sys::Uint8Array::from(tile_data));
    }
    if pbf_tiles.len() != pbf_xs.len() || pbf_tiles.len() != pbf_ys.len() {
        return Err(JsValue::from_str("pbf_tiles/pbf_xs/pbf_ys length mismatch"));
    }

    let mut buf = tile_data.to_vec();

    // Parse .abt header
    let mut c = Cursor::new(&tile_data[4..44]);
    let _version = c.read_u16::<LittleEndian>().unwrap();
    let size_px = c.read_u16::<LittleEndian>().unwrap() as u32;
    let ul_lat = c.read_f64::<LittleEndian>().unwrap();
    let ul_lon = c.read_f64::<LittleEndian>().unwrap();
    let scale_y = c.read_f64::<LittleEndian>().unwrap();
    let _scale_x = c.read_f64::<LittleEndian>().unwrap();
    let _base_elev = c.read_i16::<LittleEndian>().unwrap();
    let stride = c.read_u16::<LittleEndian>().unwrap() as usize;

    let px_deg = scale_y;
    let mut total_buildings = 0usize;
    let mut total_modified = 0u32;

    for i in 0..pbf_tiles.len() {
        let pbf_bytes = pbf_tiles[i].to_vec();
        let buildings = match mvt::extract_buildings_from_pbf(&pbf_bytes, pbf_xs[i], pbf_ys[i], pbf_zoom) {
            Ok(b) => b,
            Err(_) => continue,
        };

        total_buildings += buildings.len();

        for bldg in &buildings {
            if bldg.coords.len() < 3 { continue; }

            let bldg_h_i16 = (bldg.height_m * 2.0).round() as i16;
            if bldg_h_i16 <= 0 { continue; }

            // Convert to pixel coords + compute bbox
            let mut vertices: Vec<(f64, f64)> = Vec::with_capacity(bldg.coords.len());
            let mut min_x = size_px as f64;
            let mut max_x = 0.0f64;
            let mut min_y = size_px as f64;
            let mut max_y = 0.0f64;

            for &(lon, lat) in &bldg.coords {
                let px = (lon - ul_lon) / px_deg;
                let py = (ul_lat - lat) / px_deg;
                min_x = min_x.min(px);
                max_x = max_x.max(px);
                min_y = min_y.min(py);
                max_y = max_y.max(py);
                vertices.push((px, py));
            }

            if max_x < 0.0 || min_x >= size_px as f64 || max_y < 0.0 || min_y >= size_px as f64 {
                continue;
            }

            let start_x = (min_x.floor().max(0.0)) as u32;
            let end_x = (max_x.ceil().min(size_px as f64)) as u32;
            let start_y = (min_y.floor().max(0.0)) as u32;
            let end_y = (max_y.ceil().min(size_px as f64)) as u32;

            for y in start_y..end_y {
                let py_center = y as f64 + 0.5;
                for x in start_x..end_x {
                    if point_in_poly(x as f64 + 0.5, py_center, &vertices) {
                        let offset = 44 + y as usize * stride + x as usize * 2;
                        if offset + 1 < buf.len() {
                            let current = i16::from_le_bytes([buf[offset], buf[offset + 1]]);
                            let with_building = current.saturating_add(bldg_h_i16);
                            buf[offset..offset + 2].copy_from_slice(&with_building.to_le_bytes());
                            total_modified += 1;
                        }
                    }
                }
            }
        }
    }

    if total_buildings > 0 {
        web_sys::console::log_1(
            &format!("[AETHER] apply_buildings_pbf: {} buildings decoded, {} pixels modified",
                     total_buildings, total_modified).into(),
        );
    }

    Ok(js_sys::Uint8Array::from(&buf[..]))
}
