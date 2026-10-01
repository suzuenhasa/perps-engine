//! The journal's file operations, as a small trait, so that the writer and recovery can run
//! against real files, against nothing (discard mode), or against a simulated disk that
//! crashes (`docs/PIPELINE.md` 11.1, 11.6 and 18.2).
//!
//! **Contract.** [`JournalFiles`] is a directory of fixed-size segment files
//! `seg-000000.jnl`, `seg-000001.jnl`, ..., addressed by index:
//! - `create_segment` makes a new segment of `segment_bytes` zeros, durable, name and all;
//! - `write_at` and `read_at` are `pwrite` and `pread` at an offset;
//! - `sync_data` is `fdatasync` of one segment; `sync_dir` is `fsync` of the directory;
//! - `write_side_file` writes a new file next to the segments and syncs it (recovery's
//!   copy of a torn tail, 11.8); it never overwrites one that exists;
//! - `prepare` gets a segment ready ahead of time: [`StdFiles`] opens it, so that when the
//!   writer moves into a preallocated segment during a run, it builds no path and opens no
//!   file, and so allocates nothing on its thread (18.4).
//!
//! **Implementations.**
//! - [`StdFiles`]: `std::fs`. Segments are opened read-write, never with `O_APPEND`: on
//!   Linux, `pwrite` on an `O_APPEND` file ignores the offset and appends (11.1).
//! - [`DiscardFiles`]: does nothing, for the journal's discard mode (11.6).
//! - [`SimDisk`]: memory, for the crash tests (18.2). It keeps the durable bytes and the
//!   page cache apart, per 512-byte sector, so a test can lose power, crash a process
//!   mid-write, or fail a sync the way Linux does.
//!
//! **Creating a segment** (11.1): write zeros over all of it (8 MiB at a time), `sync_all`,
//! then fsync the directory, all before any record in it can be released. POSIX doesn't
//! promise that an fsync of a new file makes its *name* durable; the directory fsync does.
//! The same holds one level up for every directory above the segments that a run creates:
//! [`create_dir_all_durable`] fsyncs each new directory's parent, so the path to the
//! journal can't vanish at a power loss either (review finding F-DIR-FSYNC).
//! [`StdFiles`] also writes the zeros under a temporary name and renames the file only once
//! it is complete and synced, so a crash in the middle of `create_segment` can't leave a
//! short segment that later looks real: a segment either exists whole or not at all.
//!
//! **Complexity.** Every operation is one or two system calls on the bytes it names, except
//! `create_segment`, which writes the whole segment.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

/// The journal's directory of segment files. See the module docs.
pub trait JournalFiles {
    /// Bytes in every segment.
    fn segment_bytes(&self) -> u64;
    /// The indexes of the segments that exist, in increasing order.
    fn segments(&mut self) -> io::Result<Vec<u32>>;
    /// True if segment `segment` exists.
    fn exists(&mut self, segment: u32) -> io::Result<bool>;
    /// Reads `buf.len()` bytes at `offset` (`pread`). Reading past the segment's end is an
    /// error.
    fn read_at(&mut self, segment: u32, offset: u64, buf: &mut [u8]) -> io::Result<()>;
    /// Writes `bytes` at `offset` (`pwrite`), not yet durable.
    fn write_at(&mut self, segment: u32, offset: u64, bytes: &[u8]) -> io::Result<()>;
    /// Makes everything written to the segment durable (`fdatasync`).
    fn sync_data(&mut self, segment: u32) -> io::Result<()>;
    /// Creates segment `segment`: all zeros, durable, and its name durable. An error if it
    /// already exists.
    fn create_segment(&mut self, segment: u32) -> io::Result<()>;
    /// Makes the directory's entries durable (`fsync` of the directory).
    fn sync_dir(&mut self) -> io::Result<()>;
    /// Writes a new file `name` next to the segments and syncs it. Call `sync_dir` after,
    /// to make its name durable. An error of kind `AlreadyExists` if a file of that name
    /// exists: a side file is never overwritten.
    fn write_side_file(&mut self, name: &str, bytes: &[u8]) -> io::Result<()>;
    /// Gets existing segment `segment` ready for writing ahead of time, so that using it
    /// later allocates nothing (module docs). Does nothing by default.
    fn prepare(&mut self, segment: u32) -> io::Result<()> {
        let _ = segment;
        Ok(())
    }
}

