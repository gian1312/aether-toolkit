//! Peak memory of a real `ingest` run is bounded by the streaming budgets,
//! not by the size of the source (CONTRACT changelog 23).
//!
//! One test, alone in its own test binary: it reads the peak RSS of the
//! converter child process through `getrusage(RUSAGE_CHILDREN)`, which covers
//! every child this process has waited for, so no other test may spawn one.
//! Unix only (that is where `getrusage` exists); it drives the CLI and
//! nothing else, so it runs unchanged against the pre-streaming converter —
//! which decoded the whole source (twice: raw, then converted) and fails it.

#![cfg(unix)]

mod common;

use std::path::Path;

use common::assemble;

/// A `n x n` Int16 GeoTIFF of 256x256 uncompressed tiles whose offsets all
/// point at ONE tile blob: a 512 MiB raster in a 128 KiB file. The decoder
/// still reads, and the converter still decodes and averages, every tile.
fn repeating_tiled_source(path: &Path, n: u32, src_deg: f64) {
    let t = 256u32;
    assert_eq!(n % t, 0);
    let blob: Vec<u8> = (0..t * t)
        .flat_map(|i| (((i % t) / 4 + (i / t) / 8) as i16 + 100).to_le_bytes())
        .collect();
    let longs = |v: &[u32]| -> Vec<u8> { v.iter().flat_map(|x| x.to_le_bytes()).collect() };
    let shorts = |v: &[u16]| -> Vec<u8> { v.iter().flat_map(|x| x.to_le_bytes()).collect() };
    let doubles = |v: &[f64]| -> Vec<u8> { v.iter().flat_map(|x| x.to_le_bytes()).collect() };
    let entries = vec![
        (256, 4, 1, longs(&[n])),
        (257, 4, 1, longs(&[n])),
        (258, 3, 1, shorts(&[16])),
        (259, 3, 1, shorts(&[1])),
        (262, 3, 1, shorts(&[1])),
        (277, 3, 1, shorts(&[1])),
        (322, 4, 1, longs(&[t])),
        (323, 4, 1, longs(&[t])),
        (339, 3, 1, shorts(&[2])),
        (33550, 12, 3, doubles(&[src_deg, src_deg, 0.0])),
        (33922, 12, 6, doubles(&[0.0, 0.0, 0.0, 8.0, 47.0, 0.0])),
        (34735, 3, 8, shorts(&[1, 1, 0, 1, 2048, 0, 1, 4326])),
    ];
    let n_tiles = ((n / t) * (n / t)) as usize;
    assemble(path, entries, &[blob], &vec![0usize; n_tiles], true);
}

/// Peak resident set of all waited-for children, in bytes.
fn children_max_rss() -> u64 {
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    assert_eq!(unsafe { libc::getrusage(libc::RUSAGE_CHILDREN, &mut ru) }, 0);
    let v = ru.ru_maxrss as u64;
    // Linux reports KiB; macOS reports bytes.
    if cfg!(target_os = "macos") { v } else { v * 1024 }
}

#[test]
fn ingest_peak_rss_stays_below_the_decoded_size_of_its_source() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("big_repeating.tif");
    let n = 16_384u32;
    let decoded = u64::from(n) * u64::from(n) * 2; // 512 MiB of Int16
    let out_px = 512u32;
    let pixel_deg = 30.0 / 111111.0;
    repeating_tiled_source(&src, n, pixel_deg / f64::from(n / out_px));

    let job = serde_json::json!({
        "output_path": dir.path().join("big.abt"),
        "ul_lat": 47.0, "ul_lon": 8.0,
        "resolution_m": 30.0, "size_px": out_px,
        "sources": [{"path": src}],
    });
    let started = std::time::Instant::now();
    let (ok, said) = common::run_cli(dir.path(), job);
    let elapsed = started.elapsed();
    assert!(ok, "the conversion itself failed:\n{said}");
    let rss = children_max_rss();
    println!(
        "source {} MiB decoded; converter peak RSS {} MiB; {:.1} s",
        decoded >> 20,
        rss >> 20,
        elapsed.as_secs_f64()
    );

    // Every output pixel averages its 32x32 block, so all of the source was
    // really decoded.
    let bytes = std::fs::read(dir.path().join("big.abt")).unwrap();
    let px = common::payload(&bytes, out_px as usize);
    assert!(px.iter().all(|&v| v > 0), "the source did not cover the tile");

    // The streaming budgets (128 MiB of band windows + a 192 MiB chunk cache)
    // plus the binary and output buffer stay well under the 512 MiB the
    // whole-file path needed just to hold the raster once.
    assert!(
        rss < decoded,
        "peak RSS {} MiB is not below the {} MiB decoded source — the converter \
         is still holding whole sources",
        rss >> 20,
        decoded >> 20
    );
}
