//! Adds the files a previous run reported in its log to the record, for
//! results that were moved somewhere the record does not know about, such
//! as over their originals.
//!
//! The log names each file and the size of its result, rounded (for example
//! `69.98 KB`). A file is only added if it is still where the run left it,
//! or where it came from, with a size that matches the result and a modified
//! time no later than the end of that run. Each added file is read once to
//! record its content hash.

use std::collections::HashMap;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};

use crate::output::{parse_utc, Kind, Output};
use crate::record::{self, Entry, Mode, Record};
use crate::{Command, OUTPUT_DIR};

/// Slack for the modified-time check, for clocks and file systems that
/// round times.
const SLACK_SECS: u64 = 120;

/// What a run reported about one file.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Expect {
    /// The result's size: about `bytes`, give or take `tolerance`.
    Size { bytes: f64, tolerance: f64 },
    /// The original was kept as it was; its size was not logged.
    Kept,
}

#[derive(Clone, Debug, PartialEq)]
struct Reported {
    path: PathBuf,
    /// Where the run wrote it, if not over `path` itself.
    output: Option<PathBuf>,
    expect: Expect,
    mode: Mode,
    /// When the run ended, in seconds since the Unix epoch.
    deadline: u64,
}

/// One run in the log.
#[derive(Default)]
struct Run {
    start: u64,
    end: Option<u64>,
    base: Option<PathBuf>,
    replace: bool,
    mode: Option<Mode>,
    /// Name relative to `base` and what was reported.
    files: Vec<(String, Expect)>,
}

/// The file whose report lines are being read: its name, its result so
/// far, and whether it was repaired.
type Current = Option<(String, Option<Expect>, bool)>;

/// Adds the file being read to `run`, if it had a usable result.
fn finish(run: Option<&mut Run>, current: &mut Current) {
    if let (Some(run), Some((name, Some(expect), _))) = (run, current.take()) {
        run.files.push((name, expect));
    }
}

/// The latest report for every file in `log`, and the number of runs that
/// could not be used because they were given paths rather than a folder.
fn parse(log: &str, now: u64) -> (Vec<Reported>, usize) {
    let mut runs: Vec<Run> = Vec::new();
    let mut current: Current = None;
    for line in log.lines() {
        let line = line.trim_end_matches('\r');
        if let Some(header) = line.strip_prefix("===== ") {
            finish(runs.last_mut(), &mut current);
            let start = header.get(..19).and_then(parse_utc).unwrap_or(0);
            let command = header
                .split_once(" UTC: ")
                .map_or("", |(_, c)| c.trim_end_matches(" ====="));
            let options = command_options(command);
            runs.push(Run {
                start,
                replace: options.as_ref().is_some_and(|o| o.replace),
                mode: options.map(|o| o.mode()),
                ..Run::default()
            });
            continue;
        }
        let Some(run) = runs.last_mut() else { continue };
        if run.base.is_none() {
            if let Some(base) = line.strip_prefix("GIF Optimizer - ") {
                if !base.ends_with(" path(s) given") {
                    run.base = Some(PathBuf::from(base));
                }
                continue;
            }
        }
        if let Some(secs) = line
            .strip_prefix("Finished in ")
            .and_then(|s| s.strip_suffix('s'))
            .and_then(|s| s.parse::<f64>().ok())
        {
            run.end = Some(run.start + secs.ceil() as u64);
        }
        if line.starts_with('[') {
            if let Some((counter, name)) = line.split_once("] ") {
                if counter[1..].split_once('/').is_some() {
                    finish(Some(run), &mut current);
                    current = Some((name.to_string(), None, false));
                    continue;
                }
            }
        }
        let Some((_, expect, repaired)) = &mut current else {
            continue;
        };
        let Some(detail) = line.strip_prefix("    ") else {
            finish(Some(run), &mut current);
            continue;
        };
        if detail.starts_with("repaired:") {
            *repaired = true;
        } else if let Some(size) = detail.strip_prefix("size:") {
            *expect = result_size(size);
        } else if detail.starts_with("kept original file:") && !*repaired {
            *expect = Some(Expect::Kept);
        }
    }
    finish(runs.last_mut(), &mut current);

    let mut latest: HashMap<PathBuf, Reported> = HashMap::new();
    let mut unusable = 0;
    for (i, run) in runs.iter().enumerate() {
        let Some(base) = &run.base else {
            unusable += usize::from(!run.files.is_empty());
            continue;
        };
        let deadline = run
            .end
            .or_else(|| runs.get(i + 1).map(|r| r.start))
            .unwrap_or(now);
        for (name, expect) in &run.files {
            let path = base.join(name);
            let output = (!run.replace).then(|| base.join(OUTPUT_DIR).join(name));
            latest.insert(
                path.clone(),
                Reported {
                    path,
                    output,
                    expect: *expect,
                    // Unknown options: assume lossless, so that a run that
                    // may merge look-alikes still tries these files.
                    mode: run.mode.unwrap_or(Mode::Lossless),
                    deadline,
                },
            );
        }
    }
    let mut reported: Vec<Reported> = latest.into_values().collect();
    reported.sort_by(|a, b| a.path.cmp(&b.path));
    (reported, unusable)
}

