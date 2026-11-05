//! A batch `ingest` that fetches its building tiles while it samples its
//! terrain: `buildings_pbf_ready` (CONTRACT §9a, changelog 48;
//! `aether_converter` 0.2.12).
//!
//! A batch decodes each `buildings_pbf_dir` once and, without a marker, does
//! so before its first tile — so a caller had to finish fetching the
//! building tiles before it could launch the ingest (measured on the owner's
//! machine: Site Analysis waited 6 s of a 27 s run, the Map Converter 39 s
//! before its 123 s converter run). With `buildings_pbf_ready` naming a
//! marker file inside the directory — the hand-over of the `download` job
//! (changelog 45), same rules — and the marker missing at launch, the batch:
//!
//! * starts every tile's terrain at once, as before;
//! * looks for the marker at launch and then every 250 ms (one watcher
//!   thread per directory, [`BuildingsHandOver::watch`]) and decodes the
//!   directory as soon as it is there — the loader and lines of a decode at
//!   launch, on a pool of the batch's size beside the batch's own, whose
//!   threads may be waiting for that very set;
//! * holds a tile whose terrain is finished before the set is decoded — its
//!   samples, in memory, not written — while the held tiles fit the hold
//!   budget ([`hold_budget`]); a tile that does not fit waits on its worker
//!   thread, which starts no other tile meanwhile;
//! * once the set is decoded, burns and writes the held tiles in the order
//!   they finished — as many drainer threads as the batch has workers, and
//!   every worker that comes free, take them from the front — and every
//!   later tile inline, as before;
//! * fails the run — exit 1, no tile of that directory written — when the
//!   marker does not come within `buildings_pbf_wait_s` or the directory does
//!   not decode: the held tiles are dropped, every tile not yet started is
//!   skipped and every one being sampled stops at its next band.
//!
//! The bytes are those of a batch whose directory was complete at launch:
//! the same loader over the same files gives the same building list, and a
//! held tile is burnt by the same code, only later and maybe on another
//! thread.

use super::*;
use std::collections::VecDeque;
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

/// Bytes per MB, as the converter counts them in its lines (MiB).
const MB: u64 = 1 << 20;
/// The ceiling of the default hold budget: 4096 MB.
pub const HOLD_CAP_BYTES: u64 = 4096 * MB;
/// The largest `buildings_hold_mb` a job may give (1 TiB): a larger number is
/// a unit mistake (bytes for megabytes), not a budget.
pub const HOLD_MB_MAX: f64 = 1_048_576.0;

/// A directory's hand-over: the marker's path and how long to wait for it.
pub type Marker = (PathBuf, Duration);

