// =============================================================================
// Raster Aggregation — Geo-mapped tiles + pyramid overviews
// =============================================================================
//
// Main tiles: parallel geo-coordinate mapping from input files.
// Max is written natively as 8-bit Integer (scaled inside standard GIS RAM).
// Count is written natively as 16-bit Integer (65k overhead ceiling).
//
// Visibility index: Dense Tiled CSR (.vix) — per-tile compressed CSR arrays
// with O(1) pixel lookup. Written by a background thread fed from rayon.

use crate::reader::InputRaster;
use crate::writer::{self, BigTiffWriter, TileStoreU8, TileStoreU16};
use flate2::write::ZlibEncoder;
use flate2::Compression;
use rayon::prelude::*;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::Instant;

// ═══════════════════════════════════════════════════════════════════════════════
// Public Interface
// ═══════════════════════════════════════════════════════════════════════════════

pub struct AggregateStats {
    pub has_valid_data: bool,
    pub max_min: f64,
    pub max_max: f64,
    pub count_max: f64,
}

/// Message sent from rayon workers to the .vix writer thread.
struct VixTile {
    tx: u32,
    ty: u32,
    compressed: Vec<u8>,
}

pub fn run(
    inputs: &[InputRaster],
    wp_ids: &[u32],
    max_path: &str,
    count_path: &str,
    visibility_path: Option<&str>,
    tile_size: usize,
    compress_level: u32,
) -> Result<AggregateStats, Box<dyn std::error::Error>> {
    let nodata: u8 = 0;
    let count_nodata: u16 = 0;
    let ts = tile_size;
    let build_vis = visibility_path.is_some();

    // --- 1. Compute master grid ---
    let (master_gt, master_w, master_h) = compute_master_grid(inputs)?;
    let tiles_x = (master_w + ts - 1) / ts;
    let tiles_y = (master_h + ts - 1) / ts;

    eprintln!(
        "[Aggregate] Master grid: {}x{} | Tiles: {}x{} = {}",
        master_w, master_h, tiles_x, tiles_y, tiles_x * tiles_y
    );

    // --- 2. File geographic bounds ---
    let file_geo: Vec<GeoBounds> = inputs
        .iter()
        .map(|inp| {
            let gt = &inp.geotransform;
            GeoBounds {
                left:   gt[0],
                top:    gt[3],
                right:  gt[0] + inp.width as f64 * gt[1],
                bottom: gt[3] + inp.height as f64 * gt[5],
            }
        })
        .collect();

    // --- 3. Build tile → files map ---
    let rows = build_tile_file_map(&file_geo, &master_gt, master_w, master_h, ts, tiles_x, tiles_y);
    let data_tiles: usize = rows.iter().map(|r| r.len()).sum();
    eprintln!(
        "[Aggregate] {} data tiles (of {}, {:.0}% sparse)",
        data_tiles, tiles_x * tiles_y,
        (1.0 - data_tiles as f64 / (tiles_x * tiles_y) as f64) * 100.0
    );

    // --- 4. Open outputs + tile stores ---
    let mut max_out = BigTiffWriter::create(max_path, master_w, master_h, ts, &master_gt, nodata as u16, compress_level, 8)?;
    let mut count_out = BigTiffWriter::create(count_path, master_w, master_h, ts, &master_gt, count_nodata, compress_level, 16)?;
    let mut max_store = TileStoreU8::new(master_w, master_h, ts, nodata, compress_level);
    let mut count_store = TileStoreU16::new(master_w, master_h, ts, count_nodata, compress_level);

    // --- .vix writer thread ---
    let (vix_tx, vix_rx) = mpsc::channel::<VixTile>();
    let vix_thread = if let Some(vis_path) = visibility_path {
        let vis_path = vis_path.to_string();
        let master_w_copy = master_w;
        let master_h_copy = master_h;
        let ts_copy = ts;
        let gt_copy = master_gt;
        Some(thread::spawn(move || {
            let mut file = BufWriter::new(File::create(&vis_path).expect("create .vix"));
            let mut toc: Vec<(u32, u32, u64, u64)> = Vec::new(); // (tx, ty, offset, size)
            let mut current_offset: u64 = 0;

            for tile in vix_rx {
                let size = tile.compressed.len() as u64;
                file.write_all(&tile.compressed).expect("write .vix tile");
                toc.push((tile.tx, tile.ty, current_offset, size));
                current_offset += size;
            }

            // Write JSON TOC (matching Python VixWriter format)
            let mut tiles_map = serde_json::Map::new();
            for &(tx, ty, off, sz) in &toc {
                tiles_map.insert(
                    format!("{},{}", tx, ty),
                    serde_json::json!({ "offset": off, "size": sz }),
                );
            }
            let toc_json = serde_json::json!({
                "meta": {
                    "tile_size": ts_copy,
                    "master_width": master_w_copy,
                    "master_height": master_h_copy,
                    "geotransform": gt_copy,
                    "projection": "EPSG:4326",
                },
                "tiles": tiles_map,
            });
            let toc_bytes = serde_json::to_vec(&toc_json).expect("serialize TOC");
            let toc_offset = current_offset;
            file.write_all(&toc_bytes).expect("write TOC");

            // Trailer: toc_offset(u64) + magic("VIX!")
            file.write_all(&toc_offset.to_le_bytes()).expect("write toc offset");
            file.write_all(b"VIX!").expect("write magic");
            file.flush().expect("flush .vix");

            eprintln!("[VIX] Written {} tiles, {:.1} MB compressed",
                     toc.len(), current_offset as f64 / 1e6);
        }))
    } else {
        None
    };

    // --- 5. Process main tiles (parallel per row) ---
    let t_main = Instant::now();
    let prof_read_us = AtomicU64::new(0);
    let prof_read_calls = AtomicU64::new(0);
    let prof_read_pixels = AtomicU64::new(0);
    let prof_process_us = AtomicU64::new(0);
    let prof_vis_bits = AtomicU64::new(0);
    let mut global = AggregateStats {
        has_valid_data: false,
        max_min: f64::INFINITY,
        max_max: f64::NEG_INFINITY,
        count_max: 0.0,
    };
    let mut processed = 0usize;

    for ty in 0..tiles_y {
        if rows[ty].is_empty() { continue; }

        let results: Vec<TileResult> = rows[ty]
            .par_iter()
            .map(|(tx, file_indices)| {
                process_tile(
                    *tx, ty, ts, master_w, master_h, &master_gt,
                    inputs, &file_geo, file_indices, nodata, compress_level,
                    wp_ids, build_vis,
                    &prof_read_us, &prof_read_calls, &prof_read_pixels,
                    &prof_process_us, &prof_vis_bits
                )
            })
            .collect();

        for r in results {
            max_out.write_tile(r.tx, ty, r.max_data.as_ref())?;
            count_out.write_tile(r.tx, ty, r.count_data.as_ref())?;
            if let Some(ref d) = r.max_data { max_store.store(r.tx, ty, d.clone()); }
            if let Some(ref d) = r.count_data { count_store.store(r.tx, ty, d.clone()); }

            if let Some(vix_data) = r.vix_compressed {
                let _ = vix_tx.send(VixTile { tx: r.tx as u32, ty: ty as u32, compressed: vix_data });
            }

            if let Some(s) = r.stats {
                global.has_valid_data = true;
                if s.0 < global.max_min { global.max_min = s.0; }
                if s.1 > global.max_max { global.max_max = s.1; }
                if s.2 > global.count_max { global.count_max = s.2; }
            }
            processed += 1;
        }

        if data_tiles > 20 && processed % (data_tiles / 10).max(1) == 0 {
            let pct = (processed as f64 / data_tiles as f64 * 100.0) as u32;
            println!("[P:{}]", pct.min(90));
            println!("[S:Main tiles {:.0}%]", pct);
        }
    }

    let tile_loop_secs = t_main.elapsed().as_secs_f64();
    eprintln!("[Profile] Tile loop: {:.2}s", tile_loop_secs);
    eprintln!("[Profile] read_region_u8: {} calls, {:.1}M pixels, {:.2}s (thread-sum)",
             prof_read_calls.load(Ordering::Relaxed),
             prof_read_pixels.load(Ordering::Relaxed) as f64 / 1e6,
             prof_read_us.load(Ordering::Relaxed) as f64 / 1e6);
    eprintln!("[Profile] pixel processing: {:.2}s (thread-sum), {} vis links",
             prof_process_us.load(Ordering::Relaxed) as f64 / 1e6,
             prof_vis_bits.load(Ordering::Relaxed));

    // Close channel and wait for .vix writer
    drop(vix_tx);
    let t_vix_join = Instant::now();
    if let Some(t) = vix_thread {
        println!("[S:Finalizing visibility index...]");
        t.join().unwrap();
    }
    eprintln!("[Profile] VIX write wait: {:.2}s", t_vix_join.elapsed().as_secs_f64());

    eprintln!("[Aggregate] Main tiles done in {:.2}s", t_main.elapsed().as_secs_f64());

    // --- 6. Build overview pyramid ---
    let t_ovr = Instant::now();
    let mut cur_max = max_store;
    let mut cur_count = count_store;
    let mut level = 0u32;

    loop {
        let cur_dim = cur_max.width.max(cur_max.height);
        if cur_dim <= 256 { break; }

        level += 1;
        let factor = 4usize.pow(level);
        eprintln!("[Aggregate] Building overview {}× ({} → {})",
                  factor, cur_max.width, (cur_max.width + 3) / 4);

        let ovr_max = cur_max.build_overview_4x();
        let ovr_count = cur_count.build_overview_4x();

        max_out.write_overview_from_store_u8(&ovr_max)?;
        count_out.write_overview_from_store_u16(&ovr_count)?;

        println!("[S:Overview {}× done]", factor);

        cur_max = ovr_max;
        cur_count = ovr_count;
    }
    eprintln!("[Aggregate] Overviews done in {:.2}s", t_ovr.elapsed().as_secs_f64());

    // --- 7. Finalize ---
    println!("[P:98]");
    println!("[S:Writing file headers]");

    let gdal_scale = inputs.first().map_or(1.0, |i| i.scale);
    let gdal_offset = inputs.first().map_or(0.0, |i| i.offset);

    let max_stats = if global.has_valid_data { Some((global.max_min, global.max_max)) } else { None };
    let count_stats = if global.has_valid_data { Some((1.0, global.count_max)) } else { None };

    eprintln!("[Aggregate] Max GeoTIFF: scale={}, offset={}, stats(raw)={:?}",
             gdal_scale, gdal_offset, max_stats);
    eprintln!("[Aggregate] Count GeoTIFF: scale=1.0, offset=0.0, stats={:?}", count_stats);

    max_out.finalize(max_stats, gdal_scale, gdal_offset)?;
    count_out.finalize(count_stats, 1.0, 0.0)?;

    Ok(global)
}

