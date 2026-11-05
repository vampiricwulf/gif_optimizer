//! Shrinks every GIF in the executable's directory by removing redundant
//! frames and re-encoding only what changes between frames.
//!
//! By default, frames that differ only by dithering noise are treated as the
//! same image (see `similar`), so the output looks identical but is not
//! guaranteed to be pixel-identical. Every replaced frame is compared
//! directly with the frame that replaces it. Pass `--exact` to merge only
//! pixel-identical frames, making the result lossless.
//!
//! GIFs are processed in parallel (`--jobs=N`, default: one per CPU core).
//! If `gifsicle` is on the PATH, its `-O3` pass is tried on each result and
//! kept only if it is smaller and passes the same verification; disable it
//! with `--no-gifsicle`.
//!
//! Output:  `<exe dir>/gif_optimized/<original name>.gif`
//! Temp:    `<exe dir>/gif_optimizing/` (removed when everything succeeds)
//! Original GIFs are never modified or deleted.

mod composite;
mod encode;
mod lzw;
mod similar;
mod store;
mod timeline;

use std::fs::{self, File};
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Instant;

use anyhow::{bail, Context, Result};

use composite::composite_frames;
use store::FrameStore;
use timeline::{collapse_repeats, merge_consecutive, TimelineFrame};

const OUTPUT_DIR: &str = "gif_optimized";
const WORK_DIR: &str = "gif_optimizing";

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

struct Options {
    exact: bool,
    gifsicle: bool,
    /// Number of GIFs processed at the same time.
    jobs: usize,
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let has = |flag: &str| args.iter().any(|a| a == flag);
    let pause = !has("--no-pause");
    let jobs = args
        .iter()
        .find_map(|a| a.strip_prefix("--jobs="))
        .and_then(|n| n.parse().ok())
        .filter(|&n| n > 0)
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(1, |n| n.get()));
    let options = Options {
        exact: has("--exact"),
        gifsicle: !has("--no-gifsicle"),
        jobs,
    };
    let code = match run(&options) {
        Ok(true) => 0,
        Ok(false) => 1,
        Err(e) => {
            eprintln!("Fatal error: {e:#}");
            1
        }
    };
    if pause {
        print!("\nPress Enter to exit...");
        let _ = io::stdout().flush();
        let _ = io::stdin().lock().read_line(&mut String::new());
    }
    std::process::exit(code);
}

/// Returns `Ok(true)` if every GIF was processed without errors.
fn run(options: &Options) -> Result<bool> {
    let exe = std::env::current_exe().context("cannot locate the executable")?;
    let base_dir = exe
        .parent()
        .context("executable has no parent directory")?
        .to_path_buf();

    let gifs = find_gifs(&base_dir)?;
    println!("GIF Optimizer - {}", base_dir.display());
    if gifs.is_empty() {
        println!("No GIF files found.");
        return Ok(true);
    }
    println!("Found {} GIF file(s).\n", gifs.len());

    let out_dir = base_dir.join(OUTPUT_DIR);
    let work_root = base_dir.join(WORK_DIR);
    fs::create_dir_all(&out_dir).with_context(|| format!("cannot create {}", out_dir.display()))?;
    fs::create_dir_all(&work_root)
        .with_context(|| format!("cannot create {}", work_root.display()))?;

    let gifsicle_version = options.gifsicle.then(gifsicle_version).flatten();
    let use_gifsicle = gifsicle_version.is_some();
    let jobs = options.jobs.min(gifs.len());
    println!(
        "Processing {jobs} at a time; gifsicle {}.\n",
        match (options.gifsicle, &gifsicle_version) {
            (false, _) => "disabled".to_string(),
            (true, None) => "not found (install it for slightly smaller files)".to_string(),
            // Output can differ between gifsicle versions, so name it.
            (true, Some(v)) => format!("found ({v}), used for extra compression"),
        }
    );

    // Workers take the next unprocessed GIF until none are left. Each GIF's
    // report is printed in one piece when it finishes, so the order follows
    // completion rather than the file list.
    let next = AtomicUsize::new(0);
    let print_lock = Mutex::new(());
    let started_all = Instant::now();
    let (errors, total_in, total_out) = std::thread::scope(|s| {
        let workers: Vec<_> = (0..jobs)
            .map(|_| {
                s.spawn(|| {
                    let (mut errors, mut total_in, mut total_out) = (0usize, 0u64, 0u64);
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        let Some(src) = gifs.get(i) else { break };
                        let work_dir = work_root.join(work_dir_name(i, src));
                        let started = Instant::now();
                        let result =
                            process_gif(src, &out_dir, &work_dir, options.exact, use_gifsicle);
                        let name = src.file_name().unwrap_or_default().to_string_lossy();
                        let header = format!("[{}/{}] {name}", i + 1, gifs.len());
                        let _guard = print_lock.lock().unwrap_or_else(|e| e.into_inner());
                        match result {
                            Ok(r) => {
                                total_in += r.original_size;
                                total_out += r.output_size;
                                println!("{header}\n{}", describe(&r, started));
                            }
                            Err(e) => {
                                errors += 1;
                                eprintln!("{header}\n    ERROR: {e:#}");
                                eprintln!("    temporary files kept in {}", work_dir.display());
                            }
                        }
                    }
                    (errors, total_in, total_out)
                })
            })
            .collect();
        workers
            .into_iter()
            .map(join)
            .fold((0, 0, 0), |a, b| (a.0 + b.0, a.1 + b.1, a.2 + b.2))
    });

    println!();
    if total_in > 0 {
        println!(
            "Total: {} -> {} (saved {})",
            format_size(total_in),
            format_size(total_out),
            format_size(total_in.saturating_sub(total_out))
        );
    }
    println!("Finished in {:.1}s", started_all.elapsed().as_secs_f64());
    println!("Optimized GIFs are in {}", out_dir.display());

    if errors == 0 {
        fs::remove_dir_all(&work_root)
            .with_context(|| format!("cannot remove {}", work_root.display()))?;
        Ok(true)
    } else {
        eprintln!(
            "{errors} GIF(s) failed; {} was left in place.",
            work_root.display()
        );
        Ok(false)
    }
}

