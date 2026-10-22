//! Shared test helpers: a hand-written GeoTIFF writer (strips of any height,
//! tiles with ragged edges, Deflate/ZSTD/any compression code,
//! ModelTransformation, GDAL_METADATA) and the `.abt` writer, plus small
//! runners. The `tiff` crate's encoder cannot write the Geo tags.
#![allow(dead_code)]

use std::io::Write;
use std::path::Path;


#[derive(Clone)]
pub enum Px {
    I16(Vec<i16>),
    U16(Vec<u16>),
    F32(Vec<f32>),
}

impl Px {
    fn bits_and_format(&self) -> (u16, u16) {
        match self {
            Px::I16(_) => (16, 2),
            Px::U16(_) => (16, 1),
            Px::F32(_) => (32, 3),
        }
    }
    fn len(&self) -> usize {
        match self {
            Px::I16(v) => v.len(),
            Px::U16(v) => v.len(),
            Px::F32(v) => v.len(),
        }
    }
    /// Little-endian bytes of sample `i`, or zeros for padding (`None`).
    fn put(&self, i: Option<usize>, out: &mut Vec<u8>) {
        match (self, i) {
            (Px::I16(v), Some(i)) => out.extend_from_slice(&v[i].to_le_bytes()),
            (Px::U16(v), Some(i)) => out.extend_from_slice(&v[i].to_le_bytes()),
            (Px::F32(v), Some(i)) => out.extend_from_slice(&v[i].to_le_bytes()),
            (Px::F32(_), None) => out.extend_from_slice(&[0; 4]),
            (_, None) => out.extend_from_slice(&[0; 2]),
        }
    }
}

#[derive(Clone, Copy)]
pub enum Layout {
    Strips(u32),
    Tiles(u32, u32),
}

#[derive(Clone, Copy)]
pub enum Geo {
    /// ModelTiepoint + ModelPixelScale: (origin_e, origin_n, scale_x, scale_y).
    TieScale(f64, f64, f64, f64),
    /// ModelTransformation, all 16 terms.
    Matrix([f64; 16]),
}

pub struct Tif<'a> {
    pub w: u32,
    pub h: u32,
    pub px: Px,
    pub layout: Layout,
    /// TIFF Compression code. 1, 8 (Deflate) and 50000 (ZSTD) are really
    /// encoded; any other code writes raw bytes under that label.
    pub compression: u16,
    pub geo: Geo,
    pub geokeys: &'a [(u16, u16)],
    pub nodata: Option<&'a str>,
    pub gdal_metadata: Option<&'a str>,
}

pub fn compress(code: u16, raw: Vec<u8>) -> Vec<u8> {
    match code {
        8 => {
            let mut e = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
            e.write_all(&raw).unwrap();
            e.finish().unwrap()
        }
        50000 => zstd::encode_all(&raw[..], 3).unwrap(),
        _ => raw,
    }
}

/// One IFD entry: tag, type, count, value bytes (inline when <= 4 bytes).
type Entry = (u16, u16, u32, Vec<u8>);

const T_ASCII: u16 = 2;
const T_SHORT: u16 = 3;
const T_LONG: u16 = 4;
const T_DOUBLE: u16 = 12;

fn shorts(v: &[u16]) -> Vec<u8> {
    v.iter().flat_map(|x| x.to_le_bytes()).collect()
}
fn longs(v: &[u32]) -> Vec<u8> {
    v.iter().flat_map(|x| x.to_le_bytes()).collect()
}
fn doubles(v: &[f64]) -> Vec<u8> {
    v.iter().flat_map(|x| x.to_le_bytes()).collect()
}
fn ascii(s: &str) -> Vec<u8> {
    let mut b = s.as_bytes().to_vec();
    b.push(0);
    b
}

