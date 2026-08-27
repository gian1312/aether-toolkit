//! What a `download` run says when it refuses, what it writes where no tile
//! arrived, and what geometry it accepts from a tile source.
//!
//! All contract surface (`docs/CONTRACT.md` §1.2 `download`, §6, §9b):
//!
//! * the fatal message names the error class — MapTiler's Terrain-RGB v2 serves
//!   WebP, and a bare "could not be fetched" reads as a network fault to anyone
//!   reading the process output for a decode-shaped problem;
//! * the run removes the `.abt` files it created. They are created with a valid
//!   header before the first fetch, so a refusal used to leave a complete,
//!   plausible-looking tile behind — and consumers pool tiles by filename, so
//!   the next run took it as a cache hit and built a coverage over it;
//! * a tile that never arrived is written as the `-9999` void sentinel, never
//!   as 0 m. 0 m is sea level, a perfectly valid elevation, so the old fill
//!   presented terrain that was never downloaded as surveyed ground;
//! * the XYZ tile edge comes from the PNG. MapTiler's terrain-rgb serves 512 px
//!   (`@2x`) tiles where Terrarium serves 256, and assuming 256 folded every
//!   512 px tile in half — west half onto the even output rows, east half onto
//!   the odd ones — for terrain that looked plausible and was ~555 m out.
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

