mod geo;
mod ingest;

use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::fs;
use std::collections::HashMap;

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
    // FIX: Strictly ignore environment variables for noisy modules.
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
            let mut texture_cache = HashMap::new();

            if let Ok(job) = serde_json::from_str::<ingest::IngestJob>(&content) {
                println!("[Rust] Processing single tile: {:?}", job.output_path);
                ingest::process_tile_with_cache(job, &mut texture_cache)?;
            } else if let Ok(jobs) = serde_json::from_str::<Vec<ingest::IngestJob>>(&content) {
                println!("[Rust] Batch processing {} tiles...", jobs.len());
                let total = jobs.len();
                for (i, job) in jobs.into_iter().enumerate() {
                    // Log progress every 10 tiles
                    if i % 10 == 0 || i == total - 1 {
                        println!("[Rust] Progress: {}/{}", i + 1, total);
                    }
                    ingest::process_tile_with_cache(job, &mut texture_cache)?;

                    // --- OOM FIX / CACHE THRASHING FIX ---
                    // Prevent memory fragmentation and thread-starvation.
                    // If we hold more than 30 TIFFs, we drop the small Swiss tiles
                    // BUT permanently retain the massive 'Chunk_*.tif' Base DEMs!
                    if texture_cache.len() > 30 {
                        texture_cache.retain(|k, _| k.file_name().unwrap_or_default().to_string_lossy().starts_with("Chunk_"));
                    }
                }
            }
            Ok(())
        }
    }
}