/// The per-file report lines, without the header.
fn describe(r: &Report, started: Instant) -> String {
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
    let mut out = format!(
        "    frames: {} -> {} ({} unique{visual}{folded})\n",
        r.original_frames, r.output_frames, r.unique_frames,
    );
    match &r.kept_original {
        Some(reason) => out += &format!("    kept original file: {reason}\n"),
        None => {
            out += &format!(
                "    size:   {} -> {} ({:.1}% smaller), {}{}\n",
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
            )
        }
    }
    out += &format!("    done in {:.1}s", started.elapsed().as_secs_f64());
    out
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

fn find_gifs(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut gifs: Vec<PathBuf> = fs::read_dir(dir)
        .with_context(|| format!("cannot read {}", dir.display()))?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.is_file()
                && p.extension()
                    .is_some_and(|ext| ext.to_string_lossy().eq_ignore_ascii_case("gif"))
        })
        .collect();
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

type Candidate = (
    Result<Vec<u8>>,
    (Vec<TimelineFrame>, gif::Repeat, usize),
    usize,
);

/// Encodes the timeline with visually identical frames merged, or returns
/// `None` if no frames look alike.
fn lookalike_candidate(
    store: &FrameStore,
    raw: &[TimelineFrame],
    info: &composite::GifInfo,
) -> Result<Option<Candidate>> {
    let (w, h) = (info.width as usize, info.height as usize);
    let canonical = similar::group_similar(store, w, h)?;
    let visual_unique = canonical
        .iter()
        .enumerate()
        .filter(|&(i, &c)| i == c)
        .count();
    if visual_unique == store.unique_count() {
        return Ok(None);
    }
    let plan = plan_timeline(raw, &canonical, info.repeat);
    let encoded = optimize(store, &plan.0, info.width, info.height, plan.1);
    Ok(Some((encoded, plan, visual_unique)))
}

/// Runs `gifsicle -O3` on `bytes`. Returns `None` if gifsicle is not
/// installed or fails.
fn gifsicle(bytes: &[u8], work_dir: &Path) -> Option<Vec<u8>> {
    let (input, output) = (work_dir.join("pre.gif"), work_dir.join("gifsicle.gif"));
    fs::write(&input, bytes).ok()?;
    let status = std::process::Command::new("gifsicle")
        .args(["-O3", "--no-warnings", "-o"])
        .arg(&output)
        .arg(&input)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .ok()?;
    let result = status.success().then(|| fs::read(&output).ok()).flatten();
    let _ = fs::remove_file(&input);
    let _ = fs::remove_file(&output);
    result
}

