use aether_aggregate::writer::*;
use flate2::read::ZlibDecoder;
use std::fs::File;
use std::io::Read;
use tempfile::tempdir;

// ===========================================================================
// compress_u8_tile
// ===========================================================================

#[test]
fn test_compress_u8_no_compression() {
    let data: Vec<u8> = (0..64).collect();
    let out = compress_u8_tile(&data, 8, 8, 0);
    assert_eq!(out, data);
}

#[test]
fn test_compress_u8_with_compression() {
    // 16x16 tile filled with a repeating pattern (compresses well).
    let mut data = vec![0u8; 256];
    for (i, v) in data.iter_mut().enumerate() {
        *v = (i % 13) as u8;
    }

    let compressed = compress_u8_tile(&data, 16, 16, 1);

    // Compressed output should be smaller (pattern is compressible).
    assert!(!compressed.is_empty());
    assert!(compressed.len() < data.len());

    // Decompress and undo horizontal predictor to recover original.
    let mut decompressed = Vec::new();
    ZlibDecoder::new(compressed.as_slice())
        .read_to_end(&mut decompressed)
        .unwrap();
    assert_eq!(decompressed.len(), data.len());

    // Undo predictor: for each row, wrapping_add left-to-right.
    let row_bytes = 16;
    for y in 0..16 {
        let s = y * row_bytes;
        for i in 1..row_bytes {
            decompressed[s + i] = decompressed[s + i].wrapping_add(decompressed[s + i - 1]);
        }
    }
    assert_eq!(decompressed, data);
}

// ===========================================================================
// compress_u16_tile
// ===========================================================================

#[test]
fn test_compress_u16_no_compression() {
    let data: Vec<u16> = (0..16).collect();
    let out = compress_u16_tile(&data, 4, 4, 0);

    // Output should be the LE byte representation.
    assert_eq!(out.len(), 32);
    for (i, &v) in data.iter().enumerate() {
        let le = v.to_le_bytes();
        assert_eq!(out[i * 2], le[0]);
        assert_eq!(out[i * 2 + 1], le[1]);
    }
}

#[test]
fn test_compress_u16_with_compression() {
    // 8x8 tile with a repeating pattern.
    let mut data = vec![0u16; 64];
    for (i, v) in data.iter_mut().enumerate() {
        *v = (i % 7) as u16 + 100;
    }

    let compressed = compress_u16_tile(&data, 8, 8, 1);
    assert!(!compressed.is_empty());
    assert!(compressed.len() < data.len() * 2);

    // Decompress.
    let mut raw = Vec::new();
    ZlibDecoder::new(compressed.as_slice())
        .read_to_end(&mut raw)
        .unwrap();
    assert_eq!(raw.len(), data.len() * 2);

    // Undo horizontal predictor on u16 values.
    let width = 8;
    let row_bytes = width * 2;
    for y in 0..8 {
        let s = y * row_bytes;
        for x in 1..width {
            let cx = s + x * 2;
            let px = s + (x - 1) * 2;
            let prev = u16::from_le_bytes([raw[px], raw[px + 1]]);
            let curr = u16::from_le_bytes([raw[cx], raw[cx + 1]]);
            let sum = curr.wrapping_add(prev);
            raw[cx..cx + 2].copy_from_slice(&sum.to_le_bytes());
        }
    }

    // Convert back to u16 and compare.
    let recovered: Vec<u16> = (0..64)
        .map(|i| u16::from_le_bytes([raw[i * 2], raw[i * 2 + 1]]))
        .collect();
    assert_eq!(recovered, data);
}

// ===========================================================================
// TileStoreU8
// ===========================================================================

#[test]
fn test_tile_store_u8_new() {
    let store = TileStoreU8::new(100, 100, 64, 0, 0);
    assert_eq!(store.tiles_x, 2); // ceil(100/64) = 2
    assert_eq!(store.tiles_y, 2);
    assert_eq!(store.width, 100);
    assert_eq!(store.height, 100);
}

#[test]
fn test_tile_store_u8_store_and_get() {
    let mut store = TileStoreU8::new(64, 64, 64, 0, 0);
    let compressed = vec![1, 2, 3, 4, 5];
    store.store(0, 0, compressed.clone());

    let retrieved = store.get_compressed(0, 0).unwrap();
    assert_eq!(*retrieved, compressed);
}

#[test]
fn test_tile_store_u8_get_empty() {
    let store = TileStoreU8::new(64, 64, 64, 0, 0);
    assert!(store.get_compressed(0, 0).is_none());
}

#[test]
fn test_tile_store_u8_overview() {
    let ts = 64;
    let w = 128;
    let h = 128;
    let nodata = 0u8;

    let mut store = TileStoreU8::new(w, h, ts, nodata, 0);

    // Fill all 4 tiles (2x2 grid) with known data (no compression).
    for ty in 0..2 {
        for tx in 0..2 {
            let mut tile = vec![nodata; ts * ts];
            // Put a distinct value in each tile.
            let val = (ty * 2 + tx + 1) as u8;
            for p in tile.iter_mut() {
                *p = val;
            }
            let compressed = compress_u8_tile(&tile, ts, ts, 0);
            store.store(tx, ty, compressed);
        }
    }

    let ovr = store.build_overview_4x();

    // Overview dimensions: ceil(128/4) = 32 x 32.
    assert_eq!(ovr.width, 32);
    assert_eq!(ovr.height, 32);
    // Tiles: ceil(32/64) = 1 x 1.
    assert_eq!(ovr.tiles_x, 1);
    assert_eq!(ovr.tiles_y, 1);
}

