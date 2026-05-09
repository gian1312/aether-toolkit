use aether_aggregate::reader::InputRaster;
use std::io::Write;
use std::path::Path;

/// Helper: write a synthetic .bit file and its JSON sidecar.
///
/// Layout:
///   [tile_data bytes]
///   [tile index: tx(u32) ty(u32) offset(u64) size(u32) per entry]
///   [footer 24 bytes: index_offset(u64) tile_count(u64) "ATIL" reserved(u32)]
fn write_bit_file(
    dir: &Path,
    name: &str,
    tile_size: usize,
    width: usize,
    height: usize,
    output_format: &str,
    tile_data: &[u8],
) {
    let bit_path = dir.join(format!("{}.bit", name));
    let json_path = dir.join(format!("{}.json", name));

    // Tile data offset in the file is 0, size is tile_data.len()
    let tile_data_len = tile_data.len() as u32;
    let index_offset = tile_data.len() as u64;
    let tile_count: u64 = 1;

    let mut file = std::fs::File::create(&bit_path).unwrap();
    // Write tile data
    file.write_all(tile_data).unwrap();
    // Write tile index entry: tx=0, ty=0, offset=0, size=tile_data_len
    file.write_all(&0u32.to_le_bytes()).unwrap(); // tx
    file.write_all(&0u32.to_le_bytes()).unwrap(); // ty
    file.write_all(&0u64.to_le_bytes()).unwrap(); // offset
    file.write_all(&tile_data_len.to_le_bytes()).unwrap(); // size
    // Write footer
    file.write_all(&index_offset.to_le_bytes()).unwrap(); // index_offset
    file.write_all(&tile_count.to_le_bytes()).unwrap(); // tile_count
    file.write_all(b"ATIL").unwrap(); // magic
    file.write_all(&0u32.to_le_bytes()).unwrap(); // reserved
    file.flush().unwrap();

    // Write JSON sidecar
    let sidecar = format!(
        r#"{{"dimensions":{{"width":{},"height":{}}},"geotransform":[8.0,0.001,0.0,47.0,0.0,-0.001],"output_format":"{}","tile_size":{}}}"#,
        width, height, output_format, tile_size
    );
    std::fs::write(&json_path, sidecar).unwrap();
}

#[test]
fn test_open_bit_8bit() {
    let dir = tempfile::tempdir().unwrap();

    // 8x8 tile filled with value 42
    let tile_data = vec![42u8; 64];
    write_bit_file(dir.path(), "test8bit", 8, 8, 8, "8BIT_PROP", &tile_data);

    let raster = InputRaster::open(&dir.path().join("test8bit.bit")).unwrap();
    assert_eq!(raster.width, 8);
    assert_eq!(raster.height, 8);

    let pixels = raster.read_region_u8(0, 0, 8, 8);
    assert_eq!(pixels.len(), 64);
    for &v in &pixels {
        assert_eq!(v, 42, "Expected all pixels to be 42");
    }
}

#[test]
fn test_open_bit_1bit() {
    let dir = tempfile::tempdir().unwrap();

    // 1-bit tile: 8 rows, each row is 1 byte (8 pixels packed MSB-first).
    // Row byte layout for tile_size=8: row_bytes = (8+7)/8 = 1 byte per row
    // Set specific patterns:
    //   Row 0: 0b10101010 => pixels: 1,0,1,0,1,0,1,0
    //   Row 1: 0b11110000 => pixels: 1,1,1,1,0,0,0,0
    //   Row 2-7: 0x00 => all zeros
    let mut tile_data = vec![0u8; 8];
    tile_data[0] = 0b10101010;
    tile_data[1] = 0b11110000;

    write_bit_file(dir.path(), "test1bit", 8, 8, 8, "1BIT_LOS", &tile_data);

    let raster = InputRaster::open(&dir.path().join("test1bit.bit")).unwrap();
    assert_eq!(raster.width, 8);
    assert_eq!(raster.height, 8);

    let pixels = raster.read_region_u8(0, 0, 8, 8);
    assert_eq!(pixels.len(), 64);

    // Row 0: 1,0,1,0,1,0,1,0
    assert_eq!(pixels[0], 1);
    assert_eq!(pixels[1], 0);
    assert_eq!(pixels[2], 1);
    assert_eq!(pixels[3], 0);
    assert_eq!(pixels[4], 1);
    assert_eq!(pixels[5], 0);
    assert_eq!(pixels[6], 1);
    assert_eq!(pixels[7], 0);

    // Row 1: 1,1,1,1,0,0,0,0
    assert_eq!(pixels[8], 1);
    assert_eq!(pixels[9], 1);
    assert_eq!(pixels[10], 1);
    assert_eq!(pixels[11], 1);
    assert_eq!(pixels[12], 0);
    assert_eq!(pixels[13], 0);
    assert_eq!(pixels[14], 0);
    assert_eq!(pixels[15], 0);

    // Rows 2-7: all zeros
    for i in 16..64 {
        assert_eq!(pixels[i], 0, "pixel {} should be 0", i);
    }
}

