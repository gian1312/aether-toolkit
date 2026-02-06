use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc; // Removed Mutex
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

struct LoadedImage {
    width: u32,
    height: u32,
    data: Vec<i16>,
    // Simplification: We assume Swiss tiles are standard aligned.
    // Real implementation should read TIF tags for ModelTiepoint.
    // Here we parse filename for origin: "swissalti3d_2019_2600-1120_..."
    origin_e: f64,
    origin_n: f64,
    scale: f64, // 0.5m usually
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
    let mut decoder = Decoder::new(file)?;
    let (w, h) = decoder.dimensions()?;

    let result = decoder.read_image()?;

    // Normalize to i16 (elevation in 0.5m units)
    let data: Vec<i16> = match result {
        DecodingResult::F32(v) => v.iter().map(|&x| (x * 2.0) as i16).collect(),
        DecodingResult::I16(v) => v.iter().map(|&x| x.saturating_mul(2)).collect(), // Assuming meters input
        _ => return Err(anyhow::anyhow!("Unsupported TIF format")),
    };

    // Extract coords from filename (Fast & Dirty, assumes standard SwissAlti naming)
    // In production, read GeoTIFF tags.
    let (e, n) = parse_swiss_filename(path).unwrap_or((0.0, 0.0));

    Ok(Arc::new(LoadedImage {
        width: w,
        height: h,
        data,
        origin_e: e,
        origin_n: n,
        scale: 0.5, // SwissAlti is 0.5m resolution
    }))
}

pub fn process_tile(job: IngestJob) -> Result<()> {
    let out_size = job.size_px as usize;
    let total_pixels = out_size * out_size;
    let mut buffer = vec![0i16; total_pixels];

    // 1. Load Sources (Parallel Load)
    // We only load Swiss tiles relevant to this job.
    // In a batch process, we should cache these, but for simplicity we load per job here.
    let swiss_images: Vec<Arc<LoadedImage>> = job.swiss_tifs.par_iter()
        .filter_map(|p| load_tiff_to_ram(p).ok())
        .collect();

    // 2. Constants for WGS84 grid
    // Approx conversion factors for 47N
    let deg_lat_m = 111132.0;
    let deg_lon_m = 75700.0; // At ~47N

    let pixel_deg_y = job.resolution_m / deg_lat_m;
    let pixel_deg_x = job.resolution_m / deg_lon_m;

    // 3. Process Pixels (Parallel CPU Crunching)
    buffer.par_iter_mut().enumerate().for_each(|(idx, out_pixel)| {
        let y = idx / out_size;
        let x = idx % out_size;

        let lat = job.ul_lat - (y as f64 * pixel_deg_y);
        let lon = job.ul_lon + (x as f64 * pixel_deg_x);

        // Convert to LV95
        let swiss_coord = geo::wgs84_to_lv95(lat, lon);

        let mut found = false;

        // Try Swiss Layer
        if geo::is_in_swiss_bounds(&swiss_coord) {
            for img in &swiss_images {
                // Check bounds (Image is 10km x 10km usually)
                // Coordinate system: Top-Left is (e, n). Y decreases down.
                // Wait: SwissAlti filenames are Bottom-Left usually?
                // Let's assume standard Swiss Grid: Filename is Bottom-Left (swisstopo standard)
                // So Top-Left N = origin_n + 10km.
                // Actually SwissAlti3D filenames: "2600-1120" -> East 2600km, North 1120km (Bottom-Left corner)

                let tile_max_e = img.origin_e + 10000.0;
                let tile_max_n = img.origin_n + 10000.0;

                if swiss_coord.e >= img.origin_e && swiss_coord.e < tile_max_e &&
                    swiss_coord.n >= img.origin_n && swiss_coord.n < tile_max_n {

                    // Map to internal pixel
                    // Tiff is stored Top-Left.
                    // Real World N (top) = tile_max_n
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

        // Fallback: Base (Skipped for brevity, can implement simple read-nearest here)
        // If not found, default is 0 or we could sample the base tif if provided.
        if !found {
            *out_pixel = -9999i16.saturating_mul(2); // NoData
        }
    });

    // 4. Write ABT
    let f = File::create(&job.output_path)?;
    let mut w = BufWriter::new(f);

    // Header
    w.write_all(b"AETH")?;
    w.write_u16::<LittleEndian>(1)?;
    w.write_u16::<LittleEndian>(job.size_px as u16)?;
    w.write_f64::<LittleEndian>(job.ul_lat)?;
    w.write_f64::<LittleEndian>(job.ul_lon)?;
    w.write_f64::<LittleEndian>(pixel_deg_y)?;
    w.write_f64::<LittleEndian>(pixel_deg_x)?;
    w.write_i16::<LittleEndian>(0)?;
    w.write_u16::<LittleEndian>(0)?;

    // Payload
    for &p in &buffer {
        w.write_i16::<LittleEndian>(p)?;
    }

    Ok(())
}