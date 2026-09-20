// rust/aether_converter/src/ingest.rs
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{BufWriter, Write, BufReader};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use serde::Deserialize;
#[cfg(feature = "native")]
use rayon::prelude::*;
use byteorder::{LittleEndian, WriteBytesExt};
use tiff::decoder::{Decoder, DecodingResult, Limits};
use tiff::tags::Tag;
use anyhow::{Context, Result};
use proj4rs::Proj;
use proj4rs::transform::transform;
use flatgeobuf::{FeatureProperties, FgbReader, GeometryType};
use crate::buildings::{
    load_pbf_building_dir, parse_leading_metres, rasterize_buildings, resolve_building_height,
    Building, BuildingHeight, HeightSource, I16Grid, PbfBuildingSet, RasterOpts, Rounding, TileRef,
    DEFAULT_HEIGHT_M,
};
use fallible_streaming_iterator::FallibleStreamingIterator;
#[cfg(feature = "bc6h")]
use image_dds::{SurfaceRgba32Float, ImageFormat, Mipmaps, Quality};

/// One terrain input of an ingest job.
///
/// `sources` is ordered by priority: for every output pixel the first source
/// with real ground under it wins. `crs` is either `"EPSG:nnnn"` or a raw proj
/// string starting with `+`; when absent the GeoTIFF's GeoKeyDirectory must
/// carry a usable EPSG code, else the job fails naming this file. `nodata`
/// overrides the file's `GDAL_NODATA` tag; matching samples become the void
/// sentinel at load time.
#[derive(Deserialize, Debug, Clone)]
pub struct SourceSpec {
    pub path: PathBuf,
    #[serde(default)]
    pub crs: Option<String>,
    #[serde(default)]
    pub nodata: Option<f64>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct IngestJob {
    pub output_path: PathBuf,
    pub format: Option<String>, // "bc6h" or "r16sint"
    pub ul_lat: f64,
    pub ul_lon: f64,
    pub resolution_m: f64,
    pub size_px: u32,
    /// Prioritised terrain inputs (first-valid-wins per pixel). The modern
    /// surface; mutually exclusive with the deprecated `base_tif`/`swiss_tifs`.
    #[serde(default)]
    pub sources: Vec<SourceSpec>,
    /// If set, a pixel no source covers is written as `round(void_fill_m * 2)`
    /// half-metres (saturating) instead of the `-9999` void sentinel.
    #[serde(default)]
    pub void_fill_m: Option<f64>,
    /// DEPRECATED alias: normalized to a trailing `sources` entry with
    /// `crs = "EPSG:4326"`. Slated for removal in the next major version.
    #[serde(default)]
    pub base_tif: Option<PathBuf>,
    /// DEPRECATED alias: normalized to leading `sources` entries with
    /// `crs = "EPSG:2056"` (in order). Slated for removal in the next major
    /// version.
    #[serde(default)]
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

impl IngestJob {
    /// The job's terrain inputs in priority order, with the deprecated
    /// `base_tif`/`swiss_tifs` aliases normalized into `sources` form.
    ///
    /// Legacy shim: each `swiss_tifs` entry becomes a source with
    /// `crs = "EPSG:2056"` (in order), then `base_tif` is appended with
    /// `crs = "EPSG:4326"` — which preserves the old per-pixel priority
    /// (the high-resolution stack first, the base DEM as fallback). After
    /// this call there is exactly ONE code path.
    pub fn effective_sources(&self) -> Result<Vec<SourceSpec>> {
        let has_legacy = self.base_tif.is_some() || !self.swiss_tifs.is_empty();
        if !self.sources.is_empty() && has_legacy {
            anyhow::bail!(
                "ingest job for {:?}: 'sources' cannot be combined with the deprecated \
                 'base_tif'/'swiss_tifs' fields — the priority order would be ambiguous. \
                 Move every input into 'sources' (array order = priority) and delete the \
                 legacy fields.",
                self.output_path
            );
        }
        if !self.sources.is_empty() {
            return Ok(self.sources.clone());
        }
        let mut out = Vec::with_capacity(self.swiss_tifs.len() + 1);
        for p in &self.swiss_tifs {
            out.push(SourceSpec { path: p.clone(), crs: Some("EPSG:2056".into()), nodata: None });
        }
        if let Some(base) = &self.base_tif {
            out.push(SourceSpec { path: base.clone(), crs: Some("EPSG:4326".into()), nodata: None });
        }
        Ok(out)
    }
}

/// Key under which a loaded source is cached.
///
/// The decoded pixels depend on the *explicit* per-source `nodata` override
/// (it is applied at load time), so the same file requested with two different
/// overrides must occupy two cache slots. The override's f64 bit pattern keeps
/// the key hashable.
pub type CacheKey = (PathBuf, Option<u64>);

/// Shared cache of loaded sources, keyed by [`CacheKey`].
pub type ImageCache = Arc<std::sync::Mutex<HashMap<CacheKey, Arc<LoadedImage>>>>;

/// The [`CacheKey`] for one source spec.
pub fn cache_key(spec: &SourceSpec) -> CacheKey {
    (spec.path.clone(), spec.nodata.map(f64::to_bits))
}

/// Per-path in-flight load guard, process-global.
///
/// The cache check and the (slow) TIFF decode cannot share one lock, so
/// without this a source used by many parallel tiles — but below the batch
/// preloader's 100 MB threshold — would be decoded by every rayon thread at
/// once, holding N transient copies in RAM (the pre-sources[] code preloaded
/// every shared `base_tif` unconditionally, so legacy callers relied on that
/// protection). Only one thread decodes a given path; the others wait on the
/// condvar and re-check the cache. Pure `std::sync`, so the wasm/non-native
/// build compiles unchanged — and being single-threaded it never waits.
static INFLIGHT: std::sync::OnceLock<(std::sync::Mutex<std::collections::HashSet<PathBuf>>, std::sync::Condvar)> =
    std::sync::OnceLock::new();

fn inflight() -> &'static (std::sync::Mutex<std::collections::HashSet<PathBuf>>, std::sync::Condvar) {
    INFLIGHT.get_or_init(|| (std::sync::Mutex::new(std::collections::HashSet::new()), std::sync::Condvar::new()))
}

/// Removes the path from the in-flight set and wakes waiters on drop, so the
/// mark is released on success, error, and panic alike (a leaked mark would
/// hang every waiter forever).
struct InflightMark {
    path: PathBuf,
}

impl Drop for InflightMark {
    fn drop(&mut self) {
        let (set, cv) = inflight();
        set.lock().unwrap().remove(&self.path);
        cv.notify_all();
    }
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

/// How much of one tile a source actually reached.
///
/// Counted BEFORE `void_fill_m` is applied, so it answers "did any source
/// cover this pixel?" and not "is this pixel non-sentinel?" — a job that fills
/// its holes with 0 m would otherwise report full coverage of a tile no source
/// touched. [`process_tile`] returns it so the caller can refuse a run that
/// produced no terrain at all; a partially covered tile is normal and says
/// nothing on its own (the edge tiles of any area straddle the source).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TileStats {
    /// Pixels at least one source supplied a sample for.
    pub covered_px: u64,
    /// Pixels in the tile.
    pub total_px: u64,
}

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
    i64_sample_to_half_metres(i64::from(x))
}

