//! Shrinks GIFs by removing redundant frames and re-encoding only what
//! changes between frames. Processes the GIFs in the executable's directory
//! (and its subdirectories with `--recursive`), or the files and folders
//! given on the command line or dropped onto the executable.
//!
//! By default, frames that differ only by dithering noise are treated as the
//! same image (see `similar`), so the output looks identical but is not
//! guaranteed to be pixel-identical. Every replaced frame is compared
//! directly with the frame that replaces it. Pass `--exact` to merge only
//! pixel-identical frames, making the result lossless.
//!
//! GIFs are processed in parallel (`--jobs=N`, default: 4).
//! If `gifsicle` is on the PATH, its `-O3` pass is tried on each result and
//! kept only if it is smaller and passes the same verification; disable it
//! with `--no-gifsicle`.
//!
//! Output:  `<input folder>/gif_optimized/<relative path>.gif`, skipped
//!          when it is already newer than its source (unless `--force`)
//! Temp:    `<exe dir>/gif_optimizing/` (removed when everything succeeds)
//! Original GIFs are never modified or deleted, unless `--replace` is given:
//! then each original is overwritten in place, keeping its modified time,
//! but only when the result is smaller. Replacing is lossless unless
//! `--lossy` is also given.
//!
//! `--log` appends everything shown to `<exe dir>/gif_optimizer.log`.
//! Started without arguments in a terminal, an interactive screen lets you
//! pick these options before running.

mod composite;
mod encode;
mod lzw;
mod output;
mod progress;
mod similar;
mod store;
mod timeline;
mod tui;

use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{bail, Context, Result};

use composite::composite_frames;
use lzw::Effort;
use output::{Event, FileResult, Kind, Output, Sink};
use progress::{is_stopped, Progress, Stage};
use store::FrameStore;
use timeline::{collapse_repeats, merge_consecutive, TimelineFrame};

const OUTPUT_DIR: &str = "gif_optimized";
const WORK_DIR: &str = "gif_optimizing";
const LOG_FILE: &str = "gif_optimizer.log";

const HELP: &str = "\
Usage: gif_optimizer [OPTIONS] [FILE.gif | FOLDER]...

Shrinks GIFs by removing redundant frames and re-encoding only what changes
between frames. Without paths, the GIFs next to the executable are processed;
files and folders can also be dropped onto the executable. Started without
arguments in a terminal, an interactive screen opens instead.

Results are written to a gif_optimized folder next to each input. A GIF whose
result there is already newer than the GIF itself is skipped.

By default, frames that differ only by dithering noise are merged too: the
result looks the same but may not be pixel-identical.

Options:
  --exact         Merge only pixel-identical frames, so results are lossless
  --replace       Overwrite originals where the result is smaller, keeping
                  their modified time. Lossless unless --lossy is given.
  --lossy         With --replace, also merge frames that only look identical
  --recursive     Also process GIFs in subfolders
  --force         Process GIFs again even if their result is up to date
  --no-gifsicle   Do not try gifsicle -O3 on the results
  --jobs=N        Process N GIFs at a time (default: 4)
  --log           Append all output to gif_optimizer.log next to the executable
  --no-pause      Exit without waiting for Enter
  -h, --help      Show this help
";

struct Report {
    original_frames: usize,
    output_frames: usize,
    unique_frames: usize,
    /// Distinct images left after treating visually identical frames as equal.
    visual_unique_frames: usize,
    cycles_folded: usize,
    original_size: u64,
    output_size: u64,
    /// Set when the original file was copied unchanged, with the reason.
    kept_original: Option<String>,
    /// Whether gifsicle shrank the output further.
    used_gifsicle: bool,
}

#[derive(Clone)]
struct Options {
    exact: bool,
    /// Allow look-alike merging when replacing originals.
    lossy: bool,
    gifsicle: bool,
    /// Number of GIFs processed at the same time.
    jobs: usize,
    /// Overwrite the originals instead of writing to `OUTPUT_DIR`.
    replace: bool,
    /// Also process GIFs in subdirectories.
    recursive: bool,
    /// Append all output to `LOG_FILE`.
    log: bool,
    /// Process GIFs even if their output is up to date.
    force: bool,
    /// Files and folders to process instead of the executable's directory.
    paths: Vec<PathBuf>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            exact: false,
            lossy: false,
            gifsicle: true,
            jobs: Self::default_jobs(),
            replace: false,
            recursive: false,
            log: false,
            force: false,
            paths: Vec::new(),
        }
    }
}

/// GIFs processed at the same time by default. Each one already uses a few
/// threads and holds its frames in memory, so running one per core makes
/// GIFs with hundreds of frames slow everything down.
const DEFAULT_JOBS: usize = 4;

impl Options {
    /// `DEFAULT_JOBS`, or fewer on machines with fewer cores.
    fn default_jobs() -> usize {
        let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
        DEFAULT_JOBS.min(cores)
    }

    /// Whether only pixel-identical frames may be merged. Replacing an
    /// original cannot be undone, so it is lossless unless asked otherwise.
    fn lossless(&self) -> bool {
        self.exact || (self.replace && !self.lossy)
    }

    /// The command line that runs with these options, for the log header.
    fn command(&self) -> String {
        let mut cmd = "gif_optimizer".to_string();
        for (on, flag) in [
            (self.exact, "--exact"),
            (!self.gifsicle, "--no-gifsicle"),
            (self.replace, "--replace"),
            (self.lossy, "--lossy"),
            (self.recursive, "--recursive"),
            (self.force, "--force"),
            (self.log, "--log"),
        ] {
            if on {
                cmd += " ";
                cmd += flag;
            }
        }
        cmd += &format!(" --jobs={}", self.jobs);
        for path in &self.paths {
            cmd += &format!(" \"{}\"", path.display());
        }
        cmd
    }
}

enum Command {
    Run { options: Options, pause: bool },
    Help,
}

