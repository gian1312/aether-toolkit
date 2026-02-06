mod geo;
mod ingest;

use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::fs;

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
            if let Ok(job) = serde_json::from_str::<ingest::IngestJob>(&content) {
                println!("[Rust] Processing single tile: {:?}", job.output_path);
                ingest::process_tile(job)?;
            } else if let Ok(jobs) = serde_json::from_str::<Vec<ingest::IngestJob>>(&content) {
                println!("[Rust] Batch processing {} tiles...", jobs.len());
                let total = jobs.len();
                for (i, job) in jobs.into_iter().enumerate() {
                    // FIX: Added 'total' variable to match the 3 placeholders [{}/{}] {:?}
                    println!("[Rust] [{}/{}] Generating {:?}", i+1, total, job.output_path);
                    ingest::process_tile(job)?;
                }
            }
            Ok(())
        }
    }
}