//! The `plan` subcommand: enumerate, without downloading or converting
//! anything, exactly the `.abt` tiles an area/resolution request produces.
//!
//! The geometry here deliberately reproduces the Python authority
//! (`waveshed/core/terrain_adapter.py` in the QGIS plugin) EXACTLY — the
//! plugin cross-checks its own enumeration against this output and aborts on
//! any mismatch. Every constant and rounding step below mirrors that file;
//! do not "improve" the arithmetic without versioning the schema.

use anyhow::{bail, Result};
use serde::Serialize;

/// `.abt` tile extent (degrees) per resolution (metres), as a `<=` ladder:
/// the first entry whose key is `>= res` applies, and anything coarser than
/// the last entry takes its extent. Mirrors `ABT_EXTENT_DEG`.
const EXTENT_LADDER: &[(u32, f64)] =
    &[(2, 0.1), (5, 0.25), (10, 0.25), (30, 0.5), (90, 1.0), (250, 2.0)];

/// The `.abt` header stores the row stride as a u16, capping the tile side at
/// ~32000 px; the extent is halved until a tile fits. Mirrors `_ABT_MAX_SIZE_PX`.
const ABT_MAX_SIZE_PX: f64 = 32000.0;

/// Hard ceiling on the enumeration; beyond this the bbox is a typo, not a plan.
const MAX_TILES: usize = 2_000_000;

/// Python's `round(x, 6)`, as specified for this geometry: f64,
/// round-half-away-from-zero via `(x*1e6).round()/1e6`.
#[inline]
fn round6(x: f64) -> f64 {
    (x * 1e6).round() / 1e6
}

fn extent_for_resolution(res: u32) -> f64 {
    for &(key, extent) in EXTENT_LADDER {
        if res <= key {
            return extent;
        }
    }
    EXTENT_LADDER[EXTENT_LADDER.len() - 1].1
}

/// Sub-tile size in degrees for one resolution: the ladder extent, halved
/// while the row stride would overflow the `.abt` u16 header field.
pub fn subtile_degrees(res: u32) -> f64 {
    let mut sub = extent_for_resolution(res);
    while sub > 0.05 && sub * 111_111.0 / res as f64 > ABT_MAX_SIZE_PX {
        sub /= 2.0;
    }
    sub
}

#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct PlanTile {
    /// The integer resolution the caller asked for (also printed in the
    /// filename); `exact_res_m` is what the tile actually delivers.
    pub resolution_m: u32,
    pub filename: String,
    pub ul_lat: f64,
    pub ul_lon: f64,
    pub size_px: u32,
    pub exact_res_m: f64,
    pub est_bytes: u64,
}

#[derive(Serialize, Debug)]
pub struct PlanOutput {
    pub schema: &'static str,
    pub tile_count: usize,
    pub total_bytes: u64,
    pub tiles: Vec<PlanTile>,
}

/// One tile's parameters — the `_tile_params` formula verbatim: `calc_size`
/// rounded up to a multiple of 4 (BC6H block alignment), the resolution
/// recomputed so the tile spans exactly `sub` degrees, `ul_lat` the north
/// edge, `ul_lon` the west edge.
fn tile_params(sub: f64, lat: f64, lon: f64, res: u32) -> PlanTile {
    let target_deg = res as f64 / 111_111.0;
    // round_ties_even: Python's int(round()) is banker's rounding, and the
    // cross-checking plugin must never see an off-by-one size_px on an exact
    // .5 ratio.
    let mut calc_size = (sub / target_deg).round_ties_even() as u64;
    calc_size = calc_size.div_ceil(4) * 4;
    let exact_res = sub / calc_size as f64 * 111_111.0;
    // .abt on-disk size: 44-byte header + 256-aligned rows of i16.
    let stride = (calc_size * 2 + 255) & !255;
    PlanTile {
        resolution_m: res,
        filename: format!("tile_N{:.2}E{:.2}_{}m.abt", lat + sub, lon, res),
        ul_lat: round6(lat + sub),
        ul_lon: round6(lon),
        size_px: calc_size as u32,
        exact_res_m: exact_res,
        est_bytes: 44 + stride * calc_size,
    }
}

