//! CLI `download`, changelog 38: the raw source-tile cache (`cache_dir`).
//!
//! A run with `cache_dir` looks every source tile up before any request,
//! decodes hits from the cached bytes, and writes every answer worth keeping
//! (an HTTP 200 body that decoded, a 404) into
//! `<cache_dir>/<service_key>/z<zoom>/x<x>_y<y/256>.pack`. Outputs, progress
//! and `[Stats] Tiles:` must be identical whether a tile came from the cache
//! or the network.
//!
//! Since changelog 40 a tile without an entry is DERIVED from the cached
//! tiles of up to four finer zooms when they cover it — the 2 × 2 means of
//! their samples, stored with status 3 — so a job never fetches ground the
//! cache holds at a finer zoom (section 7 below).
//!
//! Contract surface: `docs/CONTRACT.md` §9b (`cache_dir`, the pack format),
//! changelog 38 (`[Download] cache:`, `[Stats] Cache:`) and 40. The tile
//! source is a loopback HTTP server that counts every request, so these
//! tests assert on what went over the wire.

mod common;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use aether_converter::download::{run_download_tuned, tx2lon, ty2lat, DownloadTuning};
use common::{read_dir_bytes, serve_tiles, serve_tiles_delayed, terrarium_png, tile_ground, TileCounts};

const Z: u32 = 12;
/// The block's first column and row: rows 1534..=1537 straddle the pack
/// boundary at y = 1536 = 6 · 256 (yblocks 5 and 6).
const TX0: u32 = 2139;
const TY0: u32 = 1534;
/// The pack format of CONTRACT §9b.
const HEADER_LEN: usize = 18;
const ENTRY_LEN: usize = 20;
const DATA_START: usize = 5138;

/// `(north, south, west, east)` of z12 tile `(x, y)`.
fn tile_box(x: u32, y: u32) -> (f64, f64, f64, f64) {
    (ty2lat(y, Z), ty2lat(y + 1, Z), tx2lon(x, Z), tx2lon(x + 1, Z))
}

fn spec(name: &str, lat: f64, lon: f64, side_deg: f64, px: u32) -> serde_json::Value {
    let pd = side_deg / px as f64;
    serde_json::json!({
        "filename": name, "ul_lat": lat, "ul_lon": lon, "size_px": px, "resolution_m": pd * 111_111.0,
    })
}

/// The specs of a `cols × rows` block of z12 tiles from `(x0, y0)`: a 16 px
/// spec inside each tile — so the fetch set is exactly the block — and a
/// 64 px spec across the tiles `x0..=x0+1 × y0+1..=y0+2`, so one output
/// samples several tiles (across the pack boundary when `y0` = [`TY0`]).
/// Needs `cols >= 2`, `rows >= 3`.
fn block_specs(x0: u32, y0: u32, cols: u32, rows: u32) -> Vec<serde_json::Value> {
    let mut v = Vec::new();
    for y in y0..y0 + rows {
        for x in x0..x0 + cols {
            let (n, s, w, e) = tile_box(x, y);
            v.push(spec(&format!("t_{x}_{y}.abt"), n - (n - s) * 0.25, w + (e - w) * 0.25, (e - w) * 0.25, 16));
        }
    }
    let (n, s, w, e) = tile_box(x0, y0 + 1);
    v.push(spec("across.abt", (n + s) / 2.0, (w + e) / 2.0, e - w, 64));
    v
}

/// A job over `specs` from the server on `port`, output `<dir>/<name>`, with
/// the fields of `extra` merged in.
fn write_job(
    dir: &Path, port: u16, name: &str, zoom: u32, specs: &[serde_json::Value], extra: serde_json::Value,
) -> (PathBuf, PathBuf) {
    let out_dir = dir.join(name);
    let mut job = serde_json::json!({
        "url_template": format!("http://127.0.0.1:{port}/{{z}}/{{x}}/{{y}}.png"),
        "encoding": "terrarium", "output_dir": out_dir, "zoom": zoom,
        "max_connections": 16, "tiles": specs,
    });
    if let (Some(j), Some(e)) = (job.as_object_mut(), extra.as_object()) {
        for (k, v) in e {
            j.insert(k.clone(), v.clone());
        }
    }
    let path = dir.join(format!("{name}.json"));
    std::fs::write(&path, serde_json::to_vec_pretty(&job).unwrap()).unwrap();
    (path, out_dir)
}

fn cache_field(dir: &Path) -> serde_json::Value {
    serde_json::json!({ "cache_dir": dir })
}

/// Spawn the real CLI on `job`, with `cwd` as its working directory.
fn spawn(job: &Path, cwd: &Path) -> std::process::Child {
    Command::new(env!("CARGO_BIN_EXE_aether_converter"))
        .args(["download", "--job-file"]).arg(job)
        .current_dir(cwd)
        .stdout(Stdio::null()).stderr(Stdio::piped())
        .spawn().unwrap()
}

/// Run the real CLI on `job`; `(exit ok, stderr)`.
fn run(job: &Path, cwd: &Path) -> (bool, String) {
    let out = spawn(job, cwd).wait_with_output().unwrap();
    (out.status.success(), String::from_utf8_lossy(&out.stderr).into_owned())
}

fn run_ok(job: &Path, cwd: &Path) -> String {
    let (ok, err) = run(job, cwd);
    assert!(ok, "download failed:\n{err}");
    err
}

/// The one line of `err` starting with `prefix`.
fn line<'a>(err: &'a str, prefix: &str) -> &'a str {
    let found: Vec<&str> = err.lines().filter(|l| l.starts_with(prefix)).collect();
    assert_eq!(found.len(), 1, "want exactly one `{prefix}…` line; stderr:\n{err}");
    found[0]
}

fn has_line(err: &str, prefix: &str) -> bool {
    err.lines().any(|l| l.starts_with(prefix))
}

/// The pass total: the 4th number of `[Download] z=<z> tiles=<nx>x<ny>=<n> …`.
fn fetch_total(err: &str) -> usize {
    let l = err.lines().find(|l| l.starts_with("[Download] z=")).unwrap_or_else(|| panic!("no header:\n{err}"));
    let t = l.split("tiles=").nth(1).unwrap().split_whitespace().next().unwrap();
    t.rsplit('=').next().unwrap().parse().unwrap()
}

/// The `[Download] <pct>% (<done>/<total>)` prefixes, in order.
fn progress(err: &str) -> Vec<String> {
    err.lines()
        .filter(|l| l.starts_with("[Download] ") && l.contains("% (") && l.contains(") — "))
        .map(|l| l.split(" — ").next().unwrap().to_owned())
        .collect()
}

fn requests(c: &TileCounts) -> usize {
    c.lock().unwrap().values().sum()
}

fn requests_at(c: &TileCounts, z: u32) -> usize {
    c.lock().unwrap().iter().filter(|(k, _)| k.0 == z).map(|(_, n)| *n).sum()
}

