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
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Instant;

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
        let file_size = 44u64 + stride as u64 * sz as u64;

        let f = File::create(path)?;
        f.set_len(file_size)?;
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

    fn write_row(&mut self, y: u32, row: &[i16]) -> Result<()> {
        let offset = 44u64 + y as u64 * self.stride as u64;
        self.writer.seek(SeekFrom::Start(offset))?;
        for &v in row { self.writer.write_i16::<LittleEndian>(v)?; }
        let pad = self.stride - self.size_px as usize * 2;
        if pad > 0 { self.writer.write_all(&vec![0u8; pad])?; }
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
            async move {
                let result = async {
                    let resp = client.get(&url).send().await?;
                    let body = resp.bytes().await?;
                    decode_png(&body, dec)
                }.await;

                let done = progress.fetch_add(1, Ordering::Relaxed) + 1;
                let pct = done * 100 / total;
                let prev = (done - 1) * 100 / total;
                if pct / 10 > prev / 10 || done == total {
                    eprintln!("[Download] {}% ({}/{})", pct, done, total);
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

    // 3. Single HTTP client — reqwest handles connection pooling internally.
    let client = reqwest::Client::builder()
        .pool_max_idle_per_host(conns)
        .pool_idle_timeout(std::time::Duration::from_secs(60))
        .timeout(std::time::Duration::from_secs(30))
        .build()?;

    // 4. Prepare .abt files.
    fs::create_dir_all(&job.output_dir)?;
    let mut abt_writers: Vec<(SubTileSpec, AbtWriter)> = Vec::new();
    for spec in &job.tiles {
        let pd = spec.resolution_m / 111_111.0;
        let path = job.output_dir.join(&spec.filename);
        let writer = AbtWriter::create(&path, spec.size_px, spec.ul_lat, spec.ul_lon, pd)?;
        abt_writers.push((spec.clone(), writer));
    }

    // 5. Process in strips.
    let strip_rows = 8; // tile-rows per strip
    let progress = AtomicUsize::new(0);
    let mut ty_cursor = y0;

    while ty_cursor <= y1 {
        let sy0 = ty_cursor;
        let sy1 = (ty_cursor + strip_rows - 1).min(y1);
        let sny = (sy1 - sy0 + 1) as usize;
        let sh = sny * 256;
        let sul = ty2lat(sy0, job.zoom);
        let slr = ty2lat(sy1 + 1, job.zoom);
        let spy = (sul - slr) / sh as f64;

        // Download strip.
        let results = download_strip(
            &client, &job.url_template, job.zoom,
            x0, x1, sy0, sy1, dec, conns,
            &progress, total,
        ).await;

        // Assemble strip grid.
        let mut grid = vec![0.0f32; gw * sh];
        for (tx, ty, r) in &results {
            if let Ok(elev) = r {
                let col = (*tx - x0) as usize * 256;
                let row = (*ty - sy0) as usize * 256;
                for py in 0..256usize {
                    let dr = row + py;
                    if dr >= sh { break; }
                    let cw = 256.min(gw - col);
                    grid[dr * gw + col..dr * gw + col + cw]
                        .copy_from_slice(&elev[py * 256..py * 256 + cw]);
                }
            }
        }
        drop(results);

        // Sample into .abt rows.
        for (spec, writer) in &mut abt_writers {
            let pd = spec.resolution_m / 111_111.0;
            let sz = spec.size_px as usize;
            for y in 0..sz {
                let lat = spec.ul_lat - (y as f64 + 0.5) * pd;
                if lat > sul || lat < slr { continue; }
                let gr = ((sul - lat) / spy).round() as usize;
                if gr >= sh { continue; }
                let mut row = vec![0i16; sz];
                for x in 0..sz {
                    let lon = spec.ul_lon + (x as f64 + 0.5) * pd;
                    let gc = ((lon - gul_lon) / gpx).round() as usize;
                    if gc < gw {
                        row[x] = (grid[gr * gw + gc] * 2.0).round() as i16;
                    }
                }
                writer.write_row(y as u32, &row)?;
            }
        }

        ty_cursor = sy1 + 1;
    }

    for (_, w) in abt_writers { w.finish()?; }
    eprintln!("[Download] TOTAL: {:.1}s", start.elapsed().as_secs_f64());
    Ok(())
}
