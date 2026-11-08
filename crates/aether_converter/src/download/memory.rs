//! The CLI `download`'s memory model (CONTRACT changelog 52): everything a
//! full pass holds besides its strips' tiles, priced from the job once the
//! first tile is in and before any strip is sized, so that the `[Download]
//! RAM budget: <mb> MB` line is what the process peaks at — the strips get
//! what the share leaves, and a pass whose share cannot hold the overhead
//! runs fewer connections.
//!
//! Until 0.2.14 the budget priced the strips alone and the process peaked
//! at 1.9 – 2.4× the printed figure on the owner's machine. Measured here
//! (the container, release build, 256 connections, a z15 pass 456 tiles
//! wide served by a local server; live heap from an LD_PRELOAD counter,
//! changelog 52): at the peak 7,595 decoded tiles held by strips (1,899 MB
//! of a 2,048 MB budget), the two assembly grids (2 × 114 MB), the output
//! rows of the tile-rows being written (35 – 71 MB), the connections' read
//! buffers and bodies (32 MB; 142 KB a connection between 32 and 1,024
//! connections), and the allocator's share on top of the live bytes.

use super::SubTileSpec;

/// Resident memory of a pass before its first tile: the binary's pages,
/// the runtime's threads and their stacks, the HTTP client and TLS roots
/// (measured: 32 – 36 MB peak for a 63-tile pass, 16 MB of it tiles).
pub(super) const BASE_BYTES: u64 = 24 << 20;

/// What the allocator keeps beyond the live bytes, in % of them, for a pass
/// with (*cache*) or without a raw tile cache — whose bodies of every size
/// churn through the heap. Measured as peak RSS over peak live heap on the
/// container's passes (changelog 52): glibc (Linux) 1 – 11 % on fetch-only
/// passes with 5 m outputs, 14 – 19 % with 2 m outputs (thousands of 11 KB
/// output rows a tile-row), 3 – 17 % with a cache (22 % on a 681 MB pass,
/// where the base weighs more); mimalloc (the native
/// Windows binary's allocator, changelog 37; the same mimalloc 3.3.2
/// measured on Linux, transparent huge pages off as on Windows) 17 – 35 %
/// fetch-only, 31 – 53 % with a cache. Where a pass's strips fill, the
/// figure must not fall short; the glibc figures are set so that the worst
/// pass measured still peaks within a few percent of its budget. The macOS
/// allocator is not measured; it gets the Linux figures.
pub(super) fn alloc_pct(cache: bool) -> u64 {
    match (cfg!(windows), cache) {
        (false, false) => 12,
        (false, true) => 20,
        (true, false) => 40,
        (true, true) => 55,
    }
}

/// The fewest connections a memory-short pass runs (the floor of the
/// adaptive window's own cut, changelog 30).
pub(super) const MIN_CONNS: usize = 16;

/// One connection's buffers besides its body: hyper's read buffer (grows to
/// 128 KiB on a fast link) and the TLS state. The body comes on top,
/// `edge² × 2` (a 256 px Terrarium PNG is 70 – 110 KB).
pub(super) const CONN_BYTES: u64 = 64 << 10;

/// The raw tile cache's write queue (64 MiB, `tile_cache::WRITE_QUEUE_BYTES`)
/// and the pack a write puts together (at most a column's 256 bodies).
pub(super) const CACHE_WRITE_BYTES: u64 = 96 << 20;

/// Per output row an assembly holds: its entry in the row list and the
/// allocator's word.
const ROW_HEAD_BYTES: u64 = 48;

/// The buildings post-pass at its peak, in % of the decoded set: the set and,
/// while it is put together, every tile's own building list and the
/// duplicate filter's arrays on top (measured: 1,258 MB live at the decode's
/// peak for a set of 912.4 MB, 13,824 OpenFreeMap tiles).
pub(super) const DECODE_PEAK_PCT: u64 = 140;

