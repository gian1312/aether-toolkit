// test_min_alt_export.rs — Verify aether_export writes a valid 16-bit unsigned
// single-band GeoTIFF for 16BIT_ALT (MIN_ALT) tiled inputs.

use std::collections::HashMap;
use std::io::Write;
use std::path::Path;

const SENTINEL: u16 = 0xFFFF;

/// Write a minimal tiled (.bit / ATIL) input containing a single 512×512 u16
/// tile at (0,0). `pix(x,y)` gives the quantized altitude for tile-local pixels.
fn write_tiled_bit<F: Fn(usize, usize) -> u16>(path: &Path, pix: F) {
    let ts = 512usize;
    let tile_bytes = ts * ts * 2;
    let mut tile = vec![0u8; tile_bytes];
    for y in 0..ts {
        for x in 0..ts {
            let v = pix(x, y);
            let off = (y * ts + x) * 2;
            tile[off..off + 2].copy_from_slice(&v.to_le_bytes());
        }
    }

    let mut buf: Vec<u8> = Vec::new();
    // tile 0 payload at offset 0
    buf.extend_from_slice(&tile);
    let index_offset = buf.len() as u64;
    // index: one entry (tx u32, ty u32, offset u64, size u32)
    buf.extend_from_slice(&0u32.to_le_bytes()); // tx
    buf.extend_from_slice(&0u32.to_le_bytes()); // ty
    buf.extend_from_slice(&0u64.to_le_bytes()); // offset
    buf.extend_from_slice(&(tile_bytes as u32).to_le_bytes()); // size
    // footer: index_offset u64, tile_count u64, magic, reserved u32
    buf.extend_from_slice(&index_offset.to_le_bytes());
    buf.extend_from_slice(&1u64.to_le_bytes());
    buf.extend_from_slice(b"ATIL");
    buf.extend_from_slice(&0u32.to_le_bytes());

    std::fs::File::create(path).unwrap().write_all(&buf).unwrap();
}

// ── Minimal BigTIFF reader ───────────────────────────────────────────────────

struct TiffTag {
    dtype: u16,
    count: u64,
    value: [u8; 8],
}

fn parse_bigtiff(bytes: &[u8]) -> HashMap<u16, TiffTag> {
    assert_eq!(&bytes[0..2], &0x4949u16.to_le_bytes(), "little-endian");
    assert_eq!(u16::from_le_bytes([bytes[2], bytes[3]]), 43, "BigTIFF version");
    let first_ifd = u64::from_le_bytes(bytes[8..16].try_into().unwrap()) as usize;

    let count = u64::from_le_bytes(bytes[first_ifd..first_ifd + 8].try_into().unwrap());
    let mut map = HashMap::new();
    let mut p = first_ifd + 8;
    for _ in 0..count {
        let tag = u16::from_le_bytes(bytes[p..p + 2].try_into().unwrap());
        let dtype = u16::from_le_bytes(bytes[p + 2..p + 4].try_into().unwrap());
        let cnt = u64::from_le_bytes(bytes[p + 4..p + 12].try_into().unwrap());
        let mut value = [0u8; 8];
        value.copy_from_slice(&bytes[p + 12..p + 20]);
        map.insert(tag, TiffTag { dtype, count: cnt, value });
        p += 20;
    }
    map
}

fn short_val(t: &TiffTag) -> u16 {
    u16::from_le_bytes([t.value[0], t.value[1]])
}
fn long_val(t: &TiffTag) -> u32 {
    u32::from_le_bytes(t.value[0..4].try_into().unwrap())
}
fn ascii_val<'a>(bytes: &'a [u8], t: &TiffTag) -> String {
    // count includes NUL; inline if <= 8 bytes, else at offset.
    let raw: &[u8] = if t.count <= 8 {
        &t.value[..t.count as usize]
    } else {
        let off = u64::from_le_bytes(t.value) as usize;
        &bytes[off..off + t.count as usize]
    };
    String::from_utf8_lossy(raw).trim_end_matches('\0').to_string()
}

#[test]
fn export_16bit_alt_tiff_tags_and_pixels() {
    let dir = tempfile::tempdir().unwrap();
    let bit_path = dir.path().join("alt.bit");
    let json_path = dir.path().join("alt.json");
    let tif_path = dir.path().join("alt.tif");

    let width = 300usize;
    let height = 200usize;

    // Known pixel pattern with an embedded sentinel.
    let pix = |x: usize, y: usize| -> u16 {
        if x == 100 && y == 50 {
            SENTINEL
        } else {
            ((x + y) % 500) as u16
        }
    };
    write_tiled_bit(&bit_path, pix);

    let sidecar = serde_json::json!({
        "output_format": "16BIT_ALT",
        "tile_format": "TILED",
        "tile_size": 512,
        "dimensions": { "width": width, "height": height },
        "geotransform": [8.0, 0.0002, 0.0, 47.0, 0.0, -0.0002],
        "projection": "EPSG:4326",
        "alt_step_m": 0.5,
        "alt_ref": "AGL_CLAMPED",
        "alt_sentinel": 65535
    });
    serde_json::to_writer(std::fs::File::create(&json_path).unwrap(), &sidecar).unwrap();

    // Run the exporter with no compression so tile bytes are raw u16.
    let status = std::process::Command::new(env!("CARGO_BIN_EXE_aether_export"))
        .arg("-i").arg(&bit_path)
        .arg("-j").arg(&json_path)
        .arg("-o").arg(&tif_path)
        .arg("-l").arg("0")
        .arg("--no-overviews")
        .status()
        .expect("run aether_export");
    assert!(status.success(), "aether_export failed");

    let tif = std::fs::read(&tif_path).unwrap();
    let tags = parse_bigtiff(&tif);

    // --- Tag assertions ---
    assert_eq!(long_val(&tags[&256]), width as u32, "ImageWidth");
    assert_eq!(long_val(&tags[&257]), height as u32, "ImageLength");
    assert_eq!(short_val(&tags[&258]), 16, "BitsPerSample");
    assert_eq!(short_val(&tags[&339]), 1, "SampleFormat = unsigned int");
    assert_eq!(short_val(&tags[&277]), 1, "SamplesPerPixel");
    let nodata = ascii_val(&tif, &tags[&42113]);
    assert_eq!(nodata, "65535", "GDAL_NODATA");

    // --- Pixel assertions (tile 0, raw u16, 512-wide tiles) ---
    let tile_off = u64::from_le_bytes(tags[&324].value) as usize; // TileOffsets[0]
    let ts = long_val(&tags[&322]) as usize; // TileWidth
    let read_px = |x: usize, y: usize| -> u16 {
        let off = tile_off + (y * ts + x) * 2;
        u16::from_le_bytes([tif[off], tif[off + 1]])
    };
    assert_eq!(read_px(0, 0), pix(0, 0));
    assert_eq!(read_px(100, 50), SENTINEL);
    assert_eq!(read_px(37, 21), pix(37, 21));
    assert_eq!(read_px(299, 199), pix(299, 199));
}
