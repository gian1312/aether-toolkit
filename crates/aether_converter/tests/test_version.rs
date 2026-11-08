//! `--version` handshake (CONTRACT.md §1.5): every toolkit binary prints
//! `<name> <semver>` to stdout and exits 0, so consumers can gate on
//! capability without parsing help text.
use std::process::Command;

#[test]
fn version_flag_prints_name_and_semver() {
    let out = Command::new(env!("CARGO_BIN_EXE_aether_converter"))
        .arg("--version")
        .output()
        .expect("spawn aether_converter");
    assert!(out.status.success(), "exit {:?}", out.status.code());
    let text = String::from_utf8(out.stdout).unwrap();
    let expected = format!("aether_converter {}", env!("CARGO_PKG_VERSION"));
    assert_eq!(text.trim(), expected);
}

/// The version consumers gate on (CONTRACT §1.5) must cover what this build
/// ships: 0.2.6 brought the `download` repair-job fields, 0.2.7 the `probe`
/// subcommand, the batch-ingest `[Tile]` line and atomic tile replacement,
/// 0.2.8 `AETHER_DIAG_FILE` (diagnostic lines to a file) and the `download`
/// job field `cache_dir`, 0.2.9 the `download` pipeline of changelog 39
/// (RAM budget fixed per run, per-row release, outputs held open — no new
/// field or parsed line, so no new gate), 0.2.10 the `download` job fields
/// `buildings_pbf_ready` / `buildings_pbf_wait_s` (changelog 45: wait for the
/// building tiles a caller fetches during the download), 0.2.11 the
/// `[Tile] <name> voids=<n>` lines of `download` and batch `ingest` and the
/// download progress record kept in `AETHER_DIAG_FILE` (changelog 46; the
/// parallel duplicate filter of changelog 47 needs no gate), 0.2.12 the batch
/// `ingest` job fields `buildings_pbf_ready` / `buildings_pbf_wait_s` /
/// `buildings_hold_mb` (changelog 48: building tiles fetched while the batch
/// samples its terrain) and the `[Tile] … voids=` lines kept in
/// `AETHER_DIAG_FILE`, 0.2.13 the `download` job field `seam_filter` and the
/// seam filter it switches (changelog 49; also the `[Stats] Backfill:` line)
/// and the process's peak memory at the end of a run — `[Stats] Peak RSS:`
/// and `[Rust] run: … peak rss` (changelog 50; informational, no gate of its
/// own), 0.2.14 the job field `buildings_pbf_list` of `download` and batch
/// `ingest` (changelog 51: only the listed building tiles are read from
/// `buildings_pbf_dir`, and the list file is their ready signal), 0.2.15 the
/// honest memory budgets of changelog 52 (the download's `RAM budget` covers
/// the whole process and fewer connections run when the share cannot hold
/// them — `[Download] connections <a> → <b>: …`; ingest counts the decoded
/// building set — informational lines only, nothing to gate on). Bump the
/// crate version — and the §1.5 table — with the next capability.
#[test]
fn version_covers_the_documented_capability_gates() {
    let v: Vec<u32> = env!("CARGO_PKG_VERSION").split('.').map(|p| p.parse().unwrap()).collect();
    assert_eq!(
        v,
        [0, 2, 15],
        "CONTRACT §1.5 notes the honest memory budgets (changelog 52) on \
         aether_converter 0.2.15, gates `buildings_pbf_list` (download and batch ingest) on \
         0.2.14 (changelog 51), the download seam filter (`seam_filter`) on \
         0.2.13 (changelog 49; the peak-RSS lines of changelog 50 came with it), \
         ingest `buildings_pbf_ready` on 0.2.12 \
         (changelog 48), the `[Tile] … voids=` lines on 0.2.11 (changelog 46), \
         download `buildings_pbf_ready` on 0.2.10 (changelog 45), \
         AETHER_DIAG_FILE and download `cache_dir` on 0.2.8 \
         (probe/[Tile]/atomic writes on 0.2.7)"
    );
    let out = Command::new(env!("CARGO_BIN_EXE_aether_converter"))
        .args(["probe", "--help"])
        .output()
        .expect("spawn aether_converter");
    assert!(out.status.success(), "the probe subcommand exists from 0.2.7 on");
}