/// A pass's buildings post-pass, as known when its budget is drawn
/// (changelog 52). It runs after the terrain, so it is priced apart and the
/// budget is the larger of the two phases.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PostPass {
    /// The job has no building source.
    None,
    /// Its building tiles are there: the set's estimate (their bytes on
    /// disk × `buildings::DECODED_PER_DISK_BYTE`) and the largest output
    /// tile, which the post-pass reads whole.
    Counted { set: u64, tile: u64 },
    /// Its marker or list is not there yet (they come while the terrain
    /// downloads): nothing to estimate it from.
    Later,
}

impl PostPass {
    /// Its live bytes at the peak.
    pub fn live(&self) -> u64 {
        match *self {
            PostPass::Counted { set, tile } => set * DECODE_PEAK_PCT / 100 + tile,
            PostPass::None | PostPass::Later => 0,
        }
    }
}

/// *p* with its total the larger of the terrain's and the buildings
/// post-pass's (*post*, priced like the pass's other live bytes).
pub(super) fn with_post_pass(p: MemPlan, m: &PassMemory, post: PostPass) -> MemPlan {
    match post {
        PostPass::Counted { .. } => MemPlan { total: p.total.max(m.resident(post.live())), ..p },
        PostPass::None | PostPass::Later => p,
    }
}

/// Bytes per entry of an assembly's column tables (`x_span_table`).
const X_TABLE_BYTES: u64 = 8;

/// What one CLI download pass holds besides its strips' tiles, and the
/// tiles themselves (all byte counts are live bytes, before the allocator's
/// share).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct PassMemory {
    /// One decoded source tile: `edge² × 4`.
    pub tile: u64,
    /// One tile-row of the assembly grid: `nx × tile`.
    pub grid: u64,
    /// The output rows of the largest tile-row and the column tables: what
    /// an assembly holds while it writes ([`output_rows_bytes`]).
    pub rows: u64,
    /// Assemblies at once: 2, or 1 when the job is one strip.
    pub asm: u64,
    /// One connection: [`CONN_BYTES`] and a body.
    pub per_conn: u64,
    /// The tiles the network serves — the others come from the raw tile
    /// cache and take no connection.
    pub fetched: u64,
    /// The raw tile cache's writes ([`CACHE_WRITE_BYTES`]), or 0 without a
    /// cache or with nothing to write.
    pub cache: u64,
    /// The decode buffers of the runtime's workers (and what deriving a tile
    /// from finer cached tiles takes).
    pub decode: u64,
    /// The whole job's tiles: the strips never hold more.
    pub job: u64,
    /// What the allocator keeps beyond the live bytes, in % ([`alloc_pct`]).
    pub alloc_pct: u64,
}

/// How a pass spends its share ([`plan`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct MemPlan {
    /// Connections it runs (the job's, or fewer).
    pub conns: usize,
    /// The strips' budget: the bytes of tiles launched strips may hold
    /// (the launch gate counts the parent fill's tiles in it too).
    pub strips: u64,
    /// What the process peaks at: the `[Download] RAM budget:` figure.
    pub total: u64,
    /// The share could not hold even the strips' floor at [`MIN_CONNS`]:
    /// `total` exceeds it.
    pub over: bool,
}

impl PassMemory {
    /// Live bytes beside the strips at *conns* connections.
    pub fn overhead(&self, conns: usize) -> u64 {
        self.asm * (self.grid + self.rows)
            + (conns as u64).min(self.fetched) * self.per_conn
            + self.cache
            + self.decode
    }

    /// The strips' floor: two strips of one tile-row — one being written,
    /// one downloading — or the whole job when that is less.
    pub fn strips_min(&self) -> u64 {
        (2 * self.grid).min(self.job)
    }

    /// The memory *live* bytes take: the base, the bytes, the allocator's
    /// share of them.
    pub fn resident(&self, live: u64) -> u64 {
        BASE_BYTES + live + live * self.alloc_pct / 100
    }

