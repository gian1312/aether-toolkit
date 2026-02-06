use std::collections::HashMap;
use std::fs::File;
use std::io::{BufWriter, Write, BufReader};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use serde::Deserialize;
use rayon::prelude::*;
use byteorder::{LittleEndian, WriteBytesExt};
use tiff::decoder::{Decoder, DecodingResult};
use anyhow::{Context, Result};

#[derive(Deserialize, Debug, Clone)]
pub struct IngestJob {
    pub output_path: PathBuf,
    pub ul_lat: f64,
    pub ul_lon: f64,
    pub resolution_m: f64,
    pub size_px: u32,
    pub base_tif: Option<PathBuf>,
    pub swiss_tifs: Vec<PathBuf>,
}

pub struct LoadedImage {
    pub width: u32,
    pub height: u32,
    pub data: Vec<i16>,
    pub origin_e: f64, // Bottom-Left Easting
    pub origin_n: f64, // Bottom-Left Northing
    pub max_e: f64,    // Top-Right Easting
    pub max_n: f64,    // Top-Right Northing
    pub scale: f64,
}

#[inline(always)]
fn wgs84_to_lv95_fast(lat: f64, lon: f64) -> (f64, f64) {
    let phi = (lat * 3600.0 - 169028.66) / 10000.0;
    let lam = (lon * 3600.0 - 26782.5) / 10000.0;

    let lam2 = lam * lam;
    let lam3 = lam2 * lam;
    let phi2 = phi * phi;
    let phi3 = phi2 * phi;

    let e = 2600072.37
        + 211455.93 * lam
        - 10938.51 * lam * phi
        - 0.36 * lam * phi2
        - 44.54 * lam3;

    let n = 1200147.07
        + 308807.95 * phi
        + 3745.25 * lam2
        + 76.63 * phi2
        - 194.56 * lam2 * phi
        + 119.79 * phi3;

    (e, n)
}

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
    let reader = BufReader::with_capacity(1024 * 1024, file);
    let mut decoder = Decoder::new(reader)?;
    let (w, h) = decoder.dimensions()?;

    let result = decoder.read_image()?;

    let data: Vec<i16> = match result {
        DecodingResult::F32(v) => v.iter().map(|&x| (x * 2.0) as i16).collect(),
        DecodingResult::I16(v) => v.iter().map(|&x| x.saturating_mul(2)).collect(),
        _ => return Err(anyhow::anyhow!("Unsupported TIF format")),
    };

    let (e, n) = parse_swiss_filename(path).unwrap_or((0.0, 0.0));

    // Swisstopo Filename is Bottom-Left Corner (e.g. 2600-1120)
    // Image Data is Top-Left to Bottom-Right
    // So:
    // origin_n (Bottom) = n
    // max_n (Top) = n + height * scale
    // origin_e (Left) = e
    // max_e (Right) = e + width * scale
    let scale = 0.5;

    Ok(Arc::new(LoadedImage {
        width: w,
        height: h,
        data,
        origin_e: e,
        origin_n: n,
        max_e: e + (w as f64 * scale),
        max_n: n + (h as f64 * scale),
        scale,
    }))
}

