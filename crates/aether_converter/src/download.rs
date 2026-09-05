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
    /// Optional fetch region of interest (degrees). When present, only the
    /// source tiles intersecting this box are downloaded and decoded; every
    /// pixel outside keeps the pre-filled `VOID_ELEV` sentinel. The output
    /// tile grid, geometry and `.abt` layout are unchanged — this trims the
    /// fetch set, nothing else. Callers pass the area they actually need
    /// (the web passes its simulation bbox inflated by 2 %, covering the
    /// engine's own 1 % bounds margin), because the grid-derived rectangle
    /// can span far more ground than the request (a 2048 px output tile at
    /// 90 m covers 184 km).
    ///
    /// Additive and absent from older jobs (CONTRACT §9b). Supported by the
    /// in-memory downloader only; the file-based `run_download` rejects it,
    /// because its writer produces sparse files whose unwritten rows would
    /// read as 0 m sea level instead of void.
    #[serde(default)]
    pub fetch_bounds: Option<FetchBounds>,
}

/// Region of interest for [`DownloadJob::fetch_bounds`], in degrees.
// dead_code: the CLI binary never reaches the in-memory path, so its
// reachability pass flags these pub fields; the lib (WASM) reads them all.
#[allow(dead_code)]
#[derive(Deserialize, Debug, Clone, Copy)]
pub struct FetchBounds {
    pub south: f64,
    pub north: f64,
    pub west: f64,
    pub east: f64,
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

/// Marks a decode failure that condemns the whole run rather than one tile.
///
/// Tile geometry is a property of the *service*: a source that answers with a
/// non-square or a 16-bit tile answers every request that way, so counting it
/// as one lost tile and carrying on assembles nothing but folded or garbage
/// terrain. Both the retry classifier and the assembly loop key off this
/// prefix — the first tile carrying it stops the run naming what arrived.
pub const TILE_GEOMETRY_ERR: &str = "XYZ tile geometry";

/// The tile edge assumed before any tile has been decoded — the XYZ default.
const DEFAULT_TILE_PX: usize = 256;

/// The largest tile edge a service is assumed to serve, for sizing estimates
/// made before the first tile arrives. MapTiler's `@2x` terrain-RGB is 512.
const MAX_TILE_PX: usize = 512;

/// Decode one terrain PNG into metres, with the square tile edge it came at.
///
/// The edge is returned rather than assumed: MapTiler's terrain-RGB v1 serves
/// **512 px** (`"scale": "2.000000"`) tiles where AWS Terrarium serves 256, and
/// indexing a 512-wide tile with a 256-wide row stride folds it in half — the
/// west half onto the even output rows, the east half onto the odd ones. That
/// produced terrain off by ~555 m at p95 while looking entirely plausible.
///
/// Anything this loop cannot read as 8-bit RGB(A) is refused instead of being
/// decoded into confident nonsense: a non-square tile has no assembly stride,
/// and a 16-bit tile puts two bytes where the decoder reads one channel.
pub fn decode_png(body: &[u8], dec: fn(u8, u8, u8) -> f32) -> Result<(Vec<f32>, usize)> {
    let decoder = png::Decoder::new(std::io::Cursor::new(body));
    let mut reader = decoder.read_info()?;
    let info = reader.info().clone();
    let mut buf = vec![0u8; reader.output_buffer_size()];
    let frame = reader.next_frame(&mut buf)?;
    let bytes = &buf[..frame.buffer_size()];
    let (w, h) = (info.width as usize, info.height as usize);
    if w != h || w == 0 {
        anyhow::bail!("{TILE_GEOMETRY_ERR}: tile is {w}x{h} px; XYZ tiles must be square");
    }
    if info.bit_depth != png::BitDepth::Eight {
        anyhow::bail!(
            "{TILE_GEOMETRY_ERR}: tile is {}-bit; XYZ terrain tiles must be 8 bits per channel",
            info.bit_depth as u8
        );
    }
    let ch = if info.color_type == png::ColorType::Rgba { 4 } else { 3 };
    let mut out = vec![0.0f32; w * h];
    for i in 0..w * h {
        let o = i * ch;
        if o + 2 < bytes.len() {
            out[i] = dec(bytes[o], bytes[o + 1], bytes[o + 2]);
        }
    }
    Ok((out, w))
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

/// The assembly-grid value for "no tile ever covered this sample".
///
/// Distinct from a source void (a blank Terrarium pixel, ~-32768 m) only in
/// magnitude, and far below [`NODATA_M`], so the void-exclusion in [`avg_cell`]
/// already keeps it out of any mean it shares a footprint with. It exists so
/// that a tile which never arrived reads as *absent* rather than as 0 m: 0 is a
/// perfectly valid sea-level elevation, and writing it marks fabricated ocean
/// as real ground that no consumer can tell from surveyed terrain.
const NO_TILE_M: f32 = -1.0e30;

/// The `.abt` no-data sentinel, shared with the ingest path — one convention
/// for "no terrain here" across both writers (§6 of the contract).
use crate::ingest::VOID_ELEV;

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

/// The source rows of an `n`-row tile-row that one output row covers.
///
/// `n` is the decoded tile edge, not a constant: a 512 px `@2x` tile carries
/// twice the rows a 256 px one does over the same ground.
///
/// Callers only reach this for an output row whose centre lies inside the
/// tile-row, so an empty span is a floating-point edge case at the boundary;
/// it resolves to the clamped nearest row, as the old `.round().min(255)` did.
/// A footprint that reaches past the tile-row is clipped to it — the row is
/// averaged over the part of its footprint this tile-row holds, because the
/// assembly only ever has one tile-row of the grid in memory.
#[inline]
fn tile_row_span(lo: f64, hi: f64, n: usize) -> (usize, usize) {
    let (a, b) = grid_span(lo, hi, n);
    if a < b {
        (a, b)
    } else {
        let c = (((lo + hi) * 0.5) as usize).min(n.saturating_sub(1));
        (c, c + 1)
    }
}

/// Per-output-column source spans into the assembly grid, built once per tile.
///
/// Column `x` covers `[ul_lon + x*pd, ul_lon + (x+1)*pd)`; `gul_lon`/`gpx` place
/// and scale the grid. A column whose footprint misses the grid entirely gets an
/// empty span, which [`avg_cell`] writes as the `-9999` void sentinel — the same
/// thing a tile that never arrived writes.
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
/// pixel the parent-tile backfill could not repair is ~-32768 m, a sample no
/// tile ever covered is [`NO_TILE_M`], and letting either into a mean would drag
/// the whole cell into a pit. A cell with nothing but voids under it, and a cell
/// whose footprint misses the grid entirely, are written as the `-9999`
/// [`VOID_ELEV`] sentinel — **not** as 0 m, which is a valid sea-level elevation
/// and would present a tile that never downloaded as real ocean.
///
/// One-sample cells (the target finer than the source) skip the accumulator
/// entirely, so they cost what the nearest-neighbour lookup cost.
#[inline(always)]
fn avg_cell(grid: &[f32], gw: usize, c0: usize, c1: usize, r0: usize, r1: usize) -> i16 {
    if c0 >= c1 {
        return VOID_ELEV;
    }
    if c1 - c0 == 1 && r1 - r0 == 1 {
        let v = grid[r0 * gw + c0];
        return if v <= NODATA_M { VOID_ELEV } else { half_metres(v) };
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
        return VOID_ELEV;
    }
    half_metres(vsum * recip(cnt as usize))
}

/// One `.abt` row of void, padded to `stride` — what a row no tile covered
/// looks like on disk.
///
/// The `size_px * 2` payload bytes are the `-9999` sentinel; the stride padding
/// stays zero, because it is alignment slack and not pixel data (§6 of the
/// contract puts the row payload at `44 + row*stride`, `size_px` samples wide).
fn void_row_bytes(size_px: usize, stride: usize) -> Vec<u8> {
    let mut row = vec![0u8; stride];
    let void = VOID_ELEV.to_le_bytes();
    for x in 0..size_px {
        row[x * 2..x * 2 + 2].copy_from_slice(&void);
    }
    row
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

/// One fetch + decode of a single tile (no retry) -> (grid, tile edge), or None.
async fn fetch_decode_raw(
    client: &reqwest::Client, url: &str, dec: fn(u8, u8, u8) -> f32,
) -> Option<(Vec<f32>, usize)> {
    let resp = client.get(url).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let body = resp.bytes().await.ok()?;
    decode_png(&body, dec).ok()
}

/// Fill the NODATA pixels of a zoom-`z` tile from progressively coarser parents.
///
/// `have` is the decoded tile, or `None` when the tile itself never arrived. A
/// tile that 404'd is a tile that is 100 % void, and the ancestor covering it is
/// exactly as good a source for all of its pixels as it is for a few of them.
/// With `None` the first ancestor that decodes fixes the tile edge, so the grid
/// synthesised here has the stride the rest of the run assembles at. `None` comes
/// back only when nothing was available at any zoom — a genuine hole, which the
/// caller must keep writing as void rather than as 0 m.
async fn fill_from_parents(
    client: &reqwest::Client, url_template: &str, z: u32, x: u32, y: u32,
    dec: fn(u8, u8, u8) -> f32, have: Option<(Vec<f32>, usize)>,
) -> Option<(Vec<f32>, usize)> {
    let (mut grid, mut ts) = have.map_or_else(|| (Vec::new(), 0), |(g, t)| (g, t));
    let mut missing: Vec<usize> =
        (0..grid.len()).filter(|&i| grid[i] <= NODATA_M).collect();
    let mut level = 1u32;
    while (ts == 0 || !missing.is_empty()) && z >= level + NODATA_FILL_MIN_ZOOM {
        let (az, ax, ay) = (z - level, x >> level, y >> level);
        let url = url_template
            .replace("{z}", &az.to_string())
            .replace("{x}", &ax.to_string())
            .replace("{y}", &ay.to_string());
        if let Some((anc, aedge)) = fetch_decode_raw(client, &url, dec).await {
            if ts == 0 {
                ts = aedge;
                grid = vec![NO_TILE_M; ts * ts];
                missing = (0..grid.len()).collect();
            }
            // The parent has to be the same tile size as the child — one
            // service, one grid. A parent of a different size is not a parent
            // this pixel mapping can address, so skip it rather than fold it.
            if aedge == ts && anc.len() == ts * ts {
                let t = ts as u64;
                let (base_x, base_y) = (ax as u64 * t, ay as u64 * t);
                missing.retain(|&i| {
                    let (px, py) = ((i % ts) as u64, (i / ts) as u64);
                    let apx = (((x as u64) * t + px) >> level) - base_x;
                    let apy = (((y as u64) * t + py) >> level) - base_y;
                    let v = anc[(apy * t + apx) as usize];
                    if v > NODATA_M { grid[i] = v; false } else { true }
                });
            }
        }
        level += 1;
    }
    if ts == 0 { None } else { Some((grid, ts)) }
}

/// The XYZ tile edge this run assembles for, taken from the tiles themselves.
///
/// Tile size is a property of the service, not of the job: MapTiler's
/// terrain-rgb serves 512 px (`@2x`) tiles, AWS Terrarium serves 256, and the
/// URL says nothing about which. So the first tile that decodes fixes the edge
/// for the run, and any later tile that disagrees stops it — one assembly grid
/// has one stride, and guessing which of the two sizes is "right" would fold
/// half the tiles either way.
///
/// The zoom, and therefore the ground resolution, is the caller's choice and is
/// untouched by this: a 512 px z12 tile simply carries finer samples over the
/// same ground, and the area-averaging in [`avg_cell`] absorbs them.
#[derive(Default)]
struct TileSize {
    edge: Option<usize>,
}

impl TileSize {
    /// Adopt this strip's tiles and return the edge to assemble it with.
    ///
    /// Fails on a mixed-size source, and on any tile whose decode was refused
    /// for its geometry — both are the source being wrong for every tile, not
    /// one tile being lost. Returns [`DEFAULT_TILE_PX`] while nothing has
    /// decoded yet; such a strip has no tile data to place, so the grid it
    /// sizes is written out as void whatever the edge.
    fn adopt(&mut self, results: &[(u32, u32, Result<(Vec<f32>, usize)>)]) -> Result<usize> {
        for (tx, ty, r) in results {
            match r {
                Ok((_, edge)) => match self.edge {
                    None => self.edge = Some(*edge),
                    Some(e) if e != *edge => anyhow::bail!(
                        "{TILE_GEOMETRY_ERR}: tile x={tx} y={ty} is {edge}x{edge} px but \
                         this source already served {e}x{e} px tiles; one XYZ service \
                         cannot mix tile sizes — the assembly grid has one stride."
                    ),
                    _ => {}
                },
                Err(e) => {
                    let msg = format!("{e:#}");
                    if msg.contains(TILE_GEOMETRY_ERR) {
                        anyhow::bail!("{msg} (tile x={tx} y={ty})");
                    }
                }
            }
        }
        Ok(self.edge.unwrap_or(DEFAULT_TILE_PX))
    }
}

// -- Download diagnostics -----------------------------------------------------

struct DownloadStats {
    ok_count: AtomicUsize,
    bytes_downloaded: AtomicU64,
    retries: AtomicUsize,
    err_timeout: AtomicUsize,
    err_connect: AtomicUsize,
    err_http_429: AtomicUsize,
    /// HTTP 404 — the service answered, and the answer is "no tile here".
    /// Counted apart from every other error because it is DATA, not failure:
    /// a bounded source (a local fixture, a regional DEM service) answers 404
    /// for the whole world outside its coverage, and those pixels are written
    /// as void — which consumers that fill voids (Waveshed Site Analysis,
    /// `void_fill_m: 0.0`) read as 0 m sea level. However many there are,
    /// they never make the run fatal; see [`fetch_failure_is_fatal`].
    err_http_404: AtomicUsize,
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
    backfilled: AtomicUsize,
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
            err_http_404: AtomicUsize::new(0),
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
            backfilled: AtomicUsize::new(0),
        }
    }

    fn total_errors(&self) -> usize {
        self.err_timeout.load(Ordering::Relaxed)
            + self.err_connect.load(Ordering::Relaxed)
            + self.err_http_429.load(Ordering::Relaxed)
            + self.err_http_404.load(Ordering::Relaxed)
            + self.err_http_4xx.load(Ordering::Relaxed)
            + self.err_http_5xx.load(Ordering::Relaxed)
            + self.err_decode.load(Ordering::Relaxed)
            + self.err_other.load(Ordering::Relaxed)
    }

    /// Requests the service answered with HTTP 404 — "no tile here".
    fn no_data_count(&self) -> usize {
        self.err_http_404.load(Ordering::Relaxed)
    }

    /// Errors that mean the download itself FAILED — everything except 404.
    ///
    /// A timeout, a refused connection, a 403 (bad key), a 429, a 5xx or a
    /// decode error says "I could not get the data that is there"; a 404 says
    /// "there is no data there". Only the first kind can make a run fatal —
    /// judging fatality on the sum let a bounded source's legitimate
    /// off-coverage 404s abort a run whose in-coverage half was perfect.
    fn fatal_errors(&self) -> usize {
        self.total_errors() - self.no_data_count()
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
        push("HTTP_404_no_data", self.err_http_404.load(Ordering::Relaxed));
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
        let repaired = self.backfilled.load(Ordering::Relaxed);
        if repaired > 0 {
            eprintln!("[Stats] Repaired from coarser parent tiles: {} \
                       (missing at this zoom, present at a lower one)", repaired);
        }
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

            // No-data is not failure, so it gets its own machine-readable
            // line: callers (the Waveshed plugin parses this) turn it into a
            // user-facing "part of your area has no data and reads as 0 m sea
            // level" warning instead of a network diagnosis.
            let n404 = self.no_data_count();
            if n404 > 0 {
                eprintln!("[Stats] NO-DATA (HTTP 404): {} of {} tile(s) — the \
                           service has no data there; written as void", n404, total);
                if ok == 0 {
                    eprintln!("[Stats] >>> every answered request was 404 — the \
                               area is entirely outside this service's coverage, \
                               or the source does not serve this zoom. The output \
                               is all void (0 m sea level where the consumer \
                               fills voids).");
                }
            }

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
) -> Vec<(u32, u32, Result<(Vec<f32>, usize)>)> {
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
                let mut result: Result<(Vec<f32>, usize)> =
                    Err(anyhow::anyhow!("not started"));
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
                            || msg.contains(TILE_GEOMETRY_ERR)
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
                // A tile that never arrived at all is the same hole at a
                // larger scale, so it gets the same repair. `result` keeps the
                // failure, so the error stats below still see the 404: the
                // backfill supplies pixels, it does not un-fail the request.
                let mut repair: Option<(Vec<f32>, usize)> = None;
                if result.is_err() {
                    repair = fill_from_parents(
                        &client, url_template, zoom, tx, ty, dec, None,
                    ).await;
                    if repair.is_some() {
                        stats.backfilled.fetch_add(1, Ordering::Relaxed);
                    }
                } else if matches!(&result, Ok((g, _)) if g.iter().any(|&v| v <= NODATA_M)) {
                    if let Ok((g, ts)) = result {
                        result = fill_from_parents(
                            &client, url_template, zoom, tx, ty, dec, Some((g, ts)),
                        )
                        .await
                        .ok_or_else(|| anyhow::anyhow!("unreachable: edge was known"));
                    }
                }

                stats.in_flight.fetch_sub(1, Ordering::Relaxed);
                drop(permit);
                let tile_us = t0.elapsed().as_micros() as u64;
                stats.tile_us_sum.fetch_add(tile_us, Ordering::Relaxed);
                stats.tile_us_max.fetch_max(tile_us, Ordering::Relaxed);
                stats.tile_us_min.fetch_min(tile_us, Ordering::Relaxed);

                // Classify final result (only after all retries exhausted).
                // A tile the parent backfill repaired counts as OK, not as an
                // error: `total_tiles()` is ok+errors and feeds both the
                // fatal-failure guard and the plugin's success-rate parser, so
                // counting a repaired tile as a failure would abort a run whose
                // output is complete. Repairs get their own [Stats] line.
                if repair.is_some() {
                    stats.ok_count.fetch_add(1, Ordering::Relaxed);
                } else {
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
                                    // 404 is data ("no tile here"), not failure —
                                    // never fatal, whatever the count.
                                    404 => { stats.err_http_404.fetch_add(1, Ordering::Relaxed); }
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
                                || msg.contains(TILE_GEOMETRY_ERR)
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

                (tx, ty, repair.map(Ok).unwrap_or(result))
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
                                // Same rule as download_strip: 404 = no data.
                                404 => { stats.err_http_404.fetch_add(1, Ordering::Relaxed); }
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

/// The exact `(tx, ty, URL)` list the web downloader fetches for a source-tile
/// rectangle, shard assignment included. Every consumer of tile URLs (the
/// downloader itself and the `tile_urls_for_job` prefetch export) goes through
/// this one function so the fetch set and the warmed set can never drift.
///
/// The shard is chosen from the tile *coordinates* — `(tx + ty) % n` — not
/// from the enumeration index, so a given tile has the same URL in every run
/// and in the prefetcher. Stable URLs are what let the browser HTTP cache
/// work across runs; `(x + y) % n` also balances contiguous rectangles as
/// tightly as round-robin, which a scrambling hash does not (measured
/// worst-case 2–3× per-host load on small rectangles).
pub fn plan_tile_urls(
    url_template: &str,
    zoom: u32,
    x0: u32,
    x1: u32,
    y0: u32,
    y1: u32,
) -> Vec<(u32, u32, String)> {
    let templates = shard_s3_templates(url_template);
    let shard_count = templates.len();
    (y0..=y1)
        .flat_map(|ty| (x0..=x1).map(move |tx| (tx, ty)))
        .map(|(tx, ty)| {
            let url = templates[((tx as u64 + ty as u64) % shard_count as u64) as usize]
                .replace("{z}", &zoom.to_string())
                .replace("{x}", &tx.to_string())
                .replace("{y}", &ty.to_string());
            (tx, ty, url)
        })
        .collect()
}

/// Full source-tile rectangle of a job (derived from its output-tile grid),
/// intersected with `fetch_bounds` when present. `None` means the ROI misses
/// the grid entirely — nothing to fetch, all-void terrain.
pub fn job_fetch_rect(job: &DownloadJob) -> Option<(u32, u32, u32, u32)> {
    let (mut bb_s, mut bb_n, mut bb_w, mut bb_e) = (f64::MAX, f64::MIN, f64::MAX, f64::MIN);
    for t in &job.tiles {
        let pd = t.resolution_m / 111_111.0;
        let sp = t.size_px as f64 * pd;
        bb_s = bb_s.min(t.ul_lat - sp);
        bb_n = bb_n.max(t.ul_lat);
        bb_w = bb_w.min(t.ul_lon);
        bb_e = bb_e.max(t.ul_lon + sp);
    }
    let (x0, x1) = (lon2tx(bb_w, job.zoom), lon2tx(bb_e, job.zoom));
    let (y0, y1) = (lat2ty(bb_n, job.zoom), lat2ty(bb_s, job.zoom));
    intersect_fetch_rect(x0, x1, y0, y1, job.fetch_bounds.as_ref(), job.zoom)
}

/// Intersect a source-tile rectangle with an optional [`FetchBounds`].
fn intersect_fetch_rect(
    x0: u32,
    x1: u32,
    y0: u32,
    y1: u32,
    fb: Option<&FetchBounds>,
    zoom: u32,
) -> Option<(u32, u32, u32, u32)> {
    match fb {
        None => Some((x0, x1, y0, y1)),
        Some(fb) => {
            let ix0 = x0.max(lon2tx(fb.west, zoom));
            let ix1 = x1.min(lon2tx(fb.east, zoom));
            let iy0 = y0.max(lat2ty(fb.north, zoom));
            let iy1 = y1.min(lat2ty(fb.south, zoom));
            if ix0 > ix1 || iy0 > iy1 { None } else { Some((ix0, ix1, iy0, iy1)) }
        }
    }
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
    let shard_count = shard_s3_templates(url_template).len();
    if shard_count > 1 {
        eprintln!("[DownloadWeb] Domain sharding: {} hostnames × 6 conn = {} concurrent",
            shard_count, shard_count * 6);
    }

    // One planner builds every tile URL (shared with `tile_urls_for_job`).
    let tile_info = plan_tile_urls(url_template, zoom, x0, x1, y0, y1);

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
                    } else if reason.contains("404") {
                        // Same rule as the native paths: 404 = no data there.
                        stats.err_http_404.fetch_add(1, Ordering::Relaxed);
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

    // The file-based path seek-writes sparse .abt files ("file grows as
    // strips write data"), so rows a fetch ROI skipped would read back as
    // 0 m sea level instead of the void sentinel — a no-data contract
    // violation. Reject loudly rather than mis-support it; the in-memory
    // path (`run_download_mem`) pre-fills void and supports `fetch_bounds`.
    if job.fetch_bounds.is_some() {
        anyhow::bail!(
            "fetch_bounds is not supported by the file-based download path \
             (sparse output files would read skipped rows as 0 m sea level); \
             omit it, or use the in-memory downloader"
        );
    }

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    rt.block_on(run_download_async(job))
}

/// Whether *failed* genuinely-failed fetches of *attempted* XYZ tiles must
/// fail the whole run.
///
/// *failed* is [`DownloadStats::fatal_errors`] — timeouts, refused
/// connections, 403/429/5xx, decode errors — and deliberately NOT the 404s.
/// An HTTP 404 is the service answering "no tile here": a bounded source (a
/// regional DEM service, a local fixture) 404s the entire world outside its
/// coverage, and a run over its edge is a legitimate run whose off-coverage
/// pixels are written as the `-9999` void sentinel — which a consumer that
/// fills voids (Waveshed Site Analysis, `void_fill_m: 0.0`) reads as 0 m sea
/// level, by contract. Any number of 404s therefore stays non-fatal; they are
/// reported on their own `[Stats] NO-DATA` line instead, so the caller can
/// tell the user which part of the result is sea, loudly, without killing the
/// part that is terrain.
///
/// The failures that CAN be fatal mean "the data is there and I could not get
/// it": there the line is drawn at a **majority** of the requested tiles.
/// Below it the output is still mostly real terrain and the `[Stats] ERRORS`
/// summary is the right response; above it (the network died, the key
/// expired, the source serves WebP to a PNG decoder) there is nothing worth
/// keeping — a mostly-void output that LOOKS like a bounded source is exactly
/// the silent flat-sea failure this guard exists for.
#[cfg(feature = "native")]
fn fetch_failure_is_fatal(failed: usize, attempted: usize) -> bool {
    attempted > 0 && failed * 2 > attempted
}

/// Removes the `.abt` files a run created, unless it reached [`AbtCleanup::keep`].
///
/// Every output file is created with a valid header *before* the first tile is
/// fetched and is sized to full length once assembly ends — so a run that bails
/// out afterwards leaves a complete, well-formed `.abt` behind, void where the
/// tiles never arrived and real terrain where a few did. Callers pool tiles by
/// filename, so that file is a cache hit for every later run: one refusal
/// poisons the pool with a mostly-empty tile. A failed download must therefore
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

    // The assembly grid is `nx * tile_edge` samples wide, and the tile edge is
    // not known until a tile has been decoded — see `TileSize`. Geometry is
    // therefore derived per strip, below; only the RAM estimate has to guess,
    // and it guesses high.
    let mut tile_size = TileSize::default();

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
    // Sized for the largest tile a service is assumed to serve: this runs
    // before the first fetch, so the real edge is still unknown, and guessing
    // 256 for a 512 px source would under-budget the pipeline fourfold.
    let bytes_per_tile_data: usize = MAX_TILE_PX * MAX_TILE_PX * 4; // f32 per tile pixel
    let actual_strip_rows = (strip_rows as usize).min(ny);
    let strip_mem = nx * actual_strip_rows * bytes_per_tile_data;
    let mini_grid_mem = nx * MAX_TILE_PX * MAX_TILE_PX * 4;

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

    type StripHandle =
        tokio::task::JoinHandle<Vec<(u32, u32, Result<(Vec<f32>, usize)>)>>;

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
        // Await the front of the pipeline.
        let results = pipeline.pop_front().unwrap().await?;

        // Fix (or re-check) the tile edge this run assembles for, and derive
        // the grid from it. A source that mixes sizes, or that served a tile
        // this decoder refused on its geometry, stops the run here.
        let ts = tile_size.adopt(&results)?;
        let gw = nx * ts;
        let gpx = (glr_lon - gul_lon) / gw as f64;

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
                        if let Ok((elev, _)) = r {
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

                    // Process one tile-row at a time. Grid = gw×ts ≈ 36 MB (fits in L3).
                    let t_work = Instant::now();
                    let mut mini_grid = vec![NO_TILE_M; gw * ts];
                    let mut total_rows = 0usize;
                    let mut total_px = 0usize;
                    const PAD: [u8; 256] = [0u8; 256];

                    for tr in 0..sny {
                        let ty = sy0 + tr as u32;

                        // Clear the mini-grid so failed tiles don't leak stale
                        // data from previous tile-rows. The clear value is
                        // "no tile", not 0 m: whatever a failed tile leaves
                        // behind is what gets written out for it.
                        mini_grid.fill(NO_TILE_M);

                        for &(tx, elev) in &tiles_by_row[tr] {
                            let col = (tx - x0) as usize * ts;
                            for py in 0..ts {
                                let cw = ts.min(gw - col);
                                mini_grid[py * gw + col..py * gw + col + cw]
                                    .copy_from_slice(&elev[py * ts..py * ts + cw]);
                            }
                        }

                        let tr_top = ty2lat(ty, zoom);
                        let tr_bot = ty2lat(ty + 1, zoom);
                        let tr_spy = (tr_top - tr_bot) / ts as f64;

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
                                        ts,
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

    // Bring every file to full length — assembly only wrote the rows some
    // tile-row covered, and `AbtWriter::create` deliberately skips the
    // `set_len` pre-allocation. The tail is written as void rather than
    // `set_len`'s zeros, for the same reason the grid is: 0 m is sea level.
    for (spec, path) in &abt_specs {
        let sz = spec.size_px as usize;
        let stride = ((sz * 2) + 255) & !255;
        let expected = 44 + stride as u64 * sz as u64;
        if let Ok(mut f) = fs::OpenOptions::new().write(true).open(path) {
            let actual = f.metadata().map(|m| m.len()).unwrap_or(0);
            if actual >= expected {
                continue;
            }
            // Rows are written whole, so the tail starts on a row boundary;
            // a partial one (an interrupted flush) is rewritten from its start.
            let written = actual.saturating_sub(44) / stride as u64;
            let row = void_row_bytes(sz, stride);
            let wrote = (|| -> Result<()> {
                f.seek(SeekFrom::Start(44 + written * stride as u64))?;
                let mut w = BufWriter::with_capacity(1 << 20, &mut f);
                for _ in written..sz as u64 {
                    w.write_all(&row)?;
                }
                w.flush()?;
                Ok(())
            })();
            if wrote.is_err() {
                let _ = f.set_len(expected);
            }
        }
    }

    // A run that genuinely FAILED to fetch a majority of its tiles is not a
    // successful run — the network died, the key expired, or the source serves
    // bytes the decoder cannot read, and a mostly-void output would cache as
    // plausible flat sea. Name the error class: "could not be fetched" alone
    // reads as a network fault even when the real cause is a source serving
    // WebP to a PNG decoder. `cleanup` takes the files with it.
    //
    // 404s are deliberately NOT in `failed`: the service answering "no tile
    // here" is data, not failure. A run over the edge of a bounded source (or
    // entirely outside it) keeps its output — void where the service has
    // nothing, which consumers that fill voids read as 0 m sea level — and
    // the `[Stats] NO-DATA` line in the summary above is the loud version of
    // that story. See `fetch_failure_is_fatal`.
    let failed = stats.fatal_errors();
    let attempted = stats.total_tiles();
    if fetch_failure_is_fatal(failed, attempted) {
        stats.log_summary(start.elapsed().as_secs_f64());
        let no_data = stats.no_data_count();
        anyhow::bail!(
            "Tile download failed: {} of {} terrain tiles ({}%) could not be fetched; \
             the output would be mostly void, not terrain. Errors: {}.{} Check that \
             the tile source serves zoom {} over this area and that the network is \
             reachable.",
            failed,
            attempted,
            failed * 100 / attempted.max(1),
            stats.error_breakdown(),
            if no_data > 0 {
                format!(" (A further {} tile(s) answered HTTP 404 — no data there — \
                         which alone would not fail the run.)", no_data)
            } else {
                String::new()
            },
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

    // Optional fetch ROI (additive `fetch_bounds`): only source tiles inside
    // it are fetched and decoded; everything else keeps the void pre-fill.
    // Grid geometry, strip layout and the output tiles are unchanged.
    let fetch_rect = intersect_fetch_rect(x0, x1, y0, y1, job.fetch_bounds.as_ref(), job.zoom);
    let (fx0, fx1, fy0, fy1) = fetch_rect.unwrap_or((1, 0, 1, 0)); // empty inclusive ranges
    let ftotal = fetch_rect.map_or(0usize, |(a, b, c, d)| ((b - a + 1) * (d - c + 1)) as usize);

    eprintln!("[DownloadMem] z={} tiles={}x{}={} connections={}",
        job.zoom, nx, ny, total, conns);
    if job.fetch_bounds.is_some() {
        eprintln!("[DownloadMem] fetch_bounds: fetching {ftotal} of {total} source tiles");
    }

    // 3. Prepare in-memory .abt buffers (header + a body of void).
    //    The body is pre-filled with the `-9999` sentinel so a pixel no tile
    //    ever covered reads as absent; a zeroed body would read as sea level.
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
        let sz = spec.size_px as usize;
        let total_bytes = 44 + stride * sz;
        let mut buf = vec![0u8; total_bytes];
        {
            let row = void_row_bytes(sz, stride);
            for y in 0..sz {
                buf[44 + y * stride..44 + (y + 1) * stride].copy_from_slice(&row);
            }
        }

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

    // 4. The x-lookup tables (pixel x → source column span) and the assembly
    //    grid both scale with the XYZ tile edge, which is not known until a
    //    tile has been decoded — see `TileSize`. They are built on the first
    //    strip that decodes anything and rebuilt only if the edge changes,
    //    which it can do at most once (a second change is a fatal mixed-size
    //    source).
    let mut tile_size = TileSize::default();
    let mut ts = 0usize;
    let mut gw = 0usize;
    let mut x_luts: Vec<Vec<(u32, u32)>> = Vec::new();
    let mut mini_grid: Vec<f32> = Vec::new();

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
    if let Some(cb) = &on_progress { cb(0, ftotal); }

    // ── WASM: batch-download the fetch rect via browser fetch ───
    // Bypasses reqwest; hands every URL to the browser in one JS call.
    // An empty fetch rect skips the network entirely (all tiles stay void).
    #[cfg(target_arch = "wasm32")]
    let mut tile_cache = if ftotal == 0 {
        std::collections::HashMap::new()
    } else {
        download_all_tiles_web(
            &job.url_template, zoom, fx0, fx1, fy0, fy1,
            conns.min(200),
            &stats, start, js_progress,
        ).await
    };
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

    #[cfg(target_arch = "wasm32")]
    let mut decoded_count: usize = 0;
    #[cfg(target_arch = "wasm32")]
    let mut last_decode_pct5: usize = 0;

    for (strip_idx, &(sy0, sy1)) in strips.iter().enumerate() {
        let sny = (sy1 - sy0 + 1) as usize;

        // ── Get raw tile data (fetch rect only; the rest stays void) ──
        let fsy0 = sy0.max(fy0);
        let fsy1 = sy1.min(fy1);

        #[cfg(target_arch = "wasm32")]
        let raw_results: Vec<(u32, u32, Result<Vec<u8>>)> = if fsy0 > fsy1 {
            Vec::new()
        } else {
            (fsy0..=fsy1)
                .flat_map(|ty| (fx0..=fx1).map(move |tx| (tx, ty)))
                .map(|(tx, ty)| match tile_cache.remove(&(tx, ty)) {
                    Some(bytes) => (tx, ty, Ok(bytes)),
                    None => (tx, ty, Err(anyhow::anyhow!("download failed"))),
                })
                .collect()
        };

        #[cfg(not(target_arch = "wasm32"))]
        let raw_results = if fsy0 > fsy1 {
            Vec::new()
        } else {
            download_strip_raw(
                client, &job.url_template, zoom, fx0, fx1, fsy0, fsy1,
                conns, &progress, ftotal, &stats, start, &semaphore,
                on_progress,
            ).await
        };

        // Phase 2: Decode PNGs sequentially, then classify decode errors.

        let mut results: Vec<(u32, u32, Result<(Vec<f32>, usize)>)> =
            Vec::with_capacity(raw_results.len());
        for (tx, ty, raw) in raw_results {
            match raw {
                Ok(png_bytes) => {
                    match decode_png(&png_bytes, dec) {
                        Ok(tile) => {
                            results.push((tx, ty, Ok(tile)));

                            // Report decode progress every 5 % (phase 1).
                            #[cfg(target_arch = "wasm32")]
                            {
                                decoded_count += 1;
                                if let Some(f) = js_progress {
                                    let pct5 = if ftotal > 0 { decoded_count * 20 / ftotal } else { 0 };
                                    if pct5 > last_decode_pct5 || decoded_count == ftotal || decoded_count == 1 {
                                        last_decode_pct5 = pct5;
                                        let _ = f.call3(
                                            &wasm_bindgen::JsValue::NULL,
                                            &wasm_bindgen::JsValue::from(1u32),
                                            &wasm_bindgen::JsValue::from(decoded_count as u32),
                                            &wasm_bindgen::JsValue::from(ftotal as u32),
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

        // Fix (or re-check) the tile edge, and size the grid to it. A source
        // that mixes tile sizes, or that served a tile this decoder refused on
        // its geometry, stops the run here rather than folding the assembly.
        let strip_ts = tile_size.adopt(&results)?;
        if strip_ts != ts {
            ts = strip_ts;
            gw = nx * ts;
            let gpx = (glr_lon - gul_lon) / gw as f64;
            x_luts = abt_bufs.iter().map(|abt| {
                x_span_table(abt.size_px as usize, abt.ul_lon, abt.pd, gul_lon, gpx, gw)
            }).collect();
            mini_grid = vec![NO_TILE_M; gw * ts];
        }

        // Group tiles by row within the strip.
        let mut tiles_by_row: Vec<Vec<(u32, &[f32])>> = vec![Vec::new(); sny];
        for (tx, ty, r) in &results {
            if let Ok((elev, _)) = r {
                tiles_by_row[(*ty - sy0) as usize].push((*tx, elev.as_slice()));
            }
        }

        // Process one tile-row at a time.
        for tr in 0..sny {
            let ty = sy0 + tr as u32;

            // Clear the mini-grid so failed tiles don't leak stale data from
            // previous tile-rows. The clear value is "no tile", not 0 m.
            mini_grid.fill(NO_TILE_M);

            // Fill mini-grid from downloaded tile data.
            for &(tx, elev) in &tiles_by_row[tr] {
                let col = (tx - x0) as usize * ts;
                for py in 0..ts {
                    let cw = ts.min(gw - col);
                    mini_grid[py * gw + col..py * gw + col + cw]
                        .copy_from_slice(&elev[py * ts..py * ts + cw]);
                }
            }

            let tr_top = ty2lat(ty, zoom);
            let tr_bot = ty2lat(ty + 1, zoom);
            let tr_spy = (tr_top - tr_bot) / ts as f64;

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
                        ts,
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
                    // Stride padding is already zeroed by `void_row_bytes`.
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

    // ── fetch_bounds / URL planning ────────────────────────────────────────

    fn job_json(fetch_bounds: Option<&str>) -> String {
        let fb = fetch_bounds.map_or(String::new(), |f| format!(r#""fetch_bounds": {f},"#));
        format!(
            r#"{{
                "url_template": "https://s3.amazonaws.com/elevation-tiles-prod/terrarium/{{z}}/{{x}}/{{y}}.png",
                "encoding": "terrarium",
                "output_dir": "/mem",
                {fb}
                "tiles": [{{
                    "filename": "tile_0_0.abt",
                    "ul_lat": 47.04, "ul_lon": 8.42,
                    "size_px": 2048, "resolution_m": 30.0
                }}],
                "zoom": 12
            }}"#
        )
    }

    #[test]
    fn fetch_bounds_is_optional_and_parses() {
        let plain: DownloadJob = serde_json::from_str(&job_json(None)).unwrap();
        assert!(plain.fetch_bounds.is_none());

        let with: DownloadJob = serde_json::from_str(&job_json(Some(
            r#"{"south": 46.86, "north": 47.04, "west": 8.42, "east": 8.68}"#,
        )))
        .unwrap();
        let fb = with.fetch_bounds.unwrap();
        assert_eq!(fb.north, 47.04);
        assert_eq!(fb.east, 8.68);
    }

    #[test]
    fn fetch_rect_without_bounds_is_the_full_grid_rect() {
        let job: DownloadJob = serde_json::from_str(&job_json(None)).unwrap();
        // Mirror run_download_mem's own bbox derivation for the single tile.
        let pd = 30.0 / 111_111.0;
        let sp = 2048.0 * pd;
        let (x0, x1) = (lon2tx(8.42, 12), lon2tx(8.42 + sp, 12));
        let (y0, y1) = (lat2ty(47.04, 12), lat2ty(47.04 - sp, 12));
        assert_eq!(job_fetch_rect(&job), Some((x0, x1, y0, y1)));
    }

    #[test]
    fn fetch_bounds_clamps_the_rect_and_disjoint_bounds_empty_it() {
        let job: DownloadJob = serde_json::from_str(&job_json(Some(
            r#"{"south": 46.86, "north": 47.04, "west": 8.42, "east": 8.68}"#,
        )))
        .unwrap();
        let (fx0, fx1, fy0, fy1) = job_fetch_rect(&job).unwrap();
        let clamped = ((fx1 - fx0 + 1) * (fy1 - fy0 + 1)) as usize;
        // The bbox needs a handful of z12 tiles; the 2048 px grid tile spans
        // 0.553 deg and would cover 70-80. The clamp must cut most of that.
        assert!(clamped >= 9 && clamped <= 25, "clamped = {clamped}");
        assert!(fx0 >= lon2tx(8.42, 12) && fy0 >= lat2ty(47.04, 12));

        let disjoint: DownloadJob = serde_json::from_str(&job_json(Some(
            r#"{"south": -10.0, "north": -9.0, "west": 100.0, "east": 101.0}"#,
        )))
        .unwrap();
        assert_eq!(job_fetch_rect(&disjoint), None);
    }

    #[test]
    fn planned_urls_are_stable_across_different_rectangles() {
        let tmpl = "https://s3.amazonaws.com/elevation-tiles-prod/terrarium/{z}/{x}/{y}.png";
        // The same tile must get the same URL regardless of which rectangle
        // (i.e. which run) it appears in — this is what lets the HTTP cache
        // and the prefetcher work across runs.
        let a = plan_tile_urls(tmpl, 12, 2143, 2150, 1440, 1449);
        let b = plan_tile_urls(tmpl, 12, 2145, 2146, 1442, 1443);
        for (tx, ty, url_b) in &b {
            let url_a = &a.iter().find(|(ax, ay, _)| ax == tx && ay == ty).unwrap().2;
            assert_eq!(url_a, url_b, "tile {tx}/{ty} changed hosts between runs");
        }
    }

    #[test]
    fn planned_urls_balance_hosts_like_round_robin() {
        let tmpl = "https://s3.amazonaws.com/elevation-tiles-prod/terrarium/{z}/{x}/{y}.png";
        let urls = plan_tile_urls(tmpl, 12, 2143, 2150, 1440, 1449); // 8 x 10 = 80
        assert_eq!(urls.len(), 80);
        let mut per_host = std::collections::HashMap::new();
        for (_, _, u) in &urls {
            let host = u.split('/').nth(2).unwrap().to_string();
            *per_host.entry(host).or_insert(0usize) += 1;
        }
        assert_eq!(per_host.len(), 6, "expected 6 shard hosts");
        // Round-robin ideal is 80/6 = 13.3; (x+y) % 6 stays within one row of it.
        assert!(per_host.values().all(|&n| n <= 22), "per-host: {per_host:?}");
        // Substitution sanity.
        assert!(urls[0].2.contains("/12/") && urls[0].2.ends_with(".png"));
    }

    #[test]
    fn non_s3_templates_stay_single_host() {
        let urls = plan_tile_urls("https://dem.example.org/t/{z}/{x}/{y}.png", 11, 5, 6, 7, 8);
        assert!(urls.iter().all(|(_, _, u)| u.starts_with("https://dem.example.org/")));
    }

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

    /// A `w`x`h` Terrarium PNG of uniform 100 m terrain (RGB 128,100,0).
    #[cfg(test)]
    fn terrarium_png(w: u32, h: u32, depth: png::BitDepth) -> Vec<u8> {
        let mut out = Vec::new();
        {
            let mut enc = png::Encoder::new(&mut out, w, h);
            enc.set_color(png::ColorType::Rgb);
            enc.set_depth(depth);
            let mut wr = enc.write_header().unwrap();
            let n = (w * h) as usize * 3 * if depth == png::BitDepth::Sixteen { 2 } else { 1 };
            let px: Vec<u8> = if depth == png::BitDepth::Sixteen {
                [0u8, 128, 0, 100, 0, 0].iter().copied().cycle().take(n).collect()
            } else {
                [128u8, 100, 0].iter().copied().cycle().take(n).collect()
            };
            wr.write_image_data(&px).unwrap();
        }
        out
    }

    #[test]
    fn the_decoder_reports_the_tile_edge_it_actually_read() {
        // MapTiler's terrain-rgb serves 512 px (@2x); Terrarium serves 256.
        // Assuming either is how a 512 px tile got folded in half.
        for edge in [256u32, 512] {
            let (grid, ts) =
                decode_png(&terrarium_png(edge, edge, png::BitDepth::Eight), dec_terrarium)
                    .unwrap();
            assert_eq!(ts, edge as usize);
            assert_eq!(grid.len(), (edge * edge) as usize);
            assert_eq!(grid[0], 100.0);
        }
    }

    #[test]
    fn a_tile_the_assembly_cannot_place_is_refused_naming_what_arrived() {
        // No square grid to fold into: refuse rather than guess a stride.
        let e = decode_png(&terrarium_png(256, 128, png::BitDepth::Eight), dec_terrarium)
            .unwrap_err().to_string();
        assert!(e.contains("256x128"), "the dimensions must be named; got {e:?}");
        assert!(e.contains(TILE_GEOMETRY_ERR), "must be classed as geometry; got {e:?}");

        // 16 bits per channel puts two bytes where this decoder reads one.
        let e = decode_png(&terrarium_png(256, 256, png::BitDepth::Sixteen), dec_terrarium)
            .unwrap_err().to_string();
        assert!(e.contains("16-bit"), "the depth must be named; got {e:?}");
        assert!(e.contains(TILE_GEOMETRY_ERR), "must be classed as geometry; got {e:?}");
    }

    #[test]
    fn the_run_takes_its_tile_size_from_the_first_tile_and_holds_it() {
        let ok = |edge: usize| (0u32, 0u32, Ok((vec![0.0f32; 1], edge)));

        let mut ts = TileSize::default();
        assert_eq!(ts.adopt(&[]).unwrap(), DEFAULT_TILE_PX, "nothing decoded yet");
        assert_eq!(ts.adopt(&[ok(512)]).unwrap(), 512);
        assert_eq!(ts.adopt(&[]).unwrap(), 512, "the edge holds across strips");

        // One service, one stride: a second size is a server bug, not a grid
        // to guess at, and half the tiles would fold whichever way we guessed.
        let e = ts.adopt(&[ok(256)]).unwrap_err().to_string();
        assert!(e.contains("mix tile sizes"), "got {e:?}");
        assert!(e.contains("256x256") && e.contains("512x512"), "both sizes named; got {e:?}");

        // A geometry refusal condemns the run, not just the tile that carried it.
        let mut ts = TileSize::default();
        let bad = vec![(3u32, 4u32, Err(anyhow::anyhow!(
            "{TILE_GEOMETRY_ERR}: tile is 256x128 px; XYZ tiles must be square")))];
        let e = ts.adopt(&bad).unwrap_err().to_string();
        assert!(e.contains("256x128") && e.contains("x=3") && e.contains("y=4"), "got {e:?}");

        // An ordinary lost tile is not: sparse 404s are normal at coverage edges.
        let mut ts = TileSize::default();
        let lost = vec![(0u32, 0u32, Err(anyhow::anyhow!("HTTP 404")))];
        assert_eq!(ts.adopt(&lost).unwrap(), DEFAULT_TILE_PX);
    }

    #[test]
    fn a_void_row_carries_the_sentinel_and_leaves_the_padding_alone() {
        let row = void_row_bytes(6, 256);
        assert_eq!(row.len(), 256);
        for x in 0..6 {
            assert_eq!(i16::from_le_bytes([row[x * 2], row[x * 2 + 1]]), VOID_ELEV);
        }
        assert!(row[12..].iter().all(|&b| b == 0), "stride slack is not pixel data");
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
        // Off the grid entirely: empty, which `avg_cell` writes as the void
        // sentinel — what a tile that failed to download already produces.
        assert_eq!(grid_span(-4.0, -3.5, 4), (0, 0));
        assert_eq!(grid_span(9.0, 9.5, 4), (0, 0));
    }

    #[test]
    fn an_output_row_is_clipped_to_the_tile_row_held_in_memory() {
        // Assembly only ever has one tile-row of the grid, so a footprint
        // reaching past it is clipped — but never to nothing. The tile edge is
        // whatever the source served, so the clip has to follow it: a 512 px
        // `@2x` tile-row holds 512 rows, not 255.
        for ts in [256usize, 512] {
            let e = ts as f64;
            for (lo, hi) in [(0.0, 1.2), (e / 2.0 - 0.6, e / 2.0 + 0.6),
                             (e - 1.5, e + 0.5), (e - 0.4, e + 0.4)] {
                let (a, b) = tile_row_span(lo, hi, ts);
                assert!(a < b && b <= ts, "ts={ts} {lo}..{hi} gave {a}..{b}");
            }
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
        // A footprint off the grid is a hole, not sea level: 0 m is a valid
        // elevation and would pass fabricated ocean off as surveyed ground.
        assert_eq!(avg_cell(&grid, 4, 0, 0, 0, 1), VOID_ELEV);
    }

    #[test]
    fn a_cell_no_tile_covered_is_void_and_never_drags_a_neighbour_down() {
        // What the assembly grid holds where a tile 404'd.
        let grid = vec![NO_TILE_M; 4];
        assert_eq!(avg_cell(&grid, 2, 0, 2, 0, 2), VOID_ELEV, "averaged");
        assert_eq!(avg_cell(&grid, 2, 0, 1, 0, 1), VOID_ELEV, "single sample");

        // Half a footprint missing: the half that arrived is the answer, and
        // the missing half is excluded rather than averaged in.
        let edge = vec![1000.0f32, NO_TILE_M, 1000.0, NO_TILE_M];
        assert_eq!(avg_cell(&edge, 2, 0, 2, 0, 2), 2000);
    }

    #[test]
    fn a_terrarium_void_is_not_averaged_into_the_terrain_beside_it() {
        // A blank Terrarium pixel the parent backfill could not repair is
        // -32768 m. Averaging it in would put this 1000 m cell at 235 m.
        let grid = vec![1000.0f32, -32768.0, 1000.0, 1000.0];
        assert_eq!(avg_cell(&grid, 2, 0, 2, 0, 2), 2000);

        // Nothing but voids: the cell is a hole. It is not invented ground,
        // and it is not the raw -32768 m pit passed through either — that
        // number is a Terrarium encoding artefact, and `.abt` has one
        // convention for "no terrain here".
        let all_void = vec![-32768.0f32; 4];
        assert_eq!(avg_cell(&all_void, 2, 0, 2, 0, 2), VOID_ELEV);
        assert_eq!(avg_cell(&all_void, 2, 0, 1, 0, 1), VOID_ELEV, "single sample");
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
        // The case this exists for: the network died, the key expired, or the
        // source serves bytes the decoder cannot read — the counts here are
        // fatal_errors(), which 404s never enter.
        assert!(fetch_failure_is_fatal(1369, 1369));
        assert!(fetch_failure_is_fatal(1000, 1369));
        assert!(fetch_failure_is_fatal(1, 1),
                "a one-tile job whose one fetch genuinely failed");
    }

    #[test]
    fn no_data_404s_are_counted_apart_and_never_make_a_run_fatal() {
        // A 30 m run over the local Terrain-RGB fixture's edge: ~90% of the
        // requested tiles are outside the Bern box and answer 404. That is
        // the service saying "no data here", the pixels are written as void
        // (0 m sea level once the consumer fills voids) — and the run must
        // SUCCEED, however large the 404 share is.
        let s = DownloadStats::new();
        s.ok_count.fetch_add(6, Ordering::Relaxed);
        s.err_http_404.fetch_add(43, Ordering::Relaxed);
        assert_eq!(s.no_data_count(), 43);
        assert_eq!(s.fatal_errors(), 0);
        assert_eq!(s.total_tiles(), 49, "404s still count as attempted tiles");
        assert!(!fetch_failure_is_fatal(s.fatal_errors(), s.total_tiles()));

        // Even 100% 404 — a run entirely outside a bounded source — succeeds:
        // the output is all void, the summary says so loudly, and the caller
        // turns it into flat sea plus a warning, not into a network diagnosis.
        let s = DownloadStats::new();
        s.err_http_404.fetch_add(49, Ordering::Relaxed);
        assert_eq!(s.fatal_errors(), 0);
        assert!(!fetch_failure_is_fatal(s.fatal_errors(), s.total_tiles()));

        // But real failures alongside the 404s keep their own majority rule:
        // 43 no-data + 5 timeouts in 54 attempts is fine (5*2 <= 54) …
        let s = DownloadStats::new();
        s.ok_count.fetch_add(6, Ordering::Relaxed);
        s.err_http_404.fetch_add(43, Ordering::Relaxed);
        s.err_timeout.fetch_add(5, Ordering::Relaxed);
        assert_eq!(s.fatal_errors(), 5);
        assert!(!fetch_failure_is_fatal(s.fatal_errors(), s.total_tiles()));

        // … while a majority of 403s (an expired key) stays fatal: that data
        // exists and could not be fetched, and flat sea must not paper it over.
        let s = DownloadStats::new();
        s.ok_count.fetch_add(1, Ordering::Relaxed);
        s.err_http_4xx.fetch_add(3, Ordering::Relaxed);
        assert_eq!(s.fatal_errors(), 3);
        assert!(fetch_failure_is_fatal(s.fatal_errors(), s.total_tiles()));
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

        // 404s appear under their own label — they are no-data, not failure,
        // and folding them into HTTP_4xx would make an off-coverage run read
        // like a broken service.
        s.err_http_404.fetch_add(7, Ordering::Relaxed);
        assert_eq!(s.error_breakdown(),
                   "connect=1, HTTP_404_no_data=7, HTTP_4xx=2, decode=3");
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
    fn sparse_transport_failures_do_not_fail_the_run() {
        // A few dropped connections under load are normal; the majority rule
        // only condemns a run that mostly failed to fetch data that exists.
        assert!(!fetch_failure_is_fatal(0, 1369));
        assert!(!fetch_failure_is_fatal(1, 1369));
        assert!(!fetch_failure_is_fatal(137, 1369), "10% lost is still terrain");
        assert!(!fetch_failure_is_fatal(684, 1369), "exactly half is not a majority");
        assert!(fetch_failure_is_fatal(685, 1369), "one past half is");
        // Nothing attempted is not a failure — an empty job is caught elsewhere.
        assert!(!fetch_failure_is_fatal(0, 0));
    }
}