/// The hand-over of every `buildings_pbf_dir` of a batch (changelog 48):
/// `Some(marker)` for a directory whose jobs name a `buildings_pbf_ready`,
/// `None` for one decoded at launch as before.
///
/// Checked before any tile work, nothing read: every job's fields by the
/// `download` job's rules and messages (changelog 45) — a marker needs
/// `buildings_pbf_dir`, is one plain file name and no `{z}_{x}_{y}.pbf` tile
/// name, `buildings_pbf_wait_s` is 0 … 86400 s — and, with a marker,
/// `buildings_hold_mb` 0 … [`HOLD_MB_MAX`]; then the jobs naming one
/// directory must give the same marker and wait (the directory is decoded
/// once for the run). `buildings_pbf_wait_s` and `buildings_hold_mb` without
/// a marker are not read.
pub fn batch_markers(jobs: &[IngestJob]) -> Result<HashMap<PathBuf, Option<Marker>>> {
    let mut by_dir: HashMap<PathBuf, (usize, Option<Marker>)> = HashMap::new();
    for (i, job) in jobs.iter().enumerate() {
        let in_job = |e: anyhow::Error| anyhow::anyhow!("ingest job for {:?}: {e}", job.output_path);
        let marker = crate::download::buildings_ready_marker(
            job.buildings_pbf_ready.as_deref(),
            job.buildings_pbf_dir.as_deref(),
            job.buildings_pbf_wait_s,
        )
        .map_err(in_job)?;
        if marker.is_some() {
            if let Some(mb) = job.buildings_hold_mb {
                if !(mb.is_finite() && (0.0..=HOLD_MB_MAX).contains(&mb)) {
                    return Err(in_job(anyhow::anyhow!(
                        "buildings_hold_mb must be a number of megabytes from 0 to 1048576, got {mb}"
                    )));
                }
            }
        }
        let Some(dir) = &job.buildings_pbf_dir else { continue };
        match by_dir.get(dir) {
            None => {
                by_dir.insert(dir.clone(), (i, marker));
            }
            Some((first, theirs)) if *theirs != marker => {
                let say = |m: &Option<Marker>| match m {
                    Some((p, w)) => format!("marker {:?}, {} s", p.file_name().unwrap_or_default(), w.as_secs_f64()),
                    None => "no marker".to_owned(),
                };
                anyhow::bail!(
                    "buildings_pbf_dir {:?}: the jobs for {:?} and {:?} give it different \
                     buildings_pbf_ready / buildings_pbf_wait_s ({} against {}); the directory is \
                     decoded once for the run, so every job naming it must give the same",
                    dir,
                    jobs[*first].output_path,
                    job.output_path,
                    say(theirs),
                    say(&marker)
                );
            }
            Some(_) => {}
        }
    }
    Ok(by_dir.into_iter().map(|(dir, (_, m))| (dir, m)).collect())
}

/// The samples a held tile of *size_px* keeps: `size_px² × 2` bytes.
fn held_bytes_of(size_px: u32) -> u64 {
    u64::from(size_px) * u64::from(size_px) * 2
}

/// The default hold budget (changelog 48), from the figures the batch sizes
/// its pool with: half of the *available* memory left once the pool has its
/// reservation — *threads* × *per_tile* (each worker's tile estimate,
/// [`estimate_job_bytes`]) plus the shared chunk cache — at most
/// [`HOLD_CAP_BYTES`] (4096 MB), and never below *largest_tile* (one held
/// tile, so a batch can always hold one).
pub fn default_hold_bytes(available: u64, threads: usize, per_tile: u64, chunk_cache: u64, largest_tile: u64) -> u64 {
    let reserved = (threads as u64).saturating_mul(per_tile).saturating_add(chunk_cache);
    (available.saturating_sub(reserved) / 2).min(HOLD_CAP_BYTES).max(largest_tile)
}

/// The hold budget, in bytes, of a batch that watches the directories
/// *watched* (changelog 48): the smallest `buildings_hold_mb` their jobs
/// give — taken as given, `0` holding nothing — else [`default_hold_bytes`]
/// over the largest of their tiles.
pub fn hold_budget(jobs: &[IngestJob], watched: &[PathBuf], available: u64, threads: usize, per_tile: u64) -> u64 {
    let theirs = || jobs.iter().filter(|j| j.buildings_pbf_dir.as_ref().is_some_and(|d| watched.contains(d)));
    match theirs().filter_map(|j| j.buildings_hold_mb).reduce(f64::min) {
        Some(mb) => (mb * MB as f64) as u64,
        None => {
            let largest = theirs().map(|j| held_bytes_of(j.size_px)).max().unwrap_or(0);
            default_hold_bytes(available, threads, per_tile, DEFAULT_CHUNK_CACHE_BYTES as u64, largest)
        }
    }
}

/// A directory to watch.
pub struct Watched {
    pub dir: PathBuf,
    /// `<dir>/<buildings_pbf_ready>`.
    pub marker: PathBuf,
    /// `buildings_pbf_wait_s`.
    pub wait: Duration,
    /// The first look for the marker (at launch, where it was missing):
    /// the wait counts from here.
    pub first_look: Instant,
}

/// What a watched directory's building set is.
enum GateSet {
    /// The marker is not there yet, or the directory is being decoded.
    Pending,
    Ready(Arc<PbfBuildingSet>),
    /// The run failed (at this directory or another one).
    Failed,
}