/// The file name of segment `segment`: `seg-000000.jnl`, `seg-000001.jnl`, ...
pub fn segment_file_name(segment: u32) -> String {
    format!("seg-{segment:06}.jnl")
}

/// The segment index of a file name, if it is a segment's.
fn parse_segment_file_name(name: &str) -> Option<u32> {
    let digits = name.strip_prefix("seg-")?.strip_suffix(".jnl")?;
    if digits.len() != 6 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// How much `create_segment` writes per call (11.1).
const ZERO_CHUNK_BYTES: usize = 8 << 20;

/// Creates `dir` and every missing directory above it, like `fs::create_dir_all`, and makes
/// each new directory's name durable: once a directory is created, its parent is fsynced
/// (11.1's argument for segment names, applied to every level a run creates, such as
/// `<session>/<group>/<run>/journal`). Directories that already existed are left as they
/// are: whoever created them made their names durable. Does nothing if `dir` exists.
pub fn create_dir_all_durable(dir: &Path) -> io::Result<()> {
    // The missing directories, innermost first.
    let mut missing = Vec::new();
    let mut at = dir;
    while !at.exists() {
        missing.push(at);
        match at.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => at = parent,
            _ => break,
        }
    }
    for new in missing.into_iter().rev() {
        match fs::create_dir(new) {
            Ok(()) => {}
            // Another process made it meanwhile: its name is that process's to sync.
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists && new.is_dir() => continue,
            Err(e) => return Err(e),
        }
        let parent = new.parent().filter(|parent| !parent.as_os_str().is_empty()).unwrap_or(Path::new("."));
        File::open(parent)?.sync_all()?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------
// std::fs.

/// The journal on disk, through `std::fs` (module docs).
#[derive(Debug)]
pub struct StdFiles {
    dir: PathBuf,
    segment_bytes: u64,
    /// Segments opened so far, kept open.
    open: BTreeMap<u32, File>,
}

impl StdFiles {
    /// Opens the journal directory `dir`, creating it and any missing parents (and making
    /// their names durable, [`create_dir_all_durable`]) if it doesn't exist, for segments of
    /// `segment_bytes`. Checks nothing about existing segments: see
    /// [`StdFiles::open_existing`].
    pub fn open(dir: &Path, segment_bytes: u64) -> io::Result<StdFiles> {
        create_dir_all_durable(dir)?;
        Ok(StdFiles { dir: dir.to_path_buf(), segment_bytes, open: BTreeMap::new() })
    }

    /// Opens an existing journal directory. The header doesn't record the segment size, so
    /// it is the size of the segment files, which must all be the same; `default` if there
    /// are none. An error if `dir` doesn't exist.
    pub fn open_existing(dir: &Path, default: u64) -> io::Result<StdFiles> {
        if !dir.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("no journal directory {}", dir.display()),
            ));
        }
        let mut files = StdFiles { dir: dir.to_path_buf(), segment_bytes: default, open: BTreeMap::new() };
        let mut sizes = Vec::new();
        for segment in files.segments()? {
            sizes.push((segment, fs::metadata(files.path(segment))?.len()));
        }
        if let Some(&(first, size)) = sizes.first() {
            if let Some(&(other, other_size)) = sizes.iter().find(|(_, s)| *s != size) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("segment {first} is {size} bytes but segment {other} is {other_size}"),
                ));
            }
            files.segment_bytes = size;
        }
        Ok(files)
    }

    /// The directory holding the segments.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn path(&self, segment: u32) -> PathBuf {
        self.dir.join(segment_file_name(segment))
    }

    /// The open file of segment `segment`, opened read-write (never append) on first use.
    fn file(&mut self, segment: u32) -> io::Result<&File> {
        if !self.open.contains_key(&segment) {
            let file = OpenOptions::new().read(true).write(true).open(self.path(segment))?;
            self.open.insert(segment, file);
        }
        Ok(&self.open[&segment])
    }
}

impl JournalFiles for StdFiles {
    fn segment_bytes(&self) -> u64 {
        self.segment_bytes
    }

