// rust/aether_converter/src/main.rs
mod download;
mod geo;
mod ingest;

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

                let total = jobs.len();
                let progress = AtomicUsize::new(0);

                // Run all jobs in parallel using the constrained pool
                pool.install(|| {
                    jobs.into_par_iter().for_each(|job| {
                        if let Err(e) = ingest::process_tile_with_cache(job, texture_cache.clone()) {
                            println!("[Error] Failed to process tile: {}", e);
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
            }
            Ok(())
        },
        Commands::Download { job_file } => {
            download::run_download(&job_file)
        },
    }
}