/// A route serving each tile's PNG, encoded once per tile.
fn memo(f: impl Fn(u32, u32, u32) -> Option<Vec<u8>> + Send + Sync + 'static)
    -> impl Fn(u32, u32, u32) -> Option<Vec<u8>> + Send + Sync + 'static {
    let seen: Mutex<HashMap<(u32, u32, u32), Option<Vec<u8>>>> = Mutex::new(HashMap::new());
    move |z, x, y| {
        if let Some(v) = seen.lock().unwrap().get(&(z, x, y)) {
            return v.clone();
        }
        let v = f(z, x, y);
        seen.lock().unwrap().insert((z, x, y), v.clone());
        v
    }
}

/// Every zoom-`Z` tile exists; nothing else does.
fn full_server() -> (u16, TileCounts) {
    serve_tiles(memo(|z, x, y| (z == Z).then(|| terrarium_png(tile_ground(z, x, y)))))
}

/// The cache's service directories: `<cache_dir>/<16 hex>/`, each with its
/// `service.json` read back.
fn services(cache: &Path) -> Vec<(PathBuf, serde_json::Value)> {
    let mut v: Vec<(PathBuf, serde_json::Value)> = std::fs::read_dir(cache).unwrap()
        .map(|e| e.unwrap().path())
        .map(|p| {
            let name = p.file_name().unwrap().to_string_lossy().into_owned();
            assert!(name.len() == 16 && name.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
                    "a service directory is 16 lower-case hex digits: {name}");
            let info = serde_json::from_slice(&std::fs::read(p.join("service.json")).unwrap()).unwrap();
            (p, info)
        })
        .collect();
    v.sort_by(|a, b| a.0.cmp(&b.0));
    v
}

/// The `.pack` files of one zoom directory, by name.
fn packs(service: &Path, zoom: u32) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(service.join(format!("z{zoom}"))).unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    v.sort();
    v
}

/// Index entry `(offset, length, status, crc)` of tile `y` in a pack.
fn entry(pack: &[u8], y: u32) -> (u64, u32, u8, u32) {
    let o = HEADER_LEN + (y % 256) as usize * ENTRY_LEN;
    let e = &pack[o..o + ENTRY_LEN];
    (
        u64::from_le_bytes(e[0..8].try_into().unwrap()),
        u32::from_le_bytes(e[8..12].try_into().unwrap()),
        e[12],
        u32::from_le_bytes(e[16..20].try_into().unwrap()),
    )
}

fn crc32(b: &[u8]) -> u32 {
    let mut c = flate2::Crc::new();
    c.update(b);
    c.sum()
}

// ── 1. Fetched once, then never again ───────────────────────────────────────

#[test]
fn a_second_run_fetches_nothing_and_writes_the_same_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("cache");
    let (port, counts) = full_server();
    let specs = block_specs(TX0, TY0, 3, 4);

    // Run A: an empty cache. Every tile is fetched once and written.
    let (job_a, out_a) = write_job(dir.path(), port, "a", Z, &specs, cache_field(&cache));
    let err_a = run_ok(&job_a, dir.path());
    let n = fetch_total(&err_a);
    assert_eq!(n, 12, "the 3 × 4 block");
    assert_eq!(requests(&counts), n);
    assert!(counts.lock().unwrap().values().all(|&c| c == 1), "each tile once");
    assert_eq!(line(&err_a, "[Download] cache:"), format!("[Download] cache: 0 of {n} source tiles from cache (0 derived)"));
    assert_eq!(line(&err_a, "[Stats] Cache:"),
               format!("[Stats] Cache: 0 hits (0 derived), {n} fetched, {n} written to {}", cache.display()));
    assert_eq!(line(&err_a, "[Stats] Tiles:"), format!("[Stats] Tiles: {n}/{n} OK (100.0% success)"));
    // The `[Stats] Cache:` line is the block's last [Stats] line.
    let last_stats = err_a.lines().filter(|l| l.starts_with("[Stats] ")).last().unwrap();
    assert!(last_stats.starts_with("[Stats] Cache:"), "{last_stats}");

    // The layout: one service, its service.json, a pack per (x, y / 256).
    let svc = services(&cache);
    assert_eq!(svc.len(), 1);
    assert_eq!(svc[0].1, serde_json::json!({
        "url_template": format!("http://127.0.0.1:{port}/{{z}}/{{x}}/{{y}}.png"), "encoding": "terrarium",
    }));
    let want: Vec<String> = {
        let mut w: Vec<String> = (TX0..TX0 + 3)
            .flat_map(|x| [5, 6].map(|b| format!("x{x}_y{b}.pack")))
            .collect();
        w.sort();
        w
    };
    assert_eq!(packs(&svc[0].0, Z), want, "rows 1534..1537 are yblocks 5 and 6");
    let pack = std::fs::read(svc[0].0.join("z12").join(format!("x{TX0}_y5.pack"))).unwrap();
    assert_eq!(&pack[0..4], b"AETP");
    let (off, len, status, crc) = entry(&pack, TY0);
    assert_eq!(status, 1);
    assert!(off >= DATA_START as u64);
    let body = &pack[off as usize..off as usize + len as usize];
    assert_eq!(body, terrarium_png(tile_ground(Z, TX0, TY0)), "the raw PNG, as served");
    assert_eq!(crc, crc32(body));

    // Run B: same job, fresh output directory. No request at all.
    let (job_b, out_b) = write_job(dir.path(), port, "b", Z, &specs, cache_field(&cache));
    let err_b = run_ok(&job_b, dir.path());
    assert_eq!(requests(&counts), n, "the cached run made no request:\n{err_b}");
    assert_eq!(fetch_total(&err_b), n);
    assert_eq!(line(&err_b, "[Download] cache:"), format!("[Download] cache: {n} of {n} source tiles from cache (0 derived)"));
    assert_eq!(line(&err_b, "[Stats] Cache:"),
               format!("[Stats] Cache: {n} hits (0 derived), 0 fetched, 0 written to {}", cache.display()));
    assert_eq!(line(&err_b, "[Stats] Tiles:"), line(&err_a, "[Stats] Tiles:"));
    assert_eq!(progress(&err_b), progress(&err_a), "the same progress lines");
    assert!(!err_b.contains("[Warn]"), "{err_b}");
    let (a, b) = (read_dir_bytes(&out_a), read_dir_bytes(&out_b));
    assert_eq!(a.len(), specs.len());
    assert!(a == b, "the cached run wrote different .abt bytes");

    // Control: the same job without a cache writes the same bytes too.
    let (job_c, out_c) = write_job(dir.path(), port, "c", Z, &specs, serde_json::json!({}));
    let err_c = run_ok(&job_c, dir.path());
    assert!(!has_line(&err_c, "[Download] cache:") && !has_line(&err_c, "[Stats] Cache"), "{err_c}");
    assert_eq!(requests(&counts), 2 * n);
    assert!(read_dir_bytes(&out_c) == a, "the cache changed the output");
}

// ── 2. Zooms and services keep to their own directories ─────────────────────

