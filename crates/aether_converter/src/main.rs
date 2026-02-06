mod geo;
mod ingest;

use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::fs;
use std::collections::HashMap; // Added

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
    let args = Cli::parse();

    match args.command {
        Commands::Convert { input: _, output: _ } => {
            println!("Legacy convert not optimized. Use Ingest.");
            Ok(())
        },
        Commands::Ingest { job_file } => {
            let content = fs::read_to_string(&job_file)?;

            // --- CACHE INITIALIZATION ---
            // We use a simple HashMap. It is effectively "Global" for this process run.
            let mut texture_cache = HashMap::new();

            if let Ok(job) = serde_json::from_str::<ingest::IngestJob>(&content) {
                println!("[Rust] Processing single tile: {:?}", job.output_path);
                ingest::process_tile_with_cache(job, &mut texture_cache)?;
            } else if let Ok(jobs) = serde_json::from_str::<Vec<ingest::IngestJob>>(&content) {
                println!("[Rust] Batch processing {} tiles...", jobs.len());
                let total = jobs.len();
                for (i, job) in jobs.into_iter().enumerate() {
                    println!("[Rust] [{}/{}] Generating {:?}", i+1, total, job.output_path);
                    // Pass the cache mutably
                    ingest::process_tile_with_cache(job, &mut texture_cache)?;
                }
            }
            Ok(())
        }
    }
}