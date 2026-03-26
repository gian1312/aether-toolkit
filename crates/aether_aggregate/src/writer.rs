// =============================================================================
// BigTIFF Float32 Writer — with inline overview support
// =============================================================================

use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;
use flate2::Compression;
use rayon::prelude::*;
use std::fs::File;
use std::io::{self, BufWriter, Read, Seek, SeekFrom, Write};

// ═══════════════════════════════════════════════════════════════════════════════
// TileStore — holds compressed tile data, supports decompression + downsampling
// ═══════════════════════════════════════════════════════════════════════════════

pub struct TileStore {
    pub width: usize,
    pub height: usize,
    pub tile_size: usize,
    pub tiles_x: usize,
    pub tiles_y: usize,
    pub nodata: f32,
    compress_level: u32,
    data: Vec<Option<Vec<u8>>>, // compressed tile bytes, indexed by ty * tiles_x + tx
}

impl TileStore {
    pub fn new(width: usize, height: usize, tile_size: usize, nodata: f32, compress_level: u32) -> Self {
        let tiles_x = (width + tile_size - 1) / tile_size;
        let tiles_y = (height + tile_size - 1) / tile_size;
        Self {
            width, height, tile_size, tiles_x, tiles_y, nodata, compress_level,
            data: vec![None; tiles_x * tiles_y],
        }
    }

    pub fn store(&mut self, tx: usize, ty: usize, compressed: Vec<u8>) {
        let idx = ty * self.tiles_x + tx;
        if idx < self.data.len() {
            self.data[idx] = Some(compressed);
        }
    }

    pub fn get_compressed(&self, tx: usize, ty: usize) -> Option<&Vec<u8>> {
        let idx = ty * self.tiles_x + tx;
        self.data.get(idx).and_then(|d| d.as_ref())
    }

    /// Decompress a stored tile to f32 pixels. Returns None if tile is empty.
    fn decompress_tile(&self, tx: usize, ty: usize) -> Option<Vec<f32>> {
        let compressed = self.get_compressed(tx, ty)?;
        let ts = self.tile_size;
        let expected_bytes = ts * ts * 4;

        let mut raw = if self.compress_level > 0 {
            let mut buf = Vec::with_capacity(expected_bytes);
            let _ = ZlibDecoder::new(compressed.as_slice()).read_to_end(&mut buf);
            buf
        } else {
            compressed.clone()
        };

        // Undo PREDICTOR=3 (floating point predictor)
        if self.compress_level > 0 {
            let row_bytes = ts * 4;
            let mut tmp = vec![0u8; row_bytes];
            for y in 0..ts {
                let s = y * row_bytes;
                if s + row_bytes > raw.len() { break; }

                // 1. Undifference bytes
                for i in 1..row_bytes {
                    raw[s + i] = raw[s + i].wrapping_add(raw[s + i - 1]);
                }

                // 2. Un-reorder bytes
                for b in 0..4 {
                    for x in 0..ts {
                        tmp[x * 4 + b] = raw[s + b * ts + x];
                    }
                }
                raw[s..s + row_bytes].copy_from_slice(&tmp);
            }
        }

        // Interpret bytes as f32
        let mut pixels = vec![self.nodata; ts * ts];
        for i in 0..ts * ts {
            let off = i * 4;
            if off + 4 <= raw.len() {
                if self.compress_level > 0 {
                    pixels[i] = f32::from_be_bytes([raw[off], raw[off + 1], raw[off + 2], raw[off + 3]]);
                } else {
                    pixels[i] = f32::from_le_bytes([raw[off], raw[off + 1], raw[off + 2], raw[off + 3]]);
                }
            }
        }
        Some(pixels)
    }