/// One watched directory.
struct Gate {
    marker: PathBuf,
    set: GateSet,
    /// Its held tiles, oldest first.
    queue: VecDeque<Held>,
    /// When its first tile had to wait — the `[Buildings] waiting …` line.
    waiting_since: Option<Instant>,
    /// Tiles that went into the hold, and their bytes.
    held: usize,
    held_bytes: u64,
    /// Tiles that waited on their worker thread, the hold being full.
    blocked: usize,
}

/// A held tile: its terrain, and what reports it.
struct Held {
    tile: SampledTile,
    done: TileDone,
}

impl Held {
    /// What holding it costs: its samples.
    fn bytes(&self) -> u64 {
        self.tile.buffer.len() as u64 * 2
    }
}

struct State {
    gates: Vec<Gate>,
    /// Bytes of the held tiles of every gate.
    held_bytes: u64,
}

/// The `buildings_pbf_ready` hand-over of one batch (changelog 48): the
/// directories whose marker was missing at launch, the tiles held for them
/// and the hold budget. See the module documentation.
pub struct BuildingsHandOver {
    /// The watched directories, by gate index.
    dirs: Vec<PathBuf>,
    /// Their markers' paths, how long to wait, and the first look.
    watched: Vec<(PathBuf, Duration, Instant)>,
    /// Bytes the held tiles of all gates may take.
    budget: u64,
    /// Threads that burn and write the held tiles once a set is ready: the
    /// batch's pool size (its workers may all have returned by then).
    drainers: usize,
    /// Set when the run fails: every tile not yet started is skipped and
    /// every one being sampled stops at its next band.
    abort: AtomicBool,
    state: Mutex<State>,
    /// A gate became ready or the run failed.
    changed: Condvar,
}

/// See [`BuildingsHandOver::cancel_on_panic`].
pub struct CancelOnPanic<'a>(&'a BuildingsHandOver);

impl Drop for CancelOnPanic<'_> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.0.cancel();
        }
    }
}

/// One `[Buildings]` line on stdout, the stream of every ingest line,
/// flushed at once.
fn say(line: &str) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{line}");
    let _ = out.flush();
}

/// The line that reports a gate's hold once its set is ready.
fn held_line(held: usize, held_bytes: u64, blocked: usize, budget: u64) -> String {
    let mb = |b: u64| b as f64 / MB as f64;
    let mut line = format!(
        "[Buildings] {held} tile(s) held ({:.1} MB) until the building tiles were ready",
        mb(held_bytes)
    );
    if blocked > 0 {
        line += &format!("; {blocked} more waited on their worker threads (hold budget {:.1} MB full)", mb(budget));
    }
    line
}

/// Burn *held* with *set* and hand it to *writer*.
fn finish_and_submit(held: Held, set: &PbfBuildingSet, writer: &TileWriter) {
    let Held { tile, done } = held;
    match finish_tile(tile, Some(set)) {
        Ok(ready) => submit_ready(ready, writer, done),
        Err(e) => done(Err(e)),
    }
}

impl BuildingsHandOver {
    /// The hand-over of *watched* with a hold of *budget* bytes, whose held
    /// tiles are burnt by up to *drainers* threads at once (the batch's pool
    /// size).
    pub fn new(budget: u64, drainers: usize, watched: Vec<Watched>) -> Self {
        let gates = watched
            .iter()
            .map(|w| Gate {
                marker: w.marker.clone(),
                set: GateSet::Pending,
                queue: VecDeque::new(),
                waiting_since: None,
                held: 0,
                held_bytes: 0,
                blocked: 0,
            })
            .collect();
        BuildingsHandOver {
            dirs: watched.iter().map(|w| w.dir.clone()).collect(),
            watched: watched.into_iter().map(|w| (w.marker, w.wait, w.first_look)).collect(),
            budget,
            drainers: drainers.max(1),
            abort: AtomicBool::new(false),
            state: Mutex::new(State { gates, held_bytes: 0 }),
            changed: Condvar::new(),
        }
    }