fn process_gif(
    src: &Path,
    out_dir: &Path,
    work_dir: &Path,
    exact: bool,
    use_gifsicle: bool,
) -> Result<Report> {
    if work_dir.exists() {
        fs::remove_dir_all(work_dir)?;
    }
    let mut store = FrameStore::new(&work_dir.join("frames"))?;

    // 1. Break the GIF into fully composited still frames, storing each
    //    distinct image only once.
    let mut raw = Vec::new();
    let file = File::open(src).with_context(|| format!("cannot open {}", src.display()))?;
    let info = composite_frames(file, |canvas, delay| {
        let id = store.insert(canvas)?;
        raw.push(TimelineFrame { id, delay });
        Ok(())
    })?;
    if raw.is_empty() {
        bail!("GIF contains no frames");
    }

    // 2. Drop redundant frames and re-encode, verifying against the expected
    //    frames. Exact matching is always tried. In default mode, frames that
    //    differ only by dithering noise are also tried as the same image, in
    //    parallel. Merging look-alikes can make the changes between
    //    consecutive frames larger, so it is kept only if the file shrinks.
    let identity: Vec<usize> = (0..store.unique_count()).collect();
    let exact_plan = plan_timeline(&raw, &identity, info.repeat);
    let (exact_encoded, lookalike) = std::thread::scope(|s| {
        let exact_job =
            s.spawn(|| optimize(&store, &exact_plan.0, info.width, info.height, exact_plan.1));
        let lookalike = if exact {
            Ok(None)
        } else {
            lookalike_candidate(&store, &raw, &info)
        };
        (join(exact_job), lookalike)
    });
    let mut chosen = (exact_encoded, exact_plan, store.unique_count());
    if let Some((encoded, plan, visual_unique)) = lookalike? {
        let smaller = match (&encoded, &chosen.0) {
            (Ok(new), Ok(old)) => new.len() < old.len(),
            (Ok(_), Err(_)) => true,
            (Err(_), _) => false,
        };
        if smaller {
            chosen = (encoded, plan, visual_unique);
        }
    }
    let (mut encoded, (timeline, repeat, cycles_folded), visual_unique_frames) = chosen;

    // 3. Let gifsicle try to shrink the result further, if it is installed.
    //    Its output is verified like our own before it is used.
    let mut used_gifsicle = false;
    if use_gifsicle {
        if let Ok(bytes) = &encoded {
            if let Some(g) = gifsicle(bytes, work_dir) {
                if g.len() < bytes.len()
                    && encode::verify(&g, &store, &timeline, info.width, info.height, repeat)
                        .is_ok()
                {
                    encoded = Ok(g);
                    used_gifsicle = true;
                }
            }
        }
    }

    let original_size = fs::metadata(src)?.len();
    let dest = out_dir.join(src.file_name().context("source has no file name")?);
    let (output_size, kept_original) = match encoded {
        Ok(bytes) if (bytes.len() as u64) < original_size => {
            let temp_out = work_dir.join("output.gif");
            fs::write(&temp_out, &bytes)
                .with_context(|| format!("cannot write {}", temp_out.display()))?;
            fs::copy(&temp_out, &dest)
                .with_context(|| format!("cannot write {}", dest.display()))?;
            (bytes.len() as u64, None)
        }
        other => {
            let reason = match other {
                Ok(_) => "re-encoded version was not smaller".to_string(),
                Err(e) => format!("could not re-encode losslessly ({e:#})"),
            };
            fs::copy(src, &dest).with_context(|| format!("cannot write {}", dest.display()))?;
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

fn optimize(
    store: &FrameStore,
    timeline: &[TimelineFrame],
    width: u16,
    height: u16,
    repeat: gif::Repeat,
) -> Result<Vec<u8>> {
    let plans = encode::plan_frames(store, timeline, width, height)?;

    // Per-frame palettes are smaller for some GIFs, one shared palette for
    // others; try both in parallel and keep the smaller file.
    let global = encode::global_palette(&plans);
    let (local, shared) = std::thread::scope(|s| {
        let shared = global.as_ref().map(|g| {
            s.spawn(|| encode::encode(store, timeline, &plans, width, height, repeat, Some(g)))
        });
        let local = encode::encode(store, timeline, &plans, width, height, repeat, None);
        (local, shared.map(join))
    });
    let mut best = local?;
    if let Some(candidate) = shared.transpose()? {
        if candidate.len() < best.len() {
            best = candidate;
        }
    }

    encode::verify(&best, store, timeline, width, height, repeat)?;
    Ok(best)
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

        let report = process_gif(&src, &out_dir, &work_dir, false, false).unwrap();

        assert_eq!(fs::read(&src).unwrap(), original, "original was modified");
        assert!(!work_dir.exists(), "work dir not cleaned up");
        let dest = out_dir.join(format!("{name}.gif"));
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

        let exact = process_gif(&src, &out_dir, &root.join("work_exact"), true, false).unwrap();
        assert_eq!(exact.unique_frames, 6, "dithered frames should all differ");
        assert_eq!(exact.cycles_folded, 1);

        let r = process_gif(&src, &out_dir, &root.join("work"), false, false).unwrap();
        assert_eq!(r.visual_unique_frames, 2);
        assert_eq!(r.output_frames, 2);
        assert_eq!(r.cycles_folded, 3);
        assert!(r.kept_original.is_none(), "{:?}", r.kept_original);

        let (src_frames, src_repeat) = playback(&src);
        let (dest_frames, dest_repeat) = playback(&out_dir.join("dithered.gif"));
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
}
