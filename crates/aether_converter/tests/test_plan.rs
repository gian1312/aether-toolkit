//! The `plan` geometry must reproduce the Python authority
//! (`waveshed/core/terrain_adapter.py`) EXACTLY — consumers cross-check tile
//! counts and filenames against this output and abort on mismatch. The golden
//! vectors below were computed by running the authority's formulas
//! (`_subtile_degrees` / `_tile_params` / the enumeration loop) in Python.

use std::collections::BTreeSet;

use aether_converter::plan::{build_plan, subtile_degrees, PlanOutput};

#[test]
fn golden_single_resolution_bbox() {
    // plan(47.2, 47.6, 8.1, 8.4, [30]) per the Python formulas.
    let p = build_plan(47.2, 47.6, 8.1, 8.4, &[30]).unwrap();
    assert_eq!(p.schema, "aether-plan/1");
    assert_eq!(p.tile_count, 2);
    assert_eq!(p.total_bytes, 14_223_448);
    assert_eq!(p.tiles.len(), 2);

    let t = &p.tiles[0];
    assert_eq!(t.resolution_m, 30);
    assert_eq!(t.filename, "tile_N47.50E8.00_30m.abt");
    assert_eq!(t.ul_lat, 47.5);
    assert_eq!(t.ul_lon, 8.0);
    assert_eq!(t.size_px, 1852);
    assert_eq!(t.exact_res_m, 29.99757019438445);
    assert_eq!(t.est_bytes, 7_111_724);
    assert_eq!(p.tiles[1].filename, "tile_N48.00E8.00_30m.abt");
}

#[test]
fn golden_two_resolutions_snap_on_the_finest_grid() {
    // plan(46.99, 47.51, 7.49, 8.01, [30, 90]): the bbox snaps outward on the
    // FINEST sub (0.5 deg), so the 90 m pass runs over the snapped bbox
    // (46.5..47.5, 7.0..8.5) and enumerates lat 46,47 x lon 7,8.
    let p = build_plan(46.99, 47.51, 7.49, 8.01, &[30, 90]).unwrap();
    assert_eq!(p.tile_count, 13);
    assert_eq!(p.total_bytes, 76_662_332);

    let names: Vec<&str> = p.tiles.iter().map(|t| t.filename.as_str()).collect();
    assert_eq!(
        names,
        vec![
            "tile_N47.00E7.00_30m.abt",
            "tile_N47.00E7.50_30m.abt",
            "tile_N47.00E8.00_30m.abt",
            "tile_N47.50E7.00_30m.abt",
            "tile_N47.50E7.50_30m.abt",
            "tile_N47.50E8.00_30m.abt",
            "tile_N48.00E7.00_30m.abt",
            "tile_N48.00E7.50_30m.abt",
            "tile_N48.00E8.00_30m.abt",
            "tile_N47.00E7.00_90m.abt",
            "tile_N47.00E8.00_90m.abt",
            "tile_N48.00E7.00_90m.abt",
            "tile_N48.00E8.00_90m.abt",
        ]
    );
    let t90 = &p.tiles[9];
    assert_eq!((t90.size_px, t90.est_bytes), (1236, 3_164_204));
    assert_eq!(t90.exact_res_m, 89.89563106796118);
}

#[test]
fn golden_fine_resolution_tile() {
    // plan(47.0, 47.05, 8.0, 8.05, [2]).
    let p = build_plan(47.0, 47.05, 8.0, 8.05, &[2]).unwrap();
    assert_eq!(p.tile_count, 1);
    let t = &p.tiles[0];
    assert_eq!(t.filename, "tile_N47.10E8.00_2m.abt");
    assert_eq!((t.ul_lat, t.ul_lon), (47.1, 8.0));
    assert_eq!(t.size_px, 5556);
    assert_eq!(t.exact_res_m, 1.9998380129589635);
    assert_eq!(t.est_bytes, 62_582_828);
}

