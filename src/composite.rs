//! Decodes a GIF into fully composited canvas frames, i.e. exactly what a
//! viewer displays for each frame after offsets, transparency and disposal
//! methods have been applied.

use std::io::{BufReader, Read};

use anyhow::{bail, Context, Result};
use gif::{ColorOutput, DecodeOptions, DisposalMethod, MemoryLimit, Repeat};

/// A canvas pixel packed as little-endian RGBA. Fully transparent pixels are
/// always normalized to `TRANSPARENT` so that equal-looking pixels compare equal.
pub type Pixel = u32;
pub const TRANSPARENT: Pixel = 0;

#[inline]
pub fn pack(r: u8, g: u8, b: u8, a: u8) -> Pixel {
    if a == 0 {
        TRANSPARENT
    } else {
        u32::from_le_bytes([r, g, b, 0xFF])
    }
}

#[inline]
pub fn rgb(p: Pixel) -> [u8; 3] {
    let [r, g, b, _] = p.to_le_bytes();
    [r, g, b]
}

pub struct GifInfo {
    pub width: u16,
    pub height: u16,
    pub repeat: Repeat,
    pub frame_count: usize,
}

/// Decodes `reader` and invokes `on_frame(canvas, delay)` for every displayed frame.
///
/// Compositing follows browser semantics: the canvas starts fully transparent,
/// `Any`/`Keep` leave the canvas as-is, `Background` clears the frame rectangle
/// to transparent, and `Previous` restores the canvas from before the frame.
pub fn composite_frames<R: Read>(
    reader: R,
    mut on_frame: impl FnMut(&[Pixel], u16) -> Result<()>,
) -> Result<GifInfo> {
    let mut opts = DecodeOptions::new();
    opts.set_color_output(ColorOutput::RGBA);
    opts.set_memory_limit(MemoryLimit::Unlimited);
    let mut decoder = opts
        .read_info(BufReader::new(reader))
        .context("failed to read GIF header")?;

    let width = decoder.width();
    let height = decoder.height();
    let (w, h) = (width as usize, height as usize);
    if w == 0 || h == 0 {
        bail!("GIF has a zero-sized canvas ({width}x{height})");
    }

    let mut canvas = vec![TRANSPARENT; w * h];
    let mut frame_count = 0;

    while let Some(frame) = decoder
        .read_next_frame()
        .with_context(|| format!("failed to decode frame {}", frame_count + 1))?
    {
        let (fl, ft) = (frame.left as usize, frame.top as usize);
        let (fw, fh) = (frame.width as usize, frame.height as usize);
        if frame.buffer.len() < fw * fh * 4 {
            bail!("frame {} has a truncated pixel buffer", frame_count + 1);
        }

        let saved = (frame.dispose == DisposalMethod::Previous).then(|| canvas.clone());

        // Visible (clipped) part of the frame rectangle.
        let x_end = (fl + fw).min(w);
        let y_end = (ft + fh).min(h);

        for cy in ft..y_end {
            let src_row = (cy - ft) * fw;
            for cx in fl..x_end {
                let s = (src_row + (cx - fl)) * 4;
                let px = &frame.buffer[s..s + 4];
                if px[3] != 0 {
                    canvas[cy * w + cx] = pack(px[0], px[1], px[2], px[3]);
                }
            }
        }

        on_frame(&canvas, frame.delay)?;
        frame_count += 1;

        match frame.dispose {
            DisposalMethod::Background => {
                for cy in ft..y_end {
                    canvas[cy * w + fl.min(w)..cy * w + x_end].fill(TRANSPARENT);
                }
            }
            DisposalMethod::Previous => {
                if let Some(saved) = saved {
                    canvas = saved;
                }
            }
            _ => {}
        }
    }

    Ok(GifInfo {
        width,
        height,
        repeat: decoder.repeat(),
        frame_count,
    })
}
