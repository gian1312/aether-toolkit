// Integration tests for aether_export binary.
//
// These tests invoke the compiled binary as a subprocess using
// `env!("CARGO_BIN_EXE_aether_export")` and verify its behavior
// against synthetic .bit + .json fixture files.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::tempdir;

// ─────────────────────────────────────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────────────────────────────────────

/// Create a synthetic flat .bit file and its JSON sidecar.
///
/// For 8-bit formats the .bit payload is `width * height` bytes, each set to
/// `pixel_value`.  For 1-bit formats the payload is packed MSB-first with
/// byte-aligned rows; `pixel_value` controls whether bits are all-zero (0) or
/// all-one (non-zero).
///
/// Returns `(bit_path, json_path)`.
fn create_test_bit_file(
    dir: &Path,
    name: &str,
    width: usize,
    height: usize,
    tile_size: usize,
    format: &str,
    pixel_value: u8,
) -> (PathBuf, PathBuf) {
    let bit_path = dir.join(format!("{}.bit", name));
    let json_path = dir.join(format!("{}.json", name));

    // Build raw pixel data --------------------------------------------------
    let data: Vec<u8> = if format == "1BIT_LOS" {
        // Row stride: byte-align each row (round up to next multiple of 4 bytes)
        let row_stride = ((width + 31) / 32) * 4;
        let mut buf = vec![0u8; row_stride * height];
        if pixel_value != 0 {
            // Set every bit in the valid region
            for y in 0..height {
                for x in 0..width {
                    let byte_idx = y * row_stride + x / 8;
                    buf[byte_idx] |= 1 << (7 - (x & 7));
                }
            }
        }
        buf
    } else {
        // 8-bit: one byte per pixel, row stride == width
        vec![pixel_value; width * height]
    };

    let row_stride_bytes = if format == "1BIT_LOS" {
        ((width + 31) / 32) * 4
    } else {
        width
    };

    // Write the raw .bit file
    let mut f = fs::File::create(&bit_path).unwrap();
    f.write_all(&data).unwrap();
    f.flush().unwrap();

    // Write the JSON sidecar ------------------------------------------------
    let sidecar = serde_json::json!({
        "dimensions": { "width": width, "height": height },
        "geotransform": [600000.0, 1.0, 0.0, 200000.0, 0.0, -1.0],
        "projection": "EPSG:2056",
        "output_format": format,
        "row_stride_bytes": row_stride_bytes,
        "tile_size": tile_size
    });

    fs::write(&json_path, serde_json::to_string_pretty(&sidecar).unwrap()).unwrap();

    (bit_path, json_path)
}

/// Create a synthetic tiled .tiles file (ATIL format) and its JSON sidecar.
///
/// The tiled format consists of:
///   1. Tile data blocks
///   2. Tile index (20 bytes per entry: tx:u32, ty:u32, offset:u64, size:u32)
///   3. Footer (24 bytes): index_offset:u64, tile_count:u64, "ATIL":4, reserved:u32
fn create_test_tiles_file(
    dir: &Path,
    name: &str,
    width: usize,
    height: usize,
    tile_size: usize,
    format: &str,
    pixel_value: u8,
) -> (PathBuf, PathBuf) {
    let tiles_path = dir.join(format!("{}.tiles", name));
    let json_path = dir.join(format!("{}.json", name));

    let is_8bit = format != "1BIT_LOS";
    let tiles_x = (width + tile_size - 1) / tile_size;
    let tiles_y = (height + tile_size - 1) / tile_size;

    let tile_bytes = if is_8bit {
        tile_size * tile_size
    } else {
        (tile_size * tile_size) / 8
    };

    let mut f = fs::File::create(&tiles_path).unwrap();
    let mut index_entries: Vec<(u32, u32, u64, u32)> = Vec::new();

    for ty in 0..tiles_y {
        for tx in 0..tiles_x {
            let offset = (ty * tiles_x + tx) * tile_bytes;
            let tile_data = vec![pixel_value; tile_bytes];
            f.write_all(&tile_data).unwrap();
            index_entries.push((tx as u32, ty as u32, offset as u64, tile_bytes as u32));
        }
    }

    // Write tile index
    let index_offset = (tiles_x * tiles_y * tile_bytes) as u64;
    for &(tx, ty, off, sz) in &index_entries {
        f.write_all(&tx.to_le_bytes()).unwrap();
        f.write_all(&ty.to_le_bytes()).unwrap();
        f.write_all(&off.to_le_bytes()).unwrap();
        f.write_all(&sz.to_le_bytes()).unwrap();
    }

    // Write footer (24 bytes)
    let tile_count = index_entries.len() as u64;
    f.write_all(&index_offset.to_le_bytes()).unwrap();
    f.write_all(&tile_count.to_le_bytes()).unwrap();
    f.write_all(b"ATIL").unwrap();
    f.write_all(&0u32.to_le_bytes()).unwrap(); // reserved
    f.flush().unwrap();

    // JSON sidecar with tile_format = "TILED"
    let sidecar = serde_json::json!({
        "dimensions": { "width": width, "height": height },
        "geotransform": [600000.0, 1.0, 0.0, 200000.0, 0.0, -1.0],
        "projection": "EPSG:2056",
        "output_format": format,
        "tile_format": "TILED",
        "tile_size": tile_size
    });

    fs::write(&json_path, serde_json::to_string_pretty(&sidecar).unwrap()).unwrap();

    (tiles_path, json_path)
}

