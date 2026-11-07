//! The seam filter of the CLI `download` (CONTRACT §9b `seam_filter`,
//! changelog 49; `aether_converter` 0.2.13).
//!
//! AWS Terrarium's mosaic has seams at some 1° source boundaries: at 10°E,
//! lat 46.7 – 46.9, the zoom-15 tile column x = 17294 holds −8,956 …
//! −9,717 m in pixel column 52 and −1,184 … +149 m in column 53 between
//! terrain of 1,500 – 3,000 m — valid samples next to a −32768 m nodata
//! area of the source, blended into it by the service's resampling (≈ 0.33
//! and ≈ 0.08 of −32768 m, measured), and the zoom-14 tile above them one
//! column of −3,524 … −4,709 m. The same shape was measured at 28°N over
//! Everest (z15 rows of −6,555 / +3,680 m between 8,260 m), at 70°W over
//! Aconcagua (z15 columns of −8,142 / +1,544 m between 6,070 m) and as
//! single-sample lines at z14/z15 near Trollveggen (one slanting), Yarlung
//! Tsangpo and the Dead Sea. Such a sample is above `NODATA_M`, so the
//! parent fill took it for terrain and the `.abt` got a trench kilometres
//! deep.
//!
//! **The rule.** A valid sample (above `NODATA_M`) is a *seam notch* when
//! both its row neighbours are higher than it by more than
//! [`SEAM_NOTCH_M`], or both its column neighbours are — a per-axis test:
//! a seam is a line, and the samples along it are its neighbours on the
//! other axis. A neighbour is the nearest valid sample within
//! [`SEAM_REACH_PX`] samples in that direction, inside the tile: a void is
//! skipped, never a neighbour; the tile edge or a run of voids ends the
//! search, and an axis without a neighbour on both sides is not judged.
//! Every notch of a pass is found against the tile as the pass found it,
//! then all are voided at once ([`SEAM_VOID_M`]); the rule runs again, at
//! most [`SEAM_REACH_PX`] passes, and stops at the first pass that finds
//! none. A two-sample seam whose inner sample is the lower (column 52 below
//! column 53) therefore goes in two passes: column 53's left neighbour is
//! column 52 in the first, column 51 once column 52 is void. Only tiles of
//! zoom [`SEAM_MIN_ZOOM`] or finer are filtered. The voided samples are
//! filled by the parent fill like the service's own voids.
//!
//! **The constants** come from a survey with this very file (changelog 49):
//! AWS Terrarium tiles over the steepest relief — Alps, Norway, US canyons
//! and walls, Himalaya, Karakoram, Pamir, Andes, Alaska, New Zealand, a
//! submarine canyon coast, the Dead Sea; at z11 every tile of 68 – 98°E ×
//! 27 – 38°N (13,072 tiles). The deepest notch of terrain is 1,030 m (a
//! gorge at z11 that the service's z12 and z13 tiles show too), ≤ 439 m at
//! z12, ≤ 141 m at z13, ≤ 51 m at z14 and ≤ 17 m at z15: [`SEAM_NOTCH_M`]
//! is 2.1× the first. Deeper notches at those zooms are seams like the one
//! above or single corrupted samples (holes at the shores of flat-filled
//! lakes, pits beside +6 … +21 km spikes, isolated pits; checked against
//! the service's own finer tiles). The shallowest seam sample of
//! 10°E is 2,793 m deep. Zooms 6 – 10 were surveyed thinly, and at 7 – 9
//! the service's own tiles hold single-sample pits of 3.7 – 6.0 km in
//! Himalayan gorges, so no depth there tells a seam from the data — hence
//! [`SEAM_MIN_ZOOM`].
//!
//! Self-contained (it needs only `NODATA_M` from its parent), so a survey
//! tool can compile this file and measure with the rule the converter runs.

use super::NODATA_M;

/// How much higher than a sample both its neighbours on one axis must be
/// for the sample to be a seam notch, in metres (D).
pub(super) const SEAM_NOTCH_M: f32 = 2200.0;

/// How far a neighbour search reaches past void samples, in samples, and
/// how many passes the rule runs at most (W).
pub(super) const SEAM_REACH_PX: usize = 2;

