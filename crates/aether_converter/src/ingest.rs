use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{BufWriter, Write, BufReader};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use serde::Deserialize;
use rayon::prelude::*;
use byteorder::{LittleEndian, WriteBytesExt};
use tiff::decoder::{Decoder, DecodingResult};
use tiff::tags::Tag;
use anyhow::{Context, Result};
use flatgeobuf::{FgbReader, GeometryType};
use fallible_streaming_iterator::FallibleStreamingIterator;

#[derive(Deserialize, Debug, Clone)]
pub struct IngestJob {
    pub output_path: PathBuf,
    pub ul_lat: f64,
    pub ul_lon: f64,
    pub resolution_m: f64,
    pub size_px: u32,
    pub base_tif: Option<PathBuf>,
    pub swiss_tifs: Vec<PathBuf>,
    pub buildings_file: Option<PathBuf>,
}

pub struct LoadedImage {
    pub width: u32,
    pub height: u32,
    pub data: Vec<i16>,
    pub origin_e: f64,
    pub origin_n: f64,
    pub limit_e: f64,
    pub limit_n: f64,
    pub scale: f64,
    pub name: String,
}

#[inline(always)]
fn wgs84_to_lv95_fast(lat: f64, lon: f64) -> (f64, f64) {
    let phi = (lat * 3600.0 - 169028.66) / 10000.0;
    let lam = (lon * 3600.0 - 26782.5) / 10000.0;
    let e = 2600072.37 + 211455.93 * lam - 10938.51 * lam * phi - 0.36 * lam * phi * phi - 44.54 * lam * lam * lam;
    let n = 1200147.07 + 308807.95 * phi + 3745.25 * lam * lam + 76.63 * phi * phi - 194.56 * lam * lam * phi + 119.79 * phi * phi * phi;
    (e, n)
}

fn parse_swiss_filename(p: &Path) -> Option<(f64, f64)> {
    let name = p.file_name()?.to_string_lossy();
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

    let model_trans = decoder.get_tag_f64_vec(Tag::ModelTransformationTag).unwrap_or_default();
    let tiepoints = decoder.get_tag_f64_vec(Tag::ModelTiepointTag).unwrap_or_default();
    let pixel_scales = decoder.get_tag_f64_vec(Tag::ModelPixelScaleTag).unwrap_or_default();

    let (origin_e, origin_n, scale) = if model_trans.len() == 16 {
        (model_trans[3], model_trans[7], model_trans[0].abs())
    } else if tiepoints.len() >= 6 && pixel_scales.len() >= 2 {
        (tiepoints[3], tiepoints[4], pixel_scales[0])
    } else {
        let (e, n) = parse_swiss_filename(path).unwrap_or((0.0, 0.0));
        let est_scale = if w > 0 { 1000.0 / w as f64 } else { 0.5 };
        (e, n + 1000.0, est_scale)
    };

    let result = decoder.read_image()?;
    let data: Vec<i16> = match result {
        DecodingResult::F32(v) => v.iter().map(|&x| (x * 2.0) as i16).collect(),
        DecodingResult::I16(v) => v.iter().map(|&x| x.saturating_mul(2)).collect(),
        DecodingResult::I32(v) => v.iter().map(|&x| (x as i16).saturating_mul(2)).collect(),
        _ => return Err(anyhow::anyhow!("Unsupported TIF format")),
    };

    let limit_n = origin_n - (h as f64 * scale);
    let limit_e = origin_e + (w as f64 * scale);

    Ok(Arc::new(LoadedImage {
        width: w, height: h, data,
        origin_e, origin_n, limit_e, limit_n,
        scale,
        name: path.file_name().unwrap_or_default().to_string_lossy().to_string(),
    }))
}

