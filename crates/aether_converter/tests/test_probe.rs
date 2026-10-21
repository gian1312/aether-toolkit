//! `aether_converter probe` (CONTRACT §1.2 "probe", schema `aether-probe/1`):
//! raw header facts, one JSON line per listed file, in list order; refusals
//! as `ok: false`. Field-by-field agreement with GDAL over a GDAL-written
//! matrix is checked by tools/probe_vs_gdal.py (needs GDAL); these tests pin
//! the same rules on hand-built files.
mod common;

use common::{assemble, write_abt, write_tif, Geo, Layout, Px, Tif};
use serde_json::{json, Value};
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

/// Run `probe --list` over *paths* (as given) with *extra* args:
/// `(lines, stderr)`.
fn probe(dir: &Path, paths: &[String], extra: &[&str]) -> (Vec<Value>, String) {
    let list = dir.join("list.txt");
    std::fs::write(&list, paths.join("\n")).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_aether_converter"))
        .args(["probe", "--list"])
        .arg(&list)
        .args(extra)
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(out.status.success(), "probe failed: {stderr}");
    let lines = String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    (lines, stderr)
}

fn p(dir: &Path, name: &str) -> String {
    dir.join(name).to_string_lossy().into_owned()
}

const LV95: &[(u16, u16)] = &[(1024, 1), (1025, 1), (3072, 2056), (3076, 9001)];

fn ramp(w: u32, h: u32) -> Vec<i16> {
    (0..w * h).map(|i| (i % 500) as i16).collect()
}

#[test]
fn a_tiepoint_geotiff_reports_every_header_fact() {
    let dir = tempfile::tempdir().unwrap();
    let md = r#"<GDALMetadata><Item name="SCALE" sample="0" role="scale">0.1</Item><Item name="OFFSET" sample="0" role="offset">-100</Item></GDALMetadata>"#;
    write_tif(&dir.path().join("dem.tif"), &Tif {
        w: 300,
        h: 203,
        px: Px::I16(ramp(300, 203)),
        layout: Layout::Tiles(128, 64),
        compression: 8,
        geo: Geo::TieScale(2_600_000.0, 1_200_000.0, 2.0, 2.0),
        geokeys: LV95,
        nodata: Some("-9999"),
        gdal_metadata: Some(md),
    });
    let path = p(dir.path(), "dem.tif");
    let (lines, stderr) = probe(dir.path(), &[path.clone()], &[]);
    let size = std::fs::metadata(&path).unwrap().len();
    let mut l = lines[0].clone();
    let o = l.as_object_mut().unwrap();
    assert!(o.remove("mtime").unwrap().as_i64().unwrap() > 1_600_000_000);
    assert!(o.remove("mtime_ns").unwrap().as_i64().unwrap() > 1_600_000_000_000_000_000);
    let ifd = o.remove("ifd_offset").unwrap().as_u64().unwrap();
    assert!(ifd > 8 && ifd < size, "the test writer puts the IFD after the chunks");
    // The last tile ends where the IFD starts (or one pad byte before it).
    let end = o.remove("last_chunk_end").unwrap().as_u64().unwrap();
    assert!(end == ifd || end + 1 == ifd, "last_chunk_end {end}, IFD at {ifd}");
    assert_eq!(
        l,
        json!({
            "schema": "aether-probe/1", "path": path, "ok": true, "size": size,
            "width": 300, "height": 203, "bands": 1, "dtype": "i16",
            "compression": {"code": 8, "name": "DEFLATE"}, "predictor": 1,
            "tiled": true, "tile_w": 128, "tile_h": 64,
            "epsg": 2056, "proj": null,
            "geotransform": [2_600_000.0, 2.0, 0.0, 1_200_000.0, 0.0, -2.0],
            "rotated": false, "pixel_is_point": false,
            "nodata": -9999.0, "scale": 0.1, "offset": -100.0, "overviews": 0,
        })
    );
    assert!(stderr.contains("[Probe] 1 files, 1 ok, "), "{stderr}");
}