/// The coarsest zoom the rule runs at: tiles of zoom 11 – 15 (and finer).
pub(super) const SEAM_MIN_ZOOM: u32 = 11;

/// The value a voided seam sample holds in memory: a void like every value
/// at or below `NODATA_M`, and below anything a Terrarium or Mapbox tile
/// decodes to (−32768 m and −10000 m are their lowest), so a seam void is
/// told from the service's own voids by its value alone.
pub(super) const SEAM_VOID_M: f32 = -65536.0;

#[inline(always)]
fn valid(v: f32) -> bool {
    v > NODATA_M
}

/// The nearest valid sample left of / right of / above / below sample
/// `(x, y)` within `reach` samples, inside the tile.
#[inline]
fn left(g: &[f32], e: usize, x: usize, y: usize, reach: usize) -> Option<f32> {
    (1..=reach.min(x)).map(|k| g[y * e + x - k]).find(|&v| valid(v))
}
#[inline]
fn right(g: &[f32], e: usize, x: usize, y: usize, reach: usize) -> Option<f32> {
    (1..=reach.min(e - 1 - x)).map(|k| g[y * e + x + k]).find(|&v| valid(v))
}
#[inline]
fn above(g: &[f32], e: usize, x: usize, y: usize, reach: usize) -> Option<f32> {
    (1..=reach.min(y)).map(|k| g[(y - k) * e + x]).find(|&v| valid(v))
}
#[inline]
fn below(g: &[f32], e: usize, x: usize, y: usize, reach: usize) -> Option<f32> {
    (1..=reach.min(e - 1 - y)).map(|k| g[(y + k) * e + x]).find(|&v| valid(v))
}

/// How far the lower of the two neighbours lies above `v`; −∞ when either
/// side has none.
#[inline]
fn axis_depth(v: f32, a: Option<f32>, b: Option<f32>) -> f32 {
    match (a, b) {
        (Some(a), Some(b)) => a.min(b) - v,
        _ => f32::NEG_INFINITY,
    }
}

/// The notch depth of sample `(x, y)` of the `e`×`e` tile `g` with
/// neighbours searched `reach` samples deep: the deeper of its row notch
/// and its column notch (−∞ for a void sample, and for one neither axis can
/// judge). The rule voids a sample whose depth exceeds the threshold.
pub(super) fn notch_depth(g: &[f32], e: usize, x: usize, y: usize, reach: usize) -> f32 {
    let v = g[y * e + x];
    if !valid(v) {
        return f32::NEG_INFINITY;
    }
    let row = axis_depth(v, left(g, e, x, y, reach), right(g, e, x, y, reach));
    let col = axis_depth(v, above(g, e, x, y, reach), below(g, e, x, y, reach));
    row.max(col)
}

/// The samples of one pass whose notch is deeper than `depth_m`, against
/// the tile as it is. Exactly `notch_depth(..) > depth_m` for every sample;
/// a sample whose four immediate neighbours are all valid and none more
/// than `depth_m` above it cannot be one (its neighbours on each axis are
/// those samples, and `min(a, b) − v` is `min(a − v, b − v)` in floating
/// point too), so only the others are looked at closely — nearly none in
/// real terrain.
fn pass(g: &[f32], e: usize, depth_m: f32, reach: usize, hits: &mut Vec<usize>) {
    hits.clear();
    let mut close = |x: usize, y: usize| {
        if notch_depth(g, e, x, y, reach) > depth_m {
            hits.push(y * e + x);
        }
    };
    for y in 0..e {
        // The edge rows and columns lack a neighbour: always looked at
        // closely (`notch_depth` is −∞ for a void sample).
        if y == 0 || y + 1 == e {
            (0..e).for_each(|x| close(x, y));
            continue;
        }
        let (up, row, dn) = (&g[(y - 1) * e..y * e], &g[y * e..(y + 1) * e], &g[(y + 1) * e..(y + 2) * e]);
        close(0, y);
        for x in 1..e - 1 {
            let (v, l, r, u, d) = (row[x], row[x - 1], row[x + 1], up[x], dn[x]);
            // Without branches, so it vectorises: a void sample is never
            // one; a quiet one (every neighbour valid and none more than
            // `depth_m` above it) cannot be one. Any NaN fails a comparison
            // and is looked at closely, as the definition says.
            let quiet = (l > NODATA_M) & (r > NODATA_M) & (u > NODATA_M) & (d > NODATA_M)
                & (l - v <= depth_m) & (r - v <= depth_m) & (u - v <= depth_m) & (d - v <= depth_m);
            if valid(v) & !quiet {
                close(x, y);
            }
        }
        close(e - 1, y);
    }
}

