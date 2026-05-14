// =============================================================================
// export_geotiff_wasm — In-memory GeoTIFF writer for AETHER browser pipeline
// =============================================================================
//
// Takes raw GPU coverage output (1-bit LOS or 8-bit propagation loss) and
// produces a valid tiled GeoTIFF in memory. Regular TIFF (not BigTIFF) with
// Deflate compression and 256x256 tiles.
//
// The 1-bit data from the GPU is already MSB-first packed with byte-aligned
// rows. Since tile_size (256) is a multiple of 8, tile column boundaries are
// always byte-aligned — tile extraction is pure memcpy, no bit manipulation.

use flate2::write::ZlibEncoder;
use flate2::Compression;
use std::io::{self, Cursor, Seek, SeekFrom, Write};
use wasm_bindgen::prelude::*;

// ═══════════════════════════════════════════════════════════════════════════════
// TIFF Constants
// ═══════════════════════════════════════════════════════════════════════════════

const TIFF_LE: u16 = 0x4949; // Little-endian byte order
const TIFF_VERSION: u16 = 42; // Classic TIFF

// TIFF data types
const TY_SHORT: u16 = 3; // u16
const TY_LONG: u16 = 4;  // u32
const TY_DOUBLE: u16 = 12; // f64

// TIFF tags
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

// Compression code
const COMPRESS_DEFLATE: u16 = 8;

// Subfile type (for overview IFDs)
const TAG_NEW_SUBFILE_TYPE: u16 = 254;

// GeoKey IDs
const GK_MODEL_TYPE: u16 = 1024;
const GK_RASTER_TYPE: u16 = 1025;
const GK_GEOGRAPHIC_TYPE: u16 = 2048;

// Tile size for output (fixed at 256 for browser use)
const TILE_SIZE: usize = 256;

// ═══════════════════════════════════════════════════════════════════════════════
// IFD Entry
// ═══════════════════════════════════════════════════════════════════════════════

/// One TIFF IFD entry. In classic TIFF, each entry is 12 bytes:
///   tag(2) + type(2) + count(4) + value/offset(4)
/// If data fits in 4 bytes it is stored inline; otherwise an offset is used.
struct TagEntry {
    tag: u16,
    dtype: u16,
    count: u32,
    data: Vec<u8>,
    /// File offset where overflow data was written (filled during IFD write).
    overflow_offset: u32,
}

impl TagEntry {
    fn inline(&self) -> bool {
        self.data.len() <= 4
    }

    fn short(tag: u16, val: u16) -> Self {
        Self { tag, dtype: TY_SHORT, count: 1, data: val.to_le_bytes().to_vec(), overflow_offset: 0 }
    }

    fn long(tag: u16, val: u32) -> Self {
        Self { tag, dtype: TY_LONG, count: 1, data: val.to_le_bytes().to_vec(), overflow_offset: 0 }
    }

    fn long_array(tag: u16, vals: &[u32]) -> Self {
        let data: Vec<u8> = vals.iter().flat_map(|v| v.to_le_bytes()).collect();
        Self { tag, dtype: TY_LONG, count: vals.len() as u32, data, overflow_offset: 0 }
    }

    fn double_array(tag: u16, vals: &[f64]) -> Self {
        let data: Vec<u8> = vals.iter().flat_map(|v| v.to_le_bytes()).collect();
        Self { tag, dtype: TY_DOUBLE, count: vals.len() as u32, data, overflow_offset: 0 }
    }

