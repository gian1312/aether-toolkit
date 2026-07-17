// This crate only targets wasm32 (run_download_mem's signature is cfg-dependent);
// compile to an empty lib on native so `cargo build/test --workspace` works.
#![cfg(target_arch = "wasm32")]

use wasm_bindgen::prelude::*;
use aether_converter::download::{
    DownloadJob, run_download_mem, lon2tx, lat2ty, ty2lat, tx2lon,
};
use aether_converter::mvt;
use byteorder::{LittleEndian, WriteBytesExt};
use std::io::{Cursor, Write};

/// Download terrain tiles and convert to .abt format in memory.
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

    let result = run_download_mem(&job, &client, None, on_progress.as_ref())
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
/// into .abt binary buffers.
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

    let (mut bb_s, mut bb_n, mut bb_w, mut bb_e) = (f64::MAX, f64::MIN, f64::MAX, f64::MIN);
    for t in &job.tiles {
        let pd = t.resolution_m / 111_111.0;
        let sp = t.size_px as f64 * pd;
        bb_s = bb_s.min(t.ul_lat - sp);
        bb_n = bb_n.max(t.ul_lat);
        bb_w = bb_w.min(t.ul_lon);
        bb_e = bb_e.max(t.ul_lon + sp);
    }

    let zoom = job.zoom;
    let (x0, x1) = (lon2tx(bb_w, zoom), lon2tx(bb_e, zoom));
    let (y0, y1) = (lat2ty(bb_n, zoom), lat2ty(bb_s, zoom));
    let nx = (x1 - x0 + 1) as usize;
    let gul_lon = tx2lon(x0, zoom);
    let glr_lon = tx2lon(x1 + 1, zoom);
    let gw = nx * 256;
    let gpx = (glr_lon - gul_lon) / gw as f64;

    let mut tile_map: std::collections::HashMap<(u32, u32), Vec<f32>> =
        std::collections::HashMap::with_capacity(tile_xs.len());
    for i in 0..tile_xs.len() {
        tile_map.insert((tile_xs[i], tile_ys[i]), tile_data[i].to_vec());
    }

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
        {
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

    let x_luts: Vec<Vec<usize>> = abt_bufs.iter().map(|abt| {
        let sz = abt.size_px as usize;
        (0..sz).map(|x| {
            let gc = ((abt.ul_lon + (x as f64 + 0.5) * abt.pd - gul_lon) / gpx).round() as isize;
            if gc >= 0 && (gc as usize) < gw { gc as usize } else { usize::MAX }
        }).collect()
    }).collect();

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

    let mut mini_grid = vec![0.0f32; gw * 256];

    for (strip_idx, &(sy0, sy1)) in strips.iter().enumerate() {
        let sny = (sy1 - sy0 + 1) as usize;
        let mut tiles_by_row: Vec<Vec<(u32, &[f32])>> = vec![Vec::new(); sny];
        for ty in sy0..=sy1 {
            for tx in x0..=x1 {
                if let Some(elev) = tile_map.get(&(tx, ty)) {
                    tiles_by_row[(ty - sy0) as usize].push((tx, elev.as_slice()));
                }
            }
        }

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

        if let Some(ref f) = on_progress {
            let _ = f.call2(
                &JsValue::NULL,
                &JsValue::from((strip_idx + 1) as u32),
                &JsValue::from(total_strips as u32),
            );
        }
    }

    let obj = js_sys::Object::new();
    for abt in abt_bufs {
        let arr = js_sys::Uint8Array::from(&abt.buf[..]);
        js_sys::Reflect::set(&obj, &JsValue::from_str(&abt.filename), &arr.into())
            .map_err(|e| JsValue::from_str(&format!("JS error: {e:?}")))?;
    }
    Ok(obj.into())
}

// ── Coordinate utilities ─────────────────────────────────────

#[wasm_bindgen]
pub fn tile_range(south: f64, north: f64, west: f64, east: f64, zoom: u32) -> Vec<u32> {
    vec![lon2tx(west, zoom), lat2ty(north, zoom), lon2tx(east, zoom), lat2ty(south, zoom)]
}

#[wasm_bindgen]
pub fn zoom_for_resolution(resolution_m: f64, lat: f64) -> u32 {
    ((40_075_000.0 * lat.to_radians().cos()) / (resolution_m * 256.0))
        .log2().ceil().min(15.0) as u32
}

#[wasm_bindgen]
pub fn tile_y_to_lat(y: u32, z: u32) -> f64 { ty2lat(y, z) }

#[wasm_bindgen]
pub fn tile_x_to_lon(x: u32, z: u32) -> f64 { tx2lon(x, z) }

// ── Building integration ─────────────────────────────────────
// All logic lives in aether_converter::mvt. These are thin JS↔Rust wrappers.

/// Apply buildings from PBF vector tiles to ALL .abt tile buffers at once.
/// Wrapper around `mvt::apply_buildings_to_abt_tiles`.
#[wasm_bindgen]
pub fn apply_buildings_pbf_batch(
    abt_tiles: Vec<js_sys::Uint8Array>,
    pbf_tiles: Vec<js_sys::Uint8Array>,
    pbf_xs: &[u32],
    pbf_ys: &[u32],
    pbf_zoom: u32,
) -> Result<js_sys::Array, JsValue> {
    if pbf_tiles.len() != pbf_xs.len() || pbf_tiles.len() != pbf_ys.len() {
        return Err(JsValue::from_str("pbf_tiles/pbf_xs/pbf_ys length mismatch"));
    }

    let pbf_vecs: Vec<Vec<u8>> = pbf_tiles.iter().map(|t| t.to_vec()).collect();
    let mut abt_bufs: Vec<Vec<u8>> = abt_tiles.iter().map(|t| t.to_vec()).collect();

    let stats = mvt::apply_buildings_to_abt_tiles(&mut abt_bufs, &pbf_vecs, pbf_xs, pbf_ys, pbf_zoom);

    web_sys::console::log_1(&format!(
        "[AETHER] Buildings: {} decoded, {} after dedup",
        stats.buildings_decoded, stats.buildings_after_dedup
    ).into());
    for (i, &(hits, pixels)) in stats.per_tile.iter().enumerate() {
        web_sys::console::log_1(&format!(
            "[AETHER] .abt tile {}: {} buildings, {} pixels", i, hits, pixels
        ).into());
    }

    let result = js_sys::Array::new_with_length(abt_bufs.len() as u32);
    for (i, buf) in abt_bufs.iter().enumerate() {
        result.set(i as u32, js_sys::Uint8Array::from(&buf[..]).into());
    }
    Ok(result)
}

/// Decode PBF building tiles and return GeoJSON FeatureCollection.
/// Wrapper around `mvt::decode_buildings_to_geojson`.
#[wasm_bindgen]
pub fn decode_buildings_geojson(
    pbf_tiles: Vec<js_sys::Uint8Array>,
    pbf_xs: &[u32],
    pbf_ys: &[u32],
    pbf_zoom: u32,
) -> Result<String, JsValue> {
    let pbf_vecs: Vec<Vec<u8>> = pbf_tiles.iter().map(|t| t.to_vec()).collect();
    mvt::decode_buildings_to_geojson(&pbf_vecs, pbf_xs, pbf_ys, pbf_zoom)
        .map_err(|e| JsValue::from_str(&format!("{e}")))
}
