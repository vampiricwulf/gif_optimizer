//! Perceptual frame comparison.
//!
//! GIFs often repeat the same image with different dithering: scattered
//! pixels flipped between neighbouring palette colours. Such frames look
//! identical but never compare equal, which hides the repetition.
//!
//! Two frames count as visually identical when the colour difference,
//! averaged over local windows, stays small at every scale:
//!
//! * small windows catch real detail changes (a moved line, a blink),
//! * large windows catch faint changes over a wider area,
//! * the whole-frame average catches global tints and fades.
//!
//! Dithering noise averages out at all scales, while any visible change
//! survives in at least one of them. Changes to a handful of isolated pixels
//! are treated as noise. Transparency must match exactly.

use anyhow::Result;

use crate::composite::{Pixel, TRANSPARENT};
use crate::store::FrameStore;

/// Radius used to mean "the whole frame".
const WHOLE_FRAME: usize = usize::MAX;

/// `(window radius, max mean channel difference on a 0-255 scale)`.
/// Tuned on real dithered GIFs: repeats of the same image measured at most
/// 17 / 3.6 / 0.1, while genuinely different frames measured 195 / 42 / 1.1.
const LIMITS: [(usize, f32); 3] = [(3, 28.0), (12, 8.0), (WHOLE_FRAME, 1.5)];

/// Side length of the blocks in a `Signature`: exactly one smallest window.
const BLOCK: usize = 2 * LIMITS[0].0 + 1;

/// Per-block colour means, used to rule out most non-matching frames
/// without loading them from disk.
pub struct Signature {
    blocks: Vec<[f32; 3]>,
}

impl Signature {
    pub fn new(canvas: &[Pixel], w: usize, h: usize) -> Self {
        // Only full blocks: each is exactly a window of the smallest radius,
        // so a frame within `LIMITS[0]` can never be rejected by `may_match`.
        let (bw, bh) = (w / BLOCK, h / BLOCK);
        let mut blocks = Vec::with_capacity(bw * bh);
        let n = (BLOCK * BLOCK) as f32;
        for by in 0..bh {
            for bx in 0..bw {
                let mut sum = [0u32; 3];
                for y in by * BLOCK..(by + 1) * BLOCK {
                    for p in &canvas[y * w + bx * BLOCK..y * w + (bx + 1) * BLOCK] {
                        let c = p.to_le_bytes();
                        for i in 0..3 {
                            sum[i] += u32::from(c[i]);
                        }
                    }
                }
                blocks.push(sum.map(|s| s as f32 / n));
            }
        }
        Self { blocks }
    }

    pub fn may_match(&self, other: &Signature) -> bool {
        self.blocks
            .iter()
            .zip(&other.blocks)
            .all(|(a, b)| (0..3).all(|i| (a[i] - b[i]).abs() <= LIMITS[0].1 + 0.01))
    }
}

/// Maps every stored frame id to the id of the earliest stored frame that is
/// visually identical to it (possibly itself). Each frame is compared with
/// that representative directly, so matches never drift through chains of
/// slightly different frames.
pub fn group_similar(store: &FrameStore, w: usize, h: usize) -> Result<Vec<usize>> {
    let mut reps: Vec<(usize, Signature)> = Vec::new();
    let mut map = Vec::with_capacity(store.unique_count());
    for id in 0..store.unique_count() {
        let frame = store.load(id)?;
        let sig = Signature::new(&frame, w, h);
        let mut found = None;
        for (rep, rep_sig) in &reps {
            if sig.may_match(rep_sig) && visually_identical(&frame, &store.load(*rep)?, w, h) {
                found = Some(*rep);
                break;
            }
        }
        map.push(found.unwrap_or(id));
        if found.is_none() {
            reps.push((id, sig));
        }
    }
    Ok(map)
}

pub fn visually_identical(a: &[Pixel], b: &[Pixel], w: usize, h: usize) -> bool {
    if a == b {
        return true;
    }
    let Some(sat) = summed_diff(a, b, w, h) else {
        return false;
    };
    LIMITS
        .iter()
        .all(|&(r, limit)| max_window_mean(&sat, w, h, r, limit) <= limit)
}

