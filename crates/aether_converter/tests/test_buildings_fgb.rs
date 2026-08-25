//! `buildings_file` (FlatGeobuf) ingest: the height ladder below geometry Z,
//! the Z path's continued primacy, and the fail-loud / report-loud surface.
//!
//! Every case here is driven through a **real** FlatGeobuf written by the
//! format's own writer, not a stub: the defects this file pins were all in the
//! reading of real files — a 2D geometry with no Z, a header-only geometry
//! type, ring ends counted in points — and none of them are reachable from a
//! hand-rolled fake.

use std::collections::HashMap;
use std::fs::File;
use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use aether_converter::ingest::{process_tile_with_cache, IngestJob};
use flatgeobuf::geozero::{ColumnValue, FeatureProcessor, GeomProcessor, PropertyProcessor};
use flatgeobuf::{ColumnType, FgbCrs, FgbWriter, FgbWriterOptions, GeometryType};

// ── FlatGeobuf fixtures ───────────────────────────────────────────────────

/// One square footprint: centre, half-side in degrees, optional roof Z, attrs.
struct Bldg<'a> {
    lon: f64,
    lat: f64,
    half: f64,
    /// Absolute roof elevation (AMSL) to write into the geometry Z. Only used
    /// when the file is created with `has_z`.
    z: Option<f64>,
    props: &'a [(&'a str, &'a str)],
}

impl Bldg<'_> {
    /// The closed outer ring, counter-clockwise.
    fn ring(&self) -> [(f64, f64); 5] {
        let (w, e) = (self.lon - self.half, self.lon + self.half);
        let (s, n) = (self.lat - self.half, self.lat + self.half);
        [(w, s), (e, s), (e, n), (w, n), (w, s)]
    }
}

/// Write a FlatGeobuf of square footprints.
///
/// The header carries the geometry type and the features do not repeat it —
/// which is what an ordinary single-type FlatGeobuf looks like, GDAL's
/// included, and the shape that used to be dropped wholesale.
fn write_fgb(path: &Path, has_z: bool, columns: &[&str], features: &[Bldg]) {
    let mut fgb = FgbWriter::create_with_options(
        "buildings",
        GeometryType::Polygon,
        FgbWriterOptions {
            write_index: true,
            detect_type: false,
            promote_to_multi: false,
            has_z,
            crs: FgbCrs { code: 4326, ..Default::default() },
            ..Default::default()
        },
    )
    .unwrap();
    for name in columns {
        fgb.add_column(name, ColumnType::String, |_, _| {});
    }

    for b in features {
        let ring = b.ring();
        fgb.polygon_begin(true, 1, 0).unwrap();
        fgb.linestring_begin(false, ring.len(), 0).unwrap();
        for (i, &(x, y)) in ring.iter().enumerate() {
            if has_z {
                fgb.coordinate(x, y, b.z, None, None, None, i).unwrap();
            } else {
                fgb.xy(x, y, i).unwrap();
            }
        }
        fgb.linestring_end(false, 0).unwrap();
        fgb.polygon_end(true, 0).unwrap();

        for (i, name) in columns.iter().enumerate() {
            let v = b.props.iter().find(|(k, _)| k == name).map_or("", |(_, v)| *v);
            fgb.property(i, name, &ColumnValue::String(v)).unwrap();
        }
        fgb.feature_end(0).unwrap();
    }
    fgb.write(BufWriter::new(File::create(path).unwrap())).unwrap();
}

/// A FlatGeobuf whose one feature is a polygon with **two** rings, so the
/// geometry carries an `ends` array. Both squares are drawn as footprints.
fn write_two_ring_fgb(path: &Path, a: &Bldg, b: &Bldg) {
    let mut fgb = FgbWriter::create_with_options(
        "buildings",
        GeometryType::Polygon,
        FgbWriterOptions {
            write_index: true,
            detect_type: false,
            promote_to_multi: false,
            crs: FgbCrs { code: 4326, ..Default::default() },
            ..Default::default()
        },
    )
    .unwrap();
    fgb.add_column("height", ColumnType::String, |_, _| {});

    fgb.polygon_begin(true, 2, 0).unwrap();
    for ring in [a.ring(), b.ring()] {
        fgb.linestring_begin(false, ring.len(), 0).unwrap();
        for (i, &(x, y)) in ring.iter().enumerate() {
            fgb.xy(x, y, i).unwrap();
        }
        fgb.linestring_end(false, 0).unwrap();
    }
    fgb.polygon_end(true, 0).unwrap();
    fgb.property(0, "height", &ColumnValue::String("12")).unwrap();
    fgb.feature_end(0).unwrap();

    fgb.write(BufWriter::new(File::create(path).unwrap())).unwrap();
}