/// The same rule for any integer width the `tiff` crate can decode.
///
/// UInt16 is the commonest national-DEM sample format and UInt8/Int8/UInt32/
/// Int64 all turn up in the wild; every one of them used to come back as a
/// bare "Unsupported TIF format" from a reader that handled only I16/I32/F32.
#[inline]
pub fn i64_sample_to_half_metres(x: i64) -> i16 {
    let half_metres = x.saturating_mul(2);
    if half_metres < VOID_ELEV as i64 {
        VOID_ELEV
    } else if half_metres > i16::MAX as i64 {
        i16::MAX
    } else {
        half_metres as i16
    }
}

/// Convert one Float64 DEM sample to the `.abt` half-metre unit.
///
/// Same rule as [`f32_sample_to_half_metres`] — non-finite is the void
/// sentinel, and the `as i16` cast saturates rather than wrapping.
#[inline]
pub fn f64_sample_to_half_metres(x: f64) -> i16 {
    if !x.is_finite() {
        return VOID_ELEV;
    }
    (x * 2.0) as i16
}

/// Decoded integer samples of any width -> half-metres, honouring *nodata*.
#[inline]
fn int_samples<T>(v: Vec<T>, nodata: Option<f64>) -> Vec<i16>
where
    T: Copy + Into<i64>,
{
    v.into_iter()
        .map(|x| {
            let widened: i64 = x.into();
            match nodata {
                Some(nd) if widened as f64 == nd => VOID_ELEV,
                _ => i64_sample_to_half_metres(widened),
            }
        })
        .collect()
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
    /// CRS read from the file's GeoKeyDirectory as `"EPSG:nnnn"`, if the file
    /// carries a usable (non-user-defined) code. A per-source explicit `crs`
    /// always wins over this.
    pub embedded_crs: Option<String>,
    /// True when the source was an `.abt` tile (detected by magic). Such a
    /// source is self-describing — geographic degrees, values already in
    /// half-metres — and refuses per-source `crs`/`nodata` overrides.
    pub is_abt: bool,
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
/// **Upsampling falls back to the centre pixel.** When the output cell is finer
/// than a source pixel the span can be empty — no source centre lands in it.
/// Rather than divide by zero or leave the cell void it falls back to the pixel
/// containing the cell's *centre* (callers anchor the window on the cell
/// centre, so lo/hi straddle it). It is also the continuous limit of the span
/// rule: at ratio 1 the one pixel whose centre is inside the cell is the one
/// containing the cell's centre, so nothing jumps half a pixel as the ratio
/// crosses 1.
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

/// EPSG code out of a raw GeoKeyDirectory (tag 34735) array, as `"EPSG:nnnn"`.
///
/// ProjectedCSTypeGeoKey (3072) is consulted first; only when it is absent is
/// GeographicTypeGeoKey (2048) consulted. A *present but user-defined* (32767)
/// projected key does NOT fall back to the geographic key: the pixels are in
/// an unknown projection, and sampling them as the underlying geographic CRS
/// would be silently wrong. `None` means "no usable code" and the caller must
/// error unless the source carries an explicit `crs`.
fn epsg_from_geokeys(dir: &[u16]) -> Option<String> {
    // Header: KeyDirectoryVersion, KeyRevision, MinorRevision, NumberOfKeys;
    // then 4-u16 entries (KeyID, TIFFTagLocation, Count, ValueOffset). A key
    // whose TIFFTagLocation != 0 stores its value in another tag — never the
    // case for these two SHORT-valued keys, so such entries are unusable here.
    if dir.len() < 4 {
        return None;
    }
    let n_keys = dir[3] as usize;
    let mut projected = None;
    let mut geographic = None;
    for i in 0..n_keys {
        let off = 4 + i * 4;
        if off + 4 > dir.len() {
            break;
        }
        let (key_id, location, value) = (dir[off], dir[off + 1], dir[off + 3]);
        if location != 0 {
            continue;
        }
        match key_id {
            3072 => projected = Some(value),
            2048 => geographic = Some(value),
            _ => {}
        }
    }
    let usable = |v: u16| v != 0 && v != 32767;
    match projected {
        Some(v) if usable(v) => Some(format!("EPSG:{v}")),
        Some(_) => None, // user-defined projection: explicit `crs` required
        None => geographic.filter(|&v| usable(v)).map(|v| format!("EPSG:{v}")),
    }
}

/// Inline value of one SHORT-valued GeoKey out of a raw GeoKeyDirectory
/// (tag 34735). Keys whose TIFFTagLocation != 0 store their value in another
/// tag and are not readable here — `None`.
fn geokey_short(dir: &[u16], wanted: u16) -> Option<u16> {
    if dir.len() < 4 {
        return None;
    }
    let n_keys = dir[3] as usize;
    for i in 0..n_keys {
        let off = 4 + i * 4;
        if off + 4 > dir.len() {
            break;
        }
        let (key_id, location, value) = (dir[off], dir[off + 1], dir[off + 3]);
        if key_id == wanted && location == 0 {
            return Some(value);
        }
    }
    None
}

/// Resolve a `crs` string (`"EPSG:nnnn"` or a raw `+proj=…` string) to a proj
/// string. Hard errors name the source file and how to fix the job.
fn crs_to_proj_string(crs: &str, path: &Path) -> Result<String> {
    let s = crs.trim();
    if s.starts_with('+') {
        return Ok(s.to_string());
    }
    if let Some(code_str) = s.get(..5).filter(|p| p.eq_ignore_ascii_case("EPSG:")).map(|_| &s[5..]) {
        let code: u16 = code_str.trim().parse().map_err(|_| {
            anyhow::anyhow!(
                "source {:?}: crs {:?} is not a valid EPSG code; use \"EPSG:nnnn\" \
                 or a proj string starting with '+'",
                path, s
            )
        })?;
        return match crs_definitions::from_code(code) {
            Some(def) => Ok(def.proj4.to_string()),
            None => anyhow::bail!(
                "source {:?}: unknown EPSG code {} (not in the built-in registry); \
                 supply the projection as a proj string in \"crs\" instead, e.g. \
                 \"+proj=… +ellps=… +units=m\"",
                path, code
            ),
        };
    }
    anyhow::bail!(
        "source {:?}: crs {:?} is neither \"EPSG:nnnn\" nor a proj string starting \
         with '+'; fix the \"crs\" field of this source",
        path, s
    )
}

/// How one resolved source is sampled.
enum SourceKind {
    /// Geographic CRS: pixel coordinates are lon/lat degrees and are sampled
    /// directly against the output's WGS84 grid — the historical `base_tif`
    /// path, kept byte-identical. Decision: *any* geographic CRS is treated as
    /// WGS84 degrees (datum shifts are metres — far below a DEM pixel).
    Geographic,
    /// Projected CRS: WGS84 -> source via proj4rs, by index into the job's
    /// projection group table (sources sharing a CRS share row transforms).
    Projected(usize),
}

struct ProjGroup {
    proj: Arc<Proj>,
    /// Finest source pixel size (metres) among the group's members — the unit
    /// of the row-linearity check.
    min_scale: f64,
}

/// Load one terrain source — a GeoTIFF or an `.abt` tile — into RAM.
///
/// The two are told apart by the `AETH` magic, never by extension. Public so
/// main.rs can use it for sequential pre-loading (the preload rule therefore
/// applies to `.abt` sources exactly as to GeoTIFFs).
///
/// `nodata_override` is the source's explicit `nodata` field; for a GeoTIFF
/// it wins over the file's GDAL_NODATA tag, and matching samples become
/// VOID_ELEV *here*, at load time, which keeps the samplers' `> -5000`
/// validity test unchanged. An `.abt` source refuses the override (it is
/// self-describing; its voids are already `-9999`).
pub fn load_source_to_ram(path: &Path, nodata_override: Option<f64>) -> Result<Arc<LoadedImage>> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = File::open(path).with_context(|| format!("Opening {:?}", path))?;
    let mut magic = [0u8; 4];
    let got = file.read(&mut magic).with_context(|| format!("Reading {:?}", path))?;
    if got == 4 && magic == *b"AETH" {
        if nodata_override.is_some() {
            anyhow::bail!(
                "source {:?}: .abt sources are self-describing — remove \"nodata\" \
                 from this source (its voids are already the -9999 sentinel)",
                path
            );
        }
        return load_abt_to_ram(path, file);
    }
    file.seek(SeekFrom::Start(0))
        .with_context(|| format!("Reading {:?}", path))?;
    let reader = BufReader::with_capacity(1024 * 1024, file);
    // The default tiff decode-buffer cap (~256 MB) rejects large rasters with
    // "The Decoder limits are exceeded". Callers now hand us one small,
    // per-tile GeoTIFF at a time (bounded by the plugin's warp size), so lift
    // the limit and let the tile decode rather than silently failing.
    let mut decoder = Decoder::new(reader)?.with_limits(Limits::unlimited());
    let (w, h) = decoder.dimensions()?;

    // A picture is not terrain. Elevation rasters carry ONE sample per
    // pixel; an RGB(A)/palette source — a rendered WMS/XYZ basemap export,
    // a hillshade — would have its colour values read as metres and produce
    // confident garbage. Refuse by band count/photometric, never by sample
    // type: genuine UInt8 single-band DEMs stay ingestable.
    let color = decoder.colortype()?;
    match color {
        tiff::ColorType::Gray(_) => {}
        tiff::ColorType::Multiband { num_samples: 1, .. } => {}
        other => anyhow::bail!(
            "source {:?}: {:?} pixels — this is an image (a rendered map, \
             photo or hillshade), not an elevation raster. Elevation sources \
             are single-band; converting a picture would read its colour \
             values as metres. Use a real DEM for this area instead.",
            path,
            other
        ),
    }

    let model_trans = decoder.get_tag_f64_vec(Tag::ModelTransformationTag).unwrap_or_default();
    let tiepoints = decoder.get_tag_f64_vec(Tag::ModelTiepointTag).unwrap_or_default();
    let pixel_scales = decoder.get_tag_f64_vec(Tag::ModelPixelScaleTag).unwrap_or_default();
    let geokey_dir = decoder.get_tag_u16_vec(Tag::GeoKeyDirectoryTag).ok();

    let (mut origin_e, mut origin_n, scale) = if model_trans.len() == 16 {
        (model_trans[3], model_trans[7], model_trans[0].abs())
    } else if tiepoints.len() >= 6 && pixel_scales.len() >= 2 {
        (tiepoints[3], tiepoints[4], pixel_scales[0])
    } else {
        // Filename-derived georeferencing is gone for good: a wrong guess
        // placed terrain kilometres off and the run still "succeeded".
        anyhow::bail!(
            "source {:?}: no geotransform — the file carries neither a \
             ModelTransformation tag nor a ModelTiepoint+ModelPixelScale pair, \
             so it is not georeferenced. Re-export it as a GeoTIFF with a \
             geotransform (e.g. `gdal_translate`); georeferencing is never \
             derived from file names.",
            path
        );
    };

    // GTRasterTypeGeoKey (1025) = 2, RasterPixelIsPoint: the origin above
    // names the CENTRE of pixel (0,0), not its NW corner (Copernicus and
    // SRTM ship this way). Shift to the corner so sampling is not half a
    // source pixel off. GDAL applies the same correction on read.
    if geokey_dir
        .as_deref()
        .and_then(|dir| geokey_short(dir, 1025))
        == Some(2)
    {
        origin_e -= 0.5 * scale;
        origin_n += 0.5 * scale;
    }

    let embedded_crs = geokey_dir.as_deref().and_then(epsg_from_geokeys);

    // Per-source nodata: the explicit job field wins; else the GDAL_NODATA
    // ascii tag (trimmed, NUL-stripped; "nan"/"NaN" parse to f64 NaN and the
    // existing non-finite->void rule already covers that case).
    let nodata: Option<f64> = nodata_override.or_else(|| {
        decoder
            .get_tag_ascii_string(Tag::GdalNodata)
            .ok()
            .and_then(|s| s.trim_matches(['\0', ' ', '\t', '\r', '\n']).parse::<f64>().ok())
    });

    let result = decoder.read_image()?;
    let data: Vec<i16> = match result {
        DecodingResult::F32(v) => match nodata {
            // Compare in the source unit, before scaling. The f32 comparison
            // uses the round-tripped f64->f32 value so a tag like
            // "-3.402823466e+38" matches the stored Float32 exactly.
            Some(nd) => {
                let nd32 = nd as f32;
                v.iter()
                    .map(|&x| if x == nd32 { VOID_ELEV } else { f32_sample_to_half_metres(x) })
                    .collect()
            }
            None => v.iter().map(|&x| f32_sample_to_half_metres(x)).collect(),
        },
        DecodingResult::I16(v) => match nodata {
            Some(nd) => v
                .iter()
                .map(|&x| if f64::from(x) == nd { VOID_ELEV } else { x.saturating_mul(2) })
                .collect(),
            None => v.iter().map(|&x| x.saturating_mul(2)).collect(),
        },
        DecodingResult::I32(v) => match nodata {
            Some(nd) => v
                .iter()
                .map(|&x| if f64::from(x) == nd { VOID_ELEV } else { i32_sample_to_half_metres(x) })
                .collect(),
            None => v.iter().map(|&x| i32_sample_to_half_metres(x)).collect(),
        },
        // Every remaining width the `tiff` crate can hand back. The three arms
        // above keep their own comparison rules verbatim (the F32 one rounds
        // the nodata tag through f32 on purpose); these are additive.
        DecodingResult::U8(v) => int_samples(v, nodata),
        DecodingResult::U16(v) => int_samples(v, nodata),
        DecodingResult::U32(v) => int_samples(v, nodata),
        DecodingResult::I8(v) => int_samples(v, nodata),
        DecodingResult::I64(v) => int_samples(v, nodata),
        // u64 is the one width that does not fit i64; saturate rather than
        // wrap, and let the half-metre rule clamp it to i16::MAX.
        DecodingResult::U64(v) => int_samples(
            v.into_iter().map(|x| x.min(i64::MAX as u64) as i64).collect::<Vec<i64>>(),
            nodata,
        ),
        // 16-bit float: widened to f32 and handled by the f32 rule. Named
        // through `to_f32()` so `half` stays out of this crate's dependencies.
        DecodingResult::F16(v) => match nodata {
            Some(nd) => {
                let nd32 = nd as f32;
                v.iter()
                    .map(|&x| {
                        let f = x.to_f32();
                        if f == nd32 { VOID_ELEV } else { f32_sample_to_half_metres(f) }
                    })
                    .collect()
            }
            None => v.iter().map(|&x| f32_sample_to_half_metres(x.to_f32())).collect(),
        },
        DecodingResult::F64(v) => match nodata {
            Some(nd) => v
                .iter()
                .map(|&x| if x == nd { VOID_ELEV } else { f64_sample_to_half_metres(x) })
                .collect(),
            None => v.iter().map(|&x| f64_sample_to_half_metres(x)).collect(),
        },
    };

    // Belt over the braces above: the samplers index `y*w + x`, so a decode
    // that returned anything but exactly w*h samples (interleaved bands,
    // truncated strips) must never reach them.
    if data.len() != w as usize * h as usize {
        anyhow::bail!(
            "source {:?}: decoded {} samples for a {}x{} raster — the file \
             is not a single-band elevation grid",
            path,
            data.len(),
            w,
            h
        );
    }

    let limit_n = origin_n - (h as f64 * scale);
    let limit_e = origin_e + (w as f64 * scale);

    Ok(Arc::new(LoadedImage {
        width: w, height: h, data,
        origin_e, origin_n, limit_e, limit_n,
        scale,
        name: path.file_name().unwrap_or_default().to_string_lossy().to_string(),
        embedded_crs,
        is_abt: false,
    }))
}