    /// The live bytes a resident *share* leaves room for.
    pub fn live_in(&self, share: u64) -> u64 {
        share.saturating_sub(BASE_BYTES) * 100 / (100 + self.alloc_pct)
    }
}

/// Spend *share* (min(available / 4, 2 GiB), at least 256 MiB) on a pass of
/// *want* connections: the overhead first, the rest to the strips. Where the
/// rest is less than the strips' floor, the connections shrink — to the most
/// that leave the floor, at least [`MIN_CONNS`] — and where even that is not
/// enough the strips get their floor all the same: a pass never fails for
/// memory, its total says what it takes.
pub(super) fn plan(share: u64, want: usize, m: &PassMemory) -> MemPlan {
    let live = m.live_in(share);
    let floor = m.strips_min();
    let fits = |c: usize| live.saturating_sub(m.overhead(c)) >= floor;
    let conns = if fits(want) {
        want
    } else {
        let room = live.saturating_sub(m.overhead(0) + floor);
        let c = (room / m.per_conn.max(1)).min(want as u64) as usize;
        c.max(MIN_CONNS.min(want))
    };
    let strips = live.saturating_sub(m.overhead(conns)).max(floor);
    with_strips(conns, strips, m, !fits(conns))
}

/// The plan of a pass whose strips' budget is fixed (`DownloadTuning::
/// ram_budget`, tests): *conns* as given, the total what that costs.
pub(super) fn plan_fixed(strips: u64, conns: usize, m: &PassMemory) -> MemPlan {
    with_strips(conns, strips, m, false)
}

fn with_strips(conns: usize, strips: u64, m: &PassMemory, over: bool) -> MemPlan {
    let total = m.resident(m.overhead(conns) + strips.min(m.job));
    MemPlan { conns, strips, total, over }
}

/// What an assembly holds while it writes a tile-row (changelog 52): the
/// output rows whose centres fall in the tile-row, each `size_px × 2`
/// bytes plus [`ROW_HEAD_BYTES`], for the tile-row that has the most of
/// them, and every output tile's column table. *y0*..=*y1* are the job's
/// tile-rows at *zoom*.
pub(super) fn output_rows_bytes(specs: &[SubTileSpec], zoom: u32, y0: u32, y1: u32) -> u64 {
    let ny = (y1.saturating_sub(y0) + 1) as usize;
    let mut per_row = vec![0u64; ny];
    let mut tables = 0u64;
    for spec in specs {
        let sz = spec.size_px as usize;
        let pd = spec.resolution_m / 111_111.0;
        let row = sz as u64 * 2 + ROW_HEAD_BYTES;
        tables += sz as u64 * X_TABLE_BYTES;
        for y in 0..sz {
            // The tile-row an output row samples: the one holding its centre.
            let lat = spec.ul_lat - (y as f64 + 0.5) * pd;
            let ty = super::lat2ty(lat, zoom);
            if (y0..=y1).contains(&ty) {
                per_row[(ty - y0) as usize] += row;
            }
        }
    }
    per_row.into_iter().max().unwrap_or(0) + tables
}

/// MB (2^20 bytes), whole.
fn mb(b: u64) -> u64 {
    b >> 20
}

/// The breakdown the `[Download] RAM budget:` line appends (changelog 52;
/// informational): `(strips <s> MB + assembly <a> × <g> MB + <c> connections
/// <k> MB + cache writes <w> MB + decoding <d> MB, +<p> % allocator, +<b> MB
/// base[; buildings post-pass <x> MB | ; buildings post-pass not counted,
/// its tiles are not in yet])` — the cache term only with a cache that
/// writes, the post-pass term only with a building source (*post*).
pub(super) fn breakdown(m: &PassMemory, p: &MemPlan, post: PostPass) -> String {
    let conns = (p.conns as u64).min(m.fetched);
    let cache = if m.cache > 0 { format!(" + cache writes {} MB", mb(m.cache)) } else { String::new() };
    let post = match post {
        PostPass::None => String::new(),
        PostPass::Counted { .. } => format!("; buildings post-pass {} MB", mb(m.resident(post.live()))),
        PostPass::Later => "; buildings post-pass not counted, its tiles are not in yet".to_owned(),
    };
    format!(
        "(strips {} MB + assembly {} × {} MB + {} connections {} MB{} + decoding {} MB, +{} % allocator, +{} MB base{})",
        mb(p.strips.min(m.job)),
        m.asm,
        mb(m.grid + m.rows),
        conns,
        mb(conns * m.per_conn),
        cache,
        mb(m.decode),
        m.alloc_pct,
        mb(BASE_BYTES),
        post
    )
}

