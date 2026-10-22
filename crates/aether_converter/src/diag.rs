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
//! Only lines that are explicitly diagnostic go through here. Parsed lines
//! (`[Rust] Progress:`, `[Tile]`, `[Download] <pct>%`, `[Degraded]`,
//! `[Stats] …`, `[Warn] …`) and the one-per-run summaries
//! (`[Rust] run: …`) stay on their streams.

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
