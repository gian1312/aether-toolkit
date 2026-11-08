// rust/aether_converter/src/main.rs
mod buildings;
// Reached only through `mvt::apply_buildings_to_abt_tiles`; the CLI has no
// canopy subcommand, so the rest of the module is dead code in this binary.
#[allow(dead_code)]
mod canopy;
mod diag;
mod download;
mod ingest;
mod mvt;
mod plan;
mod probe;
mod proc_usage;
mod tiff_io;

// The native Windows binary allocates through mimalloc (CONTRACT changelog
// 37): the system heap returns large blocks to the OS on free, so a batch
// paid fresh page faults for every tile buffer and window. Linux and macOS
// keep their system allocators; the library and the wasm crate never set one.
#[cfg(all(windows, not(target_arch = "wasm32"), feature = "native"))]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::fs;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
#[cfg(feature = "native")]
use rayon::prelude::*;
#[cfg(feature = "native")]
use sysinfo::System;

#[derive(Parser)]
#[command(name = "aether_converter", version)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    Convert {
        #[arg(short, long)]
        input: PathBuf,
        #[arg(short, long)]
        output: PathBuf,
    },
    Ingest {
        #[arg(short, long)]
        job_file: PathBuf,
    },
    /// Download XYZ terrain tiles in parallel and produce a WGS84 GeoTIFF.
    Download {
        #[arg(short, long)]
        job_file: PathBuf,
    },
    /// Enumerate the .abt tiles a bbox + resolution set produces, without
    /// converting anything. Prints one JSON document (schema "aether-plan/1")
    /// on stdout; consumers cross-check their own tile enumeration against it.
    Plan {
        // allow_negative_numbers: the plugin passes coordinates as separate
        // tokens (`--west -70.3`), and clap would otherwise reject the leading
        // dash as an unknown flag — southern/western hemispheres must work.
        #[arg(long, allow_negative_numbers = true)]
        south: f64,
        #[arg(long, allow_negative_numbers = true)]
        north: f64,
        #[arg(long, allow_negative_numbers = true)]
        west: f64,
        #[arg(long, allow_negative_numbers = true)]
        east: f64,
        /// Comma-separated integer resolutions in metres, e.g. 30,90
        #[arg(long, required = true, value_delimiter = ',')]
        resolutions: Vec<u32>,
    },
    /// Read the raw header facts of many GeoTIFFs without decoding pixels:
    /// one JSON line per listed file on stdout (schema "aether-probe/1"), in
    /// list order, then a `[Probe]` summary line on stderr. Header facts
    /// only — no reprojection, no footprint.
    Probe {
        /// File listing one path per line (UTF-8).
        #[arg(long, conflicts_with = "stdin", required_unless_present = "stdin")]
        list: Option<PathBuf>,
        /// Read the path list from stdin instead of --list.
        #[arg(long)]
        stdin: bool,
        /// Parallel workers [default: min(64, 4 x CPUs)].
        #[arg(long)]
        jobs: Option<usize>,
    },
}

/// The batch's per-tile accounting: coverage, failures and the progress
/// count, fed once per tile with its final result (on whichever thread
/// finished it — a compute thread for a sampling error, a writer thread for
/// a written or failed tile).
struct BatchTally {
    total: usize,
    progress: AtomicUsize,
    failures: AtomicUsize,
    // Coverage is judged over the WHOLE batch, never per tile: the edge
    // tiles of any area legitimately fall outside the sources, and failing
    // on those would refuse every ordinary run.
    covered: AtomicU64,
}

impl BatchTally {
    fn tile_done(&self, tile_name: &str, result: anyhow::Result<ingest::TileStats>) {
        let mut tile_line = None;
        match result {
            Ok(stats) => {
                self.covered.fetch_add(stats.covered_px, Ordering::Relaxed);
                // Parsed by the Waveshed plugin (CONTRACT §1.2, changelog 33):
                // per-tile coverage, so a tile the sources never reached is
                // known as void without opening it. Then, as a line of its
                // own (an older plugin's pattern ends the covered line at
                // `covered=<a>/<b>`), the void samples the written file holds
                // — at or below -5000 half-metres, after `void_fill_m` and the
                // building burns (changelog 46).
                tile_line = Some((
                    format!("[Tile] {tile_name} covered={}/{}", stats.covered_px, stats.total_px),
                    format!("[Tile] {tile_name} voids={}", stats.void_px),
                ));
            }
            Err(e) => {
                // `{:#}` prints the whole cause chain ("loading source …:
                // Opening …: No such file …"); `{}` printed only the outermost
                // context, which named the file but never said what was wrong.
                println!("[Error] Failed to process tile: {:#}", e);
                self.failures.fetch_add(1, Ordering::Relaxed);
            }
        }

        {
            // One stdout lock for the tile line and the progress line, flushed
            // together: the [Tile] line always precedes the progress count
            // that includes it (the count is taken under the lock, so no
            // other tile's progress line can overtake this tile's line).
            use std::io::Write;
            let mut out = std::io::stdout().lock();
            let curr = self.progress.fetch_add(1, Ordering::Relaxed) + 1;
            if let Some((covered, voids)) = &tile_line {
                let _ = writeln!(out, "{}", covered);
                let _ = writeln!(out, "{}", voids);
                // The voids line is kept in `AETHER_DIAG_FILE` too, in
                // stdout's order (changelog 48); the covered line is not.
                diag::keep(voids);
            }
            if curr % 10 == 0 || curr == self.total {
                let _ = writeln!(out, "[Rust] Progress: {}/{}", curr, self.total);
            }
            let _ = out.flush();
        }
    }
}

