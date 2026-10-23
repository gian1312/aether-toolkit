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
                // known as void without opening it.
                tile_line = Some(format!("[Tile] {} covered={}/{}", tile_name, stats.covered_px, stats.total_px));
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
            if let Some(line) = &tile_line {
                let _ = writeln!(out, "{}", line);
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
/// `[Info] buildings_pbf_dir` line counts them) and the decode's wall time.
fn pbf_decoded_line(set: &buildings::PbfBuildingSet, secs: f64) -> String {
    format!(
        "[Rust] Building tiles decoded: {} tile(s), {} building(s) in {:.1} s",
        set.tiles_found, set.decoded, secs
    )
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
                // is a conservative stand-in.
                let available_ram = match sys.available_memory() {
                    0 => total_ram / 2,
                    a => a,
                };
                let per_job = jobs
                    .iter()
                    .map(|j| ingest::estimate_job_bytes(j, &opts))
                    .max()
                    .unwrap_or(1)
                    .max(1);
                let usable = (available_ram as f64 * 0.8) as u64;
                let usable = usable.saturating_sub(ingest::DEFAULT_CHUNK_CACHE_BYTES as u64);
                let safe_threads = ((usable / per_job) as usize).clamp(1, available_cores);
                // Reported by the `[Rust] run:` line at exit (changelog 31).
                ingest::note_batch_threads(safe_threads);

                println!("[Rust] Batch processing {} tiles...", jobs.len());
                println!(
                    "[Rust] Rayon Pool: {} threads (System RAM: {} GB, {} GB available, ~{} MB per tile)",
                    safe_threads,
                    total_ram / 1024 / 1024 / 1024,
                    available_ram / 1024 / 1024 / 1024,
                    per_job / 1024 / 1024
                );
                {
                    use std::io::Write;
                    let _ = std::io::stdout().flush();
                }

                let pool = rayon::ThreadPoolBuilder::new().num_threads(safe_threads).build().unwrap();

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
                let mut pbf_sets: HashMap<PathBuf, buildings::PbfBuildingSet> = HashMap::new();
                for job in &jobs {
                    if let Some(dir) = &job.buildings_pbf_dir {
                        if !pbf_sets.contains_key(dir) {
                            println!("[Rust] Decoding building tiles in {:?} (once for the run)...", dir);
                            let t0 = std::time::Instant::now();
                            let set = pool.install(|| {
                                buildings::load_pbf_building_dir_with_progress(dir, &print_pbf_decode_progress)
                            })?;
                            println!("{}", pbf_decoded_line(&set, t0.elapsed().as_secs_f64()));
                            let _ = std::io::stdout().flush();
                            pbf_sets.insert(dir.clone(), set);
                        }
                    }
                }

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

                // Run all jobs in parallel using the constrained pool
                pool.install(|| {
                    jobs.into_par_iter().for_each(|job| {
                        let pbf = match &job.buildings_pbf_dir {
                            Some(dir) => pbf_sets.get(dir),
                            None => None,
                        };
                        // The output's file name, for the per-tile line below
                        // (the job moves into process_tile_batched).
                        let tile_name = job
                            .output_path
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_else(|| job.output_path.display().to_string());
                        let tally = tally.clone();
                        ingest::process_tile_batched(
                            job,
                            texture_cache.clone(),
                            pbf,
                            &opts,
                            &writer,
                            Box::new(move |result| tally.tile_done(&tile_name, result)),
                        );
                    });
                });
                // Every tile is reported before the batch result is decided.
                writer.finish();
                // The shared cache keeps up to a few hundred source files
                // open (CONTRACT changelog 37). The process exits next, which
                // closes them all at once; dropping the cache would close them
                // one by one here — a file-system round trip each on a
                // network share or a 9p mount.
                std::mem::forget(opts);

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
    fn the_decoded_line_counts_tile_files_and_decoded_buildings() {
        let set = buildings::PbfBuildingSet {
            tiles_found: 20808,
            tiles_read: 20807,
            decoded: 1_234_567,
            ..Default::default()
        };
        let line = pbf_decoded_line(&set, 40.04);
        assert_eq!(line, "[Rust] Building tiles decoded: 20808 tile(s), 1234567 building(s) in 40.0 s");
        assert!(!line.contains("Progress:"));
    }
}