#[test]
fn test_open_unsupported_extension() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.xyz");
    std::fs::write(&path, b"dummy").unwrap();

    let result = InputRaster::open(&path);
    assert!(result.is_err(), "Opening .xyz should fail");
    let err_msg = match result {
        Err(e) => e.to_string(),
        Ok(_) => panic!("Expected error"),
    };
    assert!(
        err_msg.contains("Unsupported"),
        "Error should mention unsupported format, got: {}",
        err_msg
    );
}

#[test]
fn test_open_bit_missing_sidecar() {
    let dir = tempfile::tempdir().unwrap();

    // Create a .bit file without its .json sidecar
    let bit_path = dir.path().join("nosidecar.bit");
    let mut file = std::fs::File::create(&bit_path).unwrap();
    // Write minimal content: 24-byte footer with ATIL magic
    let index_offset: u64 = 0;
    let tile_count: u64 = 0;
    file.write_all(&index_offset.to_le_bytes()).unwrap();
    file.write_all(&tile_count.to_le_bytes()).unwrap();
    file.write_all(b"ATIL").unwrap();
    file.write_all(&0u32.to_le_bytes()).unwrap();
    file.flush().unwrap();

    let result = InputRaster::open(&bit_path);
    assert!(result.is_err(), "Opening .bit without sidecar should fail");
    let err_msg = match result {
        Err(e) => e.to_string(),
        Ok(_) => panic!("Expected error"),
    };
    assert!(
        err_msg.contains("sidecar") || err_msg.contains("Missing"),
        "Error should mention missing sidecar, got: {}",
        err_msg
    );
}

#[test]
fn test_read_region_zeros() {
    let dir = tempfile::tempdir().unwrap();

    // 8x8 tile filled with zeros
    let tile_data = vec![0u8; 64];
    write_bit_file(dir.path(), "zeros", 8, 8, 8, "8BIT_PROP", &tile_data);

    let raster = InputRaster::open(&dir.path().join("zeros.bit")).unwrap();
    let pixels = raster.read_region_u8(0, 0, 8, 8);
    assert_eq!(pixels.len(), 64);
    for &v in &pixels {
        assert_eq!(v, 0, "Expected all pixels to be 0");
    }
}

#[test]
fn test_open_bit_missing_atil() {
    let dir = tempfile::tempdir().unwrap();

    let bit_path = dir.path().join("nomagic.bit");
    let json_path = dir.path().join("nomagic.json");

    // Write sidecar
    let sidecar = r#"{"dimensions":{"width":8,"height":8},"geotransform":[8.0,0.001,0.0,47.0,0.0,-0.001],"output_format":"8BIT_PROP","tile_size":8}"#;
    std::fs::write(&json_path, sidecar).unwrap();

    // Write .bit file with wrong magic (NOT "ATIL")
    let mut file = std::fs::File::create(&bit_path).unwrap();
    let index_offset: u64 = 0;
    let tile_count: u64 = 0;
    file.write_all(&index_offset.to_le_bytes()).unwrap();
    file.write_all(&tile_count.to_le_bytes()).unwrap();
    file.write_all(b"NOPE").unwrap(); // wrong magic
    file.write_all(&0u32.to_le_bytes()).unwrap();
    file.flush().unwrap();

    let result = InputRaster::open(&bit_path);
    assert!(result.is_err(), "Opening .bit without ATIL magic should fail");
    let err_msg = match result {
        Err(e) => e.to_string(),
        Ok(_) => panic!("Expected error"),
    };
    assert!(
        err_msg.contains("ATIL"),
        "Error should mention missing ATIL magic, got: {}",
        err_msg
    );
}

