//! GIF LZW encoder for pixels that may be written as one of two indices.
//!
//! Pixels that already show the right color on the canvas can be written
//! either as the transparent index (leaving the canvas as-is) or as their
//! real color. Picking whichever extends the current dictionary match gives
//! longer strings, and so shorter output, than fixing the choice up front.
//! Every choice decodes to the same image, so the result stays lossless.

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

const MAX_CODE: u16 = 4095;
/// How far ahead to look when deciding between two options.
const LOOKAHEAD: usize = 48;

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
pub fn encode(pixels: &[Choice]) -> Vec<u8> {
    let min_size = min_code_size(pixels);
    let clear: u16 = 1 << min_size;
    let eoi = clear + 1;
    let mut dict = Dict::new(min_size);
    let mut w = BitWriter {
        out: vec![min_size],
        acc: 0,
        bits: 0,
    };
    let mut size = min_size + 1;
    let mut next = eoi + 1;
    w.write(clear, size);

    let Some(&first) = pixels.first() else {
        w.write(eoi, size);
        return w.finish();
    };

    // Chooses between the options of `pixels[pos]`, given that the string
    // being built is `code` (`None` when starting a new string).
    let pick = |dict: &Dict, code: Option<u16>, pos: usize, last: u8| -> Option<u8> {
        let p = pixels[pos];
        let follow = |i: u8| match code {
            Some(c) => dict.child(c, i),
            None => Some(u16::from(i)),
        };
        let Some(alt) = p.alt else {
            return follow(p.primary).map(|_| p.primary);
        };
        match (follow(p.primary), follow(alt)) {
            (Some(a), Some(b)) => {
                let (la, lb) = (
                    dict.match_len(a, pixels, pos + 1),
                    dict.match_len(b, pixels, pos + 1),
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
    };

    let mut last = pick(&dict, None, 0, first.primary).unwrap_or(first.primary);
    let mut code = u16::from(last);
    for pos in 1..pixels.len() {
        if let Some(i) = pick(&dict, Some(code), pos, last) {
            code = dict.child(code, i).unwrap();
            last = i;
            continue;
        }
        w.write(code, size);
        let start = pick(&dict, None, pos, last).unwrap();
        if next < MAX_CODE {
            if next >= 1 << size && size < 12 {
                size += 1;
            }
            dict.insert(code, start, next);
            next += 1;
        } else {
            w.write(clear, size);
            dict.reset();
            size = min_size + 1;
            next = eoi + 1;
        }
        code = u16::from(start);
        last = start;
    }
    w.write(code, size);
    if next >= 1 << size && size < 12 {
        size += 1;
    }
    w.write(eoi, size);
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
        assert!(encode(&pixels).len() < encode(&fixed).len());
    }
}