#[test]
fn the_extent_ladder_and_stride_guard_match_the_authority() {
    // "<=" ladder, off-table values take the next entry up, coarser than the
    // last entry takes its extent. (Values from the Python functions.)
    assert_eq!(subtile_degrees(1), 0.1);
    assert_eq!(subtile_degrees(2), 0.1);
    assert_eq!(subtile_degrees(5), 0.25);
    assert_eq!(subtile_degrees(7), 0.25);
    assert_eq!(subtile_degrees(10), 0.25);
    assert_eq!(subtile_degrees(30), 0.5);
    assert_eq!(subtile_degrees(90), 1.0);
    assert_eq!(subtile_degrees(250), 2.0);
    assert_eq!(subtile_degrees(300), 2.0);
}

#[test]
fn the_output_is_stable_and_the_field_order_is_frozen() {
    let a = build_plan(47.2, 47.6, 8.1, 8.4, &[30]).unwrap();
    let b = build_plan(47.2, 47.6, 8.1, 8.4, &[30]).unwrap();
    let ja = serde_json::to_string(&a).unwrap();
    let jb = serde_json::to_string(&b).unwrap();
    assert_eq!(ja, jb, "two identical requests must serialize identically");
    // The exact document shape consumers parse (schema aether-plan/1).
    assert!(ja.starts_with(r#"{"schema":"aether-plan/1","tile_count":2,"total_bytes":14223448,"tiles":[{"resolution_m":30,"filename":"tile_N47.50E8.00_30m.abt","ul_lat":47.5,"ul_lon":8.0,"size_px":1852,"exact_res_m":29.99757019438445,"est_bytes":7111724}"#),
        "serialized plan drifted: {ja}");
}

#[test]
fn duplicate_resolutions_collapse() {
    let a = build_plan(47.2, 47.6, 8.1, 8.4, &[30, 30]).unwrap();
    let b = build_plan(47.2, 47.6, 8.1, 8.4, &[30]).unwrap();
    assert_eq!(a.tile_count, b.tile_count);
}

// ── Validation ────────────────────────────────────────────────────────────

#[test]
fn non_finite_coordinates_are_refused() {
    let err = build_plan(f64::NAN, 47.6, 8.1, 8.4, &[30]).unwrap_err().to_string();
    assert!(err.contains("--south") && err.contains("finite"), "got {err:?}");
    let err = build_plan(47.2, f64::INFINITY, 8.1, 8.4, &[30]).unwrap_err().to_string();
    assert!(err.contains("--north"), "got {err:?}");
}

#[test]
fn inverted_or_empty_extents_are_refused() {
    let err = build_plan(47.6, 47.2, 8.1, 8.4, &[30]).unwrap_err().to_string();
    assert!(err.contains("--south") && err.contains("--north"), "got {err:?}");
    let err = build_plan(47.2, 47.6, 8.4, 8.1, &[30]).unwrap_err().to_string();
    assert!(err.contains("--west") && err.contains("--east"), "got {err:?}");
    // Degenerate (equal) edges are empty too.
    assert!(build_plan(47.2, 47.2, 8.1, 8.4, &[30]).is_err());
}

#[test]
fn empty_or_zero_resolutions_are_refused() {
    let err = build_plan(47.2, 47.6, 8.1, 8.4, &[]).unwrap_err().to_string();
    assert!(err.contains("--resolutions"), "got {err:?}");
    let err = build_plan(47.2, 47.6, 8.1, 8.4, &[30, 0]).unwrap_err().to_string();
    assert!(err.contains("> 0"), "got {err:?}");
}

#[test]
fn a_bbox_outside_the_globe_is_refused() {
    // The Fiji torture case: 179.6 -> 180.4 straddles the antimeridian. The
    // grid wraps nowhere, so planning it would enumerate tiles for ground that
    // does not exist. It has to fail loudly instead.
    let err = build_plan(-18.3, -17.9, 179.6, 180.4, &[30]).unwrap_err().to_string();
    assert!(err.contains("antimeridian") && err.contains("180"), "got {err:?}");
    let err = build_plan(-18.3, -17.9, -180.4, -179.6, &[30]).unwrap_err().to_string();
    assert!(err.contains("longitude out of range"), "got {err:?}");
    let err = build_plan(-90.5, -89.9, 8.1, 8.4, &[30]).unwrap_err().to_string();
    assert!(err.contains("latitude out of range"), "got {err:?}");
    // The other spelling of the same wrap: west east of east.
    let err = build_plan(-18.3, -17.9, 179.6, -179.6, &[30]).unwrap_err().to_string();
    assert!(err.contains("antimeridian"), "got {err:?}");
    // ...and the exact domain limits stay legal.
    assert!(build_plan(-90.0, 90.0, -180.0, 180.0, &[250]).is_ok());
}

#[test]
fn an_absurd_bbox_fails_as_too_large_instead_of_hanging() {
    // The whole globe at 2 m: ~ (180/0.1)*(360/0.1) = 6.48M tiles > 2M.
    let err = build_plan(-90.0, 90.0, -180.0, 180.0, &[2]).unwrap_err().to_string();
    assert!(err.contains("bbox too large"), "got {err:?}");
}

// ── The snap and start tolerance (0.2.9, changelog 43) ────────────────────
//
// Expected values below are the decimal grid each bbox calls for — and what
// the plugin's formulas give for converters >= 0.2.9 (`_snap_bbox` with the
// 1e-9 tolerance and `round(idx * finest, 9)`, `_enumerate_tiles` starting at
// `floor(edge / ext + 1e-9)`).

/// Every tile of *p* as (resolution, row of its south edge, column of its
/// west edge), in grid steps of its own extent.
fn tile_ids(p: &PlanOutput) -> BTreeSet<(u32, i64, i64)> {
    p.tiles
        .iter()
        .map(|t| {
            let sub = subtile_degrees(t.resolution_m);
            (t.resolution_m, (t.ul_lat / sub).round() as i64 - 1, (t.ul_lon / sub).round() as i64)
        })
        .collect()
}

/// The tiles the DECIMAL bbox calls for, in exact integer arithmetic: edges
/// in units of 1e-4°, of which every extent is a whole number.
fn decimal_grid(s: i64, n: i64, w: i64, e: i64, res: &[u32]) -> BTreeSet<(u32, i64, i64)> {
    let unit = |deg: f64| (deg * 10_000.0).round() as i64;
    let fl = |a: i64, b: i64| a.div_euclid(b);
    let ce = |a: i64, b: i64| -(-a).div_euclid(b);
    let finest = res.iter().map(|&r| unit(subtile_degrees(r))).min().unwrap();
    let (s, n) = (fl(s, finest) * finest, ce(n, finest) * finest);
    let (w, e) = (fl(w, finest) * finest, ce(e, finest) * finest);
    let mut out = BTreeSet::new();
    for &r in res {
        let ext = unit(subtile_degrees(r));
        for row in fl(s, ext)..ce(n, ext) {
            for col in fl(w, ext)..ce(e, ext) {
                out.insert((r, row, col));
            }
        }
    }
    out
}

fn distinct(values: impl Iterator<Item = f64>) -> Vec<f64> {
    let mut v: Vec<f64> = values.collect();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v.dedup();
    v
}

#[test]
fn the_owner_bbox_plans_no_column_east_of_the_area() {
    // The owner's run (2026-10-23): S45.7161 N48.1913 W4.9482 E11.6466 at
    // 2, 5, 10, 30, 90 m. Up to 0.2.8 the east edge snapped to the bare
    // product 117 * 0.1 = 11.700000000000001, a hair past the 11.7 the
    // 6-decimal steps reach, so `lon < east` held once more: a 69th 2 m
    // column (E11.70, 25 tiles wholly east of the area, all VOID, 1.56 GB)
    // and 2,463 tiles.
    let p = build_plan(45.7161, 48.1913, 4.9482, 11.6466, &[2, 5, 10, 30, 90]).unwrap();
    let per_res = |r: u32| p.tiles.iter().filter(|t| t.resolution_m == r).count();
    assert_eq!(
        [per_res(2), per_res(5), per_res(10), per_res(30), per_res(90)],
        [1_700, 308, 308, 90, 32]
    );
    assert_eq!(p.tile_count, 2_438);

    let cols = distinct(p.tiles.iter().filter(|t| t.resolution_m == 2).map(|t| t.ul_lon));
    assert_eq!(cols.len(), 68, "2 m columns");
    assert_eq!((cols[0], cols[67]), (4.9, 11.6), "W4.90 … E11.60, nothing at E11.70");
    assert_eq!(tile_ids(&p), decimal_grid(457_161, 481_913, 49_482, 116_466, &[2, 5, 10, 30, 90]));
}

#[test]
fn an_edge_given_on_the_grid_is_not_moved() {
    // E 11.7 given as 11.7 must end the plan at 11.7: seven 2 m columns,
    // E11.00 … E11.60. (Up to 0.2.8: eight, the same product artefact as the
    // owner's bbox.) S 47.3 is on the grid too, and 47.3 / 0.1 =
    // 472.99999999999994 — the bare floor of the snap, and then of the
    // enumeration start, took it one row south (N47.30, wholly outside).
    let p = build_plan(47.3, 47.6, 11.0, 11.7, &[2]).unwrap();
    assert_eq!(
        distinct(p.tiles.iter().map(|t| t.ul_lon)),
        vec![11.0, 11.1, 11.2, 11.3, 11.4, 11.5, 11.6]
    );
    assert_eq!(distinct(p.tiles.iter().map(|t| t.ul_lat)), vec![47.4, 47.5, 47.6]);
    assert_eq!(p.tile_count, 21, "was 32");

    // The plugin's own on-grid case (EpsilonSnap.ON_GRID): 9 tiles, was 12.
    assert_eq!(build_plan(47.3, 47.6, 8.3, 8.6, &[2]).unwrap().tile_count, 9);

    // West of Greenwich the CEIL side is the one a quotient overshoots:
    // -69.8 / 0.1 = -697.9999999999999 took the bare ceil to -697, a column
    // east of the area. 15 tiles, was 24.
    let p = build_plan(24.7, 25.0, -70.3, -69.8, &[2]).unwrap();
    assert_eq!(distinct(p.tiles.iter().map(|t| t.ul_lon)), vec![-70.3, -70.2, -70.1, -70.0, -69.9]);
    assert_eq!(distinct(p.tiles.iter().map(|t| t.ul_lat)), vec![24.8, 24.9, 25.0]);
    assert_eq!(p.tile_count, 15);
}

#[test]
fn the_plan_is_the_decimal_grid_for_six_thousand_bboxes() {
    // Measured over this very sweep with the older formulas swapped in: up to
    // 0.2.8, 1,373 of the 6,000 plans had a row or column wholly outside the
    // area (or one missing); the 1e-9 snap alone, 995; snap and rounded edge
    // without the tolerant enumeration start, 761. All three together: none.
    let mut seed = 0x9E37_79B9_7F4A_7C15u64;
    let mut next = move |lo: i64, hi: i64| {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        lo + ((seed >> 33) as i64).rem_euclid(hi - lo + 1)
    };
    let sets: [&[u32]; 5] = [&[2], &[2, 5, 10, 30, 90], &[5, 30], &[10], &[30, 90]];
    for case in 0..6_000 {
        // Half with 4-decimal edges anywhere, half with edges on the 0.1° grid.
        let (s, n, w, e) = if case % 2 == 0 {
            let (s, w) = (next(-800_000, 790_000), next(-1_790_000, 1_780_000));
            (s, s + next(100, 10_000), w, w + next(100, 10_000))
        } else {
            let (s, w) = (1_000 * next(-800, 790), 1_000 * next(-1_790, 1_780));
            (s, s + 1_000 * next(1, 5), w, w + 1_000 * next(1, 5))
        };
        let res = sets[next(0, 4) as usize];
        let deg = |v: i64| v as f64 / 10_000.0;
        let p = build_plan(deg(s), deg(n), deg(w), deg(e), res).unwrap();
        assert_eq!(
            tile_ids(&p),
            decimal_grid(s, n, w, e, res),
            "case {case}: S{} N{} W{} E{} at {res:?}",
            deg(s),
            deg(n),
            deg(w),
            deg(e)
        );
    }
}