fn parse_args(args: &[OsString]) -> Result<Command, String> {
    let mut options = Options::default();
    let mut pause = true;
    let mut flags_done = false;
    for arg in args {
        // Anything that is not an option (including non-UTF-8 names) is a path.
        let flag = arg.to_str().filter(|a| !flags_done && a.starts_with('-'));
        let Some(flag) = flag else {
            if !flags_done && arg == "/?" {
                return Ok(Command::Help);
            }
            options.paths.push(PathBuf::from(arg));
            continue;
        };
        match flag {
            "--" => flags_done = true,
            "-h" | "--help" => return Ok(Command::Help),
            "--exact" => options.exact = true,
            "--lossy" => options.lossy = true,
            "--no-gifsicle" => options.gifsicle = false,
            "--replace" => options.replace = true,
            "--recursive" => options.recursive = true,
            "--force" => options.force = true,
            "--log" => options.log = true,
            "--no-pause" => pause = false,
            _ => match flag.strip_prefix("--jobs=") {
                Some(n) => {
                    options.jobs = n.parse().ok().filter(|&n| n > 0).ok_or_else(|| {
                        format!("invalid --jobs value {n:?}; expected a whole number above 0")
                    })?;
                }
                None => return Err(format!("unknown option {flag}")),
            },
        }
    }
    if options.exact && options.lossy {
        return Err("--exact and --lossy cannot be combined".to_string());
    }
    Ok(Command::Run { options, pause })
}

fn wait_for_enter() {
    print!("\nPress Enter to exit...");
    let _ = io::stdout().flush();
    let _ = io::stdin().lock().read_line(&mut String::new());
}

fn main() {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    if args.is_empty() && io::stdin().is_terminal() && io::stdout().is_terminal() {
        std::process::exit(tui::main());
    }
    let (options, pause) = match parse_args(&args) {
        Ok(Command::Run { options, pause }) => (options, pause),
        Ok(Command::Help) => {
            print!("{HELP}");
            std::process::exit(0);
        }
        Err(e) => {
            eprintln!("Error: {e}\nRun with --help to see the options.");
            if !args.iter().any(|a| a == "--no-pause") {
                wait_for_enter();
            }
            std::process::exit(2);
        }
    };
    let mut out = Output::new(Sink::Console);
    if options.log {
        if let Err(e) = open_log(&mut out, &options) {
            out.error(format!("Not logging: {e:#}"));
        }
    }
    let code = if run_reported(&options, &out, &AtomicBool::new(false)) {
        0
    } else {
        1
    };
    if pause {
        wait_for_enter();
    }
    std::process::exit(code);
}

/// The directory containing the executable, which is the one processed.
fn base_dir() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("cannot locate the executable")?;
    Ok(exe
        .parent()
        .context("executable has no parent directory")?
        .to_path_buf())
}

fn open_log(out: &mut Output, options: &Options) -> Result<()> {
    let path = base_dir()?.join(LOG_FILE);
    out.open_log(&path, &options.command())
        .with_context(|| format!("cannot open {}", path.display()))
}

/// Runs and reports a fatal error. Returns true if everything succeeded.
fn run_reported(options: &Options, out: &Output, stop: &AtomicBool) -> bool {
    match run(options, out, stop) {
        Ok(ok) => ok,
        Err(e) => {
            out.error(format!("Fatal error: {e:#}"));
            false
        }
    }
}

/// One GIF to process.
struct Job {
    src: PathBuf,
    /// Where the result goes; `src` itself when replacing.
    dest: PathBuf,
    /// How the file is named in the output.
    name: String,
}

impl Job {
    /// A job for `src`, found under `root`, named relative to `name_root`.
    fn new(root: &Path, name_root: &Path, src: PathBuf, replace: bool) -> Self {
        let dest = if replace {
            src.clone()
        } else {
            root.join(OUTPUT_DIR)
                .join(src.strip_prefix(root).unwrap_or(&src))
        };
        let name = src
            .strip_prefix(name_root)
            .unwrap_or(&src)
            .display()
            .to_string();
        Self { src, dest, name }
    }

    /// Whether a previous run already wrote a result that is at least as
    /// new as the source. Replaced originals cannot be recognized.
    fn up_to_date(&self) -> bool {
        let modified = |p: &Path| fs::metadata(p).and_then(|m| m.modified()).ok();
        self.dest != self.src
            && matches!(
                (modified(&self.src), modified(&self.dest)),
                (Some(src), Some(dest)) if dest >= src
            )
    }
}

fn is_gif(path: &Path) -> bool {
    path.extension()
        .is_some_and(|ext| ext.to_string_lossy().eq_ignore_ascii_case("gif"))
}

/// The GIFs to process: those in `base_dir`, or in the given files and
/// folders. Also returns how many given paths could not be used; each is
/// reported.
fn collect_jobs(
    options: &Options,
    base_dir: &Path,
    work_root: &Path,
    out: &Output,
) -> Result<(Vec<Job>, usize)> {
    let skip = |root: &Path| [root.join(OUTPUT_DIR), work_root.to_path_buf()];
    let mut jobs = Vec::new();
    if options.paths.is_empty() {
        for src in find_gifs(base_dir, options.recursive, &skip(base_dir), out)? {
            jobs.push(Job::new(base_dir, base_dir, src, options.replace));
        }
    }
    let mut problems = 0;
    for path in &options.paths {
        if path.is_dir() {
            // Name files including the folder itself, as it was given.
            let name_root = path.parent().unwrap_or(path);
            match find_gifs(path, options.recursive, &skip(path), out) {
                Ok(gifs) => jobs.extend(
                    gifs.into_iter()
                        .map(|src| Job::new(path, name_root, src, options.replace)),
                ),
                Err(e) => {
                    out.error(format!("Skipping {}: {e:#}", path.display()));
                    problems += 1;
                }
            }
        } else if path.is_file() && is_gif(path) {
            let parent = path.parent().unwrap_or(Path::new(""));
            jobs.push(Job::new(parent, parent, path.clone(), options.replace));
        } else {
            let reason = if path.exists() {
                "not a .gif file"
            } else {
                "not found"
            };
            out.error(format!("Skipping {}: {reason}", path.display()));
            problems += 1;
        }
    }
    jobs.sort_by(|a, b| a.src.cmp(&b.src));
    jobs.dedup_by(|a, b| a.src == b.src);
    Ok((jobs, problems))
}

