//! This process's resource usage, as the OS reports it, for the end-of-run
//! lines a person reads in a log — never parsed by a consumer: the CPU time
//! and page faults of `ingest`'s `[Rust] run:` line (CONTRACT changelog 31,
//! 37) and the peak resident memory of that line and of the CLI `download`'s
//! `[Stats] Peak RSS:` line (changelog 50). One reader for both commands.
//!
//! The peak resident memory, by platform:
//! - **Linux** (and Android): `VmHWM` of `/proc/self/status`, the high-water
//!   mark of this process image's resident set. Not `getrusage`'s
//!   `ru_maxrss`: Linux carries the parent's peak into a child's `ru_maxrss`
//!   at `exec` — after `fork`, `vfork` and `posix_spawn` alike — so a
//!   converter started by a process that had held 1.5 GB reported that 1.5 GB
//!   whatever it used itself (measured, changelog 50).
//! - **macOS**: `ru_maxrss`, which macOS gives in bytes; the other Unixes (the
//!   BSDs) give it in kilobytes.
//! - **Windows**: `PeakWorkingSetSize` of `GetProcessMemoryInfo`.
//! - Elsewhere — the wasm32 browser build, which has no OS process — none.
//!
//! A value the OS did not give is `None`: the line omits the field, it never
//! prints 0.

/// CPU time, page faults and peak resident memory of this process.
pub struct ProcUsage {
    /// CPU time in user mode, seconds (0 when unknown).
    pub user_s: f64,
    /// CPU time in kernel mode, seconds (0 when unknown).
    pub sys_s: f64,
    /// Minor + major page faults (Windows: `PageFaultCount`; 0 when unknown).
    pub page_faults: u64,
    /// The peak resident set so far, in bytes; `None` when the OS did not
    /// say.
    pub peak_rss: Option<u64>,
}

#[cfg(unix)]
pub fn read() -> ProcUsage {
    // SAFETY: getrusage fills the zeroed struct it is handed; on failure it
    // stays zeroed and reads as 0.
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut ru) };
    let secs = |t: libc::timeval| t.tv_sec as f64 + t.tv_usec as f64 / 1e6;
    ProcUsage {
        user_s: secs(ru.ru_utime),
        sys_s: secs(ru.ru_stime),
        // Minor + major, the same count Windows' PageFaultCount reports.
        page_faults: (ru.ru_minflt as u64).saturating_add(ru.ru_majflt as u64),
        peak_rss: peak_rss(&ru),
    }
}

/// Linux: `VmHWM`, not `ru_maxrss` (the module docs say why).
#[cfg(any(target_os = "linux", target_os = "android"))]
fn peak_rss(_ru: &libc::rusage) -> Option<u64> {
    vm_hwm_bytes(&std::fs::read_to_string("/proc/self/status").ok()?)
}

/// The other Unixes: `ru_maxrss` — bytes on macOS, kilobytes elsewhere. 0
/// (also what a failed `getrusage` leaves) is unknown.
#[cfg(all(unix, not(any(target_os = "linux", target_os = "android"))))]
fn peak_rss(ru: &libc::rusage) -> Option<u64> {
    let v = u64::try_from(ru.ru_maxrss).ok().filter(|&v| v > 0)?;
    if cfg!(target_os = "macos") {
        Some(v)
    } else {
        v.checked_mul(1024)
    }
}

/// The `VmHWM:` value of a `/proc/<pid>/status` text (`VmHWM:\t    8576 kB`)
/// in bytes; `None` when the line is missing, not in kB (KiB) or 0.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn vm_hwm_bytes(status: &str) -> Option<u64> {
    let kb = status.lines().find_map(|l| l.strip_prefix("VmHWM:"))?.trim().strip_suffix("kB")?;
    kb.trim().parse::<u64>().ok()?.checked_mul(1024).filter(|&b| b > 0)
}