    fn segments(&mut self) -> io::Result<Vec<u32>> {
        let mut segments = Vec::new();
        for entry in fs::read_dir(&self.dir)? {
            let name = entry?.file_name();
            if let Some(segment) = name.to_str().and_then(parse_segment_file_name) {
                segments.push(segment);
            }
        }
        segments.sort_unstable();
        Ok(segments)
    }

    fn exists(&mut self, segment: u32) -> io::Result<bool> {
        // An open segment exists: no path to build (module docs, `prepare`).
        if self.open.contains_key(&segment) {
            return Ok(true);
        }
        self.path(segment).try_exists()
    }

    fn read_at(&mut self, segment: u32, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        self.file(segment)?.read_exact_at(buf, offset)
    }

    fn write_at(&mut self, segment: u32, offset: u64, bytes: &[u8]) -> io::Result<()> {
        self.file(segment)?.write_all_at(bytes, offset)
    }

    fn sync_data(&mut self, segment: u32) -> io::Result<()> {
        self.file(segment)?.sync_data()
    }

    fn create_segment(&mut self, segment: u32) -> io::Result<()> {
        let path = self.path(segment);
        if path.try_exists()? {
            return Err(io::Error::new(io::ErrorKind::AlreadyExists, format!("{} exists", path.display())));
        }
        // Zeros under a temporary name, then a rename once complete (module docs).
        let temporary = self.dir.join(format!("{}.tmp", segment_file_name(segment)));
        let file = OpenOptions::new().read(true).write(true).create(true).truncate(true).open(&temporary)?;
        let zeros = vec![0u8; ZERO_CHUNK_BYTES.min(self.segment_bytes as usize)];
        let mut offset = 0;
        while offset < self.segment_bytes {
            let n = zeros.len().min((self.segment_bytes - offset) as usize);
            file.write_all_at(&zeros[..n], offset)?;
            offset += n as u64;
        }
        file.sync_all()?;
        fs::rename(&temporary, &path)?;
        self.sync_dir()
    }

    fn sync_dir(&mut self) -> io::Result<()> {
        File::open(&self.dir)?.sync_all()
    }

    fn write_side_file(&mut self, name: &str, bytes: &[u8]) -> io::Result<()> {
        // `create_new`: an error if the file exists, so no earlier copy is ever overwritten.
        let file = OpenOptions::new().write(true).create_new(true).open(self.dir.join(name))?;
        file.write_all_at(bytes, 0)?;
        file.sync_all()
    }

    fn prepare(&mut self, segment: u32) -> io::Result<()> {
        self.file(segment).map(|_| ())
    }
}

// ---------------------------------------------------------------------------------------
// Discard mode.

/// The journal in discard mode (11.6): nothing is written and nothing is synced. Every
/// segment "exists" and reads as zeros, so the writer runs its whole logic (encoding, CRCs,
/// the flush rule, segment switches, the watermark) without touching a disk.
#[derive(Clone, Copy, Debug)]
pub struct DiscardFiles {
    segment_bytes: u64,
}

impl DiscardFiles {
    pub fn new(segment_bytes: u64) -> DiscardFiles {
        DiscardFiles { segment_bytes }
    }
}

impl JournalFiles for DiscardFiles {
    fn segment_bytes(&self) -> u64 {
        self.segment_bytes
    }

    fn segments(&mut self) -> io::Result<Vec<u32>> {
        Ok(Vec::new())
    }

    fn exists(&mut self, _segment: u32) -> io::Result<bool> {
        Ok(true)
    }

    fn read_at(&mut self, _segment: u32, _offset: u64, buf: &mut [u8]) -> io::Result<()> {
        buf.fill(0);
        Ok(())
    }

    fn write_at(&mut self, _segment: u32, _offset: u64, _bytes: &[u8]) -> io::Result<()> {
        Ok(())
    }

    fn sync_data(&mut self, _segment: u32) -> io::Result<()> {
        Ok(())
    }

    fn create_segment(&mut self, _segment: u32) -> io::Result<()> {
        Ok(())
    }

    fn sync_dir(&mut self) -> io::Result<()> {
        Ok(())
    }