/// Returns `Ok(true)` if every GIF was processed without errors. Once
/// `stop` is set, no further GIFs are started and those in progress are
/// cancelled.
fn run(options: &Options, out: &Output, stop: &AtomicBool) -> Result<bool> {
    let base_dir = base_dir()?;
    let work_root = base_dir.join(WORK_DIR);
    out.line(if options.paths.is_empty() {
        format!("GIF Optimizer - {}", base_dir.display())
    } else {
        format!("GIF Optimizer - {} path(s) given", options.paths.len())
    });
    let (mut jobs, problems) = collect_jobs(options, &base_dir, &work_root, out)?;
    let found = jobs.len();
    if !options.force && !options.replace {
        jobs.retain(|j| !j.up_to_date());
    }
    let up_to_date = found - jobs.len();
    out.event(Event::Started { total: jobs.len() });
    if found == 0 {
        out.line("No GIF files found.");
        return Ok(problems == 0);
    }
    out.say(Kind::Emphasis, format!("Found {found} GIF file(s)."));
    if up_to_date > 0 {
        out.line(format!(
            "Skipping {up_to_date} with an up-to-date result in {OUTPUT_DIR}; \
             use --force to process them again."
        ));
    }
    if jobs.is_empty() {
        out.line("Nothing left to do.");
        return Ok(problems == 0);
    }
    if options.replace {
        out.say(
            Kind::Warning,
            "Originals will be replaced where the result is smaller.",
        );
        if !options.lossless() {
            out.say(
                Kind::Warning,
                "Look-alike frames may be merged (--lossy), so replaced files may not be pixel-identical.",
            );
        }
    }
    out.line("");

    fs::create_dir_all(&work_root)
        .with_context(|| format!("cannot create {}", work_root.display()))?;

    let gifsicle_version = options.gifsicle.then(gifsicle_version).flatten();
    let use_gifsicle = gifsicle_version.is_some();
    let workers = options.jobs.min(jobs.len());
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
    encode::set_threads((cores / workers).max(1));
    out.line(format!(
        "Processing {workers} at a time; gifsicle {}.\n",
        match (options.gifsicle, &gifsicle_version) {
            (false, _) => "disabled".to_string(),
            (true, None) => "not found (install it for slightly smaller files)".to_string(),
            // Output can differ between gifsicle versions, so name it.
            (true, Some(v)) => format!("found ({v}), used for extra compression"),
        }
    ));

    // Workers take the next unprocessed GIF until none are left. Each GIF's
    // report is written in one piece when it finishes, so the order follows
    // completion rather than the file list.
    let next = AtomicUsize::new(0);
    let started_all = Instant::now();
    let (errors, stopped, total_in, total_out) = std::thread::scope(|s| {
        let handles: Vec<_> = (0..workers)
            .map(|_| {
                s.spawn(|| {
                    let (mut errors, mut stopped) = (0usize, 0usize);
                    let (mut total_in, mut total_out) = (0u64, 0u64);
                    while !stop.load(Ordering::Relaxed) {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        let Some(job) = jobs.get(i) else { break };
                        let work_dir = work_root.join(work_dir_name(i, &job.src));
                        out.event(Event::FileStarted {
                            index: i,
                            name: job.name.clone(),
                        });
                        let progress = Progress::new(stop, out, i);
                        let started = Instant::now();
                        let result = process_gif(
                            &job.src,
                            &job.dest,
                            &work_dir,
                            options.lossless(),
                            use_gifsicle,
                            &progress,
                        );
                        let header = (
                            Kind::Heading,
                            format!("[{}/{}] {}", i + 1, jobs.len(), job.name),
                        );
                        let file_result = match result {
                            Ok(r) => {
                                total_in += r.original_size;
                                total_out += r.output_size;
                                out.block([vec![header], describe(&r, started)].concat());
                                FileResult::Done(r.original_size, r.output_size)
                            }
                            Err(e) if is_stopped(&e) => {
                                stopped += 1;
                                // A partial result is of no use for a later run.
                                let _ = fs::remove_dir_all(&work_dir);
                                out.block(vec![
                                    header,
                                    (Kind::Warning, "    stopped before finishing".into()),
                                ]);
                                FileResult::Stopped
                            }
                            Err(e) => {
                                errors += 1;
                                out.block(vec![
                                    header,
                                    (Kind::Error, format!("    ERROR: {e:#}")),
                                    (
                                        Kind::Error,
                                        format!(
                                            "    temporary files kept in {}",
                                            work_dir.display()
                                        ),
                                    ),
                                ]);
                                FileResult::Failed
                            }
                        };
                        out.event(Event::FileDone {
                            index: i,
                            result: file_result,
                        });
                    }
                    (errors, stopped, total_in, total_out)
                })
            })
            .collect();
        handles.into_iter().map(join).fold((0, 0, 0, 0), |a, b| {
            (a.0 + b.0, a.1 + b.1, a.2 + b.2, a.3 + b.3)
        })
    });

    out.line("");
    let unprocessed = jobs.len().saturating_sub(next.load(Ordering::Relaxed)) + stopped;
    if unprocessed > 0 {
        out.say(
            Kind::Warning,
            format!("Stopped early; {unprocessed} GIF(s) were not processed."),
        );
    }
    if total_in > 0 {
        out.say(
            Kind::Total,
            format!(
                "Total: {} -> {} (saved {})",
                format_size(total_in),
                format_size(total_out),
                format_size(total_in.saturating_sub(total_out))
            ),
        );
    }
    out.say(
        Kind::Emphasis,
        format!("Finished in {:.1}s", started_all.elapsed().as_secs_f64()),
    );
    if options.replace {
        out.line("Smaller results replaced their originals.");
    } else if options.paths.is_empty() {
        out.line(format!(
            "Optimized GIFs are in {}",
            base_dir.join(OUTPUT_DIR).display()
        ));
    } else {
        out.line(format!(
            "Optimized GIFs are in the {OUTPUT_DIR} folder next to each input."
        ));
    }

    if errors == 0 {
        fs::remove_dir_all(&work_root)
            .with_context(|| format!("cannot remove {}", work_root.display()))?;
    } else {
        out.error(format!(
            "{errors} GIF(s) failed; {} was left in place.",
            work_root.display()
        ));
    }
    Ok(errors == 0 && problems == 0)
}

/// The per-file report lines, without the header.
fn describe(r: &Report, started: Instant) -> Vec<(Kind, String)> {
    let visual = if r.visual_unique_frames < r.unique_frames {
        format!(", {} after merging look-alikes", r.visual_unique_frames)
    } else {
        String::new()
    };
    let folded = if r.cycles_folded > 1 {
        format!(", {} identical loops folded into one", r.cycles_folded)
    } else {
        String::new()
    };
    let mut lines = vec![(
        Kind::Detail,
        format!(
            "    frames: {} -> {} ({} unique{visual}{folded})",
            r.original_frames, r.output_frames, r.unique_frames,
        ),
    )];
    match &r.kept_original {
        Some(reason) => lines.push((Kind::Warning, format!("    kept original file: {reason}"))),
        None => lines.push((
            Kind::Good,
            format!(
                "    size:   {} -> {} ({:.1}% smaller), {}{}",
                format_size(r.original_size),
                format_size(r.output_size),
                100.0 * (1.0 - r.output_size as f64 / r.original_size as f64),
                if r.visual_unique_frames < r.unique_frames {
                    "look-alike frames merged (use --exact for lossless)"
                } else {
                    "verified pixel-identical"
                },
                if r.used_gifsicle {
                    ", gifsicle applied"
                } else {
                    ""
                },
            ),
        )),
    }
    lines.push((
        Kind::Detail,
        format!("    done in {:.1}s", started.elapsed().as_secs_f64()),
    ));
    lines
}