#[test]
fn a_matrix_geotransform_is_reported_in_gdal_order_with_the_pixel_is_point_shift() {
    let dir = tempfile::tempdir().unwrap();
    // ModelTransformation [a b 0 d; e f 0 h; ...]: x = a*i + b*j + d.
    let m = [2.0, 0.5, 0.0, 2_600_000.0, 0.25, -2.0, 0.0, 1_200_000.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 1.0];
    let point: &[(u16, u16)] = &[(1024, 2), (1025, 2), (2048, 4326)];
    for (name, keys) in [("area.tif", &[(1024u16, 2u16), (2048, 4326)][..]), ("point.tif", point)] {
        write_tif(&dir.path().join(name), &Tif {
            w: 40,
            h: 30,
            px: Px::F32(vec![1.5; 1200]),
            layout: Layout::Strips(7),
            compression: 1,
            geo: Geo::Matrix(m),
            geokeys: keys,
            nodata: Some("nan"),
            gdal_metadata: None,
        });
    }
    let (lines, _) = probe(dir.path(), &[p(dir.path(), "area.tif"), p(dir.path(), "point.tif")], &[]);
    let area = &lines[0];
    assert_eq!(area["geotransform"], json!([2_600_000.0, 2.0, 0.5, 1_200_000.0, 0.25, -2.0]));
    assert_eq!(area["rotated"], json!(true));
    assert_eq!(area["epsg"], json!(4326));
    assert_eq!(area["nodata"], json!("nan"));
    assert_eq!((area["tiled"].clone(), area["tile_w"].clone(), area["tile_h"].clone()), (json!(false), json!(40), json!(7)));
    assert_eq!((area["scale"].clone(), area["offset"].clone()), (Value::Null, Value::Null));
    // GDAL: x0 -= (dx + rx) / 2, y0 -= (ry + dy) / 2.
    let point = &lines[1];
    assert_eq!(point["pixel_is_point"], json!(true));
    assert_eq!(point["geotransform"], json!([2_600_000.0 - 1.25, 2.0, 0.5, 1_200_000.0 + 0.875, 0.25, -2.0]));
}

/// A minimal TIFF: one IFD per entry list, chained in order, with dummy
/// strip offsets (probe never reads pixel data).
fn tiny_tiff(path: &Path, big: bool, le: bool, ifds: &[Vec<(u16, u16, Vec<u64>)>]) {
    let put = |b: &mut Vec<u8>, v: u64, n: usize| {
        let bytes = if le { v.to_le_bytes() } else { v.to_be_bytes() };
        if le { b.extend_from_slice(&bytes[..n]) } else { b.extend_from_slice(&bytes[8 - n..]) }
    };
    let size_of = |typ: u16| match typ { 3 => 2, 4 => 4, 16 => 8, 12 => 8, _ => panic!() };
    let mut b: Vec<u8> = if le { b"II".to_vec() } else { b"MM".to_vec() };
    if big {
        put(&mut b, 43, 2);
        put(&mut b, 8, 2);
        put(&mut b, 0, 2);
        put(&mut b, 16, 8);
    } else {
        put(&mut b, 42, 2);
        put(&mut b, 8, 4);
    }
    let (cnt, ent, off) = if big { (8, 20, 8) } else { (2, 12, 4) };
    for (k, entries) in ifds.iter().enumerate() {
        let start = b.len();
        let mut ext_at = start + cnt + entries.len() * ent + off;
        let mut ext = Vec::new();
        put(&mut b, entries.len() as u64, cnt);
        for (tag, typ, vals) in entries {
            put(&mut b, *tag as u64, 2);
            put(&mut b, *typ as u64, 2);
            put(&mut b, vals.len() as u64, off);
            let mut raw = Vec::new();
            for &v in vals {
                put(&mut raw, v, size_of(*typ));
            }
            if raw.len() <= off {
                raw.resize(off, 0);
                b.extend_from_slice(&raw);
            } else {
                put(&mut b, ext_at as u64, off);
                ext_at += raw.len();
                ext.extend_from_slice(&raw);
            }
        }
        let next = if k + 1 < ifds.len() { ext_at as u64 } else { 0 };
        put(&mut b, next, off);
        b.extend_from_slice(&ext);
    }
    std::fs::write(path, b).unwrap();
}