/// Load an `.abt` tile as an ingest source.
///
/// Self-describing: the 44-byte header (contract §6) carries the geometry —
/// geographic degrees, square tile, same step both axes — and the payload is
/// already i16 half-metres, so pixels are loaded RAW (no metres→half-metres
/// conversion) with the row-stride padding stripped. `-9999` voids pass
/// through unchanged and read as no-data in the samplers. Sampling and
/// area-averaging then behave exactly as for a geographic GeoTIFF.
///
/// *file* has its cursor just past the magic the caller sniffed.
fn load_abt_to_ram(path: &Path, file: File) -> Result<Arc<LoadedImage>> {
    use byteorder::ReadBytesExt;
    use std::io::Read;

    let corrupt = |what: &str| {
        anyhow::anyhow!(
            "source {:?}: truncated or corrupt .abt header ({what}); the file \
             cannot be used as a terrain source",
            path
        )
    };

    let mut reader = BufReader::with_capacity(1024 * 1024, file);
    let mut rest = [0u8; 40]; // header minus the 4 magic bytes already read
    reader.read_exact(&mut rest).map_err(|_| corrupt("shorter than 44 bytes"))?;
    let mut hdr = &rest[..];
    let version = hdr.read_u16::<LittleEndian>()?;
    let size = hdr.read_u16::<LittleEndian>()? as usize;
    let ul_lat = hdr.read_f64::<LittleEndian>()?;
    let ul_lon = hdr.read_f64::<LittleEndian>()?;
    let _scale_y = hdr.read_f64::<LittleEndian>()?;
    let scale_x = hdr.read_f64::<LittleEndian>()?;
    let _base_elev = hdr.read_i16::<LittleEndian>()?;
    let stride = hdr.read_u16::<LittleEndian>()? as usize;

    // Decision: only R16SINT (version 1) tiles can be sources — a version-2
    // tile's payload is BC6H blocks, not i16 rows, and decoding it as rows
    // would produce silent garbage terrain.
    if version != 1 {
        anyhow::bail!(
            "source {:?}: .abt version {version} is not ingestable — only \
             R16SINT (version 1) tiles can be terrain sources",
            path
        );
    }
    if size == 0 {
        return Err(corrupt("size is 0"));
    }
    let row_bytes = size * 2;
    if stride < row_bytes {
        return Err(corrupt(&format!("row stride {stride} < row bytes {row_bytes}")));
    }
    // Legacy scale quirk, mirrored from the plugin's reader
    // (waveshed/core/abt.py): older converter builds wrote the tile's WHOLE
    // SPAN in scale_x rather than the per-pixel step. A real per-pixel step is
    // tiny (10 m is 9e-5 degrees), so anything at or above 0.005 degrees is a
    // span and is divided by the tile size. scale_y is ignored, as there.
    let pixel_res = if scale_x < 0.005 { scale_x } else { scale_x / size as f64 };
    if !(pixel_res > 0.0) || !pixel_res.is_finite() {
        return Err(corrupt(&format!("non-positive pixel scale {scale_x}")));
    }

    let mut data = vec![0i16; size * size];
    let mut row_buf = vec![0u8; row_bytes];
    let pad = (stride - row_bytes) as i64;
    for row in 0..size {
        reader.read_exact(&mut row_buf).map_err(|_| {
            anyhow::anyhow!(
                "source {:?}: truncated .abt payload (row {row} of {size} is \
                 incomplete); the file cannot be used as a terrain source",
                path
            )
        })?;
        for (i, px) in row_buf.chunks_exact(2).enumerate() {
            // RAW half-metres — .abt already stores the target unit.
            data[row * size + i] = i16::from_le_bytes([px[0], px[1]]);
        }
        if pad > 0 {
            reader.seek_relative(pad).map_err(|_| corrupt("padding seek failed"))?;
        }
    }

    let limit_n = ul_lat - (size as f64 * pixel_res);
    let limit_e = ul_lon + (size as f64 * pixel_res);
    Ok(Arc::new(LoadedImage {
        width: size as u32,
        height: size as u32,
        data,
        origin_e: ul_lon,
        origin_n: ul_lat,
        limit_e,
        limit_n,
        scale: pixel_res,
        name: path.file_name().unwrap_or_default().to_string_lossy().to_string(),
        embedded_crs: None,
        is_abt: true,
    }))
}