/// First line of `gifsicle --version`, or `None` if gifsicle is not usable.
fn gifsicle_version() -> Option<String> {
    let output = std::process::Command::new("gifsicle")
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())?;
    let text = String::from_utf8_lossy(&output.stdout);
    let line = text.lines().next().unwrap_or("").trim();
    Some(
        if line.is_empty() {
            "unknown version"
        } else {
            line
        }
        .to_string(),
    )
}

/// GIFs in `dir`, and in its subdirectories (except `skip`) if `recursive`.
/// Symlinked directories are not followed, so links cannot cause loops.
fn find_gifs(dir: &Path, recursive: bool, skip: &[PathBuf], out: &Output) -> Result<Vec<PathBuf>> {
    let mut gifs = Vec::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(current) = pending.pop() {
        let entries = match fs::read_dir(&current) {
            Ok(entries) => entries,
            Err(e) if current != dir => {
                out.error(format!("Skipping {}: {e}", current.display()));
                continue;
            }
            Err(e) => return Err(e).with_context(|| format!("cannot read {}", dir.display())),
        };
        for entry in entries.filter_map(|e| e.ok()) {
            let path = entry.path();
            if entry.file_type().is_ok_and(|t| t.is_dir()) {
                if recursive && !skip.contains(&path) {
                    pending.push(path);
                }
            } else if path.is_file() && is_gif(&path) {
                gifs.push(path);
            }
        }
    }
    gifs.sort();
    Ok(gifs)
}

fn work_dir_name(index: usize, src: &Path) -> String {
    let stem: String = src
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .take(40)
        .collect();
    format!("{:04}_{stem}", index + 1)
}

/// Joins a scoped thread, re-raising its panic if it had one.
fn join<T>(handle: std::thread::ScopedJoinHandle<'_, T>) -> T {
    handle
        .join()
        .unwrap_or_else(|e| std::panic::resume_unwind(e))
}

/// The candidate with visually identical frames merged, or `None` if no
/// frames look alike. The caller reserves progress for grouping (one unit
/// per stored frame) and `CANDIDATE_PASSES * raw.len()` for the candidate.
fn lookalike_candidate(
    store: &FrameStore,
    raw: &[TimelineFrame],
    info: &composite::GifInfo,
    progress: &Progress,
) -> Result<Option<Result<Candidate>>> {
    let (w, h) = (info.width as usize, info.height as usize);
    let canonical = similar::group_similar(store, w, h, progress)?;
    let visual_unique = canonical
        .iter()
        .enumerate()
        .filter(|&(i, &c)| i == c)
        .count();
    if visual_unique == store.unique_count() {
        progress.skip(CANDIDATE_PASSES * raw.len());
        return Ok(None);
    }
    let plan = plan_timeline(raw, &canonical, info.repeat);
    progress.skip(CANDIDATE_PASSES * raw.len().saturating_sub(plan.0.len()));
    Ok(Some(candidate(
        store,
        plan,
        visual_unique,
        info.width,
        info.height,
        progress,
    )))
}

/// Runs `gifsicle -O3` on `bytes`. Returns `None` if gifsicle is not
/// installed or fails, and an error if stopped while it runs.
fn gifsicle(bytes: &[u8], work_dir: &Path, progress: &Progress) -> Result<Option<Vec<u8>>> {
    let (input, output) = (work_dir.join("pre.gif"), work_dir.join("gifsicle.gif"));
    if fs::write(&input, bytes).is_err() {
        return Ok(None);
    }
    let result = run_gifsicle(&input, &output, progress);
    let _ = fs::remove_file(&input);
    let _ = fs::remove_file(&output);
    result
}

fn run_gifsicle(input: &Path, output: &Path, progress: &Progress) -> Result<Option<Vec<u8>>> {
    let Ok(mut child) = std::process::Command::new("gifsicle")
        .args(["-O3", "--no-warnings", "-o"])
        .arg(output)
        .arg(input)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    else {
        return Ok(None);
    };
    // Poll instead of waiting, so that a stop request can kill it.
    loop {
        let checked = progress.check();
        match child.try_wait() {
            Ok(Some(status)) => {
                return Ok(status.success().then(|| fs::read(output).ok()).flatten());
            }
            Ok(None) if checked.is_ok() => std::thread::sleep(Duration::from_millis(20)),
            // Stopped, or the process could not be queried.
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                checked?;
                return Ok(None);
            }
        }
    }
}

