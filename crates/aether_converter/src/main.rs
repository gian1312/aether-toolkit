// rust/aether_converter/src/main.rs
mod buildings;
// Reached only through `mvt::apply_buildings_to_abt_tiles`; the CLI has no
// canopy subcommand, so the rest of the module is dead code in this binary.
#[allow(dead_code)]
mod canopy;
mod download;
mod ingest;
mod mvt;
mod plan;
mod tiff_io;

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
                // Coverage is judged over the WHOLE batch, never per tile: the
                // edge tiles of any area legitimately fall outside the sources,
                // and failing on those would refuse every ordinary run.
                let covered = AtomicU64::new(0);

                // Run all jobs in parallel using the constrained pool
                pool.install(|| {
                    jobs.into_par_iter().for_each(|job| {
                        let pbf = match &job.buildings_pbf_dir {
                            Some(dir) => pbf_sets.get(dir),
                            None => None,
                        };
                        match ingest::process_tile_opts(job, texture_cache.clone(), pbf, &opts) {
                            Ok(stats) => {
                                covered.fetch_add(stats.covered_px, Ordering::Relaxed);
                            }
                            Err(e) => {
                                // `{:#}` prints the whole cause chain ("loading
                                // source …: Opening …: No such file …"); `{}`
                                // printed only the outermost context, which
                                // named the file but never said what was wrong.
                                println!("[Error] Failed to process tile: {:#}", e);
                                failures.fetch_add(1, Ordering::Relaxed);
                            }
                        }

                        let curr = progress.fetch_add(1, Ordering::Relaxed) + 1;
                        if curr % 10 == 0 || curr == total {
                            println!("[Rust] Progress: {}/{}", curr, total);
                            use std::io::Write;
                            let _ = std::io::stdout().flush();
                        }
                    });
                });

                // Fail loudly if any tile could not be produced, so callers
                // don't treat a partial/terrain-less batch as success.
                let n_failed = failures.load(Ordering::Relaxed);
                if n_failed > 0 {
                    anyhow::bail!("{} of {} tiles failed to convert", n_failed, total);
                }
                if covered.load(Ordering::Relaxed) == 0 {
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
