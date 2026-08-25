// rust/aether_converter/src/download.rs
//
// Async streaming XYZ terrain downloader → .abt directly.
// Uses reqwest (HTTP/2, connection pooling, true async I/O) with
// tokio for maximum download throughput.
// Processes tiles in strips to bound memory.

use anyhow::Result;
use byteorder::{LittleEndian, WriteBytesExt};
use futures::stream::{self, StreamExt};
use serde::Deserialize;
#[cfg(feature = "native")]
use std::fs::{self, File};
use std::io::Write;
#[cfg(feature = "native")]
use std::io::{BufWriter, Seek, SeekFrom};
#[cfg(feature = "native")]
use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;

// std::time::Instant panics on wasm32 — use a simple f64 (ms) wrapper instead.
#[cfg(not(target_arch = "wasm32"))]
use std::time::Instant;

#[cfg(target_arch = "wasm32")]
#[derive(Clone, Copy)]
struct Instant(f64);

#[cfg(target_arch = "wasm32")]
impl Instant {
    fn now() -> Self { Self(js_sys::Date::now()) }
    fn elapsed(&self) -> std::time::Duration {
        std::time::Duration::from_secs_f64((js_sys::Date::now() - self.0) / 1000.0)
    }
}
#[cfg(feature = "native")]
use sysinfo::{Disks, System};
use tokio::sync::Semaphore;

#[derive(Deserialize, Debug, Clone)]
pub struct DownloadJob {
    pub url_template: String,
    pub encoding: String,
    pub output_dir: PathBuf,
    pub tiles: Vec<SubTileSpec>,
    pub zoom: u32,
    pub max_connections: Option<usize>,
    /// Directory of `{z}_{x}_{y}.pbf` vector tiles to fuse onto the downloaded
    /// terrain, in the same encoding `IngestJob::buildings_pbf_dir` accepts.
    ///
    /// Optional and absent from older jobs, so existing callers are unaffected.
    /// Without it a caller that wants buildings has to abandon this downloader
    /// entirely and route terrain through a GeoTIFF export + ingest, which is
    /// one to two orders of magnitude slower.
    #[serde(default)]
    pub buildings_pbf_dir: Option<PathBuf>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct SubTileSpec {
    pub filename: String,
    pub ul_lat: f64,
    pub ul_lon: f64,
    pub size_px: u32,
    pub resolution_m: f64,
}

// -- Tile math ----------------------------------------------------------------

pub fn lon2tx(lon: f64, z: u32) -> u32 {
    let n = 2f64.powi(z as i32);
    ((lon + 180.0) / 360.0 * n).floor().max(0.0) as u32
}
pub fn lat2ty(lat: f64, z: u32) -> u32 {
    let n = 2f64.powi(z as i32);
    let r = lat.to_radians();
    ((1.0 - r.tan().asinh() / std::f64::consts::PI) / 2.0 * n).floor().max(0.0) as u32
}
pub fn ty2lat(y: u32, z: u32) -> f64 {
    let n = 2f64.powi(z as i32);
    (std::f64::consts::PI * (1.0 - 2.0 * y as f64 / n)).sinh().atan().to_degrees()
}
pub fn tx2lon(x: u32, z: u32) -> f64 {
    let n = 2f64.powi(z as i32);
    x as f64 / n * 360.0 - 180.0
}

// -- Decode -------------------------------------------------------------------

#[inline]
pub fn dec_terrarium(r: u8, g: u8, b: u8) -> f32 {
    (r as f32 * 256.0 + g as f32 + b as f32 / 256.0) - 32768.0
}
#[inline]
pub fn dec_mapbox(r: u8, g: u8, b: u8) -> f32 {
    -10000.0 + (r as f32 * 6553.6 + g as f32 * 25.6 + b as f32 * 0.1)
}

pub fn decode_png(body: &[u8], dec: fn(u8, u8, u8) -> f32) -> Result<Vec<f32>> {
    let decoder = png::Decoder::new(std::io::Cursor::new(body));
    let mut reader = decoder.read_info()?;
    let info = reader.info().clone();
    let mut buf = vec![0u8; reader.output_buffer_size()];
    let frame = reader.next_frame(&mut buf)?;
    let bytes = &buf[..frame.buffer_size()];
    let (w, h) = (info.width as usize, info.height as usize);
    let ch = if info.color_type == png::ColorType::Rgba { 4 } else { 3 };
    let mut out = vec![0.0f32; w * h];
    for i in 0..w * h {
        let o = i * ch;
        if o + 2 < bytes.len() {
            out[i] = dec(bytes[o], bytes[o + 1], bytes[o + 2]);
        }
    }
    Ok(out)
}

// Terrarium NODATA backfill ---------------------------------------------------
//
// Terrarium has NODATA voids at high zoom — whole or partial blank tiles
// (RGB 0,0,0 -> -32768 m) where the COARSER parent tile still holds real data
// (verified: 14/8646/5700 is blank but its 13/4323/2850 parent is full).
// Passing those through bakes a -16 km pit that renders as a stripe. Instead,
// replace only the void pixels with the correctly-mapped (upsampled) parent
// pixel, climbing z-1, z-2, ... until filled or MIN_ZOOM is reached.

const NODATA_M: f32 = -11000.0; // below the deepest ocean -> anything lower is a void
const NODATA_FILL_MIN_ZOOM: u32 = 6;

// Assembly resampling ---------------------------------------------------------
//
// An output cell covers `resolution_m / (tile ground sample distance)` source
// samples, and `tile_zoom` picks a zoom slightly FINER than the target, so that
// ratio sits just above 1 — a footprint of one or two samples per axis. Taking
// one of them and dropping the rest (what the nearest-neighbour lookup here used
// to do) aliases the discarded samples into the tile as speckle, so the cell is
// area-averaged over its whole footprint instead.

/// The half-open span of source samples whose **centres** fall inside `[lo, hi)`,
/// in source-sample units, clipped to `[0, n)`.
///
/// Sample `i` covers `[i, i+1)` and is centred at `i + 0.5`, so the cell owns
/// `round(lo) .. round(hi)`. An empty span means the output cell is finer than
/// the source: it falls back to the single sample **containing** the cell's
/// centre, which is the continuous limit of the same rule — at ratio 1 the one
/// sample whose centre is inside the cell is the one containing the cell's
/// centre, so nothing jumps as the ratio crosses 1.
///
/// The old lookup rounded the centre's grid coordinate to an index instead
/// (`((lon - gul_lon) / gpx).round()`), which reads a grid whose samples are
/// centred on integers. The assembly grid is not that grid — column `i` covers
/// `[i, i+1)` — so that lookup sat half a source sample east of, and south of,
/// where it belonged. Correcting it moves the sampled point, so `.abt` bytes
/// change for that reason as well as for the averaging.
#[inline]
fn grid_span(lo: f64, hi: f64, n: usize) -> (usize, usize) {
    // The nudge decides a sample centre that lands exactly on a cell edge the
    // same way every time, instead of leaving it to the last bit of the
    // coordinate arithmetic; adjacent cells share the edge, so the spans still
    // tile. A billionth of a sample is far below any real geometry.
    const EPS: f64 = 1e-9;
    let start = (lo + 0.5 + EPS) as usize;
    let end = ((hi + 0.5 + EPS) as usize).min(n);
    if start < end {
        return (start, end);
    }
    let c = (lo + hi) * 0.5;
    if c >= 0.0 {
        let i = c as usize;
        if i < n {
            return (i, i + 1);
        }
    }
    (0, 0)
}

/// The source rows of a 256-row tile-row that one output row covers.
///
/// Callers only reach this for an output row whose centre lies inside the
/// tile-row, so an empty span is a floating-point edge case at the boundary;
/// it resolves to the clamped nearest row, as the old `.round().min(255)` did.
/// A footprint that reaches past the tile-row is clipped to it — the row is
/// averaged over the part of its footprint this tile-row holds, because the
/// assembly only ever has one tile-row of the grid in memory.
#[inline]
fn tile_row_span(lo: f64, hi: f64) -> (usize, usize) {
    let (a, b) = grid_span(lo, hi, 256);
    if a < b {
        (a, b)
    } else {
        let c = (((lo + hi) * 0.5) as usize).min(255);
        (c, c + 1)
    }
}

/// Per-output-column source spans into the assembly grid, built once per tile.
///
/// Column `x` covers `[ul_lon + x*pd, ul_lon + (x+1)*pd)`; `gul_lon`/`gpx` place
/// and scale the grid. A column whose footprint misses the grid entirely gets an
/// empty span, which [`avg_cell`] writes as 0 m — what a missing tile already
/// produces.
fn x_span_table(
    sz: usize, ul_lon: f64, pd: f64, gul_lon: f64, gpx: f64, gw: usize,
) -> Vec<(u32, u32)> {
    (0..sz)
        .map(|x| {
            let lo = (ul_lon + x as f64 * pd - gul_lon) / gpx;
            let hi = (ul_lon + (x + 1) as f64 * pd - gul_lon) / gpx;
            let (a, b) = grid_span(lo, hi, gw);
            (a as u32, b as u32)
        })
        .collect()
}

/// Metres to the `.abt` half-metre unit, rounding half away from zero.
///
/// The same value `(v * 2.0).round() as i16` produces, without the libm call
/// `f32::round` lowers to on a baseline x86-64 target — this runs once per
/// output pixel of every tile. Adding the half is exact here: an elevation in
/// half-metres is far below the 2^24 at which an `f32` stops holding integers.
#[inline(always)]
fn half_metres(v: f32) -> i16 {
    let t = v * 2.0;
    (t + if t >= 0.0 { 0.5 } else { -0.5 }) as i16
}

/// Area-average one output cell out of the assembly grid, in half-metres.
///
/// Voids are excluded from the mean rather than averaged into it: a Terrarium
/// pixel the parent-tile backfill could not repair is ~-32768 m, and letting one
/// into a mean would drag the whole cell into a pit. A cell with nothing but
/// voids under it passes one through, exactly as the point sample did, and a
/// cell whose footprint misses the grid entirely stays 0 m — what a tile that
/// failed to download already writes.
///
/// One-sample cells (the target finer than the source) skip the accumulator
/// entirely, so they cost what the nearest-neighbour lookup cost.
#[inline(always)]
fn avg_cell(grid: &[f32], gw: usize, c0: usize, c1: usize, r0: usize, r1: usize) -> i16 {
    if c0 >= c1 {
        return 0;
    }
    if c1 - c0 == 1 && r1 - r0 == 1 {
        return half_metres(grid[r0 * gw + c0]);
    }
    // Sum and track the minimum in one branchless pass. A void is far below any
    // terrain, so the minimum alone says whether one is present, and the
    // exclusion pass then runs only for the handful of cells that touch one.
    let mut sum = 0.0f32;
    let mut lo = f32::MAX;
    for r in r0..r1 {
        let off = r * gw;
        for &v in &grid[off + c0..off + c1] {
            sum += v;
            lo = lo.min(v);
        }
    }
    let n = (r1 - r0) * (c1 - c0);
    if lo > NODATA_M {
        return half_metres(sum * recip(n));
    }
    let mut vsum = 0.0f32;
    let mut cnt = 0u32;
    for r in r0..r1 {
        let off = r * gw;
        for &v in &grid[off + c0..off + c1] {
            if v > NODATA_M {
                vsum += v;
                cnt += 1;
            }
        }
    }
    if cnt == 0 {
        return half_metres(grid[r0 * gw + c0]);
    }
    half_metres(vsum * recip(cnt as usize))
}

/// `1.0 / n`, from a table for the small counts a footprint actually has.
#[inline(always)]
fn recip(n: usize) -> f32 {
    const R: [f32; 17] = [
        0.0, 1.0, 1.0 / 2.0, 1.0 / 3.0, 1.0 / 4.0, 1.0 / 5.0, 1.0 / 6.0, 1.0 / 7.0,
        1.0 / 8.0, 1.0 / 9.0, 1.0 / 10.0, 1.0 / 11.0, 1.0 / 12.0, 1.0 / 13.0,
        1.0 / 14.0, 1.0 / 15.0, 1.0 / 16.0,
    ];
    if n < R.len() { R[n] } else { 1.0 / n as f32 }
}

/// One fetch + decode of a single tile (no retry) -> 256x256 grid, or None.
async fn fetch_decode_raw(
    client: &reqwest::Client, url: &str, dec: fn(u8, u8, u8) -> f32,
) -> Option<Vec<f32>> {
    let resp = client.get(url).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let body = resp.bytes().await.ok()?;
    decode_png(&body, dec).ok()
}

/// Fill the NODATA pixels of a zoom-`z` tile from progressively coarser parents.
async fn fill_from_parents(
    client: &reqwest::Client, url_template: &str, z: u32, x: u32, y: u32,
    dec: fn(u8, u8, u8) -> f32, mut grid: Vec<f32>,
) -> Vec<f32> {
    let mut missing: Vec<usize> =
        (0..grid.len()).filter(|&i| grid[i] <= NODATA_M).collect();
    let mut level = 1u32;
    while !missing.is_empty() && z >= level + NODATA_FILL_MIN_ZOOM {
        let (az, ax, ay) = (z - level, x >> level, y >> level);
        let url = url_template
            .replace("{z}", &az.to_string())
            .replace("{x}", &ax.to_string())
            .replace("{y}", &ay.to_string());
        if let Some(anc) = fetch_decode_raw(client, &url, dec).await {
            if anc.len() == 256 * 256 {
                let (base_x, base_y) = (ax as u64 * 256, ay as u64 * 256);
                missing.retain(|&i| {
                    let (px, py) = ((i % 256) as u64, (i / 256) as u64);
                    let apx = (((x as u64) * 256 + px) >> level) - base_x;
                    let apy = (((y as u64) * 256 + py) >> level) - base_y;
                    let v = anc[(apy * 256 + apx) as usize];
                    if v > NODATA_M { grid[i] = v; false } else { true }
                });
            }
        }
        level += 1;
    }
    grid
}

// -- Download diagnostics -----------------------------------------------------

struct DownloadStats {
    ok_count: AtomicUsize,
    bytes_downloaded: AtomicU64,
    retries: AtomicUsize,
    err_timeout: AtomicUsize,
    err_connect: AtomicUsize,
    err_http_429: AtomicUsize,
    err_http_4xx: AtomicUsize,
    err_http_5xx: AtomicUsize,
    err_decode: AtomicUsize,
    err_other: AtomicUsize,
    in_flight: AtomicUsize,
    peak_in_flight: AtomicUsize,
    tile_us_sum: AtomicU64,
    tile_us_max: AtomicU64,
    tile_us_min: AtomicU64,
    slow_logged: AtomicUsize,
}

impl DownloadStats {
    fn new() -> Self {
        Self {
            ok_count: AtomicUsize::new(0),
            bytes_downloaded: AtomicU64::new(0),
            retries: AtomicUsize::new(0),
            err_timeout: AtomicUsize::new(0),
            err_connect: AtomicUsize::new(0),
            err_http_429: AtomicUsize::new(0),
            err_http_4xx: AtomicUsize::new(0),
            err_http_5xx: AtomicUsize::new(0),
            err_decode: AtomicUsize::new(0),
            err_other: AtomicUsize::new(0),
            in_flight: AtomicUsize::new(0),
            peak_in_flight: AtomicUsize::new(0),
            tile_us_sum: AtomicU64::new(0),
            tile_us_max: AtomicU64::new(0),
            tile_us_min: AtomicU64::new(u64::MAX),
            slow_logged: AtomicUsize::new(0),
        }
    }