// =============================================================================
// TIFF Reader Tests
// =============================================================================

/// Helper: create a minimal Classic TIFF file with uncompressed, tiled, 8-bit data.
///
/// `data` should contain `width * height` bytes (row-major), which are distributed
/// across tiles of size `tile_w x tile_h`.  The TIFF layout is:
///
///   Header (8 bytes)  ->  Tile data  ->  Data areas (offsets/sizes arrays, geo tags)  ->  IFD
///
fn create_test_tiff(
    path: &std::path::Path,
    width: u32,
    height: u32,
    tile_w: u32,
    tile_h: u32,
    data: &[u8],
) {
    create_test_tiff_ex(path, width, height, tile_w, tile_h, data, 8, None, None);
}

/// Extended TIFF helper supporting BPS, geotransform tags, and 16-bit data.
fn create_test_tiff_ex(
    path: &std::path::Path,
    width: u32,
    height: u32,
    tile_w: u32,
    tile_h: u32,
    data: &[u8],
    bps: u16,
    pixel_scale: Option<[f64; 3]>,
    tiepoint: Option<[f64; 6]>,
) {
    let bytes_per_sample = if bps <= 8 { 1usize } else { (bps as usize + 7) / 8 };
    let tiles_x = ((width + tile_w - 1) / tile_w) as usize;
    let tiles_y = ((height + tile_h - 1) / tile_h) as usize;
    let n_tiles = tiles_x * tiles_y;
    let tile_bytes = tile_w as usize * tile_h as usize * bytes_per_sample;

    // Build tile data: for each tile, extract pixels from the row-major image data
    let mut tile_blobs: Vec<Vec<u8>> = Vec::with_capacity(n_tiles);
    for ty in 0..tiles_y {
        for tx in 0..tiles_x {
            let mut tile = vec![0u8; tile_bytes];
            for row in 0..tile_h as usize {
                let img_y = ty * tile_h as usize + row;
                if img_y >= height as usize { continue; }
                for col in 0..tile_w as usize {
                    let img_x = tx * tile_w as usize + col;
                    if img_x >= width as usize { continue; }
                    let src_idx = (img_y * width as usize + img_x) * bytes_per_sample;
                    let dst_idx = (row * tile_w as usize + col) * bytes_per_sample;
                    for b in 0..bytes_per_sample {
                        if src_idx + b < data.len() {
                            tile[dst_idx + b] = data[src_idx + b];
                        }
                    }
                }
            }
            tile_blobs.push(tile);
        }
    }

    // Layout: header(8) + tile_data + data_area + IFD
    let header_size = 8usize;
    let tile_data_start = header_size;
    let mut tile_offsets: Vec<u32> = Vec::with_capacity(n_tiles);
    let mut tile_sizes: Vec<u32> = Vec::with_capacity(n_tiles);
    let mut cursor = tile_data_start;
    for blob in &tile_blobs {
        tile_offsets.push(cursor as u32);
        tile_sizes.push(blob.len() as u32);
        cursor += blob.len();
    }

    // Data area for arrays that don't fit inline (tile offsets, tile sizes, geo tags)
    let data_area_start = cursor;
    let mut data_area: Vec<u8> = Vec::new();

    // Tile offsets array (LONG, 4 bytes each) -- only needed if n_tiles > 1
    let tileoff_data_offset = data_area_start + data_area.len();
    if n_tiles > 1 {
        for &off in &tile_offsets {
            data_area.extend_from_slice(&off.to_le_bytes());
        }
    }
    // Tile byte counts array (LONG, 4 bytes each) -- only needed if n_tiles > 1
    let tilesz_data_offset = data_area_start + data_area.len();
    if n_tiles > 1 {
        for &sz in &tile_sizes {
            data_area.extend_from_slice(&sz.to_le_bytes());
        }
    }

    // GeoTIFF tag data (optional)
    let pscale_data_offset = data_area_start + data_area.len();
    if let Some(ps) = pixel_scale {
        for &v in &ps {
            data_area.extend_from_slice(&v.to_le_bytes());
        }
    }
    let tiepoint_data_offset = data_area_start + data_area.len();
    if let Some(tp) = tiepoint {
        for &v in &tp {
            data_area.extend_from_slice(&v.to_le_bytes());
        }
    }

    // IFD starts after data area
    let ifd_offset = data_area_start + data_area.len();

    // Build IFD entries: (tag, dtype, count, value_or_offset)
    let mut tags: Vec<(u16, u16, u32, u32)> = Vec::new();

    tags.push((256, 4, 1, width));           // WIDTH
    tags.push((257, 4, 1, height));          // HEIGHT
    tags.push((258, 3, 1, bps as u32));      // BPS
    tags.push((259, 3, 1, 1));               // COMPRESS = none
    tags.push((322, 4, 1, tile_w));          // TILEW
    tags.push((323, 4, 1, tile_h));          // TILEH
    // TILEOFF(324)
    if n_tiles == 1 {
        tags.push((324, 4, 1, tile_offsets[0]));
    } else {
        tags.push((324, 4, n_tiles as u32, tileoff_data_offset as u32));
    }
    // TILESZ(325)
    if n_tiles == 1 {
        tags.push((325, 4, 1, tile_sizes[0]));
    } else {
        tags.push((325, 4, n_tiles as u32, tilesz_data_offset as u32));
    }
    // SampleFormat(339): unsigned int
    tags.push((339, 3, 1, 1));

    if pixel_scale.is_some() {
        tags.push((33550, 12, 3, pscale_data_offset as u32));
    }
    if tiepoint.is_some() {
        tags.push((33922, 12, 6, tiepoint_data_offset as u32));
    }

    tags.sort_by_key(|t| t.0);
    let n_tags = tags.len() as u16;

    // Write the file
    let mut file = std::fs::File::create(path).unwrap();

    // Header
    file.write_all(&0x4949u16.to_le_bytes()).unwrap();
    file.write_all(&42u16.to_le_bytes()).unwrap();
    file.write_all(&(ifd_offset as u32).to_le_bytes()).unwrap();

    // Tile data
    for blob in &tile_blobs {
        file.write_all(blob).unwrap();
    }

    // Data area
    file.write_all(&data_area).unwrap();

    // IFD
    file.write_all(&n_tags.to_le_bytes()).unwrap();
    for &(tag, dtype, count, value) in &tags {
        file.write_all(&tag.to_le_bytes()).unwrap();
        file.write_all(&dtype.to_le_bytes()).unwrap();
        file.write_all(&count.to_le_bytes()).unwrap();
        file.write_all(&value.to_le_bytes()).unwrap();
    }
    file.write_all(&0u32.to_le_bytes()).unwrap(); // next_ifd = 0
    file.flush().unwrap();
}