/// Build the plan for a bbox and a set of integer resolutions.
///
/// The bbox is snapped OUTWARD on the finest sub-tile grid among the
/// requested resolutions, then each resolution (ascending) is enumerated over
/// the snapped bbox — lat/lon from `floor(edge/sub)*sub`, stepping
/// `round6(x + sub)`, while strictly below the far edge.
pub fn build_plan(
    south: f64,
    north: f64,
    west: f64,
    east: f64,
    resolutions: &[u32],
) -> Result<PlanOutput> {
    for (flag, v) in [("--south", south), ("--north", north), ("--west", west), ("--east", east)] {
        if !v.is_finite() {
            bail!("{flag} must be a finite coordinate in degrees, got {v}");
        }
    }
    if south >= north {
        bail!("--south ({south}) must be strictly less than --north ({north})");
    }
    if west >= east {
        // west > east is how a bbox that wraps the antimeridian arrives from a
        // map canvas. Say so: "make --west smaller" is not the fix, and the
        // grid below has no wrapping in it to fall back on.
        let wrap = if west > 0.0 && east < 0.0 {
            " — a bbox wrapping the antimeridian is not supported; split it at \
             ±180 and plan each half separately"
        } else {
            ""
        };
        bail!("--west ({west}) must be strictly less than --east ({east}){wrap}");
    }
    // The grid is plain WGS84 degrees and wraps nowhere: a bbox reaching past
    // ±180 / ±90 would be enumerated as if 180.4 were a real longitude, and
    // the run would write tiles for ground that does not exist. Refuse it here
    // — the plugin aborts on any `plan` failure, so this is the loud failure
    // the antimeridian case needs.
    if west < -180.0 || east > 180.0 {
        bail!(
            "longitude out of range: --west ({west}) and --east ({east}) must lie \
             within [-180, 180] — a bbox crossing the antimeridian is not supported; \
             split it at ±180 and plan each half separately"
        );
    }
    if south < -90.0 || north > 90.0 {
        bail!(
            "latitude out of range: --south ({south}) and --north ({north}) must lie \
             within [-90, 90]"
        );
    }
    if resolutions.is_empty() {
        bail!("--resolutions must list at least one integer resolution in metres, e.g. 30,90");
    }
    if resolutions.contains(&0) {
        bail!("--resolutions entries must be integers > 0, got 0");
    }
    // Decision: duplicates collapse — "30,30,90" plans each resolution once,
    // ascending, matching the plugin's set-of-resolutions semantics.
    let mut res_list = resolutions.to_vec();
    res_list.sort_unstable();
    res_list.dedup();

    let sub_finest = res_list
        .iter()
        .map(|&r| subtile_degrees(r))
        .fold(f64::INFINITY, f64::min);
    let snapped_south = (south / sub_finest).floor() * sub_finest;
    let snapped_north = (north / sub_finest).ceil() * sub_finest;
    let snapped_west = (west / sub_finest).floor() * sub_finest;
    let snapped_east = (east / sub_finest).ceil() * sub_finest;

    let mut tiles: Vec<PlanTile> = Vec::new();
    for &res in &res_list {
        let sub = subtile_degrees(res);
        let mut lat = (snapped_south / sub).floor() * sub;
        while lat < snapped_north {
            let mut lon = (snapped_west / sub).floor() * sub;
            while lon < snapped_east {
                if tiles.len() >= MAX_TILES {
                    bail!(
                        "bbox too large: the request would produce more than {MAX_TILES} \
                         tiles; shrink the bbox or use coarser resolutions"
                    );
                }
                tiles.push(tile_params(sub, lat, lon, res));
                lon = round6(lon + sub);
            }
            lat = round6(lat + sub);
        }
    }

    let total_bytes = tiles.iter().map(|t| t.est_bytes).sum();
    Ok(PlanOutput { schema: "aether-plan/1", tile_count: tiles.len(), total_bytes, tiles })
}