/// Assemble a little-endian TIFF: header, the chunk blobs, then the IFD and
/// its out-of-line values. `offsets`/`counts` are filled in per chunk.
pub fn assemble(path: &Path, mut entries: Vec<Entry>, blobs: &[Vec<u8>], chunk_of: &[usize], tiled: bool) {
    let mut buf: Vec<u8> = b"II\x2a\x00\0\0\0\0".to_vec();
    let mut blob_off = Vec::new();
    for b in blobs {
        blob_off.push(buf.len() as u32);
        buf.extend_from_slice(b);
        if buf.len() % 2 == 1 {
            buf.push(0);
        }
    }
    let offsets: Vec<u32> = chunk_of.iter().map(|&i| blob_off[i]).collect();
    let counts: Vec<u32> = chunk_of.iter().map(|&i| blobs[i].len() as u32).collect();
    let (off_tag, cnt_tag) = if tiled { (324, 325) } else { (273, 279) };
    entries.push((off_tag, T_LONG, offsets.len() as u32, longs(&offsets)));
    entries.push((cnt_tag, T_LONG, counts.len() as u32, longs(&counts)));
    entries.sort_by_key(|e| e.0);

    let ifd_off = buf.len();
    buf[4..8].copy_from_slice(&(ifd_off as u32).to_le_bytes());
    let mut ext = ifd_off + 2 + entries.len() * 12 + 4;
    let mut ext_bytes: Vec<u8> = Vec::new();
    buf.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    for (tag, typ, count, val) in &entries {
        buf.extend_from_slice(&tag.to_le_bytes());
        buf.extend_from_slice(&typ.to_le_bytes());
        buf.extend_from_slice(&count.to_le_bytes());
        if val.len() <= 4 {
            let mut v = val.clone();
            v.resize(4, 0);
            buf.extend_from_slice(&v);
        } else {
            buf.extend_from_slice(&(ext as u32).to_le_bytes());
            ext_bytes.extend_from_slice(val);
            ext += val.len();
            if ext % 2 == 1 {
                ext_bytes.push(0);
                ext += 1;
            }
        }
    }
    buf.extend_from_slice(&0u32.to_le_bytes());
    buf.extend_from_slice(&ext_bytes);
    std::fs::write(path, buf).unwrap();
}

fn base_entries(t: &Tif, bits: u16, fmt: u16) -> Vec<Entry> {
    let mut e: Vec<Entry> = vec![
        (256, T_LONG, 1, longs(&[t.w])),
        (257, T_LONG, 1, longs(&[t.h])),
        (258, T_SHORT, 1, shorts(&[bits])),
        (259, T_SHORT, 1, shorts(&[t.compression])),
        (262, T_SHORT, 1, shorts(&[1])),
        (277, T_SHORT, 1, shorts(&[1])),
        (339, T_SHORT, 1, shorts(&[fmt])),
    ];
    match t.geo {
        Geo::TieScale(oe, on, sx, sy) => {
            e.push((33550, T_DOUBLE, 3, doubles(&[sx, sy, 0.0])));
            e.push((33922, T_DOUBLE, 6, doubles(&[0.0, 0.0, 0.0, oe, on, 0.0])));
        }
        Geo::Matrix(m) => e.push((34264, T_DOUBLE, 16, doubles(&m))),
    }
    if !t.geokeys.is_empty() {
        let mut dir = vec![1u16, 1, 0, t.geokeys.len() as u16];
        for &(k, v) in t.geokeys {
            dir.extend_from_slice(&[k, 0, 1, v]);
        }
        e.push((34735, T_SHORT, dir.len() as u32, shorts(&dir)));
    }
    if let Some(md) = t.gdal_metadata {
        let b = ascii(md);
        e.push((42112, T_ASCII, b.len() as u32, b));
    }
    if let Some(nd) = t.nodata {
        let b = ascii(nd);
        e.push((42113, T_ASCII, b.len() as u32, b));
    }
    e
}