/// Optimizes `src` and writes the result to `dest`, which may be `src`
/// itself. If the result is not smaller, the original is copied to `dest`.
/// Stops with an error as soon as `progress` reports a stop request; `dest`
/// is then left untouched.
fn process_gif(
    src: &Path,
    dest: &Path,
    work_dir: &Path,
    exact: bool,
    use_gifsicle: bool,
    progress: &Progress,
) -> Result<Report> {
    progress.stage(Stage::Reading)?;
    if work_dir.exists() {
        fs::remove_dir_all(work_dir)?;
    }
    let mut store = FrameStore::new(&work_dir.join("frames"))?;

    // 1. Break the GIF into fully composited still frames, storing each
    //    distinct image only once.
    let mut raw = Vec::new();
    let file = File::open(src).with_context(|| format!("cannot open {}", src.display()))?;
    let info = composite_frames(file, |canvas, delay| {
        progress.tick()?;
        let id = store.insert(canvas)?;
        raw.push(TimelineFrame { id, delay });
        Ok(())
    })?;
    if raw.is_empty() {
        bail!("GIF contains no frames");
    }

    // 2. Drop redundant frames and work out candidate encodings. Exact
    //    matching is always tried. In default mode, frames that differ only
    //    by dithering noise are also tried as the same image, in parallel.
    //    Merging look-alikes can make the changes between consecutive frames
    //    larger, so it is kept only if the file shrinks.
    progress.stage(Stage::Optimizing)?;
    let identity: Vec<usize> = (0..store.unique_count()).collect();
    let exact_plan = plan_timeline(&raw, &identity, info.repeat);
    // Reserve all the work up front, so the fraction done only grows. The
    // final encoding's length is not known yet; no timeline is longer than
    // the original.
    progress.expect(CANDIDATE_PASSES * exact_plan.0.len() + final_units() * raw.len());
    if !exact {
        progress.expect(store.unique_count() + CANDIDATE_PASSES * raw.len());
    }
    let (exact_candidate, lookalike) = std::thread::scope(|s| {
        let exact_job = s.spawn(|| {
            let (w, h) = (info.width, info.height);
            candidate(
                &store,
                exact_plan.clone(),
                store.unique_count(),
                w,
                h,
                progress,
            )
        });
        let lookalike = if exact {
            Ok(None)
        } else {
            lookalike_candidate(&store, &raw, &info, progress)
        };
        (join(exact_job), lookalike)
    });
    // A stopped candidate fails like a broken one; do not mistake it for that.
    progress.check()?;
    let mut failures = Vec::new();
    let mut candidates = Vec::new();
    for result in [Some(exact_candidate), lookalike?].into_iter().flatten() {
        match result {
            Ok(c) => candidates.push(c),
            Err(e) => failures.push(e),
        }
    }

    // 3. Encode the smallest candidate with the slow clear code search and
    //    verify it, falling back to the next one if that fails.
    candidates.sort_by_key(|c| c.fast_size);
    let first_len = candidates.first().map_or(0, |c| c.timeline.len());
    progress.skip(final_units() * raw.len().saturating_sub(first_len));
    let mut chosen = None;
    for c in candidates {
        match finalize(&store, &c, info.width, info.height, progress) {
            Ok(bytes) => {
                chosen = Some((bytes, c));
                break;
            }
            Err(e) => {
                progress.check()?;
                failures.push(e);
            }
        }
    }
    let (mut encoded, timeline, repeat, cycles_folded, visual_unique_frames) = match chosen {
        Some((bytes, c)) => (
            Ok(bytes),
            c.timeline,
            c.repeat,
            c.cycles_folded,
            c.visual_unique,
        ),
        None => {
            let (timeline, repeat, folded) = exact_plan;
            let e = failures.into_iter().next().expect("a candidate failed");
            (Err(e), timeline, repeat, folded, store.unique_count())
        }
    };

    // 4. Let gifsicle try to shrink the result further, if it is installed.
    //    Its output is verified like our own before it is used.
    let mut used_gifsicle = false;
    if use_gifsicle {
        if let Ok(bytes) = &encoded {
            progress.stage(Stage::Gifsicle)?;
            if let Some(g) = gifsicle(bytes, work_dir, progress)? {
                if g.len() < bytes.len() {
                    progress.expect(timeline.len());
                    let (w, h) = (info.width, info.height);
                    if encode::verify(&g, &store, &timeline, w, h, repeat, progress).is_ok() {
                        encoded = Ok(g);
                        used_gifsicle = true;
                    }
                }
            }
        }
    }
    progress.check()?;

    let src_meta = fs::metadata(src)?;
    let original_size = src_meta.len();
    // A replaced original keeps its modified time, so it still sorts and
    // syncs like before.
    let modified = if dest == src {
        src_meta.modified().ok()
    } else {
        None
    };
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("cannot create {}", parent.display()))?;
    }
    let (output_size, kept_original) = match encoded {
        Ok(bytes) if (bytes.len() as u64) < original_size => {
            write_atomically(dest, &bytes, modified)?;
            (bytes.len() as u64, None)
        }
        other => {
            let reason = match other {
                Ok(_) => "re-encoded version was not smaller".to_string(),
                Err(e) => format!("could not re-encode losslessly ({e:#})"),
            };
            if dest != src {
                fs::copy(src, dest).with_context(|| format!("cannot write {}", dest.display()))?;
            }
            (original_size, Some(reason))
        }
    };

    let report = Report {
        original_frames: raw.len(),
        output_frames: timeline.len(),
        unique_frames: store.unique_count(),
        visual_unique_frames,
        cycles_folded,
        original_size,
        output_size,
        kept_original,
        used_gifsicle,
    };

    drop(store);
    fs::remove_dir_all(work_dir)
        .with_context(|| format!("cannot remove {}", work_dir.display()))?;
    Ok(report)
}

