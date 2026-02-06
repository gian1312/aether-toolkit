use std::collections::HashMap; // Added
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use serde::Deserialize;
use rayon::prelude::*;
use byteorder::{LittleEndian, WriteBytesExt};
use tiff::decoder::{Decoder, DecodingResult};
use anyhow::{Context, Result};
use crate::geo;

#[derive(Deserialize, Debug)]
pub struct IngestJob {
    pub output_path: PathBuf,
    pub ul_lat: f64,
    pub ul_lon: f64,
    pub resolution_m: f64,
    pub size_px: u32,
    pub base_tif: Option<PathBuf>,
    pub swiss_tifs: Vec<PathBuf>,
}

// Made public so main.rs can define the Cache type
pub struct LoadedImage {
    pub width: u32,
    pub height: u32,
    pub data: Vec<i16>,
    pub origin_e: f64,
    pub origin_n: f64,
    pub scale: f64,
}

// ... (Keep parse_swiss_filename and load_tiff_to_ram exactly as they were) ...
fn parse_swiss_filename(p: &Path) -> Option<(f64, f64)> {
    let name = p.file_name()?.to_string_lossy();
    let parts: Vec<&str> = name.split('_').collect();
    for part in parts {
        if part.contains('-') {
            let coords: Vec<&str> = part.split('-').collect();
            if coords.len() == 2 {
                let e = coords[0].parse::<f64>().ok()? * 1000.0;
                let n = coords[1].parse::<f64>().ok()? * 1000.0;
                return Some((e, n));
            }
        }
    }
    None
}

fn load_tiff_to_ram(path: &Path) -> Result<Arc<LoadedImage>> {
    let file = File::open(path).with_context(|| format!("Opening {:?}", path))?;
    let mut decoder = Decoder::new(file)?;
    let (w, h) = decoder.dimensions()?;

    let result = decoder.read_image()?;

    let data: Vec<i16> = match result {
        DecodingResult::F32(v) => v.iter().map(|&x| (x * 2.0) as i16).collect(),
        DecodingResult::I16(v) => v.iter().map(|&x| x.saturating_mul(2)).collect(),
        _ => return Err(anyhow::anyhow!("Unsupported TIF format")),
    };

    let (e, n) = parse_swiss_filename(path).unwrap_or((0.0, 0.0));

    Ok(Arc::new(LoadedImage {
        width: w,
        height: h,
        data,
        origin_e: e,
        origin_n: n,
        scale: 0.5,
    }))
}

// --- CHANGED: Now accepts a Cache ---
pub fn process_tile_with_cache(
    job: IngestJob,
    cache: &mut HashMap<PathBuf, Arc<LoadedImage>>
) -> Result<()> {

    // 1. Resolve Sources using Cache
    let mut swiss_images: Vec<Arc<LoadedImage>> = Vec::with_capacity(job.swiss_tifs.len());

    for path in job.swiss_tifs {
        // If in cache, use it. If not, load it and cache it.
        // We use entry API to keep it clean.
        if !cache.contains_key(&path) {
            // println!("[Cache] Miss - Loading {:?}", path.file_name().unwrap());
            // Only load if valid
            if let Ok(img) = load_tiff_to_ram(&path) {
                cache.insert(path.clone(), img);
            }
        }

        if let Some(img) = cache.get(&path) {
            swiss_images.push(img.clone());
        }
    }

    // --- Pruning Strategy (Simple) ---
    // If the cache gets too huge (>10GB), clear it.
    // A simplistic way to prevent OOM on massive batches.
    // Assuming 60MB per tile, 200 tiles = 12GB.
    if cache.len() > 100 {
        // Simple clearing. A better LRU is complex, but this works for sequential batches.
        // Since we process geographically, we likely won't need the old ones soon.
        // println!("[Cache] Pruning memory...");
        cache.clear();
        // Note: This forces a reload for the next tile, but prevents crash.
    }

    // --- The rest of the function is IDENTICAL to before ---
    let out_size = job.size_px as usize;
    let total_pixels = out_size * out_size;
    let mut buffer = vec![0i16; total_pixels];

    let deg_lat_m = 111132.0;
    let deg_lon_m = 75700.0;

    let pixel_deg_y = job.resolution_m / deg_lat_m;
    let pixel_deg_x = job.resolution_m / deg_lon_m;

    buffer.par_iter_mut().enumerate().for_each(|(idx, out_pixel)| {
        let y = idx / out_size;
        let x = idx % out_size;

        let lat = job.ul_lat - (y as f64 * pixel_deg_y);
        let lon = job.ul_lon + (x as f64 * pixel_deg_x);

        let swiss_coord = geo::wgs84_to_lv95(lat, lon);
        let mut found = false;

        if geo::is_in_swiss_bounds(&swiss_coord) {
            for img in &swiss_images {
                let tile_max_e = img.origin_e + 10000.0;
                let tile_max_n = img.origin_n + 10000.0;

                if swiss_coord.e >= img.origin_e && swiss_coord.e < tile_max_e &&
                    swiss_coord.n >= img.origin_n && swiss_coord.n < tile_max_n {

                    let local_e = swiss_coord.e - img.origin_e;
                    let local_n_from_top = tile_max_n - swiss_coord.n;

                    let px = (local_e / img.scale) as u32;
                    let py = (local_n_from_top / img.scale) as u32;

                    if px < img.width && py < img.height {
                        let idx = (py * img.width + px) as usize;
                        if idx < img.data.len() {
                            *out_pixel = img.data[idx];
                            found = true;
                            break;
                        }
                    }
                }
            }
        }

        if !found {
            *out_pixel = -9999i16.saturating_mul(2);
        }
    });

    let f = File::create(&job.output_path)?;
    let mut w = BufWriter::new(f);

    w.write_all(b"AETH")?;
    w.write_u16::<LittleEndian>(1)?;
    w.write_u16::<LittleEndian>(job.size_px as u16)?;
    w.write_f64::<LittleEndian>(job.ul_lat)?;
    w.write_f64::<LittleEndian>(job.ul_lon)?;
    w.write_f64::<LittleEndian>(pixel_deg_y)?;
    w.write_f64::<LittleEndian>(pixel_deg_x)?;
    w.write_i16::<LittleEndian>(0)?;
    w.write_u16::<LittleEndian>(0)?;

    for &p in &buffer {
        w.write_i16::<LittleEndian>(p)?;
    }

    Ok(())
}