/// Helper: create a minimal strip-based TIFF (no tile tags).
fn create_strip_tiff(path: &std::path::Path, width: u32, height: u32, data: &[u8]) {
    let header_size = 8usize;
    let strip_data_start = header_size;

    let ifd_offset = strip_data_start + data.len();

    let mut tags: Vec<(u16, u16, u32, u32)> = Vec::new();
    tags.push((256, 4, 1, width));                        // WIDTH
    tags.push((257, 4, 1, height));                       // HEIGHT
    tags.push((258, 3, 1, 8));                            // BPS = 8
    tags.push((259, 3, 1, 1));                            // COMPRESS = none
    tags.push((273, 4, 1, strip_data_start as u32));      // StripOffsets
    tags.push((278, 4, 1, height));                       // RowsPerStrip
    tags.push((279, 4, 1, data.len() as u32));            // StripByteCounts

    tags.sort_by_key(|t| t.0);
    let n_tags = tags.len() as u16;

    let mut file = std::fs::File::create(path).unwrap();
    file.write_all(&0x4949u16.to_le_bytes()).unwrap();
    file.write_all(&42u16.to_le_bytes()).unwrap();
    file.write_all(&(ifd_offset as u32).to_le_bytes()).unwrap();
    file.write_all(data).unwrap();

    file.write_all(&n_tags.to_le_bytes()).unwrap();
    for &(tag, dtype, count, value) in &tags {
        file.write_all(&tag.to_le_bytes()).unwrap();
        file.write_all(&dtype.to_le_bytes()).unwrap();
        file.write_all(&count.to_le_bytes()).unwrap();
        file.write_all(&value.to_le_bytes()).unwrap();
    }
    file.write_all(&0u32.to_le_bytes()).unwrap();
    file.flush().unwrap();
}