fn image_entries(sub: u64, w: u64, h: u64, spp: u64) -> Vec<(u16, u16, Vec<u64>)> {
    let mut e = vec![
        (254, 4, vec![sub]),
        (256, 4, vec![w]),
        (257, 4, vec![h]),
        (258, 3, vec![32; spp as usize]),
        (259, 3, vec![50000]),
        (262, 3, vec![if spp == 1 { 1 } else { 2 }]),
        (273, 4, vec![0]),
        (277, 3, vec![spp]),
        (278, 4, vec![h]),
        (279, 4, vec![0]),
        (339, 3, vec![3; spp as usize]),
    ];
    if sub == 0 {
        e.push((317, 3, vec![3]));
        e.push((33550, 12, vec![30f64.to_bits(), 30f64.to_bits(), 0]));
        e.push((33922, 12, vec![0, 0, 0, 500_000f64.to_bits(), 5_300_000f64.to_bits(), 0]));
        e.push((34735, 3, vec![1, 1, 0, 1, 3072, 0, 1, 32632]));
    }
    e
}

#[test]
fn bigtiff_both_byte_orders_and_the_overview_count_skips_masks() {
    let dir = tempfile::tempdir().unwrap();
    let ifds = vec![
        image_entries(0, 1000, 800, 1),
        image_entries(1, 500, 400, 1), // overview
        image_entries(4, 1000, 800, 1), // mask
        image_entries(5, 500, 400, 1), // mask overview
        image_entries(1, 250, 200, 1), // overview
        image_entries(1, 125, 100, 3), // other sample count: GDAL skips it
    ];
    let mut paths = Vec::new();
    for (big, le) in [(true, true), (true, false), (false, false)] {
        let name = format!("t_{big}_{le}.tif");
        tiny_tiff(&dir.path().join(&name), big, le, &ifds);
        paths.push(p(dir.path(), &name));
    }
    let (lines, _) = probe(dir.path(), &paths, &[]);
    for (l, big) in lines.iter().zip([true, true, false]) {
        assert_eq!(l["ok"], json!(true), "{l}");
        assert_eq!(l["overviews"], json!(2), "{l}");
        assert_eq!(l["ifd_offset"], json!(if big { 16 } else { 8 }));
        assert_eq!(l["dtype"], json!("f32"));
        assert_eq!(l["compression"], json!({"code": 50000, "name": "ZSTD"}));
        assert_eq!(l["predictor"], json!(3));
        assert_eq!((l["tile_w"].clone(), l["tile_h"].clone()), (json!(1000), json!(800)));
        assert_eq!(l["epsg"], json!(32632));
        assert_eq!(l["geotransform"], json!([500_000.0, 30.0, 0.0, 5_300_000.0, 0.0, -30.0]));
        assert_eq!(l["nodata"], Value::Null);
        assert_eq!(l["last_chunk_end"], json!(0), "one sparse strip (offset 0, byte count 0): {l}");
    }
}

