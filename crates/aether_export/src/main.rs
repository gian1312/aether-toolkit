// =============================================================================
// AETHER Pure-Rust Bitmask → GeoTIFF Exporter
// =============================================================================
//
// Zero C dependencies. No GDAL, no libtiff. Writes valid BigTIFF directly.
//
// KEY INSIGHT: The input .bit file is already packed bits (MSB-first, rows
// byte-aligned). A 1-bit TIFF tile is also packed bits (MSB-first, rows
// byte-aligned per tile width). Since tile_size is a multiple of 8, tile
// column boundaries are always byte-aligned in the input.
//
// Therefore: tile extraction is pure memcpy. No bit manipulation needed
// for the main image. The entire unpack→repack cycle from the Python
// version is eliminated.
//
//[8-BIT MODE] In 8-bit propagation loss mode, the input is raw u8 bytes
// (one byte per pixel). Tile extraction is also pure memcpy — just copy
// tile_size bytes per row instead of tile_size/8.
//
// ARCHITECTURE:
//   1. mmap the .bit input (zero-copy)
//   2. For each tile-row, extract+compress all tiles in parallel (rayon)
//   3. Write compressed tiles sequentially with BufWriter
//   4. Build overview levels via nearest-neighbor bit sampling
//   5. Write BigTIFF IFDs with GeoTIFF tags at the end
//   6. Fixup header/IFD offsets with seeks

