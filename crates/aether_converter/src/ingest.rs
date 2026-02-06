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

#[derive(Deserialize, Debug, Clone)] // Added Clone for parallel logic if needed
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
    pub origin_e: f64,
    pub origin_n: f64,
    pub max_e: f64, // Pre-calculated for fast bounds check
    pub max_n: f64,
    pub scale: f64,
}

// --- OPTIMIZED MATH: Explicit multiplication is faster than pow() ---
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
    // Buffered Reader for slightly better SSD read performance
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

    Ok(Arc::new(LoadedImage {
        width: w,
        height: h,
        data,
        origin_e: e,
        origin_n: n,
        max_e: e + (w as f64 * 0.5), // Assuming 0.5m scale
        max_n: n + (h as f64 * 0.5), // Correct logic: N is bottom-left in filename, but image is Top-Left?
        // Wait: Swisstopo filenames are Lower-Left corner.
        // Tiff pixels are usually Top-Left.
        // So: Image Top Edge = n + height*scale. Image Bottom Edge = n.
        // Let's ensure this matches the lookup logic below.
        scale: 0.5,
    }))
}

pub fn process_tile_with_cache(
    job: IngestJob,
    cache: &mut HashMap<PathBuf, Arc<LoadedImage>>
) -> Result<()> {

    // --- STEP 1: Parallel Batch Loading (Fix 3) ---
    // Identify what we are missing
    let missing_paths: Vec<PathBuf> = job.swiss_tifs.iter()
        .filter(|p| !cache.contains_key(*p))
        .cloned()
        .collect();

    if !missing_paths.is_empty() {
        // println!("[Cache] Parallel loading {} new files...", missing_paths.len());

        // Load in parallel using all cores
        let results: Vec<Result<(PathBuf, Arc<LoadedImage>)>> = missing_paths
            .par_iter()
            .map(|path| {
                let img = load_tiff_to_ram(path)?;
                Ok((path.clone(), img))
            })
            .collect();

        // Insert into cache (Serial part, but fast)
        for res in results {
            if let Ok((path, img)) = res {
                cache.insert(path, img);
            }
        }
    }

    // Collect references for the compute phase
    let mut swiss_images: Vec<Arc<LoadedImage>> = Vec::with_capacity(job.swiss_tifs.len());
    for path in &job.swiss_tifs {
        if let Some(img) = cache.get(path) {
            swiss_images.push(img.clone());
        }
    }

    // Pruning: Keep cache reasonable (e.g., 50 tiles ~ 10GB)
    if cache.len() > 50 {
        cache.clear();
    }

    // --- STEP 2: Spatial Partitioning & Processing (Fix 2) ---
    let out_size = job.size_px as usize;
    let total_pixels = out_size * out_size;
    let mut buffer = vec![0i16; total_pixels];

    let deg_lat_m = 111132.0;
    let deg_lon_m = 75700.0;
    let pixel_deg_y = job.resolution_m / deg_lat_m;
    let pixel_deg_x = job.resolution_m / deg_lon_m;

    // We iterate by ROWS (Chunks of width).
    // This allows us to pre-filter which images are relevant for this latitude strip.
    buffer.par_chunks_mut(out_size).enumerate().for_each(|(y, row_buffer)| {

        // 1. Calculate Bounds for this Row
        let row_lat = job.ul_lat - (y as f64 * pixel_deg_y);

        // Optimize: Find which images overlap this Row's Latitude?
        // Convert Row Lat to Approx LV95 N
        let (_, n_start) = wgs84_to_lv95_fast(row_lat, job.ul_lon);
        // This is an approximation, but Swiss images are 10km tall.
        // For exactness, we just run the pixel loop, but we optimize the Inner Loop.

        // Inner Loop: Pixels in Row
        for (x, out_pixel) in row_buffer.iter_mut().enumerate() {
            let lon = job.ul_lon + (x as f64 * pixel_deg_x);

            // 2. Optimized Math
            let (e, n) = wgs84_to_lv95_fast(row_lat, lon);

            // 3. Optimized Search (Loop Inversion logic inside)
            let mut found = false;

            // We iterate images. Since this is in L1 cache (the vector of Arcs), it's fast.
            // But we add a Bounds Check before doing any index math.
            for img in &swiss_images {
                // Bounds Check (Coordinate Space) - Fast fail
                // Swiss Filename (Lower Left): origin_e, origin_n.
                // Image Extent: [origin_e, origin_e + 10km], [origin_n, origin_n + 10km]
                let max_e = img.max_e;
                let max_n = img.max_n;

                if e >= img.origin_e && e < max_e && n >= img.origin_n && n < max_n {
                    // Hit! Calculate Index.
                    // Image is Top-Left origin for pixels?
                    // Standard GeoTIFF: Row 0 is Top.
                    // World Y: Max N.
                    // delta_n = Max_N - current_n
                    let local_e = e - img.origin_e;
                    let local_n_from_top = max_n - n;

                    let px = (local_e / img.scale) as u32;
                    let py = (local_n_from_top / img.scale) as u32;

                    if px < img.width && py < img.height {
                        // Unchecked access is unsafe, but fast. Use get() for safety.
                        let idx = (py * img.width + px) as usize;
                        if let Some(&val) = img.data.get(idx) {
                            *out_pixel = val;
                            found = true;
                            break; // Stop looking once found (Layer Priority: First in list wins)
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
    // Large buffer for SSD write coalescing
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

    // Raw bytes write is faster than loop
    let bytes_ptr = buffer.as_ptr() as *const u8;
    let bytes_len = buffer.len() * 2;
    let bytes_slice = unsafe { std::slice::from_raw_parts(bytes_ptr, bytes_len) };
    w.write_all(bytes_slice)?;

    Ok(())
}