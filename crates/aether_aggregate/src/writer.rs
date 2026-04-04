// =============================================================================
// BigTIFF Writer — Fast 8-Bit (Max) and 16-Bit (Count) Exporter
// =============================================================================

use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;
use flate2::Compression;
use rayon::prelude::*;
use std::fs::File;
use std::io::{self, BufWriter, Read, Seek, SeekFrom, Write};

// ═══════════════════════════════════════════════════════════════════════════════
// TileStore — 8-Bit and 16-Bit implementations
// ═══════════════════════════════════════════════════════════════════════════════

pub struct TileStoreU8 {
    pub width: usize,
    pub height: usize,
    pub tile_size: usize,
    pub tiles_x: usize,
    pub tiles_y: usize,
    pub nodata: u8,
    compress_level: u32,
    data: Vec<Option<Vec<u8>>>,
}

impl TileStoreU8 {
    pub fn new(width: usize, height: usize, tile_size: usize, nodata: u8, compress_level: u32) -> Self {
        let tiles_x = (width + tile_size - 1) / tile_size;
        let tiles_y = (height + tile_size - 1) / tile_size;
        Self { width, height, tile_size, tiles_x, tiles_y, nodata, compress_level, data: vec![None; tiles_x * tiles_y] }
    }

    pub fn store(&mut self, tx: usize, ty: usize, compressed: Vec<u8>) {
        if let Some(entry) = self.data.get_mut(ty * self.tiles_x + tx) { *entry = Some(compressed); }
    }

    pub fn get_compressed(&self, tx: usize, ty: usize) -> Option<&Vec<u8>> {
        self.data.get(ty * self.tiles_x + tx).and_then(|d| d.as_ref())
    }

    fn decompress_tile(&self, tx: usize, ty: usize) -> Option<Vec<u8>> {
        let compressed = self.get_compressed(tx, ty)?;
        let mut raw = if self.compress_level > 0 {
            let mut buf = Vec::with_capacity(self.tile_size * self.tile_size);
            let _ = ZlibDecoder::new(compressed.as_slice()).read_to_end(&mut buf);
            buf
        } else { compressed.clone() };

        if self.compress_level > 0 {
            let row_bytes = self.tile_size;
            for y in 0..self.tile_size {
                let s = y * row_bytes;
                if s + row_bytes > raw.len() { break; }
                for i in 1..row_bytes { raw[s + i] = raw[s + i].wrapping_add(raw[s + i - 1]); }
            }
        }
        Some(raw)
    }

    pub fn build_overview_4x(&self) -> TileStoreU8 {
        let ts = self.tile_size;
        let ovr_w = (self.width + 3) / 4;
        let ovr_h = (self.height + 3) / 4;
        let mut ovr = TileStoreU8::new(ovr_w, ovr_h, ts, self.nodata, self.compress_level);

        ovr.data = (0..ovr.tiles_x * ovr.tiles_y).into_par_iter().map(|idx| {
            let ovr_tile = self.downsample_tile(idx % ovr.tiles_x, idx / ovr.tiles_x, ovr_w, ovr_h);
            if ovr_tile.iter().all(|&v| v == self.nodata) { None }
            else { Some(compress_u8_tile(&ovr_tile, ts, ts, self.compress_level)) }
        }).collect();
        ovr
    }

    fn downsample_tile(&self, otx: usize, oty: usize, ovr_w: usize, ovr_h: usize) -> Vec<u8> {
        let ts = self.tile_size;
        let mut out = vec![self.nodata; ts * ts];
        let (obw, obh) = (ts.min(ovr_w - otx * ts), ts.min(ovr_h - oty * ts));

        let mut src_tiles = vec![None; 16];
        for dty in 0..4 {
            for dtx in 0..4 {
                if otx * 4 + dtx < self.tiles_x && oty * 4 + dty < self.tiles_y {
                    src_tiles[dty * 4 + dtx] = self.decompress_tile(otx * 4 + dtx, oty * 4 + dty);
                }
            }
        }
        for dy in 0..obh {
            for dx in 0..obw {
                let sx = (otx * ts + dx) * 4; let sy = (oty * ts + dy) * 4;
                if sx >= self.width || sy >= self.height { continue; }
                if let Some(ref tile) = src_tiles[(sy / ts - oty * 4) * 4 + (sx / ts - otx * 4)] {
                    let val = tile[(sy % ts) * ts + (sx % ts)];
                    if val != self.nodata { out[dy * ts + dx] = val; }
                }
            }
        }
        out
    }
}

