// =============================================================================
// Input Raster Reader — .tif (TIFF / BigTIFF)
// =============================================================================
//
// Reads raster metadata (dimensions, geotransform, nodata, scale/offset) and
// provides region-based pixel access returning raw u8 values (avoiding float
// promotion in RAM).
//
// .tif: Supports tiled layout, DEFLATE compression, and GeoTIFF tags.

use flate2::read::ZlibDecoder;
use memmap2::Mmap;
use std::fs::File;
use std::io::Read;
use std::path::Path;

// ═══════════════════════════════════════════════════════════════════════════════
// Public types
// ═══════════════════════════════════════════════════════════════════════════════

pub struct InputRaster {
    pub width: usize,
    pub height: usize,
    pub geotransform:[f64; 6],
    pub nodata: f32,
    pub scale: f64,
    pub offset: f64,
    inner: Inner,
}

enum Inner {
    Tif(TifFile),
}

impl InputRaster {
    pub fn open(path: &Path) -> Result<Self, Box<dyn std::error::Error>> {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_lowercase();

        match ext.as_str() {
            "tif" | "tiff" => open_tif(path),
            _ => Err(format!("Unsupported format: .{}", ext).into()),
        }
    }

    /// Read a rectangular region in the file's own pixel coordinates as raw 8-bit bytes.
    /// Invalid pixels or nodata = 0.
    pub fn read_region_u8(&self, x: usize, y: usize, w: usize, h: usize) -> Vec<u8> {
        match &self.inner {
            Inner::Tif(t) => t.read_region_u8(x, y, w, h),
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// TIFF / BigTIFF Reader
// ═══════════════════════════════════════════════════════════════════════════════

struct TifFile {
    mmap: Mmap,
    width: usize,
    height: usize,
    tile_width: usize,
    tile_height: usize,
    tiles_x: usize,
    bits_per_sample: u16,
    sample_format: u16,
    compression: u16,
    predictor: u16,
    tile_offsets: Vec<u64>,
    tile_byte_counts: Vec<u64>,
    scale: f64,
    offset: f64,
}

#[derive(Clone, Copy)]
enum TiffVer {
    Classic,
    Big,
}

fn ru16(d: &[u8], o: usize) -> u16 { u16::from_le_bytes([d[o], d[o + 1]]) }
fn ru32(d: &[u8], o: usize) -> u32 { u32::from_le_bytes(d[o..o + 4].try_into().unwrap()) }
fn ru64(d: &[u8], o: usize) -> u64 { u64::from_le_bytes(d[o..o + 8].try_into().unwrap()) }
fn rf64(d: &[u8], o: usize) -> f64 { f64::from_le_bytes(d[o..o + 8].try_into().unwrap()) }

fn type_size(dtype: u16) -> usize {
    match dtype {
        1 | 2 | 6 | 7 => 1,
        3 | 8 => 2,
        4 | 9 | 11 => 4,
        5 | 10 | 12 => 8,
        16 => 8,
        _ => 1,
    }
}

struct IfdEntry {
    tag: u16,
    dtype: u16,
    count: u64,
    data_offset: u64,
}

fn parse_ifd(d: &[u8], off: u64, v: TiffVer) -> (Vec<IfdEntry>, u64) {
    let o = off as usize;
    let (n, start, esz) = match v {
        TiffVer::Classic => (ru16(d, o) as u64, o + 2, 12usize),
        TiffVer::Big => (ru64(d, o), o + 8, 20usize),
    };

    let mut entries = Vec::with_capacity(n as usize);
    for i in 0..n as usize {
        let e = start + i * esz;
        let tag = ru16(d, e);
        let dtype = ru16(d, e + 2);
        let (count, data_offset) = match v {
            TiffVer::Classic => {
                let c = ru32(d, e + 4) as u64;
                let total = c * type_size(dtype) as u64;
                let vo = if total <= 4 { e as u64 + 8 } else { ru32(d, e + 8) as u64 };
                (c, vo)
            }
            TiffVer::Big => {
                let c = ru64(d, e + 4);
                let total = c * type_size(dtype) as u64;
                let vo = if total <= 8 { e as u64 + 12 } else { ru64(d, e + 12) };
                (c, vo)
            }
        };
        entries.push(IfdEntry { tag, dtype, count, data_offset });
    }

    let next_pos = start + n as usize * esz;
    let next_ifd = match v {
        TiffVer::Classic => ru32(d, next_pos) as u64,
        TiffVer::Big => ru64(d, next_pos),
    };
    (entries, next_ifd)
}

fn read_u64_values(d: &[u8], e: &IfdEntry) -> Vec<u64> {
    let o = e.data_offset as usize;
    match e.dtype {
        3 => (0..e.count as usize).map(|i| ru16(d, o + i * 2) as u64).collect(),
        4 => (0..e.count as usize).map(|i| ru32(d, o + i * 4) as u64).collect(),
        16 => (0..e.count as usize).map(|i| ru64(d, o + i * 8)).collect(),
        _ => vec![],
    }
}

fn read_f64_values(d: &[u8], e: &IfdEntry) -> Vec<f64> {
    let o = e.data_offset as usize;
    (0..e.count as usize).map(|i| rf64(d, o + i * 8)).collect()
}

fn read_string(d: &[u8], e: &IfdEntry) -> String {
    let o = e.data_offset as usize;
    let end = (o + e.count as usize).min(d.len());
    let bytes = &d[o..end];
    let s = if bytes.last() == Some(&0) { &bytes[..bytes.len() - 1] } else { bytes };
    String::from_utf8_lossy(s).to_string()
}

fn read_u16_val(d: &[u8], e: &IfdEntry) -> u16 { ru16(d, e.data_offset as usize) }

fn read_u32_val(d: &[u8], e: &IfdEntry) -> u32 {
    match e.dtype {
        3 => ru16(d, e.data_offset as usize) as u32,
        4 => ru32(d, e.data_offset as usize),
        16 => ru64(d, e.data_offset as usize) as u32,
        _ => 0,
    }
}

const T_SUBFILE: u16 = 254;
const T_WIDTH: u16 = 256;
const T_HEIGHT: u16 = 257;
const T_BPS: u16 = 258;
const T_COMPRESS: u16 = 259;
const T_PREDICT: u16 = 317;
const T_TILEW: u16 = 322;
const T_TILEH: u16 = 323;
const T_TILEOFF: u16 = 324;
const T_TILESZ: u16 = 325;
const T_SFORMAT: u16 = 339;
const T_PSCALE: u16 = 33550;
const T_TIEPOINT: u16 = 33922;
const T_GDALMD: u16 = 42112;
const T_GDALND: u16 = 42113;

fn open_tif(path: &Path) -> Result<InputRaster, Box<dyn std::error::Error>> {
    let file = File::open(path)?;
    let mmap = unsafe { Mmap::map(&file)? };
    let d = &mmap[..];

    if d.len() < 8 { return Err("File too small for TIFF header".into()); }
    if ru16(d, 0) != 0x4949 { return Err("Only little-endian TIFF supported".into()); }

    let (ver, first_ifd) = match ru16(d, 2) {
        42 => (TiffVer::Classic, ru32(d, 4) as u64),
        43 => {
            if ru16(d, 4) != 8 { return Err("Invalid BigTIFF offset size".into()); }
            (TiffVer::Big, ru64(d, 8))
        }
        v => return Err(format!("Unknown TIFF version: {}", v).into()),
    };

    let mut ifd_off = first_ifd;
    let entries;
    loop {
        let (ents, next) = parse_ifd(d, ifd_off, ver);
        let is_overview = ents.iter().any(|e| e.tag == T_SUBFILE && read_u32_val(d, e) & 1 != 0);
        if !is_overview {
            entries = ents;
            break;
        }
        if next == 0 { return Err("No main IFD found".into()); }
        ifd_off = next;
    }

    let mut width = 0u32;
    let mut height = 0u32;
    let mut tile_w = 0u32;
    let mut tile_h = 0u32;
    let mut bps = 8u16;
    let mut sf = 1u16;
    let mut comp = 1u16;
    let mut pred = 1u16;
    let mut toff: Vec<u64> = Vec::new();
    let mut tsz: Vec<u64> = Vec::new();
    let mut pscale: Option<Vec<f64>> = None;
    let mut tiepoint: Option<Vec<f64>> = None;
    let mut gdalnd: Option<String> = None;
    let mut gdalmd: Option<String> = None;

    for e in &entries {
        match e.tag {
            T_WIDTH => width = read_u32_val(d, e),
            T_HEIGHT => height = read_u32_val(d, e),
            T_TILEW => tile_w = read_u32_val(d, e),
            T_TILEH => tile_h = read_u32_val(d, e),
            T_BPS => bps = read_u16_val(d, e),
            T_SFORMAT => sf = read_u16_val(d, e),
            T_COMPRESS => comp = read_u16_val(d, e),
            T_PREDICT => pred = read_u16_val(d, e),
            T_TILEOFF => toff = read_u64_values(d, e),
            T_TILESZ => tsz = read_u64_values(d, e),
            T_PSCALE => pscale = Some(read_f64_values(d, e)),
            T_TIEPOINT => tiepoint = Some(read_f64_values(d, e)),
            T_GDALND => gdalnd = Some(read_string(d, e)),
            T_GDALMD => gdalmd = Some(read_string(d, e)),
            _ => {}
        }
    }

    if tile_w == 0 || tile_h == 0 {
        return Err("Not a tiled TIFF".into());
    }

    let gt = match (pscale.as_deref(), tiepoint.as_deref()) {
        (Some(ps), Some(tp)) if ps.len() >= 2 && tp.len() >= 6 => {
            [tp[3], ps[0], 0.0, tp[4], 0.0, -ps[1]]
        }
        _ =>[0.0, 1.0, 0.0, 0.0, 0.0, -1.0],
    };

    let nodata = gdalnd
        .as_deref()
        .and_then(|s| s.trim().parse::<f64>().ok())
        .unwrap_or(-9999.0) as f32;

    let (scale, offset) = parse_gdal_scale_offset(gdalmd.as_deref());
    let tiles_x = (width as usize + tile_w as usize - 1) / tile_w as usize;

    Ok(InputRaster {
        width: width as usize,
        height: height as usize,
        geotransform: gt,
        nodata,
        scale,
        offset,
        inner: Inner::Tif(TifFile {
            mmap,
            width: width as usize,
            height: height as usize,
            tile_width: tile_w as usize,
            tile_height: tile_h as usize,
            tiles_x,
            bits_per_sample: bps,
            sample_format: sf,
            compression: comp,
            predictor: pred,
            tile_offsets: toff,
            tile_byte_counts: tsz,
            scale,
            offset,
        }),
    })
}

impl TifFile {
    fn read_region_u8(
        &self,
        x: usize,
        y: usize,
        w: usize,
        h: usize,
    ) -> Vec<u8> {
        let mut buf = vec![0u8; w * h];
        let tw = self.tile_width;
        let th = self.tile_height;

        let tx0 = x / tw;
        let ty0 = y / th;
        let tx1 = ((x + w).min(self.width) + tw - 1) / tw;
        let ty1 = ((y + h).min(self.height) + th - 1) / th;

        for ty in ty0..ty1 {
            for tx in tx0..tx1 {
                let idx = ty * self.tiles_x + tx;
                if idx >= self.tile_offsets.len() { continue; }
                let size = self.tile_byte_counts[idx];
                if size == 0 { continue; }

                let tile_data = self.decompress_tile(idx);
                if tile_data.is_empty() { continue; }

                let tile_x0 = tx * tw;
                let tile_y0 = ty * th;
                let ry0 = y.max(tile_y0);
                let ry1 = (y + h).min(tile_y0 + th).min(self.height);
                let rx0 = x.max(tile_x0);
                let rx1 = (x + w).min(tile_x0 + tw).min(self.width);

                for sy in ry0..ry1 {
                    let tile_row = sy - tile_y0;
                    let dst_row = sy - y;
                    for sx in rx0..rx1 {
                        let tile_col = sx - tile_x0;
                        let dst_col = sx - x;
                        buf[dst_row * w + dst_col] = self.extract_pixel_u8(&tile_data, tile_row, tile_col);
                    }
                }
            }
        }
        buf
    }

    fn decompress_tile(&self, idx: usize) -> Vec<u8> {
        let off = self.tile_offsets[idx] as usize;
        let sz = self.tile_byte_counts[idx] as usize;
        if off + sz > self.mmap.len() { return Vec::new(); }
        let compressed = &self.mmap[off..off + sz];

        let mut raw = match self.compression {
            1 => compressed.to_vec(),
            8 | 32946 => {
                let pixels = self.tile_width * self.tile_height;
                let expected = match self.bits_per_sample {
                    1 => (self.tile_width + 7) / 8 * self.tile_height,
                    8 => pixels,
                    16 => pixels * 2,
                    32 => pixels * 4,
                    _ => pixels,
                };
                let mut buf = Vec::with_capacity(expected);
                let _ = ZlibDecoder::new(compressed).read_to_end(&mut buf);
                buf
            }
            _ => Vec::new(),
        };

        if !raw.is_empty() {
            let bps_bytes = (self.bits_per_sample as usize).max(8) / 8;
            if self.predictor == 2 {
                undo_predictor_2(&mut raw, self.tile_width, self.tile_height, bps_bytes);
            } else if self.predictor == 3 {
                undo_predictor_3(&mut raw, self.tile_width, self.tile_height, bps_bytes);
            }
        }
        raw
    }

    fn extract_pixel_u8(&self, tile_data: &[u8], row: usize, col: usize) -> u8 {
        match self.bits_per_sample {
            1 => {
                let rb = (self.tile_width + 7) / 8;
                let idx = row * rb + col / 8;
                if idx >= tile_data.len() { return 0; }
                let bit = (tile_data[idx] >> (7 - (col & 7))) & 1;
                if bit == 0 { 0 } else { 1 }
            }
            8 => {
                let idx = row * self.tile_width + col;
                if idx >= tile_data.len() { return 0; }
                tile_data[idx]
            }
            16 => {
                let idx = (row * self.tile_width + col) * 2;
                if idx + 2 > tile_data.len() { return 0; }
                let raw = i16::from_le_bytes([tile_data[idx], tile_data[idx + 1]]);
                if raw <= 0 { 0 } else { raw.min(255) as u8 }
            }
            32 => {
                let idx = (row * self.tile_width + col) * 4;
                if idx + 4 > tile_data.len() { return 0; }
                let val = if self.predictor == 3 {
                    f32::from_be_bytes(tile_data[idx..idx + 4].try_into().unwrap())
                } else {
                    f32::from_le_bytes(tile_data[idx..idx + 4].try_into().unwrap())
                };
                if val <= -5000.0 { 0 } else { ((val - self.offset as f32) / self.scale as f32).clamp(0.0, 255.0) as u8 }
            }
            _ => 0,
        }
    }
}

fn undo_predictor_2(data: &mut [u8], width: usize, height: usize, bps: usize) {
    let row_bytes = width * bps;
    for y in 0..height {
        let s = y * row_bytes;
        if s + row_bytes > data.len() { break; }
        if bps == 1 {
            for i in 1..row_bytes { data[s + i] = data[s + i].wrapping_add(data[s + i - 1]); }
        } else if bps == 2 {
            for x in 1..width {
                let px = s + (x - 1) * 2;
                let cx = s + x * 2;
                let prev = u16::from_le_bytes([data[px], data[px + 1]]);
                let curr = u16::from_le_bytes([data[cx], data[cx + 1]]);
                let sum = curr.wrapping_add(prev);
                data[cx..cx + 2].copy_from_slice(&sum.to_le_bytes());
            }
        } else if bps == 4 {
            for x in 1..width {
                let px = s + (x - 1) * 4;
                let cx = s + x * 4;
                let prev = u32::from_le_bytes([data[px], data[px + 1], data[px + 2], data[px + 3]]);
                let curr = u32::from_le_bytes([data[cx], data[cx + 1], data[cx + 2], data[cx + 3]]);
                let sum = curr.wrapping_add(prev);
                data[cx..cx + 4].copy_from_slice(&sum.to_le_bytes());
            }
        } else if bps == 8 {
            for x in 1..width {
                let px = s + (x - 1) * 8;
                let cx = s + x * 8;
                let mut prev_buf =[0u8; 8];
                let mut curr_buf =[0u8; 8];
                prev_buf.copy_from_slice(&data[px..px + 8]);
                curr_buf.copy_from_slice(&data[cx..cx + 8]);
                let prev = u64::from_le_bytes(prev_buf);
                let curr = u64::from_le_bytes(curr_buf);
                let sum = curr.wrapping_add(prev);
                data[cx..cx + 8].copy_from_slice(&sum.to_le_bytes());
            }
        }
    }
}

fn undo_predictor_3(data: &mut [u8], width: usize, height: usize, bps: usize) {
    if bps == 0 { return; }
    let row_bytes = width * bps;
    let mut tmp = vec![0u8; row_bytes];

    for y in 0..height {
        let s = y * row_bytes;
        if s + row_bytes > data.len() { break; }

        for i in 1..row_bytes {
            data[s + i] = data[s + i].wrapping_add(data[s + i - 1]);
        }
        for b in 0..bps {
            for x in 0..width {
                tmp[x * bps + b] = data[s + b * width + x];
            }
        }
        data[s..s + row_bytes].copy_from_slice(&tmp);
    }
}

fn parse_gdal_scale_offset(xml: Option<&str>) -> (f64, f64) {
    let Some(xml) = xml else { return (1.0, 0.0); };
    let scale = extract_xml_item(xml, "SCALE").unwrap_or(1.0);
    let offset = extract_xml_item(xml, "OFFSET").unwrap_or(0.0);
    (scale, offset)
}

fn extract_xml_item(xml: &str, name: &str) -> Option<f64> {
    let pattern = format!("name=\"{}\"", name);
    let pos = xml.find(&pattern)?;
    let gt = xml[pos..].find('>')?;
    let after = &xml[pos + gt + 1..];
    let lt = after.find('<')?;
    after[..lt].trim().parse().ok()
}