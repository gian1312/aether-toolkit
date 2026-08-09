// rust/aether_converter/src/main.rs
mod buildings;
mod download;
mod geo;
mod ingest;
mod mvt;

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

    match args.command {
        Commands::Convert { .. } => {
            println!("Legacy convert command is deprecated. Use 'ingest'.");
            Ok(())
        },
        Commands::Ingest { job_file } => {
            let content = fs::read_to_string(&job_file)?;

            // Wrap cache in Arc<Mutex> for safe multi-threading
            let texture_cache = Arc::new(Mutex::new(HashMap::new()));

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

                // --- OOM FIX 2: PRE-LOAD MASSIVE BASE DEMS SEQUENTIALLY ---
                // If 16 threads try to load a 600MB Chunk TIF simultaneously, RAM explodes immediately.
                let mut unique_bases = HashSet::new();
                for job in &jobs {
                    if let Some(base) = &job.base_tif {
                        unique_bases.insert(base.clone());
                    }
                }

                if !unique_bases.is_empty() {
                    println!("[Rust] Pre-loading {} Base DEM(s) to prevent memory races...", unique_bases.len());
                    let mut cache = texture_cache.lock().unwrap();
                    for base in unique_bases {
                        if let Ok(img) = ingest::load_tiff_to_ram(&base) {
                            cache.insert(base, img);
                        } else {
                            println!("[Warn] Failed to pre-load Base DEM: {:?}", base);
                        }
                    }
                }

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
                                    // Retain Base DEMs (Chunk_), drop micro tiles
                                    cache.retain(|k, _| k.file_name().unwrap_or_default().to_string_lossy().starts_with("Chunk_"));
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
            }
            Ok(())
        },
        Commands::Download { job_file } => {
            download::run_download(&job_file)
        },
    }
}