/// The rule with explicit constants (tests, the survey's candidates);
/// returns the samples voided, all passes together. A grid that is not
/// `e`×`e`, or smaller than 3×3, is left alone.
pub(super) fn void_notches_with(g: &mut [f32], e: usize, depth_m: f32, reach: usize) -> usize {
    if e < 3 || g.len() != e * e {
        return 0;
    }
    let mut total = 0;
    let mut hits = Vec::new();
    for _ in 0..reach.max(1) {
        pass(g, e, depth_m, reach, &mut hits);
        if hits.is_empty() {
            break;
        }
        for &i in &hits {
            g[i] = SEAM_VOID_M;
        }
        total += hits.len();
    }
    total
}

/// Void every seam notch of the decoded `e`×`e` zoom-`zoom` tile `g`: each
/// becomes [`SEAM_VOID_M`] ([`SEAM_NOTCH_M`], [`SEAM_REACH_PX`]; nothing
/// below [`SEAM_MIN_ZOOM`]). Returns how many.
pub(super) fn void_seam_notches(g: &mut [f32], e: usize, zoom: u32) -> usize {
    if zoom < SEAM_MIN_ZOOM {
        return 0;
    }
    void_notches_with(g, e, SEAM_NOTCH_M, SEAM_REACH_PX)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Terrarium's void (`0, 0, 0`).
    const BLANK: f32 = -32768.0;
    const E: usize = 16;

    /// An `E`×`E` tile of gentle terrain: 2,400 m, falling 1.5 m per column
    /// and rising 2.25 m per row (1/256-m steps, as Terrarium decodes).
    fn terrain() -> Vec<f32> {
        (0..E * E).map(|i| 2400.0 - 1.5 * (i % E) as f32 + 2.25 * (i / E) as f32).collect()
    }

    fn col(g: &[f32], c: usize) -> Vec<f32> {
        (0..E).map(|r| g[r * E + c]).collect()
    }

    /// The one pass of the rule, with the converter's constants.
    fn one_pass(g: &[f32]) -> Vec<usize> {
        let mut hits = Vec::new();
        pass(g, E, SEAM_NOTCH_M, SEAM_REACH_PX, &mut hits);
        hits
    }

    #[test]
    fn a_one_sample_notch_is_voided_on_either_axis() {
        // A column 6,000 m below its neighbours (the z14 seam at 10°E is
        // −3,524 … −4,709 m between ~2,500 m): voided in one pass, the rest
        // untouched.
        let mut g = terrain();
        for r in 0..E {
            g[r * E + 9] -= 6000.0;
        }
        let before = g.clone();
        assert_eq!(void_seam_notches(&mut g, E, 14), E);
        for (i, (a, b)) in g.iter().zip(&before).enumerate() {
            if i % E == 9 { assert_eq!(*a, SEAM_VOID_M) } else { assert_eq!(a, b, "sample {i}") }
        }
        // The same as a row (a seam along a parallel, e.g. 28°N at z14).
        let mut g = terrain();
        for c in 0..E {
            g[4 * E + c] -= 8775.0;
        }
        assert_eq!(void_seam_notches(&mut g, E, 14), E);
        assert!((0..E).all(|c| g[4 * E + c] == SEAM_VOID_M));
        assert_eq!(g.iter().filter(|&&v| v == SEAM_VOID_M).count(), E);
    }

    #[test]
    fn the_two_sample_seam_of_10e_needs_the_second_pass() {
        // Measured: z15/17294/11552 row 0, columns 46 … 63 (m, rounded) —
        // column 52 at −9,313 m, column 53 at −573 m.
        let row = [
            2279.0, 2279.0, 2280.0, 2280.0, 2279.0, 2279.0, -9313.0, -573.0, 2277.0, 2277.0, 2277.0,
            2277.0, 2277.0, 2277.0, 2276.0, 2276.0,
        ];
        let mut g: Vec<f32> = (0..E).flat_map(|r| row.iter().map(move |v| v + r as f32 * 0.5)).collect();
        // Pass 1 finds column 52 (here 6) only: column 53's left neighbour
        // is the lower one.
        let first = one_pass(&g);
        assert_eq!(first, (0..E).map(|r| r * E + 6).collect::<Vec<_>>());
        // Column 53 (7) on its own is 2,850 m below both sides, but in pass
        // 1 its row neighbours are −9,313 and 2,277: not a notch.
        assert!(notch_depth(&g, E, 7, 3, SEAM_REACH_PX) < 0.0);
        // The whole rule: column 6 in pass 1, column 7 in pass 2, against
        // column 5 two samples away.
        assert_eq!(void_seam_notches(&mut g, E, 15), 2 * E);
        assert!(col(&g, 6).iter().chain(&col(&g, 7)).all(|&v| v == SEAM_VOID_M));
        assert_eq!(g.iter().filter(|&&v| v == SEAM_VOID_M).count(), 2 * E);
        // With a reach of 1 the second column survives: W = 2 is needed.
        let mut h: Vec<f32> = (0..E).flat_map(|r| row.iter().map(move |v| v + r as f32 * 0.5)).collect();
        assert_eq!(void_notches_with(&mut h, E, SEAM_NOTCH_M, 1), E);
        assert!(col(&h, 7).iter().all(|&v| v < -500.0 && v > NODATA_M));
    }

    #[test]
    fn a_cliff_and_a_gorge_shallower_than_d_are_not_notches() {
        // A 6,000 m wall from one sample to the next — steeper than any
        // terrain — is lower on one side only: never a notch.
        let mut g: Vec<f32> = (0..E * E).map(|i| if i % E < 8 { 500.0 } else { 6500.0 }).collect();
        let before = g.clone();
        assert_eq!(void_seam_notches(&mut g, E, 15), 0);
        assert_eq!(g, before);
        // A one-sample spike is no notch either (the rule looks for pits).
        let mut g = terrain();
        g[5 * E + 5] += 20_000.0;
        assert_eq!(void_seam_notches(&mut g, E, 11), 0);
        // A V-gorge one sample wide, its floor 2,200 m below both walls —
        // the depth itself is not "more than" D — and the deepest terrain
        // gorge of the survey (1,030 m at z11) are left alone; 2,201 m goes.
        for (depth, voided) in [(1030.0, 0), (2200.0, 0), (2201.0, E)] {
            let mut g = terrain();
            for r in 0..E {
                let i = r * E + 7;
                g[i] = g[i - 1].min(g[i + 1]) - depth;
            }
            assert_eq!(void_seam_notches(&mut g, E, 11), voided, "a gorge {depth} m deep");
        }
        // A two-sample-wide gorge 5,000 m deep is two walls, not a notch:
        // each floor sample has the other beside it.
        let mut g = terrain();
        for r in 0..E {
            g[r * E + 7] -= 5000.0;
            g[r * E + 8] -= 5000.0;
        }
        assert_eq!(void_seam_notches(&mut g, E, 15), 0);
    }

    #[test]
    fn a_void_is_skipped_never_a_neighbour_and_never_touched() {
        // [terrain, VOID, −9,000, terrain]: the seam sample's left neighbour
        // is the terrain two samples away — voided.
        let mut g = terrain();
        for r in 0..E {
            g[r * E + 6] = BLANK;
            g[r * E + 7] = -9000.0;
        }
        assert_eq!(void_seam_notches(&mut g, E, 15), E);
        assert!(col(&g, 7).iter().all(|&v| v == SEAM_VOID_M));
        // The service's own voids stay what they were (the parent fill
        // fills both; only the filter's are counted as seam samples).
        assert!(col(&g, 6).iter().all(|&v| v == BLANK));

        // More voids than the reach on one side: that axis is not judged,
        // however much higher the other side is — so the edge of a blank
        // area is never voided by the rule…
        let mut g = terrain();
        for r in 0..E {
            for c in 0..=6 {
                g[r * E + c] = BLANK;
            }
            g[r * E + 7] = -9000.0;
        }
        let before = g.clone();
        assert_eq!(void_seam_notches(&mut g, E, 15), 0);
        assert_eq!(g, before);
        // …nor is valid terrain beside one, low as it may be.
        let mut g: Vec<f32> = (0..E * E).map(|i| if i % E < 8 { BLANK } else { -10_900.0 + (i % E) as f32 }).collect();
        let before = g.clone();
        assert_eq!(void_seam_notches(&mut g, E, 15), 0);
        assert_eq!(g, before);
        // A tile edge ends the search like a void run: the edge columns are
        // judged on the column axis only.
        let mut g = terrain();
        for r in 0..E {
            g[r * E] = -9000.0;
        }
        assert_eq!(void_seam_notches(&mut g, E, 15), 0);
    }

    #[test]
    fn a_smooth_bathymetry_slope_is_left_alone() {
        // From −10,900 m (just above NODATA_M) up 230 m per sample to land,
        // with a canyon 1,500 m deep and 3 samples wide cut into it, and an
        // abyssal trench 6,000 m deep but 5 samples wide: real, steep, no
        // single-sample notch.
        let mut g: Vec<f32> = (0..E * E).map(|i| -10_900.0 + 230.0 * ((i % E) + (i / E)) as f32 / 2.0).collect();
        for r in 0..E {
            for (c, d) in [(9, 750.0), (10, 1500.0), (11, 750.0)] {
                g[r * E + c] -= d;
            }
        }
        let mut t = g.clone();
        for r in 0..E {
            for (c, d) in [(2, 3000.0), (3, 6000.0), (4, 6000.0), (5, 6000.0), (6, 3000.0)] {
                t[r * E + c] = (t[r * E + c] - d).max(-10_999.0);
            }
        }
        for g in [g, t] {
            let mut h = g.clone();
            assert_eq!(void_seam_notches(&mut h, E, 15), 0);
            assert_eq!(h, g);
        }
    }

    #[test]
    fn the_rule_runs_from_zoom_11() {
        let mut g = terrain();
        g[5 * E + 5] -= 9000.0;
        let before = g.clone();
        assert_eq!(void_seam_notches(&mut g, E, SEAM_MIN_ZOOM - 1), 0, "zoom 10 is not filtered");
        assert_eq!(g, before);
        assert_eq!(void_seam_notches(&mut g, E, SEAM_MIN_ZOOM), 1);
        assert_eq!(g[5 * E + 5], SEAM_VOID_M);
        // A grid that is not edge × edge is left alone.
        let mut odd = vec![0.0f32; 10];
        assert_eq!(void_seam_notches(&mut odd, 3, 15), 0);
    }

    #[test]
    fn the_fast_pass_is_exactly_the_rule() {
        // Random tiles in 1/256-m steps with void runs, pits, spikes and
        // walls: the pass with its shortcut finds exactly the samples whose
        // notch depth exceeds D, at several depths and reaches.
        let mut s: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut rnd = move || {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            s
        };
        for _ in 0..200 {
            let e = 3 + (rnd() % 20) as usize;
            let g: Vec<f32> = (0..e * e)
                .map(|_| match rnd() % 10 {
                    0 => BLANK,
                    1 => SEAM_VOID_M,
                    2 => -9000.0 + (rnd() % 4000) as f32,
                    3 => 6000.0 + (rnd() % 3000) as f32,
                    _ => 1000.0 + (rnd() % 600_000) as f32 / 256.0,
                })
                .collect();
            for (depth, reach) in [(SEAM_NOTCH_M, SEAM_REACH_PX), (500.0, 1), (100.0, 3), (2201.0, 2)] {
                let mut fast = Vec::new();
                pass(&g, e, depth, reach, &mut fast);
                let slow: Vec<usize> = (0..e * e)
                    .filter(|&i| notch_depth(&g, e, i % e, i / e, reach) > depth)
                    .collect();
                assert_eq!(fast, slow, "edge {e}, depth {depth}, reach {reach}");
            }
        }
    }
}