pub struct TileStoreU16 {
    pub width: usize,
    pub height: usize,
    pub tile_size: usize,
    pub tiles_x: usize,
    pub tiles_y: usize,
    pub nodata: u16,
    compress_level: u32,
    data: Vec<Option<Vec<u8>>>,
}

impl TileStoreU16 {
    pub fn new(width: usize, height: usize, tile_size: usize, nodata: u16, compress_level: u32) -> Self {
        let tiles_x = (width + tile_size - 1) / tile_size;
        let tiles_y = (height + tile_size - 1) / tile_size;
        Self { width, height, tile_size, tiles_x, tiles_y, nodata, compress_level, data: vec![None; tiles_x * tiles_y] }
    }

    pub fn store(&mut self, tx: usize, ty: usize, compressed: Vec<u8>) {
        if let Some(entry) = self.data.get_mut(ty * self.tiles_x + tx) { *entry = Some(compressed); }
    }

    pub fn get_compressed(&self, tx: usize, ty: usize) -> Option<&Vec<u8>> {
        self.data.get(ty * self.tiles_x + tx).and_then(|d| d.as_ref())
    }

    fn decompress_tile(&self, tx: usize, ty: usize) -> Option<Vec<u16>> {
        let compressed = self.get_compressed(tx, ty)?;
        let ts = self.tile_size;
        let mut raw = if self.compress_level > 0 {
            let mut buf = Vec::with_capacity(ts * ts * 2);
            let _ = ZlibDecoder::new(compressed.as_slice()).read_to_end(&mut buf);
            buf
        } else { compressed.clone() };

        if self.compress_level > 0 {
            let row_bytes = ts * 2;
            for y in 0..ts {
                let s = y * row_bytes;
                if s + row_bytes > raw.len() { break; }
                for x in 1..ts {
                    let px = s + (x - 1) * 2;
                    let cx = s + x * 2;
                    let prev = u16::from_le_bytes([raw[px], raw[px + 1]]);
                    let curr = u16::from_le_bytes([raw[cx], raw[cx + 1]]);
                    let sum = curr.wrapping_add(prev);
                    raw[cx..cx + 2].copy_from_slice(&sum.to_le_bytes());
                }
            }
        }
        let mut pixels = vec![self.nodata; ts * ts];
        for i in 0..ts * ts {
            if i * 2 + 2 <= raw.len() { pixels[i] = u16::from_le_bytes([raw[i * 2], raw[i * 2 + 1]]); }
        }
        Some(pixels)
    }

    pub fn build_overview_4x(&self) -> TileStoreU16 {
        let ts = self.tile_size;
        let ovr_w = (self.width + 3) / 4;
        let ovr_h = (self.height + 3) / 4;
        let mut ovr = TileStoreU16::new(ovr_w, ovr_h, ts, self.nodata, self.compress_level);

        ovr.data = (0..ovr.tiles_x * ovr.tiles_y).into_par_iter().map(|idx| {
            let ovr_tile = self.downsample_tile(idx % ovr.tiles_x, idx / ovr.tiles_x, ovr_w, ovr_h);
            if ovr_tile.iter().all(|&v| v == self.nodata) { None }
            else { Some(compress_u16_tile(&ovr_tile, ts, ts, self.compress_level)) }
        }).collect();
        ovr
    }

