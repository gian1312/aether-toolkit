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
/// job field `cache_dir`. Bump the crate version — and the §1.5 table — with
/// the next capability.
#[test]
fn version_covers_the_documented_capability_gates() {
    let v: Vec<u32> = env!("CARGO_PKG_VERSION").split('.').map(|p| p.parse().unwrap()).collect();
    assert_eq!(
        v,
        [0, 2, 8],
        "CONTRACT §1.5 gates AETHER_DIAG_FILE and download `cache_dir` on aether_converter 0.2.8 \
         (probe/[Tile]/atomic writes on 0.2.7)"
    );
    let out = Command::new(env!("CARGO_BIN_EXE_aether_converter"))
        .args(["probe", "--help"])
        .output()
        .expect("spawn aether_converter");
    assert!(out.status.success(), "the probe subcommand exists from 0.2.7 on");
}
