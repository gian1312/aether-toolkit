use aether_aggregate::reader::InputRaster;
use aether_aggregate::aggregate;
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
    geotransform: [f64; 6],
    tile_data: &[u8],
) -> std::path::PathBuf {
    let bit_path = dir.join(format!("{}.bit", name));
    let json_path = dir.join(format!("{}.json", name));

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
        r#"{{"dimensions":{{"width":{},"height":{}}},"geotransform":[{},{},{},{},{},{}],"output_format":"{}","tile_size":{}}}"#,
        width,
        height,
        geotransform[0],
        geotransform[1],
        geotransform[2],
        geotransform[3],
        geotransform[4],
        geotransform[5],
        output_format,
        tile_size
    );
    std::fs::write(&json_path, sidecar).unwrap();

    bit_path
}

#[test]
fn test_aggregate_single_input() {
    let dir = tempfile::tempdir().unwrap();

    // Create a synthetic 8-bit .bit file with a small tile
    let tile_size = 8;
    let tile_data = vec![100u8; tile_size * tile_size];
    let gt = [8.0, 0.001, 0.0, 47.0, 0.0, -0.001];
    let bit_path = write_bit_file(
        dir.path(),
        "input1",
        tile_size,
        tile_size,
        tile_size,
        "8BIT_PROP",
        gt,
        &tile_data,
    );

    let raster = InputRaster::open(&bit_path).unwrap();
    let inputs = vec![raster];
    let wp_ids = vec![1u32];

    let max_path = dir.path().join("out_max.tif");
    let count_path = dir.path().join("out_count.tif");

    let stats = aggregate::run(
        &inputs,
        &wp_ids,
        max_path.to_str().unwrap(),
        count_path.to_str().unwrap(),
        None,
        tile_size,
        0, // no compression for simplicity
    )
    .unwrap();

    // Output files should exist
    assert!(max_path.exists(), "_max.tif should be created");
    assert!(count_path.exists(), "_count.tif should be created");
    assert!(stats.has_valid_data, "Should have valid data");
}

#[test]
fn test_aggregate_empty_inputs() {
    let dir = tempfile::tempdir().unwrap();

    let inputs: Vec<InputRaster> = vec![];
    let wp_ids: Vec<u32> = vec![];

    let max_path = dir.path().join("empty_max.tif");
    let count_path = dir.path().join("empty_count.tif");

    let result = aggregate::run(
        &inputs,
        &wp_ids,
        max_path.to_str().unwrap(),
        count_path.to_str().unwrap(),
        None,
        8,
        0,
    );

    // Empty inputs should produce an error (compute_master_grid returns "No inputs")
    assert!(result.is_err(), "Empty inputs should produce an error");
}

#[test]
fn test_aggregate_stats() {
    let dir = tempfile::tempdir().unwrap();

    // Create a tile with known varying values
    let tile_size = 8;
    let mut tile_data = vec![0u8; tile_size * tile_size];
    // Set a few pixels to non-zero values
    tile_data[0] = 50;
    tile_data[1] = 100;
    tile_data[2] = 200;

    let gt = [8.0, 0.001, 0.0, 47.0, 0.0, -0.001];
    let bit_path = write_bit_file(
        dir.path(),
        "stats_input",
        tile_size,
        tile_size,
        tile_size,
        "8BIT_PROP",
        gt,
        &tile_data,
    );

    let raster = InputRaster::open(&bit_path).unwrap();
    let inputs = vec![raster];
    let wp_ids = vec![1u32];

    let max_path = dir.path().join("stats_max.tif");
    let count_path = dir.path().join("stats_count.tif");

    let stats = aggregate::run(
        &inputs,
        &wp_ids,
        max_path.to_str().unwrap(),
        count_path.to_str().unwrap(),
        None,
        tile_size,
        0,
    )
    .unwrap();

    assert!(stats.has_valid_data, "has_valid_data should be true for non-zero input");
}

// =============================================================================
// Additional Aggregate Tests
// =============================================================================