#[test]
fn another_zoom_and_another_service_get_their_own_directories() {
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("cache");
    let both = |z: u32, x: u32, y: u32| (z == 12 || z == 13).then(|| terrarium_png(tile_ground(z, x, y)));
    let (port1, counts1) = serve_tiles(memo(both));
    let (port2, counts2) = serve_tiles(memo(both));
    let specs = block_specs(TX0, TY0, 2, 3);

    let (j12, _) = write_job(dir.path(), port1, "s1z12", 12, &specs, cache_field(&cache));
    let e12 = run_ok(&j12, dir.path());
    let (j13, _) = write_job(dir.path(), port1, "s1z13", 13, &specs, cache_field(&cache));
    let e13 = run_ok(&j13, dir.path());
    assert_eq!(line(&e13, "[Download] cache:"),
               format!("[Download] cache: 0 of {} source tiles from cache (0 derived)", fetch_total(&e13)),
               "z12 tiles are no z13 tiles");
    assert_eq!(requests_at(&counts1, 12), fetch_total(&e12));
    assert_eq!(requests_at(&counts1, 13), fetch_total(&e13));

    // The same tiles from another service: its own directory, no hit.
    let (j2, _) = write_job(dir.path(), port2, "s2z12", 12, &specs, cache_field(&cache));
    let e2 = run_ok(&j2, dir.path());
    assert_eq!(line(&e2, "[Download] cache:"),
               format!("[Download] cache: 0 of {} source tiles from cache (0 derived)", fetch_total(&e2)));
    assert_eq!(requests(&counts2), fetch_total(&e2));

    let svc = services(&cache);
    assert_eq!(svc.len(), 2, "one directory per service");
    let of_port = |p: u16| svc.iter()
        .find(|(_, i)| i["url_template"] == format!("http://127.0.0.1:{p}/{{z}}/{{x}}/{{y}}.png"))
        .map(|(d, _)| d.clone()).expect("service.json names its template");
    let (d1, d2) = (of_port(port1), of_port(port2));
    let zooms = |d: &Path| {
        let mut v: Vec<String> = std::fs::read_dir(d).unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        v.sort();
        v
    };
    assert_eq!(zooms(&d1), ["service.json", "z12", "z13"]);
    assert_eq!(zooms(&d2), ["service.json", "z12"]);
    // z13 rows 3068..3074 (the z12 rows doubled) are yblock 11 and 12 (3072 = 12 · 256).
    assert!(packs(&d1, 13).iter().all(|p| p.ends_with("_y11.pack") || p.ends_with("_y12.pack")),
            "{:?}", packs(&d1, 13));

    // A second encoding of the first service is a service of its own too.
    let (jm, _) = write_job(dir.path(), port1, "s1z12mapbox", 12, &specs,
                            serde_json::json!({ "cache_dir": cache, "encoding": "mapbox" }));
    let em = run_ok(&jm, dir.path());
    assert!(line(&em, "[Download] cache:").starts_with("[Download] cache: 0 of "), "{em}");
    assert_eq!(services(&cache).len(), 3);

    // And each one is still a hit for its own job.
    let (j12b, _) = write_job(dir.path(), port1, "s1z12b", 12, &specs, cache_field(&cache));
    let n = fetch_total(&e12);
    assert_eq!(line(&run_ok(&j12b, dir.path()), "[Download] cache:"),
               format!("[Download] cache: {n} of {n} source tiles from cache (0 derived)"));
}

// ── 3. A 404 is remembered — and so are the parents that filled it ──────────

#[test]
fn not_found_tiles_and_their_parent_fill_are_remembered() {
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("cache");
    // Every third z12 tile 404s; z11 has data (it repairs them), z10…z6 404.
    let gap = |x: u32, y: u32| (x + y) % 3 == 0;
    let (port, counts) = serve_tiles(memo(move |z, x, y| match z {
        12 if !gap(x, y) => Some(terrarium_png(tile_ground(z, x, y))),
        11 => Some(terrarium_png(tile_ground(z, x, y))),
        _ => None,
    }));
    let specs = block_specs(TX0, TY0, 3, 4);
    let (job_a, out_a) = write_job(dir.path(), port, "a", Z, &specs, cache_field(&cache));
    let err_a = run_ok(&job_a, dir.path());
    let n = fetch_total(&err_a);
    let gaps = (TX0..TX0 + 3).flat_map(|x| (TY0..TY0 + 4).map(move |y| (x, y))).filter(|&(x, y)| gap(x, y)).count();
    assert!(gaps > 0);
    assert_eq!(line(&err_a, "[Stats] Repaired from coarser parent tiles:"),
               format!("[Stats] Repaired from coarser parent tiles: {gaps} (missing at this zoom, present at a lower one)"));
    assert_eq!(line(&err_a, "[Stats] Cache:"),
               format!("[Stats] Cache: 0 hits (0 derived), {n} fetched, {n} written to {}", cache.display()),
               "a 404 is written as well");
    let parents_a = requests(&counts) - requests_at(&counts, 12);
    assert!(parents_a > 0);
    assert_eq!(line(&err_a, "[Stats] Cache, coarser parents:"),
               format!("[Stats] Cache, coarser parents: 0 hits, {parents_a} fetched, {parents_a} written"));
    // A 404 entry: status 2, no bytes.
    let svc = services(&cache).remove(0).0;
    let (gx, gy) = (TX0..TX0 + 3).flat_map(|x| (TY0..TY0 + 4).map(move |y| (x, y))).find(|&(x, y)| gap(x, y)).unwrap();
    let pack = std::fs::read(svc.join("z12").join(format!("x{gx}_y{}.pack", gy / 256))).unwrap();
    assert_eq!(entry(&pack, gy), (0, 0, 2, 0));

    // Run B: not one request — neither for the 404 tiles nor their parents.
    let before = requests(&counts);
    let (job_b, out_b) = write_job(dir.path(), port, "b", Z, &specs, cache_field(&cache));
    let err_b = run_ok(&job_b, dir.path());
    assert_eq!(requests(&counts), before, "the cached run made requests:\n{err_b}");
    assert_eq!(line(&err_b, "[Download] cache:"), format!("[Download] cache: {n} of {n} source tiles from cache (0 derived)"));
    assert_eq!(line(&err_b, "[Stats] Cache, coarser parents:"),
               format!("[Stats] Cache, coarser parents: {parents_a} hits, 0 fetched, 0 written"));
    for p in ["[Stats] Tiles:", "[Stats] Repaired from coarser parent tiles:"] {
        assert_eq!(line(&err_b, p), line(&err_a, p));
    }
    assert!(read_dir_bytes(&out_a) == read_dir_bytes(&out_b), "the cached 404s changed the output");
}