pub fn write_tif(path: &Path, t: &Tif) {
    assert_eq!(t.px.len(), (t.w * t.h) as usize);
    let (bits, fmt) = t.px.bits_and_format();
    let mut entries = base_entries(t, bits, fmt);
    let mut blobs = Vec::new();
    let tiled = match t.layout {
        Layout::Strips(rps) => {
            entries.push((278, T_LONG, 1, longs(&[rps])));
            let mut y0 = 0;
            while y0 < t.h {
                let mut raw = Vec::new();
                for y in y0..(y0 + rps).min(t.h) {
                    for x in 0..t.w {
                        t.px.put(Some((y * t.w + x) as usize), &mut raw);
                    }
                }
                blobs.push(compress(t.compression, raw));
                y0 += rps;
            }
            false
        }
        Layout::Tiles(tw, th) => {
            entries.push((322, T_LONG, 1, longs(&[tw])));
            entries.push((323, T_LONG, 1, longs(&[th])));
            for ty in 0..t.h.div_ceil(th) {
                for tx in 0..t.w.div_ceil(tw) {
                    let mut raw = Vec::new();
                    for y in ty * th..(ty + 1) * th {
                        for x in tx * tw..(tx + 1) * tw {
                            let i = (x < t.w && y < t.h).then(|| (y * t.w + x) as usize);
                            t.px.put(i, &mut raw);
                        }
                    }
                    blobs.push(compress(t.compression, raw));
                }
            }
            true
        }
    };
    let chunk_of: Vec<usize> = (0..blobs.len()).collect();
    assemble(path, entries, &blobs, &chunk_of, tiled);
}

/// Hand-crafted `.abt` (contract §6): 44-byte header + 256-aligned rows.
pub fn write_abt(path: &Path, size: u16, ul_lat: f64, ul_lon: f64, pixel_deg: f64, data: &[i16]) {
    let row_bytes = size as usize * 2;
    let stride = (row_bytes + 255) & !255;
    let mut buf: Vec<u8> = b"AETH".to_vec();
    buf.extend_from_slice(&1u16.to_le_bytes());
    buf.extend_from_slice(&size.to_le_bytes());
    buf.extend_from_slice(&ul_lat.to_le_bytes());
    buf.extend_from_slice(&ul_lon.to_le_bytes());
    buf.extend_from_slice(&pixel_deg.to_le_bytes());
    buf.extend_from_slice(&pixel_deg.to_le_bytes());
    buf.extend_from_slice(&0i16.to_le_bytes());
    buf.extend_from_slice(&(stride as u16).to_le_bytes());
    for row in data.chunks(size as usize) {
        for v in row {
            buf.extend_from_slice(&v.to_le_bytes());
        }
        buf.resize(buf.len() + (stride - row_bytes), 0);
    }
    std::fs::write(path, buf).unwrap();
}

/// The payload as one row-major vector, past the 44-byte header and padding.
pub fn payload(bytes: &[u8], size: usize) -> Vec<i16> {
    let stride = (size * 2 + 255) & !255;
    let mut out = Vec::with_capacity(size * size);
    for y in 0..size {
        for x in 0..size {
            let o = 44 + y * stride + x * 2;
            out.push(i16::from_le_bytes([bytes[o], bytes[o + 1]]));
        }
    }
    out
}

/// Run the real CLI over *json*; `(exit ok, stdout + stderr)`.
pub fn run_cli(dir: &Path, json: serde_json::Value) -> (bool, String) {
    let job_file = dir.join("job.json");
    std::fs::write(&job_file, serde_json::to_vec(&json).unwrap()).unwrap();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_aether_converter"))
        .args(["ingest", "--job-file"])
        .arg(&job_file)
        .output()
        .unwrap();
    let said = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    (out.status.success(), said)
}

// ── A loopback XYZ tile source (download tests) ────────────────────────────

/// Requests seen per `(z, x, y)`.
pub type TileCounts = std::sync::Arc<std::sync::Mutex<std::collections::HashMap<(u32, u32, u32), usize>>>;

/// Loopback tile server: `route(z, x, y)` returns the PNG to serve, or `None`
/// for a 404; every request is counted. One request per connection
/// (`Connection: close`), like the other download tests' servers.
pub fn serve_tiles<F>(route: F) -> (u16, TileCounts)
where
    F: Fn(u32, u32, u32) -> Option<Vec<u8>> + Send + Sync + 'static,
{
    serve_tiles_delayed(move |z, x, y| route(z, x, y).map(|png| (png, 0)))
}