/// The line a pass prints when its share cannot hold *want* connections
/// beside the rest (changelog 52; informational, once, before the budget
/// line): `[Download] connections <want> → <conns>: <share> MB available
/// (…)`.
pub(super) fn shrink_line(want: usize, share: u64, m: &PassMemory, p: &MemPlan) -> String {
    let cache = if m.cache > 0 { format!(", cache writes ({} MB)", mb(m.cache)) } else { String::new() };
    let room = if p.over {
        format!("leave no room for connections: the pass runs {} and takes {} MB, over its share", p.conns, mb(p.total))
    } else {
        format!(
            "leave room for {} connections ({:.2} MB each, +{} % allocator)",
            p.conns,
            m.per_conn as f64 / (1u64 << 20) as f64,
            m.alloc_pct
        )
    };
    format!(
        "[Download] connections {want} → {}: {} MB available for this pass; the assembly ({} × {} MB), \
         decoding ({} MB){cache} and two tile-rows of strips ({} MB) {room}",
        p.conns,
        mb(share),
        m.asm,
        mb(m.grid + m.rows),
        mb(m.decode),
        mb(m.strips_min()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const MB: u64 = 1 << 20;

    /// A z15 pass 456 tiles wide (the measured one), 256 px tiles.
    fn wide() -> PassMemory {
        let tile = 256 * 256 * 4;
        PassMemory {
            tile,
            grid: 456 * tile,
            rows: 36 * MB,
            asm: 2,
            per_conn: CONN_BYTES + 256 * 256 * 2,
            fetched: 15_504,
            cache: 0,
            decode: 4 * 4 * tile,
            job: 15_504 * tile,
            alloc_pct: alloc_pct(false),
        }
    }

    #[test]
    fn the_overhead_is_assembly_connections_cache_and_decoding() {
        let m = wide();
        // 2 × (114 + 36) MB + 256 × 192 KiB + 4 MB.
        assert_eq!(m.overhead(256), 2 * (114 * MB + 36 * MB) + 256 * (192 << 10) + 4 * MB);
        // Connections beyond the tiles the network serves cost nothing.
        let few = PassMemory { fetched: 10, ..m };
        assert_eq!(few.overhead(256), few.overhead(10));
        assert_eq!(few.overhead(256) - few.overhead(0), 10 * few.per_conn);
        // A cache adds its writes.
        let cached = PassMemory { cache: CACHE_WRITE_BYTES, ..m };
        assert_eq!(cached.overhead(256) - m.overhead(256), 96 * MB);
    }

    #[test]
    fn the_total_is_the_live_bytes_with_the_allocators_share_and_the_base() {
        for pct in [12, 20, 40, 55] {
            let m = PassMemory { alloc_pct: pct, ..wide() };
            assert_eq!(m.resident(1000 * MB), 24 * MB + 1000 * MB + 1000 * MB * pct / 100);
            // live_in inverts resident (to the byte, rounding down).
            for share in [256 * MB, 1540 * MB, 2048 * MB] {
                let live = m.live_in(share);
                assert!(m.resident(live) <= share, "{share}");
                assert!(m.resident(live + 2) > share, "{share}");
            }
            assert_eq!(m.live_in(10 * MB), 0, "a share below the base leaves nothing");
        }
        // A cache churns bodies of every size through the heap: the
        // allocator keeps more, on every platform.
        assert!(alloc_pct(true) > alloc_pct(false));
    }

    #[test]
    fn a_roomy_share_keeps_the_connections_and_gives_the_rest_to_the_strips() {
        let m = wide();
        let p = plan(2048 * MB, 256, &m);
        assert_eq!(p.conns, 256);
        assert!(!p.over);
        assert_eq!(p.strips, m.live_in(2048 * MB) - m.overhead(256));
        // The total is the share (to a byte or two of rounding): the strips
        // take everything the overhead leaves.
        assert!(p.total <= 2048 * MB && p.total + 4 >= 2048 * MB, "{}", p.total);
    }

    #[test]
    fn a_small_job_costs_what_it_holds_not_the_share() {
        // 63 tiles: the strips can never hold more than the job.
        let tile = 256 * 256 * 4;
        let m = PassMemory { grid: 7 * tile, rows: MB, job: 63 * tile, fetched: 63, ..wide() };
        let p = plan(2048 * MB, 256, &m);
        assert_eq!(p.conns, 256);
        assert_eq!(p.total, m.resident(m.overhead(256) + 63 * tile));
        assert!(p.total < 100 * MB, "{}", p.total);
    }

    #[test]
    fn a_short_share_shrinks_the_connections_to_what_leaves_the_strips_their_floor() {
        let m = PassMemory { grid: 40 * 256 * 256 * 4, rows: 2 * MB, ..wide() };
        // 1024 connections at 192 KiB do not fit in 256 MiB beside the rest.
        let share = 256 * MB;
        let p = plan(share, 1024, &m);
        assert!(p.conns < 1024 && p.conns >= MIN_CONNS, "{p:?}");
        assert!(!p.over);
        let live = m.live_in(share);
        // The most connections that still leave the floor ...
        assert!(live - m.overhead(p.conns) >= m.strips_min(), "{p:?}");
        // ... and one more would not.
        assert!(live.saturating_sub(m.overhead(p.conns + 1)) < m.strips_min(), "{p:?}");
        assert!(p.total <= share, "{p:?}");
    }

    #[test]
    fn a_share_too_short_for_anything_runs_the_fewest_connections_and_says_it_is_over() {
        // The two assembly grids alone exceed 256 MiB: never a failure.
        let m = wide();
        let p = plan(256 * MB, 256, &m);
        assert_eq!(p.conns, MIN_CONNS);
        assert!(p.over);
        assert_eq!(p.strips, m.strips_min());
        assert_eq!(p.total, m.resident(m.overhead(MIN_CONNS) + m.strips_min()));
        assert!(p.total > 256 * MB);
        // A job asking for fewer than the floor keeps what it asked for.
        assert_eq!(plan(256 * MB, 8, &m).conns, 8);
    }

    #[test]
    fn a_fixed_strip_budget_keeps_its_connections() {
        let m = wide();
        let p = plan_fixed(16 * MB, 256, &m);
        assert_eq!((p.conns, p.strips, p.over), (256, 16 * MB, false));
        assert_eq!(p.total, m.resident(m.overhead(256) + 16 * MB));
    }

    fn spec(ul_lat: f64, ul_lon: f64, size_px: u32, resolution_m: f64) -> SubTileSpec {
        SubTileSpec { filename: "t.abt".into(), ul_lat, ul_lon, size_px, resolution_m }
    }

    #[test]
    fn the_output_rows_are_those_of_the_fullest_tile_row() {
        use super::super::{lat2ty, ty2lat};
        let z = 12;
        let (y0, y1) = (lat2ty(47.0, z), lat2ty(46.5, z));
        // Two specs side by side over the same rows, one 100 px, one 50 px.
        let a = spec(47.0, 8.0, 100, 500.0);
        let b = spec(47.0, 8.5, 50, 1000.0);
        let got = output_rows_bytes(&[a.clone(), b.clone()], z, y0, y1);
        // Count by brute force: per tile-row, the rows whose centre it holds.
        let mut want = vec![0u64; (y1 - y0 + 1) as usize];
        for s in [&a, &b] {
            let pd = s.resolution_m / 111_111.0;
            for y in 0..s.size_px {
                let lat = s.ul_lat - (y as f64 + 0.5) * pd;
                for ty in y0..=y1 {
                    if lat <= ty2lat(ty, z) && lat > ty2lat(ty + 1, z) {
                        want[(ty - y0) as usize] += s.size_px as u64 * 2 + 48;
                    }
                }
            }
        }
        let tables = (100 + 50) * 8;
        assert_eq!(got, want.iter().max().unwrap() + tables);
        assert!(got > tables);
    }

    #[test]
    fn the_buildings_post_pass_counts_when_its_tiles_are_there_and_the_budget_is_the_larger_phase() {
        let m = wide();
        let p = plan(2048 * MB, 256, &m);
        // No building source, or one whose tiles come later: the terrain's.
        assert_eq!(with_post_pass(p, &m, PostPass::None), p);
        assert_eq!(with_post_pass(p, &m, PostPass::Later), p);
        // A small set: the terrain is the larger phase.
        let small = PostPass::Counted { set: 100 * MB, tile: 62 * MB };
        assert_eq!(small.live(), 140 * MB + 62 * MB);
        assert_eq!(with_post_pass(p, &m, small), p);
        // A large one: the post-pass is — its set at the decode's peak, one
        // output tile, the allocator's share and the base.
        let large = PostPass::Counted { set: 2000 * MB, tile: 62 * MB };
        let q = with_post_pass(p, &m, large);
        assert_eq!(q.total, m.resident(2800 * MB + 62 * MB));
        assert_eq!((q.conns, q.strips, q.over), (p.conns, p.strips, p.over), "the terrain's plan is unchanged");
        let b = breakdown(&m, &q, large);
        assert!(b.ends_with(&format!(" MB base; buildings post-pass {} MB)", q.total >> 20)), "{b}");
        let b = breakdown(&m, &p, PostPass::Later);
        assert!(b.ends_with(" MB base; buildings post-pass not counted, its tiles are not in yet)"), "{b}");
        assert!(breakdown(&m, &p, PostPass::None).ends_with(" MB base)"));
    }

    #[test]
    fn the_lines_name_the_figures() {
        let m = PassMemory { cache: CACHE_WRITE_BYTES, alloc_pct: alloc_pct(true), ..wide() };
        let p = plan(2048 * MB, 256, &m);
        let b = breakdown(&m, &p, PostPass::None);
        assert!(b.starts_with("(strips ") && b.ends_with(" MB base)"), "{b}");
        assert!(b.contains(" + assembly 2 × 150 MB + 256 connections 48 MB + cache writes 96 MB + decoding 4 MB, +"), "{b}");
        let short = plan(256 * MB, 256, &m);
        let line = shrink_line(256, 256 * MB, &m, &short);
        assert!(line.starts_with("[Download] connections 256 → 16: 256 MB available for this pass; the assembly (2 × 150 MB), "), "{line}");
        let over = format!(" leave no room for connections: the pass runs 16 and takes {} MB, over its share", short.total >> 20);
        assert!(line.ends_with(&over), "{line}");
        let fits = PassMemory { grid: 40 * 256 * 256 * 4, rows: 2 * MB, ..m };
        let p = plan(512 * MB, 1024, &fits);
        let line = shrink_line(1024, 512 * MB, &fits, &p);
        assert!(line.starts_with(&format!("[Download] connections 1024 → {}: 512 MB available for this pass; ", p.conns)), "{line}");
        assert!(line.ends_with(&format!(" leave room for {} connections (0.19 MB each, +{} % allocator)", p.conns, alloc_pct(true))), "{line}");
    }
}