    /// How many directories are watched (gate indices `0..gates()`).
    pub fn gates(&self) -> usize {
        self.dirs.len()
    }

    /// The gate of `buildings_pbf_dir` *dir*, when it is watched.
    pub fn gate_of(&self, dir: &Path) -> Option<usize> {
        self.dirs.iter().position(|d| d == dir)
    }

    /// Whether the run failed (a marker that did not come, a directory that
    /// did not decode): every tile still to start is to be skipped.
    pub fn aborted(&self) -> bool {
        self.abort.load(Ordering::Relaxed)
    }

    /// Stop the hand-over as if every gate had failed, without an error of
    /// its own: held tiles dropped, waiting workers released, watchers ended
    /// at their next look.
    pub fn cancel(&self) {
        for gate in 0..self.gates() {
            self.fail(gate);
        }
    }

    /// A guard that [cancels](Self::cancel) the hand-over when the thread
    /// holding it unwinds — a panicking tile or watcher must not leave the
    /// workers waiting for a set that will never come (they would wait out
    /// `buildings_pbf_wait_s`, and the panic with them).
    pub fn cancel_on_panic(&self) -> CancelOnPanic<'_> {
        CancelOnPanic(self)
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Watch gate *gate*'s marker, on a thread of its own, from the batch's
    /// launch to its end: look every 250 ms ([`crate::download::BUILDINGS_MARKER_POLL`])
    /// until the marker is a file, then *decode* the directory (it prints
    /// the decode's lines), release the tiles waiting for it — announcing,
    /// when one did wait, `[Buildings] building tiles ready after <s.s> s`
    /// (seconds since the `waiting` line) and the hold — and burn and write
    /// the held tiles, oldest first, on as many threads as the batch has
    /// (with the workers that come free). A marker still missing
    /// `buildings_pbf_wait_s` after the first look, or a decode that fails,
    /// fails the run with that error; nothing of this gate is written then.
    /// Returns `Ok` also when another gate failed the run.
    pub fn watch(
        &self,
        gate: usize,
        decode: &(dyn Fn(&Path) -> Result<PbfBuildingSet> + Sync),
        writer: &TileWriter,
    ) -> Result<()> {
        let _unwinding = self.cancel_on_panic();
        let (marker, wait, first_look) = self.watched[gate].clone();
        loop {
            if self.aborted() {
                self.fail(gate);
                return Ok(());
            }
            if crate::download::buildings_marker_present(&marker) {
                break;
            }
            let waited = first_look.elapsed();
            if waited >= wait {
                self.fail(gate);
                anyhow::bail!(
                    "Building tiles not ready: the marker file {} (buildings_pbf_ready) did not \
                     appear within {} s (buildings_pbf_wait_s); the ingest fails rather than write \
                     tiles without their buildings",
                    marker.display(),
                    wait.as_secs_f64()
                );
            }
            std::thread::sleep(crate::download::BUILDINGS_MARKER_POLL.min(wait - waited));
        }
        let set = match decode(&self.dirs[gate]) {
            Ok(set) => set,
            Err(e) => {
                self.fail(gate);
                return Err(e);
            }
        };
        {
            let mut st = self.lock();
            if self.aborted() {
                drop(st);
                self.fail(gate);
                return Ok(());
            }
            let g = &mut st.gates[gate];
            g.set = GateSet::Ready(Arc::new(set));
            // Under the lock: no tile of this gate is burnt before these
            // lines, and the `waiting` line (printed under it too) precedes them.
            if let Some(since) = g.waiting_since {
                say(&format!("[Buildings] building tiles ready after {:.1} s", since.elapsed().as_secs_f64()));
                say(&held_line(g.held, g.held_bytes, g.blocked, self.budget));
            }
            self.changed.notify_all();
        }
        // The held tiles, on the batch's thread count: when every tile was
        // held, every worker has returned and nothing else would burn them.
        std::thread::scope(|s| {
            for i in 1..self.drainers {
                let spawned = std::thread::Builder::new()
                    .name(format!("aether-held-{i}"))
                    .spawn_scoped(s, || self.drain(writer));
                if spawned.is_err() {
                    break; // fewer drainers — never a failure
                }
            }
            self.drain(writer);
        });
        Ok(())
    }