/// Return the path to the aether_export binary built by cargo.
fn aether_export_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_aether_export"))
}

/// Assert that the first bytes of a file look like a BigTIFF header.
/// BigTIFF starts with: II (0x49 0x49) followed by version 43 (0x2B 0x00).
fn assert_bigtiff_header(path: &Path) {
    let data = fs::read(path).expect("failed to read output TIFF");
    assert!(data.len() >= 8, "TIFF file too small: {} bytes", data.len());

    // Byte-order mark: little-endian "II"
    assert_eq!(data[0], 0x49, "expected 'I' at byte 0");
    assert_eq!(data[1], 0x49, "expected 'I' at byte 1");

    // BigTIFF version: 43 (0x2B)
    let version = u16::from_le_bytes([data[2], data[3]]);
    assert_eq!(version, 43, "expected BigTIFF version 43, got {}", version);
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn test_export_8bit_produces_tiff() {
    let dir = tempdir().unwrap();
    let (bit_path, json_path) =
        create_test_bit_file(dir.path(), "test_8bit", 512, 512, 512, "8BIT_PROP", 100);
    let output_path = dir.path().join("output_8bit.tif");

    let status = Command::new(aether_export_bin())
        .arg("-i").arg(&bit_path)
        .arg("-j").arg(&json_path)
        .arg("-o").arg(&output_path)
        .arg("--no-overviews")
        .status()
        .expect("failed to execute aether_export");

    assert!(status.success(), "aether_export exited with non-zero status");
    assert!(output_path.exists(), "output TIFF was not created");

    let file_size = fs::metadata(&output_path).unwrap().len();
    assert!(file_size > 0, "output TIFF is empty");

    assert_bigtiff_header(&output_path);
}

#[test]
fn test_export_1bit_produces_tiff() {
    let dir = tempdir().unwrap();
    let (bit_path, json_path) =
        create_test_bit_file(dir.path(), "test_1bit", 512, 512, 512, "1BIT_LOS", 1);
    let output_path = dir.path().join("output_1bit.tif");

    let status = Command::new(aether_export_bin())
        .arg("-i").arg(&bit_path)
        .arg("-j").arg(&json_path)
        .arg("-o").arg(&output_path)
        .arg("--no-overviews")
        .status()
        .expect("failed to execute aether_export");

    assert!(status.success(), "aether_export exited with non-zero status");
    assert!(output_path.exists(), "output TIFF was not created");

    let file_size = fs::metadata(&output_path).unwrap().len();
    assert!(file_size > 0, "output TIFF is empty");

    assert_bigtiff_header(&output_path);
}

#[test]
fn test_export_no_overviews() {
    let dir = tempdir().unwrap();
    let (bit_path, json_path) =
        create_test_bit_file(dir.path(), "test_noovr", 512, 512, 512, "8BIT_PROP", 42);
    let output_path = dir.path().join("output_noovr.tif");

    let status = Command::new(aether_export_bin())
        .arg("-i").arg(&bit_path)
        .arg("-j").arg(&json_path)
        .arg("-o").arg(&output_path)
        .arg("--no-overviews")
        .status()
        .expect("failed to execute aether_export");

    assert!(status.success(), "aether_export with --no-overviews should succeed");
    assert!(output_path.exists(), "output TIFF was not created");

    assert_bigtiff_header(&output_path);
}

#[test]
fn test_export_missing_json_fails() {
    let dir = tempdir().unwrap();

    // Create only the .bit file, no .json sidecar
    let bit_path = dir.path().join("orphan.bit");
    fs::write(&bit_path, vec![0u8; 512 * 512]).unwrap();

    let nonexistent_json = dir.path().join("does_not_exist.json");
    let output_path = dir.path().join("should_not_exist.tif");

    let status = Command::new(aether_export_bin())
        .arg("-i").arg(&bit_path)
        .arg("-j").arg(&nonexistent_json)
        .arg("-o").arg(&output_path)
        .status()
        .expect("failed to execute aether_export");

    assert!(
        !status.success(),
        "aether_export should fail when JSON sidecar is missing"
    );
    assert!(
        !output_path.exists(),
        "output TIFF should not be created when JSON is missing"
    );
}

#[test]
fn test_export_custom_tile_size() {
    let dir = tempdir().unwrap();
    let (bit_path, json_path) =
        create_test_bit_file(dir.path(), "test_ts256", 512, 512, 256, "8BIT_PROP", 80);
    let output_path = dir.path().join("output_ts256.tif");

    let status = Command::new(aether_export_bin())
        .arg("-i").arg(&bit_path)
        .arg("-j").arg(&json_path)
        .arg("-o").arg(&output_path)
        .arg("-t").arg("256")
        .arg("--no-overviews")
        .status()
        .expect("failed to execute aether_export");

    assert!(status.success(), "aether_export with -t 256 should succeed");
    assert!(output_path.exists(), "output TIFF was not created");

    assert_bigtiff_header(&output_path);
}

#[test]
fn test_export_tiled_input_8bit() {
    let dir = tempdir().unwrap();
    let (tiles_path, json_path) =
        create_test_tiles_file(dir.path(), "test_tiled_8bit", 512, 512, 512, "8BIT_PROP", 100);
    let output_path = dir.path().join("output_tiled_8bit.tif");

    let status = Command::new(aether_export_bin())
        .arg("-i").arg(&tiles_path)
        .arg("-j").arg(&json_path)
        .arg("-o").arg(&output_path)
        .arg("--no-overviews")
        .status()
        .expect("failed to execute aether_export");

    assert!(status.success(), "aether_export on tiled input should succeed");
    assert!(output_path.exists(), "output TIFF was not created from tiled input");

    assert_bigtiff_header(&output_path);
}

#[test]
fn test_export_tiled_input_1bit() {
    let dir = tempdir().unwrap();
    let (tiles_path, json_path) =
        create_test_tiles_file(dir.path(), "test_tiled_1bit", 512, 512, 512, "1BIT_LOS", 0xFF);
    let output_path = dir.path().join("output_tiled_1bit.tif");

    let status = Command::new(aether_export_bin())
        .arg("-i").arg(&tiles_path)
        .arg("-j").arg(&json_path)
        .arg("-o").arg(&output_path)
        .arg("--no-overviews")
        .status()
        .expect("failed to execute aether_export");

    assert!(status.success(), "aether_export on tiled 1-bit input should succeed");
    assert!(output_path.exists(), "output TIFF was not created from tiled 1-bit input");

    assert_bigtiff_header(&output_path);
}

#[test]
fn test_export_compression_level_zero() {
    let dir = tempdir().unwrap();
    let (bit_path, json_path) =
        create_test_bit_file(dir.path(), "test_nocompress", 512, 512, 512, "8BIT_PROP", 50);
    let output_path = dir.path().join("output_nocompress.tif");

    let status = Command::new(aether_export_bin())
        .arg("-i").arg(&bit_path)
        .arg("-j").arg(&json_path)
        .arg("-o").arg(&output_path)
        .arg("-l").arg("0")
        .arg("--no-overviews")
        .status()
        .expect("failed to execute aether_export");

    assert!(status.success(), "aether_export with -l 0 should succeed");
    assert!(output_path.exists(), "output TIFF was not created");

    // Uncompressed output should be at least as large as the raw pixel data
    let file_size = fs::metadata(&output_path).unwrap().len();
    assert!(
        file_size >= (512 * 512) as u64,
        "uncompressed output should be at least 262144 bytes, got {}",
        file_size
    );

    assert_bigtiff_header(&output_path);
}
