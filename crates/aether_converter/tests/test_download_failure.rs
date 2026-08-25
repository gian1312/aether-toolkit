//! What a `download` run says when it refuses, and what it leaves on disk.
//!
//! Both are contract surface (`docs/CONTRACT.md` §1.2 `download`):
//!
//! * the fatal message names the error class — MapTiler's Terrain-RGB v2 serves
//!   WebP, and a bare "could not be fetched" reads as a network fault to anyone
//!   reading the process output for a decode-shaped problem;
//! * the run removes the `.abt` files it created. They are created with a valid
//!   header before the first fetch and never-fetched tiles stay 0 m, so a
//!   refusal used to leave a complete, plausible-looking flat-sea tile behind —
//!   and consumers pool tiles by filename, so the next run took it as a cache
//!   hit and built a coverage over 0 m terrain.
//!
//! The tile source is a loopback HTTP server, so these tests are hermetic.

use std::io::{BufRead, BufReader, Write};
use std::net::{Ipv4Addr, TcpListener};
use std::path::{Path, PathBuf};
use std::thread;

use aether_converter::download::{lat2ty, lon2tx, run_download, tx2lon, ty2lat};

// ── A loopback tile source ────────────────────────────────────────────────

/// Answers every request with the same canned response and returns the port.
///
/// One request per connection: `Connection: close` keeps reqwest from pooling
/// a socket this server has already dropped, which would surface as a spurious
/// connection error instead of the class under test.
fn serve(status: &'static str, content_type: &'static str, body: Vec<u8>) -> u16 {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let body = body.clone();
            thread::spawn(move || {
                // Drain the request head; the response does not depend on it.
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                while reader.read_line(&mut line).unwrap_or(0) > 0 {
                    if line == "\r\n" || line == "\n" {
                        break;
                    }
                    line.clear();
                }
                let head = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(&body);
                let _ = stream.flush();
            });
        }
    });
    port
}

/// A WebP file header — what MapTiler Terrain-RGB v2 actually returns. Enough
/// bytes to be a plausible image and not a PNG.
fn webp_body() -> Vec<u8> {
    let mut b = Vec::from(*b"RIFF");
    b.extend_from_slice(&64u32.to_le_bytes());
    b.extend_from_slice(b"WEBPVP8 ");
    b.resize(72, 0);
    b
}

/// A 256×256 Terrarium PNG of uniform terrain. RGB 128,100,0 decodes as
/// `128*256 + 100 - 32768` = **100 m**, which is above the NODATA floor, so
/// the parent-tile backfill never fires and the job is exactly one fetch.
fn terrarium_png_100m() -> Vec<u8> {
    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, 256, 256);
        enc.set_color(png::ColorType::Rgb);
        enc.set_depth(png::BitDepth::Eight);
        let mut w = enc.write_header().unwrap();
        let px: Vec<u8> = [128u8, 100, 0].iter().copied().cycle().take(256 * 256 * 3).collect();
        w.write_image_data(&px).unwrap();
    }
    out
}

// ── The job ───────────────────────────────────────────────────────────────

/// Writes a job with `n` `.abt` outputs that all sit inside a **single** z12
/// XYZ tile (64 px × 10 m = 0.0058°, against a 0.088°-wide tile), so the run
/// is exactly one fetch and the tallies below are exact.
fn write_job(dir: &Path, port: u16, n: usize) -> (PathBuf, Vec<PathBuf>) {
    const Z: u32 = 12;
    let (tx, ty) = (lon2tx(8.5, Z), lat2ty(47.0, Z));
    let (w, h) = (tx2lon(tx + 1, Z) - tx2lon(tx, Z), ty2lat(ty, Z) - ty2lat(ty + 1, Z));

    let out_dir = dir.join("out");
    let names: Vec<String> = (0..n).map(|i| format!("tile_{i}.abt")).collect();
    let tiles: Vec<serde_json::Value> = names
        .iter()
        .enumerate()
        .map(|(i, name)| {
            let f = 0.2 + 0.25 * i as f64; // still inside the XYZ tile for n <= 3
            serde_json::json!({
                "filename": name,
                "ul_lat": ty2lat(ty, Z) - h * f,
                "ul_lon": tx2lon(tx, Z) + w * f,
                "size_px": 64,
                "resolution_m": 10.0,
            })
        })
        .collect();

    let job = serde_json::json!({
        "url_template": format!("http://127.0.0.1:{port}/{{z}}/{{x}}/{{y}}.png"),
        "encoding": "terrarium",
        "output_dir": out_dir,
        "zoom": Z,
        "max_connections": 4,
        "tiles": tiles,
    });
    let job_path = dir.join("job.json");
    std::fs::write(&job_path, serde_json::to_vec_pretty(&job).unwrap()).unwrap();

    (job_path, names.iter().map(|n| out_dir.join(n)).collect())
}