    /// One tile of gate *gate*, on a batch worker: its terrain at once, then
    /// its buildings — inline when the set is ready, else held (or, the hold
    /// being full, waited for on this thread). *done* reports the tile as in
    /// [`process_tile_batched`]; a tile dropped because the run failed is
    /// never reported (the run reports its error). Afterwards, held tiles of
    /// a ready gate are taken from the front and finished here too.
    pub fn process(
        &self,
        gate: usize,
        job: IngestJob,
        cache_arc: ImageCache,
        opts: &IngestOptions,
        writer: &TileWriter,
        done: TileDone,
    ) {
        if self.aborted() {
            return;
        }
        let spare = writer.spare_buffer();
        match sample_terrain(job, cache_arc, opts, spare, Some(&self.abort)) {
            Ok(tile) => self.offer(gate, Held { tile, done }, writer),
            // Stopped because the batch failed: the run reports that.
            Err(_) if self.aborted() => return,
            Err(e) => return done(Err(e)),
        }
        self.drain(writer);
    }

    /// A tile whose terrain is done: burnt and written now when its gate is
    /// ready; held while the hold has room; else waited for on this thread
    /// until the gate is ready (then the held tiles go first) or the run
    /// fails (then it is dropped).
    fn offer(&self, gate: usize, held: Held, writer: &TileWriter) {
        let bytes = held.bytes();
        let mut held = Some(held);
        let mut waited = false;
        let mut st = self.lock();
        let set = loop {
            if self.aborted() {
                return;
            }
            let State { gates, held_bytes } = &mut *st;
            let g = &mut gates[gate];
            match &g.set {
                GateSet::Ready(set) => break set.clone(),
                GateSet::Failed => return,
                GateSet::Pending => {}
            }
            if g.waiting_since.is_none() {
                g.waiting_since = Some(Instant::now());
                say(&format!("[Buildings] waiting for the building tiles ({})", g.marker.display()));
            }
            if *held_bytes + bytes <= self.budget {
                *held_bytes += bytes;
                g.held += 1;
                g.held_bytes += bytes;
                g.queue.push_back(held.take().expect("a tile is offered once"));
                return;
            }
            if !waited {
                g.blocked += 1;
                waited = true;
            }
            st = self.changed.wait(st).unwrap_or_else(PoisonError::into_inner);
        };
        drop(st);
        // This tile finished after every held one: those go first.
        if waited {
            self.drain(writer);
        }
        finish_and_submit(held.take().expect("not held"), &set, writer);
    }

    /// Burn and write held tiles of ready gates, oldest first, until none is
    /// left (or the run failed).
    fn drain(&self, writer: &TileWriter) {
        loop {
            let (set, held) = {
                let mut st = self.lock();
                if self.aborted() {
                    return;
                }
                let State { gates, held_bytes } = &mut *st;
                let mut next = None;
                for g in gates.iter_mut() {
                    if let GateSet::Ready(set) = &g.set {
                        if let Some(h) = g.queue.pop_front() {
                            next = Some((set.clone(), h));
                            break;
                        }
                    }
                }
                let Some((set, held)) = next else { return };
                *held_bytes -= held.bytes();
                (set, held)
            };
            finish_and_submit(held, &set, writer);
        }
    }

