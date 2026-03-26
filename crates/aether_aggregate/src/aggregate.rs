// =============================================================================
// Raster Aggregation — Geo-mapped tiles + pyramid overviews
// =============================================================================
//
// Main tiles: parallel geo-coordinate mapping from input files.
// Overviews: each level built by 4× downsampling the PREVIOUS level
//            (not from inputs). Bounded memory: ~16 source tiles per overview
//            tile, decompressed on demand with local cache.

use crate::reader::InputRaster;
use crate::writer::{self, BigTiffWriter, TileStore};
use rayon::prelude::*;
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

pub fn run(
    inputs: &[InputRaster],
    max_path: &str,
    count_path: &str,
    tile_size: usize,
    compress_level: u32,
) -> Result<AggregateStats, Box<dyn std::error::Error>> {
    let nodata: f32 = -9999.0;
    let count_nodata: f32 = 0.0;
    let ts = tile_size;

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
    let mut max_out = BigTiffWriter::create(max_path, master_w, master_h, ts, &master_gt, nodata, compress_level)?;
    let mut count_out = BigTiffWriter::create(count_path, master_w, master_h, ts, &master_gt, count_nodata, compress_level)?;
    let mut max_store = TileStore::new(master_w, master_h, ts, nodata, compress_level);
    let mut count_store = TileStore::new(master_w, master_h, ts, count_nodata, compress_level);

    // --- 5. Process main tiles (parallel per row) ---
    let t_main = Instant::now();
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
                )
            })
            .collect();

        for r in results {
            // Write to output file
            max_out.write_tile(r.tx, ty, r.max_data.as_ref())?;
            count_out.write_tile(r.tx, ty, r.count_data.as_ref())?;
            // Store compressed data for overview generation
            if let Some(ref d) = r.max_data { max_store.store(r.tx, ty, d.clone()); }
            if let Some(ref d) = r.count_data { count_store.store(r.tx, ty, d.clone()); }

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
    eprintln!("[Aggregate] Main tiles done in {:.2}s", t_main.elapsed().as_secs_f64());

    // --- 6. Build overview pyramid (each level from the previous, 4× downsample) ---
    let t_ovr = Instant::now();
    let dim = master_w.max(master_h);
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

        max_out.write_overview_from_store(&ovr_max)?;
        count_out.write_overview_from_store(&ovr_count)?;

        println!("[S:Overview {}× done]", factor);

        cur_max = ovr_max;
        cur_count = ovr_count;
    }
    eprintln!("[Aggregate] Overviews done in {:.2}s", t_ovr.elapsed().as_secs_f64());

    // --- 7. Finalize ---
    println!("[P:98]");
    println!("[S:Writing file headers]");

    let max_stats = if global.has_valid_data { Some((global.max_min, global.max_max)) } else { None };
    let count_stats = if global.has_valid_data { Some((1.0, global.count_max)) } else { None };
    max_out.finalize(max_stats)?;
    count_out.finalize(count_stats)?;

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
    file_indices: &[usize], nodata: f32, compress_level: u32,
) -> TileResult {
    let bw = ts.min(img_w - tx * ts);
    let bh = ts.min(img_h - ty * ts);

    let tile_geo_x = out_gt[0] + (tx * ts) as f64 * out_gt[1];
    let tile_geo_y = out_gt[3] + (ty * ts) as f64 * out_gt[5];

    let mut max_tile = vec![nodata; ts * ts];
    let mut count_tile = vec![0.0f32; ts * ts];
    let mut has_data = false;

    for &fi in file_indices {
        let fb = &file_geo[fi];
        let inp = &inputs[fi];
        let igt = &inp.geotransform;

        let t_right  = tile_geo_x + bw as f64 * out_gt[1];
        let t_bottom = tile_geo_y + bh as f64 * out_gt[5];

        // Geographic overlap
        let ovl_left   = tile_geo_x.max(fb.left);
        let ovl_right  = t_right.min(fb.right);
        let ovl_top    = tile_geo_y.min(fb.top);
        let ovl_bottom = t_bottom.max(fb.bottom);

        if ovl_left >= ovl_right || ovl_top <= ovl_bottom { continue; }

        // Source region in INPUT file's pixel coords
        let src_x0 = ((ovl_left - igt[0]) / igt[1]).floor().max(0.0) as usize;
        let src_y0 = ((ovl_top - igt[3]) / igt[5]).floor().max(0.0) as usize;
        let src_x1 = (((ovl_right - igt[0]) / igt[1]).ceil() as usize).min(inp.width);
        let src_y1 = (((ovl_bottom - igt[3]) / igt[5]).ceil() as usize).min(inp.height);
        let src_w = src_x1.saturating_sub(src_x0);
        let src_h = src_y1.saturating_sub(src_y0);

        if src_w == 0 || src_h == 0 { continue; }

        let region = inp.read_region_f32(src_x0, src_y0, src_w, src_h);

        // Output pixel range within tile
        let dst_x0 = ((ovl_left - tile_geo_x) / out_gt[1]).round().max(0.0) as usize;
        let dst_y0 = ((ovl_top - tile_geo_y) / out_gt[5]).round().max(0.0) as usize;
        let dst_x1 = (((ovl_right - tile_geo_x) / out_gt[1]).round().max(0.0) as usize).min(bw);
        let dst_y1 = (((ovl_bottom - tile_geo_y) / out_gt[5]).round().max(0.0) as usize).min(bh);

        // Affine: output pixel → input file pixel (nearest neighbor)
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
                    if val == inp.nodata || val.is_nan() { continue; }

                    let idx = row_out_idx + dx;
                    has_data = true;
                    if max_tile[idx] == nodata || val > max_tile[idx] {
                        max_tile[idx] = val;
                    }
                    count_tile[idx] += 1.0;
                }
            }
        } else {
            // FALLBACK PATH: Precise floating-point coordinate re-projection
            for dy in dst_y0..dst_y1 {
                let fy = (base_fy + dy as f64 * step_y).round() as isize;
                if fy < 0 || fy as usize >= inp.height { continue; }
                let ly = (fy as usize).wrapping_sub(src_y0);
                if ly >= src_h { continue; }

                for dx in dst_x0..dst_x1 {
                    let fx = (base_fx + dx as f64 * step_x).round() as isize;
                    if fx < 0 || fx as usize >= inp.width { continue; }
                    let lx = (fx as usize).wrapping_sub(src_x0);
                    if lx >= src_w { continue; }

                    let val = region[ly * src_w + lx];
                    if val == inp.nodata || val.is_nan() { continue; }

                    let idx = dy * ts + dx;
                    has_data = true;
                    if max_tile[idx] == nodata || val > max_tile[idx] {
                        max_tile[idx] = val;
                    }
                    count_tile[idx] += 1.0;
                }
            }
        }
    }

    if !has_data {
        return TileResult { tx, max_data: None, count_data: None, stats: None };
    }

    let mut t_min = f64::INFINITY;
    let mut t_max = f64::NEG_INFINITY;
    let mut t_cmax = 0.0f64;
    for i in 0..ts * ts {
        if max_tile[i] != nodata {
            let v = max_tile[i] as f64;
            if v < t_min { t_min = v; }
            if v > t_max { t_max = v; }
        }
        if count_tile[i] as f64 > t_cmax { t_cmax = count_tile[i] as f64; }
    }

    TileResult {
        tx,
        max_data: Some(writer::compress_f32_tile(&max_tile, ts, ts, compress_level)),
        count_data: Some(writer::compress_f32_tile(&count_tile, ts, ts, compress_level)),
        stats: Some((t_min, t_max, t_cmax)),
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