fn abt_files(dir: &Path) -> Vec<String> {
    let Ok(rd) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut names: Vec<String> = rd
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".abt"))
        .collect();
    names.sort();
    names
}

// ── Tests ─────────────────────────────────────────────────────────────────

#[test]
fn a_source_that_serves_webp_fails_naming_the_decode_class() {
    let port = serve("200 OK", "image/webp", webp_body());
    let dir = tempfile::tempdir().unwrap();
    let (job, abts) = write_job(dir.path(), port, 2);

    let msg = format!("{:#}", run_download(&job).unwrap_err());

    assert!(msg.starts_with("Tile download failed:"), "frozen prefix; got: {msg}");
    assert!(msg.contains("1 of 1 terrain tiles (100%)"), "one fetch expected; got: {msg}");
    assert!(msg.contains("decode=1"), "the failure class must be named; got: {msg}");
    assert!(!msg.contains("connect="), "nothing network-shaped happened; got: {msg}");

    // …and the refusal cached nothing reusable.
    for abt in &abts {
        assert!(!abt.exists(), "a refused run left {abt:?} for the next run to pool");
    }
    assert_eq!(abt_files(&dir.path().join("out")), Vec::<String>::new());
}

#[test]
fn a_source_that_404s_fails_naming_the_http_class() {
    // Same refusal, different class: the message must distinguish a zoom the
    // source does not serve from a source that serves the wrong image format.
    let port = serve("404 Not Found", "text/plain", b"nope".to_vec());
    let dir = tempfile::tempdir().unwrap();
    let (job, abts) = write_job(dir.path(), port, 1);

    let msg = format!("{:#}", run_download(&job).unwrap_err());

    assert!(msg.starts_with("Tile download failed:"), "frozen prefix; got: {msg}");
    assert!(msg.contains("HTTP_4xx=1"), "the failure class must be named; got: {msg}");
    assert!(!msg.contains("decode="), "the body was never decoded; got: {msg}");
    assert!(!abts[0].exists(), "a refused run left {:?} behind", abts[0]);
}

#[test]
fn a_successful_run_keeps_its_abt_files() {
    let port = serve("200 OK", "image/png", terrarium_png_100m());
    let dir = tempfile::tempdir().unwrap();
    let (job, abts) = write_job(dir.path(), port, 2);

    run_download(&job).unwrap();

    assert_eq!(abt_files(&dir.path().join("out")), vec!["tile_0.abt", "tile_1.abt"]);
    for abt in &abts {
        let buf = std::fs::read(abt).unwrap();
        assert_eq!(&buf[0..4], b"AETH");
        let stride = u16::from_le_bytes([buf[42], buf[43]]) as usize;
        assert_eq!(buf.len(), 44 + stride * 64, "full-size tile: {abt:?}");
        // 100 m of real terrain in half-metres — not the 0 m a lost tile writes.
        let centre = 44 + 32 * stride + 32 * 2;
        assert_eq!(
            i16::from_le_bytes([buf[centre], buf[centre + 1]]),
            200,
            "{abt:?} holds the fetched elevation"
        );
    }
}