    fn downsample_tile(&self, otx: usize, oty: usize, ovr_w: usize, ovr_h: usize) -> Vec<u16> {
        let ts = self.tile_size;
        let mut out = vec![self.nodata; ts * ts];
        let (obw, obh) = (ts.min(ovr_w - otx * ts), ts.min(ovr_h - oty * ts));

        let mut src_tiles = vec![None; 16];
        for dty in 0..4 {
            for dtx in 0..4 {
                if otx * 4 + dtx < self.tiles_x && oty * 4 + dty < self.tiles_y {
                    src_tiles[dty * 4 + dtx] = self.decompress_tile(otx * 4 + dtx, oty * 4 + dty);
                }
            }
        }
        for dy in 0..obh {
            for dx in 0..obw {
                let sx = (otx * ts + dx) * 4; let sy = (oty * ts + dy) * 4;
                if sx >= self.width || sy >= self.height { continue; }
                if let Some(ref tile) = src_tiles[(sy / ts - oty * 4) * 4 + (sx / ts - otx * 4)] {
                    let val = tile[(sy % ts) * ts + (sx % ts)];
                    if val != self.nodata { out[dy * ts + dx] = val; }
                }
            }
        }
        out
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// BigTiffWriter — writes main tiles + overview levels
// ═══════════════════════════════════════════════════════════════════════════════

pub struct LevelInfo {
    pub width: usize,
    pub height: usize,
    pub tiles_x: usize,
    pub tiles_y: usize,
    pub tile_info: Vec<(u64, u64)>,
}

pub struct BigTiffWriter {
    w: BufWriter<File>,
    tile_size: usize,
    width: usize,
    height: usize,
    geotransform:[f64; 6],
    nodata: u16,
    bits_per_sample: u16,
    compress_level: u32,
    header_fixup: u64,
    pub main_level: LevelInfo,
    pub overview_levels: Vec<LevelInfo>,
}

impl BigTiffWriter {
    pub fn create(
        path: &str, width: usize, height: usize, tile_size: usize,
        geotransform: &[f64; 6], nodata: u16, compress_level: u32, bits_per_sample: u16,
    ) -> io::Result<Self> {
        let file = File::create(path)?;
        let mut w = BufWriter::with_capacity(32 * 1024 * 1024, file);
        let header_fixup = write_header(&mut w)?;
        let tiles_x = (width + tile_size - 1) / tile_size;
        let tiles_y = (height + tile_size - 1) / tile_size;
        Ok(Self {
            w, tile_size, width, height, geotransform: *geotransform, nodata, bits_per_sample, compress_level,
            header_fixup,
            main_level: LevelInfo { width, height, tiles_x, tiles_y, tile_info: vec![(0, 0); tiles_x * tiles_y] },
            overview_levels: Vec::new(),
        })
    }

    pub fn write_tile(&mut self, tx: usize, ty: usize, data: Option<&Vec<u8>>) -> io::Result<()> {
        let idx = ty * self.main_level.tiles_x + tx;
        if let Some(bytes) = data {
            let offset = self.w.stream_position()?;
            self.w.write_all(bytes)?;
            self.main_level.tile_info[idx] = (offset, bytes.len() as u64);
        }
        Ok(())
    }

    pub fn write_overview_from_store_u8(&mut self, store: &TileStoreU8) -> io::Result<()> {
        let (ts, tiles_x, tiles_y) = (self.tile_size, store.tiles_x, store.tiles_y);
        let mut tile_info = vec![(0u64, 0u64); tiles_x * tiles_y];
        for ty in 0..tiles_y {
            for tx in 0..tiles_x {
                if let Some(compressed) = store.get_compressed(tx, ty) {
                    let offset = self.w.stream_position()?;
                    self.w.write_all(compressed)?;
                    tile_info[ty * tiles_x + tx] = (offset, compressed.len() as u64);
                }
            }
        }
        self.overview_levels.push(LevelInfo { width: store.width, height: store.height, tiles_x, tiles_y, tile_info });
        Ok(())
    }

    pub fn write_overview_from_store_u16(&mut self, store: &TileStoreU16) -> io::Result<()> {
        let (ts, tiles_x, tiles_y) = (self.tile_size, store.tiles_x, store.tiles_y);
        let mut tile_info = vec![(0u64, 0u64); tiles_x * tiles_y];
        for ty in 0..tiles_y {
            for tx in 0..tiles_x {
                if let Some(compressed) = store.get_compressed(tx, ty) {
                    let offset = self.w.stream_position()?;
                    self.w.write_all(compressed)?;
                    tile_info[ty * tiles_x + tx] = (offset, compressed.len() as u64);
                }
            }
        }
        self.overview_levels.push(LevelInfo { width: store.width, height: store.height, tiles_x, tiles_y, tile_info });
        Ok(())
    }

    pub fn finalize(mut self, stats: Option<(f64, f64)>, gdal_scale: f64, gdal_offset: f64) -> io::Result<()> {
        self.w.flush()?;
        let use_deflate = self.compress_level > 0;
        let cc: u16 = if use_deflate { 8 } else { 1 };
        let ts = self.tile_size as u32;
        let bps = self.bits_per_sample;

        let mut next_ifd: u64 = 0;
        for i in (0..self.overview_levels.len()).rev() {
            let lvl = &self.overview_levels[i];
            let mut tags = build_image_tags(lvl.width as u32, lvl.height as u32, ts, cc, use_deflate, &lvl.tile_info, true, bps);
            let (ifd_off, _) = write_ifd(&mut self.w, &mut tags, next_ifd)?;
            next_ifd = ifd_off;
        }

        let lvl = &self.main_level;
        let mut tags = build_image_tags(lvl.width as u32, lvl.height as u32, ts, cc, use_deflate, &lvl.tile_info, false, bps);

        let gt = &self.geotransform;
        tags.push(Tag::f64_arr(33550, &[gt[1].abs(), gt[5].abs(), 0.0]));
        tags.push(Tag::f64_arr(33922, &[0.0, 0.0, 0.0, gt[0], gt[3], 0.0]));
        tags.push(Tag::u16_arr(34735, &[1, 1, 0, 3, 1024, 0, 1, 2, 1025, 0, 1, 1, 2048, 0, 1, 4326]));
        tags.push(Tag::ascii(42113, &format!("{}", self.nodata)));

        let mut md = String::from("<GDALMetadata>\n");
        if gdal_scale != 1.0 || gdal_offset != 0.0 {
            md.push_str(&format!("<Item name=\"OFFSET\" sample=\"0\" role=\"offset\">{}</Item>\n", gdal_offset));
            md.push_str(&format!("<Item name=\"SCALE\" sample=\"0\" role=\"scale\">{}</Item>\n", gdal_scale));
        }
        if let Some((min_v, max_v)) = stats {
            md.push_str(&format!("<Item name=\"STATISTICS_MINIMUM\" sample=\"0\">{}</Item>\n", min_v));
            md.push_str(&format!("<Item name=\"STATISTICS_MAXIMUM\" sample=\"0\">{}</Item>\n", max_v));
        }
        md.push_str("</GDALMetadata>");

        if md.len() > 16 {
            tags.push(Tag::ascii(42112, &md));
        }

        let (main_ifd_off, _) = write_ifd(&mut self.w, &mut tags, next_ifd)?;
        fixup_u64(&mut self.w, self.header_fixup, main_ifd_off)?;
        self.w.flush()?;
        Ok(())
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// Tile Compression
// ═══════════════════════════════════════════════════════════════════════════════

pub fn compress_u8_tile(data: &[u8], width: usize, height: usize, level: u32) -> Vec<u8> {
    let mut bytes = data.to_vec();
    if level == 0 { return bytes; }

    let row_bytes = width;
    for y in 0..height {
        let s = y * row_bytes;
        if s + row_bytes > bytes.len() { break; }
        for i in (1..row_bytes).rev() {
            bytes[s + i] = bytes[s + i].wrapping_sub(bytes[s + i - 1]);
        }
    }
    let mut enc = ZlibEncoder::new(Vec::with_capacity(bytes.len() / 2), Compression::new(level));
    enc.write_all(&bytes).expect("zlib compress");
    enc.finish().expect("zlib finish")
}

pub fn compress_u16_tile(data: &[u16], width: usize, height: usize, level: u32) -> Vec<u8> {
    let mut bytes = vec![0u8; data.len() * 2];
    for (i, v) in data.iter().enumerate() {
        let b = v.to_le_bytes();
        bytes[i * 2] = b[0];
        bytes[i * 2 + 1] = b[1];
    }
    if level == 0 { return bytes; }

    let row_bytes = width * 2;
    for y in 0..height {
        let s = y * row_bytes;
        if s + row_bytes > bytes.len() { break; }
        for x in (1..width).rev() {
            let cx = s + x * 2;
            let px = s + (x - 1) * 2;
            let curr = u16::from_le_bytes([bytes[cx], bytes[cx + 1]]);
            let prev = u16::from_le_bytes([bytes[px], bytes[px + 1]]);
            let diff = curr.wrapping_sub(prev);
            bytes[cx..cx + 2].copy_from_slice(&diff.to_le_bytes());
        }
    }
    let mut enc = ZlibEncoder::new(Vec::with_capacity(bytes.len() / 2), Compression::new(level));
    enc.write_all(&bytes).expect("zlib compress");
    enc.finish().expect("zlib finish")
}

// ═══════════════════════════════════════════════════════════════════════════════
// BigTIFF Internals
// ═══════════════════════════════════════════════════════════════════════════════

fn write_header(w: &mut (impl Write + Seek)) -> io::Result<u64> {
    w.write_all(&0x4949u16.to_le_bytes())?;
    w.write_all(&43u16.to_le_bytes())?;
    w.write_all(&8u16.to_le_bytes())?;
    w.write_all(&0u16.to_le_bytes())?;
    let fixup = w.stream_position()?;
    w.write_all(&0u64.to_le_bytes())?;
    Ok(fixup)
}

fn write_ifd(w: &mut (impl Write + Seek), tags: &mut Vec<Tag>, next_ifd: u64) -> io::Result<(u64, u64)> {
    tags.sort_by_key(|t| t.tag);
    for t in tags.iter_mut() {
        if t.data.len() > 8 {
            t.overflow = w.stream_position()?;
            w.write_all(&t.data)?;
        }
    }
    let pos = w.stream_position()?;
    if pos % 2 != 0 { w.write_all(&[0u8])?; }
    let ifd_offset = w.stream_position()?;
    w.write_all(&(tags.len() as u64).to_le_bytes())?;
    for t in tags.iter() {
        w.write_all(&t.tag.to_le_bytes())?;
        w.write_all(&t.dtype.to_le_bytes())?;
        w.write_all(&t.count.to_le_bytes())?;
        let mut vbuf =[0u8; 8];
        if t.data.len() <= 8 { vbuf[..t.data.len()].copy_from_slice(&t.data); }
        else { vbuf = t.overflow.to_le_bytes(); }
        w.write_all(&vbuf)?;
    }
    let next_fix = w.stream_position()?;
    w.write_all(&next_ifd.to_le_bytes())?;
    Ok((ifd_offset, next_fix))
}

fn fixup_u64(w: &mut (impl Write + Seek), pos: u64, val: u64) -> io::Result<()> {
    let cur = w.stream_position()?;
    w.seek(SeekFrom::Start(pos))?;
    w.write_all(&val.to_le_bytes())?;
    w.seek(SeekFrom::Start(cur))?;
    Ok(())
}

fn build_image_tags(width: u32, height: u32, ts: u32, compression: u16, use_pred: bool, tile_info: &[(u64, u64)], is_overview: bool, bps: u16) -> Vec<Tag> {
    let offsets: Vec<u64> = tile_info.iter().map(|t| t.0).collect();
    let sizes: Vec<u64> = tile_info.iter().map(|t| t.1).collect();
    let mut tags = vec![
        Tag::long(256, width), Tag::long(257, height),
        Tag::short(258, bps), Tag::short(259, compression), Tag::short(262, 1),
        Tag::short(277, 1), Tag::long(322, ts), Tag::long(323, ts),
        Tag::long8_arr(324, &offsets), Tag::long8_arr(325, &sizes),
        Tag::short(339, 1), // 1 = Unsigned Integer
    ];
    if use_pred { tags.push(Tag::short(317, 2)); } // Integer Differencing Predictor
    if is_overview { tags.push(Tag::long(254, 1)); }
    tags
}

struct Tag { tag: u16, dtype: u16, count: u64, data: Vec<u8>, overflow: u64 }
impl Tag {
    fn short(tag: u16, val: u16) -> Self { Self { tag, dtype: 3, count: 1, data: val.to_le_bytes().to_vec(), overflow: 0 } }
    fn long(tag: u16, val: u32) -> Self { Self { tag, dtype: 4, count: 1, data: val.to_le_bytes().to_vec(), overflow: 0 } }
    fn long8_arr(tag: u16, v: &[u64]) -> Self { Self { tag, dtype: 16, count: v.len() as u64, data: v.iter().flat_map(|x| x.to_le_bytes()).collect(), overflow: 0 } }
    fn f64_arr(tag: u16, v: &[f64]) -> Self { Self { tag, dtype: 12, count: v.len() as u64, data: v.iter().flat_map(|x| x.to_le_bytes()).collect(), overflow: 0 } }
    fn u16_arr(tag: u16, v: &[u16]) -> Self { Self { tag, dtype: 3, count: v.len() as u64, data: v.iter().flat_map(|x| x.to_le_bytes()).collect(), overflow: 0 } }
    fn ascii(tag: u16, s: &str) -> Self { let mut d = s.as_bytes().to_vec(); d.push(0); Self { tag, dtype: 2, count: d.len() as u64, data: d, overflow: 0 } }
}