#[test]
fn a_hole_with_no_parent_is_remembered_as_one() {
    // Nothing at all exists for half the tiles at any zoom: NO-DATA voids,
    // which the second run must reproduce from the cache without asking.
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("cache");
    let (port, counts) = serve_tiles(memo(|z, x, y| (z == Z && x % 2 == 0).then(|| terrarium_png(tile_ground(z, x, y)))));
    let specs = block_specs(TX0, TY0, 3, 4);
    let (job_a, out_a) = write_job(dir.path(), port, "a", Z, &specs, cache_field(&cache));
    let err_a = run_ok(&job_a, dir.path());
    assert!(has_line(&err_a, "[Stats] NO-DATA (HTTP 404):"), "{err_a}");
    let before = requests(&counts);
    let (job_b, out_b) = write_job(dir.path(), port, "b", Z, &specs, cache_field(&cache));
    let err_b = run_ok(&job_b, dir.path());
    assert_eq!(requests(&counts), before, "{err_b}");
    for p in ["[Stats] Tiles:", "[Stats] NO-DATA (HTTP 404):", "[Stats] ERRORS"] {
        assert_eq!(line(&err_b, p), line(&err_a, p));
    }
    assert!(read_dir_bytes(&out_a) == read_dir_bytes(&out_b));
}

// ── 4. A bad entry is a miss: refetched and rewritten ────────────────────────

#[test]
fn a_corrupt_or_undecodable_entry_is_refetched_and_repaired() {
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("cache");
    let (port, counts) = full_server();
    let specs = block_specs(TX0, TY0, 3, 4);
    let (job_a, out_a) = write_job(dir.path(), port, "a", Z, &specs, cache_field(&cache));
    let n = fetch_total(&run_ok(&job_a, dir.path()));
    let svc = services(&cache).remove(0).0;
    let path_of = |x: u32, y: u32| svc.join("z12").join(format!("x{x}_y{}.pack", y / 256));

    // (a) An entry pointing at its neighbour's body — a whole, decodable PNG,
    //     so only the CRC tells it is not this tile's.
    let (bx, by) = (TX0 + 1, TY0 + 2);
    let mut pack = std::fs::read(path_of(bx, by)).unwrap();
    let (n_off, n_len, _, _) = entry(&pack, by + 1);
    let o = HEADER_LEN + (by % 256) as usize * ENTRY_LEN;
    pack[o..o + 8].copy_from_slice(&n_off.to_le_bytes());
    pack[o + 8..o + 12].copy_from_slice(&n_len.to_le_bytes());
    let size_before = pack.len();
    std::fs::write(path_of(bx, by), &pack).unwrap();
    // (c) One flipped byte in a third body.
    let (fx, fy) = (TX0, TY0 + 1);
    let mut pack3 = std::fs::read(path_of(fx, fy)).unwrap();
    let (off, len, _, _) = entry(&pack3, fy);
    pack3[off as usize + len as usize / 2] ^= 0x55;
    std::fs::write(path_of(fx, fy), &pack3).unwrap();
    // (b) Another entry repointed at bytes with a valid CRC that are no PNG.
    let (ux, uy) = (TX0 + 2, TY0);
    let mut pack2 = std::fs::read(path_of(ux, uy)).unwrap();
    let junk = b"<html>not a tile</html>".to_vec();
    let junk_at = pack2.len() as u64;
    pack2.extend_from_slice(&junk);
    let o = HEADER_LEN + (uy % 256) as usize * ENTRY_LEN;
    pack2[o..o + 8].copy_from_slice(&junk_at.to_le_bytes());
    pack2[o + 8..o + 12].copy_from_slice(&(junk.len() as u32).to_le_bytes());
    pack2[o + 16..o + 20].copy_from_slice(&crc32(&junk).to_le_bytes());
    std::fs::write(path_of(ux, uy), &pack2).unwrap();

    let before = counts.lock().unwrap().clone();
    let (job_b, out_b) = write_job(dir.path(), port, "b", Z, &specs, cache_field(&cache));
    let err_b = run_ok(&job_b, dir.path());
    let after = counts.lock().unwrap().clone();
    let mut asked: Vec<(u32, u32, u32)> = after.iter()
        .filter(|(k, v)| before.get(k).copied().unwrap_or(0) < **v).map(|(k, _)| *k).collect();
    asked.sort();
    let mut want = vec![(Z, bx, by), (Z, ux, uy), (Z, fx, fy)];
    want.sort();
    assert_eq!(asked, want, "only the three bad entries went to the server");
    // All three were found in the index; they failed when read.
    assert_eq!(line(&err_b, "[Download] cache:"), format!("[Download] cache: {n} of {n} source tiles from cache (0 derived)"));
    assert_eq!(line(&err_b, "[Stats] Cache:"),
               format!("[Stats] Cache: {} hits (0 derived), 3 fetched, 3 written to {}", n - 3, cache.display()));
    assert_eq!(line(&err_b, "[Stats] Tiles:"), format!("[Stats] Tiles: {n}/{n} OK (100.0% success)"));
    assert!(read_dir_bytes(&out_a) == read_dir_bytes(&out_b));

    // Rewritten by appending: the entry points past the old end, the bad
    // bytes stay behind as garbage.
    let pack = std::fs::read(path_of(bx, by)).unwrap();
    let (off, len, status, crc) = entry(&pack, by);
    assert_eq!((status, off as usize), (1, size_before));
    assert_eq!(pack.len(), size_before + len as usize);
    assert_eq!(crc, crc32(&terrarium_png(tile_ground(Z, bx, by))));

    // Run C: whole again.
    let (job_c, out_c) = write_job(dir.path(), port, "c", Z, &specs, cache_field(&cache));
    let err_c = run_ok(&job_c, dir.path());
    assert_eq!(*counts.lock().unwrap(), after, "{err_c}");
    assert_eq!(line(&err_c, "[Stats] Cache:"),
               format!("[Stats] Cache: {n} hits (0 derived), 0 fetched, 0 written to {}", cache.display()));
    assert!(read_dir_bytes(&out_c) == read_dir_bytes(&out_a));
}

// ── 5. Two processes on one cache ────────────────────────────────────────────

#[test]
fn two_concurrent_downloads_share_one_cache_cleanly() {
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("cache");
    // 30 ms per answer so that the two runs overlap for their whole length
    // and write the same packs at the same time.
    let tiles = memo(|z, x, y| (z == Z).then(|| terrarium_png(tile_ground(z, x, y))));
    let (port, counts) = serve_tiles_delayed(move |z, x, y| tiles(z, x, y).map(|png| (png, 30)));
    let specs = block_specs(TX0, TY0, 6, 8);
    let extra = serde_json::json!({ "cache_dir": cache, "max_connections": 4 });
    let (job_a, out_a) = write_job(dir.path(), port, "a", Z, &specs, extra.clone());
    let (job_b, out_b) = write_job(dir.path(), port, "b", Z, &specs, extra.clone());
    let (pa, pb) = (spawn(&job_a, dir.path()), spawn(&job_b, dir.path()));
    let (oa, ob) = (pa.wait_with_output().unwrap(), pb.wait_with_output().unwrap());
    let (err_a, err_b) = (String::from_utf8_lossy(&oa.stderr), String::from_utf8_lossy(&ob.stderr));
    assert!(oa.status.success(), "{err_a}");
    assert!(ob.status.success(), "{err_b}");
    let n = fetch_total(&err_a);
    for err in [&err_a, &err_b] {
        assert!(!err.contains("[Warn]"), "{err}");
        assert_eq!(line(err, "[Stats] Tiles:"), format!("[Stats] Tiles: {n}/{n} OK (100.0% success)"));
    }
    let fetched = requests(&counts);
    assert!(fetched >= n && fetched <= 2 * n, "{fetched}");
    assert!(read_dir_bytes(&out_a) == read_dir_bytes(&out_b), "the concurrent runs disagree");

    // Every entry both wrote is whole: a third run is all hits.
    let (job_c, out_c) = write_job(dir.path(), port, "c", Z, &specs, extra);
    let err_c = run_ok(&job_c, dir.path());
    assert_eq!(requests(&counts), fetched, "{err_c}");
    assert_eq!(line(&err_c, "[Stats] Cache:"),
               format!("[Stats] Cache: {n} hits (0 derived), 0 fetched, 0 written to {}", cache.display()));
    assert!(read_dir_bytes(&out_c) == read_dir_bytes(&out_a));

    // Control without a cache.
    let (job_d, out_d) = write_job(dir.path(), port, "d", Z, &specs, serde_json::json!({}));
    run_ok(&job_d, dir.path());
    assert!(read_dir_bytes(&out_d) == read_dir_bytes(&out_a));
}