#[test]
fn test_open_tif_uncompressed_8bit() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test_8bit.tif");

    let data = vec![42u8; 16 * 16];
    create_test_tiff(&path, 16, 16, 16, 16, &data);

    let raster = InputRaster::open(&path).unwrap();
    assert_eq!(raster.width, 16);
    assert_eq!(raster.height, 16);

    let pixels = raster.read_region_u8(0, 0, 16, 16);
    assert_eq!(pixels.len(), 256);
    for (i, &v) in pixels.iter().enumerate() {
        assert_eq!(v, 42, "pixel {} should be 42, got {}", i, v);
    }
}

#[test]
fn test_open_tif_reads_geotransform() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test_geo.tif");

    let data = vec![0u8; 16 * 16];
    let pixel_scale = [0.5, 0.5, 0.0];
    let tiepoint = [0.0, 0.0, 0.0, 100.0, 200.0, 0.0];

    create_test_tiff_ex(
        &path, 16, 16, 16, 16, &data, 8,
        Some(pixel_scale),
        Some(tiepoint),
    );

    let raster = InputRaster::open(&path).unwrap();

    // geotransform = [tp[3], ps[0], 0.0, tp[4], 0.0, -ps[1]]
    let gt = raster.geotransform;
    assert!(
        (gt[0] - 100.0).abs() < 1e-10,
        "gt[0] x_origin should be 100.0, got {}", gt[0]
    );
    assert!(
        (gt[1] - 0.5).abs() < 1e-10,
        "gt[1] dx should be 0.5, got {}", gt[1]
    );
    assert!(
        gt[2].abs() < 1e-10,
        "gt[2] should be 0.0, got {}", gt[2]
    );
    assert!(
        (gt[3] - 200.0).abs() < 1e-10,
        "gt[3] y_origin should be 200.0, got {}", gt[3]
    );
    assert!(
        gt[4].abs() < 1e-10,
        "gt[4] should be 0.0, got {}", gt[4]
    );
    assert!(
        (gt[5] - (-0.5)).abs() < 1e-10,
        "gt[5] dy should be -0.5, got {}", gt[5]
    );
}

#[test]
fn test_open_tif_multi_tile() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test_multi.tif");

    // 32x32 image, 16x16 tiles => 2x2 tile grid
    let mut data = vec![0u8; 32 * 32];
    for y in 0..32usize {
        for x in 0..32usize {
            data[y * 32 + x] = match (x >= 16, y >= 16) {
                (false, false) => 10,
                (true, false)  => 20,
                (false, true)  => 30,
                (true, true)   => 40,
            };
        }
    }

    create_test_tiff(&path, 32, 32, 16, 16, &data);

    let raster = InputRaster::open(&path).unwrap();
    assert_eq!(raster.width, 32);
    assert_eq!(raster.height, 32);

    let pixels = raster.read_region_u8(0, 0, 32, 32);
    assert_eq!(pixels.len(), 1024);

    // Check corners of each quadrant
    assert_eq!(pixels[0 * 32 + 0], 10, "top-left corner should be 10");
    assert_eq!(pixels[0 * 32 + 15], 10, "top-left last col should be 10");
    assert_eq!(pixels[0 * 32 + 16], 20, "top-right first col should be 20");
    assert_eq!(pixels[0 * 32 + 31], 20, "top-right last col should be 20");
    assert_eq!(pixels[16 * 32 + 0], 30, "bottom-left first col should be 30");
    assert_eq!(pixels[16 * 32 + 15], 30, "bottom-left last col should be 30");
    assert_eq!(pixels[16 * 32 + 16], 40, "bottom-right first col should be 40");
    assert_eq!(pixels[31 * 32 + 31], 40, "bottom-right last pixel should be 40");

    // Check tile boundary pixels
    assert_eq!(pixels[15 * 32 + 15], 10, "last pixel of tile(0,0)");
    assert_eq!(pixels[15 * 32 + 16], 20, "first pixel of tile(1,0) in same row");
}

