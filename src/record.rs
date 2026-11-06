//! Remembers which GIFs are already optimized, so later runs skip them.
//!
//! Two independent ways, so that either one is enough:
//!
//! * A marker inside every GIF the program writes: a small application
//!   extension right after the header, which viewers ignore. It travels
//!   with the file, wherever it is copied or moved.
//! * A record file next to the executable listing the size and a content
//!   hash of every finished file, with its path and modified time so that
//!   unchanged files are recognized without reading them. It also covers
//!   files that were left as they were because nothing could be saved, and
//!   recognizes files by content after they are moved or renamed.
//!
//! Both note whether the file was finished losslessly, since a later run
//! that allows merging look-alike frames may still shrink it.

use std::collections::HashMap;
use std::fs::{self, File, Metadata, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::UNIX_EPOCH;

use crate::repair::{color_table_len, skip_sub_blocks};

/// The record file, next to the executable.
pub const DONE_FILE: &str = "gif_optimizer.done";
const HEADER: &str = "# gif_optimizer done list v1: hash, size, mode, modified (ns), path";

/// How a file was optimized.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Only pixel-identical frames were merged.
    Lossless,
    /// Frames that only look identical could be merged too.
    Lossy,
}

impl Mode {
    /// Whether a file finished in this mode needs no run in `wanted` mode.
    /// A lossless result may still shrink if look-alikes may be merged.
    pub fn covers(self, wanted: Mode) -> bool {
        self == Mode::Lossy || wanted == Mode::Lossless
    }

    fn code(self) -> u8 {
        match self {
            Mode::Lossless => b'L',
            Mode::Lossy => b'Y',
        }
    }

    fn from_code(code: u8) -> Option<Mode> {
        match code {
            b'L' => Some(Mode::Lossless),
            b'Y' => Some(Mode::Lossy),
            _ => None,
        }
    }
}

/// A 64-bit hash of `data`. It is stored in the record, so it must never
/// change between versions (unlike the standard library's hashers).
pub fn hash(data: &[u8]) -> u64 {
    const K: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut h = 0x243F_6A88_85A3_08D3 ^ (data.len() as u64).wrapping_mul(K);
    let (words, rest) = data.as_chunks::<8>();
    for w in words {
        h = (h ^ u64::from_le_bytes(*w)).wrapping_mul(K);
        h ^= h >> 29;
    }
    let mut tail = [0u8; 8];
    tail[..rest.len()].copy_from_slice(rest);
    h = (h ^ u64::from_le_bytes(tail)).wrapping_mul(K);
    // Final avalanche (from MurmurHash3's fmix64).
    h ^= h >> 33;
    h = h.wrapping_mul(0xFF51_AFD7_ED55_8CCD);
    h ^= h >> 33;
    h = h.wrapping_mul(0xC4CE_B9FE_1A85_EC53);
    h ^ (h >> 33)
}

// ----- The marker inside the GIF -----

/// Application identifier (8 bytes) and authentication code (3 bytes).
const MARKER_ID: &[u8; 11] = b"GIFOPTIM1.0";
/// Extension introducer, label, identifier block, one data block holding
/// the mode, and the terminator.
pub const MARKER_LEN: usize = 3 + 11 + 2 + 1;

/// `gif` with a marker for `mode` inserted after its header and global
/// color table. `gif` must be a valid GIF.
pub fn add_marker(gif: &[u8], mode: Mode) -> Vec<u8> {
    let at = 13 + color_table_len(gif[10]);
    let mut out = Vec::with_capacity(gif.len() + MARKER_LEN);
    out.extend_from_slice(&gif[..at]);
    out.extend_from_slice(&[0x21, 0xFF, 11]);
    out.extend_from_slice(MARKER_ID);
    out.extend_from_slice(&[1, mode.code(), 0]);
    out.extend_from_slice(&gif[at..]);
    out
}

/// The mode in the marker among the blocks before the first frame, if
/// `prefix` (the start of a file) has one.
pub fn read_marker(prefix: &[u8]) -> Option<Mode> {
    if !prefix.starts_with(b"GIF") || prefix.len() < 13 {
        return None;
    }
    let mut p = 13 + color_table_len(prefix[10]);
    while *prefix.get(p)? == 0x21 {
        let label = *prefix.get(p + 1)?;
        if label == 0xFF
            && prefix.get(p + 2) == Some(&11)
            && prefix.get(p + 3..p + 14) == Some(&MARKER_ID[..])
        {
            return match prefix.get(p + 14..p + 16)? {
                [1, code] => Mode::from_code(*code),
                _ => None,
            };
        }
        p = skip_sub_blocks(prefix, p + 2)?;
    }
    None
}