    fn write_side_file(&mut self, _name: &str, _bytes: &[u8]) -> io::Result<()> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------------------
// The simulated disk (18.2).

/// A disk sector: written completely or not at all (the assumption of 11.8).
pub const SECTOR_BYTES: usize = 512;

/// A failure [`SimDisk`] injects into its next write or sync.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fault {
    /// The next sync fails the way Linux fails a writeback error (11.7): it returns an
    /// error and marks the segment's unsynced sectors clean *without* making them durable,
    /// while reads keep returning the new bytes until the power goes.
    FailSync,
    /// The process dies inside the next sync: it returns an error and nothing more
    /// reaches the disk.
    CrashInSync,
    /// The process dies inside the next write, after its first `n` bytes reached the page
    /// cache.
    CrashInWrite(usize),
}

/// One segment of a [`SimDisk`].
#[derive(Clone, Debug)]
struct SimSegment {
    /// What the disk holds: what survives a power loss.
    durable: Vec<u8>,
    /// What reads return: the page cache, which survives a process crash.
    cached: Vec<u8>,
    /// Sectors written since the last sync, which a sync makes durable. A sector whose
    /// cached bytes differ from its durable ones but is not in this set was marked clean by
    /// a failed sync: a sync will never write it, and a power loss loses it.
    unsynced: BTreeSet<usize>,
}

/// A disk in memory that keeps the durable bytes and the page cache apart, per 512-byte
/// sector, for the crash tests (module docs; PIPELINE.md 18.2).
#[derive(Clone, Debug)]
pub struct SimDisk {
    segment_bytes: u64,
    segments: BTreeMap<u32, SimSegment>,
    side_files: BTreeMap<String, Vec<u8>>,
    fault: Option<Fault>,
}

impl SimDisk {
    pub fn new(segment_bytes: u64) -> SimDisk {
        assert!(segment_bytes.is_multiple_of(SECTOR_BYTES as u64), "segments are whole sectors");
        SimDisk { segment_bytes, segments: BTreeMap::new(), side_files: BTreeMap::new(), fault: None }
    }

    /// Makes the next write or sync (whichever the fault is about) fail.
    pub fn inject(&mut self, fault: Fault) {
        self.fault = Some(fault);
    }

    /// The power goes: each unsynced sector reaches the disk if `keep` says so, and
    /// everything else not durable is lost, including sectors a failed sync marked clean.
    pub fn lose_power(&mut self, mut keep: impl FnMut() -> bool) {
        for segment in self.segments.values_mut() {
            for &sector in &segment.unsynced {
                if keep() {
                    let range = sector_range(sector);
                    segment.durable[range.clone()].copy_from_slice(&segment.cached[range]);
                }
            }
            segment.unsynced.clear();
            segment.cached.clone_from(&segment.durable);
        }
    }

    /// Bit rot: flips `mask` in the byte at `offset`, on the disk and in the cache alike.
    pub fn corrupt(&mut self, segment: u32, offset: u64, mask: u8) {
        let segment = self.segments.get_mut(&segment).expect("the segment exists");
        segment.durable[offset as usize] ^= mask;
        segment.cached[offset as usize] ^= mask;
    }

    /// What reads of segment `segment` return (the page cache), if it exists.
    pub fn cached(&self, segment: u32) -> Option<&[u8]> {
        self.segments.get(&segment).map(|s| &s.cached[..])
    }

    /// What segment `segment` holds on the disk, if it exists.
    pub fn durable(&self, segment: u32) -> Option<&[u8]> {
        self.segments.get(&segment).map(|s| &s.durable[..])
    }

    /// A file `write_side_file` wrote.
    pub fn side_file(&self, name: &str) -> Option<&[u8]> {
        self.side_files.get(name).map(Vec::as_slice)
    }

    /// The names of the files `write_side_file` wrote.
    pub fn side_file_names(&self) -> Vec<String> {
        self.side_files.keys().cloned().collect()
    }

    fn segment(&mut self, segment: u32) -> io::Result<&mut SimSegment> {
        self.segments
            .get_mut(&segment)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("no segment {segment}")))
    }

    fn check_range(&self, offset: u64, len: usize) -> io::Result<()> {
        if offset + len as u64 > self.segment_bytes {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "past the end of the segment"));
        }
        Ok(())
    }
}