    /// Build a 4× downsampled overview TileStore by nearest-neighbor sampling.
    /// Each overview pixel picks one pixel from this level at 4× stride.
    pub fn build_overview_4x(&self) -> TileStore {
        let ts = self.tile_size;
        let ovr_w = (self.width + 3) / 4;
        let ovr_h = (self.height + 3) / 4;
        let mut ovr = TileStore::new(ovr_w, ovr_h, ts, self.nodata, self.compress_level);

        let total_tiles = ovr.tiles_x * ovr.tiles_y;
        let tiles_x = ovr.tiles_x;

        let ovr_data: Vec<Option<Vec<u8>>> = (0..total_tiles)
            .into_par_iter()
            .map(|idx| {
                let otx = idx % tiles_x;
                let oty = idx / tiles_x;
                let ovr_tile = self.downsample_tile(otx, oty, ovr_w, ovr_h);

                // Ensure rogue NaNs don't trick the algorithm into saving a tile of mostly Nodata
                if ovr_tile.iter().all(|&v| v == self.nodata || v.is_nan()) {
                    None
                } else {
                    Some(compress_f32_tile(&ovr_tile, ts, ts, self.compress_level))
                }
            })
            .collect();

        ovr.data = ovr_data;
        ovr
    }

    /// Downsample one overview tile by reading from this level's tiles.
    fn downsample_tile(&self, otx: usize, oty: usize, ovr_w: usize, ovr_h: usize) -> Vec<f32> {
        let ts = self.tile_size;
        let mut out = vec![self.nodata; ts * ts];

        let obw = ts.min(ovr_w - otx * ts);
        let obh = ts.min(ovr_h - oty * ts);

        // FAST PATH: Pre-fetch the 4x4 block of 16 source tiles this overview tile needs
        let mut src_tiles = vec![None; 16];
        for dty in 0..4 {
            let sty = oty * 4 + dty;
            for dtx in 0..4 {
                let stx = otx * 4 + dtx;
                if stx < self.tiles_x && sty < self.tiles_y {
                    src_tiles[dty * 4 + dtx] = self.decompress_tile(stx, sty);
                }
            }
        }

        for dy in 0..obh {
            // Source pixel Y in this level
            let sy = (oty * ts + dy) * 4;
            if sy >= self.height { break; }
            let sty = sy / ts;
            let local_y = sy % ts;
            let dty = sty - oty * 4;

            for dx in 0..obw {
                // Source pixel X in this level
                let sx = (otx * ts + dx) * 4;
                if sx >= self.width { break; }
                let stx = sx / ts;
                let local_x = sx % ts;
                let dtx = stx - otx * 4;

                if let Some(ref tile) = src_tiles[dty * 4 + dtx] {
                    let val = tile[local_y * ts + local_x];
                    // Skip Nodata and rogue NaN values during extraction
                    if val != self.nodata && !val.is_nan() {
                        out[dy * ts + dx] = val;
                    }
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
    nodata: f32,
    compress_level: u32,
    header_fixup: u64,
    pub main_level: LevelInfo,
    pub overview_levels: Vec<LevelInfo>,
}

impl BigTiffWriter {
    pub fn create(
        path: &str, width: usize, height: usize, tile_size: usize,
        geotransform: &[f64; 6], nodata: f32, compress_level: u32,
    ) -> io::Result<Self> {
        let file = File::create(path)?;
        let mut w = BufWriter::with_capacity(32 * 1024 * 1024, file);
        let header_fixup = write_header(&mut w)?;
        let tiles_x = (width + tile_size - 1) / tile_size;
        let tiles_y = (height + tile_size - 1) / tile_size;
        Ok(Self {
            w, tile_size, width, height, geotransform: *geotransform, nodata, compress_level,
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

    /// Write all tiles from a TileStore as an overview level.
    pub fn write_overview_from_store(&mut self, store: &TileStore) -> io::Result<()> {
        let ts = self.tile_size;
        let tiles_x = store.tiles_x;
        let tiles_y = store.tiles_y;
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

        self.overview_levels.push(LevelInfo {
            width: store.width, height: store.height, tiles_x, tiles_y, tile_info,
        });
        Ok(())
    }

    pub fn finalize(mut self, stats: Option<(f64, f64)>) -> io::Result<()> {
        self.w.flush()?;
        let use_deflate = self.compress_level > 0;
        let cc: u16 = if use_deflate { 8 } else { 1 };
        let ts = self.tile_size as u32;

        // Write overview IFDs (last to first, chaining backwards)
        let mut next_ifd: u64 = 0;
        for i in (0..self.overview_levels.len()).rev() {
            let lvl = &self.overview_levels[i];
            let mut tags = build_image_tags(lvl.width as u32, lvl.height as u32, ts, cc, use_deflate, &lvl.tile_info, true);
            let (ifd_off, _) = write_ifd(&mut self.w, &mut tags, next_ifd)?;
            next_ifd = ifd_off;
        }

        // Write main IFD
        let lvl = &self.main_level;
        let mut tags = build_image_tags(lvl.width as u32, lvl.height as u32, ts, cc, use_deflate, &lvl.tile_info, false);

        let gt = &self.geotransform;
        tags.push(Tag::f64_arr(33550, &[gt[1].abs(), gt[5].abs(), 0.0]));
        tags.push(Tag::f64_arr(33922, &[0.0, 0.0, 0.0, gt[0], gt[3], 0.0]));
        tags.push(Tag::u16_arr(34735, &[1, 1, 0, 3, 1024, 0, 1, 2, 1025, 0, 1, 1, 2048, 0, 1, 4326]));
        tags.push(Tag::ascii(42113, &format!("{}", self.nodata)));
        if let Some((min_v, max_v)) = stats {
            tags.push(Tag::ascii(42112, &format!(
                "<GDALMetadata>\n<Item name=\"STATISTICS_MINIMUM\">{}</Item>\n<Item name=\"STATISTICS_MAXIMUM\">{}</Item>\n</GDALMetadata>",
                min_v, max_v
            )));
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

pub fn compress_f32_tile(data: &[f32], width: usize, height: usize, level: u32) -> Vec<u8> {
    let mut bytes: Vec<u8> = vec![0u8; data.len() * 4];
    if level == 0 {
        for (i, v) in data.iter().enumerate() {
            let b = v.to_le_bytes();
            bytes[i * 4] = b[0];
            bytes[i * 4 + 1] = b[1];
            bytes[i * 4 + 2] = b[2];
            bytes[i * 4 + 3] = b[3];
        }
        return bytes;
    }

    // PREDICTOR=3: byte reordering (and Big-Endian encoding for standardization)
    for (i, v) in data.iter().enumerate() {
        let b = v.to_be_bytes();
        let row = i / width;
        let col = i % width;
        let s = row * width * 4;
        bytes[s + col] = b[0];
        bytes[s + width + col] = b[1];
        bytes[s + 2 * width + col] = b[2];
        bytes[s + 3 * width + col] = b[3];
    }

    // Bytewise horizontal differencing, right-to-left
    let row_bytes = width * 4;
    for y in 0..height {
        let s = y * row_bytes;
        if s + row_bytes > bytes.len() { break; }
        for i in (1..row_bytes).rev() {
            bytes[s + i] = bytes[s + i].wrapping_sub(bytes[s + i - 1]);
        }
    }

    let mut enc = ZlibEncoder::new(Vec::with_capacity(bytes.len() / 4), Compression::new(level));
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

fn build_image_tags(width: u32, height: u32, ts: u32, compression: u16, use_pred: bool, tile_info: &[(u64, u64)], is_overview: bool) -> Vec<Tag> {
    let offsets: Vec<u64> = tile_info.iter().map(|t| t.0).collect();
    let sizes: Vec<u64> = tile_info.iter().map(|t| t.1).collect();
    let mut tags = vec![
        Tag::long(256, width), Tag::long(257, height),
        Tag::short(258, 32), Tag::short(259, compression), Tag::short(262, 1),
        Tag::short(277, 1), Tag::long(322, ts), Tag::long(323, ts),
        Tag::long8_arr(324, &offsets), Tag::long8_arr(325, &sizes),
        Tag::short(339, 3),
    ];
    if use_pred { tags.push(Tag::short(317, 3)); }
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