// ── A query string: its own directory, never on disk in clear text ─────────

#[test]
fn every_query_gets_its_own_directory_and_no_key_reaches_the_disk() {
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("cache");
    let (port, counts) = full_server();
    let specs = block_specs(TX0, TY0, 2, 3);
    let base = format!("http://127.0.0.1:{port}/{{z}}/{{x}}/{{y}}.png");
    let keyed = |key: &str| serde_json::json!({ "cache_dir": cache, "url_template": format!("{base}?key={key}") });

    let (job_a, out_a) = write_job(dir.path(), port, "a", Z, &specs, keyed("first-secret"));
    let err_a = run_ok(&job_a, dir.path());
    let n = fetch_total(&err_a);
    assert_eq!(requests(&counts), n, "{err_a}");
    // Another query is another service to the cache (a `?layer=` could name
    // other terrain): its own directory, so every tile is fetched again.
    let (job_b, out_b) = write_job(dir.path(), port, "b", Z, &specs, keyed("second-secret"));
    let err_b = run_ok(&job_b, dir.path());
    assert_eq!(requests(&counts), 2 * n, "{err_b}");
    assert_eq!(line(&err_b, "[Download] cache:"), format!("[Download] cache: 0 of {n} source tiles from cache (0 derived)"));
    assert!(read_dir_bytes(&out_a) == read_dir_bytes(&out_b));
    // The first query again: its own directory is whole.
    let (job_c, out_c) = write_job(dir.path(), port, "c", Z, &specs, keyed("first-secret"));
    let err_c = run_ok(&job_c, dir.path());
    assert_eq!(requests(&counts), 2 * n, "{err_c}");
    assert_eq!(line(&err_c, "[Download] cache:"), format!("[Download] cache: {n} of {n} source tiles from cache (0 derived)"));
    assert!(read_dir_bytes(&out_c) == read_dir_bytes(&out_a));

    // Two directories; each service.json records the template without its
    // query — no `?`, no key — and no file or directory name carries one.
    let svc = services(&cache);
    assert_eq!(svc.len(), 2, "one directory per query");
    for (d, info) in &svc {
        assert_eq!(*info, serde_json::json!({ "url_template": base, "encoding": "terrarium" }));
        let raw = std::fs::read_to_string(d.join("service.json")).unwrap();
        assert!(!raw.contains('?') && !raw.contains("secret"), "{raw}");
    }
    let mut stack = vec![cache.clone()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).unwrap() {
            let p = e.unwrap().path();
            assert!(!p.to_string_lossy().contains("secret") && !p.to_string_lossy().contains('?'), "{p:?}");
            if p.is_dir() {
                stack.push(p);
            }
        }
    }
}

// ── 6. No cache_dir: nothing changes, nothing is created ─────────────────────

#[test]
fn without_cache_dir_nothing_is_created_and_nothing_changes() {
    let (port, counts) = full_server();
    let specs = block_specs(TX0, TY0, 2, 3);
    let mut outputs = Vec::new();
    for (name, extra) in [
        ("absent", serde_json::json!({})),
        ("null", serde_json::json!({ "cache_dir": null })),
        ("empty", serde_json::json!({ "cache_dir": "" })),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let before = requests(&counts);
        let (job, out) = write_job(dir.path(), port, name, Z, &specs, extra);
        let err = run_ok(&job, dir.path());
        for word in ["[Download] cache", "[Stats] Cache", "cache_dir"] {
            assert!(!err.contains(word), "{name}: {err}");
        }
        assert_eq!(requests(&counts) - before, fetch_total(&err), "{name}: every tile fetched");
        // Only the job file and the output directory — in the job's
        // directory and in the converter's working directory alike.
        let mut here: Vec<String> = std::fs::read_dir(dir.path()).unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        here.sort();
        assert_eq!(here, [name.to_string(), format!("{name}.json")], "{name}");
        outputs.push(read_dir_bytes(&out));
    }
    assert!(outputs.windows(2).all(|w| w[0] == w[1]));
}

#[test]
fn an_unusable_cache_dir_is_one_warning_and_the_run_goes_on_without_it() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("a_file");
    std::fs::write(&file, b"not a directory").unwrap();
    let bad = file.join("cache");
    let (port, counts) = full_server();
    let specs = block_specs(TX0, TY0, 2, 3);
    let (job, out) = write_job(dir.path(), port, "bad", Z, &specs, cache_field(&bad));
    let err = run_ok(&job, dir.path());
    let warn = line(&err, "[Warn] cache_dir ");
    assert!(warn.starts_with(&format!("[Warn] cache_dir {} cannot be used (", bad.display())), "{warn}");
    assert!(warn.ends_with("); downloading without cache"), "{warn}");
    assert!(!has_line(&err, "[Download] cache:") && !has_line(&err, "[Stats] Cache"), "{err}");
    assert_eq!(requests(&counts), fetch_total(&err));
    let (job_c, out_c) = write_job(dir.path(), port, "control", Z, &specs, serde_json::json!({}));
    run_ok(&job_c, dir.path());
    assert!(read_dir_bytes(&out) == read_dir_bytes(&out_c));
}

// ── Repair jobs bypass the cache for their tiles and rewrite their entries ──