pub fn process_tile_with_cache(
    job: IngestJob,
    cache: &mut HashMap<PathBuf, Arc<LoadedImage>>
) -> Result<()> {

    // --- STEP 1: Parallel Loading ---
    let missing_paths: Vec<PathBuf> = job.swiss_tifs.iter()
        .filter(|p| !cache.contains_key(*p))
        .cloned()
        .collect();

    if !missing_paths.is_empty() {
        let results: Vec<Result<(PathBuf, Arc<LoadedImage>)>> = missing_paths
            .par_iter()
            .map(|path| {
                let img = load_tiff_to_ram(path)?;
                Ok((path.clone(), img))
            })
            .collect();

        for res in results {
            if let Ok((path, img)) = res {
                cache.insert(path, img);
            }
        }
    }

    let mut swiss_images: Vec<Arc<LoadedImage>> = Vec::with_capacity(job.swiss_tifs.len());
    for path in &job.swiss_tifs {
        if let Some(img) = cache.get(path) {
            swiss_images.push(img.clone());
        }
    }

    // Aggressive Pruning for safety
    if cache.len() > 20 {
        cache.clear();
    }

    // --- STEP 2: Partitioned Processing ---
    let out_size = job.size_px as usize;
    let total_pixels = out_size * out_size;
    let mut buffer = vec![0i16; total_pixels];

    let deg_lat_m = 111132.0;
    let deg_lon_m = 75700.0;
    let pixel_deg_y = job.resolution_m / deg_lat_m;
    let pixel_deg_x = job.resolution_m / deg_lon_m;

    buffer.par_chunks_mut(out_size).enumerate().for_each(|(y, row_buffer)| {
        let row_lat = job.ul_lat - (y as f64 * pixel_deg_y);

        // --- OPTIMIZATION: Row-Level Filtering ---
        // 1. Calculate the geographic extent of this specific row (scanline)
        // Left Edge
        let (e_start, n_start) = wgs84_to_lv95_fast(row_lat, job.ul_lon);
        // Right Edge
        let (e_end, n_end) = wgs84_to_lv95_fast(row_lat, job.ul_lon + (out_size as f64 * pixel_deg_x));

        // Create a loose bounding box for the row
        // Add 50m buffer to account for projection rotation/curvature
        let row_min_n = n_start.min(n_end) - 50.0;
        let row_max_n = n_start.max(n_end) + 50.0;
        let row_min_e = e_start.min(e_end) - 50.0;
        let row_max_e = e_start.max(e_end) + 50.0;

        // 2. Build a small subset of images that overlap this row
        // This reduces checks from ~100 to ~2-3 per pixel
        let mut row_images: Vec<&LoadedImage> = Vec::with_capacity(5);

        for img in &swiss_images {
            // Check Latitude/Northing Intersection
            let overlaps_n = img.max_n >= row_min_n && img.origin_n <= row_max_n;
            // Check Longitude/Easting Intersection
            let overlaps_e = img.max_e >= row_min_e && img.origin_e <= row_max_e;

            if overlaps_n && overlaps_e {
                row_images.push(img.as_ref());
            }
        }

        // Inner Loop: Pixels in Row
        for (x, out_pixel) in row_buffer.iter_mut().enumerate() {
            let lon = job.ul_lon + (x as f64 * pixel_deg_x);
            let (e, n) = wgs84_to_lv95_fast(row_lat, lon);

            let mut found = false;

            // Iterate ONLY the filtered images
            for img in &row_images {
                // Precise Point Check
                if e >= img.origin_e && e < img.max_e && n >= img.origin_n && n < img.max_n {

                    let local_e = e - img.origin_e;
                    let local_n_from_top = img.max_n - n;

                    let px = (local_e / img.scale) as u32;
                    let py = (local_n_from_top / img.scale) as u32;

                    if px < img.width && py < img.height {
                        let idx = (py * img.width + px) as usize;
                        // Unchecked is safe here because of logic above,
                        // but get() is safer for production
                        if let Some(&val) = img.data.get(idx) {
                            *out_pixel = val;
                            found = true;
                            break;
                        }
                    }
                }
            }

            if !found {
                *out_pixel = -9999i16.saturating_mul(2);
            }
        }
    });

    // --- STEP 3: Write Output ---
    let f = File::create(&job.output_path)?;
    let mut w = BufWriter::with_capacity(1024 * 1024, f);

    w.write_all(b"AETH")?;
    w.write_u16::<LittleEndian>(1)?;
    w.write_u16::<LittleEndian>(job.size_px as u16)?;
    w.write_f64::<LittleEndian>(job.ul_lat)?;
    w.write_f64::<LittleEndian>(job.ul_lon)?;
    w.write_f64::<LittleEndian>(pixel_deg_y)?;
    w.write_f64::<LittleEndian>(pixel_deg_x)?;
    w.write_i16::<LittleEndian>(0)?;
    w.write_u16::<LittleEndian>(0)?;

    let bytes_ptr = buffer.as_ptr() as *const u8;
    let bytes_len = buffer.len() * 2;
    let bytes_slice = unsafe { std::slice::from_raw_parts(bytes_ptr, bytes_len) };
    w.write_all(bytes_slice)?;

    Ok(())
}