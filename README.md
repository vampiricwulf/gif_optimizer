# GIF Optimizer

Shrinks animated GIFs by removing redundant frames and re-encoding only what
changes between frames. Colors are never quantized, and every result is
decoded again and checked frame by frame before it is used.

## What it does

- **Merges duplicate frames.** Consecutive identical frames become one frame
  with the combined delay.
- **Folds repeated loops.** If an animation plays the same sequence several
  times, one copy is kept and the loop count is adjusted to match.
- **Merges look-alike frames (default).** Frames that differ only by
  dithering noise are treated as the same image. The result looks the same
  but may not be pixel-identical. Use `--exact` to turn this off.
- **Encodes only changes.** Each frame stores just the rectangle that changed,
  with LZW clear codes placed for the smallest output.
- **Uses gifsicle if available.** If `gifsicle` is on the `PATH`, its `-O3`
  pass is tried on each result. It is kept only if it is smaller and passes
  the same verification.

If a result is not smaller than the original, the original is kept.

## Usage

The easiest way is to put `gif_optimizer.exe` in a folder of GIFs and
double-click it. An interactive screen lets you pick options and watch
progress. You can stop a run at any time with `q`; files in progress are
cancelled and left untouched.

You can also drop GIFs or folders onto the executable, or run it from a
terminal:

```text
gif_optimizer [OPTIONS] [FILE.gif | FOLDER]...
```

Without paths, the GIFs next to the executable are processed.

| Option          | Effect                                                              |
| --------------- | ------------------------------------------------------------------- |
| `--exact`       | Merge only pixel-identical frames, so results are lossless          |
| `--replace`     | Overwrite originals where the result is smaller (see below)         |
| `--lossy`       | With `--replace`, also merge frames that only look identical        |
| `--recursive`   | Also process GIFs in subfolders                                     |
| `--force`       | Process GIFs again even if their result is up to date               |
| `--no-gifsicle` | Do not try `gifsicle -O3` on the results                            |
| `--jobs=N`      | Process N GIFs at a time (default: 4)                               |
| `--log`         | Append all output to `gif_optimizer.log` next to the executable     |
| `--no-pause`    | Exit without waiting for Enter                                      |
| `-h`, `--help`  | Show help                                                           |

Unknown options are rejected, and the exit code is non-zero if anything
failed.

## Where results go

By default, originals are never modified. Results are written to a
`gif_optimized` folder next to each input, with the same relative path.

On later runs, a GIF is skipped if its result in `gif_optimized` is already
newer than the GIF. Use `--force` (or **Redo up-to-date GIFs** on the
interactive screen) to process it again, for example after changing options.

With `--replace`, each original is overwritten in place, but only when the
result is smaller. The original's modified time is kept. Because this cannot
be undone, replacing is **lossless by default**: look-alike frames are merged
only if you also pass `--lossy`. On the interactive screen, turning on
**Replace originals** also turns on **Exact**, and you are asked to confirm
before the run starts.

Temporary files go to `gif_optimizing` next to the executable. This folder is
removed when every GIF succeeds, and kept for inspection if any GIF fails.

## Building

Requires a recent stable Rust toolchain.

```text
cargo build --release
cargo test
```

The executable is written to `target/release/gif_optimizer.exe`.