/// A batch prints `[Rust] Decoding building tiles: N/M` every this many tiles
/// of a `buildings_pbf_dir`, and once after the last (CONTRACT §9a,
/// changelog 44).
const PBF_PROGRESS_EVERY: usize = 1000;

/// The decode-progress line for *done* of *total* building tiles, when one is
/// due. Deliberately not a `Progress:` line: the plugin reads those as output
/// tiles converted.
fn pbf_decode_progress_line(done: usize, total: usize) -> Option<String> {
    (done % PBF_PROGRESS_EVERY == 0 || done == total)
        .then(|| format!("[Rust] Decoding building tiles: {done}/{total}"))
}

/// The batch's decode-progress callback: each due line on stdout, the stream
/// of the `[Rust] Progress:` lines, flushed at once.
fn print_pbf_decode_progress(done: usize, total: usize) {
    if let Some(line) = pbf_decode_progress_line(done, total) {
        use std::io::Write;
        let mut out = std::io::stdout().lock();
        let _ = writeln!(out, "{line}");
        let _ = out.flush();
    }
}

/// The line after a directory's decode: its tile files (decoded or skipped),
/// the buildings decoded from them (before the duplicate filter, as the
/// `[Info] buildings_pbf_dir` line counts them), the decode's wall time and
/// — since changelog 52 — the memory the set holds for the rest of the run
/// ([`buildings::PbfBuildingSet::heap_bytes`], MB with one decimal).
fn pbf_decoded_line(set: &buildings::PbfBuildingSet, secs: f64) -> String {
    format!(
        "[Rust] Building tiles decoded: {} tile(s), {} building(s) in {:.1} s, {} MB",
        set.tiles_found,
        set.decoded,
        secs,
        proc_usage::mb(set.heap_bytes())
    )
}

/// A batch's worker threads — and, with building sets, the tiles it samples
/// at once (changelog 52): *usable* (80 % of the available memory, less the
/// shared chunk cache) less the building sets the batch holds, over the
/// per-tile estimate; 1 at least, *max* at most.
fn batch_threads(usable: u64, building_sets: u64, per_tile: u64, max: usize) -> usize {
    ((usable.saturating_sub(building_sets) / per_tile.max(1)) as usize).clamp(1, max.max(1))
}

/// The building sets a batch will hold, as known at launch (changelog 52).
struct BuildingEstimate {
    /// Each building source there at launch — a directory without a marker
    /// or with its marker in place, a list file that exists — and its
    /// estimate: its tile files' bytes on disk ×
    /// [`buildings::DECODED_PER_DISK_BYTE`]. It is decoded before the first
    /// tile.
    present: Vec<(buildings::PbfSource, u64)>,
    /// Sources whose marker or list is not there yet: decoded while the
    /// tiles run, counted once decoded ([`TileCap::settle`]).
    later: usize,
}

impl BuildingEstimate {
    fn of(
        jobs: &[ingest::IngestJob],
        markers: &HashMap<buildings::PbfSource, Option<ingest::buildings_ready::Marker>>,
    ) -> Self {
        let mut est = BuildingEstimate { present: Vec::new(), later: 0 };
        let mut seen = std::collections::HashSet::new();
        for source in jobs.iter().filter_map(|j| j.pbf_source()) {
            if !seen.insert(source.clone()) {
                continue;
            }
            let there = match markers.get(&source) {
                Some(Some((signal, _))) => download::buildings_marker_present(signal.path()),
                _ => true,
            };
            if there {
                let bytes = buildings::pbf_source_disk_bytes(&source).map_or(0, |(_, b)| b);
                est.present.push((source, bytes.saturating_mul(buildings::DECODED_PER_DISK_BYTE)));
            } else {
                est.later += 1;
            }
        }
        est
    }

    /// The estimate of the sets there at launch, in bytes.
    fn bytes(&self) -> u64 {
        self.present.iter().map(|(_, b)| b).sum()
    }

    /// Whether the batch has a building source at all.
    fn any(&self) -> bool {
        !self.present.is_empty() || self.later > 0
    }

    /// What the `[Rust] Rayon Pool:` line appends after `~<m> MB per tile`:
    /// nothing without a building source; the estimate; the sources decoded
    /// later, which it cannot estimate yet.
    fn pool_line_suffix(&self) -> String {
        let mb = self.bytes() >> 20;
        match (self.present.is_empty(), self.later) {
            (true, 0) => String::new(),
            (false, 0) => format!(", building set ~{mb} MB"),
            (false, k) => format!(", building set ~{mb} MB + {k} source(s) decoded later"),
            (true, k) => format!(", building set: {k} source(s) decoded later"),
        }
    }
}

/// How many tiles a batch with building sets samples at once (changelog 52):
/// the pool's size at launch, lowered for the rest of the run once a decoded
/// set — its own size replacing the launch estimate, or a source decoded
/// later, which the pool's sizing did not count — leaves room for fewer.
struct TileCap {
    usable: u64,
    per_tile: u64,
    threads: usize,
    available: u64,
    /// Each building source's bytes: its launch estimate until it is decoded,
    /// then [`buildings::PbfBuildingSet::heap_bytes`].
    sets: Mutex<HashMap<buildings::PbfSource, u64>>,
    cap: AtomicUsize,
}

impl TileCap {
    fn new(usable: u64, per_tile: u64, threads: usize, available: u64, est: &BuildingEstimate) -> Self {
        TileCap {
            usable,
            per_tile,
            threads,
            available,
            sets: Mutex::new(est.present.iter().cloned().collect()),
            cap: AtomicUsize::new(threads),
        }
    }

