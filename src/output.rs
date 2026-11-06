//! Where a run's messages go: the console or the TUI, plus an optional
//! log file that is appended to.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::mpsc::Sender;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

/// Progress of a run, as seen by the TUI.
pub enum Event {
    /// One or more lines of output.
    Text { text: String, error: bool },
    /// Processing is about to start on `total` files.
    Started { total: usize },
    /// A file finished, with its sizes if it succeeded.
    FileDone { sizes: Option<(u64, u64)> },
    /// The run is over; `ok` is false if anything failed.
    Finished { ok: bool },
}

pub enum Sink {
    Console,
    Channel(Sender<Event>),
}

pub struct Output {
    sink: Sink,
    log: Option<Mutex<File>>,
}

impl Output {
    pub fn new(sink: Sink) -> Self {
        Self { sink, log: None }
    }

    /// Appends all further output to `path`, after a header naming the
    /// time and `command`.
    pub fn open_log(&mut self, path: &Path, command: &str) -> std::io::Result<()> {
        let mut file = OpenOptions::new().create(true).append(true).open(path)?;
        if file.metadata()?.len() > 0 {
            writeln!(file)?;
        }
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        writeln!(file, "===== {} UTC: {command} =====", format_utc(now))?;
        self.log = Some(Mutex::new(file));
        Ok(())
    }

    /// Writes `text`, which may span several lines, as one unit so that
    /// output from parallel workers does not interleave.
    pub fn line(&self, text: impl Into<String>) {
        self.write(text.into(), false);
    }

    pub fn error(&self, text: impl Into<String>) {
        self.write(text.into(), true);
    }

    /// Reports progress; only the TUI shows it.
    pub fn event(&self, event: Event) {
        if let Sink::Channel(tx) = &self.sink {
            let _ = tx.send(event);
        }
    }

    fn write(&self, text: String, error: bool) {
        if let Some(log) = &self.log {
            let mut file = log.lock().unwrap_or_else(|e| e.into_inner());
            let _ = writeln!(file, "{text}");
        }
        match &self.sink {
            Sink::Console if error => eprintln!("{text}"),
            Sink::Console => println!("{text}"),
            Sink::Channel(tx) => {
                let _ = tx.send(Event::Text { text, error });
            }
        }
    }
}

/// Seconds since the Unix epoch as `YYYY-MM-DD HH:MM:SS` (UTC).
fn format_utc(secs: u64) -> String {
    let (days, rem) = (secs / 86_400, secs % 86_400);
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02}",
        rem / 3600,
        rem / 60 % 60,
        rem % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_utc_dates() {
        assert_eq!(format_utc(0), "1970-01-01 00:00:00");
        assert_eq!(format_utc(951_782_400), "2000-02-29 00:00:00");
        assert_eq!(format_utc(1_709_251_199), "2024-02-29 23:59:59");
        assert_eq!(format_utc(4_102_444_800), "2100-01-01 00:00:00");
    }

    #[test]
    fn log_is_appended_with_headers() {
        let path = std::env::temp_dir().join("gif_optimizer_test_output.log");
        let _ = std::fs::remove_file(&path);
        for run in ["first", "second"] {
            let (tx, rx) = std::sync::mpsc::channel();
            let mut out = Output::new(Sink::Channel(tx));
            out.open_log(&path, run).unwrap();
            out.line(format!("{run} line"));
            out.error(format!("{run} error"));
            assert_eq!(rx.try_iter().count(), 2, "both lines reach the TUI");
        }
        let log = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = log.lines().collect();
        assert_eq!(lines.len(), 7, "{log}");
        assert!(lines[0].starts_with("===== ") && lines[0].ends_with(" UTC: first ====="));
        assert_eq!(lines[1..4], ["first line", "first error", ""]);
        assert!(lines[4].ends_with(" UTC: second ====="));
        assert_eq!(lines[5..], ["second line", "second error"]);
        std::fs::remove_file(&path).unwrap();
    }
}