#[cfg(windows)]
pub fn read() -> ProcUsage {
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};
    // FILETIME durations are 100 ns units.
    let secs = |f: FILETIME| ((u64::from(f.dwHighDateTime) << 32) | u64::from(f.dwLowDateTime)) as f64 / 1e7;
    // SAFETY: plain Win32 queries on the current-process pseudo handle (never
    // closed) into zeroed out-structs sized as the API requires.
    unsafe {
        let me = GetCurrentProcess();
        let (mut created, mut exited, mut kernel, mut user): (FILETIME, FILETIME, FILETIME, FILETIME) =
            (std::mem::zeroed(), std::mem::zeroed(), std::mem::zeroed(), std::mem::zeroed());
        let times = GetProcessTimes(me, &mut created, &mut exited, &mut kernel, &mut user) != 0;
        let mut mem: PROCESS_MEMORY_COUNTERS = std::mem::zeroed();
        mem.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
        let have_mem = GetProcessMemoryInfo(me, &mut mem, mem.cb) != 0;
        ProcUsage {
            user_s: if times { secs(user) } else { 0.0 },
            sys_s: if times { secs(kernel) } else { 0.0 },
            page_faults: if have_mem { u64::from(mem.PageFaultCount) } else { 0 },
            // The peak working set: Windows' peak resident memory, in bytes.
            peak_rss: if have_mem && mem.PeakWorkingSetSize > 0 { Some(mem.PeakWorkingSetSize as u64) } else { None },
        }
    }
}

#[cfg(not(any(unix, windows)))]
pub fn read() -> ProcUsage {
    ProcUsage { user_s: 0.0, sys_s: 0.0, page_faults: 0, peak_rss: None }
}

/// *bytes* in MB — 2^20 bytes, the `MB` of every converter line (`[Download]
/// RAM budget: <mb> MB` is the budget `>> 20`) — with one decimal: `4123.5`.
pub fn mb(bytes: u64) -> String {
    format!("{:.1}", bytes as f64 / (1u64 << 20) as f64)
}

/// The machine's RAM in bytes: the ceiling a plausible peak stays under
/// (the unit tests here, in `ingest.rs` and in `download.rs`).
#[cfg(all(test, feature = "native"))]
pub(crate) fn total_ram() -> u64 {
    let mut sys = sysinfo::System::new();
    sys.refresh_memory();
    sys.total_memory()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn megabytes_are_mebibytes_with_one_decimal() {
        assert_eq!(mb(4_323_803_136), "4123.5");
        assert_eq!(mb(2048 << 20), "2048.0");
        assert_eq!(mb(1 << 20), "1.0");
    }

    #[cfg(all(feature = "native", any(unix, windows)))]
    #[test]
    fn this_platform_reports_a_peak_above_what_the_process_touched() {
        // 64 MiB written byte by byte are resident, so the peak passes them.
        let touched = vec![1u8; 64 << 20];
        std::hint::black_box(&touched);
        let peak = read().peak_rss.expect("Linux, macOS and Windows report a peak RSS");
        assert!(peak > 64 << 20, "peak {} MB below the 64 MB this process touched", mb(peak));
        assert!(peak < total_ram(), "peak {} MB above the machine's RAM", mb(peak));
        drop(touched);
        // A peak: what was freed stays counted.
        assert!(read().peak_rss.unwrap() >= peak);
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn vm_hwm_is_read_from_the_status_text_in_kib() {
        let status = "Name:\taether_converter\nVmPeak:\t  912340 kB\nVmHWM:\t    8576 kB\nVmRSS:\t    8448 kB\n";
        assert_eq!(vm_hwm_bytes(status), Some(8576 * 1024));
        for unknown in ["VmRSS:\t 8448 kB\n", "VmHWM:\t 0 kB\n", "VmHWM:\t 8576 MB\n", "VmHWM:\t kB\n", ""] {
            assert_eq!(vm_hwm_bytes(unknown), None, "{unknown:?}");
        }
    }
}
