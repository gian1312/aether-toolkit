use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{BufWriter, Write, BufReader};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use serde::Deserialize;
use rayon::prelude::*;
use byteorder::{LittleEndian, WriteBytesExt};
use tiff::decoder::{Decoder, DecodingResult};
use anyhow::{Context, Result};

// Use specific traits for FlatGeobuf
use flatgeobuf::{FgbReader, FallibleStreamingIterator, GeometryType};

#[derive(Deserialize, Debug, Clone)]
pub struct IngestJob {
    pub output_path: PathBuf,
    pub ul_lat: f64,
    pub ul_lon: f64,
    pub resolution_m: f64,
    pub size_px: u32,
    pub base_tif: Option<PathBuf>,
    pub swiss_tifs: Vec<PathBuf>,
    pub buildings_file: Option<PathBuf>, // Can be File or Directory
}

pub struct LoadedImage {
    pub width: u32,
    pub height: u32,
    pub data: Vec<i16>,
    pub origin_e: f64,
    pub origin_n: f64,
    pub max_e: f64,
    pub max_n: f64,
    pub scale: f64,
}

#[inline(always)]
fn wgs84_to_lv95_fast(lat: f64, lon: f64) -> (f64, f64) {
    // Approximate WGS84 to LV95 conversion (swisstopo)
    let phi = (lat * 3600.0 - 169028.66) / 10000.0;
    let lam = (lon * 3600.0 - 26782.5) / 10000.0;

    let e = 2600072.37
        + 211455.93 * lam
        - 10938.51 * lam * phi
        - 0.36 * lam * phi * phi
        - 44.54 * lam * lam * lam;

    let n = 1200147.07
        + 308807.95 * phi
        + 3745.25 * lam * lam
        + 76.63 * phi * phi
        - 194.56 * lam * lam * phi
        + 119.79 * phi * phi * phi;

    (e, n)
}

