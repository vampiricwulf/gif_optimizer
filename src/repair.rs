//! Repairs GIFs that end too early, as interrupted downloads and saves do,
//! or that have junk after their last frame. Also recognizes files that are
//! not GIFs at all, such as error pages saved under a `.gif` name.
//!
//! The file's blocks are walked without decoding any pixels. If it stops
//! partway through, or something that is not a block follows a complete
//! frame, everything after the last complete frame is dropped, including a
//! frame that was only partly written, and the end marker is added. Viewers
//! disagree on how to show a partial frame, so it has no well-defined look
//! to preserve.

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
    /// Data that is not a GIF block followed frame `after`; that data and
    /// everything after it (`bytes`, not counting a final end marker) was
    /// dropped.
    Junk { after: usize, bytes: usize },
}

impl fmt::Display for Repair {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Repair::MissingTrailer => f.write_str("the end marker was missing; added it"),
            Repair::CutFrame(n) => write!(f, "frame {n} was cut off; dropped it"),
            Repair::CutBlock => f.write_str("the file ended inside a block; dropped it"),
            Repair::Junk { after, bytes } => {
                let unit = if *bytes == 1 { "byte" } else { "bytes" };
                write!(
                    f,
                    "dropped {bytes} {unit} of unexpected data after frame {after}"
                )
            }
        }
    }
}

/// The error for a file that is not a GIF, saying what it is instead.
#[derive(Debug)]
pub struct NotGif(pub &'static str);

impl fmt::Display for NotGif {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "not a GIF: {}", self.0)
    }
}

impl std::error::Error for NotGif {}

/// What `data` is, if it is clearly not a GIF. Anything starting like a
/// GIF is left for the decoder to judge.
pub fn not_gif(data: &[u8]) -> Option<NotGif> {
    if data.starts_with(b"GIF") {
        return None;
    }
    let starts = |magic: &[u8]| data.starts_with(magic);
    let what = if data.is_empty() {
        "the file is empty"
    } else if looks_like_html(data) {
        "it is a web page (probably a saved error page)"
    } else if starts(b"\x89PNG") {
        "it is a PNG image"
    } else if starts(b"\xFF\xD8\xFF") {
        "it is a JPEG image"
    } else if starts(b"RIFF") && data.get(8..12) == Some(b"WEBP") {
        "it is a WebP image"
    } else if starts(b"\x1A\x45\xDF\xA3") {
        "it is a WebM/MKV video"
    } else if data.get(4..8) == Some(b"ftyp") {
        "it is an MP4/MOV video"
    } else {
        "the data is not recognized"
    };
    Some(NotGif(what))
}

fn looks_like_html(data: &[u8]) -> bool {
    let start = &data[..data.len().min(1024)];
    let text = String::from_utf8_lossy(start).to_ascii_lowercase();
    let text = text.trim_start_matches('\u{feff}').trim_start();
    text.starts_with('<')
        && ["<!doctype html", "<html", "<head", "<body"]
            .iter()
            .any(|tag| text.contains(tag))
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
            // Not a block start. After a complete frame, the rest cannot be
            // read by the decoder either, so it is dropped.
            _ if frames > 0 => {
                // An end marker at the very end is kept, not dropped.
                let bytes = d.len() - p - usize::from(d.ends_with(&[TRAILER]));
                return Some((
                    group,
                    Repair::Junk {
                        after: frames,
                        bytes,
                    },
                ));
            }
            _ => return None,
        }
    }
}

/// Size of the color table announced by a packed flags byte.
pub(crate) fn color_table_len(flags: u8) -> usize {
    if flags & 0x80 == 0 {
        0
    } else {
        3 << ((flags & 7) + 1)
    }
}

/// The position after the sub-blocks starting at `p` and their
/// terminator, or `None` if the data ends first.
pub(crate) fn skip_sub_blocks(d: &[u8], mut p: usize) -> Option<usize> {
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

    /// Start of each top-level block after the header, and the trailer.
    fn block_starts(d: &[u8]) -> Vec<usize> {
        let mut p = 13 + color_table_len(d[10]);
        let mut starts = vec![p];
        while d[p] != TRAILER {
            p = match d[p] {
                EXTENSION => skip_sub_blocks(d, p + 2).unwrap(),
                _ => skip_sub_blocks(d, p + 10 + color_table_len(d[p + 9]) + 1).unwrap(),
            };
            starts.push(p);
        }
        starts
    }

    /// Junk between blocks, like a stray byte before the end marker, drops
    /// everything from there but keeps the complete frames before it.
    #[test]
    fn junk_after_a_frame_is_dropped() {
        let data = gif(3);
        let trailer = data.len() - 1;
        let mut stray = data.clone();
        stray.insert(trailer, 0x01);
        let (fixed, found) = repair(&stray);
        assert_eq!(found, Some(Repair::Junk { after: 3, bytes: 1 }));
        assert_eq!(&*fixed, &data[..]);
        assert_eq!(
            found.unwrap().to_string(),
            "dropped 1 byte of unexpected data after frame 3"
        );

        // Any non-block byte, at any block boundary after the first frame.
        let starts = block_starts(&data);
        for &at in &starts {
            for junk in (0..=255u8).filter(|b| ![EXTENSION, IMAGE, TRAILER].contains(b)) {
                let mut bad = data.clone();
                bad.insert(at, junk);
                let (fixed, repair) = repair(&bad);
                let kept = composite_frames(&*fixed, |_, _| Ok(()));
                match repair {
                    Some(Repair::Junk { after, .. }) => {
                        assert_eq!(kept.unwrap().frame_count, after, "junk at {at}");
                    }
                    None => assert!(kept.is_err(), "junk at {at} before any frame"),
                    other => panic!("junk at {at}: {other:?}"),
                }
            }
        }
    }

    #[test]
    fn non_gifs_are_recognized() {
        let what = |data: &[u8]| not_gif(data).map(|n| n.to_string());
        assert_eq!(what(&gif(1)), None);
        assert_eq!(what(b"GIF8 broken"), None, "left for the decoder");
        let page = b"\xEF\xBB\xBF\r\n<!DOCTYPE html>\r\n<html><title>Page not found</title>";
        assert_eq!(
            what(page).unwrap(),
            "not a GIF: it is a web page (probably a saved error page)"
        );
        assert!(what(b"").unwrap().contains("empty"));
        assert!(what(b"\x89PNG\r\n\x1a\n").unwrap().contains("PNG"));
        assert!(what(b"\xFF\xD8\xFF\xE0").unwrap().contains("JPEG"));
        assert!(what(b"RIFF\0\0\0\0WEBPVP8 ").unwrap().contains("WebP"));
        assert!(what(b"\0\0\0\x20ftypisom").unwrap().contains("MP4"));
        assert!(what(b"\x1A\x45\xDF\xA3").unwrap().contains("WebM"));
        assert!(what(b"<svg>").unwrap().contains("not recognized"));
    }
}
