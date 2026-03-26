// =============================================================================
// AETHER Raster Aggregator
// =============================================================================
//
// Aggregates multiple raster files (.tif or .dat) into max + count GeoTIFFs.
// Pure Rust, no GDAL. Writes valid BigTIFF directly.
//
// Progress protocol (matched to aether_export):
//   [P:NN]       — percentage (0–100)
//   [S:message]  — status string
//   [E:message]  — fatal error
//
// Output: two sparse BigTIFF files (<basename>_max.tif, <basename>_count.tif)
// with DEFLATE compression, tiled, GeoTIFF tags, EPSG:4326.
// Overviews are NOT built here — the Python caller adds them via GDAL
// after this process exits (clean memory context).

use clap::Parser;
use std::fs;
use std::path::PathBuf;
use std::time::Instant;

mod aggregate;
mod reader;
mod writer;

// ═══════════════════════════════════════════════════════════════════════════════
// CLI
// ═══════════════════════════════════════════════════════════════════════════════

#[derive(Parser, Debug)]
#[command(name = "aether_aggregate")]
#[command(about = "Aggregate raster files into max/count COG outputs (pure Rust, no GDAL)")]
struct Args {
    /// Text file with one input path per line (.tif or .dat)
    #[arg(short = 'f', long)]
    file_list: PathBuf,

    /// Output basename (produces <basename>_max.tif and <basename>_count.tif)
    #[arg(short = 'o', long)]
    output: String,

    /// Compression level (1=fast, 9=small). 0=no compression.
    #[arg(short = 'l', long, default_value_t = 1)]
    level: u32,

    /// Tile size in pixels (default 2048, must be multiple of 16)
    #[arg(short = 't', long, default_value_t = 2048)]
    tile_size: usize,
}

// ═══════════════════════════════════════════════════════════════════════════════
// Entry Point
// ═══════════════════════════════════════════════════════════════════════════════

fn main() {
    let args = Args::parse();
    if let Err(e) = run(args) {
        println!("[E:{}]", e);
        std::process::exit(1);
    }
}

fn run(args: Args) -> Result<(), Box<dyn std::error::Error>> {
    let t_total = Instant::now();
    println!("[P:0]");
    println!("[S:Starting Aggregation...]");

    // --- Load input file list ---
    let file_list = fs::read_to_string(&args.file_list)?;
    let paths: Vec<PathBuf> = file_list
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .map(PathBuf::from)
        .collect();

    if paths.is_empty() {
        return Err("No input files in file list.".into());
    }
    eprintln!("[Aggregate] {} input files", paths.len());

    // --- Open all inputs ---
    let mut inputs: Vec<reader::InputRaster> = Vec::with_capacity(paths.len());
    for p in &paths {
        match reader::InputRaster::open(p) {
            Ok(r) => inputs.push(r),
            Err(e) => eprintln!("[WARNING] Skipping {}: {}", p.display(), e),
        }
    }

    if inputs.is_empty() {
        return Err("No valid input files after opening.".into());
    }

    // --- Run aggregation ---
    let max_path = format!("{}_max.tif", args.output);
    let count_path = format!("{}_count.tif", args.output);

    let stats = aggregate::run(
        &inputs,
        &max_path,
        &count_path,
        args.tile_size,
        args.level,
    )?;

    let total = t_total.elapsed().as_secs_f64();
    let max_mb = fs::metadata(&max_path).map(|m| m.len()).unwrap_or(0) as f64 / (1024.0 * 1024.0);
    let cnt_mb = fs::metadata(&count_path).map(|m| m.len()).unwrap_or(0) as f64 / (1024.0 * 1024.0);

    eprintln!(
        "[Aggregate] SUCCESS | {:.2}s | max: {:.1} MB, count: {:.1} MB | has_data: {}",
        total, max_mb, cnt_mb, stats.has_valid_data
    );
    println!("[P:100]");
    println!("[S:Aggregation complete]");

    Ok(())
}