/// A GDAL-like layout — IFD and its values first, then the tiles from
/// `DATA_AT` on (16 DEFLATE tiles of `CHUNK` bytes; never decoded) — whole,
/// cut off behind the header, and with no TileByteCounts.
#[test]
fn last_chunk_end_is_where_the_bottom_right_block_ends_so_a_cut_file_shows_it() {
    const DATA_AT: u64 = 4096;
    const CHUNK: u64 = 100;
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let entries = |byte_counts: bool| {
        let mut e: Vec<(u16, u16, Vec<u64>)> = vec![
            (256, 4, vec![64]),
            (257, 4, vec![64]),
            (258, 3, vec![16]),
            (259, 3, vec![8]),
            (262, 3, vec![1]),
            (277, 3, vec![1]),
            (322, 4, vec![16]),
            (323, 4, vec![16]),
            (324, 4, (0..16).map(|k| DATA_AT + k * CHUNK).collect()),
            (339, 3, vec![2]),
            (33550, 12, vec![2f64.to_bits(), 2f64.to_bits(), 0]),
            (33922, 12, vec![0, 0, 0, 2_600_000f64.to_bits(), 1_200_000f64.to_bits(), 0]),
            (34735, 3, vec![1, 1, 0, 1, 3072, 0, 1, 2056]),
        ];
        if byte_counts {
            e.insert(9, (325, 4, vec![CHUNK; 16]));
        }
        e
    };
    let full = DATA_AT + 16 * CHUNK;
    for (name, byte_counts, len) in [("whole.tif", true, full), ("cut.tif", true, DATA_AT + 8 * CHUNK), ("no_counts.tif", false, full)] {
        let path = d.join(name);
        tiny_tiff(&path, false, true, &[entries(byte_counts)]);
        let header = std::fs::metadata(&path).unwrap().len();
        assert!(header < DATA_AT, "the header ({header} bytes) must end before the tiles");
        std::fs::OpenOptions::new().write(true).open(&path).unwrap().set_len(len).unwrap();
    }
    let (lines, _) = probe(d, &[p(d, "whole.tif"), p(d, "cut.tif"), p(d, "no_counts.tif")], &[]);
    for l in &lines {
        assert_eq!(l["ok"], json!(true), "the header is intact: {l}");
    }
    assert_eq!(lines[0]["last_chunk_end"], json!(full));
    assert_eq!(lines[0]["size"], json!(full));
    let (end, size) = (lines[1]["last_chunk_end"].as_u64().unwrap(), lines[1]["size"].as_u64().unwrap());
    assert_eq!(end, full);
    assert!(end > size, "cut behind the header: last_chunk_end {end} > size {size}");
    assert_eq!(lines[2]["last_chunk_end"], Value::Null, "no TileByteCounts: {}", lines[2]);
}

#[test]
fn files_the_plugin_must_send_to_gdal_are_ok_false_with_a_reason() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let tif = |name: &str, keys: &'static [(u16, u16)]| {
        write_tif(&d.join(name), &Tif {
            w: 8,
            h: 8,
            px: Px::I16(vec![1; 64]),
            layout: Layout::Strips(8),
            compression: 1,
            geo: Geo::TieScale(0.0, 0.0, 1.0, 1.0),
            geokeys: keys,
            nodata: None,
            gdal_metadata: None,
        })
    };
    tif("user_defined.tif", &[(1024, 1), (3072, 32767), (3075, 1)]);
    tif("model_only.tif", &[(1024, 1)]);
    // Three samples per pixel, a palette and 1-bit samples, via raw entries.
    let raw = |name: &str, extra: Vec<(u16, u16, u32, Vec<u8>)>| {
        let mut e: Vec<(u16, u16, u32, Vec<u8>)> = vec![
            (256, 4, 1, 8u32.to_le_bytes().to_vec()),
            (257, 4, 1, 8u32.to_le_bytes().to_vec()),
            (259, 3, 1, 1u16.to_le_bytes().to_vec()),
        ];
        e.extend(extra);
        assemble(&d.join(name), e, &[vec![0u8; 192]], &[0], false);
    };
    let s = |v: &[u16]| v.iter().flat_map(|x| x.to_le_bytes()).collect::<Vec<u8>>();
    raw("rgb.tif", vec![(258, 3, 3, s(&[8, 8, 8])), (262, 3, 1, s(&[2])), (277, 3, 1, s(&[3]))]);
    raw("palette.tif", vec![(258, 3, 1, s(&[8])), (262, 3, 1, s(&[3])), (320, 3, 768, s(&[0; 768]))]);
    raw("one_bit.tif", vec![(258, 3, 1, s(&[1])), (262, 3, 1, s(&[1]))]);
    raw("no_geotransform.tif", vec![(258, 3, 1, s(&[8])), (262, 3, 1, s(&[1]))]);
    write_abt(&d.join("tile.abt"), 4, 47.0, 8.0, 0.001, &[0; 16]);
    std::fs::write(d.join("notes.tif"), "not a tiff\n").unwrap();
    std::fs::create_dir(d.join("folder.tif")).unwrap();
    let cases = [
        ("user_defined.tif", "user-defined CRS"),
        ("model_only.tif", "user-defined CRS"),
        ("rgb.tif", "multi-band: 3 samples per pixel"),
        ("palette.tif", "palette"),
        ("one_bit.tif", "sub-byte samples: 1 bits"),
        ("no_geotransform.tif", "no geotransform"),
        ("tile.abt", "not a TIFF file (an .abt tile)"),
        ("notes.tif", "not a TIFF file"),
        ("folder.tif", "not a regular file"),
        ("missing.tif", "cannot open"),
    ];
    let paths: Vec<String> = cases.iter().map(|(n, _)| p(d, n)).collect();
    let (lines, stderr) = probe(d, &paths, &[]);
    assert_eq!(lines.len(), cases.len());
    for (l, (name, reason)) in lines.iter().zip(cases) {
        assert_eq!(l["ok"], json!(false), "{name}: {l}");
        assert!(l["reason"].as_str().unwrap().starts_with(reason), "{name}: {l}");
        assert!(l.get("width").is_none(), "no header facts on a refused line: {l}");
        let exists = name != "missing.tif";
        assert_eq!(l["size"].is_u64(), exists, "{name}: {l}");
        assert_eq!(l["mtime"].is_i64(), exists, "{name}: {l}");
    }
    assert!(stderr.contains(&format!("[Probe] {} files, 0 ok, ", cases.len())), "{stderr}");
}