    fn total_errors(&self) -> usize {
        self.err_timeout.load(Ordering::Relaxed)
            + self.err_connect.load(Ordering::Relaxed)
            + self.err_http_429.load(Ordering::Relaxed)
            + self.err_http_4xx.load(Ordering::Relaxed)
            + self.err_http_5xx.load(Ordering::Relaxed)
            + self.err_decode.load(Ordering::Relaxed)
            + self.err_other.load(Ordering::Relaxed)
    }

    fn total_tiles(&self) -> usize {
        self.ok_count.load(Ordering::Relaxed) + self.total_errors()
    }

    /// The per-class error tally, e.g. `"decode=1369"` or `"timeout=4, HTTP_4xx=2"`.
    ///
    /// Rendered both by the `[Stats] ERRORS` line and by the fatal
    /// `Tile download failed:` bail, so a caller that only sees the failure
    /// message still learns *which* failure it was: a source serving WebP
    /// (`decode=N`) is a different repair from an unreachable one (`connect=N`).
    /// Only non-zero classes appear; `"none"` if there are no errors at all.
    fn error_breakdown(&self) -> String {
        let mut parts = Vec::new();
        let mut push = |label: &str, n: usize| {
            if n > 0 { parts.push(format!("{}={}", label, n)); }
        };
        push("timeout", self.err_timeout.load(Ordering::Relaxed));
        push("connect", self.err_connect.load(Ordering::Relaxed));
        push("HTTP_429_rate_limited", self.err_http_429.load(Ordering::Relaxed));
        push("HTTP_4xx", self.err_http_4xx.load(Ordering::Relaxed));
        push("HTTP_5xx", self.err_http_5xx.load(Ordering::Relaxed));
        push("decode", self.err_decode.load(Ordering::Relaxed));
        push("other", self.err_other.load(Ordering::Relaxed));
        if parts.is_empty() { "none".to_string() } else { parts.join(", ") }
    }

    fn log_summary(&self, elapsed: f64) {
        let ok = self.ok_count.load(Ordering::Relaxed);
        let total = self.total_tiles();
        let bytes = self.bytes_downloaded.load(Ordering::Relaxed);
        let mb = bytes as f64 / (1024.0 * 1024.0);
        let throughput = if elapsed > 0.0 { mb / elapsed } else { 0.0 };

        eprintln!("[Stats] Tiles: {}/{} OK ({:.1}% success)",
            ok, total,
            if total > 0 { ok as f64 / total as f64 * 100.0 } else { 100.0 });
        eprintln!("[Stats] Downloaded: {:.1} MB in {:.1}s = {:.1} MB/s",
            mb, elapsed, throughput);
        let retries = self.retries.load(Ordering::Relaxed);
        eprintln!("[Stats] Retries: {} (up to 3 per failed tile)", retries);
        eprintln!("[Stats] Peak concurrent requests: {}",
            self.peak_in_flight.load(Ordering::Relaxed));

        if total > 0 {
            let avg_ms = self.tile_us_sum.load(Ordering::Relaxed) as f64
                / total as f64 / 1000.0;
            let max_ms = self.tile_us_max.load(Ordering::Relaxed) as f64 / 1000.0;
            let min_raw = self.tile_us_min.load(Ordering::Relaxed);
            let min_ms = if min_raw == u64::MAX { 0.0 } else { min_raw as f64 / 1000.0 };
            eprintln!("[Stats] Tile latency: min={:.0}ms avg={:.0}ms max={:.0}ms",
                min_ms, avg_ms, max_ms);
        }

        let errs = self.total_errors();
        if errs > 0 {
            eprintln!("[Stats] ERRORS ({}): {}", errs, self.error_breakdown());

            // The hints below need three of the counts back.
            let t = self.err_timeout.load(Ordering::Relaxed);
            let c = self.err_connect.load(Ordering::Relaxed);
            let r429 = self.err_http_429.load(Ordering::Relaxed);

            if r429 > 0 {
                eprintln!("[Stats] >>> Server rate-limiting detected (HTTP 429). Reduce download connections.");
            }
            if t > total / 4 {
                eprintln!("[Stats] >>> >25% timeouts — network saturated or server overloaded.");
            }
            if c > total / 10 {
                eprintln!("[Stats] >>> >10% connection errors — too many connections for network/router.");
            }
        }
    }
}

// -- Resource helpers ---------------------------------------------------------

#[cfg(feature = "native")]
fn available_disk_space(path: &Path) -> Option<u64> {
    let canonical = std::fs::canonicalize(path)
        .or_else(|_| {
            path.parent()
                .map(std::fs::canonicalize)
                .unwrap_or_else(|| Ok(path.to_path_buf()))
        })
        .unwrap_or_else(|_| path.to_path_buf());
    let disks = Disks::new_with_refreshed_list();
    disks
        .iter()
        .filter(|d| canonical.starts_with(d.mount_point()))
        .max_by_key(|d| d.mount_point().as_os_str().len())
        .map(|d| d.available_space())
}

#[cfg(feature = "native")]
fn estimate_abt_bytes(tiles: &[SubTileSpec]) -> u64 {
    tiles.iter().map(|spec| {
        let bpr = spec.size_px as u64 * 2;
        let stride = (bpr + 255) & !255;
        44 + stride * spec.size_px as u64
    }).sum()
}

// -- .abt writer --------------------------------------------------------------

#[cfg(feature = "native")]
struct AbtWriter {
    writer: BufWriter<File>,
    size_px: u32,
    stride: usize,
}

#[cfg(feature = "native")]
impl AbtWriter {
    fn create(path: &Path, sz: u32, ul_lat: f64, ul_lon: f64, pd: f64) -> Result<Self> {
        let bpr = sz as usize * 2;
        let stride = (bpr + 255) & !255;

        let f = File::create(path)?;
        // Skip set_len pre-allocation — file grows as strips write data.
        // Prevents "disk full" from allocating all output files upfront.
        let mut w = BufWriter::with_capacity(1024 * 1024, f);

        w.write_all(b"AETH")?;
        w.write_u16::<LittleEndian>(1)?;
        w.write_u16::<LittleEndian>(sz as u16)?;
        w.write_f64::<LittleEndian>(ul_lat)?;
        w.write_f64::<LittleEndian>(ul_lon)?;
        w.write_f64::<LittleEndian>(pd)?;
        w.write_f64::<LittleEndian>(pd)?;
        w.write_i16::<LittleEndian>(0)?;
        w.write_u16::<LittleEndian>(stride as u16)?;
        w.flush()?;

        Ok(AbtWriter { writer: w, size_px: sz, stride })
    }

    fn open_existing(path: &Path, sz: u32) -> Result<Self> {
        let bpr = sz as usize * 2;
        let stride = (bpr + 255) & !255;
        let f = fs::OpenOptions::new().write(true).open(path)?;
        let w = BufWriter::with_capacity(1024 * 1024, f);
        Ok(AbtWriter { writer: w, size_px: sz, stride })
    }

    #[allow(dead_code)]
    fn write_row(&mut self, y: u32, row: &[i16]) -> Result<()> {
        let offset = 44u64 + y as u64 * self.stride as u64;
        self.writer.seek(SeekFrom::Start(offset))?;
        // Bulk write — native LE byte order matches .abt format on x86.
        self.writer.write_all(bytemuck::cast_slice(row))?;
        let pad = self.stride - self.size_px as usize * 2;
        if pad > 0 {
            const ZEROS: [u8; 256] = [0u8; 256];
            self.writer.write_all(&ZEROS[..pad])?;
        }
        Ok(())
    }