// ═══════════════════════════════════════════════════════════════════════════════
// Internal Helpers
// ═══════════════════════════════════════════════════════════════════════════════

struct GeoBounds {
    left: f64,
    top: f64,
    right: f64,
    bottom: f64,
}

struct TileResult {
    tx: usize,
    max_data: Option<Vec<u8>>,
    count_data: Option<Vec<u8>>,
    vix_compressed: Option<Vec<u8>>,
    stats: Option<(f64, f64, f64)>,
}

fn build_tile_file_map(
    file_geo: &[GeoBounds], gt: &[f64; 6],
    img_w: usize, img_h: usize, ts: usize, tiles_x: usize, tiles_y: usize,
) -> Vec<Vec<(usize, Vec<usize>)>> {
    let mut rows: Vec<Vec<(usize, Vec<usize>)>> = vec![Vec::new(); tiles_y];

    for (fi, fb) in file_geo.iter().enumerate() {
        let tx0 = ((fb.left - gt[0]) / gt[1]).floor().max(0.0) as usize / ts;
        let ty0 = ((fb.top - gt[3]) / gt[5]).floor().max(0.0) as usize / ts;
        let px1 = ((fb.right - gt[0]) / gt[1]).ceil().max(0.0) as usize;
        let py1 = ((fb.bottom - gt[3]) / gt[5]).ceil().max(0.0) as usize;
        let tx1 = ((px1 + ts - 1) / ts).min(tiles_x);
        let ty1 = ((py1 + ts - 1) / ts).min(tiles_y);

        for ty in ty0..ty1 {
            for tx in tx0..tx1 {
                if let Some(e) = rows[ty].iter_mut().find(|(t, _)| *t == tx) {
                    if !e.1.contains(&fi) { e.1.push(fi); }
                } else {
                    rows[ty].push((tx, vec![fi]));
                }
            }
        }
    }
    rows
}