/// The byte range of sector `sector`.
fn sector_range(sector: usize) -> std::ops::Range<usize> {
    sector * SECTOR_BYTES..(sector + 1) * SECTOR_BYTES
}

/// The error a simulated crash or failure returns.
fn simulated(what: &str) -> io::Error {
    io::Error::other(format!("simulated {what}"))
}

impl JournalFiles for SimDisk {
    fn segment_bytes(&self) -> u64 {
        self.segment_bytes
    }

    fn segments(&mut self) -> io::Result<Vec<u32>> {
        Ok(self.segments.keys().copied().collect())
    }

    fn exists(&mut self, segment: u32) -> io::Result<bool> {
        Ok(self.segments.contains_key(&segment))
    }

    fn read_at(&mut self, segment: u32, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        self.check_range(offset, buf.len())?;
        let segment = self.segment(segment)?;
        buf.copy_from_slice(&segment.cached[offset as usize..offset as usize + buf.len()]);
        Ok(())
    }

    fn write_at(&mut self, segment: u32, offset: u64, bytes: &[u8]) -> io::Result<()> {
        self.check_range(offset, bytes.len())?;
        let (bytes, result) = match self.fault {
            Some(Fault::CrashInWrite(n)) => {
                self.fault = None;
                (&bytes[..n.min(bytes.len())], Err(simulated("crash in a write")))
            }
            _ => (bytes, Ok(())),
        };
        let segment = self.segment(segment)?;
        let start = offset as usize;
        segment.cached[start..start + bytes.len()].copy_from_slice(bytes);
        if !bytes.is_empty() {
            segment.unsynced.extend(start / SECTOR_BYTES..=(start + bytes.len() - 1) / SECTOR_BYTES);
        }
        result
    }

    fn sync_data(&mut self, segment: u32) -> io::Result<()> {
        let fault = self.fault.take_if(|fault| matches!(fault, Fault::FailSync | Fault::CrashInSync));
        let segment = self.segment(segment)?;
        match fault {
            Some(Fault::CrashInSync) => Err(simulated("crash in a sync")),
            Some(_) => {
                // Linux: the pages are marked clean, the error is reported once (11.7).
                segment.unsynced.clear();
                Err(simulated("failed sync"))
            }
            None => {
                for &sector in &segment.unsynced {
                    let range = sector_range(sector);
                    segment.durable[range.clone()].copy_from_slice(&segment.cached[range]);
                }
                segment.unsynced.clear();
                Ok(())
            }
        }
    }

    fn create_segment(&mut self, segment: u32) -> io::Result<()> {
        if self.segments.contains_key(&segment) {
            return Err(io::Error::new(io::ErrorKind::AlreadyExists, format!("segment {segment} exists")));
        }
        let zeros = vec![0; self.segment_bytes as usize];
        let created = SimSegment { durable: zeros.clone(), cached: zeros, unsynced: BTreeSet::new() };
        self.segments.insert(segment, created);
        Ok(())
    }

    fn sync_dir(&mut self) -> io::Result<()> {
        Ok(())
    }

    fn write_side_file(&mut self, name: &str, bytes: &[u8]) -> io::Result<()> {
        if self.side_files.contains_key(name) {
            return Err(io::Error::new(io::ErrorKind::AlreadyExists, format!("{name} exists")));
        }
        self.side_files.insert(name.to_string(), bytes.to_vec());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh directory under the system's temporary directory.
    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pipeline-files-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn missing_directories_are_created_level_by_level_and_existing_ones_left_alone() {
        let dir = temp_dir("dirs");
        let deep = dir.join("session/group/run/journal");
        create_dir_all_durable(&deep).expect("created, with every parent");
        assert!(deep.is_dir());
        create_dir_all_durable(&deep).expect("an existing directory is fine");
        fs::write(dir.join("session/file"), b"x").expect("written");
        assert!(create_dir_all_durable(&dir.join("session/file/below")).is_err(), "a file is in the way");
        fs::remove_dir_all(&dir).expect("cleaned up");
    }

    #[test]
    fn a_side_file_is_never_overwritten() {
        let dir = temp_dir("side");
        let mut files = StdFiles::open(&dir, 4_096).expect("opened");
        files.write_side_file("torn-000001-0.bin", b"first").expect("written");
        let error = files.write_side_file("torn-000001-0.bin", b"second").expect_err("exists");
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(dir.join("torn-000001-0.bin")).expect("read"), b"first");
        let mut disk = SimDisk::new(4_096);
        disk.write_side_file("a.bin", b"first").expect("written");
        assert_eq!(
            disk.write_side_file("a.bin", b"second").expect_err("exists").kind(),
            io::ErrorKind::AlreadyExists
        );
        assert_eq!(disk.side_file("a.bin"), Some(&b"first"[..]));
        fs::remove_dir_all(&dir).expect("cleaned up");
    }