    /// Tiles that may be sampled at once now.
    fn get(&self) -> usize {
        self.cap.load(Ordering::Acquire)
    }

    /// The building sets counted so far, in bytes.
    fn held_sets(&self) -> u64 {
        self.sets.lock().unwrap_or_else(std::sync::PoisonError::into_inner).values().sum()
    }

    /// *source* is decoded and holds *bytes*: count them in place of its
    /// estimate. Where the sets now leave room for fewer tiles than run at
    /// once, fewer start from here on, said in one line.
    fn settle(&self, source: &buildings::PbfSource, bytes: u64) {
        let sets: u64 = {
            let mut s = self.sets.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            s.insert(source.clone(), bytes);
            s.values().sum()
        };
        let want = batch_threads(self.usable, sets, self.per_tile, self.threads);
        if want < self.cap.fetch_min(want, Ordering::AcqRel) {
            use std::io::Write;
            let mut out = std::io::stdout().lock();
            let _ = writeln!(out, "{}", tiles_at_once_line(want, self.threads, sets, self.per_tile, self.available));
            let _ = out.flush();
        }
    }
}

/// The line of a batch whose decoded building sets lower the tiles it
/// samples at once (changelog 52; informational, not parsed).
fn tiles_at_once_line(tiles: usize, threads: usize, sets: u64, per_tile: u64, available: u64) -> String {
    format!(
        "[Rust] Tiles at once: {tiles} of {threads} threads from here on (building sets {} MB; ~{} MB per tile, {} GB available at start)",
        proc_usage::mb(sets),
        per_tile >> 20,
        available >> 30
    )
}

/// Run *jobs* on *pool*, at most `cap` at once (changelog 52): *lanes* threads
/// of their own each take the next job, in order, and run it on the pool —
/// a lane at or past the cap stops before its next job. The lanes wait
/// outside the pool, so no pool worker ever waits for another tile. A
/// panicking tile ends its lane; the others finish the jobs, then the first
/// panic goes on.
#[cfg(feature = "native")]
fn run_in_lanes<F>(pool: &rayon::ThreadPool, lanes: usize, cap: &TileCap, jobs: Vec<ingest::IngestJob>, run_tile: &F)
where
    F: Fn(ingest::IngestJob) + Sync,
{
    let queue = Mutex::new(jobs.into_iter());
    let next = || queue.lock().unwrap_or_else(std::sync::PoisonError::into_inner).next();
    let lane = |i: usize| {
        while i < cap.get() {
            let Some(job) = next() else { return };
            pool.install(|| run_tile(job));
        }
    };
    let panicked = std::thread::scope(|s| {
        let handles: Vec<_> = (0..lanes.max(1))
            .map_while(|i| {
                std::thread::Builder::new()
                    .name(format!("aether-lane-{i}"))
                    .spawn_scoped(s, move || lane(i))
                    .ok()
            })
            .collect();
        if handles.is_empty() {
            // No thread could be started: one lane, here.
            lane(0);
        }
        let mut first = None;
        for h in handles {
            if let Err(p) = h.join() {
                first.get_or_insert(p);
            }
        }
        first
    });
    if let Some(p) = panicked {
        std::panic::resume_unwind(p);
    }
}

/// The start line of a `buildings_pbf_list` decode (CONTRACT §9a,
/// changelog 51): `<n>` the distinct names the list names, a plain integer;
/// `<dir>` the job's `buildings_pbf_dir` exactly as given, between quotes —
/// not the `Debug` form the directory's start line prints, which doubles a
/// Windows path's backslashes.
fn pbf_list_decode_line(n: usize, dir: &std::path::Path) -> String {
    format!("[Rust] Decoding {n} listed building tiles from \"{}\" (once for the run)...", dir.display())
}

/// Decode the building source *source* for a batch, with its lines
/// (CONTRACT §9a, changelog 44): the start line, a progress line every
/// [`PBF_PROGRESS_EVERY`] tiles, the summary line. A directory starts with
/// `[Rust] Decoding building tiles in "<dir>" (once for the run)...`; a
/// `buildings_pbf_list` (changelog 51) is read first and starts with
/// `[Rust] Decoding <n> listed building tiles from "<dir>" (once for the
/// run)...`, `<n>` the distinct names it lists — only those are read. On
/// *pool* — the batch's at launch; a pool of its size beside it when the
/// source is decoded late (`buildings_pbf_ready`, changelog 48, or a list
/// that was missing at launch) — or, without one, on rayon's global pool.
#[cfg(feature = "native")]
fn decode_building_source(
    pool: Option<&rayon::ThreadPool>,
    source: &buildings::PbfSource,
) -> anyhow::Result<buildings::PbfBuildingSet> {
    use std::io::Write;
    let (set, t0) = match source {
        buildings::PbfSource::Dir(dir) => {
            println!("[Rust] Decoding building tiles in {:?} (once for the run)...", dir);
            let t0 = std::time::Instant::now();
            let load = || buildings::load_pbf_building_dir_with_progress(dir, &print_pbf_decode_progress);
            let set = match pool {
                Some(pool) => pool.install(load),
                None => load(),
            }?;
            (set, t0)
        }
        buildings::PbfSource::List { list, dir } => {
            // The whole load is timed: the list's read too.
            let t0 = std::time::Instant::now();
            let list = buildings::read_pbf_list(list)?;
            println!("{}", pbf_list_decode_line(list.names().len(), dir));
            let load = || buildings::load_pbf_building_list_with_progress(dir, &list, &print_pbf_decode_progress);
            let set = match pool {
                Some(pool) => pool.install(load),
                None => load(),
            }?;
            (set, t0)
        }
    };
    println!("{}", pbf_decoded_line(&set, t0.elapsed().as_secs_f64()));
    let _ = std::io::stdout().flush();
    Ok(set)
}