/// The options in a logged command line; paths are ignored.
fn command_options(command: &str) -> Option<crate::Options> {
    let flags: Vec<OsString> = command
        .split_whitespace()
        .filter(|t| t.starts_with("--"))
        .map(OsString::from)
        .collect();
    match crate::parse_args(&flags) {
        Ok(Command::Run { options, .. }) => Some(options),
        _ => None,
    }
}

/// The result size from the rest of a `size:` line, such as
/// `   71.09 KB -> 69.98 KB (1.6% smaller), ...`.
fn result_size(text: &str) -> Option<Expect> {
    let (_, after) = text.split_once("-> ")?;
    let (size, _) = after.split_once(" (")?;
    let (value, unit) = size.split_once(' ')?;
    let value: f64 = value.parse().ok()?;
    let scale = match unit {
        "B" => {
            return Some(Expect::Size {
                bytes: value,
                tolerance: 0.0,
            })
        }
        "KB" => 1024f64,
        "MB" => 1024f64.powi(2),
        "GB" => 1024f64.powi(3),
        _ => return None,
    };
    // Shown with two decimals, so off by up to half a hundredth of a unit.
    Some(Expect::Size {
        bytes: value * scale,
        tolerance: 0.005 * scale + 1.0,
    })
}

/// Why a reported file was not added.
#[derive(Default)]
struct Misses {
    missing: usize,
    size: usize,
    newer: usize,
}

/// Where `r` is now, if a file there still matches the report.
fn locate(r: &Reported, misses: &mut Misses) -> Option<PathBuf> {
    let mut found_any = false;
    let mut size_ok = false;
    for path in r.output.iter().chain([&r.path]) {
        let Ok(meta) = fs::metadata(path) else {
            continue;
        };
        found_any = true;
        let matches = match r.expect {
            Expect::Size { bytes, tolerance } => (meta.len() as f64 - bytes).abs() <= tolerance,
            Expect::Kept => true,
        };
        if !matches {
            continue;
        }
        size_ok = true;
        let secs = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs());
        if secs.is_some_and(|s| s <= r.deadline + SLACK_SECS) {
            return Some(path.clone());
        }
    }
    match (found_any, size_ok) {
        (false, _) => misses.missing += 1,
        (true, false) => misses.size += 1,
        (true, true) => misses.newer += 1,
    }
    None
}

