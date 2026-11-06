//! Progress within one GIF, which is also where a stop request cancels it.

use std::sync::atomic::{AtomicBool, Ordering};
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

/// The error returned when a file is cancelled by a stop request.
#[derive(Debug)]
pub struct Stopped;

impl std::fmt::Display for Stopped {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("stopped")
    }
}

impl std::error::Error for Stopped {}

/// Whether `e` (or anything it wraps) is a stop request.
pub fn is_stopped(e: &anyhow::Error) -> bool {
    e.chain().any(|c| c.is::<Stopped>())
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
    stop: &'a AtomicBool,
    /// Where to report, and the file's index in the run.
    report: Option<(&'a Output, usize)>,
    state: Mutex<State>,
}

impl<'a> Progress<'a> {
    pub fn new(stop: &'a AtomicBool, out: &'a Output, index: usize) -> Self {
        Self::with_report(stop, Some((out, index)))
    }

    /// Only checks for stop requests.
    #[cfg(test)]
    pub fn silent(stop: &'a AtomicBool) -> Self {
        Self::with_report(stop, None)
    }

    fn with_report(stop: &'a AtomicBool, report: Option<(&'a Output, usize)>) -> Self {
        Self {
            stop,
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
        self.check()?;
        self.update(false, |s| s.done += 1);
        Ok(())
    }

    pub fn check(&self) -> Result<()> {
        if self.stop.load(Ordering::Relaxed) {
            Err(Stopped.into())
        } else {
            Ok(())
        }
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
        let stop = AtomicBool::new(false);
        let p = Progress::silent(&stop);
        p.tick().unwrap();
        stop.store(true, Ordering::Relaxed);
        let e = p.tick().unwrap_err().context("while encoding");
        assert!(is_stopped(&e));
        assert!(!is_stopped(&anyhow::anyhow!("other")));
    }

    #[test]
    fn stage_changes_are_always_reported() {
        let (tx, rx) = std::sync::mpsc::channel();
        let out = Output::new(Sink::Channel(tx));
        let stop = AtomicBool::new(false);
        let p = Progress::new(&stop, &out, 7);
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