    fn short_array(tag: u16, vals: &[u16]) -> Self {
        let data: Vec<u8> = vals.iter().flat_map(|v| v.to_le_bytes()).collect();
        Self { tag, dtype: TY_SHORT, count: vals.len() as u32, data, overflow_offset: 0 }
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// TIFF Writer Helpers
// ═══════════════════════════════════════════════════════════════════════════════

/// Write the classic TIFF 8-byte header. Returns the position of the
/// "offset to first IFD" field (byte 4) for later fixup.
fn write_tiff_header(w: &mut Cursor<Vec<u8>>) -> io::Result<u64> {
    w.write_all(&TIFF_LE.to_le_bytes())?;      // Byte order mark
    w.write_all(&TIFF_VERSION.to_le_bytes())?;  // TIFF magic 42
    let fixup_pos = w.stream_position()?;
    w.write_all(&0u32.to_le_bytes())?;          // Placeholder: offset to first IFD
    Ok(fixup_pos)
}

/// Write one IFD. Overflow data for entries that do not fit inline is written
/// immediately before the IFD directory itself.
/// Returns the file offset where the IFD directory starts.
fn write_ifd(w: &mut Cursor<Vec<u8>>, entries: &mut Vec<TagEntry>) -> io::Result<u32> {
    // Sort by tag number (TIFF requirement)
    entries.sort_by_key(|e| e.tag);

    // Write overflow data for entries that do not fit inline
    for entry in entries.iter_mut() {
        if !entry.inline() {
            let pos = w.stream_position()? as u32;
            entry.overflow_offset = pos;
            w.write_all(&entry.data)?;
        }
    }

    // Align to word boundary
    let pos = w.stream_position()?;
    if pos % 2 != 0 {
        w.write_all(&[0u8])?;
    }

    let ifd_offset = w.stream_position()? as u32;

    // Entry count (u16 for classic TIFF)
    w.write_all(&(entries.len() as u16).to_le_bytes())?;

    // Each entry: tag(2) + type(2) + count(4) + value/offset(4) = 12 bytes
    for entry in entries.iter() {
        w.write_all(&entry.tag.to_le_bytes())?;
        w.write_all(&entry.dtype.to_le_bytes())?;
        w.write_all(&entry.count.to_le_bytes())?;

        let mut value_buf = [0u8; 4];
        if entry.inline() {
            value_buf[..entry.data.len()].copy_from_slice(&entry.data);
        } else {
            value_buf = entry.overflow_offset.to_le_bytes();
        }
        w.write_all(&value_buf)?;
    }

    // Next IFD offset — placeholder, returns position for chaining
    let next_ifd_fixup = w.stream_position()?;
    w.write_all(&0u32.to_le_bytes())?;

    Ok(ifd_offset)
}

/// Write an IFD and return (ifd_offset, next_ifd_fixup_pos) for chaining.
fn write_ifd_chained(w: &mut Cursor<Vec<u8>>, entries: &mut Vec<TagEntry>) -> io::Result<(u32, u64)> {
    entries.sort_by_key(|e| e.tag);
    for entry in entries.iter_mut() {
        if !entry.inline() {
            let pos = w.stream_position()? as u32;
            entry.overflow_offset = pos;
            w.write_all(&entry.data)?;
        }
    }
    let pos = w.stream_position()?;
    if pos % 2 != 0 { w.write_all(&[0u8])?; }

    let ifd_offset = w.stream_position()? as u32;
    w.write_all(&(entries.len() as u16).to_le_bytes())?;
    for entry in entries.iter() {
        w.write_all(&entry.tag.to_le_bytes())?;
        w.write_all(&entry.dtype.to_le_bytes())?;
        w.write_all(&entry.count.to_le_bytes())?;
        let mut value_buf = [0u8; 4];
        if entry.inline() {
            value_buf[..entry.data.len()].copy_from_slice(&entry.data);
        } else {
            value_buf = entry.overflow_offset.to_le_bytes();
        }
        w.write_all(&value_buf)?;
    }
    let next_fixup = w.stream_position()?;
    w.write_all(&0u32.to_le_bytes())?; // placeholder for next IFD
    Ok((ifd_offset, next_fixup))
}

/// Overwrite a u32 value at a previously-recorded position.
fn fixup_u32(w: &mut Cursor<Vec<u8>>, pos: u64, val: u32) -> io::Result<()> {
    let cur = w.stream_position()?;
    w.seek(SeekFrom::Start(pos))?;
    w.write_all(&val.to_le_bytes())?;
    w.seek(SeekFrom::Start(cur))?;
    Ok(())
}

// ═══════════════════════════════════════════════════════════════════════════════
// Tile Extraction
// ═══════════════════════════════════════════════════════════════════════════════

/// Extract a 1-bit tile from the flat packed input. The input has byte-aligned
/// rows of ceil(width/8) bytes. Since TILE_SIZE is a multiple of 8, tile column
/// boundaries are byte-aligned — extraction is pure memcpy.
fn extract_tile_1bit(
    data: &[u8],
    row_stride: usize,
    width: usize,
    height: usize,
    tx: usize,
    ty: usize,
) -> Vec<u8> {
    let tile_row_bytes = TILE_SIZE / 8; // 32 bytes per tile row
    let mut buf = vec![0u8; tile_row_bytes * TILE_SIZE];

    let x0 = tx * TILE_SIZE;
    let y0 = ty * TILE_SIZE;
    let tw = std::cmp::min(TILE_SIZE, width.saturating_sub(x0));
    let th = std::cmp::min(TILE_SIZE, height.saturating_sub(y0));
    if tw == 0 || th == 0 {
        return buf;
    }

    let src_byte_col = x0 / 8;
    let full_bytes = tw / 8;
    let remainder_bits = tw % 8;
    let copy_bytes = full_bytes + if remainder_bits > 0 { 1 } else { 0 };

    for row in 0..th {
        let src_offset = (y0 + row) * row_stride + src_byte_col;
        let dst_offset = row * tile_row_bytes;

        if src_offset + copy_bytes > data.len() {
            break;
        }

        buf[dst_offset..dst_offset + copy_bytes]
            .copy_from_slice(&data[src_offset..src_offset + copy_bytes]);

        // Mask trailing bits in last byte if width doesn't end on byte boundary
        if remainder_bits > 0 {
            let mask: u8 = 0xFF << (8 - remainder_bits);
            buf[dst_offset + copy_bytes - 1] &= mask;
        }
    }

    buf
}

/// Extract an 8-bit tile (one byte per pixel) via direct memcpy.
fn extract_tile_8bit(
    data: &[u8],
    row_stride: usize,
    width: usize,
    height: usize,
    tx: usize,
    ty: usize,
) -> Vec<u8> {
    let mut buf = vec![0u8; TILE_SIZE * TILE_SIZE];

    let x0 = tx * TILE_SIZE;
    let y0 = ty * TILE_SIZE;
    let tw = std::cmp::min(TILE_SIZE, width.saturating_sub(x0));
    let th = std::cmp::min(TILE_SIZE, height.saturating_sub(y0));
    if tw == 0 || th == 0 {
        return buf;
    }

    for row in 0..th {
        let src_offset = (y0 + row) * row_stride + x0;
        let dst_offset = row * TILE_SIZE;

        if src_offset + tw > data.len() {
            break;
        }

        buf[dst_offset..dst_offset + tw]
            .copy_from_slice(&data[src_offset..src_offset + tw]);
    }

    buf
}

/// Check if a byte slice is entirely zero.
fn is_all_zero(data: &[u8]) -> bool {
    // Use u64 chunks for speed
    let (prefix, chunks, suffix) = unsafe { data.align_to::<u64>() };
    prefix.iter().all(|&b| b == 0)
        && chunks.iter().all(|&v| v == 0)
        && suffix.iter().all(|&b| b == 0)
}

/// Compress tile data with zlib (TIFF DEFLATE = zlib stream format).
fn compress_tile(data: &[u8]) -> Vec<u8> {
    let mut enc = ZlibEncoder::new(
        Vec::with_capacity(data.len() / 4),
        Compression::new(1), // level 1 = fast
    );
    enc.write_all(data).expect("zlib compress failed");
    enc.finish().expect("zlib finish failed")
}

// ═══════════════════════════════════════════════════════════════════════════════
// GeoTIFF Tags
// ═══════════════════════════════════════════════════════════════════════════════

/// Build the GeoTIFF-specific IFD entries for EPSG:4326 (WGS84 geographic).
fn build_geo_tags(geotransform: &[f64]) -> Vec<TagEntry> {
    // geotransform = [originX, pixelWidth, 0, originY, 0, -pixelHeight]
    let pixel_scale_x = geotransform[1].abs();
    let pixel_scale_y = geotransform[5].abs();

    let mut tags = Vec::new();

    // ModelPixelScaleTag: [scaleX, scaleY, scaleZ]
    tags.push(TagEntry::double_array(TAG_MODEL_PIXEL_SCALE, &[
        pixel_scale_x, pixel_scale_y, 0.0,
    ]));

    // ModelTiepointTag: [I, J, K, X, Y, Z] — maps pixel (0,0) to geo origin
    tags.push(TagEntry::double_array(TAG_MODEL_TIEPOINT, &[
        0.0, 0.0, 0.0,
        geotransform[0], geotransform[3], 0.0,
    ]));

    // GeoKeyDirectoryTag for EPSG:4326 (WGS84 Geographic)
    // Header: version=1, revision=1, minor=0, num_keys=3
    let keys: Vec<u16> = vec![
        1, 1, 0, 3,                          // Header
        GK_MODEL_TYPE, 0, 1, 2,              // ModelTypeGeographic
        GK_RASTER_TYPE, 0, 1, 1,             // RasterPixelIsArea
        GK_GEOGRAPHIC_TYPE, 0, 1, 4326,      // EPSG:4326
    ];
    tags.push(TagEntry::short_array(TAG_GEO_KEY_DIRECTORY, &keys));

    tags
}

// ═══════════════════════════════════════════════════════════════════════════════
// Overview (pyramid) helpers
// ═══════════════════════════════════════════════════════════════════════════════

/// Extract an 8-bit overview tile by nearest-neighbor sampling from the full-res data.
fn extract_overview_tile_8bit(
    data: &[u8], row_stride: usize, full_w: usize, full_h: usize,
    tx: usize, ty: usize, factor: usize,
) -> Vec<u8> {
    let mut buf = vec![0u8; TILE_SIZE * TILE_SIZE];
    let ovr_x0 = tx * TILE_SIZE;
    let ovr_y0 = ty * TILE_SIZE;
    let ovr_w = (full_w + factor - 1) / factor;
    let ovr_h = (full_h + factor - 1) / factor;
    let tw = std::cmp::min(TILE_SIZE, ovr_w.saturating_sub(ovr_x0));
    let th = std::cmp::min(TILE_SIZE, ovr_h.saturating_sub(ovr_y0));
    for row in 0..th {
        let src_y = (ovr_y0 + row) * factor;
        if src_y >= full_h { break; }
        for col in 0..tw {
            let src_x = (ovr_x0 + col) * factor;
            if src_x >= full_w { continue; }
            let src_off = src_y * row_stride + src_x;
            if src_off < data.len() {
                buf[row * TILE_SIZE + col] = data[src_off];
            }
        }
    }
    buf
}

/// Extract a 1-bit overview tile by nearest-neighbor sampling from the full-res data.
fn extract_overview_tile_1bit(
    data: &[u8], row_stride: usize, full_w: usize, full_h: usize,
    tx: usize, ty: usize, factor: usize,
) -> Vec<u8> {
    // Output tile is 1-bit packed, TILE_SIZE×TILE_SIZE
    let tile_row_bytes = TILE_SIZE / 8;
    let mut buf = vec![0u8; tile_row_bytes * TILE_SIZE];
    let ovr_x0 = tx * TILE_SIZE;
    let ovr_y0 = ty * TILE_SIZE;
    let ovr_w = (full_w + factor - 1) / factor;
    let ovr_h = (full_h + factor - 1) / factor;
    let tw = std::cmp::min(TILE_SIZE, ovr_w.saturating_sub(ovr_x0));
    let th = std::cmp::min(TILE_SIZE, ovr_h.saturating_sub(ovr_y0));
    for row in 0..th {
        let src_y = (ovr_y0 + row) * factor;
        if src_y >= full_h { break; }
        for col in 0..tw {
            let src_x = (ovr_x0 + col) * factor;
            if src_x >= full_w { continue; }
            // Read source bit
            let src_byte = src_y * row_stride + src_x / 8;
            let src_bit = 7 - (src_x % 8);
            if src_byte < data.len() && (data[src_byte] >> src_bit) & 1 == 1 {
                let dst_byte = row * tile_row_bytes + col / 8;
                let dst_bit = 7 - (col % 8);
                buf[dst_byte] |= 1 << dst_bit;
            }
        }
    }
    buf
}

// ═══════════════════════════════════════════════════════════════════════════════
// Public WASM API
// ═══════════════════════════════════════════════════════════════════════════════

/// Convert raw GPU coverage output to a GeoTIFF byte array.
///
/// # Arguments
/// * `data` - Raw raster bytes from GPU (1-bit packed or 8-bit per pixel)
/// * `width` - Raster width in pixels
/// * `height` - Raster height in pixels
/// * `format` - `"1BIT_LOS"` or `"8BIT_PROP"`
/// * `geotransform` - 6-element GDAL-style geotransform array
///
/// # Returns
/// Complete GeoTIFF file contents as a byte array.
#[wasm_bindgen]
pub fn convert_to_geotiff(
    data: &[u8],
    width: u32,
    height: u32,
    format: &str,
    geotransform: Vec<f64>,
) -> Vec<u8> {
    let w = width as usize;
    let h = height as usize;
    let is_8bit = format != "1BIT_LOS";

    // Row stride in the input data
    let row_stride = if is_8bit {
        w
    } else {
        // 1-bit: rows byte-aligned, DWORD-padded (matches aether_core convention)
        ((w + 31) / 32) * 4
    };

    let bits_per_sample: u16 = if is_8bit { 8 } else { 1 };

    // Tile grid dimensions
    let tiles_x = (w + TILE_SIZE - 1) / TILE_SIZE;
    let tiles_y = (h + TILE_SIZE - 1) / TILE_SIZE;
    let total_tiles = tiles_x * tiles_y;

    // Estimate output size: input size + overhead for headers/IFD
    let estimated_size = data.len() + 4096;
    let mut cursor = Cursor::new(Vec::with_capacity(estimated_size));

    // ── 1. Write TIFF header ────────────────────────────────────────────
    let header_fixup = write_tiff_header(&mut cursor)
        .expect("failed to write TIFF header");

    // ── 2. Extract, compress, and write tiles ───────────────────────────
    let mut tile_offsets: Vec<u32> = vec![0; total_tiles];
    let mut tile_byte_counts: Vec<u32> = vec![0; total_tiles];

    for ty in 0..tiles_y {
        for tx in 0..tiles_x {
            let idx = ty * tiles_x + tx;

            let raw = if is_8bit {
                extract_tile_8bit(data, row_stride, w, h, tx, ty)
            } else {
                extract_tile_1bit(data, row_stride, w, h, tx, ty)
            };

            // Sparse optimization: skip all-zero tiles
            if is_all_zero(&raw) {
                // offset=0, size=0 signals empty tile to TIFF readers
                continue;
            }

            let compressed = compress_tile(&raw);

            let offset = cursor.stream_position().expect("stream_position") as u32;
            cursor.write_all(&compressed).expect("write tile data");

            tile_offsets[idx] = offset;
            tile_byte_counts[idx] = compressed.len() as u32;
        }
    }

    // ── 3. Build IFD entries ────────────────────────────────────────────
    let mut entries: Vec<TagEntry> = vec![
        TagEntry::long(TAG_IMAGE_WIDTH, width),
        TagEntry::long(TAG_IMAGE_LENGTH, height),
        TagEntry::short(TAG_BITS_PER_SAMPLE, bits_per_sample),
        TagEntry::short(TAG_COMPRESSION, COMPRESS_DEFLATE),
        TagEntry::short(TAG_PHOTOMETRIC, 1), // MINISBLACK
        TagEntry::short(TAG_SAMPLES_PER_PIXEL, 1),
        TagEntry::long(TAG_TILE_WIDTH, TILE_SIZE as u32),
        TagEntry::long(TAG_TILE_LENGTH, TILE_SIZE as u32),
        TagEntry::long_array(TAG_TILE_OFFSETS, &tile_offsets),
        TagEntry::long_array(TAG_TILE_BYTE_COUNTS, &tile_byte_counts),
        TagEntry::short(TAG_SAMPLE_FORMAT, 1), // Unsigned integer
    ];

    // GeoTIFF tags
    if geotransform.len() == 6 {
        entries.extend(build_geo_tags(&geotransform));
    }

    // ── 4. Write main IFD ─────────────────────────────────────────────
    let (ifd_offset, mut prev_next_fixup) = write_ifd_chained(&mut cursor, &mut entries)
        .expect("failed to write IFD");

    // ── 5. Fixup header to point to the main IFD ────────────────────
    fixup_u32(&mut cursor, header_fixup, ifd_offset)
        .expect("failed to fixup header");

    // ── 6. Write overview IFDs (pyramids) ───────────────────────────
    // Generate overviews at 4×, 16×, 64× ... until image is ≤256px wide.
    if w > 2048 {
        let mut factor = 4usize;
        while w / factor > 256 || factor == 4 {
            let ovr_w = (w + factor - 1) / factor;
            let ovr_h = (h + factor - 1) / factor;
            let ovr_tiles_x = (ovr_w + TILE_SIZE - 1) / TILE_SIZE;
            let ovr_tiles_y = (ovr_h + TILE_SIZE - 1) / TILE_SIZE;
            let ovr_total = ovr_tiles_x * ovr_tiles_y;

            let mut ovr_offsets: Vec<u32> = vec![0; ovr_total];
            let mut ovr_counts: Vec<u32> = vec![0; ovr_total];

            for oty in 0..ovr_tiles_y {
                for otx in 0..ovr_tiles_x {
                    let idx = oty * ovr_tiles_x + otx;
                    let raw = if is_8bit {
                        extract_overview_tile_8bit(data, row_stride, w, h, otx, oty, factor)
                    } else {
                        extract_overview_tile_1bit(data, row_stride, w, h, otx, oty, factor)
                    };
                    if is_all_zero(&raw) { continue; }
                    let compressed = compress_tile(&raw);
                    let offset = cursor.stream_position().expect("pos") as u32;
                    cursor.write_all(&compressed).expect("write ovr tile");
                    ovr_offsets[idx] = offset;
                    ovr_counts[idx] = compressed.len() as u32;
                }
            }

            let mut ovr_entries: Vec<TagEntry> = vec![
                TagEntry::long(TAG_NEW_SUBFILE_TYPE, 1), // reduced-resolution image
                TagEntry::long(TAG_IMAGE_WIDTH, ovr_w as u32),
                TagEntry::long(TAG_IMAGE_LENGTH, ovr_h as u32),
                TagEntry::short(TAG_BITS_PER_SAMPLE, bits_per_sample),
                TagEntry::short(TAG_COMPRESSION, COMPRESS_DEFLATE),
                TagEntry::short(TAG_PHOTOMETRIC, 1),
                TagEntry::short(TAG_SAMPLES_PER_PIXEL, 1),
                TagEntry::long(TAG_TILE_WIDTH, TILE_SIZE as u32),
                TagEntry::long(TAG_TILE_LENGTH, TILE_SIZE as u32),
                TagEntry::long_array(TAG_TILE_OFFSETS, &ovr_offsets),
                TagEntry::long_array(TAG_TILE_BYTE_COUNTS, &ovr_counts),
                TagEntry::short(TAG_SAMPLE_FORMAT, 1),
            ];

            let (ovr_ifd, ovr_next_fixup) = write_ifd_chained(&mut cursor, &mut ovr_entries)
                .expect("failed to write overview IFD");

            // Chain previous IFD → this overview IFD
            fixup_u32(&mut cursor, prev_next_fixup, ovr_ifd)
                .expect("failed to chain overview IFD");
            prev_next_fixup = ovr_next_fixup;

            factor *= 4;
            if ovr_w <= 256 { break; }
        }
    }

    cursor.into_inner()
}
