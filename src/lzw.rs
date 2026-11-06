//! GIF LZW encoder for pixels that may be written as one of two indices.
//!
//! Pixels that already show the right color on the canvas can be written
//! either as the transparent index (leaving the canvas as-is) or as their
//! real color. Picking whichever extends the current dictionary match gives
//! longer strings, and so shorter output, than fixing the choice up front.
//! Every choice decodes to the same image, so the result stays lossless.
//!
//! Clear codes are not tied to a full dictionary either: a search over
//! candidate positions picks where to clear it, which saves about 2% over
//! clearing at a fixed size.

/// Index to write for one pixel, with an optional equally valid alternative.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Choice {
    pub primary: u8,
    pub alt: Option<u8>,
}

impl Choice {
    pub fn fixed(index: u8) -> Self {
        Self {
            primary: index,
            alt: None,
        }
    }

    fn options(self) -> impl Iterator<Item = u8> {
        std::iter::once(self.primary).chain(self.alt)
    }
}

/// Number of codes a GIF dictionary can hold. Once it is full, encoding
/// continues with the dictionary as-is until the next clear code (a
/// "deferred clear"), so clear codes can go wherever they compress best.
const MAX_CODE: u16 = 4096;
/// The encoder behind many real-world GIFs resets its dictionary once it
/// holds codes up to 4092. Their pixel data (likely from lossy LZW) follows
/// that encoder's dictionary exactly and can need half as many codes when
/// the reset points line up, so those reset points are always candidates.
const COMMON_RESET: u16 = 4093;
/// How far ahead to look when deciding between two options.
const LOOKAHEAD: usize = 48;
/// Candidate clear positions are spaced so that about this many fall within
/// the pixels one dictionary typically covers...
const GRID_PER_SEGMENT: usize = 8;
/// ...but no closer than this many pixels.
const MIN_GRID_STEP: usize = 512;
/// A segment is not extended past its full dictionary by more than this
/// many times the pixels it took to fill it. Longer segments rarely pay off,
/// and the limit bounds the search time.
const MAX_FROZEN_SPAN: usize = 2;
/// A segment also stops at a candidate position once it is this many bits
/// worse than the best known way to reach it. This keeps most of the gain
/// of a full search at a fraction of the time.
const PRUNE_BITS: u64 = 2000;

struct Dict {
    /// `children[code * stride + index]` is the code for `code` followed by
    /// `index`, or 0 if absent (no real child code can be 0).
    children: Vec<u16>,
    stride: usize,
    /// Entries set since the last reset, so clearing is cheap.
    used: Vec<usize>,
}

impl Dict {
    fn new(min_size: u8) -> Self {
        let stride = 1usize << min_size;
        Self {
            children: vec![0; (MAX_CODE as usize + 1) * stride],
            stride,
            used: Vec::new(),
        }
    }

    fn child(&self, code: u16, index: u8) -> Option<u16> {
        match self.children[code as usize * self.stride + index as usize] {
            0 => None,
            c => Some(c),
        }
    }

    fn insert(&mut self, code: u16, index: u8, new: u16) {
        let slot = code as usize * self.stride + index as usize;
        self.children[slot] = new;
        self.used.push(slot);
    }

    fn reset(&mut self) {
        for slot in self.used.drain(..) {
            self.children[slot] = 0;
        }
    }

    /// Chooses between the options of `pixels[pos]`, given that the string
    /// being built is `code` (`None` when starting a new string). Returns
    /// `None` if neither option extends it.
    fn pick(&self, pixels: &[Choice], code: Option<u16>, pos: usize, last: u8) -> Option<u8> {
        let p = pixels[pos];
        let follow = |i: u8| match code {
            Some(c) => self.child(c, i),
            None => Some(u16::from(i)),
        };
        let Some(alt) = p.alt else {
            return follow(p.primary).map(|_| p.primary);
        };
        match (follow(p.primary), follow(alt)) {
            (Some(a), Some(b)) => {
                let (la, lb) = (
                    self.match_len(a, pixels, pos + 1),
                    self.match_len(b, pixels, pos + 1),
                );
                Some(match la.cmp(&lb) {
                    std::cmp::Ordering::Greater => p.primary,
                    std::cmp::Ordering::Less => alt,
                    // Repeating the previous index tends to build runs.
                    std::cmp::Ordering::Equal if alt == last => alt,
                    std::cmp::Ordering::Equal => p.primary,
                })
            }
            (Some(_), None) => Some(p.primary),
            (None, Some(_)) => Some(alt),
            (None, None) => None,
        }
    }