#[test]
fn test_aggregate_two_overlapping_inputs() {
    let dir = tempfile::tempdir().unwrap();

    let tile_size = 8;
    let gt = [8.0, 0.001, 0.0, 47.0, 0.0, -0.001];

    // Input 1: all pixels = 80
    let tile_data_1 = vec![80u8; tile_size * tile_size];
    let bit_path_1 = write_bit_file(
        dir.path(), "overlap1", tile_size, tile_size, tile_size,
        "8BIT_PROP", gt, &tile_data_1,
    );

    // Input 2: same area, all pixels = 120
    let tile_data_2 = vec![120u8; tile_size * tile_size];
    let bit_path_2 = write_bit_file(
        dir.path(), "overlap2", tile_size, tile_size, tile_size,
        "8BIT_PROP", gt, &tile_data_2,
    );

    let r1 = InputRaster::open(&bit_path_1).unwrap();
    let r2 = InputRaster::open(&bit_path_2).unwrap();
    let inputs = vec![r1, r2];
    let wp_ids = vec![1u32, 2u32];

    let max_path = dir.path().join("overlap_max.tif");
    let count_path = dir.path().join("overlap_count.tif");

    let stats = aggregate::run(
        &inputs,
        &wp_ids,
        max_path.to_str().unwrap(),
        count_path.to_str().unwrap(),
        None,
        tile_size,
        0,
    )
    .unwrap();

    assert!(stats.has_valid_data, "Should have valid data");

    // Max of (80, 120) = 120
    assert!(
        stats.max_max >= 120.0,
        "max_max should be at least 120, got {}",
        stats.max_max
    );

    // count_max should be 2 (two overlapping inputs)
    assert!(
        stats.count_max >= 2.0,
        "count_max should be at least 2, got {}",
        stats.count_max
    );
}

#[test]
fn test_aggregate_with_visibility() {
    let dir = tempfile::tempdir().unwrap();

    let tile_size = 8;
    let tile_data = vec![100u8; tile_size * tile_size];
    let gt = [8.0, 0.001, 0.0, 47.0, 0.0, -0.001];
    let bit_path = write_bit_file(
        dir.path(), "vis_input", tile_size, tile_size, tile_size,
        "8BIT_PROP", gt, &tile_data,
    );

    let raster = InputRaster::open(&bit_path).unwrap();
    let inputs = vec![raster];
    let wp_ids = vec![1u32];

    let max_path = dir.path().join("vis_max.tif");
    let count_path = dir.path().join("vis_count.tif");
    let vix_path = dir.path().join("vis_output.vix");

    let stats = aggregate::run(
        &inputs,
        &wp_ids,
        max_path.to_str().unwrap(),
        count_path.to_str().unwrap(),
        Some(vix_path.to_str().unwrap()),
        tile_size,
        0,
    )
    .unwrap();

    assert!(stats.has_valid_data);
    assert!(vix_path.exists(), ".vix file should be created");

    // Verify VIX! magic at the end of the file (last 4 bytes)
    let vix_data = std::fs::read(&vix_path).unwrap();
    assert!(
        vix_data.len() >= 12,
        ".vix file should be at least 12 bytes (toc_offset + magic), got {}",
        vix_data.len()
    );
    let magic = &vix_data[vix_data.len() - 4..];
    assert_eq!(
        magic, b"VIX!",
        "Last 4 bytes should be VIX! magic, got {:?}",
        magic
    );
}