/// The marker's mode in the file at `path`, reading only its start.
pub fn file_marker(path: &Path) -> Option<Mode> {
    let mut prefix = Vec::with_capacity(4096);
    File::open(path)
        .ok()?
        .take(4096)
        .read_to_end(&mut prefix)
        .ok()?;
    read_marker(&prefix)
}

// ----- The record file -----

/// A file that is already optimized.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub hash: u64,
    pub size: u64,
    pub mode: Mode,
    /// Modified time in nanoseconds since the Unix epoch, if known.
    pub modified: Option<u64>,
    pub path: PathBuf,
}

impl Entry {
    fn line(&self) -> String {
        format!(
            "{:016x}\t{}\t{}\t{}\t{}",
            self.hash,
            self.size,
            char::from(self.mode.code()),
            self.modified.map_or("-".to_string(), |m| m.to_string()),
            self.path.display()
        )
    }

    fn parse(line: &str) -> Option<Entry> {
        let mut parts = line.splitn(5, '\t');
        let hash = u64::from_str_radix(parts.next()?, 16).ok()?;
        let size = parts.next()?.parse().ok()?;
        let mode = Mode::from_code(*parts.next()?.as_bytes().first()?)?;
        let modified = match parts.next()? {
            "-" => None,
            m => Some(m.parse().ok()?),
        };
        let path = PathBuf::from(parts.next()?);
        Some(Entry {
            hash,
            size,
            mode,
            modified,
            path,
        })
    }
}

/// `path` made absolute, so the same file is always listed the same way.
fn absolute(path: &Path) -> PathBuf {
    std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf())
}

/// A file's modified time in nanoseconds since the Unix epoch.
pub fn modified_ns(meta: &Metadata) -> Option<u64> {
    let since = meta.modified().ok()?.duration_since(UNIX_EPOCH).ok()?;
    u64::try_from(since.as_nanos()).ok()
}

#[derive(Default)]
struct Known {
    by_path: HashMap<PathBuf, Entry>,
    /// The best mode each content was finished in.
    by_content: HashMap<(u64, u64), Mode>,
}

impl Known {
    fn insert(&mut self, e: Entry) {
        let best = self.by_content.entry((e.size, e.hash)).or_insert(e.mode);
        if e.mode.covers(*best) {
            *best = e.mode;
        }
        self.by_path.insert(e.path.clone(), e);
    }
}

pub struct Record {
    known: Mutex<Known>,
    /// Where new entries are appended as files finish, so they are kept
    /// even if a run is interrupted.
    file: Mutex<File>,
}