    fn finish(mut self) -> Result<()> { self.writer.flush()?; Ok(()) }
}

// -- Async download -----------------------------------------------------------

#[allow(dead_code)] // Used by run_download_async (native only); WASM uses download_strip_raw.
async fn download_strip(
    client: &reqwest::Client,
    url_template: &str,
    zoom: u32,
    x0: u32, x1: u32,
    strip_y0: u32, strip_y1: u32,
    dec: fn(u8, u8, u8) -> f32,
    concurrency: usize,
    progress: &AtomicUsize,
    total: usize,
    stats: &DownloadStats,
    start_time: Instant,
    semaphore: &Semaphore,
    #[cfg(not(target_arch = "wasm32"))]
    on_progress: Option<&(dyn Fn(usize, usize) + Send + Sync)>,
    #[cfg(target_arch = "wasm32")]
    on_progress: Option<&dyn Fn(usize, usize)>,
) -> Vec<(u32, u32, Result<Vec<f32>>)> {
    let coords: Vec<_> = (strip_y0..=strip_y1)
        .flat_map(|ty| (x0..=x1).map(move |tx| (tx, ty)))
        .collect();

    stream::iter(coords)
        .map(|(tx, ty)| {
            let client = client.clone();
            let url = url_template
                .replace("{x}", &tx.to_string())
                .replace("{y}", &ty.to_string())
                .replace("{z}", &zoom.to_string());
            let progress = progress;
            let stats = stats;
            let semaphore = semaphore;
            let on_progress = on_progress;
            async move {
                // Global concurrency gate — prevents PREFETCH_DEPTH × conns explosion.
                let permit = semaphore.acquire().await.unwrap();
                let t0 = Instant::now();
                let cur_flight = stats.in_flight.fetch_add(1, Ordering::Relaxed) + 1;
                stats.peak_in_flight.fetch_max(cur_flight, Ordering::Relaxed);

                // Retry loop: up to 5 retries with capped exponential backoff.
                // Retries on: timeout, connection error ("connection closed
                // before message completed"), HTTP 429, HTTP 5xx.
                // No retry on: HTTP 4xx (except 429), decode errors.
                // The extra attempts target the largest tiles: their first try
                // fails under peak concurrency, but once the backoff elapses the
                // strip has drained and a retry gets enough bandwidth to finish
                // — clearing the deterministic stripe without lowering the
                // connection count for the bulk download.
                const MAX_RETRIES: u32 = 5;
                let mut result: Result<Vec<f32>> = Err(anyhow::anyhow!("not started"));
                let mut retryable;

                for attempt in 0..=MAX_RETRIES {
                    if attempt > 0 {
                        stats.retries.fetch_add(1, Ordering::Relaxed);
                        // 1s, 2s, 4s, 8s, 8s — capped so a late failure can't
                        // stall the strip pipeline for the full 1+2+4+8+16s.
                        let delay_ms = (1000u64 << (attempt - 1)).min(8000);
                        tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                    }
                    retryable = true;

                    result = async {
                        let resp = client.get(&url).send().await?;
                        let status = resp.status();
                        if !status.is_success() {
                            anyhow::bail!("HTTP {}", status.as_u16());
                        }
                        let body = resp.bytes().await?;
                        stats.bytes_downloaded.fetch_add(body.len() as u64, Ordering::Relaxed);
                        decode_png(&body, dec)
                    }
                    .await;

                    if result.is_ok() { break; }

                    // Decide if this error is worth retrying.
                    if let Err(ref e) = result {
                        let msg = format!("{:#}", e);
                        if msg.contains("HTTP ") {
                            let code: u16 = msg.split("HTTP ")
                                .nth(1)
                                .and_then(|s| s.split_whitespace().next())
                                .and_then(|s| s.parse().ok())
                                .unwrap_or(0);
                            // Only retry 429 and 5xx; other 4xx are permanent.
                            if code != 429 && !(500..=599).contains(&code) {
                                retryable = false;
                            }
                        } else if msg.contains("decode")
                            || msg.contains("png")
                            || msg.contains("PNG")
                            || msg.contains("nvalid")
                        {
                            retryable = false; // Decode errors won't fix themselves.
                        }
                        // Timeouts and connection errors are retryable (default).
                    }

                    if !retryable { break; }
                }

                // Terrarium NODATA voids: if this tile came back with blank
                // pixels, backfill only those from real (upsampled) data in the
                // coarser parent tiles instead of leaving a -16 km pit / stripe.
                if matches!(&result, Ok(g) if g.iter().any(|&v| v <= NODATA_M)) {
                    if let Ok(g) = result {
                        result = Ok(fill_from_parents(
                            &client, url_template, zoom, tx, ty, dec, g,
                        ).await);
                    }
                }

                stats.in_flight.fetch_sub(1, Ordering::Relaxed);
                drop(permit);
                let tile_us = t0.elapsed().as_micros() as u64;
                stats.tile_us_sum.fetch_add(tile_us, Ordering::Relaxed);
                stats.tile_us_max.fetch_max(tile_us, Ordering::Relaxed);
                stats.tile_us_min.fetch_min(tile_us, Ordering::Relaxed);

                // Classify final result (only after all retries exhausted).
                match &result {
                    Ok(_) => { stats.ok_count.fetch_add(1, Ordering::Relaxed); }
                    Err(e) => {
                        let msg = format!("{:#}", e);
                        if msg.contains("HTTP ") {
                            let code: u16 = msg.split("HTTP ")
                                .nth(1)
                                .and_then(|s| s.split_whitespace().next())
                                .and_then(|s| s.parse().ok())
                                .unwrap_or(0);
                            match code {
                                429 => { stats.err_http_429.fetch_add(1, Ordering::Relaxed); }
                                400..=499 => { stats.err_http_4xx.fetch_add(1, Ordering::Relaxed); }
                                500..=599 => { stats.err_http_5xx.fetch_add(1, Ordering::Relaxed); }
                                _ => { stats.err_other.fetch_add(1, Ordering::Relaxed); }
                            }
                        } else if msg.contains("timed out")
                            || msg.contains("operation timed out")
                        {
                            let prev = stats.err_timeout.fetch_add(1, Ordering::Relaxed);
                            if prev == 0 {
                                eprintln!("[Download] first timeout: z={}/x={}/y={}",
                                    zoom, tx, ty);
                            }
                        } else if msg.contains("onnect")
                            || msg.contains("dns")
                            || msg.contains("resolve")
                        {
                            let prev = stats.err_connect.fetch_add(1, Ordering::Relaxed);
                            if prev == 0 {
                                eprintln!("[Download] first connect error: z={}/x={}/y={} — {}",
                                    zoom, tx, ty, msg);
                            }
                        } else if msg.contains("decode")
                            || msg.contains("png")
                            || msg.contains("PNG")
                            || msg.contains("nvalid")
                        {
                            let prev = stats.err_decode.fetch_add(1, Ordering::Relaxed);
                            if prev == 0 {
                                eprintln!("[Download] first decode error: z={}/x={}/y={} — {}",
                                    zoom, tx, ty, msg);
                            }
                        } else {
                            let prev = stats.err_other.fetch_add(1, Ordering::Relaxed);
                            if prev == 0 {
                                eprintln!("[Download] first unknown error: z={}/x={}/y={} — {}",
                                    zoom, tx, ty, msg);
                            }
                        }
                    }
                }

                // Warn about individually slow tiles.
                if tile_us > 5_000_000 {
                    let prev = stats.slow_logged.fetch_add(1, Ordering::Relaxed);
                    if prev < 5 {
                        eprintln!("[Download] SLOW tile z={}/x={}/y={}: {:.1}s",
                            zoom, tx, ty, tile_us as f64 / 1_000_000.0);
                    } else if prev == 5 {
                        eprintln!("[Download] (suppressing further slow-tile warnings)");
                    }
                }

                // Enhanced progress line with running throughput + error count.
                let done = progress.fetch_add(1, Ordering::Relaxed) + 1;
                let pct = done * 100 / total;
                let prev_pct = (done - 1) * 100 / total;

                // Fire callback every 5%
                if let Some(cb) = on_progress {
                    if pct / 5 > prev_pct / 5 || done == total || done == 1 {
                        cb(done, total);
                    }
                }

                if pct / 10 > prev_pct / 10 || done == total {
                    let elapsed = start_time.elapsed().as_secs_f64();
                    let mb = stats.bytes_downloaded.load(Ordering::Relaxed) as f64
                        / (1024.0 * 1024.0);
                    let tp = if elapsed > 0.0 { mb / elapsed } else { 0.0 };
                    let errs = stats.total_errors();
                    let in_fl = stats.in_flight.load(Ordering::Relaxed);
                    eprintln!(
                        "[Download] {}% ({}/{}) — {:.1} MB/s, {} errors, {} in-flight",
                        pct, done, total, tp, errs, in_fl
                    );
                }

                (tx, ty, result)
            }
        })
        .buffer_unordered(concurrency)
        .collect()
        .await
}

// -- Raw-bytes download (no decode) for WASM single-threaded mode -------------
//
// In WASM, `download_strip` calls `decode_png()` inside each async future.
// Since WASM is single-threaded, only one PNG decodes at a time even though
// `buffer_unordered(600)` queues many requests — the synchronous CPU work
// serialises all futures at the decode step.
//
// `download_strip_raw` separates download from decode: it fetches ALL tiles as
// raw `Vec<u8>` bytes concurrently (the browser handles ~100 HTTP/2 streams),
// then the caller decodes PNGs sequentially after the network phase completes.
// This fully saturates the network instead of alternating fetch→decode→fetch.

#[allow(dead_code)] // Used by native path; WASM uses download_all_tiles_web.
async fn download_strip_raw(
    client: &reqwest::Client,
    url_template: &str,
    zoom: u32,
    x0: u32, x1: u32,
    strip_y0: u32, strip_y1: u32,
    concurrency: usize,
    progress: &AtomicUsize,
    total: usize,
    stats: &DownloadStats,
    start_time: Instant,
    semaphore: &Semaphore,
    #[cfg(not(target_arch = "wasm32"))]
    on_progress: Option<&(dyn Fn(usize, usize) + Send + Sync)>,
    #[cfg(target_arch = "wasm32")]
    on_progress: Option<&dyn Fn(usize, usize)>,
) -> Vec<(u32, u32, Result<Vec<u8>>)> {
    let coords: Vec<_> = (strip_y0..=strip_y1)
        .flat_map(|ty| (x0..=x1).map(move |tx| (tx, ty)))
        .collect();

    stream::iter(coords)
        .map(|(tx, ty)| {
            let client = client.clone();
            let url = url_template
                .replace("{x}", &tx.to_string())
                .replace("{y}", &ty.to_string())
                .replace("{z}", &zoom.to_string());
            let progress = progress;
            let stats = stats;
            let semaphore = semaphore;
            let on_progress = on_progress;
            async move {
                // Global concurrency gate — prevents connection explosion.
                let permit = semaphore.acquire().await.unwrap();
                let t0 = Instant::now();
                let cur_flight = stats.in_flight.fetch_add(1, Ordering::Relaxed) + 1;
                stats.peak_in_flight.fetch_max(cur_flight, Ordering::Relaxed);

                // Retry loop: up to 3 retries with exponential backoff (1s, 2s, 4s).
                // Retries on: timeout, connection error, HTTP 429, HTTP 5xx.
                // No retry on: HTTP 4xx (except 429).
                const MAX_RETRIES: u32 = 3;
                let mut result: Result<Vec<u8>> = Err(anyhow::anyhow!("not started"));
                let mut retryable;

                for attempt in 0..=MAX_RETRIES {
                    if attempt > 0 {
                        stats.retries.fetch_add(1, Ordering::Relaxed);
                        let delay_ms = 1000u64 << (attempt - 1); // 1s, 2s, 4s
                        tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                    }
                    retryable = true;

                    result = async {
                        let resp = client.get(&url).send().await?;
                        let status = resp.status();
                        if !status.is_success() {
                            anyhow::bail!("HTTP {}", status.as_u16());
                        }
                        let body = resp.bytes().await?;
                        stats.bytes_downloaded.fetch_add(body.len() as u64, Ordering::Relaxed);
                        // Return raw PNG bytes — no decode here.
                        Ok(body.to_vec())
                    }
                    .await;

                    if result.is_ok() { break; }

                    // Decide if this error is worth retrying.
                    if let Err(ref e) = result {
                        let msg = format!("{:#}", e);
                        if msg.contains("HTTP ") {
                            let code: u16 = msg.split("HTTP ")
                                .nth(1)
                                .and_then(|s| s.split_whitespace().next())
                                .and_then(|s| s.parse().ok())
                                .unwrap_or(0);
                            // Only retry 429 and 5xx; other 4xx are permanent.
                            if code != 429 && !(500..=599).contains(&code) {
                                retryable = false;
                            }
                        }
                        // Timeouts and connection errors are retryable (default).
                    }

                    if !retryable { break; }
                }

                stats.in_flight.fetch_sub(1, Ordering::Relaxed);
                drop(permit);
                let tile_us = t0.elapsed().as_micros() as u64;
                stats.tile_us_sum.fetch_add(tile_us, Ordering::Relaxed);
                stats.tile_us_max.fetch_max(tile_us, Ordering::Relaxed);
                stats.tile_us_min.fetch_min(tile_us, Ordering::Relaxed);

                // Classify final result (only after all retries exhausted).
                match &result {
                    Ok(_) => { stats.ok_count.fetch_add(1, Ordering::Relaxed); }
                    Err(e) => {
                        let msg = format!("{:#}", e);
                        if msg.contains("HTTP ") {
                            let code: u16 = msg.split("HTTP ")
                                .nth(1)
                                .and_then(|s| s.split_whitespace().next())
                                .and_then(|s| s.parse().ok())
                                .unwrap_or(0);
                            match code {
                                429 => { stats.err_http_429.fetch_add(1, Ordering::Relaxed); }
                                400..=499 => { stats.err_http_4xx.fetch_add(1, Ordering::Relaxed); }
                                500..=599 => { stats.err_http_5xx.fetch_add(1, Ordering::Relaxed); }
                                _ => { stats.err_other.fetch_add(1, Ordering::Relaxed); }
                            }
                        } else if msg.contains("timed out")
                            || msg.contains("operation timed out")
                        {
                            let prev = stats.err_timeout.fetch_add(1, Ordering::Relaxed);
                            if prev == 0 {
                                eprintln!("[Download] first timeout: z={}/x={}/y={}",
                                    zoom, tx, ty);
                            }
                        } else if msg.contains("onnect")
                            || msg.contains("dns")
                            || msg.contains("resolve")
                        {
                            let prev = stats.err_connect.fetch_add(1, Ordering::Relaxed);
                            if prev == 0 {
                                eprintln!("[Download] first connect error: z={}/x={}/y={} — {}",
                                    zoom, tx, ty, msg);
                            }
                        } else {
                            let prev = stats.err_other.fetch_add(1, Ordering::Relaxed);
                            if prev == 0 {
                                eprintln!("[Download] first unknown error: z={}/x={}/y={} — {}",
                                    zoom, tx, ty, msg);
                            }
                        }
                    }
                }

                // Warn about individually slow tiles.
                if tile_us > 5_000_000 {
                    let prev = stats.slow_logged.fetch_add(1, Ordering::Relaxed);
                    if prev < 5 {
                        eprintln!("[Download] SLOW tile z={}/x={}/y={}: {:.1}s",
                            zoom, tx, ty, tile_us as f64 / 1_000_000.0);
                    } else if prev == 5 {
                        eprintln!("[Download] (suppressing further slow-tile warnings)");
                    }
                }

                // Enhanced progress line with running throughput + error count.
                let done = progress.fetch_add(1, Ordering::Relaxed) + 1;
                let pct = done * 100 / total;
                let prev_pct = (done - 1) * 100 / total;

                // Fire callback every 5%
                if let Some(cb) = on_progress {
                    if pct / 5 > prev_pct / 5 || done == total || done == 1 {
                        cb(done, total);
                    }
                }

                if pct / 10 > prev_pct / 10 || done == total {
                    let elapsed = start_time.elapsed().as_secs_f64();
                    let mb = stats.bytes_downloaded.load(Ordering::Relaxed) as f64
                        / (1024.0 * 1024.0);
                    let tp = if elapsed > 0.0 { mb / elapsed } else { 0.0 };
                    let errs = stats.total_errors();
                    let in_fl = stats.in_flight.load(Ordering::Relaxed);
                    eprintln!(
                        "[Download] {}% ({}/{}) — {:.1} MB/s, {} errors, {} in-flight",
                        pct, done, total, tp, errs, in_fl
                    );
                }

                (tx, ty, result)
            }
        })
        .buffer_unordered(concurrency)
        .collect()
        .await
}

// -- Browser-native batch download (WASM only) ----------------------------------
//
// Bypasses reqwest. Uses browser-native fetch() via Promise.allSettled.
// Domain sharding spreads tiles across multiple S3 hostnames to increase the
// browser's per-origin connection/stream budget.

/// Given an S3 path-style or virtual-hosted URL template, return multiple
/// equivalent URL templates using different S3 hostnames for domain sharding.
/// Falls back to a single template if the URL isn't recognised as S3.
#[cfg(target_arch = "wasm32")]
fn shard_s3_templates(url_template: &str) -> Vec<String> {
    // Try path-style: https://s3.amazonaws.com/BUCKET/path...
    if let Some(rest) = url_template.strip_prefix("https://s3.amazonaws.com/") {
        if let Some((bucket, path)) = rest.split_once('/') {
            return make_s3_shards(bucket, path);
        }
    }
    // Try virtual-hosted: https://BUCKET.s3.amazonaws.com/path...
    if let Some(rest) = url_template.strip_prefix("https://") {
        if let Some((host, path)) = rest.split_once('/') {
            if let Some(bucket) = host.strip_suffix(".s3.amazonaws.com") {
                return make_s3_shards(bucket, path);
            }
        }
    }
    vec![url_template.to_string()]
}

#[cfg(target_arch = "wasm32")]
fn make_s3_shards(bucket: &str, path: &str) -> Vec<String> {
    vec![
        format!("https://s3.amazonaws.com/{bucket}/{path}"),
        format!("https://{bucket}.s3.amazonaws.com/{path}"),
        format!("https://s3.us-east-1.amazonaws.com/{bucket}/{path}"),
        format!("https://{bucket}.s3.us-east-1.amazonaws.com/{path}"),
        format!("https://s3.dualstack.us-east-1.amazonaws.com/{bucket}/{path}"),
        format!("https://{bucket}.s3.dualstack.us-east-1.amazonaws.com/{path}"),
    ]
}

#[cfg(target_arch = "wasm32")]
async fn download_all_tiles_web(
    url_template: &str,
    zoom: u32,
    x0: u32, x1: u32,
    y0: u32, y1: u32,
    max_concurrent: usize,
    stats: &DownloadStats,
    start_time: Instant,
    js_progress: Option<&js_sys::Function>,
) -> std::collections::HashMap<(u32, u32), Vec<u8>> {
    use wasm_bindgen::JsCast;
    use wasm_bindgen_futures::JsFuture;

    let nx = (x1 - x0 + 1) as usize;
    let ny = (y1 - y0 + 1) as usize;
    let total = nx * ny;
    let mut result_map = std::collections::HashMap::with_capacity(total);

    // Domain sharding: expand single S3 URL into multiple hostnames.
    // Browser allows 6 HTTP/1.1 connections per hostname.
    let templates = shard_s3_templates(url_template);
    let shard_count = templates.len();
    if shard_count > 1 {
        eprintln!("[DownloadWeb] Domain sharding: {} hostnames × 6 conn = {} concurrent",
            shard_count, shard_count * 6);
    }

    // Build all tile URLs, round-robin across shards.
    let tile_info: Vec<(u32, u32, String)> = (y0..=y1)
        .flat_map(|ty| (x0..=x1).map(move |tx| (tx, ty)))
        .enumerate()
        .map(|(i, (tx, ty))| {
            let url = templates[i % shard_count]
                .replace("{z}", &zoom.to_string())
                .replace("{x}", &tx.to_string())
                .replace("{y}", &ty.to_string());
            (tx, ty, url)
        })
        .collect();

    // ── Concurrency: saturate the browser's HTTP/2 stream budget ──
    // HTTP/2 multiplexes ~100 streams per connection.  Domain sharding
    // adds parallel TCP connections when the browser uses HTTP/1.1.
    // A high in-flight count fills the pipe regardless of protocol.
    let concurrency = total.min(max_concurrent);

    // ── Streaming fetch pool with retry + 5 % progress ─────────
    // Each failed tile retries up to 3× with exponential backoff
    // (500 ms, 1 s, 1.5 s).  `prog(phase, done, total)` is called
    // every 5 % of tiles (plus first and last) so the UI stays live.
    let fetch_pool = js_sys::Function::new_with_args(
        "urls,conc,prog",
        "return new Promise(function(resolve){\
            var r=new Array(urls.length),n=0,d=0,t=urls.length,lp=-1;\
            function doFetch(i,retries){\
                fetch(urls[i]).then(function(resp){\
                    if(!resp.ok)throw new Error('HTTP '+resp.status);\
                    return resp.arrayBuffer();\
                }).then(function(buf){\
                    r[i]={status:'fulfilled',value:buf};\
                    fin();\
                }).catch(function(err){\
                    if(retries>0){\
                        setTimeout(function(){doFetch(i,retries-1)},(4-retries)*500);\
                    }else{\
                        r[i]={status:'rejected',reason:String(err)};\
                        fin();\
                    }\
                });\
            }\
            function fin(){\
                d++;\
                var p5=Math.floor(d*20/t);\
                if(prog&&(p5>lp||d===t||d===1)){lp=p5;try{prog(0,d,t)}catch(x){}}\
                if(d===t){\
                    try{\
                        var e=performance.getEntriesByType('resource'),p={};\
                        for(var j=Math.max(0,e.length-t);j<e.length;j++){\
                            var k=e[j].nextHopProtocol||'?';p[k]=(p[k]||0)+1;}\
                        console.log('[AETHER] tile protocols:',JSON.stringify(p));\
                    }catch(x){}\
                    resolve(r);\
                }else go();\
            }\
            function go(){\
                if(n>=t)return;\
                var i=n++;\
                doFetch(i,3);\
            }\
            for(var i=0;i<Math.min(conc,t);i++)go();\
        })",
    );

    // Build JS URL array — all tiles at once, no batching.
    let js_urls = js_sys::Array::new_with_length(total as u32);
    for (i, (_, _, url)) in tile_info.iter().enumerate() {
        js_urls.set(i as u32, wasm_bindgen::JsValue::from_str(url));
    }

    eprintln!("[DownloadWeb] Fetching {} tiles, concurrency={}", total, concurrency);

    let prog_val: wasm_bindgen::JsValue = match js_progress {
        Some(f) => f.clone().into(),
        None => wasm_bindgen::JsValue::NULL,
    };
    let promise = match fetch_pool.call3(
        &wasm_bindgen::JsValue::NULL,
        &js_urls,
        &wasm_bindgen::JsValue::from(concurrency as u32),
        &prog_val,
    ) {
        Ok(p) => js_sys::Promise::from(p),
        Err(e) => {
            eprintln!("[DownloadWeb] Failed to start pool: {:?}", e);
            stats.err_other.fetch_add(total, Ordering::Relaxed);
            return result_map;
        }
    };

    match JsFuture::from(promise).await {
        Ok(settled) => {
            let arr: js_sys::Array = settled.unchecked_into();
            for (i, (tx, ty, _)) in tile_info.iter().enumerate() {
                let entry = arr.get(i as u32);
                let status = js_sys::Reflect::get(&entry, &"status".into())
                    .ok()
                    .and_then(|s| s.as_string())
                    .unwrap_or_default();

                if status == "fulfilled" {
                    let value = js_sys::Reflect::get(&entry, &"value".into()).unwrap();
                    let bytes = js_sys::Uint8Array::new(&value).to_vec();
                    stats.bytes_downloaded.fetch_add(bytes.len() as u64, Ordering::Relaxed);
                    stats.ok_count.fetch_add(1, Ordering::Relaxed);
                    result_map.insert((*tx, *ty), bytes);
                } else {
                    let reason = js_sys::Reflect::get(&entry, &"reason".into())
                        .ok()
                        .and_then(|r| {
                            r.dyn_ref::<js_sys::Error>()
                                .map(|e| String::from(e.message()))
                                .or_else(|| r.as_string())
                        })
                        .unwrap_or_else(|| "unknown".into());

                    if reason.contains("429") {
                        stats.err_http_429.fetch_add(1, Ordering::Relaxed);
                    } else if reason.contains("HTTP 5") {
                        stats.err_http_5xx.fetch_add(1, Ordering::Relaxed);
                    } else if reason.contains("HTTP 4") {
                        stats.err_http_4xx.fetch_add(1, Ordering::Relaxed);
                    } else {
                        stats.err_other.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        }
        Err(e) => {
            eprintln!("[DownloadWeb] Pool rejected: {:?}", e);
            stats.err_other.fetch_add(total, Ordering::Relaxed);
        }
    }

    // Signal download-phase completion (phase 0).
    if let Some(f) = js_progress {
        let _ = f.call3(
            &wasm_bindgen::JsValue::NULL,
            &wasm_bindgen::JsValue::from(0u32),
            &wasm_bindgen::JsValue::from(total as u32),
            &wasm_bindgen::JsValue::from(total as u32),
        );
    }

    let elapsed = start_time.elapsed().as_secs_f64();
    let mb = stats.bytes_downloaded.load(Ordering::Relaxed) as f64 / (1024.0 * 1024.0);
    eprintln!(
        "[DownloadWeb] {:.1} MB in {:.1}s = {:.1} MB/s ({:.0} Mbit/s), {}/{} OK",
        mb, elapsed,
        if elapsed > 0.0 { mb / elapsed } else { 0.0 },
        if elapsed > 0.0 { mb * 8.0 / elapsed } else { 0.0 },
        result_map.len(), total,
    );

    result_map
}

// -- Entry point (sync wrapper around async) ----------------------------------

#[cfg(feature = "native")]
pub fn run_download(job_file: &Path) -> Result<()> {
    let content = fs::read_to_string(job_file)?;
    let job: DownloadJob = serde_json::from_str(&content)?;

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    rt.block_on(run_download_async(job))
}

/// Whether losing *failed* of *attempted* XYZ tiles must fail the whole run.
///
/// A tile that never arrived is not a hole in the output — it is written as
/// 0 m, sea level, because the assembly grid is zero-filled and the failed tile
/// is simply never copied in. So a download that 404s everything (the classic
/// cause: a max zoom the tile source does not serve) still produces a full set
/// of plausible-looking flat `.abt` files, and `run_download` used to return
/// `Ok(())` for it.
///
/// Sparse 404s at the edge of a provider's coverage are normal and must stay
/// non-fatal, so the line is drawn at a **majority** of the requested tiles:
/// below that the output is still mostly real terrain and the `[Stats] ERRORS`
/// summary is the right response; above it there is nothing worth keeping.
#[cfg(feature = "native")]
fn fetch_failure_is_fatal(failed: usize, attempted: usize) -> bool {
    attempted > 0 && failed * 2 > attempted
}

/// Removes the `.abt` files a run created, unless it reached [`AbtCleanup::keep`].
///
/// Every output file is created with a valid header *before* the first tile is
/// fetched and is sized to full length once assembly ends, while tiles that
/// never arrived stay 0 m — so a run that bails out afterwards leaves a
/// complete, plausible-looking `.abt` of sea-level terrain behind. Callers pool
/// tiles by filename, so that file is a cache hit for every later run: one
/// refusal poisons the pool with flat ocean. A failed download must therefore
/// leave nothing reusable behind.
///
/// It is a drop guard rather than a cleanup call at the fatal branch because
/// that branch is not the only way out: the tile-assembly `h.await??`, the
/// buildings post-pass and every other `?` between file creation and the end of
/// the run exit with the same half-written files on disk.
#[cfg(feature = "native")]
#[derive(Default)]
struct AbtCleanup {
    paths: Vec<PathBuf>,
}

#[cfg(feature = "native")]
impl AbtCleanup {
    /// Register a file as provisional. Called before it is created, so a file
    /// that only half-exists (created, then the header write failed) also goes.
    fn track(&mut self, path: PathBuf) {
        self.paths.push(path);
    }

    /// The run succeeded — the files are real output now, so keep them.
    fn keep(mut self) {
        self.paths.clear();
    }
}

#[cfg(feature = "native")]
impl Drop for AbtCleanup {
    fn drop(&mut self) {
        let removed = self.paths.iter().filter(|p| fs::remove_file(p).is_ok()).count();
        if removed > 0 {
            eprintln!(
                "[Download] Removed {} incomplete .abt file(s) — a failed download \
                 leaves nothing reusable behind; the next run must fetch again.",
                removed
            );
        }
    }
}

#[cfg(feature = "native")]
async fn run_download_async(job: DownloadJob) -> Result<()> {
    let start = Instant::now();
    let conns = job.max_connections.unwrap_or(256);

    let dec: fn(u8, u8, u8) -> f32 = match job.encoding.to_lowercase().as_str() {
        "terrarium" => dec_terrarium,
        "mapbox" => dec_mapbox,
        o => anyhow::bail!("Unknown encoding '{o}'"),
    };

    // 1. Full bbox.
    let (mut bb_s, mut bb_n, mut bb_w, mut bb_e) = (f64::MAX, f64::MIN, f64::MAX, f64::MIN);
    for t in &job.tiles {
        let pd = t.resolution_m / 111_111.0;
        let sp = t.size_px as f64 * pd;
        bb_s = bb_s.min(t.ul_lat - sp);
        bb_n = bb_n.max(t.ul_lat);
        bb_w = bb_w.min(t.ul_lon);
        bb_e = bb_e.max(t.ul_lon + sp);
    }

    // 2. Tile range.
    let (x0, x1) = (lon2tx(bb_w, job.zoom), lon2tx(bb_e, job.zoom));
    let (y0, y1) = (lat2ty(bb_n, job.zoom), lat2ty(bb_s, job.zoom));
    let nx = (x1 - x0 + 1) as usize;
    let ny = (y1 - y0 + 1) as usize;
    let total = nx * ny;
    let gul_lon = tx2lon(x0, job.zoom);
    let gul_lat = ty2lat(y0, job.zoom);
    let glr_lon = tx2lon(x1 + 1, job.zoom);
    let glr_lat = ty2lat(y1 + 1, job.zoom);
    let gw = nx * 256;
    let gpx = (glr_lon - gul_lon) / gw as f64;

    let tile_size_kb = 200;
    let est_mb = total * tile_size_kb / 1024;
    eprintln!("[Download] z={} tiles={}x{}={} (~{}MB) connections={}",
        job.zoom, nx, ny, total, est_mb, conns);

    // 2b. Resource checks — fail early with clear errors.
    let total_abt_bytes = estimate_abt_bytes(&job.tiles);
    let total_abt_mb = total_abt_bytes / (1024 * 1024);
    eprintln!("[Download] Output: {} .abt files, ~{} MB disk needed",
        job.tiles.len(), total_abt_mb);

    if let Some(avail) = available_disk_space(&job.output_dir) {
        let avail_mb = avail / (1024 * 1024);
        if total_abt_bytes > avail {
            anyhow::bail!(
                "Insufficient disk space: need ~{} MB for {} .abt output files, \
                 only {} MB available in {:?}. Free disk space or reduce area/resolution.",
                total_abt_mb, job.tiles.len(), avail_mb, job.output_dir
            );
        }
        if total_abt_bytes > avail * 4 / 5 {
            eprintln!("[Download] WARNING: disk space is tight — need {} MB, {} MB available",
                total_abt_mb, avail_mb);
        }
    }

    // 3. Single HTTP client — reqwest handles connection pooling internally.
    //
    // "connection closed before message completed" (the deterministic stripe on
    // the largest mountain tiles) comes from hyper reusing a pooled keep-alive
    // socket the CDN has already closed. The old 60s idle window made that
    // likely: the biggest tiles are slowest, so their connections sit idle
    // longest between reuses and get reaped server-side, then reused blind.
    // Keep the pool wide for throughput, but drop idle sockets fast so a stale
    // one is never reused, and enable TCP keepalive so a dead socket is detected
    // rather than reused. (The per-tile retry below recovers the rare race.)
    let client = reqwest::Client::builder()
        .pool_max_idle_per_host(conns)
        .pool_idle_timeout(std::time::Duration::from_secs(5))
        .tcp_keepalive(std::time::Duration::from_secs(15))
        .timeout(std::time::Duration::from_secs(30))
        .build()?;

    // 4. Prepare .abt files (write headers, then close — assembly reopens per-strip).
    //    Everything created from here on is provisional: until `cleanup.keep()`
    //    at the end of a successful run, any exit removes these files again
    //    rather than leaving flat 0 m tiles for the next run to pool as a hit.
    fs::create_dir_all(&job.output_dir)?;
    let mut cleanup = AbtCleanup::default();
    let abt_specs: Vec<(SubTileSpec, PathBuf)> = {
        let mut specs = Vec::new();
        for spec in &job.tiles {
            let pd = spec.resolution_m / 111_111.0;
            let path = job.output_dir.join(&spec.filename);
            cleanup.track(path.clone());
            let writer = AbtWriter::create(&path, spec.size_px, spec.ul_lat, spec.ul_lon, pd)?;
            writer.finish()?;
            specs.push((spec.clone(), path));
        }
        specs
    };

    // 4b. Assembly thread pool — small pool; assembly is memory-bound (<1% CPU).
    //     Keep most cores free for Tokio (TLS + PNG decode = 78% of CPU).
    let num_cpus = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    let asm_threads = (num_cpus / 4).max(2);
    let asm_pool = Arc::new(
        rayon::ThreadPoolBuilder::new()
            .num_threads(asm_threads)
            .build()?,
    );

    // 5. Process in strips with deep prefetch pipeline.
    //    Keep prefetch_depth strips downloading ahead while processing.
    let strip_rows: u32 = 32;
    const MAX_PREFETCH: usize = 5;

    // Build strip ranges.
    let mut strips: Vec<(u32, u32)> = Vec::new();
    let mut cur = y0;
    while cur <= y1 {
        let end = (cur + strip_rows - 1).min(y1);
        strips.push((cur, end));
        cur = end + 1;
    }

    // Dynamic prefetch depth: keep total buffered tile data within RAM budget.
    let bytes_per_tile_data: usize = 256 * 256 * 4; // f32 per XYZ tile pixel
    let actual_strip_rows = (strip_rows as usize).min(ny);
    let strip_mem = nx * actual_strip_rows * bytes_per_tile_data;
    let mini_grid_mem = gw * 256 * 4;

    let mut sys = System::new();
    sys.refresh_memory();
    let available_ram = sys.available_memory() as usize;
    let ram_budget = available_ram / 2; // use at most 50% for download buffers
    let prefetch_depth = if strip_mem + mini_grid_mem > 0 {
        (ram_budget / (strip_mem + mini_grid_mem)).clamp(1, MAX_PREFETCH)
    } else {
        MAX_PREFETCH
    };

    const MAX_CONCURRENT_ASM: usize = 1;
    eprintln!("[Download] RAM: {:.1} GB available, ~{:.0} MB/strip, prefetch depth: {}",
        available_ram as f64 / 1e9, strip_mem as f64 / 1e6, prefetch_depth);
    eprintln!("[Download] {} strips, {} tile-rows/strip, assembly {}/{} threads (max {} concurrent)",
        strips.len(), strip_rows, asm_threads, num_cpus, MAX_CONCURRENT_ASM);

    let zoom = job.zoom;
    let url_tpl = Arc::new(job.url_template.clone());
    let progress = Arc::new(AtomicUsize::new(0));
    let stats = Arc::new(DownloadStats::new());
    let semaphore = Arc::new(Semaphore::new(conns));
    let asm_semaphore = Arc::new(Semaphore::new(MAX_CONCURRENT_ASM));

    type StripHandle = tokio::task::JoinHandle<Vec<(u32, u32, Result<Vec<f32>>)>>;

    let spawn_strip = |sy0: u32, sy1: u32, cl: reqwest::Client, ut: Arc<String>,
                       pr: Arc<AtomicUsize>, st: Arc<DownloadStats>,
                       sem: Arc<Semaphore>| -> StripHandle {
        let start_time = start;
        tokio::spawn(async move {
            download_strip(
                &cl, &ut, zoom, x0, x1, sy0, sy1, dec, conns, &pr, total, &st, start_time,
                &sem, None,
            )
            .await
        })
    };

    // Prefill the pipeline.
    let mut pipeline: std::collections::VecDeque<StripHandle> = std::collections::VecDeque::new();
    for i in 0..prefetch_depth.min(strips.len()) {
        let (sy0, sy1) = strips[i];
        pipeline.push_back(spawn_strip(
            sy0, sy1, client.clone(), url_tpl.clone(), progress.clone(), stats.clone(),
            semaphore.clone(),
        ));
    }
    let mut next_to_launch = prefetch_depth.min(strips.len());

    // Assembly tasks run concurrently (gated by asm_semaphore, each opens own file handles).
    // Back-pressure: don't let more than prefetch_depth+1 assemblies queue up,
    // otherwise strip data accumulates in memory faster than it can be written.
    let mut assembly_handles: Vec<tokio::task::JoinHandle<Result<()>>> = Vec::new();
    let max_queued_asm = prefetch_depth + 1;

    for strip_idx in 0..strips.len() {
        let (sy0, sy1) = strips[strip_idx];
        let sny = (sy1 - sy0 + 1) as usize;
        let sh = sny * 256;
        let sul = ty2lat(sy0, zoom);
        let slr = ty2lat(sy1 + 1, zoom);
        let spy = (sul - slr) / sh as f64;

        // Await the front of the pipeline.
        let results = pipeline.pop_front().unwrap().await?;

        // Log strip download results.
        let strip_ok = results.iter().filter(|(_, _, r)| r.is_ok()).count();
        let strip_total = results.len();
        let strip_err = strip_total - strip_ok;
        if strip_err > 0 {
            eprintln!("[Strip {}/{}] {} tiles: {} ok, {} FAILED",
                strip_idx + 1, strips.len(), strip_total, strip_ok, strip_err);
        }

        // Launch the next strip to keep the pipeline full.
        if next_to_launch < strips.len() {
            let (ny0, ny1) = strips[next_to_launch];
            pipeline.push_back(spawn_strip(
                ny0, ny1, client.clone(), url_tpl.clone(), progress.clone(), stats.clone(),
                semaphore.clone(),
            ));
            next_to_launch += 1;
        }

        // Back-pressure: drain completed assemblies to free strip data from memory.
        while assembly_handles.len() >= max_queued_asm {
            let h = assembly_handles.remove(0);
            h.await??;
        }

        // Spawn assembly — process one tile-row at a time (36 MB grid fits in L3).
        assembly_handles.push(tokio::spawn({
            let specs = abt_specs.clone();
            let pool = asm_pool.clone();
            let asm_sem = asm_semaphore.clone();
            let si = strip_idx + 1;
            let ns = strips.len();
            async move {
                let _permit = asm_sem.acquire().await.unwrap();
                tokio::task::spawn_blocking(move || -> Result<()> {
                    let asm_start = Instant::now();
                    let sny = (sy1 - sy0 + 1) as usize;

                    // Setup: group tiles by row, pre-compute x_luts, open writers.
                    let t_setup = Instant::now();
                    let mut tiles_by_row: Vec<Vec<(u32, &[f32])>> = vec![Vec::new(); sny];
                    for (tx, ty, r) in &results {
                        if let Ok(elev) = r {
                            tiles_by_row[(*ty - sy0) as usize]
                                .push((*tx, elev.as_slice()));
                        }
                    }

                    let x_luts: Vec<Vec<(u32, u32)>> = specs.iter().map(|(spec, _)| {
                        let pd = spec.resolution_m / 111_111.0;
                        let sz = spec.size_px as usize;
                        x_span_table(sz, spec.ul_lon, pd, gul_lon, gpx, gw)
                    }).collect();

                    let strides: Vec<usize> = specs.iter().map(|(spec, _)| {
                        let bpr = spec.size_px as usize * 2;
                        (bpr + 255) & !255
                    }).collect();
                    let mut writers: Vec<Option<BufWriter<File>>> = specs.iter()
                        .map(|(_, path)| {
                            fs::OpenOptions::new().write(true).open(path)
                                .ok().map(|f| BufWriter::with_capacity(1 << 20, f))
                        }).collect();
                    let mut sought = vec![false; specs.len()];
                    let setup_ms = t_setup.elapsed().as_millis();

                    // Process one tile-row at a time. Grid = gw×256 ≈ 36 MB (fits in L3).
                    let t_work = Instant::now();
                    let mut mini_grid = vec![0.0f32; gw * 256];
                    let mut total_rows = 0usize;
                    let mut total_px = 0usize;
                    const PAD: [u8; 256] = [0u8; 256];

                    for tr in 0..sny {
                        let ty = sy0 + tr as u32;

                        // Clear mini-grid so failed tiles don't leak stale data
                        // from previous tile-rows.
                        mini_grid.fill(0.0);

                        for &(tx, elev) in &tiles_by_row[tr] {
                            let col = (tx - x0) as usize * 256;
                            for py in 0..256usize {
                                let cw = 256.min(gw - col);
                                mini_grid[py * gw + col..py * gw + col + cw]
                                    .copy_from_slice(&elev[py * 256..py * 256 + cw]);
                            }
                        }

                        let tr_top = ty2lat(ty, zoom);
                        let tr_bot = ty2lat(ty + 1, zoom);
                        let tr_spy = (tr_top - tr_bot) / 256.0;

                        // Sample + write output rows that fall within this tile-row.
                        // Parallel across sub-tiles (each writes to its own file).
                        let grid_ref: &[f32] = &mini_grid;
                        let per_tile: Vec<Vec<(u32, Vec<i16>)>> = pool.install(|| {
                            use rayon::prelude::*;
                            specs.par_iter().enumerate().map(|(sti, (spec, _))| {
                                let pd = spec.resolution_m / 111_111.0;
                                let sz = spec.size_px as usize;
                                let x_lut = &x_luts[sti];

                                // Skip sub-tiles that don't overlap this tile-row.
                                let spec_bot = spec.ul_lat - sz as f64 * pd;
                                if spec.ul_lat <= tr_bot || spec_bot >= tr_top {
                                    return Vec::new();
                                }

                                (0..sz).filter_map(|y| {
                                    let lat = spec.ul_lat - (y as f64 + 0.5) * pd;
                                    if lat > tr_top || lat <= tr_bot { return None; }
                                    let (r0, r1) = tile_row_span(
                                        (tr_top - (spec.ul_lat - y as f64 * pd)) / tr_spy,
                                        (tr_top - (spec.ul_lat - (y + 1) as f64 * pd)) / tr_spy,
                                    );

                                    let mut row_data = vec![0i16; sz];
                                    for x in 0..sz {
                                        let (c0, c1) = x_lut[x];
                                        row_data[x] = avg_cell(
                                            grid_ref, gw, c0 as usize, c1 as usize, r0, r1,
                                        );
                                    }
                                    Some((y as u32, row_data))
                                }).collect()
                            }).collect()
                        });

                        // Write rows (sequential per sub-tile, single seek then stream).
                        for (sti, rows) in per_tile.iter().enumerate() {
                            if rows.is_empty() { continue; }
                            total_rows += rows.len();
                            total_px += rows.len() * specs[sti].0.size_px as usize;
                            if let Some(ref mut w) = writers[sti] {
                                if !sought[sti] {
                                    let first_y = rows[0].0;
                                    w.seek(SeekFrom::Start(
                                        44 + first_y as u64 * strides[sti] as u64,
                                    ))?;
                                    sought[sti] = true;
                                }
                                let p = strides[sti] - specs[sti].0.size_px as usize * 2;
                                for (_, row_data) in rows {
                                    w.write_all(bytemuck::cast_slice(row_data))?;
                                    if p > 0 { w.write_all(&PAD[..p])?; }
                                }
                            }
                        }
                    }

                    // Flush.
                    for w in writers.iter_mut().flatten() { w.flush()?; }
                    drop(tiles_by_row);
                    drop(results);
                    let work_ms = t_work.elapsed().as_millis();
                    let out_mb = total_px as f64 * 2.0 / (1024.0 * 1024.0);
                    eprintln!(
                        "[Strip {}/{}] setup={}ms work={}ms({} rows, {:.1}Mpx, {:.0}MB) total={:.2}s",
                        si, ns, setup_ms, work_ms, total_rows,
                        total_px as f64 / 1e6, out_mb, asm_start.elapsed().as_secs_f64(),
                    );
                    Ok(())
                }).await?
            }
        }));
    }

    // Wait for all assembly tasks.
    for h in assembly_handles {
        h.await??;
    }

    // Set final file sizes — fills any unwritten trailing rows with zeros.
    // (Needed because we skipped set_len pre-allocation.)
    for (spec, path) in &abt_specs {
        let bpr = spec.size_px as u64 * 2;
        let stride = (bpr + 255) & !255;
        let expected = 44 + stride * spec.size_px as u64;
        if let Ok(f) = fs::OpenOptions::new().write(true).open(path) {
            let actual = f.metadata().map(|m| m.len()).unwrap_or(0);
            if actual < expected {
                let _ = f.set_len(expected);
            }
        }
    }

    // A run that fetched almost nothing is not a successful run. A tile that
    // never arrived is never copied into the just-`fill(0.0)`-ed mini-grid, so
    // it is written out as 0 m — flat terrain at sea level, indistinguishable
    // downstream from real data. Report it as the failure it is instead of
    // exiting 0 with an ocean-flat .abt, and name the error class: "could not
    // be fetched" alone reads as a network fault even when the real cause is a
    // source serving WebP to a PNG decoder. `cleanup` takes the files with it.
    let failed = stats.total_errors();
    let attempted = stats.total_tiles();
    if fetch_failure_is_fatal(failed, attempted) {
        stats.log_summary(start.elapsed().as_secs_f64());
        anyhow::bail!(
            "Tile download failed: {} of {} terrain tiles ({}%) could not be fetched; \
             the output would be mostly flat 0 m, not terrain. Errors: {}. Check that \
             the tile source serves zoom {} over this area and that the network is \
             reachable.",
            failed,
            attempted,
            failed * 100 / attempted.max(1),
            stats.error_breakdown(),
            job.zoom
        );
    }

    if let Some(pbf_dir) = &job.buildings_pbf_dir {
        apply_buildings_post_pass(pbf_dir, &abt_specs)?;
    }

    let elapsed = start.elapsed().as_secs_f64();
    stats.log_summary(elapsed);
    eprintln!("[Download] TOTAL: {:.1}s", elapsed);
    cleanup.keep();
    Ok(())
}

/// Fuse vector-tile buildings onto the `.abt` tiles the downloader just wrote.
///
/// The downloader streams each tile to disk row by row, so buildings cannot be
/// applied mid-stream: resolving a footprint's roof needs the terrain under the
/// whole footprint to be present. This runs as a post-pass over the finished
/// files instead.
///
/// Cost is `O(pbf tiles + abt tiles)`, not their product — the PBF set is
/// decoded **once** for the run and then rasterized per tile. Tiles are read,
/// modified and written back one at a time, so peak memory stays at one `.abt`
/// plus the decoded building model.
#[cfg(feature = "native")]
fn apply_buildings_post_pass(pbf_dir: &Path, abt_specs: &[(SubTileSpec, PathBuf)]) -> Result<()> {
    use crate::buildings::{load_pbf_building_dir, rasterize_buildings, AbtGrid, RasterOpts, TileRef};

    let t0 = Instant::now();

    // 1. Decode every PBF tile once. The shared loader also refuses a
    //    mixed-zoom directory, which would apply every above-ground height
    //    twice — see `load_pbf_building_dir`.
    let set = load_pbf_building_dir(pbf_dir)?;

    if set.buildings.is_empty() {
        eprintln!("[Buildings] {} pbf tile(s), no buildings in extent", set.tiles_read);
        return Ok(());
    }

    // 2. Rasterize onto each finished tile, one at a time.
    let opts = RasterOpts::default();
    let (mut tiles_hit, mut px_total) = (0usize, 0u64);
    for (spec, path) in abt_specs {
        let mut buf = match fs::read(path) {
            Ok(b) if b.len() >= 44 => b,
            // A tile the download never produced is a download failure and is
            // already reported as one; do not turn it into a buildings error.
            _ => continue,
        };

        let pd = spec.resolution_m / 111_111.0;
        let stride = (spec.size_px as usize * 2 + 255) & !255;
        let tile = TileRef {
            ul_lat: spec.ul_lat,
            ul_lon: spec.ul_lon,
            scale_x: pd,
            scale_y: pd,
            size_px: spec.size_px,
        };
        let mut grid = AbtGrid { buf: &mut buf, size: spec.size_px, stride };
        let stats = rasterize_buildings(&mut grid, &tile, &set.buildings, &opts);

        if stats.pixels_modified > 0 {
            fs::write(path, &buf)?;
            tiles_hit += 1;
            px_total += stats.pixels_modified as u64;
        }
    }

    eprintln!(
        "[Buildings] {} pbf tile(s) → {} building(s) ({} dup dropped) → {}/{} abt tile(s), {} px in {:.1}s",
        set.tiles_read,
        set.buildings.len(),
        set.duplicates_dropped(),
        tiles_hit,
        abt_specs.len(),
        px_total,
        t0.elapsed().as_secs_f64(),
    );
    Ok(())
}

// -- In-memory download (WASM-compatible, no rayon/sysinfo/File) --------------

/// Same pipeline as `run_download_async` but returns .abt bytes in memory.
/// Does NOT use rayon, sysinfo, or filesystem I/O — suitable for WASM targets.
pub async fn run_download_mem(
    job: &DownloadJob,
    client: &reqwest::Client,
    #[cfg(not(target_arch = "wasm32"))]
    on_progress: Option<&(dyn Fn(usize, usize) + Send + Sync)>,
    #[cfg(target_arch = "wasm32")]
    on_progress: Option<&dyn Fn(usize, usize)>,
    // Raw JS callback for WASM progress: (phase: u32, done: u32, total: u32).
    // Phase 0 = download, 1 = decode.
    #[cfg(target_arch = "wasm32")]
    js_progress: Option<&js_sys::Function>,
) -> Result<std::collections::HashMap<String, Vec<u8>>> {
    let start = Instant::now();
    let conns = job.max_connections.unwrap_or(256);

    let dec: fn(u8, u8, u8) -> f32 = match job.encoding.to_lowercase().as_str() {
        "terrarium" => dec_terrarium,
        "mapbox" => dec_mapbox,
        o => anyhow::bail!("Unknown encoding '{o}'"),
    };

    // 1. Full bbox across all sub-tiles.
    let (mut bb_s, mut bb_n, mut bb_w, mut bb_e) = (f64::MAX, f64::MIN, f64::MAX, f64::MIN);
    for t in &job.tiles {
        let pd = t.resolution_m / 111_111.0;
        let sp = t.size_px as f64 * pd;
        bb_s = bb_s.min(t.ul_lat - sp);
        bb_n = bb_n.max(t.ul_lat);
        bb_w = bb_w.min(t.ul_lon);
        bb_e = bb_e.max(t.ul_lon + sp);
    }

    // 2. Tile range.
    let (x0, x1) = (lon2tx(bb_w, job.zoom), lon2tx(bb_e, job.zoom));
    let (y0, y1) = (lat2ty(bb_n, job.zoom), lat2ty(bb_s, job.zoom));
    let nx = (x1 - x0 + 1) as usize;
    let ny = (y1 - y0 + 1) as usize;
    let total = nx * ny;
    let gul_lon = tx2lon(x0, job.zoom);
    let glr_lon = tx2lon(x1 + 1, job.zoom);
    let gw = nx * 256;
    let gpx = (glr_lon - gul_lon) / gw as f64;

    eprintln!("[DownloadMem] z={} tiles={}x{}={} connections={}",
        job.zoom, nx, ny, total, conns);

    // 3. Prepare in-memory .abt buffers (pre-allocate with header + zeroed body).
    struct MemAbt {
        filename: String,
        buf: Vec<u8>,
        size_px: u32,
        stride: usize,
        ul_lat: f64,
        ul_lon: f64,
        pd: f64,
    }

    let mut abt_bufs: Vec<MemAbt> = Vec::with_capacity(job.tiles.len());
    for spec in &job.tiles {
        let pd = spec.resolution_m / 111_111.0;
        let bpr = spec.size_px as usize * 2;
        let stride = (bpr + 255) & !255;
        let total_bytes = 44 + stride * spec.size_px as usize;
        let mut buf = vec![0u8; total_bytes];

        // Write 44-byte .abt header (identical to AbtWriter::create).
        {
            let mut cursor = std::io::Cursor::new(&mut buf[..44]);
            cursor.write_all(b"AETH")?;
            cursor.write_u16::<LittleEndian>(1)?;             // version
            cursor.write_u16::<LittleEndian>(spec.size_px as u16)?;
            cursor.write_f64::<LittleEndian>(spec.ul_lat)?;
            cursor.write_f64::<LittleEndian>(spec.ul_lon)?;
            cursor.write_f64::<LittleEndian>(pd)?;             // pixel_deg lat
            cursor.write_f64::<LittleEndian>(pd)?;             // pixel_deg lon
            cursor.write_i16::<LittleEndian>(0)?;              // base_elev
            cursor.write_u16::<LittleEndian>(stride as u16)?;
        }

        abt_bufs.push(MemAbt {
            filename: spec.filename.clone(),
            buf,
            size_px: spec.size_px,
            stride,
            ul_lat: spec.ul_lat,
            ul_lon: spec.ul_lon,
            pd,
        });
    }

    // 4. Pre-compute x-lookup tables (pixel x → source column span).
    let x_luts: Vec<Vec<(u32, u32)>> = abt_bufs.iter().map(|abt| {
        x_span_table(abt.size_px as usize, abt.ul_lon, abt.pd, gul_lon, gpx, gw)
    }).collect();

    // 5. Build strip ranges.
    let strip_rows: u32 = 32;
    let mut strips: Vec<(u32, u32)> = Vec::new();
    {
        let mut cur = y0;
        while cur <= y1 {
            let end = (cur + strip_rows - 1).min(y1);
            strips.push((cur, end));
            cur = end + 1;
        }
    }

    let zoom = job.zoom;
    let stats = Arc::new(DownloadStats::new());

    // Report 0% immediately
    if let Some(cb) = &on_progress { cb(0, total); }

    // ── WASM: batch-download ALL tiles via browser fetch ────────
    // Bypasses reqwest; hands every URL to the browser in one JS call.
    #[cfg(target_arch = "wasm32")]
    let mut tile_cache = download_all_tiles_web(
        &job.url_template, zoom, x0, x1, y0, y1,
        conns.min(200),
        &stats, start, js_progress,
    ).await;
    #[cfg(target_arch = "wasm32")]
    let _ = client; // browser fetch bypasses reqwest

    #[cfg(not(target_arch = "wasm32"))]
    let progress = Arc::new(AtomicUsize::new(0));
    #[cfg(not(target_arch = "wasm32"))]
    let semaphore = Arc::new(Semaphore::new(conns));

    // 6. Process strips — decode + assemble into .abt buffers.
    //
    // WASM: tiles already downloaded via download_all_tiles_web (browser fetch).
    //       Strip loop only decodes PNGs and assembles.
    // Native: downloads per-strip via reqwest (download_strip_raw), then decodes.
    let mut mini_grid = vec![0.0f32; gw * 256];

    #[cfg(target_arch = "wasm32")]
    let mut decoded_count: usize = 0;
    #[cfg(target_arch = "wasm32")]
    let mut last_decode_pct5: usize = 0;

    for (strip_idx, &(sy0, sy1)) in strips.iter().enumerate() {
        let sny = (sy1 - sy0 + 1) as usize;

        // ── Get raw tile data ───────────────────────────────────
        #[cfg(target_arch = "wasm32")]
        let raw_results: Vec<(u32, u32, Result<Vec<u8>>)> = (sy0..=sy1)
            .flat_map(|ty| (x0..=x1).map(move |tx| (tx, ty)))
            .map(|(tx, ty)| match tile_cache.remove(&(tx, ty)) {
                Some(bytes) => (tx, ty, Ok(bytes)),
                None => (tx, ty, Err(anyhow::anyhow!("download failed"))),
            })
            .collect();

        #[cfg(not(target_arch = "wasm32"))]
        let raw_results = download_strip_raw(
            client, &job.url_template, zoom, x0, x1, sy0, sy1,
            conns, &progress, total, &stats, start, &semaphore,
            on_progress,
        ).await;

        // Phase 2: Decode PNGs sequentially, then classify decode errors.

        let mut results: Vec<(u32, u32, Result<Vec<f32>>)> =
            Vec::with_capacity(raw_results.len());
        for (tx, ty, raw) in raw_results {
            match raw {
                Ok(png_bytes) => {
                    match decode_png(&png_bytes, dec) {
                        Ok(elev) => {
                            results.push((tx, ty, Ok(elev)));

                            // Report decode progress every 5 % (phase 1).
                            #[cfg(target_arch = "wasm32")]
                            {
                                decoded_count += 1;
                                if let Some(f) = js_progress {
                                    let pct5 = if total > 0 { decoded_count * 20 / total } else { 0 };
                                    if pct5 > last_decode_pct5 || decoded_count == total || decoded_count == 1 {
                                        last_decode_pct5 = pct5;
                                        let _ = f.call3(
                                            &wasm_bindgen::JsValue::NULL,
                                            &wasm_bindgen::JsValue::from(1u32),
                                            &wasm_bindgen::JsValue::from(decoded_count as u32),
                                            &wasm_bindgen::JsValue::from(total as u32),
                                        );
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            #[cfg(target_arch = "wasm32")]
                            { decoded_count += 1; }

                            let prev = stats.err_decode.fetch_add(1, Ordering::Relaxed);
                            // Undo ok_count credited during download
                            // (HTTP fetch succeeded, but PNG decode failed).
                            stats.ok_count.fetch_sub(1, Ordering::Relaxed);
                            if prev == 0 {
                                eprintln!(
                                    "[Download] first decode error: z={}/x={}/y={} — {:#}",
                                    zoom, tx, ty, e
                                );
                            }
                            results.push((tx, ty, Err(e)));
                        }
                    }
                }
                Err(e) => {
                    // Network error — already classified by download_strip_raw.
                    results.push((tx, ty, Err(e)));
                }
            }
        }

        let strip_ok = results.iter().filter(|(_, _, r)| r.is_ok()).count();
        let strip_total = results.len();
        let strip_err = strip_total - strip_ok;
        if strip_err > 0 {
            eprintln!("[StripMem {}/{}] {} tiles: {} ok, {} FAILED",
                strip_idx + 1, strips.len(), strip_total, strip_ok, strip_err);
        }

        // Group tiles by row within the strip.
        let mut tiles_by_row: Vec<Vec<(u32, &[f32])>> = vec![Vec::new(); sny];
        for (tx, ty, r) in &results {
            if let Ok(elev) = r {
                tiles_by_row[(*ty - sy0) as usize].push((*tx, elev.as_slice()));
            }
        }

        // Process one tile-row at a time.
        for tr in 0..sny {
            let ty = sy0 + tr as u32;

            // Clear mini-grid so failed tiles don't leak stale data
            // from previous tile-rows.
            mini_grid.fill(0.0);

            // Fill mini-grid from downloaded tile data.
            for &(tx, elev) in &tiles_by_row[tr] {
                let col = (tx - x0) as usize * 256;
                for py in 0..256usize {
                    let cw = 256.min(gw - col);
                    mini_grid[py * gw + col..py * gw + col + cw]
                        .copy_from_slice(&elev[py * 256..py * 256 + cw]);
                }
            }

            let tr_top = ty2lat(ty, zoom);
            let tr_bot = ty2lat(ty + 1, zoom);
            let tr_spy = (tr_top - tr_bot) / 256.0;

            // Sample output rows from the mini-grid into each abt buffer.
            for (sti, abt) in abt_bufs.iter_mut().enumerate() {
                let sz = abt.size_px as usize;
                let x_lut = &x_luts[sti];

                // Skip sub-tiles that don't overlap this tile-row.
                let spec_bot = abt.ul_lat - sz as f64 * abt.pd;
                if abt.ul_lat <= tr_bot || spec_bot >= tr_top {
                    continue;
                }

                for y in 0..sz {
                    let lat = abt.ul_lat - (y as f64 + 0.5) * abt.pd;
                    if lat > tr_top || lat <= tr_bot { continue; }
                    let (r0, r1) = tile_row_span(
                        (tr_top - (abt.ul_lat - y as f64 * abt.pd)) / tr_spy,
                        (tr_top - (abt.ul_lat - (y + 1) as f64 * abt.pd)) / tr_spy,
                    );

                    let buf_offset = 44 + y * abt.stride;

                    // Write i16 elevation values directly into the buffer.
                    for x in 0..sz {
                        let (c0, c1) = x_lut[x];
                        let val = avg_cell(
                            &mini_grid, gw, c0 as usize, c1 as usize, r0, r1,
                        );
                        let byte_off = buf_offset + x * 2;
                        abt.buf[byte_off..byte_off + 2]
                            .copy_from_slice(&val.to_le_bytes());
                    }
                    // Stride padding is already zeroed from vec![0u8; ...].
                }
            }
        }

        // Report assembly progress per strip (phase 2).
        #[cfg(target_arch = "wasm32")]
        if let Some(f) = js_progress {
            let _ = f.call3(
                &wasm_bindgen::JsValue::NULL,
                &wasm_bindgen::JsValue::from(2u32),
                &wasm_bindgen::JsValue::from((strip_idx + 1) as u32),
                &wasm_bindgen::JsValue::from(strips.len() as u32),
            );
        }
    }

    // 7. Build result map.
    let mut result = std::collections::HashMap::with_capacity(abt_bufs.len());
    for abt in abt_bufs {
        result.insert(abt.filename, abt.buf);
    }

    let elapsed = start.elapsed().as_secs_f64();
    stats.log_summary(elapsed);
    eprintln!("[DownloadMem] TOTAL: {:.1}s", elapsed);
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Tile math ──────────────────────────────────────────────────────────

    #[test]
    fn tile_x_and_longitude_are_inverses() {
        assert_eq!(tx2lon(0, 0), -180.0);
        assert_eq!(tx2lon(1, 1), 0.0);
        assert_eq!(lon2tx(-180.0, 0), 0);
        assert_eq!(lon2tx(-180.0, 1), 0);
        assert_eq!(lon2tx(0.0, 1), 1);
        assert_eq!(lon2tx(179.999, 1), 1);

        for z in 0..=14u32 {
            for x in [0u32, 1, 3, (1u32 << z) - 1] {
                // A point just inside the tile must land back on it.
                let lon = tx2lon(x, z) + (tx2lon(x + 1, z) - tx2lon(x, z)) * 0.5;
                assert_eq!(lon2tx(lon, z), x, "z={z} x={x}");
            }
        }
    }

    #[test]
    fn tile_y_and_latitude_are_inverses() {
        // The Web-Mercator cutoff, and the equator at the middle row.
        assert!((ty2lat(0, 0) - 85.051_128_779_806_6).abs() < 1e-9);
        assert!(ty2lat(1, 1).abs() < 1e-12);

        for z in 0..=14u32 {
            for y in [0u32, 1, 3, (1u32 << z) - 1] {
                let lat = (ty2lat(y, z) + ty2lat(y + 1, z)) * 0.5;
                assert_eq!(lat2ty(lat, z), y, "z={z} y={y}");
            }
        }
    }

    #[test]
    fn latitude_decreases_as_tile_y_increases() {
        let z = 5u32;
        let mut prev = f64::MAX;
        for y in 0..(1u32 << z) {
            let lat = ty2lat(y, z);
            assert!(lat < prev, "y={y} is not south of y={}", y.saturating_sub(1));
            prev = lat;
        }
    }

    #[test]
    fn out_of_range_coordinates_clamp_to_zero_rather_than_wrapping() {
        // `.max(0.0) as u32` — a negative index would otherwise wrap to ~4e9.
        assert_eq!(lon2tx(-181.0, 4), 0);
        assert_eq!(lat2ty(89.0, 4), 0);
    }

    // ── Terrain PNG decoding ───────────────────────────────────────────────

    #[test]
    fn the_terrarium_and_mapbox_decoders_match_their_specs() {
        // Terrarium: (r*256 + g + b/256) - 32768.
        assert_eq!(dec_terrarium(128, 0, 0), 0.0);
        assert_eq!(dec_terrarium(0, 0, 0), -32768.0);
        assert_eq!(dec_terrarium(128, 100, 128), 100.5);
        // Mapbox: -10000 + (r*65536 + g*256 + b) * 0.1.
        assert!((dec_mapbox(0, 0, 0) - -10000.0).abs() < 1e-3);
        assert!((dec_mapbox(1, 134, 160) - 0.0).abs() < 1e-2, "100000 * 0.1 - 10000 = 0 m");
        assert!((dec_mapbox(1, 173, 176) - 1000.0).abs() < 1e-2);
    }

    #[test]
    fn a_blank_terrarium_tile_decodes_below_the_nodata_floor() {
        // The void backfill keys off this: RGB 0,0,0 is -32768 m, not ground.
        assert!(dec_terrarium(0, 0, 0) <= NODATA_M);
        assert!(dec_terrarium(128, 0, 0) > NODATA_M);
    }

    // ── Assembly resampling ────────────────────────────────────────────────

    #[test]
    fn a_cell_owns_the_samples_whose_centres_fall_inside_it() {
        // Two source samples per output cell.
        assert_eq!(grid_span(0.0, 2.0, 8), (0, 2));
        assert_eq!(grid_span(2.0, 4.0, 8), (2, 4));

        // The ratio the downloader actually produces (a zoom slightly finer
        // than the target) is not an integer. The spans must still tile the
        // source: every sample counted once, none skipped — which is exactly
        // what the nearest-neighbour lookup did not do.
        let mut next = 0;
        for x in 0..7usize {
            let (a, b) = grid_span(x as f64 * 1.14, (x + 1) as f64 * 1.14, 8);
            assert_eq!(a, next, "gap or overlap at cell {x}");
            assert!(b > a, "cell {x} came out empty");
            next = b;
        }
    }

    #[test]
    fn an_output_cell_finer_than_the_source_takes_the_sample_it_sits_in() {
        // Ratio 0.5 — nothing to average, so both cells inside sample 0 take
        // sample 0 rather than dividing by zero.
        assert_eq!(grid_span(0.0, 0.5, 4), (0, 1));
        assert_eq!(grid_span(0.5, 1.0, 4), (0, 1));
        assert_eq!(grid_span(1.0, 1.5, 4), (1, 2));
        // Off the grid entirely: empty, which `avg_cell` writes as 0 m — what a
        // tile that failed to download already produces.
        assert_eq!(grid_span(-4.0, -3.5, 4), (0, 0));
        assert_eq!(grid_span(9.0, 9.5, 4), (0, 0));
    }

    #[test]
    fn an_output_row_is_clipped_to_the_tile_row_held_in_memory() {
        // Assembly only ever has one 256-row tile-row of the grid, so a
        // footprint reaching past it is clipped — but never to nothing.
        for (lo, hi) in [(0.0, 1.2), (127.4, 128.6), (254.5, 256.5), (255.6, 256.4)] {
            let (a, b) = tile_row_span(lo, hi);
            assert!(a < b && b <= 256, "{lo}..{hi} gave {a}..{b}");
        }
    }

    #[test]
    fn the_column_table_covers_the_grid_without_gaps() {
        // 8 grid samples under 4 output columns of the same total width.
        let spans = x_span_table(4, 0.0, 2.0, 0.0, 1.0, 8);
        assert_eq!(spans, vec![(0, 2), (2, 4), (4, 6), (6, 8)]);
    }

    #[test]
    fn avg_cell_averages_its_footprint_and_leaves_one_sample_alone() {
        let grid = vec![
            100.0f32, 200.0, 0.0, 0.0, //
            300.0, 400.0, 0.0, 0.0, //
            0.0, 0.0, 0.0, 0.0, //
            0.0, 0.0, 0.0, 0.0,
        ];
        // 2x2: mean 250 m = 500 half-metres.
        assert_eq!(avg_cell(&grid, 4, 0, 2, 0, 2), 500);
        // One sample: the value the point sample wrote, unchanged.
        assert_eq!(avg_cell(&grid, 4, 1, 2, 0, 1), 400);
        // A footprint off the grid stays 0 m.
        assert_eq!(avg_cell(&grid, 4, 0, 0, 0, 1), 0);
    }

    #[test]
    fn a_terrarium_void_is_not_averaged_into_the_terrain_beside_it() {
        // A blank Terrarium pixel the parent backfill could not repair is
        // -32768 m. Averaging it in would put this 1000 m cell at 235 m.
        let grid = vec![1000.0f32, -32768.0, 1000.0, 1000.0];
        assert_eq!(avg_cell(&grid, 2, 0, 2, 0, 2), 2000);

        // Nothing but voids: the pit passes through rather than becoming
        // invented ground, exactly as the point sample passed it through.
        let all_void = vec![-32768.0f32; 4];
        assert_eq!(avg_cell(&all_void, 2, 0, 2, 0, 2), half_metres(-32768.0));
    }

    // ── .abt writer ────────────────────────────────────────────────────────

    #[test]
    fn the_abt_writer_emits_the_44_byte_header_from_the_contract() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.abt");
        let (size, ul_lat, ul_lon, pd) = (300u32, 47.25, 8.5, 9.0e-5);

        AbtWriter::create(&path, size, ul_lat, ul_lon, pd).unwrap().finish().unwrap();

        let b = fs::read(&path).unwrap();
        assert_eq!(b.len(), 44, "the header is written up front, rows follow");
        assert_eq!(&b[0..4], b"AETH");
        assert_eq!(u16::from_le_bytes([b[4], b[5]]), 1);
        assert_eq!(u16::from_le_bytes([b[6], b[7]]), size as u16);
        assert_eq!(f64::from_le_bytes(b[8..16].try_into().unwrap()), ul_lat);
        assert_eq!(f64::from_le_bytes(b[16..24].try_into().unwrap()), ul_lon);
        assert_eq!(f64::from_le_bytes(b[24..32].try_into().unwrap()), pd);
        assert_eq!(f64::from_le_bytes(b[32..40].try_into().unwrap()), pd);
        assert_eq!(i16::from_le_bytes([b[40], b[41]]), 0);
        // 300*2 = 600 bytes/row, aligned up to 768.
        assert_eq!(u16::from_le_bytes([b[42], b[43]]), 768);
    }

    #[test]
    fn written_rows_land_where_the_reader_looks_for_them() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.abt");
        let size = 6u32;
        let mut w = AbtWriter::create(&path, size, 47.0, 8.0, 1e-4).unwrap();
        for y in 0..size {
            let row: Vec<i16> = (0..size).map(|x| (y * 100 + x) as i16).collect();
            w.write_row(y, &row).unwrap();
        }
        w.finish().unwrap();

        // Read it back through the same addressing the engine and the building
        // post-pass use: 44 + row * row_stride + col * 2.
        let mut buf = fs::read(&path).unwrap();
        let stride = u16::from_le_bytes([buf[42], buf[43]]) as usize;
        assert_eq!(stride, 256, "6*2 = 12 bytes, aligned up to 256");
        assert_eq!(buf.len(), 44 + stride * size as usize);
        let mut grid = crate::buildings::AbtGrid { buf: &mut buf, size, stride };
        use crate::buildings::ElevGrid;
        for y in 0..size {
            for x in 0..size {
                assert_eq!(grid.get(x, y), Some((y * 100 + x) as i16), "({x},{y})");
            }
        }
    }

    #[test]
    fn the_abt_size_estimate_matches_what_the_writer_produces() {
        let dir = tempfile::tempdir().unwrap();
        let spec = SubTileSpec {
            filename: "t.abt".into(),
            ul_lat: 47.0,
            ul_lon: 8.0,
            size_px: 100,
            resolution_m: 10.0,
        };
        let path = dir.path().join(&spec.filename);
        let mut w = AbtWriter::create(&path, spec.size_px, spec.ul_lat, spec.ul_lon, 1e-4).unwrap();
        let row = vec![0i16; spec.size_px as usize];
        for y in 0..spec.size_px {
            w.write_row(y, &row).unwrap();
        }
        w.finish().unwrap();

        assert_eq!(
            estimate_abt_bytes(std::slice::from_ref(&spec)),
            fs::metadata(&path).unwrap().len()
        );
    }

    // ── Buildings post-pass ────────────────────────────────────────────────

    #[test]
    fn the_buildings_post_pass_raises_roofs_on_finished_abt_tiles() {
        use crate::buildings::tests::synthetic_building_tile;

        let (z, x, y) = (14u32, 8531u32, 5752u32);
        let dir = tempfile::tempdir().unwrap();
        let pbf_dir = dir.path().join("pbf");
        fs::create_dir(&pbf_dir).unwrap();
        fs::write(pbf_dir.join(format!("{z}_{x}_{y}.pbf")), synthetic_building_tile()).unwrap();

        // One .abt covering exactly that vector tile, on flat 200 m terrain.
        let size = 128u32;
        let (ul_lon, ul_lat) = (tx2lon(x, z), ty2lat(y, z));
        let pd = (tx2lon(x + 1, z) - ul_lon) / size as f64;
        let spec = SubTileSpec {
            filename: "t.abt".into(),
            ul_lat,
            ul_lon,
            size_px: size,
            resolution_m: pd * 111_111.0,
        };
        let path = dir.path().join(&spec.filename);
        let mut w = AbtWriter::create(&path, size, ul_lat, ul_lon, pd).unwrap();
        for row in 0..size {
            w.write_row(row, &vec![400i16; size as usize]).unwrap();
        }
        w.finish().unwrap();

        apply_buildings_post_pass(&pbf_dir, &[(spec, path.clone())]).unwrap();

        // 200 m ground + a 12 m render_height = 212 m = 424 half-metres.
        let buf = fs::read(&path).unwrap();
        let stride = u16::from_le_bytes([buf[42], buf[43]]) as usize;
        let mut raised = 0usize;
        for row in 0..size as usize {
            for col in 0..size as usize {
                let o = 44 + row * stride + col * 2;
                match i16::from_le_bytes([buf[o], buf[o + 1]]) {
                    400 => {}
                    424 => raised += 1,
                    v => panic!("unexpected elevation {v} at ({col},{row})"),
                }
            }
        }
        assert!(raised > 0, "the building must have been drawn");
    }

    #[test]
    fn the_buildings_post_pass_refuses_a_mixed_zoom_directory() {
        use crate::buildings::tests::synthetic_building_tile;

        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("14_8531_5752.pbf"), synthetic_building_tile()).unwrap();
        fs::write(dir.path().join("15_17062_11504.pbf"), synthetic_building_tile()).unwrap();

        let err = apply_buildings_post_pass(dir.path(), &[]).unwrap_err().to_string();
        assert!(err.contains("buildings_pbf_dir mixes zoom levels"), "got {err:?}");
    }

    // ── Download failure threshold ─────────────────────────────────────────

    #[test]
    fn a_near_total_fetch_failure_fails_the_run() {
        // The case this exists for: a max zoom the source does not serve, so
        // every tile 404s and every .abt is written as flat 0 m.
        assert!(fetch_failure_is_fatal(1369, 1369));
        assert!(fetch_failure_is_fatal(1000, 1369));
        assert!(fetch_failure_is_fatal(1, 1), "a one-tile job that fetched nothing");
    }

    #[test]
    fn the_error_breakdown_names_only_the_classes_that_occurred() {
        let s = DownloadStats::new();
        assert_eq!(s.error_breakdown(), "none");

        // The MapTiler Terrain-RGB v2 case: WebP into a PNG decoder. Reported
        // as a decode fault, never as a network one.
        s.err_decode.fetch_add(3, Ordering::Relaxed);
        assert_eq!(s.error_breakdown(), "decode=3");

        s.err_connect.fetch_add(1, Ordering::Relaxed);
        s.err_http_4xx.fetch_add(2, Ordering::Relaxed);
        assert_eq!(s.error_breakdown(), "connect=1, HTTP_4xx=2, decode=3");
    }

    // ── Failed-run cleanup ─────────────────────────────────────────────────

    #[test]
    fn the_cleanup_guard_removes_what_a_failed_run_created() {
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = (dir.path().join("a.abt"), dir.path().join("b.abt"));
        for p in [&a, &b] { fs::write(p, b"AETH").unwrap(); }

        {
            let mut g = AbtCleanup::default();
            g.track(a.clone());
            g.track(b.clone());
        } // dropped without keep(): every exit path, not just the fatal branch

        assert!(!a.exists() && !b.exists(), "a refused run must cache nothing reusable");
    }

    #[test]
    fn the_cleanup_guard_leaves_a_successful_run_alone() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.abt");
        fs::write(&a, b"AETH").unwrap();

        let mut g = AbtCleanup::default();
        g.track(a.clone());
        g.keep();

        assert!(a.exists(), "a run that reached keep() owns its output");
    }

    #[test]
    fn sparse_edge_failures_do_not_fail_the_run() {
        // 404s at the edge of a provider's coverage are normal.
        assert!(!fetch_failure_is_fatal(0, 1369));
        assert!(!fetch_failure_is_fatal(1, 1369));
        assert!(!fetch_failure_is_fatal(137, 1369), "10% lost is still terrain");
        assert!(!fetch_failure_is_fatal(684, 1369), "exactly half is not a majority");
        assert!(fetch_failure_is_fatal(685, 1369), "one past half is");
        // Nothing attempted is not a failure — an empty job is caught elsewhere.
        assert!(!fetch_failure_is_fatal(0, 0));
    }
}