#[test]
fn lines_keep_list_order_under_many_workers_and_stdin_gives_the_same() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    write_tif(&d.join("a.tif"), &Tif {
        w: 16,
        h: 16,
        px: Px::U16(vec![7; 256]),
        layout: Layout::Strips(16),
        compression: 1,
        geo: Geo::TieScale(0.0, 0.0, 1.0, 1.0),
        geokeys: LV95,
        nodata: None,
        gdal_metadata: None,
    });
    // 60 entries alternating a readable file and a missing one; the stdin
    // run below sends the same list with a BOM, CRLF and blank lines.
    let paths: Vec<String> = (0..60).map(|i| if i % 2 == 0 { p(d, "a.tif") } else { p(d, &format!("nope{i}.tif")) }).collect();
    let (lines, stderr) = probe(d, &paths, &["--jobs", "7"]);
    let got: Vec<&str> = lines.iter().map(|l| l["path"].as_str().unwrap()).collect();
    assert_eq!(got, paths);
    for (i, l) in lines.iter().enumerate() {
        assert_eq!(l["ok"], json!(i % 2 == 0));
    }
    let re_ok = stderr.lines().last().unwrap();
    assert!(re_ok.starts_with("[Probe] 60 files, 30 ok, "), "{stderr}");
    assert!(re_ok.contains(" ms avg, read ") && re_ok.ends_with(" ms avg"), "{stderr}");

    let mut child = Command::new(env!("CARGO_BIN_EXE_aether_converter"))
        .args(["probe", "--stdin", "--jobs", "3"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let text = format!("\u{feff}{}\r\n\r\n", paths.join("\r\n"));
    child.stdin.take().unwrap().write_all(text.as_bytes()).unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success());
    let stdin_lines: Vec<Value> =
        String::from_utf8(out.stdout).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    assert_eq!(stdin_lines, lines);
}

#[test]
fn the_list_and_stdin_are_exclusive_and_one_is_required() {
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_aether_converter")).arg("probe").args(args).output().unwrap()
    };
    assert!(!run(&[]).status.success());
    assert!(!run(&["--list", "x.txt", "--stdin"]).status.success());
    let missing = run(&["--list", "/nonexistent/list.txt"]);
    assert!(!missing.status.success());
    assert!(String::from_utf8_lossy(&missing.stderr).contains("reading probe list"));
}