/// One linear segment of a row's WGS84 -> source-CRS geometry: over the pixels
/// it covers, `coord = c0 + step * local_x`.
struct Seg {
    e0: f64,
    n0: f64,
    nlo0: f64,
    step_e: f64,
    step_n: f64,
    step_nlo: f64,
}

/// Piecewise-linear source-CRS coordinates of one output row.
///
/// Either a single segment spanning the whole row (when the row-midpoint
/// check passes) or 64-px segments with per-chunk endpoint transforms —
/// O(size/64) transforms per row either way, never per-pixel. In practice a
/// conformal projection's midpoint deviation (~1.2 m over a 0.1° row for
/// EPSG:2056, growing quadratically with row width) exceeds the sub-pixel
/// tolerance, so projected sources take the chunked path — strictly *more*
/// accurate than the retired polynomial's whole-row lerp. Geographic sources
/// never reach this machinery at all. Pixel `x` maps to segment `x >> shift`
/// and local offset `x & mask`, so the hot loop stays branch- and
/// division-free.
struct RowGeom {
    segs: Vec<Seg>,
    shift: u32,
    mask: usize,
    /// Top-edge coordinate bounds, for the per-row source-overlap filter.
    min_e: f64,
    max_e: f64,
    min_n: f64,
    max_n: f64,
}