    #[test]
    fn segment_file_names_have_six_digits() {
        assert_eq!(segment_file_name(0), "seg-000000.jnl");
        assert_eq!(segment_file_name(123), "seg-000123.jnl");
        assert_eq!(parse_segment_file_name("seg-000123.jnl"), Some(123));
        for other in ["seg-000123.jnl.tmp", "seg-123.jnl", "seg-00012a.jnl", "torn-000001-128.bin"] {
            assert_eq!(parse_segment_file_name(other), None, "{other}");
        }
    }

    #[test]
    fn std_files_create_zeroed_segments_and_read_back_what_was_written() {
        let dir = temp_dir("std");
        let mut files = StdFiles::open(&dir.join("journal"), 4_096).expect("the directory is created");
        assert_eq!(files.segments().expect("listed"), Vec::<u32>::new());
        files.create_segment(1).expect("created");
        files.create_segment(0).expect("created");
        assert!(files.create_segment(1).is_err(), "never over an existing segment");
        assert_eq!(files.segments().expect("listed"), [0, 1]);
        assert!(files.exists(1).expect("checked") && !files.exists(2).expect("checked"));
        let mut all = vec![0xFF; 4_096];
        files.read_at(1, 0, &mut all).expect("read");
        assert!(all.iter().all(|&b| b == 0), "a new segment is all zeros");

        files.write_at(1, 100, b"hello").expect("written");
        files.write_at(1, 4_091, b"end!!").expect("written at the very end");
        files.sync_data(1).expect("synced");
        let mut back = [0; 5];
        files.read_at(1, 100, &mut back).expect("read");
        assert_eq!(&back, b"hello");
        assert!(files.read_at(1, 4_094, &mut back).is_err(), "past the end");
        assert_eq!(fs::metadata(dir.join("journal/seg-000001.jnl")).expect("exists").len(), 4_096);

        // A leftover temporary file is not a segment.
        fs::write(dir.join("journal/seg-000002.jnl.tmp"), b"half").expect("written");
        assert_eq!(files.segments().expect("listed"), [0, 1]);
        files.write_side_file("torn-000001-128.bin", b"tail").expect("written");
        files.sync_dir().expect("synced");
        assert_eq!(fs::read(dir.join("journal/torn-000001-128.bin")).expect("read"), b"tail");

        let reopened = StdFiles::open_existing(&dir.join("journal"), 1 << 30).expect("opened");
        assert_eq!(reopened.segment_bytes(), 4_096, "the size of the segment files");
        assert!(StdFiles::open_existing(&dir.join("nowhere"), 4_096).is_err());
        fs::remove_dir_all(&dir).expect("cleaned up");
    }

    #[test]
    fn a_prepared_segment_is_open_and_known_to_exist_without_a_look_at_the_disk() {
        let dir = temp_dir("prepare");
        let mut files = StdFiles::open(&dir, 4_096).expect("opened");
        files.create_segment(3).expect("created");
        files.prepare(3).expect("prepared");
        assert!(files.open.contains_key(&3), "opened ahead of use");
        assert!(files.prepare(4).is_err(), "only an existing segment can be prepared");
        // The map answers now: even with the file gone, the open segment still exists.
        fs::remove_file(dir.join(segment_file_name(3))).expect("removed");
        assert!(files.exists(3).expect("checked"));
        fs::remove_dir_all(&dir).expect("cleaned up");
    }