/// [`serve_tiles`] whose route also says how long to wait (ms) before the
/// response head goes out.
pub fn serve_tiles_delayed<F>(route: F) -> (u16, TileCounts)
where
    F: Fn(u32, u32, u32) -> Option<(Vec<u8>, u64)> + Send + Sync + 'static,
{
    use std::io::{BufRead, BufReader};
    let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let route = std::sync::Arc::new(route);
    let counts: TileCounts = Default::default();
    let c2 = counts.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let (route, counts) = (route.clone(), c2.clone());
            std::thread::spawn(move || {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut request = String::new();
                if reader.read_line(&mut request).unwrap_or(0) == 0 { return; }
                let mut line = String::new();
                while reader.read_line(&mut line).unwrap_or(0) > 0 && line != "\r\n" { line.clear(); }
                let path = request.split_whitespace().nth(1).unwrap_or("/").to_owned();
                // A query (`?key=…`) is not part of the tile's address.
                let path = path.split('?').next().unwrap_or("/");
                let zxy: Vec<u32> = path.trim_start_matches('/').trim_end_matches(".png")
                    .split('/').filter_map(|p| p.parse().ok()).collect();
                if zxy.len() != 3 { return; }
                *counts.lock().unwrap().entry((zxy[0], zxy[1], zxy[2])).or_insert(0) += 1;
                let (status, body) = match route(zxy[0], zxy[1], zxy[2]) {
                    Some((png, delay_ms)) => {
                        std::thread::sleep(std::time::Duration::from_millis(delay_ms));
                        ("200 OK", png)
                    }
                    None => ("404 Not Found", Vec::new()),
                };
                let _ = write!(stream, "HTTP/1.1 {status}\r\nContent-Length: {}\r\n\
                                        Connection: close\r\n\r\n", body.len());
                let _ = stream.write_all(&body);
            });
        }
    });
    (port, counts)
}

/// A 256 px Terrarium PNG whose sample `(px, py)` is `f(px, py)` metres
/// (integers, so the encoding is exact).
pub fn terrarium_png(f: impl Fn(usize, usize) -> i32) -> Vec<u8> {
    let mut px = Vec::with_capacity(256 * 256 * 3);
    for py in 0..256 {
        for x in 0..256 {
            let t = (f(x, py) + 32768) as u32;
            px.extend_from_slice(&[(t >> 8) as u8, (t & 255) as u8, 0]);
        }
    }
    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, 256, 256);
        enc.set_color(png::ColorType::Rgb);
        enc.set_depth(png::BitDepth::Eight);
        enc.write_header().unwrap().write_image_data(&px).unwrap();
    }
    out
}

/// Terrain for tile `(z, x, y)` that differs per tile and per sample, so a
/// misplaced tile or sample changes the output bytes.
pub fn tile_ground(z: u32, x: u32, y: u32) -> impl Fn(usize, usize) -> i32 {
    let base = 200 + (z as i32) * 300 + ((x % 9) as i32) * 37 + ((y % 11) as i32) * 23;
    move |px, py| base + (px as i32 * 3) % 97 + (py as i32 * 5) % 89
}

/// Every `.abt` in `dir`, by name, with its bytes.
pub fn read_dir_bytes(dir: &Path) -> Vec<(String, Vec<u8>)> {
    let mut v: Vec<_> = std::fs::read_dir(dir).unwrap().filter_map(|e| e.ok())
        .map(|e| (e.file_name().to_string_lossy().into_owned(), std::fs::read(e.path()).unwrap()))
        .collect();
    v.sort();
    v
}

/// Run the real CLI `download` over *job_file*; `(exit ok, stderr)`.
pub fn run_download_cli(job_file: &Path) -> (bool, String) {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_aether_converter"))
        .args(["download", "--job-file"]).arg(job_file).output().unwrap();
    (out.status.success(), String::from_utf8_lossy(&out.stderr).into_owned())
}