/// Answers each tile request from `route`, which sees the requested `(z, x, y)`
/// and returns the PNG to serve or `None` for a 404. Returns the port.
///
/// Same one-request-per-connection discipline as [`serve`]; the difference is
/// that the response depends on *which* tile was asked for, which is what a
/// partially-covered run and a per-size comparison both need.
fn serve_tiles<F>(route: F) -> u16
where
    F: Fn(u32, u32, u32) -> Option<Vec<u8>> + Send + Sync + 'static,
{
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let route = std::sync::Arc::new(route);
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let route = route.clone();
            thread::spawn(move || {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut request = String::new();
                if reader.read_line(&mut request).unwrap_or(0) == 0 {
                    return;
                }
                let mut line = String::new();
                while reader.read_line(&mut line).unwrap_or(0) > 0 {
                    if line == "\r\n" || line == "\n" {
                        break;
                    }
                    line.clear();
                }
                // "GET /12/2134/1424.png HTTP/1.1"
                let path = request.split_whitespace().nth(1).unwrap_or("/").to_owned();
                let zxy: Vec<u32> = path
                    .trim_start_matches('/')
                    .trim_end_matches(".png")
                    .split('/')
                    .filter_map(|p| p.parse().ok())
                    .collect();
                let served = if zxy.len() == 3 { route(zxy[0], zxy[1], zxy[2]) } else { None };
                let (status, body) = match served {
                    Some(png) => ("200 OK", png),
                    None => ("404 Not Found", b"no tile here".to_vec()),
                };
                let head = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: image/png\r\n\
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

/// An 8-bit RGB PNG of the given size from raw `w*h*3` bytes.
fn rgb_png(w: u32, h: u32, px: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, w, h);
        enc.set_color(png::ColorType::Rgb);
        enc.set_depth(png::BitDepth::Eight);
        let mut writer = enc.write_header().unwrap();
        writer.write_image_data(px).unwrap();
    }
    out
}

/// A square Terrarium tile of `edge` px whose pixel `(px, py)` holds
/// `elev(px, py)` metres, encoded as `(r*256 + g + b/256) - 32768`.
fn terrarium_tile<F: Fn(usize, usize) -> f64>(edge: usize, elev: F) -> Vec<u8> {
    let mut px = Vec::with_capacity(edge * edge * 3);
    for y in 0..edge {
        for x in 0..edge {
            let t = (elev(x, y) + 32768.0).clamp(0.0, 65535.996);
            let r = (t / 256.0).floor();
            let g = (t - r * 256.0).floor();
            let b = ((t - r * 256.0 - g) * 256.0).round().clamp(0.0, 255.0);
            px.extend_from_slice(&[r as u8, g as u8, b as u8]);
        }
    }
    rgb_png(edge as u32, edge as u32, &px)
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

/// One `.abt` of `size_px` at `pd` degrees per pixel, NW corner given, in its
/// own `out/` under `dir`. Unlike [`write_job`] the extent is the caller's, so
/// it can be placed to straddle two XYZ tiles or to sit well inside one.
fn write_job_at(
    dir: &Path, port: u16, ul_lat: f64, ul_lon: f64, size_px: u32, pd: f64,
) -> (PathBuf, PathBuf) {
    let out_dir = dir.join("out");
    let job = serde_json::json!({
        "url_template": format!("http://127.0.0.1:{port}/{{z}}/{{x}}/{{y}}.png"),
        "encoding": "terrarium",
        "output_dir": out_dir,
        "zoom": 12,
        "max_connections": 4,
        "tiles": [{
            "filename": "tile.abt",
            "ul_lat": ul_lat,
            "ul_lon": ul_lon,
            "size_px": size_px,
            "resolution_m": pd * 111_111.0,
        }],
    });
    let job_path = dir.join("job.json");
    std::fs::write(&job_path, serde_json::to_vec_pretty(&job).unwrap()).unwrap();
    (job_path, out_dir.join("tile.abt"))
}

/// The `size`×`size` half-metre grid of a finished `.abt`, row-major.
fn read_abt(path: &Path, size: usize) -> Vec<i16> {
    let buf = std::fs::read(path).unwrap();
    assert_eq!(&buf[0..4], b"AETH");
    let stride = u16::from_le_bytes([buf[42], buf[43]]) as usize;
    assert_eq!(buf.len(), 44 + stride * size, "full-size tile: {path:?}");
    let mut g = Vec::with_capacity(size * size);
    for y in 0..size {
        for x in 0..size {
            let o = 44 + y * stride + x * 2;
            g.push(i16::from_le_bytes([buf[o], buf[o + 1]]));
        }
    }
    g
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

#[test]
fn the_half_no_tile_covered_is_void_and_the_half_that_arrived_is_terrain() {
    // One `.abt` straddling two XYZ tiles: the west one serves 100 m terrain,
    // the east one 404s. Losing half the tiles is not a majority, so the run
    // succeeds — and the output must say *where* the terrain is, rather than
    // reporting the missing half as a plain of sea-level ground.
    const Z: u32 = 12;
    let (tx, ty) = (lon2tx(8.5, Z), lat2ty(47.0, Z));
    let w = tx2lon(tx + 1, Z) - tx2lon(tx, Z);

    let port = serve_tiles(move |_z, x, _y| {
        (x == tx).then(|| terrarium_tile(256, |_, _| 100.0))
    });

    let dir = tempfile::tempdir().unwrap();
    let size = 64usize;
    let pd = w / size as f64; // one XYZ tile wide…
    let (job, abt) = write_job_at(
        dir.path(), port, ty2lat(ty, Z), tx2lon(tx, Z) + w * 0.5, size as u32, pd,
    ); // …starting at the middle of tile `tx`, so columns 32.. are tile `tx+1`.

    run_download(&job).unwrap();
    let g = read_abt(&abt, size);

    for y in 0..size {
        for x in 0..size {
            let v = g[y * size + x];
            if x < size / 2 {
                assert_eq!(v, 200, "({x},{y}) is under the tile that arrived: 100 m");
            } else {
                assert_eq!(v, -9999, "({x},{y}) had no tile: void, not sea level");
            }
        }
    }
    assert!(!g.contains(&0), "0 m is an elevation, never a marker for absence");
}

#[test]
fn a_512_px_source_lands_on_the_same_ground_as_a_256_px_one() {
    // MapTiler's terrain-rgb serves 512 px (@2x) tiles; Terrarium serves 256.
    // Both are the same ground at the same zoom — the larger tile just carries
    // finer samples — so the same terrain field served at either size must
    // assemble to the same `.abt`. Reading a 512 px tile with a 256 px row
    // stride instead folded it in half and moved pixels by hundreds of metres.
    const Z: u32 = 12;
    let (tx, ty) = (lon2tx(8.5, Z), lat2ty(47.0, Z));
    let w = tx2lon(tx + 1, Z) - tx2lon(tx, Z);
    let h = ty2lat(ty, Z) - ty2lat(ty + 1, Z);

    // A plane in tile space: sampling it at either density and area-averaging
    // over the same footprint gives the same answer, so any disagreement
    // between the two runs is the assembly placing samples differently.
    let (fx, fy) = (tx as f64, ty as f64);
    let field = move |ux: f64, uy: f64| 100.0 + 40.0 * (ux - fx) + 30.0 * (uy - fy);
    let source = move |edge: usize| {
        move |_z: u32, x: u32, y: u32| {
            Some(terrarium_tile(edge, move |px, py| {
                field(
                    x as f64 + (px as f64 + 0.5) / edge as f64,
                    y as f64 + (py as f64 + 0.5) / edge as f64,
                )
            }))
        }
    };

    // Well inside one XYZ tile, ~2 source samples per output cell at 256 px.
    let size = 48usize;
    let pd = 0.5 * h / size as f64;
    let (ul_lat, ul_lon) = (ty2lat(ty, Z) - 0.25 * h, tx2lon(tx, Z) + 0.25 * w);

    let dir256 = tempfile::tempdir().unwrap();
    let (job, abt) = write_job_at(
        dir256.path(), serve_tiles(source(256)), ul_lat, ul_lon, size as u32, pd,
    );
    run_download(&job).unwrap();
    let small = read_abt(&abt, size);

    let dir512 = tempfile::tempdir().unwrap();
    let (job, abt) = write_job_at(
        dir512.path(), serve_tiles(source(512)), ul_lat, ul_lon, size as u32, pd,
    );
    run_download(&job).unwrap();
    let large = read_abt(&abt, size);

    // The test would pass on any two constant tiles, so check the ramp is
    // there — and that both runs found real ground everywhere.
    let (lo, hi) = (*small.iter().min().unwrap(), *small.iter().max().unwrap());
    assert!(hi - lo > 20, "the terrain must actually vary: {lo}..{hi}");
    assert!(lo > 0, "no voids in a fully-covered extent: {lo}");

    let worst = small.iter().zip(&large)
        .map(|(a, b)| (*a as i32 - *b as i32).abs())
        .max().unwrap();
    assert!(worst <= 2, "512 px and 256 px disagree by {worst} half-metres");

    // The fold's fingerprint: it put the tile's west half on the even output
    // rows and its east half on the odd ones, so adjacent rows disagreed by
    // roughly the tile's whole east-west relief.
    let row_delta: i32 = (0..size - 1)
        .map(|y| (large[y * size] as i32 - large[(y + 1) * size] as i32).abs())
        .max().unwrap();
    assert!(row_delta <= 4, "adjacent rows differ by {row_delta} — the tile is folded");
}

#[test]
fn a_non_square_tile_is_refused_naming_its_dimensions() {
    // Tile geometry is a property of the service: a source answering with a
    // non-square tile answers every request that way, so this is not one lost
    // tile to average around — there is no stride to assemble it with at all.
    let px = vec![0u8; 256 * 128 * 3];
    let port = serve_tiles(move |_z, _x, _y| Some(rgb_png(256, 128, &px)));
    let dir = tempfile::tempdir().unwrap();
    let (job, abts) = write_job(dir.path(), port, 1);

    let msg = format!("{:#}", run_download(&job).unwrap_err());

    assert!(msg.contains("256x128"), "the dimensions must be named; got: {msg}");
    assert!(msg.contains("must be square"), "and the rule; got: {msg}");
    assert!(!abts[0].exists(), "a refused run left {:?} behind", abts[0]);
    assert_eq!(abt_files(&dir.path().join("out")), Vec::<String>::new());
}