fn main() -> anyhow::Result<()> {
    // Force line-buffered stdout so progress reaches the parent process
    // immediately when piped (default is fully-buffered when not a TTY).
    use std::io::Write;
    let _ = std::io::stdout().flush(); // touch stdout to init

    // Strictly ignore environment variables for noisy modules.
    let mut builder = env_logger::Builder::new();

    builder.filter(None, log::LevelFilter::Info); // Default to Info
    builder.filter_module("flatgeobuf", log::LevelFilter::Off); // Silence FGB
    builder.filter_module("wgpu", log::LevelFilter::Error);     // Silence GPU
    builder.filter_module("tiff", log::LevelFilter::Warn);

    builder.init();

    let args = Cli::parse();

    // `[Rust] run: …` diagnostic line (CONTRACT changelog 31, not parsed):
    // printed when this guard drops — at the very end of `main`, after the
    // batch result, on every exit path. Ingest only; other subcommands' stdout
    // is JSON or silent.
    let _run_stats = matches!(args.command, Commands::Ingest { .. }).then(ingest::RunStats::start);

    match args.command {
        Commands::Convert { .. } => {
            println!("Legacy convert command is deprecated. Use 'ingest'.");
            Ok(())
        },
        Commands::Ingest { job_file } => {
            let content = fs::read_to_string(&job_file)
                .map_err(|e| anyhow::anyhow!("reading job file {:?}: {e}", job_file))?;

            // Caller-supplied whole images (none here): every source streams
            // through the byte-budgeted chunk cache instead.
            let texture_cache: ingest::ImageCache = Arc::new(Mutex::new(HashMap::new()));

            if let Ok(job) = serde_json::from_str::<ingest::IngestJob>(&content) {
                println!("[Rust] Processing single tile: {:?}", job.output_path);
                let out_path = job.output_path.clone();
                let stats = ingest::process_tile_with_cache(job, texture_cache)?;
                // An all-void tile is not terrain, and exiting 0 on one hands
                // the caller a file it will read as ground 4999.5 m below sea
                // level (or, with void_fill_m, a flat plain at the fill). The
                // usual cause is a source that does not cover this tile at all
                // — a wrong CRS, or an area outside the dataset.
                if stats.covered_px == 0 {
                    anyhow::bail!(
                        "no source covered any pixel of {:?}: all {} samples are void, so \
                         the tile holds no terrain. Check that the sources overlap this \
                         tile's area and that their CRS is right.",
                        out_path,
                        stats.total_px
                    );
                }
            } else if let Ok(jobs) = serde_json::from_str::<Vec<ingest::IngestJob>>(&content) {

                // Validate every job up front, so a bad job (ambiguous
                // sources, etc.) fails before any tile work starts.
                for job in &jobs {
                    job.effective_sources()?;
                }
                // The building-tile hand-over (`buildings_pbf_ready`,
                // changelog 48; `buildings_pbf_list`, changelog 51): every
                // job's fields, and one hand-over per directory or list,
                // checked before anything is read.
                let markers = ingest::buildings_ready::batch_markers(&jobs)?;

                // --- THREAD COUNT FROM A PER-JOB MEMORY ESTIMATE ---
                // Each tile streams its sources in row bands bounded by the
                // band budget, and all tiles share ONE byte-budgeted chunk
                // cache, so a tile's peak no longer grows with the size or
                // number of its sources. Size the pool from that estimate and
                // the memory actually AVAILABLE (not installed): the old
                // `total RAM / 1.5 GB` rule ignored both what else was running
                // and what a tile really holds (580 whole 0.5 m files per
                // tile is 4.6 GB, far past 1.5 GB).
                let opts = ingest::IngestOptions {
                    band_budget_bytes: ingest::DEFAULT_BAND_BUDGET_BYTES,
                    chunk_cache: Some(Arc::new(ingest::ChunkCache::new(ingest::DEFAULT_CHUNK_CACHE_BYTES))),
                };
                let available_cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
                let mut sys = System::new();
                sys.refresh_memory();
                let total_ram = sys.total_memory();
                // Some platforms report no "available" figure; half the total
                // is a conservative stand-in. A test may impose the figure
                // (changelog 52, `AETHER_TEST_AVAILABLE_MB`).
                let available_ram = match proc_usage::test_available_override() {
                    Some(a) => a,
                    None => match sys.available_memory() {
                        0 => total_ram / 2,
                        a => a,
                    },
                };
                let per_job = jobs
                    .iter()
                    .map(|j| ingest::estimate_job_bytes(j, &opts))
                    .max()
                    .unwrap_or(1)
                    .max(1);
                let usable = (available_ram as f64 * 0.8) as u64;
                let usable = usable.saturating_sub(ingest::DEFAULT_CHUNK_CACHE_BYTES as u64);
                // The building sets the batch holds for the whole run
                // (changelog 52): each source there at launch is estimated
                // from its tile files' bytes on disk — it is decoded before
                // the first tile — and a source whose marker or list is not
                // there yet counts once it is decoded (`TileCap`).
                let buildings = BuildingEstimate::of(&jobs, &markers);
                let safe_threads = batch_threads(usable, buildings.bytes(), per_job, available_cores);
                // Reported by the `[Rust] run:` line at exit (changelog 31).
                ingest::note_batch_threads(safe_threads);

                println!("[Rust] Batch processing {} tiles...", jobs.len());
                println!(
                    "[Rust] Rayon Pool: {} threads (System RAM: {} GB, {} GB available, ~{} MB per tile{})",
                    safe_threads,
                    total_ram / 1024 / 1024 / 1024,
                    available_ram / 1024 / 1024 / 1024,
                    per_job / 1024 / 1024,
                    buildings.pool_line_suffix()
                );
                {
                    use std::io::Write;
                    let _ = std::io::stdout().flush();
                }

                let pool = rayon::ThreadPoolBuilder::new().num_threads(safe_threads).build().unwrap();
                // How many tiles are sampled at once: the pool's size, lowered
                // when a decoded building set turns out larger than the pool's
                // sizing counted (changelog 52).
                let cap = TileCap::new(usable, per_job, safe_threads, available_ram, &buildings);

                // --- PBF DECODE HOIST ---
                // A `buildings_pbf_dir` decodes to the same building set for
                // every output tile, so scan and decode each directory once for
                // the whole run. Doing it inside the loop below re-read and
                // re-decoded the entire directory once per .abt.
                //
                // The tiles are read and decoded in parallel on the batch's
                // pool (changelog 44: 20,808 tiles took 6 min one after the
                // other while a download wrote to the same HDD), in name order
                // as far as the result goes, with a progress line every
                // PBF_PROGRESS_EVERY tiles — nothing at all was printed during
                // the decode, so the plugin's progress bar froze.
                //
                // A directory whose `buildings_pbf_ready` marker is missing at
                // this first look (changelog 48) is not decoded here: it is
                // watched while the tiles start, and decoded when the marker
                // appears. A marker that is there decodes now, as without one.
                // A `buildings_pbf_list` (changelog 51) is a source of its
                // own — the listed tiles of its directory, never the
                // directory's listing — and its file is its marker.
                let mut pbf_sets: HashMap<buildings::PbfSource, buildings::PbfBuildingSet> = HashMap::new();
                let mut watched: Vec<ingest::buildings_ready::Watched> = Vec::new();
                for job in &jobs {
                    if let Some(source) = job.pbf_source() {
                        if pbf_sets.contains_key(&source) || watched.iter().any(|w| w.source == source) {
                            continue;
                        }
                        if let Some(Some((marker, wait))) = markers.get(&source) {
                            let first_look = std::time::Instant::now();
                            if !download::buildings_marker_present(marker.path()) {
                                watched.push(ingest::buildings_ready::Watched {
                                    source,
                                    marker: marker.clone(),
                                    wait: *wait,
                                    first_look,
                                });
                                continue;
                            }
                        }
                        let set = decode_building_source(Some(&pool), &source)?;
                        cap.settle(&source, set.heap_bytes());
                        pbf_sets.insert(source, set);
                    }
                }
                // The tiles finished before their building set is decoded are
                // held, up to this budget (changelog 48) — by default from the
                // memory left once the sets decoded at launch are counted too
                // (changelog 52).
                let handover = (!watched.is_empty()).then(|| {
                    let sources: Vec<buildings::PbfSource> = watched.iter().map(|w| w.source.clone()).collect();
                    let available = available_ram.saturating_sub(cap.held_sets());
                    let budget = ingest::buildings_ready::hold_budget(&jobs, &sources, available, safe_threads, per_job);
                    ingest::buildings_ready::BuildingsHandOver::new(budget, safe_threads, watched)
                });

                let total = jobs.len();
                let tally = Arc::new(BatchTally {
                    total,
                    progress: AtomicUsize::new(0),
                    failures: AtomicUsize::new(0),
                    covered: AtomicU64::new(0),
                });

                // Finished tiles are written, synced and renamed on dedicated
                // writer threads (CONTRACT changelog 37), so a compute thread
                // starts its next tile instead of waiting on the file system.
                // A tile is reported — `[Tile]`, `[Error]`, progress — once
                // its file is committed (or its write failed). Written tiles'
                // buffers are reused by the next tiles (one per thread).
                let writer = ingest::TileWriter::new((safe_threads / 2).clamp(1, 4), safe_threads);

                // A watched source is decoded on a pool of the batch's
                // size beside the batch's own, whose threads may be waiting
                // for that very set (changelog 48).
                // Its set then counts against the tiles sampled at once from
                // there on (changelog 52).
                let decode_late = |source: &buildings::PbfSource| -> anyhow::Result<buildings::PbfBuildingSet> {
                    let set = match rayon::ThreadPoolBuilder::new()
                        .num_threads(safe_threads)
                        .thread_name(|i| format!("aether-pbf-{i}"))
                        .build()
                    {
                        Ok(late_pool) => decode_building_source(Some(&late_pool), source),
                        Err(_) => decode_building_source(None, source),
                    }?;
                    cap.settle(source, set.heap_bytes());
                    Ok(set)
                };
                let handed_over: anyhow::Result<()> = std::thread::scope(|s| {
                    // One watcher per watched directory, from the start.
                    let (decode_late, writer_ref) = (&decode_late, &writer);
                    let watchers: Vec<_> = handover
                        .iter()
                        .flat_map(|h| (0..h.gates()).map(move |g| (h, g)))
                        .map(|(h, g)| s.spawn(move || h.watch(g, decode_late, writer_ref)))
                        .collect();

                    // One tile, on a worker of the constrained pool.
                    let run_tile = |job: ingest::IngestJob| {
                        // A panicking tile must not leave the workers
                        // waiting for building tiles (changelog 48).
                        let _unwinding = handover.as_ref().map(|h| h.cancel_on_panic());
                        // The output's file name, for the per-tile line below
                        // (the job moves into process_tile_batched).
                        let tile_name = job
                            .output_path
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_else(|| job.output_path.display().to_string());
                        let tally = tally.clone();
                        let done: ingest::TileDone = Box::new(move |result| tally.tile_done(&tile_name, result));
                        let source = job.pbf_source();
                        let gate = handover
                            .as_ref()
                            .and_then(|h| Some((h, h.gate_of(source.as_ref()?)?)));
                        match gate {
                            Some((h, g)) => h.process(g, job, texture_cache.clone(), &opts, &writer, done),
                            None => {
                                // A failed hand-over fails the run: no
                                // further tile starts.
                                if handover.as_ref().is_some_and(|h| h.aborted()) {
                                    return;
                                }
                                let pbf = source.as_ref().and_then(|s| pbf_sets.get(s));
                                ingest::process_tile_batched(job, texture_cache.clone(), pbf, &opts, &writer, done);
                            }
                        }
                    };
                    if buildings.any() {
                        // With building sets, at most `cap` tiles at once
                        // (changelog 52): one lane per worker takes the jobs
                        // in order and runs each on the pool; a lane at or
                        // past the cap stops before its next tile. The lanes
                        // are threads of their own, outside the pool, so no
                        // worker ever waits for another tile to end.
                        run_in_lanes(&pool, safe_threads, &cap, jobs, &run_tile);
                    } else {
                        // Run all jobs in parallel using the constrained pool
                        pool.install(|| jobs.into_par_iter().for_each(&run_tile));
                    }
                    // The watchers end once their tiles are burnt and handed
                    // to the writers, or the run failed.
                    let mut first_error = None;
                    for w in watchers {
                        match w.join() {
                            Ok(Ok(())) => {}
                            Ok(Err(e)) => {
                                first_error.get_or_insert(e);
                            }
                            Err(panic) => std::panic::resume_unwind(panic),
                        }
                    }
                    first_error.map_or(Ok(()), Err)
                });
                // Every tile is reported before the batch result is decided.
                writer.finish();
                // The shared cache keeps up to a few hundred source files
                // open (CONTRACT changelog 37). The process exits next, which
                // closes them all at once; dropping the cache would close them
                // one by one here — a file-system round trip each on a
                // network share or a 9p mount.
                std::mem::forget(opts);
                // A marker that never came, or a directory that did not
                // decode: no tile of that directory was written (changelog 48).
                handed_over?;

                // Fail loudly if any tile could not be produced, so callers
                // don't treat a partial/terrain-less batch as success.
                let n_failed = tally.failures.load(Ordering::Relaxed);
                if n_failed > 0 {
                    anyhow::bail!("{} of {} tiles failed to convert", n_failed, total);
                }
                if tally.covered.load(Ordering::Relaxed) == 0 {
                    // The no-data contract (CONTRACT changelog item 18,
                    // 2026-08-31): an area the sources do not reach is
                    // no-data, not an error — the same ruling the download
                    // path applies to HTTP 404 (item 17). A run over ground
                    // outside a bounded DEM must degrade to void tiles (or
                    // the job's void_fill_m, e.g. Waveshed Site Analysis's
                    // 0 m sea level), loudly — a wrong-CRS mistake produces
                    // the identical shape and this line is what makes either
                    // cause visible instead of silent.
                    eprintln!(
                        "[Warn] NO-DATA batch: {} tile(s) were written and not one \
                         pixel of any of them came from a source — the requested \
                         area is outside every source's coverage (or a source CRS \
                         is wrong). Voids (or void_fill_m) everywhere.",
                        total
                    );
                }
            } else {
                // Neither a single job nor a batch parsed. The old code fell
                // through SILENTLY here and exited 0 having done nothing —
                // fail loudly instead, with the parse error for the shape the
                // file appears to be.
                let err = if content.trim_start().starts_with('[') {
                    serde_json::from_str::<Vec<ingest::IngestJob>>(&content).unwrap_err()
                } else {
                    serde_json::from_str::<ingest::IngestJob>(&content).unwrap_err()
                };
                anyhow::bail!(
                    "ingest job file {:?} is not a valid job object or job array: {err}",
                    job_file
                );
            }
            Ok(())
        },
        Commands::Download { job_file } => {
            download::run_download(&job_file)
        },
        Commands::Plan { south, north, west, east, resolutions } => {
            let plan = plan::build_plan(south, north, west, east, &resolutions)?;
            println!("{}", serde_json::to_string(&plan)?);
            Ok(())
        },
        Commands::Probe { list, stdin: _, jobs } => probe::run(list.as_deref(), jobs),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_accepts_negative_coordinates_as_separate_tokens() {
        // The plugin invokes `--west -70.3` as two argv entries; without
        // allow_negative_numbers clap fails with "unexpected argument '-7'".
        let cli = Cli::try_parse_from([
            "aether_converter", "plan",
            "--south", "-34.7", "--north", "-33.9",
            "--west", "-70.9", "--east", "-70.3",
            "--resolutions", "30,90",
        ])
        .expect("a southern/western-hemisphere bbox must parse");
        match cli.command {
            Commands::Plan { south, north, west, east, resolutions } => {
                assert_eq!((south, north, west, east), (-34.7, -33.9, -70.9, -70.3));
                assert_eq!(resolutions, vec![30, 90]);
            }
            _ => panic!("expected the plan subcommand"),
        }
    }

    #[test]
    fn building_decode_progress_comes_every_thousand_tiles_and_at_the_end() {
        let due = |total: usize| -> Vec<String> {
            (1..=total).filter_map(|done| pbf_decode_progress_line(done, total)).collect()
        };
        assert_eq!(
            due(2500),
            [
                "[Rust] Decoding building tiles: 1000/2500",
                "[Rust] Decoding building tiles: 2000/2500",
                "[Rust] Decoding building tiles: 2500/2500",
            ]
        );
        // The last thousand is the end: one line, not two.
        assert_eq!(
            due(3000),
            [
                "[Rust] Decoding building tiles: 1000/3000",
                "[Rust] Decoding building tiles: 2000/3000",
                "[Rust] Decoding building tiles: 3000/3000",
            ]
        );
        assert_eq!(due(7), ["[Rust] Decoding building tiles: 7/7"]);
        assert!(due(0).is_empty(), "a directory without tiles reports no progress");
        // The plugin reads output-tile progress with `Progress:\s*(\d+)/(\d+)`
        // searched anywhere in a line; these must never match it.
        for line in due(2500) {
            assert!(!line.contains("Progress:"), "{line:?}");
        }
    }

    #[test]
    fn the_list_decode_line_names_the_directory_as_given_and_counts_plainly() {
        // The Waveshed plugin compares the line with the one it expects.
        assert_eq!(
            pbf_list_decode_line(21000, std::path::Path::new(r"C:\Users\me\cache\pool\0a1b2c")),
            r#"[Rust] Decoding 21000 listed building tiles from "C:\Users\me\cache\pool\0a1b2c" (once for the run)..."#
        );
        assert_eq!(
            pbf_list_decode_line(0, std::path::Path::new("/cache//pool/./0a1b2c")),
            r#"[Rust] Decoding 0 listed building tiles from "/cache//pool/./0a1b2c" (once for the run)..."#
        );
        assert!(!pbf_list_decode_line(1_234_567, std::path::Path::new("p")).contains("Progress:"));
        assert!(pbf_list_decode_line(1_234_567, std::path::Path::new("p")).contains(" 1234567 listed "));
    }

    #[test]
    fn the_decoded_line_counts_tile_files_and_decoded_buildings() {
        let set = buildings::PbfBuildingSet {
            tiles_found: 20808,
            tiles_read: 20807,
            decoded: 1_234_567,
            ..Default::default()
        };
        let line = pbf_decoded_line(&set, 40.04);
        // Changelog 52: the set's memory last (an empty set: nothing held).
        assert_eq!(line, "[Rust] Building tiles decoded: 20808 tile(s), 1234567 building(s) in 40.0 s, 0.0 MB");
        assert!(!line.contains("Progress:"));
        // The Waveshed plugin's pattern still reads the tiles, buildings and
        // seconds off it (`_DECODED_RE` of its run_progress.py, searched).
        let tail = line.strip_prefix("[Rust] Building tiles decoded: 20808 tile(s), 1234567 building(s) in ").unwrap();
        assert!(tail.starts_with("40.0 s"), "{tail}");
        // A set's figure is its heap bytes, in MB with one decimal.
        let house = buildings::Building {
            coords: vec![(8.0, 47.0); 5],
            holes: vec![vec![(8.0, 47.0); 4]],
            height: buildings::BuildingHeight::AboveGround(9.0),
            source: buildings::HeightSource::ExplicitHeight,
        };
        let set = buildings::PbfBuildingSet { buildings: vec![house; 20_000], decoded: 20_000, ..Default::default() };
        let line = pbf_decoded_line(&set, 1.0);
        assert!(line.ends_with(&format!(" in 1.0 s, {} MB", proc_usage::mb(set.heap_bytes()))), "{line}");
        assert!(line.ends_with(" in 1.0 s, 5.3 MB"), "{line}");
    }

    #[test]
    fn the_batch_threads_count_the_building_sets() {
        const MB: u64 = 1 << 20;
        // 16 cores, 250 MB a tile: 4 GB usable runs 16, the sets take theirs.
        assert_eq!(batch_threads(4096 * MB, 0, 250 * MB, 16), 16);
        assert_eq!(batch_threads(4096 * MB, 1000 * MB, 250 * MB, 16), 12);
        // At least one, never more than the cores, no overflow.
        assert_eq!(batch_threads(1000 * MB, 5000 * MB, 250 * MB, 16), 1);
        assert_eq!(batch_threads(u64::MAX, 0, 1, 16), 16);
        assert_eq!(batch_threads(0, 0, 0, 0), 1);
    }

    #[test]
    fn the_pool_line_appends_the_building_estimate_and_what_comes_later() {
        let dir = |p: &str| buildings::PbfSource::Dir(std::path::PathBuf::from(p));
        let est = |present: Vec<(buildings::PbfSource, u64)>, later| BuildingEstimate { present, later };
        assert_eq!(est(vec![], 0).pool_line_suffix(), "");
        assert!(!est(vec![], 0).any());
        assert_eq!(est(vec![(dir("a"), 980 << 20), (dir("b"), 20 << 20)], 0).pool_line_suffix(), ", building set ~1000 MB");
        assert_eq!(est(vec![(dir("a"), 980 << 20)], 1).pool_line_suffix(), ", building set ~980 MB + 1 source(s) decoded later");
        assert_eq!(est(vec![], 2).pool_line_suffix(), ", building set: 2 source(s) decoded later");
        assert!(est(vec![], 2).any());
    }

    #[test]
    fn a_decoded_set_larger_than_counted_lowers_the_tiles_at_once_and_never_raises_them() {
        const MB: u64 = 1 << 20;
        let a = buildings::PbfSource::Dir(std::path::PathBuf::from("a"));
        let b = buildings::PbfSource::Dir(std::path::PathBuf::from("b"));
        // 4 GB usable, 250 MB a tile, a 500 MB estimate: 14 threads.
        let est = BuildingEstimate { present: vec![(a.clone(), 500 * MB)], later: 1 };
        let threads = batch_threads(4096 * MB, est.bytes(), 250 * MB, 16);
        assert_eq!(threads, 14);
        let cap = TileCap::new(4096 * MB, 250 * MB, threads, 6 << 30, &est);
        assert_eq!((cap.get(), cap.held_sets()), (14, 500 * MB));
        // Decoded smaller than estimated: the cap stays (the pool is no larger).
        cap.settle(&a, 300 * MB);
        assert_eq!((cap.get(), cap.held_sets()), (14, 300 * MB));
        // The later source decodes to 1 GB: 4096 − 1324 MB leaves room for 11.
        cap.settle(&b, 1024 * MB);
        assert_eq!((cap.get(), cap.held_sets()), (11, 1324 * MB));
        // Settling again with less never raises it.
        cap.settle(&b, 10 * MB);
        assert_eq!(cap.get(), 11);
        assert_eq!(
            tiles_at_once_line(11, 14, 1324 * MB, 250 * MB, 6 << 30),
            "[Rust] Tiles at once: 11 of 14 threads from here on (building sets 1324.0 MB; ~250 MB per tile, 6 GB available at start)"
        );
    }

    #[test]
    fn the_lanes_run_every_job_once_at_most_cap_at_once_and_stop_at_a_lowered_cap() {
        use std::sync::atomic::AtomicUsize;
        let pool = rayon::ThreadPoolBuilder::new().num_threads(6).build().unwrap();
        let job = |i: usize| -> ingest::IngestJob {
            serde_json::from_value(serde_json::json!({
                "output_path": format!("/nonexistent/t{i}.abt"), "ul_lat": 47.0, "ul_lon": 8.0,
                "resolution_m": 10.0, "size_px": 16, "sources": [{"path": "/nonexistent/s.tif"}],
            }))
            .unwrap()
        };
        let est = BuildingEstimate { present: Vec::new(), later: 1 };
        // 6 lanes, cap 6: every job once, never more than 6 at once — the
        // tiles also run nested rayon work, as real ones do.
        let cap = TileCap::new(6 << 30, 1 << 30, 6, 0, &est);
        let (now, most) = (AtomicUsize::new(0), AtomicUsize::new(0));
        let seen = Mutex::new(Vec::new());
        let run = |j: ingest::IngestJob| {
            let n = now.fetch_add(1, Ordering::SeqCst) + 1;
            most.fetch_max(n, Ordering::SeqCst);
            let _: u64 = (0..20_000u64).into_par_iter().map(|x| x % 7).sum();
            std::thread::sleep(std::time::Duration::from_millis(5));
            seen.lock().unwrap().push(j.output_path);
            now.fetch_sub(1, Ordering::SeqCst);
        };
        run_in_lanes(&pool, 6, &cap, (0..60).map(job).collect(), &run);
        let mut got = seen.into_inner().unwrap();
        got.sort();
        let mut want: Vec<_> = (0..60).map(|i| job(i).output_path).collect();
        want.sort();
        assert_eq!(got, want);
        assert!(most.load(Ordering::SeqCst) <= 6);
        // A cap lowered to 2 before the start: never more than 2 at once.
        let cap = TileCap::new(6 << 30, 1 << 30, 6, 0, &est);
        cap.cap.store(2, Ordering::SeqCst);
        most.store(0, Ordering::SeqCst);
        let seen = Mutex::new(Vec::new());
        let run = |j: ingest::IngestJob| {
            let n = now.fetch_add(1, Ordering::SeqCst) + 1;
            most.fetch_max(n, Ordering::SeqCst);
            std::thread::sleep(std::time::Duration::from_millis(5));
            seen.lock().unwrap().push(j.output_path);
            now.fetch_sub(1, Ordering::SeqCst);
        };
        run_in_lanes(&pool, 6, &cap, (0..30).map(job).collect(), &run);
        assert_eq!(seen.into_inner().unwrap().len(), 30);
        assert_eq!(most.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn a_panicking_tile_ends_its_lane_the_others_finish_and_the_panic_goes_on() {
        let pool = rayon::ThreadPoolBuilder::new().num_threads(3).build().unwrap();
        let job = |i: usize| -> ingest::IngestJob {
            serde_json::from_value(serde_json::json!({
                "output_path": format!("/nonexistent/t{i}.abt"), "ul_lat": 47.0, "ul_lon": 8.0,
                "resolution_m": 10.0, "size_px": 16, "sources": [{"path": "/nonexistent/s.tif"}],
            }))
            .unwrap()
        };
        let est = BuildingEstimate { present: Vec::new(), later: 1 };
        let cap = TileCap::new(6 << 30, 1 << 30, 3, 0, &est);
        let done = AtomicUsize::new(0);
        let run = |j: ingest::IngestJob| {
            if j.output_path.ends_with("t4.abt") {
                panic!("tile 4");
            }
            done.fetch_add(1, Ordering::SeqCst);
        };
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run_in_lanes(&pool, 3, &cap, (0..20).map(job).collect(), &run)
        }));
        let panic = r.expect_err("the tile's panic goes on");
        assert_eq!(panic.downcast_ref::<&str>(), Some(&"tile 4"));
        assert_eq!(done.load(Ordering::SeqCst), 19, "every other job ran");
    }
}