use clap::Parser;
use flate2::write::ZlibEncoder;
use flate2::Compression;
use memmap2::Mmap;
use rayon::prelude::*;
use serde::Deserialize;
use std::fs::File;
use std::io::{self, BufReader, BufWriter, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::time::Instant;

// ═══════════════════════════════════════════════════════════════════════════════
// CLI
// ═══════════════════════════════════════════════════════════════════════════════

#[derive(Parser, Debug)]
#[command(name = "aether_export")]
#[command(about = "Convert AETHER .bit bitmask → Cloud-Optimized GeoTIFF (pure Rust, no GDAL)")]
struct Args {
    /// Path to the packed bitmask file (.bit)
    #[arg(short = 'i', long)]
    bit_path: PathBuf,

    /// Path to the JSON sidecar metadata
    #[arg(short = 'j', long)]
    json_path: PathBuf,

    /// Output GeoTIFF path
    #[arg(short = 'o', long)]
    output: PathBuf,

    /// Compression level (1=fast, 9=small). 0=no compression.
    #[arg(short = 'l', long, default_value_t = 1)]
    level: u32,

    /// Tile size in pixels (must be multiple of 16, default 512)
    #[arg(short = 't', long, default_value_t = 512)]
    tile_size: usize,

    /// Skip overview generation
    #[arg(long, default_value_t = false)]
    no_overviews: bool,
}

// ═══════════════════════════════════════════════════════════════════════════════
// Metadata
// ═══════════════════════════════════════════════════════════════════════════════

#[derive(Deserialize)]
struct Metadata {
    dimensions: Dimensions,
    geotransform: [f64; 6],
    projection: String,
    #[serde(default)]
    row_stride_bytes: Option<usize>,
    //[8-BIT MODE] Output format from sidecar: "1BIT_LOS" or "8BIT_PROP"
    #[serde(default = "default_output_format")]
    output_format: String,
}

//[8-BIT MODE] Default to 1-bit for backward compatibility
fn default_output_format() -> String { "1BIT_LOS".to_string() }

#[derive(Deserialize)]
struct Dimensions {
    width: usize,
    height: usize,
}

// ═══════════════════════════════════════════════════════════════════════════════
// Profiler
// ═══════════════════════════════════════════════════════════════════════════════

struct Profiler(Instant);

impl Profiler {
    fn new() -> Self { Self(Instant::now()) }
    fn lap(&mut self, label: &str) {
        eprintln!("[Profile] {}: {:.2}s", label, self.0.elapsed().as_secs_f64());
        self.0 = Instant::now();
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// BigTIFF Constants
// ═══════════════════════════════════════════════════════════════════════════════

// Byte order & version
const BIGTIFF_LE: u16 = 0x4949; // Little-endian
const BIGTIFF_VERSION: u16 = 43;
const BIGTIFF_OFFSET_SIZE: u16 = 8;

// TIFF data types
const TY_ASCII: u16 = 2;   // NUL-terminated string
const TY_SHORT: u16 = 3;   // u16
const TY_LONG: u16 = 4;    // u32
const TY_DOUBLE: u16 = 12; // f64
const TY_LONG8: u16 = 16;  // u64 (BigTIFF)

// TIFF tags
const TAG_NEW_SUBFILE_TYPE: u16 = 254;
const TAG_IMAGE_WIDTH: u16 = 256;
const TAG_IMAGE_LENGTH: u16 = 257;
const TAG_BITS_PER_SAMPLE: u16 = 258;
const TAG_COMPRESSION: u16 = 259;
const TAG_PHOTOMETRIC: u16 = 262;
const TAG_SAMPLES_PER_PIXEL: u16 = 277;
const TAG_TILE_WIDTH: u16 = 322;
const TAG_TILE_LENGTH: u16 = 323;
const TAG_TILE_OFFSETS: u16 = 324;
const TAG_TILE_BYTE_COUNTS: u16 = 325;
const TAG_SAMPLE_FORMAT: u16 = 339;

// GeoTIFF tags
const TAG_MODEL_PIXEL_SCALE: u16 = 33550;
const TAG_MODEL_TIEPOINT: u16 = 33922;
const TAG_GEO_KEY_DIRECTORY: u16 = 34735;

//[8-BIT MODE] GDAL extension tags for offset/scale and nodata
const TAG_GDAL_METADATA: u16 = 42112;
const TAG_GDAL_NODATA: u16 = 42113;

// Compression codes
const COMPRESS_NONE: u16 = 1;
const COMPRESS_DEFLATE: u16 = 8;

// GeoKey IDs
const GK_MODEL_TYPE: u16 = 1024;
const GK_RASTER_TYPE: u16 = 1025;
const GK_GEOGRAPHIC_TYPE: u16 = 2048;
const GK_PROJECTED_CS_TYPE: u16 = 3072;

// ═══════════════════════════════════════════════════════════════════════════════
// BigTIFF IFD Builder
// ═══════════════════════════════════════════════════════════════════════════════

/// Represents one IFD entry. Data is stored as raw bytes.
/// If data.len() <= 8, it fits inline in the value/offset field.
/// Otherwise it must be written separately and the offset recorded.
struct TagEntry {
    tag: u16,
    dtype: u16,
    count: u64,
    data: Vec<u8>,
    /// Filled during write: file offset where overflow data was written.
    overflow_offset: u64,
}

impl TagEntry {
    fn inline(&self) -> bool { self.data.len() <= 8 }

    fn short(tag: u16, val: u16) -> Self {
        Self { tag, dtype: TY_SHORT, count: 1, data: val.to_le_bytes().to_vec(), overflow_offset: 0 }
    }
    fn long(tag: u16, val: u32) -> Self {
        Self { tag, dtype: TY_LONG, count: 1, data: val.to_le_bytes().to_vec(), overflow_offset: 0 }
    }
    fn long8(tag: u16, val: u64) -> Self {
        Self { tag, dtype: TY_LONG8, count: 1, data: val.to_le_bytes().to_vec(), overflow_offset: 0 }
    }
    fn long8_array(tag: u16, vals: &[u64]) -> Self {
        let data: Vec<u8> = vals.iter().flat_map(|v| v.to_le_bytes()).collect();
        Self { tag, dtype: TY_LONG8, count: vals.len() as u64, data, overflow_offset: 0 }
    }
    fn double_array(tag: u16, vals: &[f64]) -> Self {
        let data: Vec<u8> = vals.iter().flat_map(|v| v.to_le_bytes()).collect();
        Self { tag, dtype: TY_DOUBLE, count: vals.len() as u64, data, overflow_offset: 0 }
    }
    fn short_array(tag: u16, vals: &[u16]) -> Self {
        let data: Vec<u8> = vals.iter().flat_map(|v| v.to_le_bytes()).collect();
        Self { tag, dtype: TY_SHORT, count: vals.len() as u64, data, overflow_offset: 0 }
    }
    //[8-BIT MODE] ASCII string tag (NUL-terminated, count includes NUL)
    fn ascii(tag: u16, s: &str) -> Self {
        let mut data = s.as_bytes().to_vec();
        data.push(0); // NUL terminator
        let count = data.len() as u64;
        Self { tag, dtype: TY_ASCII, count, data, overflow_offset: 0 }
    }
}

/// Write BigTIFF header. Returns position of the "offset to first IFD" field for fixup.
fn write_bigtiff_header(w: &mut (impl Write + Seek)) -> io::Result<u64> {
    w.write_all(&BIGTIFF_LE.to_le_bytes())?;
    w.write_all(&BIGTIFF_VERSION.to_le_bytes())?;
    w.write_all(&BIGTIFF_OFFSET_SIZE.to_le_bytes())?;
    w.write_all(&0u16.to_le_bytes())?; // reserved
    let fixup_pos = w.stream_position()?;
    w.write_all(&0u64.to_le_bytes())?; // placeholder: offset to first IFD
    Ok(fixup_pos)
}

/// Write an IFD (entries + next_ifd pointer).
/// Overflow data for large tags is written immediately before the IFD.
/// Returns (ifd_offset, position_of_next_ifd_field) for later fixup.
fn write_ifd(
    w: &mut (impl Write + Seek),
    entries: &mut Vec<TagEntry>,
    next_ifd: u64,
) -> io::Result<(u64, u64)> {
    // Sort by tag (TIFF requirement)
    entries.sort_by_key(|e| e.tag);

    // Write overflow data for entries that don't fit inline
    for entry in entries.iter_mut() {
        if !entry.inline() {
            entry.overflow_offset = w.stream_position()?;
            w.write_all(&entry.data)?;
        }
    }

    // Align to 2-byte boundary (TIFF requirement for IFDs)
    let pos = w.stream_position()?;
    if pos % 2 != 0 {
        w.write_all(&[0u8])?;
    }

    let ifd_offset = w.stream_position()?;

    // Entry count (u64 for BigTIFF)
    w.write_all(&(entries.len() as u64).to_le_bytes())?;

    // Each entry: tag(2) + type(2) + count(8) + value/offset(8) = 20 bytes
    for entry in entries.iter() {
        w.write_all(&entry.tag.to_le_bytes())?;
        w.write_all(&entry.dtype.to_le_bytes())?;
        w.write_all(&entry.count.to_le_bytes())?;

        let mut value_buf = [0u8; 8];
        if entry.inline() {
            value_buf[..entry.data.len()].copy_from_slice(&entry.data);
        } else {
            value_buf = entry.overflow_offset.to_le_bytes();
        }
        w.write_all(&value_buf)?;
    }

    // Next IFD offset
    let next_ifd_fixup = w.stream_position()?;
    w.write_all(&next_ifd.to_le_bytes())?;

    Ok((ifd_offset, next_ifd_fixup))
}

/// Seek back and overwrite a u64 at the given position.
fn fixup_u64(w: &mut (impl Write + Seek), pos: u64, val: u64) -> io::Result<()> {
    let cur = w.stream_position()?;
    w.seek(SeekFrom::Start(pos))?;
    w.write_all(&val.to_le_bytes())?;
    w.seek(SeekFrom::Start(cur))?;
    Ok(())
}

// ═══════════════════════════════════════════════════════════════════════════════
// Tile Operations
// ═══════════════════════════════════════════════════════════════════════════════

/// Check if a byte slice is entirely zero (using u64 chunks for speed).
fn is_all_zero(data: &[u8]) -> bool {
    let (prefix, chunks, suffix) = unsafe { data.align_to::<u64>() };
    prefix.iter().all(|&b| b == 0)
        && chunks.iter().all(|&v| v == 0)
        && suffix.iter().all(|&b| b == 0)
}

/// Extract a 1-bit tile's packed bit data directly from the memory-mapped input.
///
/// This is the critical optimization: since tile_size is a multiple of 8,
/// tile column boundaries are always byte-aligned in the packed bit input.
/// Tile extraction is pure memcpy — no bit manipulation needed.
///
/// The returned buffer is tile_size/8 × tile_size bytes (the full tile,
/// zero-padded at edges), ready for DEFLATE compression.
fn extract_tile_packed_1bit(
    mmap: &[u8],
    row_stride: usize,
    width: usize,
    height: usize,
    ts: usize, // tile_size
    tx: usize,
    ty: usize,
) -> Vec<u8> {
    let tile_row_bytes = ts / 8; // tile_size guaranteed multiple of 8
    let mut buf = vec![0u8; tile_row_bytes * ts];

    let x0 = tx * ts;
    let y0 = ty * ts;
    let tw = std::cmp::min(ts, width.saturating_sub(x0));
    let th = std::cmp::min(ts, height.saturating_sub(y0));
    if tw == 0 || th == 0 {
        return buf;
    }

    let src_byte_col = x0 / 8; // byte-aligned since ts is multiple of 8
    let full_bytes = tw / 8;
    let remainder_bits = tw % 8;
    let copy_bytes = full_bytes + if remainder_bits > 0 { 1 } else { 0 };

    for row in 0..th {
        let src_offset = (y0 + row) * row_stride + src_byte_col;
        let dst_offset = row * tile_row_bytes;

        // Bounds check
        if src_offset + copy_bytes > mmap.len() {
            break;
        }

        buf[dst_offset..dst_offset + copy_bytes]
            .copy_from_slice(&mmap[src_offset..src_offset + copy_bytes]);

        // Mask trailing bits in last byte if width doesn't end on byte boundary
        if remainder_bits > 0 {
            let mask: u8 = 0xFF << (8 - remainder_bits);
            buf[dst_offset + copy_bytes - 1] &= mask;
        }
    }

    buf
}

//[8-BIT MODE] Extract an 8-bit tile (one byte per pixel) via direct memcpy.
// Returns tile_size × tile_size bytes, zero-padded at edges.
fn extract_tile_packed_8bit(
    mmap: &[u8],
    row_stride: usize,
    width: usize,
    height: usize,
    ts: usize,
    tx: usize,
    ty: usize,
) -> Vec<u8> {
    let tile_row_bytes = ts; // 1 byte per pixel
    let mut buf = vec![0u8; tile_row_bytes * ts];

    let x0 = tx * ts;
    let y0 = ty * ts;
    let tw = std::cmp::min(ts, width.saturating_sub(x0));
    let th = std::cmp::min(ts, height.saturating_sub(y0));
    if tw == 0 || th == 0 {
        return buf;
    }

    for row in 0..th {
        let src_offset = (y0 + row) * row_stride + x0;
        let dst_offset = row * tile_row_bytes;

        if src_offset + tw > mmap.len() {
            break;
        }

        buf[dst_offset..dst_offset + tw]
            .copy_from_slice(&mmap[src_offset..src_offset + tw]);
    }

    buf
}

/// Read a single bit from the packed input.
#[inline(always)]
fn get_bit(mmap: &[u8], row_stride: usize, x: usize, y: usize) -> u8 {
    let idx = y * row_stride + x / 8;
    if idx >= mmap.len() { return 0; }
    (mmap[idx] >> (7 - (x & 7))) & 1
}

//[8-BIT MODE] Read a single byte from 8-bit input.
#[inline(always)]
fn get_byte(mmap: &[u8], row_stride: usize, x: usize, y: usize) -> u8 {
    let idx = y * row_stride + x;
    if idx >= mmap.len() { return 0; }
    mmap[idx]
}

/// Extract a 1-bit overview tile by nearest-neighbor sampling from the original.
fn extract_overview_tile_1bit(
    mmap: &[u8],
    row_stride: usize,
    src_width: usize,
    src_height: usize,
    ts: usize,
    tx: usize,
    ty: usize,
    factor: usize,
) -> Vec<u8> {
    let tile_row_bytes = ts / 8;
    let mut buf = vec![0u8; tile_row_bytes * ts];

    let ovr_w = (src_width + factor - 1) / factor;
    let ovr_h = (src_height + factor - 1) / factor;
    let x0 = tx * ts;
    let y0 = ty * ts;

    for py in 0..ts {
        let oy = y0 + py;
        if oy >= ovr_h { break; }
        let sy = oy * factor;
        if sy >= src_height { break; }

        for px in 0..ts {
            let ox = x0 + px;
            if ox >= ovr_w { break; }
            let sx = ox * factor;
            if sx >= src_width { break; }

            if get_bit(mmap, row_stride, sx, sy) != 0 {
                buf[py * tile_row_bytes + px / 8] |= 1 << (7 - (px & 7));
            }
        }
    }

    buf
}

//[8-BIT MODE] Extract an 8-bit overview tile by nearest-neighbor sampling.
fn extract_overview_tile_8bit(
    mmap: &[u8],
    row_stride: usize,
    src_width: usize,
    src_height: usize,
    ts: usize,
    tx: usize,
    ty: usize,
    factor: usize,
) -> Vec<u8> {
    let tile_row_bytes = ts;
    let mut buf = vec![0u8; tile_row_bytes * ts];

    let ovr_w = (src_width + factor - 1) / factor;
    let ovr_h = (src_height + factor - 1) / factor;
    let x0 = tx * ts;
    let y0 = ty * ts;

    for py in 0..ts {
        let oy = y0 + py;
        if oy >= ovr_h { break; }
        let sy = oy * factor;
        if sy >= src_height { break; }

        for px in 0..ts {
            let ox = x0 + px;
            if ox >= ovr_w { break; }
            let sx = ox * factor;
            if sx >= src_width { break; }

            buf[py * tile_row_bytes + px] = get_byte(mmap, row_stride, sx, sy);
        }
    }

    buf
}

/// Compress tile data with zlib (TIFF DEFLATE = zlib stream format).
fn compress_tile(data: &[u8], level: u32) -> Vec<u8> {
    let mut enc = ZlibEncoder::new(Vec::with_capacity(data.len() / 4), Compression::new(level));
    enc.write_all(data).expect("zlib compress failed");
    enc.finish().expect("zlib finish failed")
}

// ═══════════════════════════════════════════════════════════════════════════════
// Image Level — holds tile data for one resolution level
// ═══════════════════════════════════════════════════════════════════════════════

struct ImageLevel {
    width: usize,
    height: usize,
    tiles_x: usize,
    tiles_y: usize,
    /// For each tile: (file_offset, compressed_size). offset=0 means sparse/empty.
    tile_info: Vec<(u64, u64)>,
}

// ═══════════════════════════════════════════════════════════════════════════════
// GeoTIFF Tag Builder
// ═══════════════════════════════════════════════════════════════════════════════

fn build_geo_tags(meta: &Metadata) -> Vec<TagEntry> {
    let gt = &meta.geotransform;
    // gt = [originX, pixelWidth, rotX, originY, rotY, pixelHeight]
    // pixelHeight is negative for north-up
    let pixel_scale_x = gt[1].abs();
    let pixel_scale_y = gt[5].abs();

    let mut tags = Vec::new();

    // ModelPixelScaleTag: [scaleX, scaleY, scaleZ]
    tags.push(TagEntry::double_array(TAG_MODEL_PIXEL_SCALE, &[
        pixel_scale_x, pixel_scale_y, 0.0,
    ]));

    // ModelTiepointTag: [I, J, K, X, Y, Z] — maps pixel (0,0) to geo origin
    tags.push(TagEntry::double_array(TAG_MODEL_TIEPOINT, &[
        0.0, 0.0, 0.0, gt[0], gt[3], 0.0,
    ]));

    // GeoKeyDirectoryTag: parse EPSG code and set appropriate keys
    let epsg = parse_epsg(&meta.projection).unwrap_or(0);

    if epsg > 0 {
        // Determine if geographic (EPSG 4000-4999) or projected
        let is_geographic = (4000..5000).contains(&epsg);

        if is_geographic {
            // [version, revision, minor, num_keys, key_entries...]
            let keys: Vec<u16> = vec![
                1, 1, 0, 3,                              // Header: v1.1.0, 3 keys
                GK_MODEL_TYPE, 0, 1, 2,                  // ModelTypeGeographic
                GK_RASTER_TYPE, 0, 1, 1,                 // RasterPixelIsArea
                GK_GEOGRAPHIC_TYPE, 0, 1, epsg as u16,   // EPSG code
            ];
            tags.push(TagEntry::short_array(TAG_GEO_KEY_DIRECTORY, &keys));
        } else {
            let keys: Vec<u16> = vec![
                1, 1, 0, 3,
                GK_MODEL_TYPE, 0, 1, 1,                  // ModelTypeProjected
                GK_RASTER_TYPE, 0, 1, 1,                 // RasterPixelIsArea
                GK_PROJECTED_CS_TYPE, 0, 1, epsg as u16, // EPSG code
            ];
            tags.push(TagEntry::short_array(TAG_GEO_KEY_DIRECTORY, &keys));
        }
    }

    tags
}

/// Parse "EPSG:XXXX" → Some(XXXX)
fn parse_epsg(proj: &str) -> Option<u32> {
    let s = proj.trim().to_uppercase();
    if s.starts_with("EPSG:") {
        s[5..].parse().ok()
    } else {
        None
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// Core Export Logic
// ═══════════════════════════════════════════════════════════════════════════════

fn run(args: Args) -> Result<(), Box<dyn std::error::Error>> {
    let mut prof = Profiler::new();
    let t_total = Instant::now();

    // ── 1. Load metadata ─────────────────────────────────────────────────
    let meta: Metadata =
        serde_json::from_reader(BufReader::new(File::open(&args.json_path)?))?;

    let width = meta.dimensions.width;
    let height = meta.dimensions.height;

    //[8-BIT MODE] Detect output format from sidecar
    let is_8bit = meta.output_format == "8BIT_PROP";

    // FIX: The core engine (aether_core) aligns rows to 32-bit (4-byte) boundaries.
    // The previous fallback ((width + 7) / 8) assumed 1-byte alignment, causing
    // diagonal streaks if 'row_stride_bytes' was missing from JSON or failed to parse.
    let row_stride = meta.row_stride_bytes.unwrap_or_else(|| {
        //[8-BIT MODE] Default stride depends on format
        if is_8bit {
            width // 1 byte per pixel, no alignment needed
        } else {
            ((width + 31) / 32) * 4
        }
    });

    let ts = args.tile_size;

    assert!(ts % 8 == 0 && ts >= 16, "Tile size must be multiple of 8, >= 16");

    let use_compression = args.level > 0;
    let compress_code = if use_compression { COMPRESS_DEFLATE } else { COMPRESS_NONE };

    //[8-BIT MODE] Bits per sample depends on format
    let bits_per_sample: u16 = if is_8bit { 8 } else { 1 };

    eprintln!("[Export] Image: {}×{} | Tile: {} | Stride: {} B | Format: {} | Compress: {}",
              width, height, ts, row_stride,
              if is_8bit { "8BIT_PROP" } else { "1BIT_LOS" },
              if use_compression { format!("deflate-{}", args.level) } else { "none".into() }
    );

    // ── 2. Memory-map input ──────────────────────────────────────────────
    let file = File::open(&args.bit_path)?;
    let mmap = unsafe { Mmap::map(&file)? };
    prof.lap("Setup");

    // ── 3. Determine overview levels ─────────────────────────────────────
    let mut overview_factors: Vec<usize> = Vec::new();
    if !args.no_overviews && width > 2048 {
        let mut f = 4usize;
        while width / f > 256 {
            overview_factors.push(f);
            f *= 4;
        }
        overview_factors.push(f);
    }

    // ── 4. Open output, write header ─────────────────────────────────────
    let out_file = File::create(&args.output)?;
    let mut w = BufWriter::with_capacity(64 * 1024 * 1024, out_file); // 64 MB buffer
    let header_fixup = write_bigtiff_header(&mut w)?;

    // ── 5. Write main image tiles ────────────────────────────────────────
    let main_level = write_tiles(
        &mut w, &mmap, row_stride, width, height, ts, args.level, None, is_8bit,
    )?;
    prof.lap("Main tiles written");

    // ── 6. Write overview tiles ──────────────────────────────────────────
    let mut ovr_levels: Vec<ImageLevel> = Vec::new();
    for (i, &factor) in overview_factors.iter().enumerate() {
        let ovr_w = (width + factor - 1) / factor;
        let ovr_h = (height + factor - 1) / factor;
        eprintln!("[Export] Overview {}: {}× → {}×{}", i + 1, factor, ovr_w, ovr_h);

        let level = write_tiles(
            &mut w, &mmap, row_stride, width, height, ts, args.level, Some(factor), is_8bit,
        )?;
        ovr_levels.push(level);
    }
    if !overview_factors.is_empty() {
        prof.lap("Overview tiles written");
    }

    // ── 7. Write IFDs (from last overview → first overview → main) ───────
    // We write them in reverse order so each IFD can point to the next.
    // The chain is: main → ovr[0] → ovr[1] → ... → 0

    let mut next_ifd_offset: u64 = 0;
    let mut ovr_ifd_offsets: Vec<u64> = vec![0; ovr_levels.len()];
    let mut ovr_next_fixups: Vec<u64> = vec![0; ovr_levels.len()];

    // Write overview IFDs (last to first)
    for i in (0..ovr_levels.len()).rev() {
        let level = &ovr_levels[i];
        let mut entries = build_image_tags(
            level.width as u32,
            level.height as u32,
            ts as u32,
            compress_code,
            &level.tile_info,
            true, // is_overview
            bits_per_sample, //[8-BIT MODE]
        );

        let (ifd_off, next_fix) = write_ifd(&mut w, &mut entries, next_ifd_offset)?;
        ovr_ifd_offsets[i] = ifd_off;
        ovr_next_fixups[i] = next_fix;
        next_ifd_offset = ifd_off;
    }

    // Write main IFD
    let mut main_entries = build_image_tags(
        width as u32,
        height as u32,
        ts as u32,
        compress_code,
        &main_level.tile_info,
        false,
        bits_per_sample, //[8-BIT MODE]
    );
    // Add GeoTIFF tags
    main_entries.extend(build_geo_tags(&meta));

    //[8-BIT MODE] Add GDAL offset/scale metadata so GIS tools display real dBm values.
    // Stored pixel = clamp(dBm + 150, 0, 255). GIS applies: real = pixel * scale + offset.
    // With scale=1, offset=-150: real = pixel - 150 = dBm. Nodata = pixel value 0.
    if is_8bit {
        main_entries.push(TagEntry::ascii(TAG_GDAL_METADATA,
                                          "<GDALMetadata>\n\
             <Item name=\"OFFSET\" sample=\"0\" role=\"offset\">-150`.0</Item>\n\
             <Item name=\"SCALE\" sample=\"0\" role=\"scale\">1</Item>\n\
             </GDALMetadata>"
        ));
        main_entries.push(TagEntry::ascii(TAG_GDAL_NODATA, "0"));
    }

    let (main_ifd_off, _main_next_fix) = write_ifd(&mut w, &mut main_entries, next_ifd_offset)?;

    // ── 8. Fixup header to point to main IFD ─────────────────────────────
    fixup_u64(&mut w, header_fixup, main_ifd_off)?;

    w.flush()?;
    prof.lap("IFDs written");

    // ── Summary ──────────────────────────────────────────────────────────
    let total = t_total.elapsed().as_secs_f64();
    let input_mb = (row_stride * height) as f64 / (1024.0 * 1024.0);
    let output_size = std::fs::metadata(&args.output)?.len();
    let output_mb = output_size as f64 / (1024.0 * 1024.0);
    let ratio = (row_stride * height) as f64 / output_size as f64;

    eprintln!(
        "[Export] SUCCESS | {:.2}s | {:.0} MB in → {:.0} MB out ({:.1}:1) | {:.0} MB/s",
        total, input_mb, output_mb, ratio, input_mb / total
    );

    Ok(())
}

/// Build standard TIFF tags for an image level.
//[8-BIT MODE] Added bits_per_sample parameter
fn build_image_tags(
    width: u32,
    height: u32,
    tile_size: u32,
    compression: u16,
    tile_info: &[(u64, u64)],
    is_overview: bool,
    bits_per_sample: u16,
) -> Vec<TagEntry> {
    let offsets: Vec<u64> = tile_info.iter().map(|t| t.0).collect();
    let sizes: Vec<u64> = tile_info.iter().map(|t| t.1).collect();

    let mut tags = vec![
        TagEntry::long(TAG_IMAGE_WIDTH, width),
        TagEntry::long(TAG_IMAGE_LENGTH, height),
        TagEntry::short(TAG_BITS_PER_SAMPLE, bits_per_sample), //[8-BIT MODE] Parametrized
        TagEntry::short(TAG_COMPRESSION, compression),
        TagEntry::short(TAG_PHOTOMETRIC, 1), // MINISBLACK
        TagEntry::short(TAG_SAMPLES_PER_PIXEL, 1),
        TagEntry::long(TAG_TILE_WIDTH, tile_size),
        TagEntry::long(TAG_TILE_LENGTH, tile_size),
        TagEntry::long8_array(TAG_TILE_OFFSETS, &offsets),
        TagEntry::long8_array(TAG_TILE_BYTE_COUNTS, &sizes),
        TagEntry::short(TAG_SAMPLE_FORMAT, 1), // Unsigned integer
    ];

    if is_overview {
        tags.push(TagEntry::long(TAG_NEW_SUBFILE_TYPE, 1)); // Reduced resolution
    }

    tags
}

/// Process and write all tiles for one image level.
/// If `overview_factor` is None, extracts tiles directly (memcpy path).
/// If Some(f), does nearest-neighbor sampling at factor f.
//[8-BIT MODE] Added is_8bit parameter to switch extraction logic
fn write_tiles(
    w: &mut (impl Write + Seek),
    mmap: &Mmap,
    row_stride: usize,
    src_width: usize,
    src_height: usize,
    ts: usize,
    level: u32,
    overview_factor: Option<usize>,
    is_8bit: bool,
) -> io::Result<ImageLevel> {
    let (img_w, img_h) = match overview_factor {
        None => (src_width, src_height),
        Some(f) => ((src_width + f - 1) / f, (src_height + f - 1) / f),
    };

    let tiles_x = (img_w + ts - 1) / ts;
    let tiles_y = (img_h + ts - 1) / ts;
    let total_tiles = tiles_x * tiles_y;

    let mut tile_info = vec![(0u64, 0u64); total_tiles];
    let use_compression = level > 0;
    let mut sparse_count = 0usize;

    // Process one tile-row at a time, tiles within a row in parallel
    for ty in 0..tiles_y {
        // ── Parallel: extract + compress tiles ───────────────────────────
        let row_tiles: Vec<Option<Vec<u8>>> = (0..tiles_x)
            .into_par_iter()
            .map(|tx| {
                //[8-BIT MODE] Route to format-specific extraction
                let raw = match (overview_factor, is_8bit) {
                    (None, false) => extract_tile_packed_1bit(mmap, row_stride, src_width, src_height, ts, tx, ty),
                    (None, true)  => extract_tile_packed_8bit(mmap, row_stride, src_width, src_height, ts, tx, ty),
                    (Some(f), false) => extract_overview_tile_1bit(mmap, row_stride, src_width, src_height, ts, tx, ty, f),
                    (Some(f), true)  => extract_overview_tile_8bit(mmap, row_stride, src_width, src_height, ts, tx, ty, f),
                };

                // Sparse optimization: skip all-zero tiles
                if is_all_zero(&raw) {
                    return None;
                }

                if use_compression {
                    Some(compress_tile(&raw, level))
                } else {
                    Some(raw)
                }
            })
            .collect();

        // ── Sequential: write to file ────────────────────────────────────
        for (tx, tile_data) in row_tiles.into_iter().enumerate() {
            let idx = ty * tiles_x + tx;

            match tile_data {
                None => {
                    // Sparse: offset=0, size=0
                    tile_info[idx] = (0, 0);
                    sparse_count += 1;
                }
                Some(data) => {
                    let offset = w.stream_position()?;
                    w.write_all(&data)?;
                    tile_info[idx] = (offset, data.len() as u64);
                }
            }
        }

        // Progress reporting for large images
        if tiles_y > 20 && (ty + 1) % (tiles_y / 10).max(1) == 0 {
            let pct = (ty + 1) as f64 / tiles_y as f64 * 100.0;
            let label = overview_factor.map_or("Main".to_string(), |f| format!("OVR {}×", f));
            eprintln!("[Export] {} {:.0}%", label, pct);
        }
    }

    if sparse_count > 0 {
        let pct = sparse_count as f64 / total_tiles as f64 * 100.0;
        eprintln!("[Export] Sparse: {}/{} tiles skipped ({:.0}%)", sparse_count, total_tiles, pct);
    }

    Ok(ImageLevel {
        width: img_w,
        height: img_h,
        tiles_x,
        tiles_y,
        tile_info,
    })
}

// ═══════════════════════════════════════════════════════════════════════════════
// Main
// ═══════════════════════════════════════════════════════════════════════════════

fn main() {
    let args = Args::parse();
    if let Err(e) = run(args) {
        eprintln!("[Export] FATAL: {}", e);
        std::process::exit(1);
    }
}