fn parse_swiss_filename(p: &Path) -> Option<(f64, f64)> {
    let name = p.file_name()?.to_string_lossy();
    // Filename format: swissalti3d_2019_2501-1120_...
    let parts: Vec<&str> = name.split('_').collect();
    for part in parts {
        if part.contains('-') {
            let coords: Vec<&str> = part.split('-').collect();
            if coords.len() == 2 {
                if let (Ok(e_km), Ok(n_km)) = (coords[0].parse::<f64>(), coords[1].parse::<f64>()) {
                    return Some((e_km * 1000.0, n_km * 1000.0));
                }
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

    // SwissAlti3D is usually f32 or i16. We normalize to i16 (0.5m units).
    let data: Vec<i16> = match result {
        DecodingResult::F32(v) => v.iter().map(|&x| (x * 2.0) as i16).collect(),
        DecodingResult::I16(v) => v.iter().map(|&x| x.saturating_mul(2)).collect(),
        _ => return Err(anyhow::anyhow!("Unsupported TIF format")),
    };

    let (e, n) = parse_swiss_filename(path).unwrap_or((0.0, 0.0));

    // Scale = WorldUnits / Pixels = 1000m / 2000px = 0.5m.
    let scale = 1000.0 / w as f64;

    Ok(Arc::new(LoadedImage {
        width: w, height: h, data,
        // origin_n is TOP (High N), max_n is BOTTOM (Low N)
        origin_e: e, origin_n: n + 1000.0,
        max_e: e + 1000.0, max_n: n,
        scale,
    }))
}

// --- BUILDINGS LOGIC ---

fn apply_buildings(job: &IngestJob, buffer: &mut [i16], fgb_target: &Path, px_deg: f64) -> Result<()> {
    // 1. Resolve Files
    let mut files_to_process = Vec::new();
    if fgb_target.is_dir() {
        if let Ok(entries) = fs::read_dir(fgb_target) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().map_or(false, |ext| ext == "fgb") {
                    files_to_process.push(path);
                }
            }
        }
    } else {
        files_to_process.push(fgb_target.to_path_buf());
    }

    // 2. Calculate Query Bounds (LV95)
    let (ul_e, ul_n) = wgs84_to_lv95_fast(job.ul_lat, job.ul_lon);

    // Calculate Lower-Right Lat/Lon
    let lr_lat = job.ul_lat - (job.size_px as f64 * px_deg);
    let lr_lon = job.ul_lon + (job.size_px as f64 * px_deg);

    let (lr_e, lr_n) = wgs84_to_lv95_fast(lr_lat, lr_lon);

    // Bounding Box with Safety Padding
    let min_e = ul_e.min(lr_e) - 20.0;
    let max_e = ul_e.max(lr_e) + 20.0;
    let min_n = ul_n.min(lr_n) - 20.0;
    let max_n = ul_n.max(lr_n) + 20.0;

    println!("[Rust] Building Query: LV95 E={:.1}..{:.1}, N={:.1}..{:.1}", min_e, max_e, min_n, max_n);

    let tile_width_m = lr_e - ul_e;
    let tile_height_m = ul_n - lr_n;

    for fgb_path in files_to_process {
        let file = File::open(&fgb_path)?;
        // Use buffered reader for FGB
        let fgb = FgbReader::open(BufReader::new(file))?; // Removed mut

        // 3. Header Check (Fast Skip)
        let header = fgb.header();
        if let Some(env) = header.envelope() {
            let e_min_x = env.get(0);
            let e_min_y = env.get(1);
            let e_max_x = env.get(2);
            let e_max_y = env.get(3);

            // Disjoint check
            if e_min_x > max_e || e_max_x < min_e || e_min_y > max_n || e_max_y < min_n {
                continue;
            }
        }

        // 4. Spatial Query
        let mut features = fgb.select_bbox(min_e, min_n, max_e, max_n)?;

        while let Ok(Some(feature)) = features.next() {
            if let Some(geo) = feature.geometry() {
                process_geometry_lv95(&geo, buffer, job.size_px, ul_e, ul_n, tile_width_m, tile_height_m);
            }
        }
    }
    Ok(())
}

fn process_geometry_lv95(
    geo: &flatgeobuf::Geometry,
    buffer: &mut [i16],
    size: u32,
    ul_e: f64, ul_n: f64,
    total_w_m: f64, total_h_m: f64
) {
    if geo.type_() != GeometryType::MultiPolygon { return; }

    let xy = match geo.xy() { Some(v) => v, None => return };
    let z_vals = match geo.z() { Some(v) => v, None => return };

    // Find max Z (Height of building)
    let mut max_z: f64 = -1000.0;
    for z in z_vals.iter() {
        if z > max_z { max_z = z; }
    }

    // AETHER stores elevation as (Meters * 2).
    let roof_val = (max_z * 2.0) as i16;
    if roof_val < -1000 { return; } // Skip invalid/underground

    let ends = geo.ends();

    // Scale Factors: Pixels per Meter
    let px_per_m_x = size as f64 / total_w_m;
    let px_per_m_y = size as f64 / total_h_m;

    // Rasterizer Closure
    let mut rasterize_ring = |stop_idx: usize, start_idx: usize| {
        let count = (stop_idx - start_idx) / 2;
        if count < 3 { return; }

        let mut vertices: Vec<(f64, f64)> = Vec::with_capacity(count);
        let mut min_x: f64 = size as f64; let mut max_x: f64 = 0.0;
        let mut min_y: f64 = size as f64; let mut max_y: f64 = 0.0;

        let mut i = start_idx;
        while i < stop_idx {
            let e = xy.get(i);
            let n = xy.get(i + 1);

            // Project World(m) -> Pixel
            let px = (e - ul_e) * px_per_m_x;
            let py = (ul_n - n) * px_per_m_y;

            if px < min_x { min_x = px; }
            if px > max_x { max_x = px; }
            if py < min_y { min_y = py; }
            if py > max_y { max_y = py; }

            vertices.push((px, py));
            i += 2;
        }

        // Bounding Box of Polygon in Pixels (Clamped to Tile)
        let start_x = min_x.floor().max(0.0) as u32;
        let end_x = max_x.ceil().min(size as f64) as u32;
        let start_y = min_y.floor().max(0.0) as u32;
        let end_y = max_y.ceil().min(size as f64) as u32;

        for y in start_y..end_y {
            let py_center = y as f64 + 0.5;
            for x in start_x..end_x {
                let px_center = x as f64 + 0.5;
                if point_in_poly(px_center, py_center, &vertices) {
                    let idx = (y * size + x) as usize;
                    if idx < buffer.len() {
                        // Max composite (Building on top of terrain)
                        if roof_val > buffer[idx] {
                            buffer[idx] = roof_val;
                        }
                    }
                }
            }
        }
    };

    if let Some(ends_vec) = ends {
        let mut start = 0;
        for end in ends_vec.iter() {
            rasterize_ring(end as usize, start);
            start = end as usize;
        }
    } else {
        rasterize_ring(xy.len(), 0);
    }
}

fn point_in_poly(x: f64, y: f64, poly: &[(f64, f64)]) -> bool {
    let mut inside = false;
    let mut j = poly.len() - 1;
    for i in 0..poly.len() {
        let (xi, yi) = poly[i];
        let (xj, yj) = poly[j];

        let intersect = ((yi > y) != (yj > y)) &&
            (x < (xj - xi) * (y - yi) / (yj - yi) + xi);

        if intersect { inside = !inside; }
        j = i;
    }
    inside
}

// --- MAIN PROCESS ---

pub fn process_tile_with_cache(
    job: IngestJob,
    cache: &mut HashMap<PathBuf, Arc<LoadedImage>>
) -> Result<()> {

    // 1. Sync Cache (Load missing TIFs)
    let missing_paths: Vec<PathBuf> = job.swiss_tifs.iter()
        .filter(|p| !cache.contains_key(*p)).cloned().collect();

    if !missing_paths.is_empty() {
        println!("[Rust] Loading {} new TIFs...", missing_paths.len());
        let results: Vec<Result<(PathBuf, Arc<LoadedImage>)>> = missing_paths
            .par_iter()
            .map(|path| { Ok((path.clone(), load_tiff_to_ram(path)?)) }).collect();

        for res in results {
            if let Ok((path, img)) = res { cache.insert(path, img); }
        }
    }

    // 2. Select Relevant Images
    let mut swiss_images: Vec<Arc<LoadedImage>> = Vec::with_capacity(job.swiss_tifs.len());
    for path in &job.swiss_tifs {
        if let Some(img) = cache.get(path) { swiss_images.push(img.clone()); }
    }
    // Simple GC
    if cache.len() > 30 { cache.clear(); }

    // 3. Setup Buffer
    let out_size = job.size_px as usize;
    let total_pixels = out_size * out_size;
    let mut buffer = vec![0i16; total_pixels];

    // FIX: Match Python's exact constant (111111.0) for pixel scaling
    let deg_per_meter = 1.0 / 111111.0;
    let pixel_deg = job.resolution_m * deg_per_meter;

    println!("[Rust] Processing Tile: {:.4}N {:.4}E | PxDeg: {:.8}", job.ul_lat, job.ul_lon, pixel_deg);

    // 4. Rasterize Terrain (Parallel)
    buffer.par_chunks_mut(out_size).enumerate().for_each(|(y, row_buffer)| {
        let row_lat = job.ul_lat - (y as f64 * pixel_deg);
        let (e_start, n_start) = wgs84_to_lv95_fast(row_lat, job.ul_lon);
        let (e_end, n_end) = wgs84_to_lv95_fast(row_lat, job.ul_lon + (out_size as f64 * pixel_deg));

        let step_e = (e_end - e_start) / (out_size as f64);
        let step_n = (n_end - n_start) / (out_size as f64);

        // Pre-filter images for this row
        let row_min_n = n_start.min(n_end) - 50.0;
        let row_max_n = n_start.max(n_end) + 50.0;
        let row_min_e = e_start.min(e_end) - 50.0;
        let row_max_e = e_start.max(e_end) + 50.0;

        let mut row_images = Vec::with_capacity(5);
        for img in &swiss_images {
            // FIX: Overlap Logic!
            // Previous code required img to be INSIDE row (impossible).
            // Correct code: Check if Image Interval overlaps Row Interval.
            // North: [img.max_n, img.origin_n] vs [row_min_n, row_max_n]
            let n_overlap = img.origin_n > row_min_n && img.max_n < row_max_n;
            // East: [img.origin_e, img.max_e] vs [row_min_e, row_max_e]
            let e_overlap = img.max_e > row_min_e && img.origin_e < row_max_e;

            if n_overlap && e_overlap {
                row_images.push(img.as_ref());
            }
        }

        for (x, out_pixel) in row_buffer.iter_mut().enumerate() {
            let e = e_start + (step_e * x as f64);
            let n = n_start + (step_n * x as f64);
            let mut found = false;

            for img in &row_images {
                // Strict check: Point (e,n) must be inside Image
                if n <= img.origin_n && n >= img.max_n && e >= img.origin_e && e < img.max_e {
                    let px = ((e - img.origin_e) / img.scale) as u32;
                    let py = ((img.origin_n - n) / img.scale) as u32;

                    if px < img.width && py < img.height {
                        unsafe { *out_pixel = *img.data.get_unchecked((py * img.width + px) as usize); }
                        found = true;
                        break;
                    }
                }
            }
            if !found {
                *out_pixel = -9999i16.saturating_mul(2);
            }
        }
    });

    // 5. Rasterize Buildings
    if let Some(fgb) = &job.buildings_file {
        if let Err(e) = apply_buildings(&job, &mut buffer, fgb, pixel_deg) {
            eprintln!("Warning: Failed to apply buildings for tile {:?}: {}", job.output_path, e);
        }
    }

    // 6. Write Output
    let f = File::create(&job.output_path)?;
    let mut w = BufWriter::with_capacity(1024 * 1024, f);
    w.write_all(b"AETH")?;
    w.write_u16::<LittleEndian>(1)?;
    w.write_u16::<LittleEndian>(job.size_px as u16)?;
    w.write_f64::<LittleEndian>(job.ul_lat)?;
    w.write_f64::<LittleEndian>(job.ul_lon)?;
    w.write_f64::<LittleEndian>(pixel_deg)?;
    w.write_f64::<LittleEndian>(pixel_deg)?;
    w.write_i16::<LittleEndian>(0)?;
    w.write_u16::<LittleEndian>(0)?;

    let bytes_ptr = buffer.as_ptr() as *const u8;
    let bytes_len = buffer.len() * 2;
    let bytes_slice = unsafe { std::slice::from_raw_parts(bytes_ptr, bytes_len) };
    w.write_all(bytes_slice)?;

    Ok(())
}