    /// Number of pixels from `pos` that greedily extend the string `code`.
    fn match_len(&self, mut code: u16, pixels: &[Choice], pos: usize) -> usize {
        let mut len = 0;
        for &p in pixels[pos..].iter().take(LOOKAHEAD) {
            match p.options().find_map(|i| self.child(code, i)) {
                Some(c) => code = c,
                None => break,
            }
            len += 1;
        }
        len
    }
}

struct BitWriter {
    out: Vec<u8>,
    acc: u32,
    bits: u8,
}

impl BitWriter {
    fn write(&mut self, code: u16, size: u8) {
        self.acc |= u32::from(code) << self.bits;
        self.bits += size;
        while self.bits >= 8 {
            self.out.push(self.acc as u8);
            self.acc >>= 8;
            self.bits -= 8;
        }
    }

    fn finish(mut self) -> Vec<u8> {
        if self.bits > 0 {
            self.out.push(self.acc as u8);
        }
        self.out
    }
}

/// Smallest valid GIF minimum code size that can hold every index.
pub fn min_code_size(pixels: &[Choice]) -> u8 {
    let max = pixels.iter().flat_map(|p| p.options()).max().unwrap_or(0);
    (u32::from(max) + 1)
        .next_power_of_two()
        .trailing_zeros()
        .max(2) as u8
}

/// Encodes `pixels` and returns the minimum code size byte followed by the
/// LZW stream, as expected by `gif::Encoder::write_lzw_pre_encoded_frame`.
///
/// Also tries writing every pixel with its real color where it has one,
/// since that can recreate the source encoder's exact strings (see
/// `COMMON_RESET`), and returns whichever result is shorter.
pub fn encode(pixels: &[Choice]) -> Vec<u8> {
    if pixels.iter().all(|p| p.alt.is_none()) {
        return encode_flexible(pixels);
    }
    let colors: Vec<Choice> = pixels
        .iter()
        .map(|p| Choice::fixed(p.alt.unwrap_or(p.primary)))
        .collect();
    // The search is slow, so it only refines the more promising variant.
    if chain_cost(&colors) < chain_cost(pixels) {
        encode_flexible(&colors)
    } else {
        encode_flexible(pixels)
    }
}

/// Bits needed when clearing the dictionary at every `COMMON_RESET`.
fn chain_cost(pixels: &[Choice]) -> u64 {
    let min_size = min_code_size(pixels);
    let mut dict = Dict::new(min_size);
    let (mut start, mut total) = (0, 0);
    while start < pixels.len() {
        let mut last = 0;
        let end = segment(&mut dict, pixels, min_size, start, None, |_, next, cost| {
            last = cost;
            next <= COMMON_RESET
        });
        match end.common_reset {
            Some((pos, cost)) => {
                total += cost;
                start = pos;
            }
            None => return total + last,
        }
    }
    total
}

/// How a segment ended.
struct SegmentEnd {
    /// Code size for the clear or end code that follows the segment.
    end_size: u8,
    /// Position and cost (see `segment`) where the dictionary reached
    /// `COMMON_RESET`, if it did.
    common_reset: Option<(usize, u64)>,
}

