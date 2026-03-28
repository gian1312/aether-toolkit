// rust/aether_converter/src/main.rs
mod geo;
mod ingest;

use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::fs;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicUsize, Ordering};
use rayon::prelude::*;

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
    }
}

fn main() -> anyhow::Result<()> {
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
                println!("[Rust] Batch processing {} tiles in parallel...", jobs.len());
                let total = jobs.len();
                let progress = AtomicUsize::new(0);

                // Run all jobs in parallel using Rayon
                jobs.into_par_iter().for_each(|job| {
                    if let Err(e) = ingest::process_tile_with_cache(job, texture_cache.clone()) {
                        println!("[Error] Failed to process tile: {}", e);
                    }

                    let curr = progress.fetch_add(1, Ordering::Relaxed) + 1;
                    if curr % 10 == 0 || curr == total {
                        println!("[Rust] Progress: {}/{}", curr, total);
                    }

                    // --- OOM FIX / CACHE THRASHING FIX ---
                    // Periodically let one thread clean the cache if it gets too large
                    if curr % 5 == 0 {
                        if let Ok(mut cache) = texture_cache.try_lock() {
                            if cache.len() > 30 {
                                cache.retain(|k, _| k.file_name().unwrap_or_default().to_string_lossy().starts_with("Chunk_"));
                            }
                        }
                    }
                });
            }
            Ok(())
        }
    }
}