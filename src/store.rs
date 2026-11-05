//! On-disk store of unique composited frames, kept in the temp directory so
//! large GIFs don't have to be held in memory all at once.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::composite::Pixel;

pub struct FrameStore {
    dir: PathBuf,
    by_hash: HashMap<u64, Vec<usize>>,
    count: usize,
}

impl FrameStore {
    pub fn new(dir: &Path) -> Result<Self> {
        fs::create_dir_all(dir).with_context(|| format!("failed to create {}", dir.display()))?;
        Ok(Self {
            dir: dir.to_path_buf(),
            by_hash: HashMap::new(),
            count: 0,
        })
    }

    /// Returns the id of an identical, already stored frame, or stores it as a new one.
    pub fn insert(&mut self, canvas: &[Pixel]) -> Result<usize> {
        let hash = hash_canvas(canvas);
        if let Some(candidates) = self.by_hash.get(&hash) {
            // Hashes only narrow the search; equality is always confirmed pixel by pixel.
            for &id in candidates {
                if self.load(id)? == canvas {
                    return Ok(id);
                }
            }
        }

        let id = self.count;
        let mut bytes = Vec::with_capacity(canvas.len() * 4);
        for p in canvas {
            bytes.extend_from_slice(&p.to_le_bytes());
        }
        let path = self.path(id);
        fs::write(&path, bytes).with_context(|| format!("failed to write {}", path.display()))?;

        self.by_hash.entry(hash).or_default().push(id);
        self.count += 1;
        Ok(id)
    }

    pub fn load(&self, id: usize) -> Result<Vec<Pixel>> {
        let path = self.path(id);
        let bytes =
            fs::read(&path).with_context(|| format!("failed to read {}", path.display()))?;
        Ok(bytes
            .as_chunks::<4>()
            .0
            .iter()
            .map(|&c| u32::from_le_bytes(c))
            .collect())
    }

    pub fn unique_count(&self) -> usize {
        self.count
    }

    fn path(&self, id: usize) -> PathBuf {
        self.dir.join(format!("frame_{id:06}.rgba"))
    }
}

fn hash_canvas(canvas: &[Pixel]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &p in canvas {
        h = (h.rotate_left(5) ^ u64::from(p)).wrapping_mul(0x517c_c1b7_2722_0a95);
    }
    h
}
