//! Diagnostic line sink (CONTRACT changelog 37).
//!
//! The converter prints per-tile (`[Rust] tile …`) and per-strip
//! (`[Strip k/n] …`) timing lines that locate where a slow run spends its
//! time. Consumers never parse them, but a GUI that mirrors every converter
//! line into its log panel drowns in them (920 tiles = 920 lines). When the
//! environment variable `AETHER_DIAG_FILE` names a path, those lines are
//! appended to that file instead of the console; when it is unset, they go
//! where they always went (stdout for ingest, stderr for download), so a
//! plain CLI run is unchanged.
//!
//! Only lines that are explicitly diagnostic go through here ([`out`],
//! [`err`]). Parsed lines (`[Rust] Progress:`, `[Tile]`, `[Download] <pct>%`,
//! `[Degraded]`, `[Stats] …`, `[Warn] …`) and the one-per-run summaries
//! (`[Rust] run: …`) stay on their streams.
//!
//! A download's progress record is the exception that is kept twice
//! (CONTRACT changelog 46): its `[Download] <pct>%` lines, the `[Download]
//! STALL …` / `[Download] first …` / `[Degraded] …` lines and the `[Stats]`
//! block stay on stderr exactly as before and, when `AETHER_DIAG_FILE` is
//! set, are ALSO appended to the file ([`err_kept`]) — a GUI that shows only
//! every tenth progress line loses nothing. So, since changelog 48, are the
//! `[Tile] <name> voids=<n>` lines of `download` (stderr, [`err_kept`]) and
//! of a batch `ingest` (stdout, [`keep`]). A download's per-tile
//! `[Backfill] …` lines (changelog 49) go to the file only ([`keep`]):
//! nothing when the variable is unset.

use std::io::Write;
use std::sync::{Mutex, OnceLock};

/// Name of the environment variable that redirects diagnostic lines.
pub const ENV_VAR: &str = "AETHER_DIAG_FILE";

enum Sink {
    /// Lines go to the console stream the caller names.
    Console,
    /// Lines are appended to the file `AETHER_DIAG_FILE` names.
    File(Mutex<std::io::BufWriter<std::fs::File>>),
}

fn sink() -> &'static Sink {
    static SINK: OnceLock<Sink> = OnceLock::new();
    SINK.get_or_init(|| {
        let path = match std::env::var_os(ENV_VAR) {
            Some(p) if !p.is_empty() => p,
            _ => return Sink::Console,
        };
        match std::fs::OpenOptions::new().create(true).append(true).open(&path) {
            Ok(f) => Sink::File(Mutex::new(std::io::BufWriter::new(f))),
            Err(e) => {
                // A file that cannot be opened must not take the run down:
                // say so once and fall back to the console.
                eprintln!("[Warn] {} = {:?} cannot be opened ({}); diagnostics go to the console", ENV_VAR, path, e);
                Sink::Console
            }
        }
    })
}

/// Whether diagnostic lines are being written to a file (so a caller may
/// skip building an expensive line when nobody will see it is NOT what this
/// means — the line is always kept somewhere; this only tells where).
pub fn to_file() -> bool {
    matches!(sink(), Sink::File(_))
}

fn write_file(w: &Mutex<std::io::BufWriter<std::fs::File>>, line: &str) {
    if let Ok(mut g) = w.lock() {
        let _ = g.write_all(line.as_bytes());
        let _ = g.write_all(b"\n");
        // Line-flushed so a killed process leaves everything it printed.
        let _ = g.flush();
    }
}

/// A diagnostic line whose console home is **stdout** (ingest timing lines).
pub fn out(line: &str) {
    match sink() {
        Sink::Console => println!("{line}"),
        Sink::File(w) => write_file(w, line),
    }
}

/// A diagnostic line whose console home is **stderr** (download strip lines).
pub fn err(line: &str) {
    match sink() {
        Sink::Console => eprintln!("{line}"),
        Sink::File(w) => write_file(w, line),
    }
}

/// A **parsed** stderr line that the diagnostic file keeps as well
/// (CONTRACT changelog 46): printed on stderr exactly as `eprintln!` prints
/// it, and — when `AETHER_DIAG_FILE` is set — appended to that file too,
/// unchanged (no prefix, line-flushed). Unset: just the stderr line.
///
/// Both writes happen under stderr's lock, so the file keeps the lines in
/// stderr's order even when several threads print. The sink is resolved
/// before the lock is taken (its first use may print a `[Warn]`). A caller
/// that already holds stderr's lock (the progress line, which takes its
/// count under it) must have resolved the sink before, e.g. by calling
/// [`to_file`] once at the start of the run — a lookup of the resolved sink
/// never blocks.
pub fn err_kept(line: &str) {
    let file = match sink() {
        Sink::File(w) => Some(w),
        Sink::Console => None,
    };
    let _in_order = std::io::stderr().lock();
    // `eprintln!` re-takes the (reentrant) lock this thread holds.
    eprintln!("{line}");
    if let Some(w) = file {
        write_file(w, line);
    }
}

/// A **parsed** line the caller prints on its console stream itself — under
/// that stream's lock, with the lines it belongs with — that the diagnostic
/// file keeps as well (CONTRACT changelog 48: a batch `ingest`'s `[Tile]
/// <name> voids=<n>` line, written on stdout with its covered and progress
/// lines): appended to the file unchanged (no prefix, line-flushed) when
/// `AETHER_DIAG_FILE` is set, else nothing. Called under the console lock,
/// the file keeps the console's order. The sink's first use may print a
/// `[Warn]` on stderr, never on stdout.
pub fn keep(line: &str) {
    if let Sink::File(w) = sink() {
        write_file(w, line);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn console_when_unset_is_the_default_shape() {
        // The sink is process-global; this only asserts the module's API
        // compiles and the console path does not panic. The file path is
        // covered by the CLI test in tests/test_diag_file.rs, which runs the
        // binary with the variable set.
        if std::env::var_os(ENV_VAR).is_none() {
            assert!(!to_file());
            out("[Rust] diag self-test line (stdout)");
            err("[Rust] diag self-test line (stderr)");
        }
    }
}
