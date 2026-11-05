//! Lossless GIF re-encoder.
//!
//! Every output frame only contains the rectangle that changed since the
//! previous frame. Unchanged pixels are written as transparent or as their
//! real color, whichever compresses better (see `lzw`). Disposal methods are chosen so any pixel that
//! must become transparent in the next frame gets cleared. No colors are ever
//! quantized: if a frame cannot be expressed exactly, encoding fails instead.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};

use anyhow::{bail, Context, Result};
use gif::{DisposalMethod, Encoder, Frame, Repeat};

use crate::composite::{composite_frames, rgb, Pixel, TRANSPARENT};
use crate::lzw::{self, Choice};
use crate::store::FrameStore;
use crate::timeline::TimelineFrame;

const MAX_COLORS: usize = 256;

#[derive(Clone, Copy, Debug)]
struct Rect {
    x: usize,
    y: usize,
    w: usize,
    h: usize,
}

impl Rect {
    fn union(self, o: Rect) -> Rect {
        let x = self.x.min(o.x);
        let y = self.y.min(o.y);
        let x_end = (self.x + self.w).max(o.x + o.w);
        let y_end = (self.y + self.h).max(o.y + o.h);
        Rect {
            x,
            y,
            w: x_end - x,
            h: y_end - y,
        }
    }

    fn indices(self, canvas_w: usize) -> impl Iterator<Item = usize> {
        (self.y..self.y + self.h)
            .flat_map(move |y| (self.x..self.x + self.w).map(move |x| y * canvas_w + x))
    }
}