fn process_tile(
    tx: usize, ty: usize, ts: usize,
    img_w: usize, img_h: usize, out_gt: &[f64; 6],
    inputs: &[InputRaster], file_geo: &[GeoBounds],
    file_indices: &[usize], nodata: u8, compress_level: u32,
    wp_ids: &[u32], build_vis: bool,
    prof_read_us: &AtomicU64, prof_read_calls: &AtomicU64, prof_read_pixels: &AtomicU64,
    prof_process_us: &AtomicU64, prof_vis_bits: &AtomicU64
) -> TileResult {
    let bw = ts.min(img_w - tx * ts);
    let bh = ts.min(img_h - ty * ts);

    let tile_geo_x = out_gt[0] + (tx * ts) as f64 * out_gt[1];
    let tile_geo_y = out_gt[3] + (ty * ts) as f64 * out_gt[5];

    let mut max_tile = vec![nodata; ts * ts];
    let mut count_tile = vec![0u16; ts * ts];
    let mut has_data = false;
    let mut local_vis_links: u64 = 0;
    let t_process = Instant::now();

    // Dense pre-allocated visibility storage (fixed-stride, zero realloc)
    let max_wp_per_pixel = file_indices.len();
    let num_pixels = ts * ts;
    let mut vis_storage: Option<Vec<u16>> = if build_vis && max_wp_per_pixel > 0 {
        Some(vec![0u16; num_pixels * max_wp_per_pixel])
    } else {
        None
    };
    let mut vis_cursors: Option<Vec<u16>> = if build_vis {
        Some(vec![0u16; num_pixels])
    } else {
        None
    };

    for &fi in file_indices {
        let fb = &file_geo[fi];
        let inp = &inputs[fi];
        let igt = &inp.geotransform;
        let wpid = wp_ids[fi] as u16;

        let t_right  = tile_geo_x + bw as f64 * out_gt[1];
        let t_bottom = tile_geo_y + bh as f64 * out_gt[5];

        let ovl_left   = tile_geo_x.max(fb.left);
        let ovl_right  = t_right.min(fb.right);
        let ovl_top    = tile_geo_y.min(fb.top);
        let ovl_bottom = t_bottom.max(fb.bottom);

        if ovl_left >= ovl_right || ovl_top <= ovl_bottom { continue; }

        let src_x0 = ((ovl_left - igt[0]) / igt[1]).floor().max(0.0) as usize;
        let src_y0 = ((ovl_top - igt[3]) / igt[5]).floor().max(0.0) as usize;
        let src_x1 = (((ovl_right - igt[0]) / igt[1]).ceil() as usize).min(inp.width);
        let src_y1 = (((ovl_bottom - igt[3]) / igt[5]).ceil() as usize).min(inp.height);
        let src_w = src_x1.saturating_sub(src_x0);
        let src_h = src_y1.saturating_sub(src_y0);

        if src_w == 0 || src_h == 0 { continue; }

        let t_read = Instant::now();
        let region = inp.read_region_u8(src_x0, src_y0, src_w, src_h);
        prof_read_us.fetch_add(t_read.elapsed().as_micros() as u64, Ordering::Relaxed);
        prof_read_calls.fetch_add(1, Ordering::Relaxed);
        prof_read_pixels.fetch_add((src_w * src_h) as u64, Ordering::Relaxed);

        let dst_x0 = ((ovl_left - tile_geo_x) / out_gt[1]).round().max(0.0) as usize;
        let dst_y0 = ((ovl_top - tile_geo_y) / out_gt[5]).round().max(0.0) as usize;
        let dst_x1 = (((ovl_right - tile_geo_x) / out_gt[1]).round().max(0.0) as usize).min(bw);
        let dst_y1 = (((ovl_bottom - tile_geo_y) / out_gt[5]).round().max(0.0) as usize).min(bh);

        let base_fx = (tile_geo_x - igt[0]) / igt[1];
        let base_fy = (tile_geo_y - igt[3]) / igt[5];
        let step_x = out_gt[1] / igt[1];
        let step_y = out_gt[5] / igt[5];

        let is_1_to_1 = (step_x - 1.0).abs() < 1e-5 && (step_y - 1.0).abs() < 1e-5;
        let is_integer_offset = (base_fx - base_fx.round()).abs() < 1e-3 && (base_fy - base_fy.round()).abs() < 1e-3;

        // FAST PATH: Direct integer indexing for 1:1 aligned grids
        if is_1_to_1 && is_integer_offset {
            let int_base_x = base_fx.round() as isize;
            let int_base_y = base_fy.round() as isize;

            for dy in dst_y0..dst_y1 {
                let fy = int_base_y + dy as isize;
                if fy < 0 || (fy as usize) >= inp.height { continue; }
                let ly = (fy as usize).wrapping_sub(src_y0);
                if ly >= src_h { continue; }

                let row_out_idx = dy * ts;
                let row_in_idx = ly * src_w;

                for dx in dst_x0..dst_x1 {
                    let fx = int_base_x + dx as isize;
                    if fx < 0 || (fx as usize) >= inp.width { continue; }
                    let lx = (fx as usize).wrapping_sub(src_x0);
                    if lx >= src_w { continue; }

                    let val = region[row_in_idx + lx];
                    if val == 0 { continue; }

                    let idx = row_out_idx + dx;
                    has_data = true;
                    if val > max_tile[idx] {
                        max_tile[idx] = val;
                    }
                    count_tile[idx] = count_tile[idx].saturating_add(1);

                    if let (Some(ref mut st), Some(ref mut cu)) = (&mut vis_storage, &mut vis_cursors) {
                        let slot = idx * max_wp_per_pixel + cu[idx] as usize;
                        st[slot] = wpid;
                        cu[idx] += 1;
                        local_vis_links += 1;
                    }
                }
            }
        } else {
            // FALLBACK PATH: Precise floating-point coordinate re-projection
            for dy in dst_y0..dst_y1 {
                let fy = (base_fy + dy as f64 * step_y).round() as isize;
                if fy < 0 || fy as usize >= inp.height { continue; }
                let ly = (fy as usize).wrapping_sub(src_y0);
                if ly >= src_h { continue; }

                let mut current_fx = base_fx + dst_x0 as f64 * step_x;
                for dx in dst_x0..dst_x1 {
                    let fx = current_fx.round() as isize;
                    current_fx += step_x;
                    if fx < 0 || fx as usize >= inp.width { continue; }
                    let lx = (fx as usize).wrapping_sub(src_x0);
                    if lx >= src_w { continue; }

                    let val = region[ly * src_w + lx];
                    if val == 0 { continue; }

                    let idx = dy * ts + dx;
                    has_data = true;
                    if val > max_tile[idx] {
                        max_tile[idx] = val;
                    }
                    count_tile[idx] = count_tile[idx].saturating_add(1);

                    if let (Some(ref mut st), Some(ref mut cu)) = (&mut vis_storage, &mut vis_cursors) {
                        let slot = idx * max_wp_per_pixel + cu[idx] as usize;
                        st[slot] = wpid;
                        cu[idx] += 1;
                        local_vis_links += 1;
                    }
                }
            }
        }
    }

    prof_process_us.fetch_add(t_process.elapsed().as_micros() as u64, Ordering::Relaxed);
    prof_vis_bits.fetch_add(local_vis_links, Ordering::Relaxed);

    if !has_data {
        return TileResult { tx, max_data: None, count_data: None, vix_compressed: None, stats: None };
    }

    // --- Build Dense CSR and compress for .vix ---
    let vix_compressed = if let (Some(st), Some(cu)) = (vis_storage, vis_cursors) {
        // Build CSR offsets from cursors (prefix sum) + compact-copy wp_ids
        let total_links: usize = cu.iter().map(|&c| c as usize).sum();
        if total_links == 0 {
            None
        } else {
            let mut offsets = Vec::with_capacity(num_pixels + 1);
            let mut flat_wp_ids = Vec::with_capacity(total_links);
            let mut running = 0u32;

            for i in 0..num_pixels {
                offsets.push(running);
                let count = cu[i] as usize;
                let base = i * max_wp_per_pixel;
                for j in 0..count {
                    flat_wp_ids.push(st[base + j]);
                }
                running += count as u32;
            }
            offsets.push(running);

            // Zero-copy serialization: cast slices directly to bytes
            let offsets_bytes: &[u8] = unsafe {
                std::slice::from_raw_parts(
                    offsets.as_ptr() as *const u8,
                    offsets.len() * 4,
                )
            };
            let wp_ids_bytes: &[u8] = unsafe {
                std::slice::from_raw_parts(
                    flat_wp_ids.as_ptr() as *const u8,
                    flat_wp_ids.len() * 2,
                )
            };

            // Compress with level 1 (fast — repeating zeros compress equally well)
            let mut encoder = ZlibEncoder::new(
                Vec::with_capacity(offsets_bytes.len() / 4),
                Compression::new(1),
            );
            encoder.write_all(offsets_bytes).expect("zlib compress vix offsets");
            encoder.write_all(wp_ids_bytes).expect("zlib compress vix wp_ids");
            Some(encoder.finish().expect("zlib finish vix tile"))
        }
    } else {
        None
    };

    // Process exact integer max values natively
    let mut t_min = 255u8;
    let mut t_max = 0u8;
    let mut t_cmax = 0u16;
    for i in 0..ts * ts {
        let v = max_tile[i];
        if v != 0 {
            if v < t_min { t_min = v; }
            if v > t_max { t_max = v; }
        }
        let cv = count_tile[i];
        if cv > t_cmax { t_cmax = cv; }
    }

    // Stats as raw stored values (GDAL convention: consumer applies scale/offset)
    let t_min_f = if t_min <= t_max { t_min as f64 } else { f64::INFINITY };
    let t_max_f = if t_min <= t_max { t_max as f64 } else { f64::NEG_INFINITY };

    TileResult {
        tx,
        max_data: Some(writer::compress_u8_tile(&max_tile, ts, ts, compress_level)),
        count_data: Some(writer::compress_u16_tile(&count_tile, ts, ts, compress_level)),
        vix_compressed,
        stats: Some((t_min_f, t_max_f, t_cmax as f64)),
    }
}