// ── The tile under test ───────────────────────────────────────────────────

const UL_LAT: f64 = 46.96;
const UL_LON: f64 = 7.42;
const SIZE: u32 = 64;
const RES_M: f64 = 10.0;
/// Bern's actual ground, near enough. The number matters: an attribute height
/// read as an *absolute* roof would land ~528 m under it and vanish.
const GROUND_M: f64 = 540.0;

/// A job over the test tile, with flat [`GROUND_M`] terrain and no source
/// files at all — `void_fill_m` fills every pixel, which is the cheapest
/// honest terrain there is and leaves the buildings as the only variable.
fn job(dir: &Path, name: &str, buildings: Option<&Path>) -> serde_json::Value {
    let mut j = serde_json::json!({
        "output_path": dir.join(name),
        "format": "r16sint",
        "ul_lat": UL_LAT,
        "ul_lon": UL_LON,
        "resolution_m": RES_M,
        "size_px": SIZE,
        "sources": [],
        "void_fill_m": GROUND_M,
    });
    if let Some(b) = buildings {
        j["buildings_file"] = serde_json::json!(b);
    }
    j
}

/// Run *json* through the real ingest surface and return the tile as metres.
fn run(json: serde_json::Value) -> Vec<f64> {
    let job: IngestJob = serde_json::from_value(json).unwrap();
    let out = job.output_path.clone();
    process_tile_with_cache(job, Arc::new(Mutex::new(HashMap::new()))).unwrap();
    let bytes = std::fs::read(&out).unwrap();
    let size = SIZE as usize;
    let stride = (size * 2 + 255) & !255;
    (0..size)
        .flat_map(|y| {
            (0..size).map(move |x| {
                let o = 44 + y * stride + x * 2;
                (o, ())
            })
        })
        .map(|(o, ())| i16::from_le_bytes([bytes[o], bytes[o + 1]]) as f64 / 2.0)
        .collect()
}

/// The elevation at the pixel containing *(lon, lat)*.
fn at(tile: &[f64], lon: f64, lat: f64) -> f64 {
    let pd = RES_M / 111_111.0;
    let x = ((lon - UL_LON) / pd) as usize;
    let y = ((UL_LAT - lat) / pd) as usize;
    tile[y * SIZE as usize + x]
}

fn max_of(tile: &[f64]) -> f64 {
    tile.iter().copied().fold(f64::MIN, f64::max)
}

fn raised_px(tile: &[f64]) -> usize {
    tile.iter().filter(|v| **v > GROUND_M).count()
}

// ── The ladder below geometry Z ───────────────────────────────────────────

#[test]
fn a_2d_flatgeobuf_burns_its_attribute_heights_into_the_terrain() {
    // The defect this pins: a 2D FlatGeobuf has no geometry Z, the reader
    // returned on its first line for every feature, the building list came
    // back empty and the burn was a silent no-op — the tile was byte-identical
    // to the bare one. Three rungs of the ladder, one building each.
    let dir = tempfile::tempdir().unwrap();
    let fgb = dir.path().join("bern.fgb");
    write_fgb(
        &fgb,
        false,
        &["height", "levels"],
        &[
            Bldg { lon: 7.4230, lat: 46.9570, half: 0.0006, z: None,
                   props: &[("height", "12")] },
            Bldg { lon: 7.4245, lat: 46.9570, half: 0.0006, z: None,
                   props: &[("levels", "4")] },
            Bldg { lon: 7.4215, lat: 46.9556, half: 0.0006, z: None,
                   props: &[] },
        ],
    );

    let bare = run(job(dir.path(), "bare.abt", None));
    assert!(
        bare.iter().all(|v| *v == GROUND_M),
        "the control tile must be flat terrain, or nothing below proves anything"
    );

    let built = run(job(dir.path(), "built.abt", Some(&fgb)));
    assert_ne!(bare, built, "buildings must change the terrain");

    // Explicit height: 540 m ground + 12 m.
    assert_eq!(at(&built, 7.4230, 46.9570), GROUND_M + 12.0);
    // Storey count: 4 levels x 3 m.
    assert_eq!(at(&built, 7.4245, 46.9570), GROUND_M + 12.0);
    // Nothing usable: the 6 m default, still above ground.
    assert_eq!(at(&built, 7.4215, 46.9556), GROUND_M + 6.0);
}