/// Adds the files reported in `log` that still match to the record at
/// `done`.
pub fn import_log(log: &Path, done: &Path, out: &Output) -> Result<()> {
    let bytes = fs::read(log).with_context(|| format!("cannot read {}", log.display()))?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let (reported, unusable) = parse(&String::from_utf8_lossy(&bytes), now);
    out.line(format!(
        "{} file(s) reported in {}.",
        reported.len(),
        log.display()
    ));
    if unusable > 0 {
        out.say(
            Kind::Warning,
            format!(
                "Ignoring {unusable} run(s) that were given files or folders; \
                 their log does not say where those were."
            ),
        );
    }

    let mut misses = Misses::default();
    let found: Vec<(PathBuf, Mode)> = reported
        .iter()
        .filter_map(|r| locate(r, &mut misses).map(|p| (p, r.mode)))
        .collect();
    out.line(format!(
        "{} still match; reading them to record their content...",
        found.len()
    ));

    let record = Record::open(done).with_context(|| format!("cannot open {}", done.display()))?;
    let read = AtomicUsize::new(0);
    let failed = AtomicUsize::new(0);
    let total = found.len();
    let chunk = total.div_ceil(8).max(1);
    std::thread::scope(|s| {
        for part in found.chunks(chunk) {
            let (record, read, failed) = (&record, &read, &failed);
            s.spawn(move || {
                for (path, mode) in part {
                    let added = fs::read(path).and_then(|data| {
                        let meta = fs::metadata(path)?;
                        record.add(Entry {
                            hash: record::hash(&data),
                            size: data.len() as u64,
                            mode: *mode,
                            modified: record::modified_ns(&meta),
                            path: path.clone(),
                        })
                    });
                    if let Err(e) = added {
                        failed.fetch_add(1, Ordering::Relaxed);
                        out.error(format!("Cannot read {}: {e}", path.display()));
                    }
                    let n = read.fetch_add(1, Ordering::Relaxed) + 1;
                    if n % 1000 == 0 {
                        out.line(format!("  {n} of {total} read"));
                    }
                }
            });
        }
    });

    let failed = failed.into_inner();
    out.say(
        Kind::Total,
        format!(
            "Added {} file(s) to {}.",
            found.len() - failed,
            done.display()
        ),
    );
    if misses.missing + misses.size + misses.newer > 0 {
        out.line(format!(
            "Not added: {} no longer found, {} with a different size, \
             {} changed after their run. These are processed normally.",
            misses.missing, misses.size, misses.newer
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::Sink;
    use std::fs::File;
    use std::time::Duration;

    fn log_for(base: &Path) -> String {
        let b = base.display();
        format!(
            "===== 2026-11-06 05:02:26 UTC: gif_optimizer --lossy --recursive --log --jobs=4 =====\n\
             GIF Optimizer - {b}\n\
             Found 4 GIF file(s).\n\
             \n\
             [1/5] moved.gif\n    frames: 8 -> 8 (8 unique)\n    size:   71.09 KB -> 69.98 KB (1.6% smaller), verified pixel-identical\n    done in 0.1s\n\
             [2/5] sub\\kept.gif\n    frames: 3 -> 3 (3 unique)\n    kept original file: re-encoded version was not smaller\n    done in 0.1s\n\
             [3/5] edited.gif\n    size:   1.00 KB -> 512 B (50.0% smaller), verified pixel-identical\n\
             [4/5] gone.gif\n    size:   1.00 KB -> 512 B (50.0% smaller), verified pixel-identical\n\
             [5/5] fixed.gif\n    repaired: frame 3 was cut off; dropped it\n    wrote the repaired file: re-encoded version was not smaller\n\
             \n\
             Finished in 100.0s\n\
             ===== 2026-11-06 06:00:00 UTC: gif_optimizer --exact --jobs=4 \"x.gif\" =====\n\
             GIF Optimizer - 1 path(s) given\n\
             [1/1] x.gif\n    size:   2 B -> 1 B (50.0% smaller), verified pixel-identical\n\
             ===== 2026-11-06 07:00:00 UTC: gif_optimizer --exact --jobs=4 =====\n\
             GIF Optimizer - {b}\n\
             [1/1] edited.gif\n    size:   2.00 KB -> 600 B (70.0% smaller), verified pixel-identical\n\
             Finished in 1.0s\n"
        )
    }

    fn set_time(path: &Path, secs: u64) {
        let file = File::options().write(true).open(path).unwrap();
        file.set_modified(UNIX_EPOCH + Duration::from_secs(secs))
            .unwrap();
    }

    #[test]
    fn parses_the_latest_result_of_each_file() {
        let base = Path::new("E:\\gifs");
        let (reported, unusable) = parse(&log_for(base), 0);
        assert_eq!(unusable, 1);
        let names: Vec<_> = reported.iter().map(|r| r.path.clone()).collect();
        assert_eq!(names.len(), 4, "{names:?}");
        let moved = reported
            .iter()
            .find(|r| r.path.ends_with("moved.gif"))
            .unwrap();
        assert_eq!(moved.output, Some(base.join(OUTPUT_DIR).join("moved.gif")));
        assert_eq!(moved.mode, Mode::Lossy);
        let start = parse_utc("2026-11-06 05:02:26").unwrap();
        assert_eq!(moved.deadline, start + 100);
        let Expect::Size { bytes, tolerance } = moved.expect else {
            panic!("{:?}", moved.expect)
        };
        assert!((bytes - 71_659.5).abs() < 1.0 && tolerance > 5.0 && tolerance < 7.0);
        let edited = reported
            .iter()
            .find(|r| r.path.ends_with("edited.gif"))
            .unwrap();
        assert_eq!(edited.mode, Mode::Lossless, "the later run wins");
        assert_eq!(
            edited.expect,
            Expect::Size {
                bytes: 600.0,
                tolerance: 0.0
            }
        );
        assert!(reported.iter().any(|r| r.expect == Expect::Kept));
        assert!(!reported.iter().any(|r| r.path.ends_with("fixed.gif")));
    }

    #[test]
    fn imports_only_files_that_still_match() {
        let root = std::env::temp_dir().join("gif_optimizer_test_import");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("sub")).unwrap();
        let log = root.join("run.log");
        fs::write(&log, log_for(&root)).unwrap();
        let start = parse_utc("2026-11-06 05:02:26").unwrap();

        // The result moved over its original, written during the run.
        let moved = root.join("moved.gif");
        fs::write(&moved, vec![1u8; 71_657]).unwrap();
        set_time(&moved, start + 50);
        // Kept as it was, from long ago.
        let kept = root.join("sub").join("kept.gif");
        fs::write(&kept, b"GIF89a old").unwrap();
        set_time(&kept, 1_000_000);
        // The right size, but edited after the run.
        let edited = root.join("edited.gif");
        fs::write(&edited, vec![2u8; 600]).unwrap();
        set_time(&edited, start + 10_000);

        let done = root.join(record::DONE_FILE);
        import_log(&log, &done, &Output::new(Sink::Console)).unwrap();
        let record = Record::open(&done).unwrap();
        let knows = |p: &Path, mode| record.knows_file(p, &fs::metadata(p).unwrap(), mode);
        assert!(knows(&moved, Mode::Lossy));
        assert!(knows(&kept, Mode::Lossy));
        assert!(!knows(&edited, Mode::Lossless));
        let data = fs::read(&moved).unwrap();
        assert!(record.knows_content(data.len() as u64, record::hash(&data), Mode::Lossy));
        fs::remove_dir_all(&root).unwrap();
    }
}
