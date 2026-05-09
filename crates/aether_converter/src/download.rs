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
use std::fs::{self, File};
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Instant;
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

fn lon2tx(lon: f64, z: u32) -> u32 {
    let n = 2f64.powi(z as i32);
    ((lon + 180.0) / 360.0 * n).floor().max(0.0) as u32
}
fn lat2ty(lat: f64, z: u32) -> u32 {
    let n = 2f64.powi(z as i32);
    let r = lat.to_radians();
    ((1.0 - r.tan().asinh() / std::f64::consts::PI) / 2.0 * n).floor().max(0.0) as u32
}
fn ty2lat(y: u32, z: u32) -> f64 {
    let n = 2f64.powi(z as i32);
    (std::f64::consts::PI * (1.0 - 2.0 * y as f64 / n)).sinh().atan().to_degrees()
}
fn tx2lon(x: u32, z: u32) -> f64 {
    let n = 2f64.powi(z as i32);
    x as f64 / n * 360.0 - 180.0
}

// -- Decode -------------------------------------------------------------------

#[inline]
fn dec_terrarium(r: u8, g: u8, b: u8) -> f32 {
    (r as f32 * 256.0 + g as f32 + b as f32 / 256.0) - 32768.0
}
#[inline]
fn dec_mapbox(r: u8, g: u8, b: u8) -> f32 {
    -10000.0 + (r as f32 * 6553.6 + g as f32 * 25.6 + b as f32 * 0.1)
}

fn decode_png(body: &[u8], dec: fn(u8, u8, u8) -> f32) -> Result<Vec<f32>> {
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
            let mut parts = Vec::new();
            let t = self.err_timeout.load(Ordering::Relaxed);
            let c = self.err_connect.load(Ordering::Relaxed);
            let r429 = self.err_http_429.load(Ordering::Relaxed);
            let r4xx = self.err_http_4xx.load(Ordering::Relaxed);
            let r5xx = self.err_http_5xx.load(Ordering::Relaxed);
            let d = self.err_decode.load(Ordering::Relaxed);
            let o = self.err_other.load(Ordering::Relaxed);
            if t > 0 { parts.push(format!("timeout={}", t)); }
            if c > 0 { parts.push(format!("connect={}", c)); }
            if r429 > 0 { parts.push(format!("HTTP_429_rate_limited={}", r429)); }
            if r4xx > 0 { parts.push(format!("HTTP_4xx={}", r4xx)); }
            if r5xx > 0 { parts.push(format!("HTTP_5xx={}", r5xx)); }
            if d > 0 { parts.push(format!("decode={}", d)); }
            if o > 0 { parts.push(format!("other={}", o)); }
            eprintln!("[Stats] ERRORS ({}): {}", errs, parts.join(", "));

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

fn estimate_abt_bytes(tiles: &[SubTileSpec]) -> u64 {
    tiles.iter().map(|spec| {
        let bpr = spec.size_px as u64 * 2;
        let stride = (bpr + 255) & !255;
        44 + stride * spec.size_px as u64
    }).sum()
}

// -- .abt writer --------------------------------------------------------------

struct AbtWriter {
    writer: BufWriter<File>,
    size_px: u32,
    stride: usize,
}

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
            async move {
                // Global concurrency gate — prevents PREFETCH_DEPTH × conns explosion.
                let permit = semaphore.acquire().await.unwrap();
                let t0 = Instant::now();
                let cur_flight = stats.in_flight.fetch_add(1, Ordering::Relaxed) + 1;
                stats.peak_in_flight.fetch_max(cur_flight, Ordering::Relaxed);

                // Retry loop: up to 3 retries with exponential backoff (1s, 2s, 4s).
                // Retries on: timeout, connection error, HTTP 429, HTTP 5xx.
                // No retry on: HTTP 4xx (except 429), decode errors.
                const MAX_RETRIES: u32 = 3;
                let mut result: Result<Vec<f32>> = Err(anyhow::anyhow!("not started"));
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

// -- Entry point (sync wrapper around async) ----------------------------------

pub fn run_download(job_file: &Path) -> Result<()> {
    let content = fs::read_to_string(job_file)?;
    let job: DownloadJob = serde_json::from_str(&content)?;

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    rt.block_on(run_download_async(job))
}

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
    let client = reqwest::Client::builder()
        .pool_max_idle_per_host(conns)
        .pool_idle_timeout(std::time::Duration::from_secs(60))
        .timeout(std::time::Duration::from_secs(30))
        .build()?;

    // 4. Prepare .abt files (write headers, then close — assembly reopens per-strip).
    fs::create_dir_all(&job.output_dir)?;
    let abt_specs: Vec<(SubTileSpec, PathBuf)> = {
        let mut specs = Vec::new();
        for spec in &job.tiles {
            let pd = spec.resolution_m / 111_111.0;
            let path = job.output_dir.join(&spec.filename);
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
                &sem,
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

                    let x_luts: Vec<Vec<usize>> = specs.iter().map(|(spec, _)| {
                        let pd = spec.resolution_m / 111_111.0;
                        let sz = spec.size_px as usize;
                        (0..sz).map(|x| {
                            let gc = ((spec.ul_lon + (x as f64 + 0.5) * pd - gul_lon)
                                / gpx).round() as isize;
                            if gc >= 0 && (gc as usize) < gw {
                                gc as usize
                            } else {
                                usize::MAX
                            }
                        }).collect()
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

                        // Fill mini-grid from tiles. Vec allocation zeroed it initially;
                        // subsequent iterations reuse previous data (overwritten by copy_from_slice).
                        // Skipping fill(0.0) saves 36 MB memset and keeps grid hot in L3.
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
                                    let gr = ((tr_top - lat) / tr_spy).round() as usize;
                                    if gr >= 256 { return None; }

                                    let row_off = gr * gw;
                                    let mut row_data = vec![0i16; sz];
                                    for x in 0..sz {
                                        let gc = x_lut[x];
                                        if gc < gw {
                                            row_data[x] = (grid_ref[row_off + gc] * 2.0)
                                                .round() as i16;
                                        }
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

    let elapsed = start.elapsed().as_secs_f64();
    stats.log_summary(elapsed);
    eprintln!("[Download] TOTAL: {:.1}s", elapsed);
    Ok(())
}