#[test]
fn an_attribute_height_is_above_ground_never_an_absolute_roof() {
    // A 12 m *absolute* roof on 540 m Bern terrain is 528 m underground, and
    // the `max` composite would drop it without a word — the whole burn would
    // look like it ran and change nothing. This is the single assertion that
    // separates the fix from the bug it replaced.
    let dir = tempfile::tempdir().unwrap();
    let fgb = dir.path().join("agl.fgb");
    write_fgb(
        &fgb,
        false,
        &["height"],
        &[Bldg { lon: 7.4230, lat: 46.9570, half: 0.0006, z: None,
                 props: &[("height", "12")] }],
    );

    let built = run(job(dir.path(), "agl.abt", Some(&fgb)));
    assert_eq!(max_of(&built), GROUND_M + 12.0);
    assert!(raised_px(&built) > 100, "the footprint must actually be drawn");
    assert!(
        !built.iter().any(|v| *v == 12.0),
        "12 m must never be written as an elevation — that is the absolute reading"
    );
}

#[test]
fn a_height_attribute_outranks_a_levels_attribute() {
    // Ladder order, not tag order: an explicit height is measured, a storey
    // count is a 3 m-per-floor guess, so the measured one wins even when the
    // guess would be taller.
    let dir = tempfile::tempdir().unwrap();
    let fgb = dir.path().join("both.fgb");
    write_fgb(
        &fgb,
        false,
        &["height", "levels"],
        &[Bldg { lon: 7.4230, lat: 46.9570, half: 0.0006, z: None,
                 props: &[("height", "9"), ("levels", "20")] }],
    );
    assert_eq!(max_of(&run(job(dir.path(), "both.abt", Some(&fgb)))), GROUND_M + 9.0);
}

#[test]
fn a_unit_suffix_and_an_unusable_value_both_land_on_the_right_rung() {
    // `"12 m"` is a height; `""`, `"tall"` and `"0"` are not, and must fall
    // through to the next rung rather than being taken as zero.
    let dir = tempfile::tempdir().unwrap();
    let fgb = dir.path().join("messy.fgb");
    write_fgb(
        &fgb,
        false,
        &["height", "levels"],
        &[
            Bldg { lon: 7.4230, lat: 46.9570, half: 0.0006, z: None,
                   props: &[("height", "12.5 m")] },
            Bldg { lon: 7.4245, lat: 46.9570, half: 0.0006, z: None,
                   props: &[("height", "tall"), ("levels", "3")] },
            Bldg { lon: 7.4215, lat: 46.9556, half: 0.0006, z: None,
                   props: &[("height", "0"), ("levels", "-2")] },
        ],
    );

    let built = run(job(dir.path(), "messy.abt", Some(&fgb)));
    // 12.5 m -> 25 half-metres exactly; this path truncates, so no rounding
    // ambiguity is being papered over here.
    assert_eq!(at(&built, 7.4230, 46.9570), GROUND_M + 12.5);
    assert_eq!(at(&built, 7.4245, 46.9570), GROUND_M + 9.0);
    assert_eq!(at(&built, 7.4215, 46.9556), GROUND_M + 6.0);
}

// ── Geometry Z keeps the top rung ─────────────────────────────────────────

#[test]
fn geometry_z_still_wins_and_attribute_heights_do_not_disturb_it() {
    // The Z path is unchanged: a roof read from the geometry is an ABSOLUTE
    // elevation, it ignores the terrain under it, and it ignores the attribute
    // columns entirely. The two files differ only in an added `height` column
    // of 999 — a value that would be unmissable if it leaked in — and their
    // tiles must be byte-identical.
    let dir = tempfile::tempdir().unwrap();
    let plain = dir.path().join("z_plain.fgb");
    let noisy = dir.path().join("z_noisy.fgb");
    let square = |props: &'static [(&'static str, &'static str)]| Bldg {
        lon: 7.4230,
        lat: 46.9570,
        half: 0.0006,
        z: Some(560.0),
        props,
    };
    write_fgb(&plain, true, &[], &[square(&[])]);
    write_fgb(&noisy, true, &["height"], &[square(&[("height", "999")])]);

    let a = run(job(dir.path(), "z_plain.abt", Some(&plain)));
    let b = run(job(dir.path(), "z_noisy.abt", Some(&noisy)));

    assert_eq!(a, b, "an attribute height must not reach a Z-carrying geometry");
    assert_eq!(max_of(&a), 560.0, "the roof is the absolute Z, not ground + Z");
    assert_eq!(at(&a, 7.4230, 46.9570), 560.0);
}