pub fn process_tile_with_cache(
    job: IngestJob,
    cache: &mut HashMap<PathBuf, Arc<LoadedImage>>
) -> Result<()> {

    let mut missing: Vec<PathBuf> = job.swiss_tifs.iter()
        .filter(|p| !cache.contains_key(*p)).cloned().collect();

    if let Some(base_path) = &job.base_tif {
        if !cache.contains_key(base_path) {
            missing.push(base_path.clone());
        }
    }

    if !missing.is_empty() {
        let loaded: Vec<_> = missing.par_iter()
            .map(|p| (p.clone(), load_tiff_to_ram(p))).collect();
        for (p, res) in loaded {
            match res {
                Ok(img) => { cache.insert(p, img); },
                Err(e) => println!("[Warn] Failed to load {:?}: {}", p, e),
            }
        }
    }

    let mut swiss_images = Vec::new();
    for p in &job.swiss_tifs {
        if let Some(img) = cache.get(p) { swiss_images.push(img.clone()); }
    }

    let base_image = job.base_tif.as_ref().and_then(|p| cache.get(p).cloned());
    let base_image_ref = base_image.as_deref();

    let deg_per_meter = 1.0 / 111111.0;
    let pixel_deg = job.resolution_m * deg_per_meter;
    let out_size = job.size_px as usize;
    let total_pixels = out_size * out_size;
    let mut buffer = vec![0i16; total_pixels];

    // Terrain Rasterization
    buffer.par_chunks_mut(out_size).enumerate().for_each(|(y, row_buffer)| {
        let row_lat = job.ul_lat - (y as f64 * pixel_deg);

        // Base Map Y-axis Fast Intersect
        let mut base_row_valid = false;
        let mut base_py = 0;
        if let Some(base) = base_image_ref {
            if row_lat <= base.origin_n && row_lat >= base.limit_n {
                let py_f = (base.origin_n - row_lat) / base.scale;
                if py_f >= 0.0 {
                    let py = py_f as u32;
                    if py < base.height {
                        base_row_valid = true;
                        base_py = py;
                    }
                }
            }
        }

        let (e_start, n_start) = wgs84_to_lv95_fast(row_lat, job.ul_lon);
        let (e_end, n_end) = wgs84_to_lv95_fast(row_lat, job.ul_lon + (out_size as f64 * pixel_deg));

        let step_e = (e_end - e_start) / out_size as f64;
        let step_n = (n_end - n_start) / out_size as f64;

        let row_min_n = n_start.min(n_end);
        let row_max_n = n_start.max(n_end);
        let row_min_e = e_start.min(e_end);
        let row_max_e = e_start.max(e_end);

        let mut row_images = Vec::with_capacity(5);
        for img in &swiss_images {
            if img.origin_n >= row_min_n && img.limit_n <= row_max_n &&
                img.limit_e >= row_min_e && img.origin_e <= row_max_e {
                row_images.push(img.as_ref());
            }
        }

        for (x, out_pixel) in row_buffer.iter_mut().enumerate() {
            let e = e_start + (step_e * x as f64);
            let n = n_start + (step_n * x as f64);
            let mut val = -9999i16;

            for img in &row_images {
                if n <= img.origin_n && n >= img.limit_n && e >= img.origin_e && e < img.limit_e {
                    let px = ((e - img.origin_e) / img.scale) as u32;
                    let py = ((img.origin_n - n) / img.scale) as u32;
                    if px < img.width && py < img.height {
                        let v = unsafe { *img.data.get_unchecked((py * img.width + px) as usize) };
                        if v > -5000 {
                            val = v;
                            break;
                        }
                    }
                }
            }

            if val <= -5000 && base_row_valid {
                let base = base_image_ref.unwrap();
                let pixel_lon = job.ul_lon + (x as f64 * pixel_deg);

                if pixel_lon >= base.origin_e && pixel_lon < base.limit_e {
                    let px_f = (pixel_lon - base.origin_e) / base.scale;
                    if px_f >= 0.0 {
                        let px = px_f as u32;
                        if px < base.width {
                            let v = unsafe { *base.data.get_unchecked((base_py * base.width + px) as usize) };
                            if v > -5000 {
                                val = v;
                            }
                        }
                    }
                }
            }

            *out_pixel = val;
        }
    });

    // Building Rasterization
    if let Some(fgb) = &job.buildings_file {
        if let Err(e) = apply_buildings(&job, &mut buffer, fgb, pixel_deg) {
            println!("[Warn] Failed to apply buildings: {}", e);
        }
    }

    let f = File::create(&job.output_path)?;
    let mut w = BufWriter::with_capacity(1024 * 1024, f);

    let bytes_per_row = job.size_px as u32 * 2;
    let aligned_stride = (bytes_per_row + 255) & !255;
    let padding_bytes = aligned_stride - bytes_per_row;

    w.write_all(b"AETH")?;
    w.write_u16::<LittleEndian>(1)?;
    w.write_u16::<LittleEndian>(job.size_px as u16)?;
    w.write_f64::<LittleEndian>(job.ul_lat)?;
    w.write_f64::<LittleEndian>(job.ul_lon)?;
    w.write_f64::<LittleEndian>(pixel_deg)?;
    w.write_f64::<LittleEndian>(pixel_deg)?;
    w.write_i16::<LittleEndian>(0)?;
    w.write_u16::<LittleEndian>(aligned_stride as u16)?;

    let padding_buf = vec![0u8; padding_bytes as usize];
    for chunk in buffer.chunks(job.size_px as usize) {
        let ptr = chunk.as_ptr() as *const u8;
        let len = chunk.len() * 2;
        let slice = unsafe { std::slice::from_raw_parts(ptr, len) };
        w.write_all(slice)?;
        if padding_bytes > 0 {
            w.write_all(&padding_buf)?;
        }
    }

    Ok(())
}

