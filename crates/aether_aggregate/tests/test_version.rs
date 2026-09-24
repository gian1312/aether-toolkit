//! `--version` handshake (CONTRACT.md §1.5): every toolkit binary prints
//! `<name> <semver>` to stdout and exits 0, so consumers can gate on
//! capability without parsing help text.
use std::process::Command;

#[test]
fn version_flag_prints_name_and_semver() {
    let out = Command::new(env!("CARGO_BIN_EXE_aether_aggregate"))
        .arg("--version")
        .output()
        .expect("spawn aether_aggregate");
    assert!(out.status.success(), "exit {:?}", out.status.code());
    let text = String::from_utf8(out.stdout).unwrap();
    let expected = format!("aether_aggregate {}", env!("CARGO_PKG_VERSION"));
    assert_eq!(text.trim(), expected);
}
