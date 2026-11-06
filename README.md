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

- **Repairs damaged files.** GIFs that end too early, as interrupted
  downloads do, are repaired first: a missing end marker is added, and a
  last frame that was only partly written is dropped. Unexpected data after
  a complete frame is dropped too. The report says what was repaired, and
  the repaired result is written even if it is not smaller.
- **Skips files that are not GIFs.** A `.gif` file that is really a saved
  web page (such as a 404 error page), another image format or a video is
  skipped and listed at the end of the run instead of counting as a failure.

Otherwise, if a result is not smaller than the original, the original is
kept.

## Usage

The easiest way is to put `gif_optimizer.exe` in a folder of GIFs and
double-click it. An interactive screen lets you pick options and watch
progress. You can stop a run at any time with `q`; files in progress are
cancelled and left untouched.

To skip a folder during a run, click a file in the **In progress** list (or
select it with `Tab` and press `x`). Choose the file's folder or one of its
parents with `←`/`→`, then confirm. GIFs being processed in that folder and
its subfolders are cancelled, and the remaining ones are skipped.

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
| `--force`       | Process GIFs again even if already optimized or up to date          |
| `--import-log=FILE` | Record the GIFs a previous run reported in its log (see below)  |
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
newer than the GIF, or if the GIF is already optimized. Use `--force` (or
**Redo optimized GIFs** on the interactive screen) to process it again.

## Remembering optimized GIFs

Already optimized GIFs are recognized in two ways, so either is enough:

- **A marker inside the GIF.** Every GIF the program writes gets a small
  application extension (17 bytes) that viewers ignore. It stays with the
  file wherever it is moved or copied, for example over its original.
- **`gif_optimizer.done` next to the executable.** It lists every finished
  file by path, size, modified time and a hash of its content. Unchanged
  files are recognized without being read; moved or renamed ones are
  recognized by content when read. This also covers GIFs that were kept as
  they were because nothing could be saved, which get no marker.

Both note whether the GIF was finished losslessly (`--exact`, or `--replace`
without `--lossy`). Such a GIF is processed again by a run that may merge
look-alike frames, since that may shrink it further.

GIFs optimized by versions before 0.2.0 have neither. If you still have the
log of those runs, `gif_optimizer --import-log=gif_optimizer.log` adds the
GIFs it reports to `gif_optimizer.done`, without optimizing anything. A GIF
is only added if it is still where the run left it (or was moved over its
original), has the size the log reports for its result, and was not changed
after that run. Each added GIF is read once to record its content.

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