#[test]
fn a_repair_refetches_its_listed_tile_and_replaces_the_cached_answer() {
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("cache");
    let (tx, ty) = (TX0 + 1, TY0 + 1);
    let changed = Arc::new(AtomicBool::new(false));
    let c2 = changed.clone();
    // After `changed`, tile (tx, ty) answers with other terrain.
    let (port, counts) = serve_tiles(move |z, x, y| {
        (z == Z).then(|| {
            let lift = if c2.load(Ordering::SeqCst) && (x, y) == (tx, ty) { 500 } else { 0 };
            let g = tile_ground(z, x, y);
            terrarium_png(move |px, py| g(px, py) + lift)
        })
    });
    let specs = block_specs(TX0, TY0, 3, 4);
    let (job_a, _) = write_job(dir.path(), port, "a", Z, &specs, cache_field(&cache));
    let n = fetch_total(&run_ok(&job_a, dir.path()));
    let svc = services(&cache).remove(0).0;
    let pack_path = svc.join("z12").join(format!("x{tx}_y{}.pack", ty / 256));
    let old = entry(&std::fs::read(&pack_path).unwrap(), ty);

    changed.store(true, Ordering::SeqCst);
    let before = requests(&counts);
    let (job_r, _) = write_job(dir.path(), port, "a", Z, &specs,
                               serde_json::json!({ "cache_dir": cache, "repair_source_tiles": [[Z, tx, ty]] }));
    let err_r = run_ok(&job_r, dir.path());
    assert_eq!(requests(&counts) - before, 1, "the listed tile was fetched, not read from the cache:\n{err_r}");
    assert_eq!(line(&err_r, "[Download] cache:"),
               "[Download] cache: 0 of 1 source tiles from cache (a repair refetches every listed tile)");
    assert_eq!(line(&err_r, "[Stats] Cache:"),
               format!("[Stats] Cache: 0 hits (0 derived), 1 fetched, 1 written to {}", cache.display()));
    let new = entry(&std::fs::read(&pack_path).unwrap(), ty);
    assert_ne!(new.3, old.3, "the entry holds the fresh body now");
    assert!(new.0 > old.0, "appended");

    // A full run from the cache now sees the new terrain — exactly what a
    // download from the changed server writes.
    let before = requests(&counts);
    let (job_b, out_b) = write_job(dir.path(), port, "b", Z, &specs, cache_field(&cache));
    let err_b = run_ok(&job_b, dir.path());
    assert_eq!(requests(&counts), before, "{err_b}");
    assert_eq!(line(&err_b, "[Download] cache:"), format!("[Download] cache: {n} of {n} source tiles from cache (0 derived)"));
    let (job_c, out_c) = write_job(dir.path(), port, "c", Z, &specs, serde_json::json!({}));
    run_ok(&job_c, dir.path());
    assert!(read_dir_bytes(&out_b) == read_dir_bytes(&out_c));
}

// ── Strips: a cached run is fast and assembles the same bytes in any strips ─

#[test]
fn a_cached_run_needs_no_slot_and_any_strip_size_writes_the_same_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("cache");
    // 400 ms per answer and 2 connections: the fetching run takes seconds.
    let tiles = memo(|z, x, y| (z == Z).then(|| terrarium_png(tile_ground(z, x, y))));
    let (port, counts) = serve_tiles_delayed(move |z, x, y| tiles(z, x, y).map(|png| (png, 400)));
    let specs = block_specs(TX0, TY0, 3, 4);
    let extra = serde_json::json!({ "cache_dir": cache, "max_connections": 2 });
    let (job_a, out_a) = write_job(dir.path(), port, "a", Z, &specs, extra.clone());
    let t0 = Instant::now();
    let err_a = run_ok(&job_a, dir.path());
    let fetching = t0.elapsed();
    let n = fetch_total(&err_a);
    assert!(fetching >= Duration::from_millis(400 * n as u64 / 2), "{fetching:?}");

    let (job_b, out_b) = write_job(dir.path(), port, "b", Z, &specs, extra.clone());
    let t0 = Instant::now();
    let err_b = run_ok(&job_b, dir.path());
    let cached = t0.elapsed();
    assert_eq!(requests(&counts), n, "{err_b}");
    assert!(cached * 2 < fetching, "the cached run took {cached:?}, the fetching one {fetching:?}");
    assert!(read_dir_bytes(&out_a) == read_dir_bytes(&out_b));

    // One tile-row per strip (4 strips), every tile from the cache.
    let (job_c, out_c) = write_job(dir.path(), port, "c", Z, &specs, extra);
    let report = run_download_tuned(&job_c, DownloadTuning { max_strip_rows: Some(1), ..Default::default() })
        .unwrap();
    assert_eq!((report.tiles_fetched, report.strips, report.max_strip_rows), (n, 4, 1));
    assert_eq!(requests(&counts), n);
    assert!(read_dir_bytes(&out_c) == read_dir_bytes(&out_a));
}

// ── 7. Deriving a tile from the finer tiles the cache holds (changelog 40) ──

/// `(north, south, west, east)` of tile `(x, y)` at zoom `z`.
fn tile_box_at(z: u32, x: u32, y: u32) -> (f64, f64, f64, f64) {
    (ty2lat(y, z), ty2lat(y + 1, z), tx2lon(x, z), tx2lon(x + 1, z))
}

/// One 16 px spec inside each of `tiles` (at zoom `z`): a job at `z` over
/// them fetches exactly `tiles`.
fn inner_specs(z: u32, tiles: &[(u32, u32)]) -> Vec<serde_json::Value> {
    tiles.iter().map(|&(x, y)| {
        let (n, s, w, e) = tile_box_at(z, x, y);
        spec(&format!("z{z}_{x}_{y}.abt"), n - (n - s) * 0.25, w + (e - w) * 0.25, (e - w) * 0.25, 16)
    }).collect()
}

/// The z12 tiles of the `cols × rows` block from (`TX0`, `TY0`).
fn block(cols: u32, rows: u32) -> Vec<(u32, u32)> {
    (TX0..TX0 + cols).flat_map(|x| (TY0..TY0 + rows).map(move |y| (x, y))).collect()
}

/// The tiles `k` zooms below `tiles`, each tile's in one go.
fn under(tiles: &[(u32, u32)], k: u32) -> Vec<(u32, u32)> {
    let n = 1 << k;
    tiles.iter()
        .flat_map(|&(x, y)| (0..n).flat_map(move |dx| (0..n).map(move |dy| (x * n + dx, y * n + dy))))
        .collect()
}

/// The samples (metres, row-major 256 × 256) a server whose finest zoom is
/// `fine` (that zoom: [`tile_ground`]) serves at zoom `z <= fine`: above
/// `fine`, each sample the mean of the 2 × 2 samples under it one zoom finer
/// — exactly what the cache derives (no voids here, and every mean of these
/// integers lands on Terrarium's 1/256 m grid, so no rounding is involved).
fn averaged(fine: u32, z: u32, x: u32, y: u32) -> Vec<f64> {
    if z == fine {
        let g = tile_ground(z, x, y);
        return (0..256 * 256).map(|i| g(i % 256, i / 256) as f64).collect();
    }
    let kids: Vec<Vec<f64>> = [(0, 0), (1, 0), (0, 1), (1, 1)].iter()
        .map(|&(dx, dy)| averaged(fine, z + 1, 2 * x + dx, 2 * y + dy))
        .collect();
    (0..256 * 256).map(|i| {
        let (px, py) = (i % 256, i / 256);
        let c = &kids[(px / 128) + 2 * (py / 128)];
        let (sx, sy) = (2 * (px % 128), 2 * (py % 128));
        (c[sy * 256 + sx] + c[sy * 256 + sx + 1] + c[(sy + 1) * 256 + sx] + c[(sy + 1) * 256 + sx + 1]) / 4.0
    }).collect()
}

