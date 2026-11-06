//! Progress within one GIF, which is also where a stop request or an
//! excluded folder cancels it.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::Result;

use crate::output::{Event, Output};

/// How often progress events are sent at most, per file.
const INTERVAL: Duration = Duration::from_millis(100);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    /// Decoding frames; the total is not known yet.
    Reading,
    /// Grouping, planning, encoding and verifying the candidates.
    Optimizing,
    /// Running gifsicle and verifying its output.
    Gifsicle,
}

impl Stage {
    pub fn label(self) -> &'static str {
        match self {
            Stage::Reading => "reading",
            Stage::Optimizing => "optimizing",
            Stage::Gifsicle => "gifsicle",
        }
    }
}

/// Requests made while a run is in progress, shared by all its files.
pub struct Control {
    stop: AtomicBool,
    /// Folders whose GIFs are skipped, including those in subfolders.
    excluded: Mutex<Vec<PathBuf>>,
    /// Bumped on every exclusion, so files only look at the list when it
    /// has changed.
    generation: AtomicUsize,
}

impl Control {
    pub const fn new() -> Self {
        Self {
            stop: AtomicBool::new(false),
            excluded: Mutex::new(Vec::new()),
            generation: AtomicUsize::new(0),
        }
    }

    /// Cancels the files in progress and starts no more.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }

    pub fn stopped(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }

    /// Cancels the files in progress in `dir` (or below it) and skips the
    /// ones not started yet.
    pub fn exclude(&self, dir: PathBuf) {
        self.excluded
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(dir);
        self.generation.fetch_add(1, Ordering::Relaxed);
    }

    pub fn is_excluded(&self, file: &Path) -> bool {
        self.excluded
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .any(|dir| file.starts_with(dir))
    }
}

/// Why a file was cancelled; returned as an error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cancelled {
    Stopped,
    /// Its folder was excluded.
    Excluded,
}

impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Cancelled::Stopped => "stopped",
            Cancelled::Excluded => "folder excluded",
        })
    }
}

impl std::error::Error for Cancelled {}

/// Why `e` (or anything it wraps) cancelled a file, if it did.
pub fn cancelled(e: &anyhow::Error) -> Option<Cancelled> {
    e.chain()
        .find_map(|c| c.downcast_ref::<Cancelled>().copied())
}

struct State {
    stage: Stage,
    done: usize,
    total: usize,
    sent: Option<Instant>,
}

/// Counts units of work (mostly frames) in the current stage. Parallel
/// tasks share one counter: each reserves its work with `expect` up front
/// and returns what it did not need with `skip`, so the fraction only grows.
pub struct Progress<'a> {
    control: &'a Control,
    /// The file, to notice when its folder is excluded.
    file: &'a Path,
    /// The `Control::generation` the exclusions were last checked at.
    seen: AtomicUsize,
    excluded: AtomicBool,
    /// Where to report, and the file's index in the run.
    report: Option<(&'a Output, usize)>,
    state: Mutex<State>,
}

impl<'a> Progress<'a> {
    pub fn new(control: &'a Control, file: &'a Path, out: &'a Output, index: usize) -> Self {
        Self::with_report(control, file, Some((out, index)))
    }

    /// Only checks for cancellation.
    #[cfg(test)]
    pub fn silent(control: &'a Control, file: &'a Path) -> Self {
        Self::with_report(control, file, None)
    }