fn apply_buildings(job: &IngestJob, buffer: &mut [i16], fgb_target: &Path, px_deg: f64) -> Result<()> {
    let mut files_to_process = Vec::new();
    if fgb_target.is_dir() {
        if let Ok(entries) = fs::read_dir(fgb_target) {
            for entry in entries.flatten() {
                if entry.path().extension().map_or(false, |ext| ext == "fgb") {
                    files_to_process.push(entry.path());
                }
            }
        }
    } else {
        files_to_process.push(fgb_target.to_path_buf());
    }

    // --- MINIMAL FIX: Search Bounding Box is now strictly WGS84 ---
    let lr_lat = job.ul_lat - (job.size_px as f64 * px_deg);
    let lr_lon = job.ul_lon + (job.size_px as f64 * px_deg);

    // Approx 20 meter padding in degrees
    let pad_deg = 20.0 / 111111.0;

    let min_lon = job.ul_lon.min(lr_lon) - pad_deg;
    let max_lon = job.ul_lon.max(lr_lon) + pad_deg;
    let min_lat = job.ul_lat.min(lr_lat) - pad_deg;
    let max_lat = job.ul_lat.max(lr_lat) + pad_deg;

    let mut total_pixels_mod = 0;
    let mut total_features = 0;

    for fgb_path in files_to_process {
        let file = File::open(&fgb_path)?;
        let fgb = FgbReader::open(BufReader::new(file))?;

        if let Some(env) = fgb.header().envelope() {
            // env is[min_x, min_y, max_x, max_y]
            if env.get(0) > max_lon || env.get(2) < min_lon || env.get(1) > max_lat || env.get(3) < min_lat {
                continue;
            }
        }

        let mut features = fgb.select_bbox(min_lon, min_lat, max_lon, max_lat)?;
        while let Some(feature) = features.next()? {
            if let Some(geo) = feature.geometry() {
                let g_type = geo.type_();
                if g_type == GeometryType::MultiPolygon || g_type == GeometryType::Polygon {
                    total_features += 1;
                    total_pixels_mod += process_geometry_wgs84(
                        &geo, buffer, job.size_px,
                        job.ul_lon, job.ul_lat, px_deg
                    );
                }
            }
        }
    }

    if total_features > 0 {
        println!("[Build] Scanned {} features. Modified {} pixels.", total_features, total_pixels_mod);
    }
    Ok(())
}

fn process_geometry_wgs84(
    geo: &flatgeobuf::Geometry, buffer: &mut [i16], size: u32,
    ul_lon: f64, ul_lat: f64, px_deg: f64
) -> usize {
    if let Some(parts) = geo.parts() {
        if parts.len() > 0 {
            let mut total_modified = 0;
            for i in 0..parts.len() {
                let part = parts.get(i);
                total_modified += process_geometry_wgs84(&part, buffer, size, ul_lon, ul_lat, px_deg);
            }
            return total_modified;
        }
    }

    let xy = match geo.xy() { Some(v) => v, None => return 0 };
    let z_vals = match geo.z() { Some(v) => v, None => return 0 };

    let mut max_z: f64 = -1000.0;
    for z in z_vals {
        if z > max_z { max_z = z; }
    }

    let roof_val = (max_z * 2.0) as i16;
    if roof_val < 0 { return 0; }

    let mut pixels_modified = 0;

    let mut rasterize_ring = |stop_idx: usize, start_idx: usize| {
        let count = (stop_idx - start_idx) / 2;
        if count < 3 { return; }

        let mut vertices: Vec<(f64, f64)> = Vec::with_capacity(count);
        let mut min_x = size as f64; let mut max_x = 0.0;
        let mut min_y = size as f64; let mut max_y = 0.0;

        let mut i = start_idx;
        while i < stop_idx {
            // --- MINIMAL FIX: Directly map Longitude and Latitude ---
            let lon = xy.get(i);
            let lat = xy.get(i + 1);

            let px = (lon - ul_lon) / px_deg;
            let py = (ul_lat - lat) / px_deg;

            if px < min_x { min_x = px; }
            if px > max_x { max_x = px; }
            if py < min_y { min_y = py; }
            if py > max_y { max_y = py; }
            vertices.push((px, py));
            i += 2;
        }

        let start_x = min_x.floor().max(0.0) as u32;
        let end_x = max_x.ceil().min(size as f64) as u32;
        let start_y = min_y.floor().max(0.0) as u32;
        let end_y = max_y.ceil().min(size as f64) as u32;

        for y in start_y..end_y {
            let py_center = y as f64 + 0.5;
            for x in start_x..end_x {
                if point_in_poly(x as f64 + 0.5, py_center, &vertices) {
                    let idx = (y * size + x) as usize;
                    if idx < buffer.len() {
                        let current_h = buffer[idx];
                        if roof_val > current_h {
                            buffer[idx] = roof_val;
                            pixels_modified += 1;
                        }
                    }
                }
            }
        }
    };

    if let Some(ends_vec) = geo.ends() {
        let mut start = 0;
        for end in ends_vec {
            rasterize_ring(end as usize, start);
            start = end as usize;
        }
    } else {
        rasterize_ring(xy.len(), 0);
    }

    pixels_modified
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