/// A 256 px Terrarium PNG of `v` (metres on the 1/256 m grid — asserted).
fn terrarium_png_exact(v: &[f64]) -> Vec<u8> {
    let mut px = Vec::with_capacity(256 * 256 * 3);
    for &m in v {
        let k = (m + 32768.0) * 256.0;
        assert_eq!(k.fract(), 0.0, "{m} m is not on Terrarium's grid");
        let k = k as u32;
        px.extend_from_slice(&[(k >> 16) as u8, (k >> 8) as u8, k as u8]);
    }
    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, 256, 256);
        enc.set_color(png::ColorType::Rgb);
        enc.set_depth(png::BitDepth::Eight);
        enc.write_header().unwrap().write_image_data(&px).unwrap();
    }
    out
}

/// A server whose finest zoom is `fine` and whose coarser zooms down to
/// `coarsest` hold the 2 × 2 averages of it ([`averaged`]); nothing else.
fn averaging_server(fine: u32, coarsest: u32) -> (u16, TileCounts) {
    serve_tiles(memo(move |z, x, y| (coarsest..=fine).contains(&z).then(|| {
        if z == fine { terrarium_png(tile_ground(z, x, y)) } else { terrarium_png_exact(&averaged(fine, z, x, y)) }
    })))
}

/// The requests made since `before`, by tile.
fn asked_since(c: &TileCounts, before: &HashMap<(u32, u32, u32), usize>) -> Vec<(u32, u32, u32)> {
    let mut v: Vec<_> = c.lock().unwrap().iter()
        .filter(|(k, n)| before.get(k).copied().unwrap_or(0) < **n).map(|(k, _)| *k).collect();
    v.sort();
    v
}

/// Every entry of `tiles` at zoom `z` under service directory `svc` has
/// status `want`; for a body, its CRC is that of a PNG.
fn assert_entries(svc: &Path, z: u32, tiles: &[(u32, u32)], want: u8) {
    for &(x, y) in tiles {
        let pack = std::fs::read(svc.join(format!("z{z}")).join(format!("x{x}_y{}.pack", y / 256))).unwrap();
        let (off, len, status, crc) = entry(&pack, y);
        assert_eq!(status, want, "z{z} ({x}, {y})");
        if want != 2 {
            let body = &pack[off as usize..off as usize + len as usize];
            assert_eq!(crc, crc32(body), "z{z} ({x}, {y})");
            assert_eq!(&body[..8], b"\x89PNG\r\n\x1a\n", "z{z} ({x}, {y})");
        }
    }
}

#[test]
fn a_tile_cached_one_zoom_finer_is_derived_without_a_request() {
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("cache");
    let (port, counts) = averaging_server(13, 12);
    let tiles = block(3, 4);
    let fine = under(&tiles, 1);

    // Run P: a z13 job over the block's 48 children caches them (rows
    // 3068..3075 straddle the z13 pack boundary at 3072 = 12 · 256).
    let (job_p, _) = write_job(dir.path(), port, "p", 13, &inner_specs(13, &fine), cache_field(&cache));
    let err_p = run_ok(&job_p, dir.path());
    assert_eq!(fetch_total(&err_p), 48);
    assert_eq!(requests_at(&counts, 13), 48);

    // Run D: the z12 job asks for nothing — every tile is derived.
    let specs = block_specs(TX0, TY0, 3, 4);
    let before = requests(&counts);
    let (job_d, out_d) = write_job(dir.path(), port, "d", Z, &specs, cache_field(&cache));
    let err_d = run_ok(&job_d, dir.path());
    assert_eq!(requests(&counts), before, "the derived run made requests:\n{err_d}");
    let n = fetch_total(&err_d);
    assert_eq!(n, 12);
    assert_eq!(line(&err_d, "[Download] cache:"),
               format!("[Download] cache: {n} of {n} source tiles from cache ({n} derived)"));
    assert_eq!(line(&err_d, "[Stats] Cache:"),
               format!("[Stats] Cache: {n} hits ({n} derived), 0 fetched, {n} written to {}", cache.display()));
    assert_eq!(line(&err_d, "[Stats] Tiles:"), format!("[Stats] Tiles: {n}/{n} OK (100.0% success)"));
    assert!(!err_d.contains("[Warn]") && !has_line(&err_d, "[Stats] Cache, coarser parents:"), "{err_d}");

    // Run R: the same job served by the server's own z12 tiles, which hold
    // those very 2 × 2 means — the same bytes, the same progress.
    let (job_r, out_r) = write_job(dir.path(), port, "r", Z, &specs, serde_json::json!({}));
    let err_r = run_ok(&job_r, dir.path());
    assert_eq!(requests_at(&counts, 12), n);
    let (d, r) = (read_dir_bytes(&out_d), read_dir_bytes(&out_r));
    assert_eq!(d.len(), specs.len());
    assert!(d == r, "the derived tiles wrote other .abt bytes than the served ones");
    assert_eq!(progress(&err_d), progress(&err_r));
    assert_eq!(line(&err_d, "[Stats] Tiles:"), line(&err_r, "[Stats] Tiles:"));

    // Stored with status 3, CRC-checked PNGs; the z13 entries untouched.
    let svc = services(&cache).remove(0).0;
    assert_entries(&svc, 12, &tiles, 3);
    assert_entries(&svc, 13, &fine, 1);

    // Run D2: a later z12 job reads them back — hits, nothing derived.
    let (job_d2, out_d2) = write_job(dir.path(), port, "d2", Z, &specs, cache_field(&cache));
    let err_d2 = run_ok(&job_d2, dir.path());
    assert_eq!(requests(&counts), before + n, "{err_d2}");
    assert_eq!(line(&err_d2, "[Download] cache:"),
               format!("[Download] cache: {n} of {n} source tiles from cache (0 derived)"));
    assert_eq!(line(&err_d2, "[Stats] Cache:"),
               format!("[Stats] Cache: {n} hits (0 derived), 0 fetched, 0 written to {}", cache.display()));
    assert!(read_dir_bytes(&out_d2) == r);
}