const CHUNK_SHIFT: u32 = 6; // 64-px row chunks on the chunked path
/// A row's midpoint may deviate from the endpoint lerp by at most this many
/// source pixels before the row is chunked. Measured, real projections
/// (EPSG:2056, UTM) exceed this at practical tile widths and take the chunked
/// path; the single-segment path serves quasi-linear cases (tiny tiles, very
/// coarse sources).
const ROW_LERP_TOLERANCE_PX: f64 = 0.25;

/// WGS84 (degrees) -> `dst` (east, north). `None` when the point is outside
/// the projection's domain.
#[inline]
fn fwd(wgs84: &Proj, dst: &Proj, lat: f64, lon: f64) -> Option<(f64, f64)> {
    let mut pt = (lon.to_radians(), lat.to_radians(), 0.0);
    transform(wgs84, dst, &mut pt).ok()?;
    Some((pt.0, pt.1))
}

/// Build one output row's [`RowGeom`] for one projected CRS.
///
/// `lat_hi` is the row's sample latitude (the cell's north edge lerp uses it,
/// exactly as the old code did) and `lat_lo` is one output row further south.
/// Returns `None` when any needed point fails to transform: the row is then
/// outside the projection's domain, where no source in this CRS can have
/// pixels, so the row simply has no candidates from this group. (Decision:
/// this is a coverage question, not an error — the job's other sources and
/// `void_fill_m` decide what such pixels become.)
fn make_row_geom(
    wgs84: &Proj,
    dst: &Proj,
    lat_hi: f64,
    lat_lo: f64,
    ul_lon: f64,
    pixel_deg: f64,
    out_size: usize,
    min_scale: f64,
) -> Option<RowGeom> {
    let lon_end = ul_lon + out_size as f64 * pixel_deg;
    let (e_start, n_start) = fwd(wgs84, dst, lat_hi, ul_lon)?;
    let (e_end, n_end) = fwd(wgs84, dst, lat_hi, lon_end)?;
    let (_, n_lo_start) = fwd(wgs84, dst, lat_lo, ul_lon)?;
    let (_, n_lo_end) = fwd(wgs84, dst, lat_lo, lon_end)?;

    let inv_size = 1.0 / out_size as f64;
    let step_e = (e_end - e_start) * inv_size;
    let step_n = (n_end - n_start) * inv_size;
    let step_nlo = (n_lo_end - n_lo_start) * inv_size;

    // Row-midpoint linearity check: if the true midpoint sits within a
    // fraction of a source pixel of the endpoint lerp, one segment serves the
    // whole row.
    let mid_lon = ul_lon + (out_size as f64 * 0.5) * pixel_deg;
    let (e_mid, n_mid) = fwd(wgs84, dst, lat_hi, mid_lon)?;
    let lerp_e = (e_start + e_end) * 0.5;
    let lerp_n = (n_start + n_end) * 0.5;
    let dev = ((e_mid - lerp_e).powi(2) + (n_mid - lerp_n).powi(2)).sqrt();

    if dev < ROW_LERP_TOLERANCE_PX * min_scale {
        return Some(RowGeom {
            segs: vec![Seg { e0: e_start, n0: n_start, nlo0: n_lo_start, step_e, step_n, step_nlo }],
            // One segment for every x: x >> (BITS-1) == 0 for any valid index,
            // and x & usize::MAX == x. (A hardcoded 63 would be a >=-width
            // shift on a 32-bit usize — wasm — and panic in debug builds.)
            shift: usize::BITS - 1,
            mask: usize::MAX,
            min_e: e_start.min(e_end),
            max_e: e_start.max(e_end),
            min_n: n_start.min(n_end),
            max_n: n_start.max(n_end),
        });
    }

    // Slow path: 64-px chunks, endpoints transformed per chunk boundary.
    let chunk = 1usize << CHUNK_SHIFT;
    let n_segs = out_size.div_ceil(chunk);
    let mut boundaries = Vec::with_capacity(n_segs + 1);
    for i in 0..=n_segs {
        let x = (i * chunk).min(out_size);
        let lon = ul_lon + x as f64 * pixel_deg;
        let (e, n) = fwd(wgs84, dst, lat_hi, lon)?;
        let (_, n_lo) = fwd(wgs84, dst, lat_lo, lon)?;
        boundaries.push((x, e, n, n_lo));
    }
    let (mut min_e, mut max_e) = (f64::INFINITY, f64::NEG_INFINITY);
    let (mut min_n, mut max_n) = (f64::INFINITY, f64::NEG_INFINITY);
    for &(_, e, n, _) in &boundaries {
        min_e = min_e.min(e);
        max_e = max_e.max(e);
        min_n = min_n.min(n);
        max_n = max_n.max(n);
    }
    let segs = boundaries
        .windows(2)
        .map(|w| {
            let (x0, e0, n0, nlo0) = w[0];
            let (x1, e1, n1, nlo1) = w[1];
            let inv = 1.0 / (x1 - x0) as f64;
            Seg {
                e0,
                n0,
                nlo0,
                step_e: (e1 - e0) * inv,
                step_n: (n1 - n0) * inv,
                step_nlo: (nlo1 - nlo0) * inv,
            }
        })
        .collect();
    Some(RowGeom { segs, shift: CHUNK_SHIFT, mask: chunk - 1, min_e, max_e, min_n, max_n })
}

