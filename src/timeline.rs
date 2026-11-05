//! Timeline analysis: decides which frames can be dropped without changing
//! what the viewer sees.
//!
//! Two transformations are applied, both of which are exactly lossless:
//!
//! 1. Runs of *consecutive* identical frames are merged into one frame whose
//!    delay is the sum of the run. Non-consecutive repeats (e.g. `1 2 1 3 1 4`)
//!    are left alone because GIF has no way to reference an earlier frame.
//! 2. If the whole (merged) timeline is one cycle repeated `k` times
//!    (e.g. `1 2 3 4 1 2 3 4 1 2 3 4`), only one cycle is kept and the loop
//!    count is multiplied by `k` so that playback is identical.

use gif::Repeat;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TimelineFrame {
    /// Id of the unique composited frame in the `FrameStore`.
    pub id: usize,
    /// Delay in centiseconds.
    pub delay: u16,
}

/// Delay as rendered by browsers: values of 0 or 1 cs are displayed as 100 ms.
fn effective_delay(delay: u16) -> u32 {
    if delay <= 1 {
        10
    } else {
        u32::from(delay)
    }
}

pub fn merge_consecutive(frames: &[TimelineFrame]) -> Vec<TimelineFrame> {
    struct Run {
        id: usize,
        first_delay: u16,
        total: u32,
        len: usize,
    }

    fn finish(run: &Run) -> TimelineFrame {
        // A lone frame keeps its original delay byte-for-byte. Merged runs use
        // effective delays so "0 cs" frames keep the time browsers give them.
        let delay = if run.len == 1 {
            run.first_delay
        } else {
            run.total as u16
        };
        TimelineFrame { id: run.id, delay }
    }

    let mut out = Vec::with_capacity(frames.len());
    let mut run: Option<Run> = None;

    for f in frames {
        if let Some(r) = run.as_mut() {
            let merged_total = r.total + effective_delay(f.delay);
            if r.id == f.id && merged_total <= u32::from(u16::MAX) {
                r.total = merged_total;
                r.len += 1;
                continue;
            }
            out.push(finish(r));
        }
        run = Some(Run {
            id: f.id,
            first_delay: f.delay,
            total: effective_delay(f.delay),
            len: 1,
        });
    }
    if let Some(r) = run {
        out.push(finish(&r));
    }
    out
}

/// Smallest `p` such that the timeline is `frames[..p]` repeated `len / p` times.
pub fn find_period(frames: &[TimelineFrame]) -> usize {
    let n = frames.len();
    (1..n)
        .filter(|&p| n.is_multiple_of(p))
        .find(|&p| (p..n).all(|i| frames[i] == frames[i - p]))
        .unwrap_or(n)
}

/// Collapses a fully repeated timeline to a single cycle if the loop count can
/// be adjusted to keep playback identical. Returns the new timeline, loop
/// setting and the number of cycles that were folded together.
pub fn collapse_repeats(
    frames: Vec<TimelineFrame>,
    repeat: Repeat,
) -> (Vec<TimelineFrame>, Repeat, usize) {
    let period = find_period(&frames);
    let cycles = frames.len() / period.max(1);
    if cycles <= 1 {
        return (frames, repeat, 1);
    }

    let new_repeat = match repeat {
        Repeat::Infinite => Some(Repeat::Infinite),
        // `Finite(n)` plays the animation n + 1 times in total (no extension = once).
        Repeat::Finite(n) => {
            let total_plays = (u64::from(n) + 1) * cycles as u64;
            u16::try_from(total_plays - 1).ok().map(Repeat::Finite)
        }
    };

    match new_repeat {
        Some(r) => (frames[..period].to_vec(), r, cycles),
        None => (frames, repeat, 1),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tl(ids: &[usize], delay: u16) -> Vec<TimelineFrame> {
        ids.iter().map(|&id| TimelineFrame { id, delay }).collect()
    }

    fn ids(frames: &[TimelineFrame]) -> Vec<usize> {
        frames.iter().map(|f| f.id).collect()
    }

    #[test]
    fn keeps_interleaved_repeats() {
        let frames = tl(&[1, 2, 1, 3, 1, 4, 1, 5], 5);
        let merged = merge_consecutive(&frames);
        assert_eq!(merged, frames);
        let (out, repeat, cycles) = collapse_repeats(merged, Repeat::Infinite);
        assert_eq!(ids(&out), vec![1, 2, 1, 3, 1, 4, 1, 5]);
        assert_eq!(repeat, Repeat::Infinite);
        assert_eq!(cycles, 1);
    }

    #[test]
    fn collapses_full_repeats() {
        let frames = tl(&[1, 2, 3, 4, 1, 2, 3, 4, 1, 2, 3, 4], 5);
        let (out, repeat, cycles) = collapse_repeats(merge_consecutive(&frames), Repeat::Infinite);
        assert_eq!(ids(&out), vec![1, 2, 3, 4]);
        assert_eq!(repeat, Repeat::Infinite);
        assert_eq!(cycles, 3);
    }

    #[test]
    fn collapse_adjusts_finite_loop_count() {
        let frames = tl(&[1, 2, 1, 2, 1, 2], 5);
        // No loop extension: plays once -> 3 plays of one cycle.
        let (_, repeat, _) = collapse_repeats(frames.clone(), Repeat::Finite(0));
        assert_eq!(repeat, Repeat::Finite(2));
        // Loop count 1: plays twice -> 6 plays of one cycle.
        let (_, repeat, _) = collapse_repeats(frames, Repeat::Finite(1));
        assert_eq!(repeat, Repeat::Finite(5));
    }

    #[test]
    fn keeps_repeats_followed_by_new_frames() {
        let frames = tl(&[1, 2, 3, 4, 1, 2, 3, 4, 1, 2, 3, 4, 5, 6, 7, 8], 5);
        let (out, _, cycles) = collapse_repeats(merge_consecutive(&frames), Repeat::Infinite);
        assert_eq!(out.len(), 16);
        assert_eq!(cycles, 1);
    }

    #[test]
    fn delays_must_match_to_collapse() {
        let mut frames = tl(&[1, 2, 1, 2], 5);
        frames[3].delay = 6;
        let (out, _, _) = collapse_repeats(frames, Repeat::Infinite);
        assert_eq!(out.len(), 4);
    }

    #[test]
    fn merges_consecutive_duplicates() {
        let frames = tl(&[1, 1, 1, 2, 2, 3], 4);
        let merged = merge_consecutive(&frames);
        assert_eq!(
            merged,
            vec![
                TimelineFrame { id: 1, delay: 12 },
                TimelineFrame { id: 2, delay: 8 },
                TimelineFrame { id: 3, delay: 4 },
            ]
        );
    }

    #[test]
    fn merging_respects_browser_minimum_delay() {
        let merged = merge_consecutive(&tl(&[7, 7], 0));
        assert_eq!(merged, vec![TimelineFrame { id: 7, delay: 20 }]);
    }

    #[test]
    fn merge_then_collapse() {
        let frames = tl(&[1, 1, 2, 1, 1, 2], 5);
        let (out, _, cycles) = collapse_repeats(merge_consecutive(&frames), Repeat::Infinite);
        assert_eq!(
            out,
            vec![
                TimelineFrame { id: 1, delay: 10 },
                TimelineFrame { id: 2, delay: 5 }
            ]
        );
        assert_eq!(cycles, 2);
    }
}
