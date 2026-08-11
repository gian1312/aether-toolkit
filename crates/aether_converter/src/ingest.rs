// rust/aether_converter/src/ingest.rs
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{BufWriter, Write, BufReader};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use serde::Deserialize;
#[cfg(feature = "native")]
use rayon::prelude::*;
use byteorder::{LittleEndian, WriteBytesExt};
use tiff::decoder::{Decoder, DecodingResult, Limits};
use tiff::tags::Tag;
use anyhow::{Context, Result};
use flatgeobuf::{FgbReader, GeometryType};
use crate::buildings::{
    load_pbf_building_dir, rasterize_buildings, Building, BuildingHeight, HeightSource, I16Grid,
    PbfBuildingSet, RasterOpts, Rounding, TileRef,
};
use fallible_streaming_iterator::FallibleStreamingIterator;
#[cfg(feature = "bc6h")]
use image_dds::{SurfaceRgba32Float, ImageFormat, Mipmaps, Quality};

#[derive(Deserialize, Debug, Clone)]
pub struct IngestJob {
    pub output_path: PathBuf,
    pub format: Option<String>, // "bc6h" or "r16sint"
    pub ul_lat: f64,
    pub ul_lon: f64,
    pub resolution_m: f64,
    pub size_px: u32,
    pub base_tif: Option<PathBuf>,
    pub swiss_tifs: Vec<PathBuf>,
    pub buildings_file: Option<PathBuf>,
    /// Directory of Mapbox-Vector-Tile building tiles named `{z}_{x}_{y}.pbf`
    /// (gzipped or not), e.g. an OpenFreeMap planet fetch.
    ///
    /// Optional and absent from older jobs, so existing callers are unaffected.
    /// Unlike `buildings_file`, these carry a height *above ground*, which the
    /// rasterizer resolves against the terrain under each footprint.
    #[serde(default)]
    pub buildings_pbf_dir: Option<PathBuf>,
}

/// The value written for a pixel with no terrain under it.
///
/// Elevations are stored in **half-metres**, so this is -4999.5 m — far below
/// any real ground, and below the `-5000` half-metre (-2500 m) floor the
/// samplers in this file use to tell "no data" from "very low ground". A void
/// must never be confused with 0 (sea level): a reader that treats it as ground
/// gets flat terrain at mean sea level instead of a hole it can fill from
/// another source.
pub const VOID_ELEV: i16 = -9999;

/// Convert one Float32 DEM sample to the `.abt` half-metre unit.
///
/// `f32 as i16` is defined to produce **0** for NaN, and NaN is GDAL's default
/// Float32 nodata — so a Float32 DEM's voids used to arrive as 0 half-metres,
/// pass the `> -5000` validity test, and get written as sea level. Non-finite
/// samples now become the void sentinel instead. Finite samples are unchanged:
/// the cast already truncates toward zero and saturates at the i16 bounds.
#[inline]
pub fn f32_sample_to_half_metres(x: f32) -> i16 {
    if !x.is_finite() {
        return VOID_ELEV;
    }
    (x * 2.0) as i16
}