/// Convert one tile, decoding this job's `buildings_pbf_dir` (if any) for it.
///
/// Convenient for a one-tile run. A batch must not use this: the PBF decode is
/// a whole-directory scan whose result is the same for every output tile, so
/// doing it here runs it once per `.abt`. Batch callers load the set once with
/// [`load_pbf_building_dir`] and call [`process_tile`].
pub fn process_tile_with_cache(job: IngestJob, cache_arc: ImageCache) -> Result<TileStats> {
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
    cache_arc: ImageCache,
    pbf_buildings: Option<&PbfBuildingSet>,
) -> Result<TileStats> {
    // 0. Normalize the legacy aliases into sources[] — ONE code path from here.
    let sources = job.effective_sources()?;

    // 1.-2. Fetch every source, loading cache misses one at a time (OOM fix)
    // with a per-path in-flight guard: when several tiles miss on the same
    // path at once, exactly one thread decodes it and the rest wait on the
    // condvar, then take it from the cache — never N parallel decodes of one
    // file. A listed source that cannot be loaded is FATAL: writing the tile
    // without it would silently drop terrain and the run would still report
    // success.
    let mut images: Vec<Arc<LoadedImage>> = Vec::with_capacity(sources.len());
    for s in &sources {
        let key = cache_key(s);
        let img = loop {
            {
                let cache = cache_arc.lock().unwrap();
                if let Some(img) = cache.get(&key) {
                    break img.clone();
                }
            }
            let (set, cv) = inflight();
            let mut in_flight = set.lock().unwrap();
            if in_flight.insert(s.path.clone()) {
                // This thread owns the load. The mark is dropped (and waiters
                // woken) whether the decode succeeds, errors, or panics.
                drop(in_flight);
                let _mark = InflightMark { path: s.path.clone() };
                let img = load_source_to_ram(&s.path, s.nodata)
                    .with_context(|| format!("loading source {:?}", key.0))?;
                cache_arc.lock().unwrap().insert(key.clone(), img.clone());
                break img;
            }
            // Another thread is decoding this path: wait for it to finish,
            // then re-check the cache (a spurious wakeup just loops again).
            let _unused = cv.wait(in_flight).unwrap();
        };
        images.push(img);
    }

    // Per-source Proj objects are built once per job and cached by CRS string;
    // sources sharing a CRS share one projection group (and, later, one set of
    // per-row transforms).
    let wgs84 = Proj::from_proj_string("+proj=longlat +datum=WGS84 +no_defs")
        .expect("the WGS84 proj string is a constant and always parses");
    let mut proj_cache: HashMap<String, (usize, Arc<Proj>)> = HashMap::new();
    let mut groups: Vec<ProjGroup> = Vec::new();
    let mut kinds: Vec<SourceKind> = Vec::with_capacity(sources.len());
    for (spec, img) in sources.iter().zip(&images) {
        // An .abt source is self-describing: geographic degrees by format
        // definition. A crs field on it is refused rather than ignored — the
        // caller is asserting something the format cannot honor. (The nodata
        // twin of this error lives in load_source_to_ram.)
        if img.is_abt {
            if spec.crs.is_some() {
                anyhow::bail!(
                    "source {:?}: .abt sources are self-describing — remove \
                     \"crs\" from this source (the header fixes the geometry \
                     to geographic WGS84 degrees)",
                    spec.path
                );
            }
            kinds.push(SourceKind::Geographic);
            continue;
        }
        let crs = match (&spec.crs, &img.embedded_crs) {
            (Some(c), _) => c.clone(),
            (None, Some(c)) => c.clone(),
            (None, None) => anyhow::bail!(
                "source {:?}: no CRS — the job does not set one and the file's \
                 GeoKeyDirectory carries no usable EPSG code (absent or \
                 user-defined/32767). Add \"crs\": \"EPSG:nnnn\" (or a proj \
                 string starting with '+') to this source.",
                spec.path
            ),
        };
        let proj_string = crs_to_proj_string(&crs, &spec.path)?;
        let kind = match proj_cache.get(&proj_string) {
            Some((group_idx, _)) => SourceKind::Projected(*group_idx),
            None => {
                let proj = Proj::from_proj_string(&proj_string).map_err(|e| {
                    anyhow::anyhow!(
                        "source {:?}: crs {:?} did not parse as a projection: {e}",
                        spec.path, crs
                    )
                })?;
                if proj.is_latlong() {
                    SourceKind::Geographic
                } else {
                    let group_idx = groups.len();
                    let proj = Arc::new(proj);
                    proj_cache.insert(proj_string, (group_idx, proj.clone()));
                    groups.push(ProjGroup { proj, min_scale: img.scale });
                    SourceKind::Projected(group_idx)
                }
            }
        };
        if let SourceKind::Projected(gi) = kind {
            groups[gi].min_scale = groups[gi].min_scale.min(img.scale);
        }
        kinds.push(kind);
    }

    let deg_per_meter = 1.0 / 111111.0;
    let pixel_deg = job.resolution_m * deg_per_meter;
    // Cell-CENTRE grid (CONTRACT §6: a pixel averages the ground footprint it
    // stands for). Anchoring the ±half-pixel window on the NW corner instead
    // sampled everything half an output pixel to the north-west.
    let ul_lon_c = job.ul_lon + 0.5 * pixel_deg;
    let ul_lat_c = job.ul_lat - 0.5 * pixel_deg;
    let out_size = job.size_px as usize;
    let total_pixels = out_size * out_size;
    let mut buffer = vec![0i16; total_pixels];

    // What a pixel no source covers becomes: the void sentinel, or the job's
    // requested fill elevation (round half away from zero, saturating i16).
    let void_fill: i16 = match job.void_fill_m {
        Some(m) => {
            let half_metres = (m * 2.0).round();
            if half_metres >= f64::from(i16::MAX) {
                i16::MAX
            } else if half_metres <= f64::from(i16::MIN) {
                i16::MIN
            } else {
                half_metres as i16
            }
        }
        None => VOID_ELEV,
    };

    // Per-column source spans for each *geographic* source, computed once for
    // the whole tile: such a source is axis-aligned in degrees, so a column's
    // footprint is the same on every row. `(0, 0)` means the column falls
    // outside the source. (This is the historical base_tif fast path,
    // byte-identical.)
    let geo_x_spans: Vec<Option<Vec<(u32, u32)>>> = kinds
        .iter()
        .zip(&images)
        .map(|(kind, img)| match kind {
            SourceKind::Projected(_) => None,
            SourceKind::Geographic => {
                let half = 0.5 * pixel_deg / img.scale;
                Some(
                    (0..out_size)
                        .map(|x| {
                            let pixel_lon = ul_lon_c + (x as f64 * pixel_deg);
                            if pixel_lon >= img.origin_e && pixel_lon < img.limit_e {
                                let px_f = (pixel_lon - img.origin_e) / img.scale;
                                if px_f >= 0.0 && (px_f as u32) < img.width {
                                    return sample_span(px_f - half, px_f + half, img.width);
                                }
                            }
                            (0, 0)
                        })
                        .collect(),
                )
            }
        })
        .collect();

    /// One row-ready source, in job priority order.
    enum RowSrc<'a> {
        Geo { img: &'a LoadedImage, spans: &'a [(u32, u32)], py0: u32, py1: u32 },
        // `inv` carries 1/scale so the per-pixel footprint maths is multiplies,
        // not a division per pixel per candidate image.
        Prj { img: &'a LoadedImage, inv: f64, geom_idx: usize },
    }

    // Terrain Rasterization
    #[cfg(feature = "native")]
    let iter = buffer.par_chunks_mut(out_size);
    #[cfg(not(feature = "native"))]
    let iter = buffer.chunks_mut(out_size);
    // Pixels a source actually reached, folded once per row. Counted before
    // `void_fill` rewrites the holes, which is the only place the answer still
    // exists: after the fill, an uncovered tile and a real sea-level one are
    // the same bytes.
    let covered_px = AtomicU64::new(0);
    iter.enumerate().for_each(|(y, row_buffer)| {
        let mut row_covered: u64 = 0;
        let row_lat = ul_lat_c - (y as f64 * pixel_deg);

        // The projected geometry of this row, one per CRS group. `row_lat` is
        // the row's cell-centre parallel; the second latitude one pixel south
        // gets its own transforms because the cell footprint (±half a pixel
        // around the centre) needs a northing extent, and a projected northing
        // of a WGS84 parallel drifts along the row.
        let row_geoms: Vec<Option<RowGeom>> = groups
            .iter()
            .map(|g| {
                make_row_geom(
                    &wgs84, &g.proj, row_lat, row_lat - pixel_deg,
                    ul_lon_c, pixel_deg, out_size, g.min_scale,
                )
            })
            .collect();

        // Row candidates, in source priority order.
        let mut row_srcs: Vec<RowSrc> = Vec::with_capacity(sources.len());
        for ((kind, img), spans) in kinds.iter().zip(&images).zip(&geo_x_spans) {
            match kind {
                SourceKind::Geographic => {
                    if row_lat <= img.origin_n && row_lat >= img.limit_n {
                        let py_f = (img.origin_n - row_lat) / img.scale;
                        if py_f >= 0.0 {
                            let py = py_f as u32;
                            if py < img.height {
                                // The cell's own row band, hoisted: it is the
                                // same for every pixel of this output row.
                                let half = 0.5 * pixel_deg / img.scale;
                                let (py0, py1) =
                                    sample_span(py_f - half, py_f + half, img.height);
                                row_srcs.push(RowSrc::Geo {
                                    img,
                                    spans: spans.as_deref().unwrap(),
                                    py0,
                                    py1,
                                });
                            }
                        }
                    }
                }
                SourceKind::Projected(gi) => {
                    if let Some(geom) = &row_geoms[*gi] {
                        if img.origin_n >= geom.min_n && img.limit_n <= geom.max_n
                            && img.limit_e >= geom.min_e && img.origin_e <= geom.max_e
                        {
                            row_srcs.push(RowSrc::Prj { img, inv: 1.0 / img.scale, geom_idx: *gi });
                        }
                    }
                }
            }
        }

        for (x, out_pixel) in row_buffer.iter_mut().enumerate() {
            let mut val = VOID_ELEV;
            // Consecutive candidates in the same CRS reuse the pixel's
            // transformed cell footprint — one lerp per pixel per CRS, so a
            // stack of same-CRS tiles costs what the old single-CRS loop cost.
            let mut cached_geom = usize::MAX;
            let mut cell = (0.0f64, 0.0f64, 0.0f64, 0.0f64, 0.0f64, 0.0f64);

            for src in &row_srcs {
                match src {
                    RowSrc::Geo { img, spans, py0, py1 } => {
                        let (px0, px1) = spans[x];
                        if px0 < px1 {
                            if let Some(v) = sample_cell(img, px0, px1, *py0, *py1) {
                                val = v;
                                break;
                            }
                        }
                    }
                    RowSrc::Prj { img, inv, geom_idx } => {
                        if *geom_idx != cached_geom {
                            // The cell this output pixel stands for: one
                            // output pixel wide and one output row tall,
                            // centred on the cell's own centre (e, n) — the
                            // ground footprint CONTRACT §6 promises the
                            // average is taken over.
                            let geom = row_geoms[*geom_idx].as_ref().unwrap();
                            let seg = &geom.segs[x >> geom.shift];
                            let lx = (x & geom.mask) as f64;
                            let e = seg.e0 + seg.step_e * lx;
                            let n = seg.n0 + seg.step_n * lx;
                            let e_lo = e - seg.step_e * 0.5;
                            let e_hi = e + seg.step_e * 0.5;
                            let half_n = (n - (seg.nlo0 + seg.step_nlo * lx)) * 0.5;
                            cell = (e, n, e_lo, e_hi, n + half_n, n - half_n);
                            cached_geom = *geom_idx;
                        }
                        let (e, n, e_lo, e_hi, n_hi, n_lo) = cell;
                        if n <= img.origin_n && n >= img.limit_n
                            && e >= img.origin_e && e < img.limit_e
                        {
                            let (px0, px1) = sample_span(
                                (e_lo - img.origin_e) * inv,
                                (e_hi - img.origin_e) * inv,
                                img.width,
                            );
                            let (py0, py1) = sample_span(
                                (img.origin_n - n_hi) * inv,
                                (img.origin_n - n_lo) * inv,
                                img.height,
                            );
                            // First source that has real ground under the cell
                            // wins, as before. The footprint is clipped to that
                            // one image, so a cell straddling two source tiles
                            // averages the part inside the tile its corner
                            // landed in — still an average of real terrain.
                            if let Some(v) = sample_cell(img, px0, px1, py0, py1) {
                                val = v;
                                break;
                            }
                        }
                    }
                }
            }

            if val == VOID_ELEV {
                *out_pixel = void_fill;
            } else {
                row_covered += 1;
                *out_pixel = val;
            }
        }
        covered_px.fetch_add(row_covered, Ordering::Relaxed);
    });
    let stats = TileStats {
        covered_px: covered_px.load(Ordering::Relaxed),
        total_px: total_pixels as u64,
    };

    // Building Rasterization
    if let Some(fgb) = &job.buildings_file {
        // A buildings source that cannot be read fails the tile, exactly as an
        // unusable `buildings_pbf_dir` does below. This used to warn and exit 0,
        // which wrote building-less terrain under an identity that claims
        // buildings — a cache hit forever, for every later run.
        if let Err(e) = apply_buildings(&job, &mut buffer, fgb, pixel_deg) {
            anyhow::bail!(
                "failed to apply buildings from buildings_file {:?} to {:?}: {:#}; \
                 refusing to write a building-less tile",
                fgb,
                job.output_path,
                e
            );
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

    if let Some(dir) = job.output_path.parent() {
        fs::create_dir_all(dir)
            .with_context(|| format!("creating output directory {:?}", dir))?;
    }
    let f = File::create(&job.output_path)
        .with_context(|| format!("creating output tile {:?}", job.output_path))?;
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

    Ok(stats)
}

/// Burn a `buildings_file` — one `.fgb`, or a directory of them — into *buffer*.
///
/// Errors here are fatal to the tile. A source that cannot be opened, parsed or
/// scanned used to be a warning and an exit 0, which handed the caller a
/// building-*less* tile under an identity claiming buildings; the caller then
/// cached it and every later run was a hit. Finding **no** buildings over this
/// particular tile is not that: it is the ordinary fate of an edge tile, so it
/// is reported and the terrain is written.
fn apply_buildings(job: &IngestJob, buffer: &mut [i16], fgb_target: &Path, px_deg: f64) -> Result<()> {
    let mut files_to_process = Vec::new();
    if fgb_target.is_dir() {
        let entries = fs::read_dir(fgb_target)
            .with_context(|| format!("buildings_file directory {:?} cannot be read", fgb_target))?;
        for entry in entries.flatten() {
            if entry.path().extension().map_or(false, |ext| ext == "fgb") {
                files_to_process.push(entry.path());
            }
        }
        if files_to_process.is_empty() {
            anyhow::bail!("buildings_file directory {:?} holds no .fgb file", fgb_target);
        }
        // Directory order is not defined; sorting makes the reported counts
        // reproducible run to run. The drawn surface never depended on it —
        // `max` compositing is order-independent.
        files_to_process.sort();
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
    let mut features_seen = 0usize;

    for fgb_path in files_to_process {
        let file = File::open(&fgb_path)
            .with_context(|| format!("buildings_file {:?} cannot be opened", fgb_path))?;
        let fgb = FgbReader::open(BufReader::new(file))
            .with_context(|| format!("buildings_file {:?} is not a readable FlatGeobuf", fgb_path))?;

        if let Some(env) = fgb.header().envelope() {
            if env.get(0) > max_lon || env.get(2) < min_lon || env.get(1) > max_lat || env.get(3) < min_lat {
                continue;
            }
        }

        // A FlatGeobuf writes the geometry type ONCE, in the header; a feature
        // repeats it only in a mixed-type dataset, where the header says
        // `Unknown`. Reading it off the feature alone — which this did — makes
        // every feature of an ordinary single-type file look like `Unknown`
        // and drops the lot. This is the resolution order the format's own
        // reader uses (`flatgeobuf::geometry_reader::read_geometry`).
        let header_type = fgb.header().geometry_type();

        let mut features = fgb
            .select_bbox(min_lon, min_lat, max_lon, max_lat)
            .with_context(|| format!("buildings_file {:?} cannot be scanned", fgb_path))?;
        while let Some(feature) = features.next()? {
            let Some(geo) = feature.geometry() else { continue };
            let g_type = match geo.type_() {
                GeometryType::Unknown => header_type,
                t => t,
            };
            // TIN and PolyhedralSurface are polygon collections (the shape
            // GDAL writes for swissBUILDINGS3D 3.0 solids/roofs); a Triangle
            // is a 3-point ring. All walk through `collect_fgb_buildings`
            // exactly like a MultiPolygon — parts recurse, rings split on
            // `ends` — so refusing them dropped whole national datasets on
            // the floor with nothing but the "no footprint to draw" line.
            if !matches!(
                g_type,
                GeometryType::Polygon
                    | GeometryType::MultiPolygon
                    | GeometryType::PolyhedralSurface
                    | GeometryType::TIN
                    | GeometryType::Triangle
            ) {
                continue;
            }
            features_seen += 1;

            // Read every attribute column once, then walk the shared ladder
            // over them. This is only consulted for a geometry with no Z —
            // see `collect_fgb_buildings` — but resolving it here keeps it one
            // property scan per feature rather than one per ring.
            let props = feature.properties().unwrap_or_default();
            let fallback = resolve_building_height(None, |key| {
                props.get(key).map(String::as_str).and_then(parse_leading_metres)
            });

            collect_fgb_buildings(&geo, fallback, &mut all_buildings);
        }
    }

    if all_buildings.is_empty() {
        // Not an error — an edge tile with no buildings over it is ordinary —
        // but never again silent: this line is the only thing separating a
        // burn that drew nothing from one that never ran.
        println!(
            "[Buildings] {}: {} polygon feature(s) in this tile's extent, \
             no footprint to draw",
            fgb_target.display(),
            features_seen
        );
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

    let measured = all_buildings.iter().filter(|b| b.source.is_measured()).count();
    println!(
        "[Buildings] {}: {} polygon feature(s) → {} footprint(s), {} with a height \
         from the data ({} at the {} m default), {} drawn, {} px raised{}",
        fgb_target.display(),
        features_seen,
        all_buildings.len(),
        measured,
        all_buildings.len() - measured,
        DEFAULT_HEIGHT_M,
        stats.buildings_hit,
        stats.pixels_modified,
        if stats.pixels_modified == 0 {
            " — the surface is unchanged"
        } else {
            ""
        },
    );

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
/// The roof comes off the geometry Z when there is one — an **absolute**
/// elevation, which is what this path has always used and still the top rung
/// of [`resolve_building_height`]. A 2D FlatGeobuf has no Z at all, and used to
/// return here on its first line: every feature was dropped, the building list
/// came back empty and the burn was a silent no-op. Such a geometry now falls
/// back to *fallback* — the height the caller resolved from this feature's
/// attribute columns, always **above ground** — so a 2D file with `height` or
/// `levels` columns draws the buildings it has always described.
///
/// *fallback* is resolved once per feature, not per ring: a multi-polygon's
/// parts are one building's outline and share its attributes. Each part still
/// takes its own Z, exactly as before.
fn collect_fgb_buildings(
    geo: &flatgeobuf::Geometry,
    fallback: (BuildingHeight, HeightSource),
    out: &mut Vec<Building>,
) {
    if let Some(parts) = geo.parts() {
        if parts.len() > 0 {
            for i in 0..parts.len() {
                collect_fgb_buildings(&parts.get(i), fallback, out);
            }
            return;
        }
    }

    let xy = match geo.xy() { Some(v) => v, None => return };

    let (height, source) = match geo.z() {
        Some(z_vals) => {
            let mut max_z: f64 = -1000.0;
            for z in z_vals {
                if z > max_z { max_z = z; }
            }
            // Preserved from the original write loop: a roof that lands below
            // zero in half-metre units is dropped rather than drawn. This is a
            // property of an *absolute* roof only — an above-ground height is
            // screened by the rasterizer instead, against real terrain.
            if (max_z * 2.0).trunc() < 0.0 { return; }
            (BuildingHeight::Absolute(max_z), HeightSource::AbsoluteZ)
        }
        None => fallback,
    };

    let mut push_ring = |stop_idx: usize, start_idx: usize, out: &mut Vec<Building>| {
        let count = (stop_idx - start_idx) / 2;
        if count < 3 { return; }
        let mut coords: Vec<(f64, f64)> = Vec::with_capacity(count);
        let mut i = start_idx;
        while i < stop_idx {
            coords.push((xy.get(i), xy.get(i + 1)));
            i += 2;
        }
        out.push(Building { coords, height, source });
    };

    // `ends` counts POINTS, not `xy` slots — the format's own reader shifts
    // each entry left by one to index `xy` (`geometry_reader::read_polygon`).
    // Taking them as `xy` offsets, as this did, read the first half of each
    // ring and closed the footprint through the middle of the building.
    if let Some(ends_vec) = geo.ends() {
        let mut start = 0;
        for end in ends_vec {
            let stop = (end as usize) * 2;
            push_ring(stop, start, out);
            start = stop;
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
            embedded_crs: None,
            is_abt: false,
        }
    }

    /// Convert one tile from an in-memory base DEM and read the payload back.
    fn convert_with_base(mut job: IngestJob, base: LoadedImage) -> Vec<Vec<i16>> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("base.tif");
        job.output_path = dir.path().join("tile.abt");
        // Exercises the deprecated base_tif shim: the shim maps it to a
        // trailing EPSG:4326 source, whose geographic sampling path must stay
        // byte-identical to the historical base path these tests pin.
        job.base_tif = Some(path.clone());
        let mut cache = HashMap::new();
        cache.insert((path, None), Arc::new(base));
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
        // Base three times finer than the target: every output pixel owns the
        // clean 3x3 block that IS its ground footprint — cell (x,y) covers
        // source pixels [3x, 3x+3) x [3y, 3y+3), centred on (3x+1, 3y+1).
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
        let expect: Vec<(usize, usize, i16)> = (0..4)
            .flat_map(|y| (0..4).map(move |x| (x, y, 0)))
            .map(|(x, y, _)| (x, y, block_mean(3 * x + 1, 3 * y + 1)))
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
        // The fallback takes the source pixel the cell's CENTRE sits in,
        // spelled with the same truncation the code uses, so the comparison
        // holds on the cells where the arithmetic lands a hair either side of
        // a source-pixel boundary.
        let expect: Vec<Vec<i16>> = (0..8)
            .map(|y| {
                let py = (((y as f64 + 0.5) * pixel_deg) / scale) as usize;
                (0..8)
                    .map(|x| {
                        let px = (((x as f64 + 0.5) * pixel_deg) / scale) as usize;
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
            sources: Vec::new(),
            void_fill_m: None,
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