    fn with_report(
        control: &'a Control,
        file: &'a Path,
        report: Option<(&'a Output, usize)>,
    ) -> Self {
        Self {
            control,
            file,
            // Exclusions made before the file started are checked by the
            // caller, so only later ones matter.
            seen: AtomicUsize::new(control.generation.load(Ordering::Relaxed)),
            excluded: AtomicBool::new(false),
            report,
            state: Mutex::new(State {
                stage: Stage::Reading,
                done: 0,
                total: 0,
                sent: None,
            }),
        }
    }

    /// Starts `stage` with nothing done or expected yet.
    pub fn stage(&self, stage: Stage) -> Result<()> {
        self.check()?;
        self.update(true, |s| {
            *s = State {
                stage,
                done: 0,
                total: 0,
                sent: s.sent,
            }
        });
        Ok(())
    }

    /// Adds `n` units to the work expected in this stage.
    pub fn expect(&self, n: usize) {
        self.update(false, |s| s.total += n);
    }

    /// Counts `n` expected units as done without doing them.
    pub fn skip(&self, n: usize) {
        self.update(false, |s| s.done += n);
    }

    /// Counts one unit of work as done; fails if a stop was requested.
    pub fn tick(&self) -> Result<()> {
        self.advance(1)
    }

    /// Counts `n` units of work as done; fails if a stop was requested.
    pub fn advance(&self, n: usize) -> Result<()> {
        self.check()?;
        self.update(false, |s| s.done += n);
        Ok(())
    }

    /// Fails if the file was cancelled.
    pub fn check(&self) -> Result<()> {
        if self.control.stopped() {
            return Err(Cancelled::Stopped.into());
        }
        let generation = self.control.generation.load(Ordering::Relaxed);
        if self.seen.swap(generation, Ordering::Relaxed) != generation
            && self.control.is_excluded(self.file)
        {
            self.excluded.store(true, Ordering::Relaxed);
        }
        if self.excluded.load(Ordering::Relaxed) {
            return Err(Cancelled::Excluded.into());
        }
        Ok(())
    }

    fn update(&self, force: bool, f: impl FnOnce(&mut State)) {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        f(&mut s);
        let Some((out, index)) = self.report else {
            return;
        };
        let now = Instant::now();
        if force || s.sent.is_none_or(|t| now.duration_since(t) >= INTERVAL) {
            s.sent = Some(now);
            out.event(Event::Progress {
                index,
                stage: s.stage,
                done: s.done,
                total: s.total,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::Sink;

    #[test]
    fn ticks_fail_after_stop() {
        let control = Control::new();
        let p = Progress::silent(&control, Path::new("a.gif"));
        p.tick().unwrap();
        control.stop();
        let e = p.tick().unwrap_err().context("while encoding");
        assert_eq!(cancelled(&e), Some(Cancelled::Stopped));
        assert_eq!(cancelled(&anyhow::anyhow!("other")), None);
    }

    #[test]
    fn excluding_a_folder_cancels_only_its_files() {
        let control = Control::new();
        let inside = Progress::silent(&control, Path::new("root/sub/deep/a.gif"));
        let outside = Progress::silent(&control, Path::new("root/subway/b.gif"));
        control.exclude(PathBuf::from("root/sub"));
        let e = inside.tick().unwrap_err();
        assert_eq!(cancelled(&e), Some(Cancelled::Excluded));
        assert!(inside.check().is_err(), "stays cancelled");
        outside.tick().unwrap();
        assert!(control.is_excluded(Path::new("root/sub/c.gif")));
        assert!(!control.is_excluded(Path::new("root/c.gif")));
    }

    #[test]
    fn stage_changes_are_always_reported() {
        let (tx, rx) = std::sync::mpsc::channel();
        let out = Output::new(Sink::Channel(tx));
        let control = Control::new();
        let p = Progress::new(&control, Path::new("a.gif"), &out, 7);
        p.stage(Stage::Reading).unwrap();
        for _ in 0..50 {
            p.tick().unwrap();
        }
        p.stage(Stage::Optimizing).unwrap();
        p.expect(4);
        let events: Vec<_> = rx.try_iter().collect();
        assert!(events.len() < 10, "progress is throttled");
        assert!(matches!(
            events.last(),
            Some(Event::Progress {
                index: 7,
                stage: Stage::Optimizing,
                done: 0,
                total: 0
            })
        ));
    }
}