/// Encodes pixels from `start` with a fresh dictionary, writing codes to `w`
/// if given. `visit(pos, next, cost)` is called before each position from
/// `start + 1` to the end, where `next` is the next free code and `cost` is
/// the number of bits the segment takes if it ends there, including its
/// final clear or end code. The segment ends when `visit` returns false or
/// all pixels are encoded; the final code is written, but not the code
/// after it.
fn segment(
    dict: &mut Dict,
    pixels: &[Choice],
    min_size: u8,
    start: usize,
    mut w: Option<&mut BitWriter>,
    mut visit: impl FnMut(usize, u16, u64) -> bool,
) -> SegmentEnd {
    let eoi = (1u16 << min_size) + 1;
    dict.reset();
    let mut size = min_size + 1;
    let mut next = eoi + 1;
    let mut bits = 0u64;
    let mut common_reset = None;
    // The first index of a segment never depends on what came before, so
    // the segment encodes the same however it is reached.
    let mut last = dict
        .pick(pixels, None, start, pixels[start].primary)
        .unwrap();
    let mut code = u16::from(last);
    let mut pos = start + 1;
    loop {
        let end_size = size + u8::from(next >= 1 << size && size < 12);
        let cost = bits + u64::from(size + end_size);
        if !visit(pos, next, cost) || pos == pixels.len() {
            if let Some(w) = w {
                w.write(code, size);
            }
            return SegmentEnd {
                end_size,
                common_reset,
            };
        }
        if let Some(i) = dict.pick(pixels, Some(code), pos, last) {
            code = dict.child(code, i).unwrap();
            last = i;
            pos += 1;
            continue;
        }
        if next == COMMON_RESET && common_reset.is_none() {
            common_reset = Some((pos, cost));
        }
        if let Some(w) = w.as_deref_mut() {
            w.write(code, size);
        }
        bits += u64::from(size);
        let s = dict.pick(pixels, None, pos, last).unwrap();
        if next < MAX_CODE {
            if next >= 1 << size && size < 12 {
                size += 1;
            }
            dict.insert(code, s, next);
            next += 1;
        }
        code = u16::from(s);
        last = s;
        pos += 1;
    }
}

/// A position where a segment can start, in the search for clear positions.
#[derive(Clone, Copy)]
struct Node {
    /// Fewest bits that encode all pixels before this position.
    cost: u64,
    /// Start of the last segment on that cheapest path.
    from: usize,
    /// Whether this is on the chain of `COMMON_RESET` resets from the start.
    chain: bool,
}