#[test]
fn test_aggregate_produces_valid_bigtiff() {
    let dir = tempfile::tempdir().unwrap();

    let tile_size = 8;
    let tile_data = vec![55u8; tile_size * tile_size];
    let gt = [8.0, 0.001, 0.0, 47.0, 0.0, -0.001];
    let bit_path = write_bit_file(
        dir.path(), "bigtiff_input", tile_size, tile_size, tile_size,
        "8BIT_PROP", gt, &tile_data,
    );

    let raster = InputRaster::open(&bit_path).unwrap();
    let inputs = vec![raster];
    let wp_ids = vec![1u32];

    let max_path = dir.path().join("bigtiff_max.tif");
    let count_path = dir.path().join("bigtiff_count.tif");

    aggregate::run(
        &inputs,
        &wp_ids,
        max_path.to_str().unwrap(),
        count_path.to_str().unwrap(),
        None,
        tile_size,
        0,
    )
    .unwrap();

    // Read the _max.tif and verify BigTIFF header
    let max_bytes = std::fs::read(&max_path).unwrap();
    assert!(max_bytes.len() >= 16, "BigTIFF file should be at least 16 bytes");

    // Byte order: 0x4949 (little-endian)
    let byte_order = u16::from_le_bytes([max_bytes[0], max_bytes[1]]);
    assert_eq!(byte_order, 0x4949, "Should be little-endian (0x4949), got {:#06x}", byte_order);

    // Version: 43 (BigTIFF)
    let version = u16::from_le_bytes([max_bytes[2], max_bytes[3]]);
    assert_eq!(version, 43, "Should be BigTIFF version 43, got {}", version);

    // Offset size: 8
    let offset_size = u16::from_le_bytes([max_bytes[4], max_bytes[5]]);
    assert_eq!(offset_size, 8, "BigTIFF offset size should be 8, got {}", offset_size);

    // Also verify count file is BigTIFF
    let count_bytes = std::fs::read(&count_path).unwrap();
    let count_version = u16::from_le_bytes([count_bytes[2], count_bytes[3]]);
    assert_eq!(count_version, 43, "Count file should also be BigTIFF version 43");
}

#[test]
fn test_aggregate_stats_values() {
    let dir = tempfile::tempdir().unwrap();

    let tile_size = 8;
    let gt = [8.0, 0.001, 0.0, 47.0, 0.0, -0.001];

    // Input 1: mixed values -- minimum non-zero is 30, maximum is 200
    let mut tile_data_1 = vec![0u8; tile_size * tile_size];
    tile_data_1[0] = 30;
    tile_data_1[1] = 100;
    tile_data_1[2] = 200;
    tile_data_1[3] = 50;

    let bit_path_1 = write_bit_file(
        dir.path(), "stats1", tile_size, tile_size, tile_size,
        "8BIT_PROP", gt, &tile_data_1,
    );

    // Input 2: same area, one pixel = 250 (higher than input 1's max)
    let mut tile_data_2 = vec![0u8; tile_size * tile_size];
    tile_data_2[0] = 250;
    tile_data_2[5] = 10;

    let bit_path_2 = write_bit_file(
        dir.path(), "stats2", tile_size, tile_size, tile_size,
        "8BIT_PROP", gt, &tile_data_2,
    );

    let r1 = InputRaster::open(&bit_path_1).unwrap();
    let r2 = InputRaster::open(&bit_path_2).unwrap();
    let inputs = vec![r1, r2];
    let wp_ids = vec![1u32, 2u32];

    let max_path = dir.path().join("sv_max.tif");
    let count_path = dir.path().join("sv_count.tif");

    let stats = aggregate::run(
        &inputs,
        &wp_ids,
        max_path.to_str().unwrap(),
        count_path.to_str().unwrap(),
        None,
        tile_size,
        0,
    )
    .unwrap();

    assert!(stats.has_valid_data);

    // max_max: the per-tile max is max(200, 250) across all tiles = 250
    assert!(
        (stats.max_max - 250.0).abs() < 1e-10,
        "max_max should be 250.0, got {}",
        stats.max_max
    );

    // max_min: the per-tile min non-zero value. Since there's one tile,
    // the minimum non-zero in the max-overlay is min(30, 10) = 10.
    assert!(
        (stats.max_min - 10.0).abs() < 1e-10,
        "max_min should be 10.0, got {}",
        stats.max_min
    );

    // count_max: pixel [0] has contributions from both inputs (30 and 250),
    // so count=2 at that pixel. That should be the maximum count.
    assert!(
        stats.count_max >= 2.0,
        "count_max should be at least 2, got {}",
        stats.count_max
    );
}