/// Bounding box of all canvas indices matching `pred`.
fn bbox(w: usize, h: usize, pred: impl Fn(usize) -> bool) -> Option<Rect> {
    let (mut x0, mut y0, mut x1, mut y1) = (usize::MAX, usize::MAX, 0, 0);
    for y in 0..h {
        let row = y * w;
        for x in 0..w {
            if pred(row + x) {
                x0 = x0.min(x);
                x1 = x1.max(x);
                y0 = y0.min(y);
                y1 = y1.max(y);
            }
        }
    }
    (x0 != usize::MAX).then(|| Rect {
        x: x0,
        y: y0,
        w: x1 - x0 + 1,
        h: y1 - y0 + 1,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    /// Pixels identical to the underlying canvas are written as transparent.
    Delta,
    /// Every pixel of the rectangle is written with its real color.
    /// Used when the delta variant would need one color too many.
    Full,
}

pub struct FramePlan {
    rect: Rect,
    dispose: DisposalMethod,
    mode: Mode,
    /// Sorted distinct opaque colors used by the frame.
    colors: Vec<Pixel>,
    /// Whether the frame needs a transparent palette entry.
    transparent: bool,
}

/// Pixel values to write for `rect`; `TRANSPARENT` means "leave canvas as-is".
fn symbols(
    target: &[Pixel],
    base: &[Pixel],
    canvas_w: usize,
    rect: Rect,
    mode: Mode,
) -> Vec<Pixel> {
    rect.indices(canvas_w)
        .map(|i| match mode {
            Mode::Delta if target[i] == base[i] => TRANSPARENT,
            _ => target[i],
        })
        .collect()
}

fn color_set(symbols: &[Pixel]) -> (Vec<Pixel>, bool) {
    let set: HashSet<Pixel> = symbols.iter().copied().collect();
    let transparent = set.contains(&TRANSPARENT);
    let mut colors: Vec<Pixel> = set.into_iter().filter(|&p| p != TRANSPARENT).collect();
    colors.sort_unstable();
    (colors, transparent)
}

fn clear_rect(canvas: &mut [Pixel], canvas_w: usize, rect: Rect) {
    for y in rect.y..rect.y + rect.h {
        canvas[y * canvas_w + rect.x..y * canvas_w + rect.x + rect.w].fill(TRANSPARENT);
    }
}

/// Decides rectangle, disposal method and palette mode for every output frame.
pub fn plan_frames(
    store: &FrameStore,
    timeline: &[TimelineFrame],
    width: u16,
    height: u16,
) -> Result<Vec<FramePlan>> {
    let (w, h) = (width as usize, height as usize);
    let mut plans = Vec::with_capacity(timeline.len());
    // Canvas the next frame is drawn onto. Viewers start (and restart each
    // loop) from a fully transparent canvas.
    let mut base = vec![TRANSPARENT; w * h];
    let mut current = store.load(timeline[0].id)?;

    for k in 0..timeline.len() {
        let next = match timeline.get(k + 1) {
            Some(f) => Some(store.load(f.id)?),
            None => None,
        };

        if (0..w * h).any(|i| current[i] == TRANSPARENT && base[i] != TRANSPARENT) {
            bail!("internal error: frame {} would need to erase pixels", k + 1);
        }

        let changed = bbox(w, h, |i| current[i] != base[i]);
        // Opaque pixels that turn transparent in the next frame can only be
        // erased by this frame's "restore to background" disposal, so this
        // frame's rectangle must cover them.
        let must_clear = next
            .as_ref()
            .and_then(|nx| bbox(w, h, |i| current[i] != TRANSPARENT && nx[i] == TRANSPARENT));

        let rect = match (changed, must_clear) {
            (Some(a), Some(b)) => a.union(b),
            (Some(a), None) | (None, Some(a)) => a,
            // Nothing changed (only possible for timing-only frames): GIF still
            // needs at least one pixel, which is written as a no-op.
            (None, None) => Rect {
                x: 0,
                y: 0,
                w: 1,
                h: 1,
            },
        };
        let dispose = if must_clear.is_some() {
            DisposalMethod::Background
        } else {
            DisposalMethod::Keep
        };

        let (delta_colors, delta_t) = color_set(&symbols(&current, &base, w, rect, Mode::Delta));
        let (mode, colors, transparent) = if delta_colors.len() + usize::from(delta_t) <= MAX_COLORS
        {
            (Mode::Delta, delta_colors, delta_t)
        } else {
            let (full_colors, full_t) = color_set(&symbols(&current, &base, w, rect, Mode::Full));
            let needed = full_colors.len() + usize::from(full_t);
            if needed > MAX_COLORS {
                bail!(
                    "frame {} needs {needed} colors, more than one GIF frame can hold losslessly",
                    k + 1
                );
            }
            (Mode::Full, full_colors, full_t)
        };

        plans.push(FramePlan {
            rect,
            dispose,
            mode,
            colors,
            transparent,
        });

        base.copy_from_slice(&current);
        if dispose == DisposalMethod::Background {
            clear_rect(&mut base, w, rect);
        }
        if let Some(nx) = next {
            current = nx;
        }
    }

    Ok(plans)
}

/// A single palette shared by all frames, if the whole animation fits in 256 entries.
pub struct GlobalPalette {
    map: HashMap<Pixel, u8>,
    rgb: Vec<u8>,
    transparent: Option<u8>,
}

pub fn global_palette(plans: &[FramePlan]) -> Option<GlobalPalette> {
    let mut all: Vec<Pixel> = plans
        .iter()
        .flat_map(|p| p.colors.iter().copied())
        .collect();
    all.sort_unstable();
    all.dedup();
    let needs_t = plans.iter().any(|p| p.transparent);
    if all.len() + usize::from(needs_t) > MAX_COLORS {
        return None;
    }
    let map = all.iter().enumerate().map(|(i, &c)| (c, i as u8)).collect();
    let mut rgb_table: Vec<u8> = all.iter().flat_map(|&c| rgb(c)).collect();
    let transparent = needs_t.then(|| {
        rgb_table.extend_from_slice(&[0, 0, 0]);
        all.len() as u8
    });
    Some(GlobalPalette {
        map,
        rgb: rgb_table,
        transparent,
    })
}

pub fn encode(
    store: &FrameStore,
    timeline: &[TimelineFrame],
    plans: &[FramePlan],
    width: u16,
    height: u16,
    repeat: Repeat,
    global: Option<&GlobalPalette>,
) -> Result<Vec<u8>> {
    let w = width as usize;
    let mut encoder = Encoder::new(
        Vec::new(),
        width,
        height,
        global.map_or(&[][..], |g| g.rgb.as_slice()),
    )?;
    encoder.set_repeat(repeat)?;

    let mut base = vec![TRANSPARENT; w * height as usize];

    for (k, (tf, plan)) in timeline.iter().zip(plans).enumerate() {
        let current = store.load(tf.id)?;
        let syms = symbols(&current, &base, w, plan.rect, plan.mode);

        let local_map: HashMap<Pixel, u8>;
        let (map, transparent, palette) = match global {
            Some(g) => (&g.map, g.transparent.filter(|_| plan.transparent), None),
            None => {
                local_map = plan
                    .colors
                    .iter()
                    .enumerate()
                    .map(|(i, &c)| (c, i as u8))
                    .collect();
                let mut table: Vec<u8> = plan.colors.iter().flat_map(|&c| rgb(c)).collect();
                let t = plan.transparent.then(|| {
                    table.extend_from_slice(&[0, 0, 0]);
                    plan.colors.len() as u8
                });
                (&local_map, t, Some(table))
            }
        };

        // Unchanged pixels may be written as transparent or, if the palette
        // has it, as their real color; the LZW encoder picks per pixel.
        let choices = syms
            .iter()
            .zip(plan.rect.indices(w))
            .map(|(s, i)| {
                if *s != TRANSPARENT {
                    return map.get(s).map(|&c| Choice::fixed(c));
                }
                let t = transparent?;
                let alt = (current[i] != TRANSPARENT)
                    .then(|| map.get(&current[i]).copied())
                    .flatten();
                Some(Choice { primary: t, alt })
            })
            .collect::<Option<Vec<Choice>>>()
            .with_context(|| format!("internal error: palette mismatch in frame {}", k + 1))?;

        let frame = Frame {
            delay: tf.delay,
            dispose: plan.dispose,
            transparent,
            left: plan.rect.x as u16,
            top: plan.rect.y as u16,
            width: plan.rect.w as u16,
            height: plan.rect.h as u16,
            palette,
            buffer: Cow::Owned(lzw::encode(&choices)),
            ..Frame::default()
        };
        encoder.write_lzw_pre_encoded_frame(&frame)?;

        base.copy_from_slice(&current);
        if plan.dispose == DisposalMethod::Background {
            clear_rect(&mut base, w, plan.rect);
        }
    }

    Ok(encoder.into_inner()?)
}

/// Decodes the encoded output and checks it against the expected timeline
/// pixel for pixel, including delays and loop count. The timeline refers to
/// stored frames, so after look-alike grouping each frame is checked against
/// its group's representative rather than the original frame it replaced.
pub fn verify(
    encoded: &[u8],
    store: &FrameStore,
    timeline: &[TimelineFrame],
    width: u16,
    height: u16,
    repeat: Repeat,
) -> Result<()> {
    let mut idx = 0;
    let info = composite_frames(encoded, |canvas, delay| {
        let Some(expected) = timeline.get(idx) else {
            bail!("output has more frames than expected");
        };
        if delay != expected.delay {
            bail!(
                "frame {} delay is {delay}, expected {}",
                idx + 1,
                expected.delay
            );
        }
        if store.load(expected.id)? != canvas {
            bail!("frame {} pixels differ from the original", idx + 1);
        }
        idx += 1;
        Ok(())
    })?;

    if info.frame_count != timeline.len() {
        bail!(
            "output has {} frames, expected {}",
            info.frame_count,
            timeline.len()
        );
    }
    if (info.width, info.height) != (width, height) {
        bail!("output canvas size differs from the original");
    }
    if info.repeat != repeat {
        bail!(
            "output loop count {:?} differs from expected {:?}",
            info.repeat,
            repeat
        );
    }
    Ok(())
}