/// Convert one Int32 DEM sample to the `.abt` half-metre unit.
///
/// The old `(x as i16).saturating_mul(2)` narrowed **before** the multiply, so
/// the cast kept only the low 16 bits: the classic Int32 nodata `i32::MIN` has
/// them all zero and arrived as 0 half-metres — sea level again. Widen first,
/// then saturate over the full range, and map anything that saturates low to
/// the void sentinel.
#[inline]
pub fn i32_sample_to_half_metres(x: i32) -> i16 {
    let half_metres = x as i64 * 2;
    if half_metres < VOID_ELEV as i64 {
        VOID_ELEV
    } else if half_metres > i16::MAX as i64 {
        i16::MAX
    } else {
        half_metres as i16
    }
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

/// Nudge that decides a source-pixel centre landing exactly on a cell edge.
///
/// Grids that line up exactly are the common case — the plugin warps its base
/// DEM onto the analysis grid — and then every cell edge falls on a pixel
/// centre, where a 1-ulp difference in the coordinate arithmetic would decide
/// which cell owns the pixel. This resolves it the same way every time (the
/// low edge belongs to the cell above, the high edge to this one), which keeps
/// the spans tiling and keeps a 3-to-1 average symmetric about its cell instead
/// of leaning half a pixel whichever way the last rounding went. A billionth of
/// a pixel is far below any real geometry and far above the arithmetic's noise.
const SPAN_EPS: f64 = 1e-9;

/// The half-open source-pixel span whose **centres** fall inside one output
/// cell, along one axis.
///
/// `lo`/`hi` are the cell's leading and trailing edges in source-pixel units
/// (`lo < hi`) and `dim` is the source raster's extent on that axis. Source
/// pixel `i` covers `[i, i+1)` with its centre at `i + 0.5`, so the cell
/// `[lo, hi)` owns the indices `i` with `lo <= i + 0.5 < hi`, clipped to the
/// raster. That is `round(lo) .. round(hi)`, with [`SPAN_EPS`] settling an edge
/// that lands exactly on a pixel centre. (`(v + 0.5) as u32` truncates and
/// saturates, so it costs one instruction and maps negatives to 0; `f64::ceil`
/// would lower to a libm call on a baseline x86-64 target, twice per axis per
/// output pixel.)
///
/// **Upsampling degrades to the old behaviour.** When the output cell is finer
/// than a source pixel the span can be empty — no source centre lands in it.
/// Rather than divide by zero or leave the cell void it falls back to the pixel
/// containing the cell's *centre*, and callers centre the cell on the point the
/// old code sampled, so an upsampling job keeps its exact bytes. It is also the
/// continuous limit of the span rule: at ratio 1 the one pixel whose centre is
/// inside the cell is the one containing the cell's centre, so nothing jumps
/// half a pixel as the ratio crosses 1.
#[inline]
fn sample_span(lo: f64, hi: f64, dim: u32) -> (u32, u32) {
    if dim == 0 {
        return (0, 0);
    }
    let start = (lo + 0.5 + SPAN_EPS) as u32;
    let end = ((hi + 0.5 + SPAN_EPS) as u32).min(dim);
    if start < end {
        (start, end)
    } else {
        // `as u32` truncates and saturates, so a negative midpoint lands on 0.
        let p = (((lo + hi) * 0.5) as u32).min(dim - 1);
        (p, p + 1)
    }
}

/// Sample one output cell out of one source image: the area-average of the
/// source pixels its footprint covers, in half-metres.
///
/// A cell covering exactly one source pixel — every cell of a source at or
/// coarser than the target, which is the whole of the warped-base-DEM path —
/// reads that pixel straight, with none of the accumulator's machinery, so the
/// common one-sample case costs what the old point sample cost.
#[inline(always)]
fn sample_cell(img: &LoadedImage, px0: u32, px1: u32, py0: u32, py1: u32) -> Option<i16> {
    let w = img.width as usize;
    let a = px0 as usize;
    if px1 - px0 == 1 && py1 - py0 == 1 {
        let v = img.data[py0 as usize * w + a];
        return if v > -5000 { Some(v) } else { None };
    }
    mean_valid(&img.data, w, a, px1 as usize, py0 as usize, py1 as usize)
}

/// Mean of the **valid** samples of one source-pixel rectangle, in half-metres.
///
/// Voids are excluded from the mean, never averaged into it: a cell beside a
/// coastline or a DEM edge must not be dragged toward -4999.5 m by the no-data
/// pixels next to it. A rectangle holding no valid sample at all returns `None`,
/// and the caller falls through to its next source exactly as a void point
/// sample used to.
///
/// The inner pass is branchless and sums into an `i32`, which is what makes it
/// vectorize — an `i64` accumulator measured 44% slower on a 60×60 footprint.
/// `32768 * 32768` is `i32::MAX + 1`, so the run is split at 32768 samples and
/// widened into `i64` between runs; no real DEM has a row that long, but it
/// splits rather than wraps if one ever does. The division rounds once at the
/// end — accumulating in `i16` would overflow after two mountain pixels.
#[inline]
fn mean_valid(data: &[i16], w: usize, a: usize, b: usize, y0: usize, y1: usize) -> Option<i16> {
    let mut sum: i64 = 0;
    let mut cnt: u32 = 0;
    for py in y0..y1 {
        let row = py * w;
        for run in data[row + a..row + b].chunks(1 << 15) {
            let mut rsum: i32 = 0;
            let mut rcnt: u32 = 0;
            for &v in run {
                let ok = v > -5000;
                rsum += if ok { v as i32 } else { 0 };
                rcnt += ok as u32;
            }
            sum += rsum as i64;
            cnt += rcnt;
        }
    }
    match cnt {
        0 => None,
        // Single-sample cells keep the exact source value — no divide, and no
        // rounding drift on the upsampling path.
        1 => Some(sum as i16),
        // Round half away from zero, integer-only.
        _ => {
            let c = cnt as i64;
            let r = if sum >= 0 {
                (2 * sum + c) / (2 * c)
            } else {
                (2 * sum - c) / (2 * c)
            };
            Some(r as i16)
        }
    }
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

// FIX: Made public so main.rs can access it for sequential pre-loading
pub fn load_tiff_to_ram(path: &Path) -> Result<Arc<LoadedImage>> {
    let file = File::open(path).with_context(|| format!("Opening {:?}", path))?;
    let reader = BufReader::with_capacity(1024 * 1024, file);
    // The default tiff decode-buffer cap (~256 MB) rejects large rasters with
    // "The Decoder limits are exceeded". Callers now hand us one small,
    // per-tile GeoTIFF at a time (bounded by the plugin's warp size), so lift
    // the limit and let the tile decode rather than silently failing.
    let mut decoder = Decoder::new(reader)?.with_limits(Limits::unlimited());
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
        DecodingResult::F32(v) => v.iter().map(|&x| f32_sample_to_half_metres(x)).collect(),
        DecodingResult::I16(v) => v.iter().map(|&x| x.saturating_mul(2)).collect(),
        DecodingResult::I32(v) => v.iter().map(|&x| i32_sample_to_half_metres(x)).collect(),
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

/// Convert one tile, decoding this job's `buildings_pbf_dir` (if any) for it.
///
/// Convenient for a one-tile run. A batch must not use this: the PBF decode is
/// a whole-directory scan whose result is the same for every output tile, so
/// doing it here runs it once per `.abt`. Batch callers load the set once with
/// [`load_pbf_building_dir`] and call [`process_tile`].
pub fn process_tile_with_cache(
    job: IngestJob,
    cache_arc: Arc<std::sync::Mutex<HashMap<PathBuf, Arc<LoadedImage>>>>
) -> Result<()> {
    let pbf_buildings = match &job.buildings_pbf_dir {
        Some(dir) => Some(load_pbf_building_dir(dir)?),
        None => None,
    };
    process_tile(job, cache_arc, pbf_buildings.as_ref())
}

/// Convert one tile, drawing *pbf_buildings* (decoded once per run) onto it.
///
/// *pbf_buildings* must be `Some` whenever the job carries a
/// `buildings_pbf_dir`; passing `None` for such a job is a caller bug and is
/// refused rather than quietly producing a building-less tile.
pub fn process_tile(
    job: IngestJob,
    cache_arc: Arc<std::sync::Mutex<HashMap<PathBuf, Arc<LoadedImage>>>>,
    pbf_buildings: Option<&PbfBuildingSet>,
) -> Result<()> {

    // 1. Identify missing files inside a lock
    let mut missing = Vec::new();
    {
        let cache = cache_arc.lock().unwrap();
        for p in &job.swiss_tifs {
            if !cache.contains_key(p) { missing.push(p.clone()); }
        }
        if let Some(base_path) = &job.base_tif {
            if !cache.contains_key(base_path) { missing.push(base_path.clone()); }
        }
    }

    // 2. Load missing files sequentially to prevent RAM spikes (OOM fix)
    if !missing.is_empty() {
        let loaded: Vec<_> = missing.into_iter()
            .map(|p| (p.clone(), load_tiff_to_ram(&p))).collect();

        let mut cache = cache_arc.lock().unwrap();
        for (p, res) in loaded {
            match res {
                Ok(img) => { cache.insert(p, img); },
                Err(e) => println!("[Warn] Failed to load {:?}: {}", p, e),
            }
        }
    }

    // 3. Extract required images from cache
    let mut swiss_images = Vec::new();
    let mut base_image = None;
    {
        let cache = cache_arc.lock().unwrap();
        for p in &job.swiss_tifs {
            if let Some(img) = cache.get(p) { swiss_images.push(img.clone()); }
        }
        if let Some(p) = &job.base_tif {
            base_image = cache.get(p).cloned();
        }
    }
    // A base DEM that was requested but could not be loaded must be fatal:
    // otherwise the tile is written with no terrain (flat 0) and the whole run
    // reports success with empty coverage. (The classic cause was the source
    // exceeding the tiff decoder limit — see the earlier [Warn].)
    if job.base_tif.is_some() && base_image.is_none() {
        anyhow::bail!(
            "base DEM {:?} was specified but could not be loaded; refusing to \
             write a terrain-less tile",
            job.base_tif
        );
    }
    let base_image_ref = base_image.as_deref();

    let deg_per_meter = 1.0 / 111111.0;
    let pixel_deg = job.resolution_m * deg_per_meter;
    let out_size = job.size_px as usize;
    let total_pixels = out_size * out_size;
    let mut buffer = vec![0i16; total_pixels];

    // Per-column source spans for the base DEM, computed once for the whole
    // tile: the base is axis-aligned in degrees, so a column's footprint is the
    // same on every row. `(0, 0)` means the column falls outside the base.
    let base_x_spans: Vec<(u32, u32)> = match base_image_ref {
        Some(base) => {
            let half = 0.5 * pixel_deg / base.scale;
            (0..out_size)
                .map(|x| {
                    let pixel_lon = job.ul_lon + (x as f64 * pixel_deg);
                    if pixel_lon >= base.origin_e && pixel_lon < base.limit_e {
                        let px_f = (pixel_lon - base.origin_e) / base.scale;
                        if px_f >= 0.0 && (px_f as u32) < base.width {
                            return sample_span(px_f - half, px_f + half, base.width);
                        }
                    }
                    (0, 0)
                })
                .collect()
        }
        None => Vec::new(),
    };

    // Terrain Rasterization
    #[cfg(feature = "native")]
    let iter = buffer.par_chunks_mut(out_size);
    #[cfg(not(feature = "native"))]
    let iter = buffer.chunks_mut(out_size);
    iter.enumerate().for_each(|(y, row_buffer)| {
        let row_lat = job.ul_lat - (y as f64 * pixel_deg);

        let mut base_row_valid = false;
        let mut base_py = (0u32, 0u32);
        if let Some(base) = base_image_ref {
            if row_lat <= base.origin_n && row_lat >= base.limit_n {
                let py_f = (base.origin_n - row_lat) / base.scale;
                if py_f >= 0.0 {
                    let py = py_f as u32;
                    if py < base.height {
                        base_row_valid = true;
                        // The cell's own row band, hoisted: it is the same for
                        // every pixel of this output row.
                        let half = 0.5 * pixel_deg / base.scale;
                        base_py = sample_span(py_f - half, py_f + half, base.height);
                    }
                }
            }
        }

        let (e_start, n_start) = wgs84_to_lv95_fast(row_lat, job.ul_lon);
        let (e_end, n_end) = wgs84_to_lv95_fast(row_lat, job.ul_lon + (out_size as f64 * pixel_deg));
        // The same two points one output row further down: the cell's south
        // edge. Area-averaging needs the cell's footprint, not just the corner
        // the old point sample read, and the LV95 northing of a WGS84 parallel
        // drifts along the row, so the south edge needs its own row.
        let (_, n_lo_start) = wgs84_to_lv95_fast(row_lat - pixel_deg, job.ul_lon);
        let (_, n_lo_end) =
            wgs84_to_lv95_fast(row_lat - pixel_deg, job.ul_lon + (out_size as f64 * pixel_deg));

        let step_e = (e_end - e_start) / out_size as f64;
        let step_n = (n_end - n_start) / out_size as f64;
        let step_n_lo = (n_lo_end - n_lo_start) / out_size as f64;
        let half_step_e = step_e * 0.5;

        let row_min_n = n_start.min(n_end);
        let row_max_n = n_start.max(n_end);
        let row_min_e = e_start.min(e_end);
        let row_max_e = e_start.max(e_end);

        // Carries 1/scale so the per-pixel footprint maths is multiplies, not a
        // division per pixel per candidate image.
        let mut row_images = Vec::with_capacity(5);
        for img in &swiss_images {
            if img.origin_n >= row_min_n && img.limit_n <= row_max_n &&
                img.limit_e >= row_min_e && img.origin_e <= row_max_e {
                row_images.push((img.as_ref(), 1.0 / img.scale));
            }
        }

        for (x, out_pixel) in row_buffer.iter_mut().enumerate() {
            let e = e_start + (step_e * x as f64);
            let n = n_start + (step_n * x as f64);
            let mut val = VOID_ELEV;

            for &(img, inv) in &row_images {
                // The cell this output pixel stands for, centred on the point
                // the old code point-sampled: one output pixel wide and one
                // output row tall. Centring it there is what keeps a source at
                // or below the target resolution on the pixel it already used —
                // the average is taken *around* the old sample, never offset
                // from it. (Inside the loop so a tile with no candidate image
                // under this row does not pay for it.)
                let e_lo = e - half_step_e;
                let e_hi = e + half_step_e;
                let half_n = (n - (n_lo_start + step_n_lo * x as f64)) * 0.5;
                let n_hi = n + half_n;
                let n_lo = n - half_n;
                if n <= img.origin_n && n >= img.limit_n && e >= img.origin_e && e < img.limit_e {
                    let (px0, px1) =
                        sample_span((e_lo - img.origin_e) * inv, (e_hi - img.origin_e) * inv, img.width);
                    let (py0, py1) =
                        sample_span((img.origin_n - n_hi) * inv, (img.origin_n - n_lo) * inv, img.height);
                    // First image that has real ground under the cell wins, as
                    // before. The footprint is clipped to that one image, so a
                    // cell straddling two source tiles averages the part inside
                    // the tile its corner landed in — still an average of real
                    // terrain, and still better than the single pixel it took
                    // before.
                    if let Some(v) = sample_cell(img, px0, px1, py0, py1) {
                        val = v;
                        break;
                    }
                }
            }

            if val <= -5000 && base_row_valid {
                let base = base_image_ref.unwrap();
                let (px0, px1) = base_x_spans[x];
                if px0 < px1 {
                    if let Some(v) = sample_cell(base, px0, px1, base_py.0, base_py.1) {
                        val = v;
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
    match (&job.buildings_pbf_dir, pbf_buildings) {
        (Some(_), Some(set)) => apply_buildings_pbf(&job, &mut buffer, set, pixel_deg),
        (Some(dir), None) => anyhow::bail!(
            "buildings_pbf_dir {:?} was requested but no decoded building set was \
             supplied for {:?}; refusing to write a building-less tile",
            dir,
            job.output_path
        ),
        (None, _) => {}
    }

    let format_str = job.format.as_deref().unwrap_or("r16sint");
    let is_bc6h = format_str.eq_ignore_ascii_case("bc6h");

    #[cfg(not(feature = "bc6h"))]
    if is_bc6h {
        return Err(anyhow::anyhow!("BC6H format requested but binary was built without the 'bc6h' feature"));
    }

    // Dynamic BaseElev calculation for BC6H
    let mut base_elev = 0i16;
    if is_bc6h {
        let mut min_elev = i16::MAX;
        for &v in buffer.iter() {
            if v > -5000 && v < min_elev {
                min_elev = v;
            }
        }
        if min_elev == i16::MAX { min_elev = 0; }
        base_elev = min_elev;
    }

    let f = File::create(&job.output_path)?;
    let mut w = BufWriter::with_capacity(1024 * 1024, f);

    w.write_all(b"AETH")?;

    // Version 2 for BC6H, Version 1 for legacy R16SINT
    let version = if is_bc6h { 2u16 } else { 1u16 };
    w.write_u16::<LittleEndian>(version)?;
    w.write_u16::<LittleEndian>(job.size_px as u16)?;
    w.write_f64::<LittleEndian>(job.ul_lat)?;
    w.write_f64::<LittleEndian>(job.ul_lon)?;
    w.write_f64::<LittleEndian>(pixel_deg)?;
    w.write_f64::<LittleEndian>(pixel_deg)?;
    w.write_i16::<LittleEndian>(base_elev)?;

    let bytes_per_row_r16 = job.size_px as u32 * 2;
    let aligned_stride_r16 = (bytes_per_row_r16 + 255) & !255;

    // Calculate final physical row stride (to skip padding during decode)
    let final_stride = if is_bc6h {
        // BC6H block is 16 bytes. Row stride is the width of blocks * 16.
        ((job.size_px + 3) / 4 * 16) as u16
    } else {
        aligned_stride_r16 as u16
    };
    w.write_u16::<LittleEndian>(final_stride)?;

    #[cfg(feature = "bc6h")]
    if is_bc6h {
         // 1. Map terrain to 0-aligned RGBA float buffer
        let mut rgba = vec![0.0f32; total_pixels * 4];
        for (i, &v) in buffer.iter().enumerate() {
            let real_val = if v > -5000 {
                (v - base_elev) as f32 * 0.5 + 1.0
            } else {
                0.0
            };
            rgba[i * 4] = real_val;
            rgba[i * 4 + 1] = real_val;
            rgba[i * 4 + 2] = real_val;
            rgba[i * 4 + 3] = 1.0;
        }

        // ═════════════════════════════════════════════════════════════════════
        // [BC6H-DEBUG] Sample values before encoding
        // ═════════════════════════════════════════════════════════════════════
        let sample_positions: Vec<(usize, usize)> = vec![
            (0, 0), (1, 0), (2, 0), (3, 0),
            (0, 1), (1, 1), (2, 1), (3, 1),
            (out_size/2, out_size/2),
            (out_size/2+1, out_size/2),
            (out_size-1, out_size-1),
        ];
        eprintln!("[BC6H-DEBUG] PRE-ENCODE sample values:");
        for &(x, y) in &sample_positions {
            if x < out_size && y < out_size {
                let idx = y * out_size + x;
                eprintln!("[BC6H-DEBUG]   px({:4},{:4}) raw_i16={:6} → R={:12.4}",
                          x, y, buffer[idx], rgba[idx * 4]);
            }
        }

        // 2. Encode
        let surface = SurfaceRgba32Float {
            width: job.size_px,
            height: job.size_px,
            depth: 1,
            layers: 1,
            mipmaps: 1,
            data: rgba.clone(),
        };

        eprintln!("[BC6H-DEBUG] Calling image_dds encode(BC6Sfloat, Fast) ...");
        let dds_surface = surface.encode(
            ImageFormat::BC6Ufloat,
            Quality::Fast,
            Mipmaps::Disabled
        ).map_err(|e| anyhow::anyhow!("BC6H Encoding failed: {:?}", e))?;

        w.write_all(&dds_surface.data)?;
    }

    if !is_bc6h {
        // Legacy R16SINT writing with 256-byte pitch padding
        let padding_bytes = aligned_stride_r16 - bytes_per_row_r16;
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

    let lr_lat = job.ul_lat - (job.size_px as f64 * px_deg);
    let lr_lon = job.ul_lon + (job.size_px as f64 * px_deg);
    let pad_deg = 20.0 / 111111.0;

    let min_lon = job.ul_lon.min(lr_lon) - pad_deg;
    let max_lon = job.ul_lon.max(lr_lon) + pad_deg;
    let min_lat = job.ul_lat.min(lr_lat) - pad_deg;
    let max_lat = job.ul_lat.max(lr_lat) + pad_deg;

    let mut all_buildings: Vec<Building> = Vec::new();

    for fgb_path in files_to_process {
        let file = File::open(&fgb_path)?;
        let fgb = FgbReader::open(BufReader::new(file))?;

        if let Some(env) = fgb.header().envelope() {
            if env.get(0) > max_lon || env.get(2) < min_lon || env.get(1) > max_lat || env.get(3) < min_lat {
                continue;
            }
        }

        let mut features = fgb.select_bbox(min_lon, min_lat, max_lon, max_lat)?;
        while let Some(feature) = features.next()? {
            if let Some(geo) = feature.geometry() {
                let g_type = geo.type_();
                if g_type == GeometryType::MultiPolygon || g_type == GeometryType::Polygon {
                    collect_fgb_buildings(&geo, &mut all_buildings);
                }
            }
        }
    }

    if all_buildings.is_empty() {
        return Ok(());
    }

    let tile = TileRef {
        ul_lat: job.ul_lat,
        ul_lon: job.ul_lon,
        scale_x: px_deg,
        scale_y: px_deg,
        size_px: job.size_px,
    };
    let mut grid = I16Grid { buf: buffer, size: job.size_px };
    // Truncation, not rounding: this path has always truncated `z * 2.0`, and
    // keeping that keeps previously generated .abt tiles byte-identical.
    let opts = RasterOpts { rounding: Rounding::Truncate, ..Default::default() };
    let stats = rasterize_buildings(&mut grid, &tile, &all_buildings, &opts);

    if stats.datum_suspect {
        eprintln!(
            "[WARN] buildings: roofs sit a median {:.1} m from the terrain — the \
             source may be above-ground heights labelled as absolute, or use a \
             different vertical datum than the terrain.",
            stats.median_roof_above_terrain.unwrap_or(0.0)
        );
    }

    Ok(())
}

/// Parse the `{z}_{x}_{y}.pbf` tile coordinates out of a file name.
pub fn parse_pbf_tile_name(name: &str) -> Option<(u32, u32, u32)> {
    let stem = name.strip_suffix(".pbf").or_else(|| name.strip_suffix(".mvt"))?;
    let parts: Vec<&str> = stem.split('_').collect();
    if parts.len() != 3 {
        return None;
    }
    Some((parts[0].parse().ok()?, parts[1].parse().ok()?, parts[2].parse().ok()?))
}

/// Rasterize an already-decoded vector-tile building set onto an ingest tile.
///
/// Shares the decoder and the write rule with the WASM pipeline, so the native
/// converter and the browser produce the same surface from the same tiles.
///
/// *set* is decoded once per run by [`load_pbf_building_dir`]: it is a whole
/// directory scan plus a PBF decode of every file in it, and its result is the
/// same for every output tile. This function is what runs per tile.
fn apply_buildings_pbf(
    job: &IngestJob,
    buffer: &mut [i16],
    set: &PbfBuildingSet,
    px_deg: f64,
) {
    let tiles_read = set.tiles_read;

    if set.buildings.is_empty() {
        println!("[Info] buildings_pbf_dir: {tiles_read} tile(s), no buildings in extent");
        return;
    }

    let tile = TileRef {
        ul_lat: job.ul_lat,
        ul_lon: job.ul_lon,
        scale_x: px_deg,
        scale_y: px_deg,
        size_px: job.size_px,
    };
    let mut grid = I16Grid { buf: buffer, size: job.size_px };
    let stats = rasterize_buildings(&mut grid, &tile, &set.buildings, &RasterOpts::default());

    println!(
        "[Info] buildings_pbf_dir: {tiles_read} tile(s), {} building(s) \
         ({} after dedup), {} drawn, {} pixel(s) raised",
        set.decoded, set.buildings.len(), stats.buildings_hit, stats.pixels_modified
    );
}

/// Collect building rings from a FlatGeobuf geometry into the shared model.
///
/// FlatGeobuf carries the roof as an absolute elevation in the geometry Z, so
/// every ring becomes a [`BuildingHeight::Absolute`] at the geometry's maximum
/// Z — which is what this path has always used.
fn collect_fgb_buildings(geo: &flatgeobuf::Geometry, out: &mut Vec<Building>) {
    if let Some(parts) = geo.parts() {
        if parts.len() > 0 {
            for i in 0..parts.len() {
                collect_fgb_buildings(&parts.get(i), out);
            }
            return;
        }
    }

    let xy = match geo.xy() { Some(v) => v, None => return };
    let z_vals = match geo.z() { Some(v) => v, None => return };

    let mut max_z: f64 = -1000.0;
    for z in z_vals {
        if z > max_z { max_z = z; }
    }

    // Preserved from the original write loop: a roof that lands below zero in
    // half-metre units is dropped rather than drawn.
    if (max_z * 2.0).trunc() < 0.0 { return; }

    let mut push_ring = |stop_idx: usize, start_idx: usize, out: &mut Vec<Building>| {
        let count = (stop_idx - start_idx) / 2;
        if count < 3 { return; }
        let mut coords: Vec<(f64, f64)> = Vec::with_capacity(count);
        let mut i = start_idx;
        while i < stop_idx {
            coords.push((xy.get(i), xy.get(i + 1)));
            i += 2;
        }
        out.push(Building {
            coords,
            height: BuildingHeight::Absolute(max_z),
            source: HeightSource::AbsoluteZ,
        });
    };

    if let Some(ends_vec) = geo.ends() {
        let mut start = 0;
        for end in ends_vec {
            push_ring(end as usize, start, out);
            start = end as usize;
        }
    } else {
        push_ring(xy.len(), 0, out);
    }
}

pub fn point_in_poly(x: f64, y: f64, poly: &[(f64, f64)]) -> bool {
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

#[cfg(test)]
mod tests {
    use super::*;

    // ── DEM sample decoding ────────────────────────────────────────────────

    #[test]
    fn the_casts_that_used_to_decode_dem_samples_map_nodata_to_zero() {
        // Not a test of our code — a test of the language rule the bug rested
        // on, so the reason for the two helpers stays visible. Both of these
        // are 0, i.e. sea level, and both used to reach the .abt that way.
        assert_eq!(f32::NAN as i16, 0, "NaN is GDAL's default Float32 nodata");
        assert_eq!(i32::MIN as i16, 0, "an int-to-int cast keeps the low 16 bits");
        assert_eq!(65536i32 as i16, 0);
    }

    #[test]
    fn float32_nodata_becomes_a_void_not_sea_level() {
        assert_eq!(f32_sample_to_half_metres(f32::NAN), VOID_ELEV);
        assert_eq!(f32_sample_to_half_metres(f32::INFINITY), VOID_ELEV);
        assert_eq!(f32_sample_to_half_metres(f32::NEG_INFINITY), VOID_ELEV);
    }

    #[test]
    fn float32_elevations_decode_exactly_as_before() {
        // Half-metre units, truncating toward zero, saturating at the bounds —
        // unchanged for every finite sample, so real terrain keeps its bytes.
        assert_eq!(f32_sample_to_half_metres(0.0), 0);
        assert_eq!(f32_sample_to_half_metres(100.0), 200);
        assert_eq!(f32_sample_to_half_metres(-430.5), -861); // Dead Sea shore
        assert_eq!(f32_sample_to_half_metres(8848.9), 17697); // 17697.8 truncates
        assert_eq!(f32_sample_to_half_metres(-0.4), 0);
        assert_eq!(f32_sample_to_half_metres(1.0e9), i16::MAX);
        assert_eq!(f32_sample_to_half_metres(-1.0e9), i16::MIN);
    }

    #[test]
    fn int32_nodata_becomes_a_void_not_sea_level() {
        // i32::MIN is the classic Int32 nodata. Its low 16 bits are zero, so
        // the old `(x as i16)` narrowing produced 0 m.
        assert_eq!(i32_sample_to_half_metres(i32::MIN), VOID_ELEV);
        assert_eq!(i32_sample_to_half_metres(-32768), VOID_ELEV);
        assert_eq!(i32_sample_to_half_metres(-9999), VOID_ELEV);
        // 65536 m is not an elevation either, but the truncating cast made it 0.
        assert_ne!(i32_sample_to_half_metres(65536), 0);
    }

    #[test]
    fn int32_elevations_saturate_instead_of_truncating() {
        assert_eq!(i32_sample_to_half_metres(0), 0);
        assert_eq!(i32_sample_to_half_metres(100), 200);
        assert_eq!(i32_sample_to_half_metres(-430), -860);
        assert_eq!(i32_sample_to_half_metres(8849), 17698);
        assert_eq!(i32_sample_to_half_metres(i32::MAX), i16::MAX);
        // The whole i16 range is reachable and nothing wraps.
        for m in [-4000i32, -2000, -1, 1, 16000, 16383] {
            assert_eq!(i32_sample_to_half_metres(m), (m * 2) as i16, "m = {m}");
        }
    }

    #[test]
    fn a_void_is_rejected_by_the_validity_test_that_a_zero_would_pass() {
        // This is why the fix matters: the samplers in this file accept any
        // value above -5000 *half-metres* (-2500 m) as real ground.
        assert!(VOID_ELEV <= -5000, "the sentinel must read as no-data");
        assert!(0 > -5000, "sea level reads as valid ground — as it should");
    }

    // ── Area-averaged resampling ───────────────────────────────────────────

    /// A source image in the base DEM's frame: degrees, origin at the tile's
    /// upper-left, `scale` degrees per pixel.
    fn img(w: u32, h: u32, origin_e: f64, origin_n: f64, scale: f64, data: Vec<i16>) -> LoadedImage {
        assert_eq!(data.len(), (w * h) as usize);
        LoadedImage {
            width: w,
            height: h,
            data,
            origin_e,
            origin_n,
            limit_e: origin_e + w as f64 * scale,
            limit_n: origin_n - h as f64 * scale,
            scale,
            name: "test".into(),
        }
    }

    /// Convert one tile from an in-memory base DEM and read the payload back.
    fn convert_with_base(mut job: IngestJob, base: LoadedImage) -> Vec<Vec<i16>> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("base.tif");
        job.output_path = dir.path().join("tile.abt");
        job.base_tif = Some(path.clone());
        let mut cache = HashMap::new();
        cache.insert(path, Arc::new(base));
        let out = job.output_path.clone();
        let size = job.size_px as usize;
        process_tile(job, Arc::new(std::sync::Mutex::new(cache)), None).unwrap();

        let bytes = fs::read(&out).unwrap();
        let stride = ((size * 2) + 255) & !255;
        (0..size)
            .map(|y| {
                (0..size)
                    .map(|x| {
                        let o = 44 + y * stride + x * 2;
                        i16::from_le_bytes([bytes[o], bytes[o + 1]])
                    })
                    .collect()
            })
            .collect()
    }

    fn ramp(w: u32, h: u32) -> Vec<i16> {
        (0..w * h).map(|i| (i % w) as i16 * 10 + (i / w) as i16).collect()
    }

    #[test]
    fn an_output_cell_is_the_mean_of_the_source_block_under_it() {
        // Base three times finer than the target: every interior output pixel
        // owns a clean 3x3 block, centred on the pixel the old code point-
        // sampled. 3x3 = 9 samples of which the old code kept one.
        let mut job = empty_job(PathBuf::new());
        job.size_px = 4;
        let pixel_deg = job.resolution_m / 111111.0;
        let src = img(12, 12, job.ul_lon, job.ul_lat, pixel_deg / 3.0, ramp(12, 12));

        let block_mean = |cx: usize, cy: usize| -> i16 {
            let mut s = 0i32;
            for y in cy - 1..=cy + 1 {
                for x in cx - 1..=cx + 1 {
                    s += src.data[y * 12 + x] as i32;
                }
            }
            ((s * 2 + 9) / 18) as i16 // round half away from zero, 9 samples
        };
        let expect: Vec<(usize, usize, i16)> = (1..4)
            .flat_map(|y| (1..4).map(move |x| (x, y, 0)))
            .map(|(x, y, _)| (x, y, block_mean(3 * x, 3 * y)))
            .collect();

        let out = convert_with_base(job, src);
        for (x, y, want) in expect {
            assert_eq!(out[y][x], want, "pixel ({x},{y})");
        }
    }

    #[test]
    fn voids_are_left_out_of_the_average_instead_of_dragging_the_cell_down() {
        // The DEM-edge / coastline case: averaging the sentinel in would pull a
        // 1000 m cell to -1200 m and bury the coast.
        let src = &[
            2000, 2000, 2000, //
            2000, VOID_ELEV, 2000, //
            2000, 2000, VOID_ELEV,
        ];
        // 7 valid samples of 2000, 2 voids -> exactly 2000, not 2000*7/9.
        assert_eq!(mean_valid(src, 3, 0, 3, 0, 3), Some(2000));

        // A mixed block averages only the real ground in it.
        let src = &[10i16, 20, VOID_ELEV, 40];
        assert_eq!(mean_valid(src, 4, 0, 4, 0, 1), Some(23)); // (10+20+40)/3 = 23.33
    }

    #[test]
    fn a_cell_with_nothing_but_voids_under_it_stays_void() {
        let src = &[VOID_ELEV; 9];
        assert_eq!(mean_valid(src, 3, 0, 3, 0, 3), None);

        // End to end: a base DEM of pure voids must not become sea level.
        let mut job = empty_job(PathBuf::new());
        job.size_px = 4;
        let pixel_deg = job.resolution_m / 111111.0;
        let src = img(12, 12, job.ul_lon, job.ul_lat, pixel_deg / 3.0, vec![VOID_ELEV; 144]);
        let out = convert_with_base(job, src);
        for row in &out {
            for &v in row {
                assert_eq!(v, VOID_ELEV);
            }
        }
    }

    #[test]
    fn a_source_coarser_than_the_target_still_gives_every_cell_a_value() {
        // Ratio < 1: the cell holds no source-pixel centre at all, so there is
        // nothing to average. It must not divide by zero or fall through as a
        // void — it takes the pixel it sits in, exactly as before.
        let mut job = empty_job(PathBuf::new());
        job.size_px = 8;
        let pixel_deg = job.resolution_m / 111111.0;
        // One source pixel per four output pixels.
        let scale = pixel_deg * 4.0;
        let src = img(4, 4, job.ul_lon, job.ul_lat, scale, ramp(4, 4));
        // What the point sampler this replaced would have written, spelled the
        // way it spelled it — including the truncation, so the comparison holds
        // on the cells where the arithmetic lands a hair either side of a
        // source-pixel boundary.
        let expect: Vec<Vec<i16>> = (0..8)
            .map(|y| {
                let py = ((job.ul_lat - (job.ul_lat - y as f64 * pixel_deg)) / scale) as usize;
                (0..8)
                    .map(|x| {
                        let px = ((job.ul_lon + x as f64 * pixel_deg - job.ul_lon) / scale) as usize;
                        src.data[py * 4 + px]
                    })
                    .collect()
            })
            .collect();

        let out = convert_with_base(job, src);
        assert_eq!(out, expect);
    }

    #[test]
    fn the_span_never_empties_and_never_divides_by_zero_at_any_ratio() {
        for &ratio in &[0.01f64, 0.5, 0.999, 1.0, 1.001, 3.26, 60.0] {
            for step in 0..7 {
                let centre = 3.0 + step as f64 * 0.137;
                let (a, b) = sample_span(centre - ratio / 2.0, centre + ratio / 2.0, 16);
                assert!(a < b, "ratio {ratio} centre {centre} gave an empty span");
                assert!(b <= 16);
            }
        }
        // A cell entirely off the low edge still resolves to a real pixel.
        let (a, b) = sample_span(-8.0, -7.0, 16);
        assert!(a < b && b <= 16);
    }

    #[test]
    fn a_non_integer_ratio_uses_every_source_pixel_exactly_once() {
        // 3.26 source pixels per output cell — the ratio that exposed the bug.
        // Nearest-neighbour keeps pixel `int(3.26 * x)` and drops the other
        // 2.26, so a feature in a dropped pixel is invisible and which pixels
        // survive follows a 3,3,3,4 beat. The spans must instead tile the
        // source: contiguous, no gaps, no pixel counted twice.
        const RATIO: f64 = 3.26;
        let n = 326u32;
        let mut next = None;
        // From 1: cell 0's footprint runs off the raster's edge and is clipped.
        for x in 1..99u32 {
            let centre = RATIO * x as f64;
            let (a, b) = sample_span(centre - RATIO / 2.0, centre + RATIO / 2.0, n);
            assert!(b - a >= 3 && b - a <= 4, "x={x} took {} pixels", b - a);
            if let Some(prev_end) = next {
                assert_eq!(a, prev_end, "gap or overlap before output cell {x}");
            }
            next = Some(b);
        }
    }

    #[test]
    fn a_feature_the_old_sampler_skipped_now_reaches_the_output() {
        // The same 3.26 ratio, end to end. A spike sits on a source pixel the
        // truncating index never reads; with area-averaging it lifts its cell.
        let mut job = empty_job(PathBuf::new());
        job.size_px = 8;
        let pixel_deg = job.resolution_m / 111111.0;
        let w = 40u32;
        let flat = vec![1000i16; (w * w) as usize];

        let out_flat = convert_with_base(job.clone(), img(w, w, job.ul_lon, job.ul_lat, pixel_deg / 3.26, flat.clone()));
        assert_eq!(out_flat[4][4], 1000);

        // Pixel (4*3.26 = 13.04 -> the old code read column 13); put the spike
        // on column 14, which it never reads, and keep row 13 so only the
        // column moves.
        let mut spiked = flat.clone();
        spiked[13 * w as usize + 14] = 1000 + 900; // +450 m mast
        let old_sample = spiked[13 * w as usize + 13];
        assert_eq!(old_sample, 1000, "the old point sample is blind to the spike");

        let out = convert_with_base(job.clone(), img(w, w, job.ul_lon, job.ul_lat, pixel_deg / 3.26, spiked));
        assert!(
            out[4][4] > out_flat[4][4],
            "the spike must raise the cell it stands in: {} vs {}",
            out[4][4], out_flat[4][4]
        );
    }

    // ── .abt writer ────────────────────────────────────────────────────────

    fn empty_job(out: PathBuf) -> IngestJob {
        IngestJob {
            output_path: out,
            format: None,
            ul_lat: 47.5,
            ul_lon: 8.25,
            resolution_m: 10.0,
            size_px: 8,
            base_tif: None,
            swiss_tifs: Vec::new(),
            buildings_file: None,
            buildings_pbf_dir: None,
        }
    }

    #[test]
    fn the_abt_writer_emits_the_44_byte_header_and_padded_rows() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("tile.abt");
        let job = empty_job(out.clone());
        let pixel_deg = job.resolution_m / 111111.0;
        let size = job.size_px as usize;

        process_tile_with_cache(job, Arc::new(std::sync::Mutex::new(HashMap::new()))).unwrap();

        let bytes = fs::read(&out).unwrap();
        let stride = ((size * 2) + 255) & !255;
        assert_eq!(bytes.len(), 44 + stride * size);

        let u16at = |o: usize| u16::from_le_bytes([bytes[o], bytes[o + 1]]);
        let f64at = |o: usize| f64::from_le_bytes(bytes[o..o + 8].try_into().unwrap());
        assert_eq!(&bytes[0..4], b"AETH");
        assert_eq!(u16at(4), 1, "version 1 = R16SINT");
        assert_eq!(u16at(6), 8, "width");
        assert_eq!(f64at(8), 47.5);
        assert_eq!(f64at(16), 8.25);
        assert_eq!(f64at(24), pixel_deg);
        assert_eq!(f64at(32), pixel_deg);
        assert_eq!(i16::from_le_bytes([bytes[40], bytes[41]]), 0, "base_elev");
        assert_eq!(u16at(42) as usize, stride);
    }

    #[test]
    fn a_tile_with_no_terrain_source_is_all_void_not_all_sea_level() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("tile.abt");
        let job = empty_job(out.clone());
        let size = job.size_px as usize;

        process_tile_with_cache(job, Arc::new(std::sync::Mutex::new(HashMap::new()))).unwrap();

        let bytes = fs::read(&out).unwrap();
        let stride = ((size * 2) + 255) & !255;
        for y in 0..size {
            for x in 0..size {
                let o = 44 + y * stride + x * 2;
                assert_eq!(
                    i16::from_le_bytes([bytes[o], bytes[o + 1]]),
                    VOID_ELEV,
                    "pixel ({x},{y})"
                );
            }
            // Row padding is zero-filled, per the 256-byte alignment rule.
            assert!(bytes[44 + y * stride + size * 2..44 + (y + 1) * stride].iter().all(|&b| b == 0));
        }
    }

    #[test]
    fn a_job_with_a_pbf_dir_but_no_decoded_set_is_refused() {
        // Guards the E1 hoist: a batch caller that forgets to pass the set gets
        // an error, not silently building-less tiles.
        let dir = tempfile::tempdir().unwrap();
        let mut job = empty_job(dir.path().join("tile.abt"));
        job.buildings_pbf_dir = Some(dir.path().to_path_buf());

        let err = process_tile(job, Arc::new(std::sync::Mutex::new(HashMap::new())), None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("no decoded building set"), "got {err:?}");
    }

    // ── Point-in-polygon ───────────────────────────────────────────────────

    #[test]
    fn point_in_poly_classifies_inside_outside_and_the_edges() {
        let sq = [(0.0, 0.0), (4.0, 0.0), (4.0, 4.0), (0.0, 4.0)];
        assert!(point_in_poly(2.0, 2.0, &sq));
        assert!(!point_in_poly(5.0, 2.0, &sq));
        assert!(!point_in_poly(-1.0, 2.0, &sq));
        assert!(!point_in_poly(2.0, -1.0, &sq));
        assert!(!point_in_poly(2.0, 5.0, &sq));
        // Half-open on purpose: the low edge is in, the high edge is out, so
        // pixel centres on a shared boundary belong to exactly one polygon.
        assert!(point_in_poly(0.0, 2.0, &sq));
        assert!(!point_in_poly(4.0, 2.0, &sq));
    }

    #[test]
    fn point_in_poly_handles_a_concave_ring() {
        // A "U": the notch between the arms must read as outside.
        let u = [
            (0.0, 0.0),
            (6.0, 0.0),
            (6.0, 6.0),
            (4.0, 6.0),
            (4.0, 2.0),
            (2.0, 2.0),
            (2.0, 6.0),
            (0.0, 6.0),
        ];
        assert!(point_in_poly(1.0, 4.0, &u));
        assert!(point_in_poly(5.0, 4.0, &u));
        assert!(!point_in_poly(3.0, 4.0, &u), "the notch is outside");
        assert!(point_in_poly(3.0, 1.0, &u), "the base is inside");
    }
}