// rust/aether_converter/src/main.rs
mod buildings;
mod download;
mod ingest;
mod mvt;
mod plan;

use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::fs;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicUsize, Ordering};
#[cfg(feature = "native")]
use rayon::prelude::*;
#[cfg(feature = "native")]
use sysinfo::System;

#[derive(Parser)]
#[command(name = "aether_converter")]
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
}

/// Files at or above this size are pre-loaded sequentially when shared by
/// more than one job, so a thread-per-tile batch cannot load the same huge
/// raster many times at once (the classic OOM). Decision: "100 MB" is read as
/// the decimal 100,000,000 bytes — the threshold is a heuristic, not a format.
const PRELOAD_MIN_BYTES: u64 = 100_000_000;

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

    match args.command {
        Commands::Convert { .. } => {
            println!("Legacy convert command is deprecated. Use 'ingest'.");
            Ok(())
        },
        Commands::Ingest { job_file } => {
            let content = fs::read_to_string(&job_file)?;

            // Wrap cache in Arc<Mutex> for safe multi-threading
            let texture_cache: ingest::ImageCache = Arc::new(Mutex::new(HashMap::new()));

            if let Ok(job) = serde_json::from_str::<ingest::IngestJob>(&content) {
                println!("[Rust] Processing single tile: {:?}", job.output_path);
                ingest::process_tile_with_cache(job, texture_cache)?;
            } else if let Ok(jobs) = serde_json::from_str::<Vec<ingest::IngestJob>>(&content) {

                // --- OOM FIX 1: CALCULATE SAFE THREAD COUNT ---
                let available_cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
                let mut sys = System::new_all();
                sys.refresh_memory();
                let total_ram_gb = sys.total_memory() / 1024 / 1024 / 1024;

                // Budget ~1.5 GB of RAM per thread safely (BC6H RGBA Buffer + Tiff decoding overhead)
                let safe_threads = ((total_ram_gb as f64 / 1.5).floor() as usize).clamp(1, available_cores);

                println!("[Rust] Batch processing {} tiles...", jobs.len());
                println!("[Rust] Rayon Pool: {} threads (System RAM: {} GB)", safe_threads, total_ram_gb);
                {
                    use std::io::Write;
                    let _ = std::io::stdout().flush();
                }

                let pool = rayon::ThreadPoolBuilder::new().num_threads(safe_threads).build().unwrap();

                // --- OOM FIX 2: PRE-LOAD LARGE SHARED SOURCES SEQUENTIALLY ---
                // If 16 threads try to load a 600MB source simultaneously, RAM
                // explodes immediately. Normalizing every job up front also
                // surfaces bad-job errors (ambiguous sources, etc.) before any
                // tile work starts. Rule: pre-load every unique source that
                // appears in MORE THAN ONE job and is larger than
                // PRELOAD_MIN_BYTES on disk; small per-tile files keep the
                // lazy per-tile cache loading.
                let mut jobs_per_source: HashMap<ingest::CacheKey, usize> = HashMap::new();
                for job in &jobs {
                    let mut seen_in_job: HashSet<ingest::CacheKey> = HashSet::new();
                    for spec in job.effective_sources()? {
                        let key = ingest::cache_key(&spec);
                        if seen_in_job.insert(key.clone()) {
                            *jobs_per_source.entry(key).or_default() += 1;
                        }
                    }
                }
                let preload: HashSet<ingest::CacheKey> = jobs_per_source
                    .into_iter()
                    .filter(|(key, n_jobs)| {
                        *n_jobs > 1
                            && fs::metadata(&key.0)
                                .map(|m| m.len() > PRELOAD_MIN_BYTES)
                                .unwrap_or(false) // missing file: the per-tile load errors loudly
                    })
                    .map(|(key, _)| key)
                    .collect();

                if !preload.is_empty() {
                    println!("[Rust] Pre-loading {} large shared source(s) to prevent memory races...", preload.len());
                    let mut cache = texture_cache.lock().unwrap();
                    for key in &preload {
                        // load_source_to_ram sniffs the AETH magic itself, so
                        // large shared .abt sources preload exactly like TIFFs.
                        let img = ingest::load_source_to_ram(&key.0, key.1.map(f64::from_bits))
                            .map_err(|e| anyhow::anyhow!("pre-loading shared source {:?}: {e}", key.0))?;
                        cache.insert(key.clone(), img);
                    }
                }
                let preload = Arc::new(preload);

                // --- PBF DECODE HOIST ---
                // A `buildings_pbf_dir` decodes to the same building set for
                // every output tile, so scan and decode each directory once for
                // the whole run. Doing it inside the loop below re-read and
                // re-decoded the entire directory once per .abt.
                let mut pbf_sets: HashMap<PathBuf, buildings::PbfBuildingSet> = HashMap::new();
                for job in &jobs {
                    if let Some(dir) = &job.buildings_pbf_dir {
                        if !pbf_sets.contains_key(dir) {
                            println!("[Rust] Decoding building tiles in {:?} (once for the run)...", dir);
                            pbf_sets.insert(dir.clone(), buildings::load_pbf_building_dir(dir)?);
                        }
                    }
                }

                let total = jobs.len();
                let progress = AtomicUsize::new(0);
                let failures = AtomicUsize::new(0);

                // Run all jobs in parallel using the constrained pool
                pool.install(|| {
                    jobs.into_par_iter().for_each(|job| {
                        let pbf = match &job.buildings_pbf_dir {
                            Some(dir) => pbf_sets.get(dir),
                            None => None,
                        };
                        if let Err(e) = ingest::process_tile(job, texture_cache.clone(), pbf) {
                            println!("[Error] Failed to process tile: {}", e);
                            failures.fetch_add(1, Ordering::Relaxed);
                        }

                        let curr = progress.fetch_add(1, Ordering::Relaxed) + 1;
                        if curr % 10 == 0 || curr == total {
                            println!("[Rust] Progress: {}/{}", curr, total);
                            use std::io::Write;
                            let _ = std::io::stdout().flush();
                        }

                        // --- CACHE THRASHING FIX ---
                        // Periodically let one thread clean the cache if it gets too large
                        if curr % 5 == 0 {
                            if let Ok(mut cache) = texture_cache.try_lock() {
                                if cache.len() > 30 {
                                    // Retain the pre-loaded shared sources and
                                    // legacy Chunk_ base DEMs, drop micro tiles
                                    let keep = preload.clone();
                                    cache.retain(|k, _| {
                                        keep.contains(k)
                                            || k.0.file_name().unwrap_or_default().to_string_lossy().starts_with("Chunk_")
                                    });
                                }
                            }
                        }
                    });
                });

                // Fail loudly if any tile could not be produced, so callers
                // don't treat a partial/terrain-less batch as success.
                let n_failed = failures.load(Ordering::Relaxed);
                if n_failed > 0 {
                    anyhow::bail!("{} of {} tiles failed to convert", n_failed, total);
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
}