#[test]
fn a_cascade_derives_two_zooms_up_stores_the_middle_one_and_a_later_run_reuses_it() {
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("cache");
    let (port, counts) = averaging_server(14, 12);
    let tiles = block(3, 4);
    let mid = under(&tiles, 1);
    let fine = under(&tiles, 2);

    // Run P: z14 only — 192 tiles.
    let (job_p, _) = write_job(dir.path(), port, "p", 14, &inner_specs(14, &fine), cache_field(&cache));
    assert_eq!(fetch_total(&run_ok(&job_p, dir.path())), 192);
    assert_eq!(requests(&counts), 192);

    // Run D: z12 from z14 through 48 derived z13 tiles, no request.
    let specs = block_specs(TX0, TY0, 3, 4);
    let (job_d, out_d) = write_job(dir.path(), port, "d", Z, &specs, cache_field(&cache));
    let err_d = run_ok(&job_d, dir.path());
    assert_eq!(requests(&counts), 192, "the derived run made requests:\n{err_d}");
    assert_eq!(line(&err_d, "[Download] cache:"), "[Download] cache: 12 of 12 source tiles from cache (12 derived)");
    assert_eq!(line(&err_d, "[Stats] Cache:"),
               format!("[Stats] Cache: 12 hits (12 derived), 0 fetched, 60 written to {}", cache.display()),
               "12 z12 tiles and their 48 z13 intermediates");
    let svc = services(&cache).remove(0).0;
    assert_entries(&svc, 12, &tiles, 3);
    assert_entries(&svc, 13, &mid, 3);
    let (job_r, out_r) = write_job(dir.path(), port, "r", Z, &specs, serde_json::json!({}));
    run_ok(&job_r, dir.path());
    assert!(read_dir_bytes(&out_d) == read_dir_bytes(&out_r), "means of means differ from the served z12");

    // Run L: a z13 job over the middle zoom finds the intermediates as hits.
    let before = requests(&counts);
    let specs13 = inner_specs(13, &mid);
    let (job_l, out_l) = write_job(dir.path(), port, "l", 13, &specs13, cache_field(&cache));
    let err_l = run_ok(&job_l, dir.path());
    assert_eq!(requests(&counts), before, "{err_l}");
    assert_eq!(line(&err_l, "[Download] cache:"), "[Download] cache: 48 of 48 source tiles from cache (0 derived)");
    let (job_l2, out_l2) = write_job(dir.path(), port, "l2", 13, &specs13, serde_json::json!({}));
    run_ok(&job_l2, dir.path());
    assert_eq!(requests_at(&counts, 13), 48);
    assert!(read_dir_bytes(&out_l) == read_dir_bytes(&out_l2), "the stored z13 tiles differ from the served ones");
}

#[test]
fn only_the_tiles_the_finer_cache_does_not_cover_are_requested() {
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("cache");
    let (port, counts) = averaging_server(13, 12);
    let tiles = block(3, 4);
    // The first two columns' children all cached; of the third column only
    // three children of its first tile.
    let covered = block(2, 4);
    let mut fine = under(&covered, 1);
    fine.extend(under(&[(TX0 + 2, TY0)], 1).into_iter().take(3));
    let (job_p, _) = write_job(dir.path(), port, "p", 13, &inner_specs(13, &fine), cache_field(&cache));
    run_ok(&job_p, dir.path());

    let before = counts.lock().unwrap().clone();
    let specs = block_specs(TX0, TY0, 3, 4);
    let (job_d, out_d) = write_job(dir.path(), port, "d", Z, &specs, cache_field(&cache));
    let err_d = run_ok(&job_d, dir.path());
    let want: Vec<(u32, u32, u32)> = (TY0..TY0 + 4).map(|y| (Z, TX0 + 2, y)).collect();
    assert_eq!(asked_since(&counts, &before), want, "exactly the third column, nothing finer:\n{err_d}");
    assert_eq!(line(&err_d, "[Download] cache:"), "[Download] cache: 8 of 12 source tiles from cache (8 derived)");
    assert_eq!(line(&err_d, "[Stats] Cache:"),
               format!("[Stats] Cache: 8 hits (8 derived), 4 fetched, 12 written to {}", cache.display()));
    let svc = services(&cache).remove(0).0;
    assert_entries(&svc, 12, &covered, 3);
    assert_entries(&svc, 12, &[(TX0 + 2, TY0), (TX0 + 2, TY0 + 3)], 1);
    let (job_r, out_r) = write_job(dir.path(), port, "r", Z, &specs, serde_json::json!({}));
    run_ok(&job_r, dir.path());
    assert!(read_dir_bytes(&out_d) == read_dir_bytes(&out_r));
    assert_eq!(tiles.len(), 12);
}

#[test]
fn a_derived_404_goes_to_the_parent_fill_like_a_cached_one() {
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("cache");
    // Tile T has no data at z13 (its four children 404) nor at z12; z11
    // has. While `live` is false every zoom but z13 answers 200 with bytes
    // that decode as nothing, so the z13 run's parent fill caches no
    // ancestor of T — the z12 run must derive T's 404 from its children.
    let t = (TX0 + 1, TY0 + 1);
    let live = Arc::new(AtomicBool::new(false));
    let l2 = live.clone();
    let fine = memo(move |z, x, y| (z == 13 && (x / 2, y / 2) != t).then(|| terrarium_png(tile_ground(z, x, y))));
    let coarse = memo(move |z, x, y| match z {
        12 if (x, y) != t => Some(terrarium_png_exact(&averaged(13, 12, x, y))),
        11 => Some(terrarium_png(tile_ground(z, x, y))),
        _ => None,
    });
    let (port, counts) = serve_tiles(move |z, x, y| match z {
        13 => fine(z, x, y),
        _ if !l2.load(Ordering::SeqCst) => Some(b"not a tile".to_vec()),
        _ => coarse(z, x, y),
    });
    let tiles = block(3, 4);
    let (job_p, _) = write_job(dir.path(), port, "p", 13, &inner_specs(13, &under(&tiles, 1)), cache_field(&cache));
    let err_p = run_ok(&job_p, dir.path());
    assert!(line(&err_p, "[Stats] Cache, coarser parents:").ends_with(" fetched, 0 written"), "{err_p}");
    live.store(true, Ordering::SeqCst);

    let before = counts.lock().unwrap().clone();
    let specs = block_specs(TX0, TY0, 3, 4);
    let (job_d, out_d) = write_job(dir.path(), port, "d", Z, &specs, cache_field(&cache));
    let err_d = run_ok(&job_d, dir.path());
    let asked = asked_since(&counts, &before);
    assert!(asked.iter().all(|k| k.0 < Z), "no z12 or z13 request: {asked:?}\n{err_d}");
    assert!(asked.contains(&(11, t.0 / 2, t.1 / 2)), "T's z11 parent was fetched for the fill: {asked:?}");
    assert_eq!(line(&err_d, "[Download] cache:"), "[Download] cache: 12 of 12 source tiles from cache (12 derived)");
    assert_eq!(line(&err_d, "[Stats] Cache:"),
               format!("[Stats] Cache: 12 hits (12 derived), 0 fetched, 11 written to {}", cache.display()),
               "a derived 404 is not stored");
    assert_eq!(line(&err_d, "[Stats] Repaired from coarser parent tiles:"),
               "[Stats] Repaired from coarser parent tiles: 1 (missing at this zoom, present at a lower one)");
    assert_eq!(line(&err_d, "[Stats] Cache, coarser parents:"),
               "[Stats] Cache, coarser parents: 0 hits, 6 fetched, 6 written", "z11 with data, z10..z6 404");

    // The same job from the server's z12 tiles — T's a 404 there — writes
    // the same bytes and the same parsed lines.
    let (job_r, out_r) = write_job(dir.path(), port, "r", Z, &specs, serde_json::json!({}));
    let err_r = run_ok(&job_r, dir.path());
    for p in ["[Stats] Tiles:", "[Stats] Repaired from coarser parent tiles:"] {
        assert_eq!(line(&err_d, p), line(&err_r, p));
    }
    assert!(read_dir_bytes(&out_d) == read_dir_bytes(&out_r));
}