    #[test]
    fn segments_of_different_sizes_are_refused() {
        let dir = temp_dir("sizes");
        let mut files = StdFiles::open(&dir, 4_096).expect("opened");
        files.create_segment(0).expect("created");
        let mut larger = StdFiles::open(&dir, 8_192).expect("opened");
        larger.create_segment(1).expect("created");
        let error = StdFiles::open_existing(&dir, 4_096).expect_err("mixed sizes");
        assert!(error.to_string().contains("segment 0 is 4096 bytes but segment 1 is 8192"), "{error}");
        fs::remove_dir_all(&dir).expect("cleaned up");
    }

    #[test]
    fn discard_files_do_nothing() {
        let mut files = DiscardFiles::new(4_096);
        assert!(files.exists(7).expect("always"));
        files.write_at(7, 0, &[1; 10]).expect("ignored");
        let mut back = [9; 4];
        files.read_at(7, 0, &mut back).expect("zeros");
        assert_eq!(back, [0; 4]);
        assert!(files.segments().expect("none").is_empty());
    }

    #[test]
    fn a_sim_disk_keeps_unsynced_sectors_out_of_the_durable_bytes() {
        let mut disk = SimDisk::new(4 * SECTOR_BYTES as u64);
        disk.create_segment(0).expect("created");
        disk.write_at(0, 500, &[7; 30]).expect("written"); // sectors 0 and 1
        assert_eq!(disk.cached(0).expect("exists")[500..530], [7; 30]);
        assert_eq!(disk.durable(0).expect("exists")[500..530], [0; 30], "not synced yet");
        disk.sync_data(0).expect("synced");
        assert_eq!(disk.durable(0).expect("exists")[500..530], [7; 30]);

        // Unsynced sectors 2 and 3; the power fails and only the first one is kept.
        disk.write_at(0, 1_100, &[8; 600]).expect("written");
        let mut decisions = [true, false].into_iter();
        disk.lose_power(|| decisions.next().expect("two unsynced sectors"));
        let after = disk.cached(0).expect("exists");
        assert_eq!(after[1_100..1_536], [8; 436], "sector 2 reached the disk");
        assert_eq!(after[1_536..1_700], [0; 164], "sector 3 was lost");
        assert_eq!(after[500..530], [7; 30], "synced bytes survive");
    }

    #[test]
    fn a_failed_sync_marks_the_sectors_clean_but_reads_still_see_them() {
        let mut disk = SimDisk::new(2 * SECTOR_BYTES as u64);
        disk.create_segment(0).expect("created");
        disk.write_at(0, 0, &[5; 10]).expect("written");
        disk.inject(Fault::FailSync);
        assert!(disk.sync_data(0).is_err());
        assert_eq!(disk.cached(0).expect("exists")[..10], [5; 10], "the page cache still has them");
        disk.sync_data(0).expect("a retried sync 'succeeds'");
        assert_eq!(disk.durable(0).expect("exists")[..10], [0; 10], "without writing anything");
        disk.lose_power(|| true);
        assert_eq!(disk.cached(0).expect("exists")[..10], [0; 10], "lost at the power loss");
    }

    #[test]
    fn a_crash_in_a_write_leaves_a_prefix_and_a_crash_in_a_sync_changes_nothing() {
        let mut disk = SimDisk::new(2 * SECTOR_BYTES as u64);
        disk.create_segment(0).expect("created");
        disk.inject(Fault::CrashInWrite(3));
        assert!(disk.write_at(0, 10, &[1; 8]).is_err());
        assert_eq!(disk.cached(0).expect("exists")[10..18], [1, 1, 1, 0, 0, 0, 0, 0]);
        disk.inject(Fault::CrashInSync);
        assert!(disk.sync_data(0).is_err());
        assert_eq!(disk.durable(0).expect("exists")[10..13], [0; 3]);
        disk.sync_data(0).expect("the next sync works");
        assert_eq!(disk.durable(0).expect("exists")[10..13], [1; 3]);
        assert!(disk.read_at(1, 0, &mut [0]).is_err(), "no such segment");
        assert!(disk.write_at(0, 1_020, &[0; 8]).is_err(), "past the end");
    }
}