/// Encodes `pixels`, choosing where to clear the dictionary by finding the
/// cheapest split into segments among candidate positions.
fn encode_flexible(pixels: &[Choice]) -> Vec<u8> {
    use std::collections::BTreeMap;

    let n = pixels.len();
    let min_size = min_code_size(pixels);
    let clear: u16 = 1 << min_size;
    let eoi = clear + 1;
    let mut dict = Dict::new(min_size);
    let mut w = BitWriter {
        out: vec![min_size],
        acc: 0,
        bits: 0,
    };
    w.write(clear, min_size + 1);
    if n == 0 {
        w.write(eoi, min_size + 1);
        return w.finish();
    }

    // Pixels the first dictionary covers before it is full, to size the grid.
    let mut fill = None;
    segment(&mut dict, pixels, min_size, 0, None, |pos, next, _| {
        if next == MAX_CODE {
            fill = Some(pos);
        }
        fill.is_none()
    });
    let mut ends = vec![n];
    if let Some(fill) = fill {
        let step = (fill / GRID_PER_SEGMENT).max(MIN_GRID_STEP);
        let mut nodes = BTreeMap::from([(
            0,
            Node {
                cost: 0,
                from: 0,
                chain: true,
            },
        )]);
        let mut chain: Vec<usize> = Vec::new();
        let mut edges: Vec<(usize, u64)> = Vec::new();
        let mut at = 0;
        while at < n {
            let node = nodes[&at];
            let targets = &chain[chain.partition_point(|&c| c <= at)..];
            let mut target = targets.iter().copied().peekable();
            let mut full = None;
            edges.clear();
            let end = segment(&mut dict, pixels, min_size, at, None, |pos, next, cost| {
                if next == MAX_CODE && full.is_none() {
                    full = Some(pos);
                }
                while target.next_if(|&t| t < pos).is_some() {}
                let mut more = full.is_none_or(|f| pos - f <= MAX_FROZEN_SPAN * (f - at));
                if more && full.is_some() && pos % step == 0 {
                    if let Some(b) = nodes.get(&pos) {
                        more = node.cost + cost <= b.cost.saturating_add(PRUNE_BITS);
                    }
                }
                // Stopping always leaves a node to continue from.
                if !more || pos % step == 0 || pos == n || target.peek() == Some(&pos) {
                    edges.push((pos, cost));
                }
                more
            });
            if node.chain {
                if let Some((pos, cost)) = end.common_reset {
                    edges.push((pos, cost));
                    chain.push(pos);
                }
            }
            for &(pos, cost) in &edges {
                let cost = node.cost + cost;
                let is_chain = node.chain && end.common_reset.is_some_and(|(p, _)| p == pos);
                let e = nodes.entry(pos).or_insert(Node {
                    cost: u64::MAX,
                    from: at,
                    chain: false,
                });
                if cost < e.cost {
                    e.cost = cost;
                    e.from = at;
                }
                e.chain |= is_chain;
            }
            at = *nodes.range(at + 1..).next().unwrap().0;
        }
        ends.clear();
        let mut pos = n;
        while pos > 0 {
            ends.push(pos);
            pos = nodes[&pos].from;
        }
        ends.reverse();
    }

    let mut start = 0;
    for end in ends {
        let seg = segment(
            &mut dict,
            pixels,
            min_size,
            start,
            Some(&mut w),
            |pos, _, _| pos < end,
        );
        w.write(if end == n { eoi } else { clear }, seg.end_size);
        start = end;
    }
    w.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode(data: &[u8]) -> Vec<u8> {
        let mut dec = weezl::decode::Decoder::new(weezl::BitOrder::Lsb, data[0]);
        dec.decode(&data[1..]).unwrap()
    }

    fn check(pixels: &[Choice]) {
        let out = decode(&encode(pixels));
        assert_eq!(out.len(), pixels.len());
        for (o, p) in out.iter().zip(pixels) {
            assert!(p.options().any(|i| i == *o), "{o} not in {p:?}");
        }
    }

    #[test]
    fn round_trips_fixed_pixels() {
        let mut s = 7u32;
        let pixels: Vec<Choice> = (0..200_000)
            .map(|_| {
                s = s.wrapping_mul(1_103_515_245).wrapping_add(12_345);
                Choice::fixed(((s >> 16) % 200) as u8)
            })
            .collect();
        check(&pixels);
        check(&pixels[..1]);
        check(&[]);
    }

    #[test]
    fn round_trips_repeated_noise() {
        // A full dictionary that keeps matching makes clearing it late pay
        // off, so this exercises deferred clears.
        let mut s = 11u32;
        let block: Vec<Choice> = (0..30_000)
            .map(|_| {
                s = s.wrapping_mul(1_103_515_245).wrapping_add(12_345);
                Choice::fixed(((s >> 16) % 16) as u8)
            })
            .collect();
        let pixels: Vec<Choice> = block
            .iter()
            .cycle()
            .take(block.len() * 12)
            .copied()
            .collect();
        check(&pixels);
    }

    #[test]
    fn round_trips_choices_and_beats_fixed() {
        let mut s = 3u32;
        let pixels: Vec<Choice> = (0..100_000)
            .map(|i| {
                s = s.wrapping_mul(1_103_515_245).wrapping_add(12_345);
                let color = ((i / 7) % 13) as u8;
                if (s >> 16).is_multiple_of(3) {
                    Choice::fixed(color)
                } else {
                    Choice {
                        primary: color,
                        alt: Some(15),
                    }
                }
            })
            .collect();
        check(&pixels);
        let fixed: Vec<Choice> = pixels
            .iter()
            .map(|p| Choice::fixed(p.alt.unwrap_or(p.primary)))
            .collect();
        assert!(encode_flexible(&pixels).len() < encode_flexible(&fixed).len());
    }
}