impl Record {
    /// Loads the record at `path` (creating it if needed), rewriting it
    /// with only the latest entry for each file.
    pub fn open(path: &Path) -> io::Result<Record> {
        let mut known = Known::default();
        match File::open(path) {
            Ok(f) => {
                for line in BufReader::new(f).lines() {
                    if let Some(e) = Entry::parse(&line?) {
                        known.insert(e);
                    }
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        let mut entries: Vec<&Entry> = known.by_path.values().collect();
        entries.sort_by(|a, b| a.path.cmp(&b.path));
        let mut text = format!("{HEADER}\n");
        for e in entries {
            text += &e.line();
            text.push('\n');
        }
        let mut temp = path.as_os_str().to_owned();
        temp.push(".tmp");
        fs::write(&temp, text)?;
        fs::rename(&temp, path)?;
        let file = OpenOptions::new().append(true).open(path)?;
        Ok(Record {
            known: Mutex::new(known),
            file: Mutex::new(file),
        })
    }

    fn known(&self) -> std::sync::MutexGuard<'_, Known> {
        self.known.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Whether the file at `path`, unchanged since it was recorded, needs
    /// no run in `wanted` mode. Reads nothing but `meta`.
    pub fn knows_file(&self, path: &Path, meta: &Metadata, wanted: Mode) -> bool {
        let modified = modified_ns(meta);
        self.known().by_path.get(&absolute(path)).is_some_and(|e| {
            e.size == meta.len()
                && modified.is_some()
                && e.modified == modified
                && e.mode.covers(wanted)
        })
    }

    /// Whether a file with this size and hash needs no run in `wanted` mode.
    pub fn knows_content(&self, size: u64, hash: u64, wanted: Mode) -> bool {
        self.known()
            .by_content
            .get(&(size, hash))
            .is_some_and(|m| m.covers(wanted))
    }

    pub fn add(&self, mut entry: Entry) -> io::Result<()> {
        entry.path = absolute(&entry.path);
        let line = entry.line();
        self.known().insert(entry);
        let mut file = self.file.lock().unwrap_or_else(|e| e.into_inner());
        writeln!(file, "{line}")
    }
}

/// The error for a file that the record recognizes by its content.
#[derive(Debug)]
pub struct AlreadyOptimized;

impl std::fmt::Display for AlreadyOptimized {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("already optimized (recognized by its content)")
    }
}

impl std::error::Error for AlreadyOptimized {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::composite::composite_frames;

    fn gif() -> Vec<u8> {
        let mut out = Vec::new();
        {
            let mut enc = gif::Encoder::new(&mut out, 8, 8, &[]).unwrap();
            enc.set_repeat(gif::Repeat::Infinite).unwrap();
            for i in 0..3u8 {
                let mut rgba: Vec<u8> = (0..64u8).flat_map(|p| [p * 4, i * 50, 7, 255]).collect();
                let mut f = gif::Frame::from_rgba_speed(8, 8, &mut rgba, 10);
                f.delay = 3;
                enc.write_frame(&f).unwrap();
            }
        }
        out
    }

    fn decode(data: &[u8]) -> (Vec<(Vec<u32>, u16)>, gif::Repeat) {
        let mut frames = Vec::new();
        let info = composite_frames(data, |c, d| {
            frames.push((c.to_vec(), d));
            Ok(())
        })
        .unwrap();
        (frames, info.repeat)
    }

    #[test]
    fn hash_is_stable_and_sensitive() {
        // Stored in record files, so it must never change.
        assert_eq!(hash(b""), 0x226e_0401_4447_0ef3, "{:x}", hash(b""));
        assert_eq!(
            hash(b"GIF89a"),
            0x89b3_1b77_807d_4f75,
            "{:x}",
            hash(b"GIF89a")
        );
        let data = gif();
        let mut other = data.clone();
        *other.last_mut().unwrap() ^= 1;
        assert_ne!(hash(&data), hash(&other));
        assert_ne!(hash(&data), hash(&data[..data.len() - 1]));
    }

    #[test]
    fn marker_is_invisible_and_found() {
        let data = gif();
        assert_eq!(read_marker(&data), None);
        for mode in [Mode::Lossless, Mode::Lossy] {
            let marked = add_marker(&data, mode);
            assert_eq!(marked.len(), data.len() + MARKER_LEN);
            assert_eq!(decode(&marked), decode(&data), "viewers see the same");
            assert_eq!(read_marker(&marked), Some(mode));
            let at = 13 + color_table_len(data[10]);
            assert_eq!(read_marker(&marked[..at + 15]), None, "prefix too short");
            assert_eq!(crate::repair::repair(&marked).1, None, "still well-formed");
        }
    }

    #[test]
    fn lossless_results_may_be_redone_lossy() {
        assert!(Mode::Lossy.covers(Mode::Lossy) && Mode::Lossy.covers(Mode::Lossless));
        assert!(Mode::Lossless.covers(Mode::Lossless));
        assert!(!Mode::Lossless.covers(Mode::Lossy));
    }

    #[test]
    fn record_survives_reopening() {
        let dir = std::env::temp_dir().join("gif_optimizer_test_record");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(DONE_FILE);
        let gif_path = dir.join("a b.gif");
        fs::write(&gif_path, gif()).unwrap();
        let meta = fs::metadata(&gif_path).unwrap();
        let entry = |mode| Entry {
            hash: 42,
            size: meta.len(),
            mode,
            modified: modified_ns(&meta),
            path: gif_path.clone(),
        };

        let record = Record::open(&path).unwrap();
        assert!(!record.knows_file(&gif_path, &meta, Mode::Lossless));
        record.add(entry(Mode::Lossless)).unwrap();
        record.add(entry(Mode::Lossy)).unwrap();
        assert!(record.knows_file(&gif_path, &meta, Mode::Lossy));
        drop(record);

        let record = Record::open(&path).unwrap();
        assert!(record.knows_file(&gif_path, &meta, Mode::Lossy));
        assert!(record.knows_content(meta.len(), 42, Mode::Lossy));
        assert!(!record.knows_content(meta.len(), 43, Mode::Lossless));
        let text = fs::read_to_string(&path).unwrap();
        assert_eq!(
            text.lines().count(),
            2,
            "compacted to the latest entry:\n{text}"
        );

        // A changed file is not recognized by its path any more.
        fs::write(&gif_path, b"GIF89a changed").unwrap();
        let changed = fs::metadata(&gif_path).unwrap();
        assert!(!record.knows_file(&gif_path, &changed, Mode::Lossless));
        fs::remove_dir_all(&dir).unwrap();
    }
}