fn compute_master_grid(
    inputs: &[InputRaster],
) -> Result<([f64; 6], usize, usize), Box<dyn std::error::Error>> {
    if inputs.is_empty() { return Err("No inputs".into()); }

    let gt1 = inputs[0].geotransform[1];
    let gt5 = inputs[0].geotransform[5];

    for (i, inp) in inputs.iter().enumerate().skip(1) {
        let d1 = (inp.geotransform[1] - gt1).abs() / gt1.abs();
        let d5 = (inp.geotransform[5] - gt5).abs() / gt5.abs();
        if d1 > 0.01 || d5 > 0.01 {
            eprintln!(
                "[WARNING] Input {} resolution differs >1%: ({:.10}, {:.10}) vs ({:.10}, {:.10}). Mixing different Coordinate Reference Systems may cause artifacts.",
                i, inp.geotransform[1], inp.geotransform[5], gt1, gt5
            );
        }
    }

    let mut min_x = f64::INFINITY;
    let mut min_y = f64::INFINITY;
    let mut max_x = f64::NEG_INFINITY;
    let mut max_y = f64::NEG_INFINITY;

    for inp in inputs {
        let gt = &inp.geotransform;
        min_x = min_x.min(gt[0]);
        max_y = max_y.max(gt[3]);
        max_x = max_x.max(gt[0] + inp.width as f64 * gt[1]);
        min_y = min_y.min(gt[3] + inp.height as f64 * gt[5]);
    }

    let master_gt =[min_x, gt1, 0.0, max_y, 0.0, gt5];
    let master_w = ((max_x - min_x) / gt1).round() as usize;
    let master_h = ((max_y - min_y) / gt5.abs()).round() as usize;

    Ok((master_gt, master_w, master_h))
}
