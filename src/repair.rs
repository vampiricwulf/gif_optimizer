//! Repairs GIFs that end too early, as interrupted downloads and saves do.
//!
//! The file's blocks are walked without decoding any pixels. If it stops
//! partway through, everything after the last complete frame is dropped,
//! including a frame that was only partly written, and the end marker is
//! added. Viewers disagree on how to show a partial frame, so it has no
//! well-defined look to preserve.

use std::borrow::Cow;
use std::fmt;

const EXTENSION: u8 = 0x21;
const IMAGE: u8 = 0x2C;
const TRAILER: u8 = 0x3B;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Repair {
    /// Every frame was complete, but the end marker was missing.
    MissingTrailer,
    /// The file ends inside this frame (counting from 1), which was dropped.
    CutFrame(usize),
    /// The file ends inside a block after the last frame, which was dropped.
    CutBlock,
}

impl fmt::Display for Repair {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Repair::MissingTrailer => f.write_str("the end marker was missing; added it"),
            Repair::CutFrame(n) => write!(f, "frame {n} was cut off; dropped it"),
            Repair::CutBlock => f.write_str("the file ended inside a block; dropped it"),
        }
    }
}

/// `data` with any repair applied, and which one. Data that is complete,
/// or too damaged to understand, is returned unchanged for the decoder to
/// accept or reject.
pub fn repair(data: &[u8]) -> (Cow<'_, [u8]>, Option<Repair>) {
    match find_cut(data) {
        Some((keep, repair)) => {
            let mut fixed = data[..keep].to_vec();
            fixed.push(TRAILER);
            (Cow::Owned(fixed), Some(repair))
        }
        None => (Cow::Borrowed(data), None),
    }
}

/// How many bytes to keep before adding the end marker, if needed. A file
/// without a single complete frame has nothing worth keeping.
fn find_cut(d: &[u8]) -> Option<(usize, Repair)> {
    if d.len() < 13 || !(d.starts_with(b"GIF87a") || d.starts_with(b"GIF89a")) {
        return None;
    }
    let mut p = 13 + color_table_len(d[10]);
    if p > d.len() {
        return None;
    }
    // Where the blocks after the last complete frame begin. Extensions
    // such as frame timing belong to the frame that follows them, so they
    // are dropped along with it.
    let mut group = p;
    let mut frames = 0;
    loop {
        let Some(&block) = d.get(p) else {
            if frames == 0 {
                return None;
            }
            let repair = if group == d.len() {
                Repair::MissingTrailer
            } else {
                Repair::CutBlock
            };
            return Some((group, repair));
        };
        match block {
            TRAILER => return None,
            EXTENSION => match skip_sub_blocks(d, p + 2) {
                Some(end) => p = end,
                None => return (frames > 0).then_some((group, Repair::CutBlock)),
            },
            IMAGE => {
                let table = d.get(p + 9).map(|&flags| color_table_len(flags));
                // Descriptor, local color table, then the LZW minimum code size.
                let data = table.map(|t| p + 10 + t + 1);
                match data.and_then(|start| skip_sub_blocks(d, start)) {
                    Some(end) => {
                        frames += 1;
                        p = end;
                        group = end;
                    }
                    None => return (frames > 0).then_some((group, Repair::CutFrame(frames + 1))),
                }
            }
            // Not a block start: the file is damaged in some other way.
            _ => return None,
        }
    }
}

/// Size of the color table announced by a packed flags byte.
fn color_table_len(flags: u8) -> usize {
    if flags & 0x80 == 0 {
        0
    } else {
        3 << ((flags & 7) + 1)
    }
}

/// The position after the sub-blocks starting at `p` and their
/// terminator, or `None` if the data ends first.
fn skip_sub_blocks(d: &[u8], mut p: usize) -> Option<usize> {
    loop {
        let len = usize::from(*d.get(p)?);
        if len == 0 {
            return Some(p + 1);
        }
        p += len + 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::composite::composite_frames;
    use gif::{Encoder, Frame, Repeat};

    /// A looping GIF with `n` different 16x16 frames.
    fn gif(n: u8) -> Vec<u8> {
        let mut out = Vec::new();
        {
            let mut enc = Encoder::new(&mut out, 16, 16, &[]).unwrap();
            enc.set_repeat(Repeat::Infinite).unwrap();
            for i in 0..n {
                let mut rgba: Vec<u8> = (0..256u32)
                    .flat_map(|p| [(p as u8).wrapping_mul(i + 1), i * 40, 99, 255])
                    .collect();
                let mut f = Frame::from_rgba_speed(16, 16, &mut rgba, 10);
                f.delay = 5;
                enc.write_frame(&f).unwrap();
            }
        }
        out
    }

    fn frame_count(data: &[u8]) -> usize {
        composite_frames(data, |_, _| Ok(())).unwrap().frame_count
    }

    #[test]
    fn complete_files_are_untouched() {
        let data = gif(3);
        let (fixed, repair) = repair(&data);
        assert!(matches!(fixed, Cow::Borrowed(_)) && repair.is_none());
    }

    #[test]
    fn missing_end_marker_is_added() {
        let data = gif(3);
        let (fixed, repair) = repair(&data[..data.len() - 1]);
        assert_eq!(repair, Some(Repair::MissingTrailer));
        assert_eq!(&*fixed, &data[..]);
    }

    #[test]
    fn cut_off_frame_is_dropped() {
        let data = gif(3);
        let (fixed, repair) = repair(&data[..data.len() - 20]);
        assert_eq!(repair, Some(Repair::CutFrame(3)));
        assert_eq!(frame_count(&fixed), 2);
    }

    /// Wherever the file is cut after its first frame, the repaired data
    /// decodes, keeping the frames that were complete. Before that there is
    /// nothing to keep, so the data is left for the decoder to reject.
    #[test]
    fn every_cut_decodes() {
        let data = gif(3);
        let mut last = 0;
        for len in 0..data.len() {
            let (fixed, repair) = repair(&data[..len]);
            if repair.is_none() {
                assert_eq!(last, 0, "cut at {len} not noticed after a frame");
                continue;
            }
            let frames = frame_count(&fixed);
            assert!(frames >= last.max(1), "cut at {len}: {frames} frames");
            last = frames;
        }
        assert_eq!(
            last, 3,
            "a cut just before the end marker keeps every frame"
        );
        assert_eq!(repair(&data[..5]).1, None, "too short to understand");
    }
}