#[test]
fn a_roof_below_the_terrain_is_dropped_exactly_as_before() {
    // An absolute roof under the surface loses the `max` and changes nothing.
    // Preserved deliberately: it is how a Z source with a wrong vertical datum
    // has always behaved, and the attribute fallback must not rescue it.
    let dir = tempfile::tempdir().unwrap();
    let fgb = dir.path().join("sunken.fgb");
    write_fgb(
        &fgb,
        true,
        &["height"],
        &[Bldg { lon: 7.4230, lat: 46.9570, half: 0.0006, z: Some(100.0),
                 props: &[("height", "12")] }],
    );

    let built = run(job(dir.path(), "sunken.abt", Some(&fgb)));
    assert!(built.iter().all(|v| *v == GROUND_M), "nothing may be drawn");
}

// ── Geometry the reader used to mangle ────────────────────────────────────

#[test]
fn a_single_type_flatgeobuf_is_not_dropped_for_lacking_a_per_feature_type() {
    // A FlatGeobuf writes its geometry type once, in the header; a feature
    // repeats it only in a mixed-type file. Reading it off the feature alone
    // saw `Unknown` on every feature of an ordinary file and skipped the lot.
    // Every fixture in this file is written that way, so any of these tests
    // failing would catch a regression — this one says so out loud.
    let dir = tempfile::tempdir().unwrap();
    let fgb = dir.path().join("typed.fgb");
    write_fgb(
        &fgb,
        false,
        &["height"],
        &[Bldg { lon: 7.4230, lat: 46.9570, half: 0.0006, z: None,
                 props: &[("height", "12")] }],
    );
    assert!(raised_px(&run(job(dir.path(), "typed.abt", Some(&fgb)))) > 100);
}

#[test]
fn ring_ends_are_point_counts_not_coordinate_offsets() {
    // A multi-ring polygon stores ring ends as POINT indices. Taking them as
    // `xy` slots halved every ring: with five-point rings each one collapsed
    // to two vertices and was discarded as degenerate, so a building with a
    // courtyard drew nothing at all.
    let dir = tempfile::tempdir().unwrap();
    let fgb = dir.path().join("rings.fgb");
    write_two_ring_fgb(
        &fgb,
        &Bldg { lon: 7.4230, lat: 46.9570, half: 0.0006, z: None, props: &[] },
        &Bldg { lon: 7.4245, lat: 46.9570, half: 0.0006, z: None, props: &[] },
    );

    let built = run(job(dir.path(), "rings.abt", Some(&fgb)));
    assert_eq!(at(&built, 7.4230, 46.9570), GROUND_M + 12.0, "first ring");
    assert_eq!(at(&built, 7.4245, 46.9570), GROUND_M + 12.0, "second ring");
}

// ── Fail loudly, report loudly (CLI surface) ──────────────────────────────

/// A flat `.abt` terrain source amply covering the test tile.
///
/// The CLI refuses a run no source covered, so the report/exit-code cases need
/// real coverage; an `.abt` source is self-describing and takes twenty lines.
fn abt_source(dir: &Path) -> PathBuf {
    let (size, pd) = (64usize, 0.001f64);
    let row_bytes = size * 2;
    let stride = (row_bytes + 255) & !255;
    let mut buf: Vec<u8> = Vec::new();
    buf.extend_from_slice(b"AETH");
    buf.extend_from_slice(&1u16.to_le_bytes()); // version 1 = R16SINT
    buf.extend_from_slice(&(size as u16).to_le_bytes());
    buf.extend_from_slice(&46.97f64.to_le_bytes()); // ul_lat
    buf.extend_from_slice(&7.41f64.to_le_bytes()); // ul_lon
    buf.extend_from_slice(&pd.to_le_bytes()); // scale_y
    buf.extend_from_slice(&pd.to_le_bytes()); // scale_x
    buf.extend_from_slice(&0i16.to_le_bytes()); // base_elev
    buf.extend_from_slice(&(stride as u16).to_le_bytes());
    for _ in 0..size {
        for _ in 0..size {
            buf.extend_from_slice(&((GROUND_M * 2.0) as i16).to_le_bytes());
        }
        buf.resize(buf.len() + (stride - row_bytes), 0);
    }
    let p = dir.join("terrain.abt");
    std::fs::write(&p, buf).unwrap();
    p
}

/// Run the real CLI over *json*; `(exit ok, stdout + stderr)`.
fn run_cli(dir: &Path, json: serde_json::Value) -> (bool, String) {
    let job_file = dir.join("job.json");
    std::fs::write(&job_file, serde_json::to_vec(&json).unwrap()).unwrap();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_aether_converter"))
        .args(["ingest", "--job-file"])
        .arg(&job_file)
        .output()
        .unwrap();
    let said = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.success(), said)
}