// ===========================================================================
// TileStoreU16
// ===========================================================================

#[test]
fn test_tile_store_u16_store_and_get() {
    let mut store = TileStoreU16::new(64, 64, 64, 0, 0);
    let compressed = vec![10, 20, 30];
    store.store(0, 0, compressed.clone());

    let retrieved = store.get_compressed(0, 0).unwrap();
    assert_eq!(*retrieved, compressed);
}

#[test]
fn test_tile_store_u16_overview() {
    let ts = 64;
    let w = 128;
    let h = 128;
    let nodata = 0u16;

    let mut store = TileStoreU16::new(w, h, ts, nodata, 0);

    // Fill all 4 tiles with known data (no compression).
    for ty in 0..2 {
        for tx in 0..2 {
            let mut tile = vec![nodata; ts * ts];
            let val = (ty * 2 + tx + 100) as u16;
            for p in tile.iter_mut() {
                *p = val;
            }
            let compressed = compress_u16_tile(&tile, ts, ts, 0);
            store.store(tx, ty, compressed);
        }
    }

    let ovr = store.build_overview_4x();

    assert_eq!(ovr.width, 32);
    assert_eq!(ovr.height, 32);
    assert_eq!(ovr.tiles_x, 1);
    assert_eq!(ovr.tiles_y, 1);
}

// ===========================================================================
// BigTiffWriter
// ===========================================================================

#[test]
fn test_bigtiff_creates_valid_file() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("test.tif");
    let path_str = path.to_str().unwrap();

    let gt = [0.0, 1.0, 0.0, 0.0, 0.0, -1.0];
    let ts = 64;
    let mut writer = BigTiffWriter::create(path_str, 64, 64, ts, &gt, 0, 0, 8).unwrap();

    // Write one tile of non-zero data.
    let tile_data = vec![128u8; ts * ts];
    let compressed = compress_u8_tile(&tile_data, ts, ts, 0);
    writer.write_tile(0, 0, Some(&compressed)).unwrap();
    writer.finalize(None, 1.0, 0.0).unwrap();

    // Read back and verify BigTIFF header.
    let mut raw = Vec::new();
    File::open(&path).unwrap().read_to_end(&mut raw).unwrap();
    assert!(raw.len() > 8);

    // II (little-endian), version 43, offset_size 8, reserved 0.
    let expected_header: [u8; 8] = [0x49, 0x49, 0x2B, 0x00, 0x08, 0x00, 0x00, 0x00];
    assert_eq!(&raw[..8], &expected_header);
}

#[test]
fn test_bigtiff_with_geotransform() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("geo.tif");
    let path_str = path.to_str().unwrap();

    let gt = [8.5, 0.001, 0.0, 47.5, 0.0, -0.001];
    let ts = 64;
    let writer = BigTiffWriter::create(path_str, 128, 128, ts, &gt, 255, 1, 8).unwrap();
    writer.finalize(Some((0.0, 200.0)), 0.5, 10.0).unwrap();

    let meta = std::fs::metadata(&path).unwrap();
    assert!(meta.len() > 0);
}

#[test]
fn test_bigtiff_empty_tiles() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("empty.tif");
    let path_str = path.to_str().unwrap();

    let gt = [0.0, 1.0, 0.0, 0.0, 0.0, -1.0];
    let ts = 64;
    let writer = BigTiffWriter::create(path_str, 64, 64, ts, &gt, 0, 0, 8).unwrap();
    // No tiles written.
    writer.finalize(None, 1.0, 0.0).unwrap();

    // File should still be valid (has header).
    let mut raw = Vec::new();
    File::open(&path).unwrap().read_to_end(&mut raw).unwrap();
    assert!(raw.len() >= 8);

    let expected_header: [u8; 8] = [0x49, 0x49, 0x2B, 0x00, 0x08, 0x00, 0x00, 0x00];
    assert_eq!(&raw[..8], &expected_header);
}

#[test]
fn test_bigtiff_multiple_tiles() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("multi.tif");
    let path_str = path.to_str().unwrap();

    let gt = [0.0, 1.0, 0.0, 0.0, 0.0, -1.0];
    let ts = 64;
    // 256x256 image with 64x64 tiles -> 4x4 = 16 tiles.
    let mut writer = BigTiffWriter::create(path_str, 256, 256, ts, &gt, 0, 0, 8).unwrap();

    for ty in 0..4 {
        for tx in 0..4 {
            let mut tile = vec![0u8; ts * ts];
            tile[0] = ((ty * 4 + tx) as u8).wrapping_add(1);
            let compressed = compress_u8_tile(&tile, ts, ts, 0);
            writer.write_tile(tx, ty, Some(&compressed)).unwrap();
        }
    }
    writer.finalize(None, 1.0, 0.0).unwrap();

    let meta = std::fs::metadata(&path).unwrap();
    // 16 uncompressed tiles of 4096 bytes each = 65536 bytes of payload alone.
    assert!(meta.len() >= 65536);

    // Verify header.
    let mut raw = vec![0u8; 8];
    let mut f = File::open(&path).unwrap();
    f.read_exact(&mut raw).unwrap();
    let expected_header: [u8; 8] = [0x49, 0x49, 0x2B, 0x00, 0x08, 0x00, 0x00, 0x00];
    assert_eq!(&raw, &expected_header);
}