#[test]
fn test_open_tif_partial_region() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test_partial.tif");

    // 32x32 image with 16x16 tiles, same quadrant layout
    let mut data = vec![0u8; 32 * 32];
    for y in 0..32usize {
        for x in 0..32usize {
            data[y * 32 + x] = match (x >= 16, y >= 16) {
                (false, false) => 10,
                (true, false)  => 20,
                (false, true)  => 30,
                (true, true)   => 40,
            };
        }
    }

    create_test_tiff(&path, 32, 32, 16, 16, &data);
    let raster = InputRaster::open(&path).unwrap();

    // Read a 16x16 region starting at (8, 8) -- spans all 4 tiles
    let pixels = raster.read_region_u8(8, 8, 16, 16);
    assert_eq!(pixels.len(), 256);

    // Within the 16x16 sub-region:
    //   rows 0-7, cols 0-7   => tile(0,0) => 10
    //   rows 0-7, cols 8-15  => tile(1,0) => 20
    //   rows 8-15, cols 0-7  => tile(0,1) => 30
    //   rows 8-15, cols 8-15 => tile(1,1) => 40
    assert_eq!(pixels[0 * 16 + 0], 10, "sub-region top-left should be 10");
    assert_eq!(pixels[0 * 16 + 7], 10, "sub-region top-left edge should be 10");
    assert_eq!(pixels[0 * 16 + 8], 20, "sub-region top-right should be 20");
    assert_eq!(pixels[0 * 16 + 15], 20, "sub-region top-right edge should be 20");
    assert_eq!(pixels[8 * 16 + 0], 30, "sub-region bottom-left should be 30");
    assert_eq!(pixels[8 * 16 + 7], 30, "sub-region bottom-left edge should be 30");
    assert_eq!(pixels[8 * 16 + 8], 40, "sub-region bottom-right should be 40");
    assert_eq!(pixels[15 * 16 + 15], 40, "sub-region bottom-right corner should be 40");
}

#[test]
fn test_open_not_tiled() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("strip_based.tif");

    let data = vec![0u8; 16 * 16];
    create_strip_tiff(&path, 16, 16, &data);

    let result = InputRaster::open(&path);
    assert!(result.is_err(), "Opening strip-based TIFF should fail");
    let err_msg = match result {
        Err(e) => e.to_string(),
        Ok(_) => panic!("Expected error for non-tiled TIFF"),
    };
    assert!(
        err_msg.contains("Not a tiled TIFF"),
        "Error should say 'Not a tiled TIFF', got: {}",
        err_msg
    );
}

#[test]
fn test_open_tif_16bit() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test_16bit.tif");

    // 16-bit data: i16 values. extract_pixel_u8 clamps: <=0 -> 0, else min(raw,255).
    let width = 4u32;
    let height = 2u32;
    let test_values: Vec<i16> = vec![0, 100, 200, 255, 1, 50, 150, 250];
    let mut data = Vec::with_capacity(test_values.len() * 2);
    for &v in &test_values {
        data.extend_from_slice(&v.to_le_bytes());
    }

    create_test_tiff_ex(&path, width, height, width, height, &data, 16, None, None);

    let raster = InputRaster::open(&path).unwrap();
    assert_eq!(raster.width, width as usize);
    assert_eq!(raster.height, height as usize);

    let pixels = raster.read_region_u8(0, 0, width as usize, height as usize);
    assert_eq!(pixels.len(), (width * height) as usize);

    // extract_pixel_u8 for 16-bit: raw = i16, if raw <= 0 => 0, else raw.min(255) as u8
    assert_eq!(pixels[0], 0, "i16=0 -> u8=0");
    assert_eq!(pixels[1], 100, "i16=100 -> u8=100");
    assert_eq!(pixels[2], 200, "i16=200 -> u8=200");
    assert_eq!(pixels[3], 255, "i16=255 -> u8=255");
    assert_eq!(pixels[4], 1, "i16=1 -> u8=1");
    assert_eq!(pixels[5], 50, "i16=50 -> u8=50");
    assert_eq!(pixels[6], 150, "i16=150 -> u8=150");
    assert_eq!(pixels[7], 250, "i16=250 -> u8=250");
}
