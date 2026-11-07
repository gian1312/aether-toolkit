pub mod buildings;
pub mod diag;
pub mod canopy;
pub mod download;
pub mod ingest;
pub mod mvt;
pub mod plan;
// This process's CPU time, page faults and peak memory for the end-of-run
// lines of `ingest` and `download` (changelog 31, 37, 50).
mod proc_usage;
mod tiff_io;