/// Writes `bytes` to a temporary file next to `dest`, then renames it over
/// `dest`, so `dest` is never left half-written (it may be an original).
/// The file gets `modified` as its modified time, if given.
fn write_atomically(dest: &Path, bytes: &[u8], modified: Option<SystemTime>) -> Result<()> {
    let mut temp_name = dest
        .file_name()
        .context("output has no file name")?
        .to_owned();
    temp_name.push(".optimizing");
    let temp = dest.with_file_name(temp_name);
    let write = || -> io::Result<()> {
        let mut file = File::create(&temp)?;
        file.write_all(bytes)?;
        if let Some(time) = modified {
            file.set_modified(time)?;
        }
        Ok(())
    };
    let result = write()
        .with_context(|| format!("cannot write {}", temp.display()))
        .and_then(|()| {
            fs::rename(&temp, dest).with_context(|| format!("cannot replace {}", dest.display()))
        });
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

/// Maps every frame to its canonical image, then drops consecutive repeats
/// and folds identical loops.
fn plan_timeline(
    raw: &[TimelineFrame],
    canonical: &[usize],
    repeat: gif::Repeat,
) -> (Vec<TimelineFrame>, gif::Repeat, usize) {
    let grouped: Vec<TimelineFrame> = raw
        .iter()
        .map(|f| TimelineFrame {
            id: canonical[f.id],
            ..*f
        })
        .collect();
    collapse_repeats(merge_consecutive(&grouped), repeat)
}

/// One way to encode a GIF, sized with fast LZW compression. The clear code
/// search takes far longer than anything else, but changes every
/// candidate's size by about the same few percent, so only the smallest
/// candidate gets it.
struct Candidate {
    timeline: Vec<TimelineFrame>,
    repeat: gif::Repeat,
    cycles_folded: usize,
    /// Distinct images left in `timeline`'s source after grouping.
    visual_unique: usize,
    plans: Vec<encode::FramePlan>,
    /// The shared palette, if it beat per-frame palettes.
    global: Option<encode::GlobalPalette>,
    fast_size: usize,
}

/// Progress units per frame for a candidate: planning, then fast encoding
/// with per-frame and with shared palettes.
const CANDIDATE_PASSES: usize = 3;

/// Progress units per frame for `finalize`: the slow encoding, then
/// verifying.
fn final_units() -> usize {
    encode::units(Effort::Best) + 1
}

/// Plans `timeline` and picks the smaller palette layout. The caller
/// reserves `CANDIDATE_PASSES` progress units per frame.
fn candidate(
    store: &FrameStore,
    (timeline, repeat, cycles_folded): (Vec<TimelineFrame>, gif::Repeat, usize),
    visual_unique: usize,
    width: u16,
    height: u16,
    progress: &Progress,
) -> Result<Candidate> {
    let plans = encode::plan_frames(store, &timeline, width, height, progress)?;

    // Per-frame palettes are smaller for some GIFs, one shared palette for
    // others; try both in parallel.
    let global = encode::global_palette(&plans);
    if global.is_none() {
        progress.skip(timeline.len());
    }
    let fast_size = &|g: Option<&encode::GlobalPalette>| {
        encode::encode(
            store,
            &timeline,
            &plans,
            width,
            height,
            repeat,
            g,
            Effort::Fast,
            progress,
        )
        .map(|bytes| bytes.len())
    };
    let (local, shared) = std::thread::scope(|s| {
        let shared = global.as_ref().map(|g| s.spawn(move || fast_size(Some(g))));
        (fast_size(None), shared.map(join))
    });
    let local = local?;
    let (global, fast_size) = match shared.transpose()? {
        Some(size) if size < local => (global, size),
        _ => (None, local),
    };
    Ok(Candidate {
        timeline,
        repeat,
        cycles_folded,
        visual_unique,
        plans,
        global,
        fast_size,
    })
}

/// Encodes `c` as small as possible and verifies the result. The caller
/// reserves `final_units()` progress units per frame.
fn finalize(
    store: &FrameStore,
    c: &Candidate,
    width: u16,
    height: u16,
    progress: &Progress,
) -> Result<Vec<u8>> {
    let bytes = encode::encode(
        store,
        &c.timeline,
        &c.plans,
        width,
        height,
        c.repeat,
        c.global.as_ref(),
        Effort::Best,
        progress,
    )?;
    encode::verify(
        &bytes,
        store,
        &c.timeline,
        width,
        height,
        c.repeat,
        progress,
    )?;
    Ok(bytes)
}

fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.2} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gif::{DisposalMethod, Encoder, Frame, Repeat};

    const W: u16 = 48;
    const H: u16 = 48;

    static NEVER: AtomicBool = AtomicBool::new(false);

    fn process(src: &Path, dest: &Path, work_dir: &Path, exact: bool) -> Result<Report> {
        process_gif(src, dest, work_dir, exact, false, &Progress::silent(&NEVER))
    }

    /// A distinct, non-trivial image per `seed` (gradient plus a moving box).
    fn image(seed: u8, transparent_box: bool) -> Vec<u8> {
        let mut px = Vec::with_capacity(W as usize * H as usize * 4);
        for y in 0..H as usize {
            for x in 0..W as usize {
                let in_box = (x / 8 + y / 8) % 6 == seed as usize % 6;
                if in_box && transparent_box {
                    px.extend_from_slice(&[0, 0, 0, 0]);
                } else if in_box {
                    px.extend_from_slice(&[255, seed.wrapping_mul(40), 0, 255]);
                } else {
                    px.extend_from_slice(&[(x * 5) as u8, (y * 5) as u8, 128, 255]);
                }
            }
        }
        px
    }

    /// Few-colour image (flat 8x8 blocks) so that, even with dithering
    /// added, it fits one GIF palette and is stored without quantization.
    fn blocky(seed: u8) -> Vec<u8> {
        let mut px = Vec::with_capacity(W as usize * H as usize * 4);
        for y in 0..H as usize {
            for x in 0..W as usize {
                let (bx, by) = ((x / 8) as u8, (y / 8) as u8);
                let hit = (bx + by) % 6 == seed % 6;
                let v = if hit {
                    230
                } else {
                    40 + 30 * ((bx + 2 * by) % 5)
                };
                px.extend_from_slice(&[v, 255 - v, 128, 255]);
            }
        }
        px
    }

    /// Shifts about a quarter of the opaque pixels by ±`amp`, like dithering
    /// noise. Different `seed`s give different noise.
    fn dither(rgba: &mut [u8], amp: i16, seed: u32) {
        let mut s = seed;
        for px in rgba.as_chunks_mut::<4>().0.iter_mut().filter(|p| p[3] != 0) {
            s = s.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            let d = match (s >> 16) % 8 {
                0 => amp,
                1 => -amp,
                _ => continue,
            };
            for c in &mut px[..3] {
                *c = (i16::from(*c) + d).clamp(0, 255) as u8;
            }
        }
    }

    /// Writes a GIF whose frames are full-canvas images `seeds[i]` with `delay`.
    fn write_gif(path: &Path, seeds: &[u8], repeat: Repeat, transparent: bool) {
        let frames = seeds.iter().map(|&s| image(s, transparent)).collect();
        write_frames(path, frames, repeat, transparent);
    }

    fn write_frames(path: &Path, frames: Vec<Vec<u8>>, repeat: Repeat, transparent: bool) {
        let mut out = Vec::new();
        {
            let mut enc = Encoder::new(&mut out, W, H, &[]).unwrap();
            enc.set_repeat(repeat).unwrap();
            for mut rgba in frames {
                let mut f = Frame::from_rgba_speed(W, H, &mut rgba, 10);
                f.delay = 10;
                if transparent {
                    f.dispose = DisposalMethod::Background;
                }
                enc.write_frame(&f).unwrap();
            }
        }
        fs::write(path, out).unwrap();
    }

    /// Expands a GIF into one composited canvas per centisecond-ish timeline
    /// entry: (canvas, delay) pairs with consecutive duplicates merged.
    fn playback(path: &Path) -> (Vec<(Vec<u32>, u32)>, Repeat) {
        let mut frames: Vec<(Vec<u32>, u32)> = Vec::new();
        let info = composite_frames(File::open(path).unwrap(), |canvas, delay| {
            match frames.last_mut() {
                Some((last, d)) if last.as_slice() == canvas => *d += u32::from(delay),
                _ => frames.push((canvas.to_vec(), u32::from(delay))),
            }
            Ok(())
        })
        .unwrap();
        (frames, info.repeat)
    }

    fn run_case(name: &str, seeds: &[u8], repeat: Repeat, transparent: bool) -> Report {
        let root = std::env::temp_dir().join(format!("gif_optimizer_test_{name}"));
        let _ = fs::remove_dir_all(&root);
        let out_dir = root.join(OUTPUT_DIR);
        let work_dir = root.join(WORK_DIR).join("0001");
        fs::create_dir_all(&out_dir).unwrap();

        let src = root.join(format!("{name}.gif"));
        write_gif(&src, seeds, repeat, transparent);
        let original = fs::read(&src).unwrap();

        let dest = out_dir.join(format!("{name}.gif"));
        let report = process(&src, &dest, &work_dir, false).unwrap();

        assert_eq!(fs::read(&src).unwrap(), original, "original was modified");
        assert!(!work_dir.exists(), "work dir not cleaned up");
        assert!(dest.exists());
        assert!(fs::metadata(&dest).unwrap().len() <= original.len() as u64);

        let (src_frames, src_repeat) = playback(&src);
        let (dest_frames, dest_repeat) = playback(&dest);
        if report.cycles_folded == 1 {
            assert_eq!(dest_frames, src_frames);
            assert_eq!(dest_repeat, src_repeat);
        } else {
            // One output cycle repeated must equal the original, and the
            // total number of plays must be unchanged.
            let n = report.cycles_folded;
            let unrolled: Vec<_> = (0..n).flat_map(|_| dest_frames.iter().cloned()).collect();
            assert_eq!(unrolled, src_frames);
            match (src_repeat, dest_repeat) {
                (Repeat::Infinite, Repeat::Infinite) => {}
                (Repeat::Finite(a), Repeat::Finite(b)) => {
                    assert_eq!((u32::from(a) + 1) * n as u32, u32::from(b) + 1)
                }
                other => panic!("loop setting changed: {other:?}"),
            }
        }

        fs::remove_dir_all(&root).unwrap();
        report
    }

    #[test]
    fn interleaved_repeats_are_kept() {
        let r = run_case("interleaved", &[1, 2, 1, 3, 1, 4], Repeat::Infinite, false);
        assert_eq!(r.output_frames, 6);
        assert_eq!(r.unique_frames, 4);
        assert_eq!(r.cycles_folded, 1);
    }

    #[test]
    fn full_cycles_are_folded() {
        let seeds = [1, 2, 3, 4, 1, 2, 3, 4, 1, 2, 3, 4];
        let r = run_case("cycles", &seeds, Repeat::Infinite, false);
        assert_eq!(r.original_frames, 12);
        assert_eq!(r.output_frames, 4);
        assert_eq!(r.cycles_folded, 3);
        assert!(r.kept_original.is_none(), "{:?}", r.kept_original);
    }

    #[test]
    fn finite_cycles_adjust_loop_count() {
        let r = run_case("finite", &[5, 6, 5, 6], Repeat::Finite(2), false);
        assert_eq!(r.output_frames, 2);
        assert_eq!(r.cycles_folded, 2);
    }

    #[test]
    fn repeats_followed_by_new_frames_are_kept() {
        let r = run_case("tail", &[1, 2, 3, 1, 2, 3, 4], Repeat::Infinite, false);
        assert_eq!(r.output_frames, 7);
        assert_eq!(r.cycles_folded, 1);
    }

    #[test]
    fn consecutive_duplicates_are_merged() {
        let r = run_case(
            "dupes",
            &[1, 1, 1, 2, 2, 3, 3, 3, 3],
            Repeat::Infinite,
            false,
        );
        assert_eq!(r.original_frames, 9);
        assert_eq!(r.output_frames, 3);
        assert!(r.kept_original.is_none(), "{:?}", r.kept_original);
    }

    #[test]
    fn transparency_is_preserved() {
        let r = run_case("transparent", &[1, 2, 2, 3, 1, 4], Repeat::Infinite, true);
        assert_eq!(r.output_frames, 5);
    }

    /// Repeats that differ only by dithering noise fold in the default mode,
    /// and every output frame still looks like the original it replaces.
    #[test]
    fn dithered_cycles_are_folded() {
        let root = std::env::temp_dir().join("gif_optimizer_test_dithered");
        let _ = fs::remove_dir_all(&root);
        let out_dir = root.join(OUTPUT_DIR);
        fs::create_dir_all(&out_dir).unwrap();
        let src = root.join("dithered.gif");
        let frames = [1u8, 2, 1, 2, 1, 2]
            .iter()
            .zip(1..)
            .map(|(&s, seed)| {
                let mut rgba = blocky(s);
                dither(&mut rgba, 16, seed);
                rgba
            })
            .collect();
        write_frames(&src, frames, Repeat::Infinite, false);

        let dest = out_dir.join("dithered.gif");
        let exact = process(&src, &dest, &root.join("work_exact"), true).unwrap();
        assert_eq!(exact.unique_frames, 6, "dithered frames should all differ");
        assert_eq!(exact.cycles_folded, 1);

        let r = process(&src, &dest, &root.join("work"), false).unwrap();
        assert_eq!(r.visual_unique_frames, 2);
        assert_eq!(r.output_frames, 2);
        assert_eq!(r.cycles_folded, 3);
        assert!(r.kept_original.is_none(), "{:?}", r.kept_original);

        let (src_frames, src_repeat) = playback(&src);
        let (dest_frames, dest_repeat) = playback(&dest);
        assert_eq!(src_repeat, dest_repeat);
        assert_eq!(src_frames.len(), dest_frames.len() * 3);
        for (s, d) in src_frames.iter().zip(dest_frames.iter().cycle()) {
            assert_eq!(s.1, d.1, "delay changed");
            assert!(similar::visually_identical(
                &s.0, &d.0, W as usize, H as usize
            ));
        }

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn replace_overwrites_original_in_place() {
        let root = std::env::temp_dir().join("gif_optimizer_test_replace");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let src = root.join("replace.gif");
        write_gif(&src, &[1, 1, 2, 2, 1, 1, 2, 2], Repeat::Infinite, false);
        let original_size = fs::metadata(&src).unwrap().len();
        let (before, _) = playback(&src);
        let old = SystemTime::UNIX_EPOCH + Duration::from_secs(1_500_000_000);
        File::options()
            .write(true)
            .open(&src)
            .unwrap()
            .set_modified(old)
            .unwrap();

        let r = process(&src, &src, &root.join("work"), true).unwrap();
        assert!(r.kept_original.is_none(), "{:?}", r.kept_original);
        let meta = fs::metadata(&src).unwrap();
        assert!(meta.len() < original_size);
        assert_eq!(meta.modified().unwrap(), old, "modified time is kept");
        let (after, _) = playback(&src);
        let unrolled: Vec<_> = (0..r.cycles_folded)
            .flat_map(|_| after.iter().cloned())
            .collect();
        assert_eq!(unrolled, before);
        let leftovers: Vec<_> = fs::read_dir(&root).unwrap().collect();
        assert_eq!(leftovers.len(), 1, "temporary files left behind");

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn stop_cancels_without_writing() {
        let root = std::env::temp_dir().join("gif_optimizer_test_stop");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let src = root.join("stop.gif");
        write_gif(&src, &[1, 2, 1, 2], Repeat::Infinite, false);
        let original = fs::read(&src).unwrap();
        let stop = AtomicBool::new(true);
        let progress = Progress::silent(&stop);
        for dest in [root.join("out.gif"), src.clone()] {
            let result = process_gif(&src, &dest, &root.join("work"), false, false, &progress);
            let e = result.err().expect("a stopped file must fail");
            assert!(is_stopped(&e), "{e:#}");
        }
        assert!(!root.join("out.gif").exists());
        assert_eq!(fs::read(&src).unwrap(), original);
        fs::remove_dir_all(&root).unwrap();
    }

    fn parse(args: &[&str]) -> Result<Command, String> {
        let args: Vec<OsString> = args.iter().map(OsString::from).collect();
        parse_args(&args)
    }

    #[test]
    fn arguments_are_validated() {
        let Ok(Command::Run { options, pause }) = parse(&[
            "--replace",
            "a.gif",
            "--jobs=3",
            "--no-pause",
            "--",
            "--odd",
        ]) else {
            panic!("valid arguments rejected");
        };
        assert!(!pause && options.replace && options.jobs == 3);
        assert!(options.lossless(), "replacing is lossless by default");
        let paths: Vec<_> = options.paths.iter().map(|p| p.to_str().unwrap()).collect();
        assert_eq!(paths, ["a.gif", "--odd"]);
        assert!(options.command().ends_with("--jobs=3 \"a.gif\" \"--odd\""));

        let Ok(Command::Run { options, .. }) = parse(&["--replace", "--lossy"]) else {
            panic!("--lossy rejected");
        };
        assert!(!options.lossless());
        let Ok(Command::Run { options, .. }) = parse(&[]) else {
            panic!("no arguments rejected");
        };
        assert!(!options.lossless() && options.paths.is_empty());

        for help in ["-h", "--help", "/?"] {
            assert!(matches!(parse(&["--exact", help]), Ok(Command::Help)));
        }
        assert!(HELP.contains(OUTPUT_DIR) && HELP.contains(LOG_FILE));
        for bad in ["--jobs=0", "--jobs=x", "--jobs=", "--jobs", "--exat", "-x"] {
            let e = parse(&[bad])
                .err()
                .unwrap_or_else(|| panic!("{bad} accepted"));
            assert!(e.contains("--jobs") || e.contains(bad), "{e}");
        }
        assert!(parse(&["--exact", "--lossy"]).is_err());
    }

    #[test]
    fn given_paths_are_collected() {
        let root = std::env::temp_dir().join("gif_optimizer_test_paths");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("anims/sub")).unwrap();
        fs::create_dir_all(root.join("anims").join(OUTPUT_DIR)).unwrap();
        for file in [
            "single.gif",
            "note.txt",
            "anims/a.gif",
            "anims/sub/b.gif",
            "anims/gif_optimized/old.gif",
        ] {
            fs::write(root.join(file), b"").unwrap();
        }
        let options = Options {
            recursive: true,
            paths: [
                "single.gif",
                "anims",
                "note.txt",
                "missing.gif",
                "single.gif",
            ]
            .iter()
            .map(|p| root.join(p))
            .collect(),
            ..Options::default()
        };
        let out = Output::new(Sink::Console);
        let work_root = root.join(WORK_DIR);
        let (jobs, problems) = collect_jobs(&options, &root, &work_root, &out).unwrap();
        assert_eq!(problems, 2, "the text file and the missing file");
        let rel = |p: &Path| {
            p.strip_prefix(&root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/")
        };
        let found: Vec<_> = jobs
            .iter()
            .map(|j| (j.name.replace('\\', "/"), rel(&j.dest)))
            .collect();
        assert_eq!(
            found,
            [
                ("anims/a.gif".into(), "anims/gif_optimized/a.gif".into()),
                (
                    "anims/sub/b.gif".into(),
                    "anims/gif_optimized/sub/b.gif".into()
                ),
                ("single.gif".into(), "gif_optimized/single.gif".into()),
            ] as [(String, String); 3]
        );
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn outputs_newer_than_their_source_are_up_to_date() {
        let root = std::env::temp_dir().join("gif_optimizer_test_up_to_date");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join(OUTPUT_DIR)).unwrap();
        let src = root.join("a.gif");
        fs::write(&src, b"").unwrap();
        let job = Job::new(&root, &root, src.clone(), false);
        assert!(!job.up_to_date(), "no output yet");
        fs::write(&job.dest, b"").unwrap();
        let set = |p: &Path, secs: u64| {
            let t = SystemTime::UNIX_EPOCH + Duration::from_secs(secs);
            let file = File::options().write(true).open(p).unwrap();
            file.set_modified(t).unwrap();
        };
        set(&src, 2_000);
        set(&job.dest, 1_000);
        assert!(!job.up_to_date(), "source changed since");
        set(&job.dest, 2_000);
        assert!(job.up_to_date());
        assert!(!Job::new(&root, &root, src, true).up_to_date(), "replacing");
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn find_gifs_recurses_only_when_asked() {
        let root = std::env::temp_dir().join("gif_optimizer_test_find");
        let _ = fs::remove_dir_all(&root);
        let out_dir = root.join(OUTPUT_DIR);
        for dir in [root.join("a/b"), out_dir.clone()] {
            fs::create_dir_all(dir).unwrap();
        }
        for file in ["top.gif", "a/mid.GIF", "a/b/deep.gif", "a/note.txt"] {
            fs::write(root.join(file), b"").unwrap();
        }
        fs::write(out_dir.join("old.gif"), b"").unwrap();
        let names = |gifs: Vec<PathBuf>| -> Vec<String> {
            gifs.iter()
                .map(|p| {
                    p.strip_prefix(&root)
                        .unwrap()
                        .to_string_lossy()
                        .replace('\\', "/")
                })
                .collect()
        };

        let out = Output::new(Sink::Console);
        let flat = find_gifs(&root, false, std::slice::from_ref(&out_dir), &out).unwrap();
        assert_eq!(names(flat), ["top.gif"]);
        let deep = find_gifs(&root, true, &[out_dir], &out).unwrap();
        assert_eq!(names(deep), ["a/b/deep.gif", "a/mid.GIF", "top.gif"]);

        fs::remove_dir_all(&root).unwrap();
    }
}
