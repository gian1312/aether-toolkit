//! The `plan` geometry must reproduce the Python authority
//! (`waveshed/core/terrain_adapter.py`) EXACTLY — consumers cross-check tile
//! counts and filenames against this output and abort on mismatch. The golden
//! vectors below were computed by running the authority's formulas
//! (`_subtile_degrees` / `_tile_params` / the enumeration loop) in Python.

use aether_converter::plan::{build_plan, subtile_degrees};

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