/// A CLI job with real terrain coverage, so only the buildings are on trial.
fn covered_job(dir: &Path, name: &str, buildings: Option<&Path>) -> serde_json::Value {
    let mut j = job(dir, name, buildings);
    j["sources"] = serde_json::json!([{ "path": abt_source(dir) }]);
    j
}

#[test]
fn a_burn_reports_itself_on_the_marker_the_plugin_parses() {
    // The ingest path used to print NOTHING on a successful burn, so a
    // consumer had no way to tell a burn from a no-op — and the tiles were
    // then pooled under an identity claiming buildings either way. The marker
    // is the one `download` already prints, deliberately: a second spelling
    // would be a second thing for consumers to learn.
    let dir = tempfile::tempdir().unwrap();
    let fgb = dir.path().join("report.fgb");
    write_fgb(
        &fgb,
        false,
        &["height", "levels"],
        &[
            Bldg { lon: 7.4230, lat: 46.9570, half: 0.0006, z: None,
                   props: &[("height", "12")] },
            Bldg { lon: 7.4245, lat: 46.9570, half: 0.0006, z: None,
                   props: &[] },
        ],
    );

    let (ok, said) = run_cli(dir.path(), covered_job(dir.path(), "report.abt", Some(&fgb)));
    assert!(ok, "a good burn still exits 0: {said}");
    assert!(said.contains("[Buildings]"), "the marker is contract: {said}");
    assert!(said.contains("report.fgb"), "must name the source: {said}");
    assert!(said.contains("2 polygon feature(s)"), "must count features: {said}");
    assert!(said.contains("1 with a height from the data"), "must say how many were guessed: {said}");
    assert!(said.contains("2 drawn"), "must count what was drawn: {said}");
    assert!(!said.contains("0 px raised"), "must count raised pixels: {said}");
}

#[test]
fn a_burn_that_drew_nothing_says_so() {
    // Buildings a long way from this tile. Not an error — an edge tile with no
    // buildings over it is ordinary — but never again silent.
    let dir = tempfile::tempdir().unwrap();
    let fgb = dir.path().join("elsewhere.fgb");
    write_fgb(
        &fgb,
        false,
        &["height"],
        &[Bldg { lon: -73.98, lat: 40.75, half: 0.0006, z: None,
                 props: &[("height", "12")] }],
    );

    let (ok, said) = run_cli(dir.path(), covered_job(dir.path(), "nowhere.abt", Some(&fgb)));
    assert!(ok, "no buildings over THIS tile is not a failure: {said}");
    assert!(said.contains("[Buildings]"), "silence is the bug: {said}");
    assert!(said.contains("no footprint to draw"), "must say it drew nothing: {said}");
}

#[test]
fn an_unreadable_buildings_file_fails_the_run_instead_of_warning() {
    // This used to print `[Warn] Failed to apply buildings: …` and exit 0,
    // handing back building-LESS terrain under an identity claiming buildings.
    // A consumer then cached it, and every later run was a hit.
    let dir = tempfile::tempdir().unwrap();
    let bogus = dir.path().join("not_really.fgb");
    std::fs::write(&bogus, b"this is not a FlatGeobuf").unwrap();

    let out = dir.path().join("nope.abt");
    let (ok, said) = run_cli(dir.path(), covered_job(dir.path(), "nope.abt", Some(&bogus)));
    assert!(!ok, "an unreadable buildings source must fail the run: {said}");
    assert!(
        said.to_lowercase().contains("failed to apply buildings"),
        "the complaint older consumers grep for must survive: {said}"
    );
    assert!(said.contains("refusing to write a building-less tile"), "{said}");
    assert!(!out.exists(), "a refused tile must not be written: {said}");
}

#[test]
fn a_buildings_file_that_is_not_there_fails_the_run() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("absent.fgb");
    let (ok, said) = run_cli(dir.path(), covered_job(dir.path(), "gone.abt", Some(&missing)));
    assert!(!ok, "a missing buildings source must fail the run: {said}");
    assert!(said.contains("absent.fgb"), "must name the file: {said}");
}

#[test]
fn a_buildings_directory_with_no_fgb_in_it_fails_the_run() {
    // A directory is an accepted `buildings_file`. An empty one is not "no
    // buildings here", it is a misconfigured path, and it used to burn nothing
    // and exit 0.
    let dir = tempfile::tempdir().unwrap();
    let empty = dir.path().join("empty_dir");
    std::fs::create_dir(&empty).unwrap();
    let (ok, said) = run_cli(dir.path(), covered_job(dir.path(), "emptydir.abt", Some(&empty)));
    assert!(!ok, "an empty buildings directory must fail the run: {said}");
    assert!(said.contains("holds no .fgb file"), "{said}");
}