    /// The run fails at gate *gate*: it is failed, every held tile of every
    /// gate is dropped — never written, never reported — every waiting
    /// worker released (its tile dropped), and [`Self::aborted`] is set.
    fn fail(&self, gate: usize) {
        let dropped: Vec<Held> = {
            let mut st = self.lock();
            self.abort.store(true, Ordering::Relaxed);
            st.gates[gate].set = GateSet::Failed;
            st.held_bytes = 0;
            let all = st.gates.iter_mut().flat_map(|g| g.queue.drain(..)).collect();
            self.changed.notify_all();
            all
        };
        // Freed outside the lock.
        drop(dropped);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(out: &str, dir: Option<&str>, ready: Option<&str>) -> IngestJob {
        let mut j: IngestJob = serde_json::from_value(serde_json::json!({
            "output_path": out, "ul_lat": 47.0, "ul_lon": 8.0, "resolution_m": 10.0, "size_px": 1000,
        }))
        .unwrap();
        j.buildings_pbf_dir = dir.map(PathBuf::from);
        j.buildings_pbf_ready = ready.map(str::to_owned);
        j
    }

    #[test]
    fn the_default_hold_is_half_of_what_the_pool_leaves_capped_and_never_below_one_tile() {
        const GB: u64 = 1 << 30;
        // 17 GB available, 16 threads of ~254 MB (a 2 m tile), the 192 MB
        // cache: (17 GB − 4.16 GB) / 2 = 6.4 GB → the 4096 MB cap.
        assert_eq!(default_hold_bytes(17 * GB, 16, 254 * MB, 192 * MB, 62 * MB), HOLD_CAP_BYTES);
        // 2 GB available, 5 such threads: (2048 − 1270 − 192) / 2 = 293 MB.
        assert_eq!(default_hold_bytes(2 * GB, 5, 254 * MB, 192 * MB, 62 * MB), 293 * MB);
        // Nothing left: one tile all the same.
        assert_eq!(default_hold_bytes(GB, 8, 254 * MB, 192 * MB, 62 * MB), 62 * MB);
        assert_eq!(default_hold_bytes(0, 1, u64::MAX, u64::MAX, 7), 7, "no overflow");
    }

    #[test]
    fn the_hold_budget_is_the_smallest_given_else_the_default_over_the_watched_tiles() {
        let mut a = job("a.abt", Some("pbf"), Some("_ready"));
        let mut b = job("b.abt", Some("pbf"), Some("_ready"));
        let mut c = job("c.abt", Some("other"), Some("_ready"));
        c.size_px = 4000;
        let watched = [PathBuf::from("pbf")];
        // No value: the default, never below the largest WATCHED tile (1000² × 2).
        let jobs = [a.clone(), b.clone(), c.clone()];
        assert_eq!(hold_budget(&jobs, &watched, 0, 1, 0), 2_000_000);
        // Given: the smallest, taken as given (0 holds nothing); an
        // unwatched job's value does not count.
        a.buildings_hold_mb = Some(2.5);
        b.buildings_hold_mb = Some(0.5);
        c.buildings_hold_mb = Some(0.0);
        assert_eq!(hold_budget(&[a.clone(), b.clone(), c.clone()], &watched, 0, 1, 0), MB / 2);
        b.buildings_hold_mb = Some(0.0);
        assert_eq!(hold_budget(&[a, b, c], &watched, 0, 1, 0), 0);
    }

    #[test]
    fn markers_are_checked_per_job_and_must_agree_per_directory() {
        let m = batch_markers(&[job("a.abt", Some("pbf"), Some("_ready")), job("b.abt", None, None)]).unwrap();
        assert_eq!(m.len(), 1);
        assert_eq!(m[Path::new("pbf")], Some((Path::new("pbf").join("_ready"), Duration::from_secs(1800))));

        let err = |jobs: &[IngestJob]| batch_markers(jobs).unwrap_err().to_string();
        let e = err(&[job("a.abt", None, Some("_ready"))]);
        assert!(e.starts_with("ingest job for \"a.abt\": buildings_pbf_ready \"_ready\" needs buildings_pbf_dir"), "{e}");
        let e = err(&[job("a.abt", Some("pbf"), Some("14_1_2.pbf"))]);
        assert!(e.contains("is named like a building tile"), "{e}");
        let mut w = job("a.abt", Some("pbf"), Some("_ready"));
        w.buildings_pbf_wait_s = Some(-1.0);
        assert!(err(&[w]).contains("buildings_pbf_wait_s must be a number of seconds from 0 to 86400, got -1"));
        let mut h = job("a.abt", Some("pbf"), Some("_ready"));
        h.buildings_hold_mb = Some(f64::NAN);
        assert!(err(&[h]).contains("buildings_hold_mb must be a number of megabytes from 0 to 1048576, got NaN"));
        // Without a marker neither is read.
        let mut quiet = job("a.abt", Some("pbf"), None);
        quiet.buildings_pbf_wait_s = Some(-1.0);
        quiet.buildings_hold_mb = Some(-1.0);
        assert_eq!(batch_markers(&[quiet]).unwrap()[Path::new("pbf")], None);

        // One directory, two hand-overs: refused.
        let e = err(&[job("a.abt", Some("pbf"), Some("_ready")), job("b.abt", Some("pbf"), None)]);
        assert!(e.starts_with("buildings_pbf_dir \"pbf\": the jobs for \"a.abt\" and \"b.abt\" give it different"), "{e}");
        let mut late = job("b.abt", Some("pbf"), Some("_ready"));
        late.buildings_pbf_wait_s = Some(60.0);
        let e = err(&[job("a.abt", Some("pbf"), Some("_ready")), late]);
        assert!(e.contains("(marker \"_ready\", 1800 s against marker \"_ready\", 60 s)"), "{e}");
    }

    fn one_gate(marker: PathBuf, wait: Duration) -> BuildingsHandOver {
        let dir = marker.parent().unwrap().to_path_buf();
        BuildingsHandOver::new(0, 1, vec![Watched { dir, marker, wait, first_look: Instant::now() }])
    }

    #[test]
    fn a_marker_that_never_comes_fails_its_watcher_and_aborts_the_batch() {
        let tmp = tempfile::tempdir().unwrap();
        let marker = tmp.path().join("pbf").join("_ready");
        // The wait counts from the first look, made when the gate is set up.
        let t = Instant::now();
        let h = one_gate(marker.clone(), Duration::from_millis(300));
        let writer = TileWriter::new(1, 1);
        let decode = |_: &Path| -> Result<PbfBuildingSet> { panic!("no marker, no decode") };
        let e = h.watch(0, &decode, &writer).unwrap_err().to_string();
        let waited = t.elapsed();
        assert!(waited >= Duration::from_millis(300) && waited < Duration::from_secs(5), "{waited:?}");
        assert_eq!(
            e,
            format!(
                "Building tiles not ready: the marker file {} (buildings_pbf_ready) did not appear \
                 within 0.3 s (buildings_pbf_wait_s); the ingest fails rather than write tiles \
                 without their buildings",
                marker.display()
            )
        );
        assert!(h.aborted(), "every tile still to start is skipped");
    }

    #[test]
    fn a_panic_under_the_guard_cancels_the_hand_over_and_a_quiet_drop_does_not() {
        let tmp = tempfile::tempdir().unwrap();
        let h = one_gate(tmp.path().join("_ready"), Duration::from_secs(1800));
        drop(h.cancel_on_panic());
        assert!(!h.aborted());
        std::thread::scope(|s| {
            let r = s
                .spawn(|| {
                    let _unwinding = h.cancel_on_panic();
                    panic!("a tile's bug (expected by this test)");
                })
                .join();
            assert!(r.is_err());
        });
        assert!(h.aborted(), "workers waiting for the set are released");
        // A watcher of a cancelled batch ends at its next look, without an
        // error of its own (the panic is the run's error).
        let writer = TileWriter::new(1, 1);
        let decode = |_: &Path| -> Result<PbfBuildingSet> { panic!("cancelled, no decode") };
        assert!(h.watch(0, &decode, &writer).is_ok());
    }

    #[test]
    fn the_held_line_names_the_waiting_workers_only_when_there_were_some() {
        assert_eq!(
            held_line(3, 3 * 32768, 0, MB),
            "[Buildings] 3 tile(s) held (0.1 MB) until the building tiles were ready"
        );
        assert_eq!(
            held_line(1, 32768, 4, 40000),
            "[Buildings] 1 tile(s) held (0.0 MB) until the building tiles were ready; \
             4 more waited on their worker threads (hold budget 0.0 MB full)"
        );
    }
}
