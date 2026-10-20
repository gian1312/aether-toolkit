//! Tiled LZW GeoTIFFs whose width is not a multiple of the tile width
//! (swissALTI3D 2 m: 500 px, 128² tiles). tiff 0.11.3's row-by-row read of a
//! padded edge tile trips weezl 0.1's false `NoProgress` and fails with
//! "no lzw end code found" (image-rs/image-tiff#395). The converter reads
//! chunks whole instead (`ingest::decode_chunk`); these tests pin that both
//! decode paths give the same samples as a DEFLATE copy of the same pixels.

use std::collections::HashMap;
use std::fs::File;
use std::io::BufReader;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use aether_converter::ingest::{load_source_to_ram, process_tile_with_cache, IngestJob, VOID_ELEV};

fn data(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/data").join(name)
}

const LZW: &str = "lzw_edge_tiles.tif";
const DEFLATE: &str = "lzw_edge_tiles_deflate.tif";

/// The fixture must really exercise the bug, or the tests below would pass
/// for nothing. When this fails, the `tiff` in the lockfile reads padded LZW
/// edge tiles itself: `decode_chunk` can go back to `read_chunk`.
#[test]
fn canary_fixture_still_breaks_plain_read_chunk() {
    let file = File::open(data(LZW)).unwrap();
    let mut dec = tiff::decoder::Decoder::new(BufReader::new(file)).unwrap();
    let n = dec.tile_count().unwrap();
    let failed: Vec<String> = (0..n)
        .filter_map(|i| dec.read_chunk(i).err().map(|e| format!("tile {i}: {e}")))
        .collect();
    assert!(
        failed.iter().any(|f| f.contains("no lzw end code found")),
        "tiff now decodes every tile of {LZW} via read_chunk — the whole-chunk \
         workaround in ingest::decode_chunk is no longer needed (failures: {failed:?})"
    );
}

#[test]
fn whole_file_path_decodes_lzw_edge_tiles() {
    let lzw = load_source_to_ram(&data(LZW), None).expect("LZW source must load");
    let reference = load_source_to_ram(&data(DEFLATE), None).unwrap();
    assert_eq!((lzw.width, lzw.height), (244, 128));
    assert_eq!(lzw.data.len(), reference.data.len());
    assert!(lzw.data == reference.data, "LZW samples differ from the DEFLATE copy");
    assert!(!lzw.data.contains(&VOID_ELEV));
    // Spot values in half-metres (truncated, f32_sample_to_half_metres):
    // pixel (0,0) = 400.00 m, (243,127) = 551.35 m.
    assert_eq!(lzw.data[0], 800);
    assert_eq!(lzw.data[127 * 244 + 243], 1102);
}

fn ingest(src: &str, out: &std::path::Path) -> Vec<u8> {
    let job: IngestJob = serde_json::from_value(serde_json::json!({
        "output_path": out,
        "format": "r16sint",
        "ul_lat": 47.0112, "ul_lon": 8.2930,
        "resolution_m": 2.0, "size_px": 512,
        "sources": [{"path": data(src), "crs": "EPSG:2056"}]
    }))
    .unwrap();
    process_tile_with_cache(job, Arc::new(Mutex::new(HashMap::new())))
        .unwrap_or_else(|e| panic!("ingest of {src} failed: {e:#}"));
    std::fs::read(out).unwrap()
}

#[test]
fn streaming_ingest_decodes_lzw_edge_tiles() {
    let dir = tempfile::tempdir().unwrap();
    let lzw = ingest(LZW, &dir.path().join("lzw.abt"));
    let reference = ingest(DEFLATE, &dir.path().join("deflate.abt"));
    assert!(lzw == reference, "LZW .abt differs from the DEFLATE one");
}