/// Summed-area table of the signed per-channel difference `a - b`, with an
/// extra zero row and column. `None` if transparency differs anywhere.
fn summed_diff(a: &[Pixel], b: &[Pixel], w: usize, h: usize) -> Option<Vec<[i64; 3]>> {
    let stride = w + 1;
    let mut sat = vec![[0i64; 3]; stride * (h + 1)];
    for y in 0..h {
        let mut row = [0i64; 3];
        for x in 0..w {
            let (pa, pb) = (a[y * w + x], b[y * w + x]);
            if (pa == TRANSPARENT) != (pb == TRANSPARENT) {
                return None;
            }
            let (ca, cb) = (pa.to_le_bytes(), pb.to_le_bytes());
            let above = sat[y * stride + x + 1];
            let cell = &mut sat[(y + 1) * stride + x + 1];
            for c in 0..3 {
                row[c] += i64::from(ca[c]) - i64::from(cb[c]);
                cell[c] = above[c] + row[c];
            }
        }
    }
    Some(sat)
}

/// Largest |mean difference| over all windows of radius `r` (clipped at the
/// edges). Stops early once `limit` is exceeded.
fn max_window_mean(sat: &[[i64; 3]], w: usize, h: usize, r: usize, limit: f32) -> f32 {
    let stride = w + 1;
    let mean = |x0: usize, y0: usize, x1: usize, y1: usize| {
        let area = ((y1 - y0) * (x1 - x0)) as f32;
        (0..3)
            .map(|c| {
                let sum =
                    sat[y1 * stride + x1][c] - sat[y0 * stride + x1][c] - sat[y1 * stride + x0][c]
                        + sat[y0 * stride + x0][c];
                sum.unsigned_abs() as f32 / area
            })
            .fold(0.0, f32::max)
    };

    if r >= w.max(h) {
        return mean(0, 0, w, h);
    }
    let mut max = 0.0f32;
    for y in 0..h {
        let (y0, y1) = (y.saturating_sub(r), (y + r + 1).min(h));
        for x in 0..w {
            let (x0, x1) = (x.saturating_sub(r), (x + r + 1).min(w));
            max = max.max(mean(x0, y0, x1, y1));
        }
        if max > limit {
            break;
        }
    }
    max
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::composite::pack;

    const W: usize = 64;
    const H: usize = 64;

    fn flat(v: u8) -> Vec<Pixel> {
        vec![pack(v, v, v, 255); W * H]
    }

    /// Deterministic dither: shifts about a quarter of the pixels by ±`amp`.
    fn dithered(base: &[Pixel], amp: i16, seed: u32) -> Vec<Pixel> {
        let mut s = seed;
        base.iter()
            .map(|&p| {
                s = s.wrapping_mul(1_103_515_245).wrapping_add(12_345);
                let [r, g, b, _] = p.to_le_bytes();
                let d = match (s >> 16) % 8 {
                    0 => amp,
                    1 => -amp,
                    _ => 0,
                };
                let f = |c: u8| (i16::from(c) + d).clamp(0, 255) as u8;
                pack(f(r), f(g), f(b), 255)
            })
            .collect()
    }

    fn same(a: &[Pixel], b: &[Pixel]) -> bool {
        let matched = visually_identical(a, b, W, H);
        if matched {
            assert!(Signature::new(a, W, H).may_match(&Signature::new(b, W, H)));
        }
        matched
    }

    #[test]
    fn dither_noise_is_identical() {
        let base = flat(128);
        assert!(same(&dithered(&base, 40, 1), &dithered(&base, 40, 2)));
    }

    #[test]
    fn global_fade_is_different() {
        assert!(!same(&flat(128), &flat(132)));
    }

    #[test]
    fn thin_line_change_is_different() {
        let a = flat(255);
        let mut b = a.clone();
        for y in 10..30 {
            b[y * W + 20] = pack(0, 0, 0, 255);
        }
        assert!(!same(&a, &b));
    }

    #[test]
    fn small_spot_change_is_different() {
        let a = flat(255);
        let mut b = a.clone();
        for y in 30..33 {
            for x in 30..33 {
                b[y * W + x] = pack(0, 0, 0, 255);
            }
        }
        assert!(!same(&a, &b));
    }

    #[test]
    fn transparency_change_is_different() {
        let a = flat(255);
        let mut b = a.clone();
        b[0] = TRANSPARENT;
        assert!(!same(&a, &b));
    }
}
