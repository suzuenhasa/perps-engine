//! The journal: every sequenced command, on disk, before any of its results is released
//! (`docs/PIPELINE.md` section 11; `docs/DECISIONS.md` D-025 and D-030).
//!
//! **Contract.** The journal writer appends each command the sequencer gives it as one
//! record with its own CRC32C, in seq order, to fixed-size segment files filled with zeros
//! in advance, and makes every batch durable with one `pwrite` and one `fdatasync` before it
//! publishes the durable watermark. Recovery, at the next start, finds the longest valid
//! prefix, checks that whatever follows can only be one unfinished flush, zeroes that, and
//! makes the prefix durable, all before anything is derived from the journal. Replay
//! (`replay.rs`) then rebuilds the engine and the nonce table from the recovered records.
//!
//! **Files** (PIPELINE.md 19.1):
//! - [`mod@format`]: the segment header and the journal's identity, `ENGINE_SEMANTICS`, and the
//!   record encoding and its rules (11.2, 11.3);
//! - [`files`]: the file operations as a small trait, with the `std::fs` implementation,
//!   a discarding one, and the simulated disk the crash tests use (11.1, 18.2);
//! - [`writer`]: group commit, preallocation and discard mode (11.5 to 11.7);
//! - [`recovery`]: finding the end, the tail check, zeroing and re-writing (11.8).
//!
//! This file holds what they share: where the journal ends ([`JournalPosition`]) and what
//! can go wrong ([`JournalError`]).

pub mod files;
pub mod format;
pub mod recovery;
pub mod writer;

use std::fmt;
use std::io;

use format::JournalIdentity;

/// A place in the journal: a segment and a byte offset in it. Recovery's `end` is the
/// position just after the last valid record (11.8).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct JournalPosition {
    pub segment: u32,
    pub offset: u64,
}

impl JournalPosition {
    /// The segment a new life starts in when the journal ends here (11.8, step 4): the one
    /// after the end's segment. If the journal is empty (its end is segment 0, offset 0: no
    /// valid header at all), segment 0 itself, so that the journal still starts with
    /// segment 0.
    pub fn next_life_segment(self) -> u32 {
        if self.offset == 0 { self.segment } else { self.segment + 1 }
    }
}

impl fmt::Display for JournalPosition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "segment {}, offset {}", self.segment, self.offset)
    }
}

/// Why the journal can't be recovered, replayed or started. Every variant that is about the
/// journal's contents names the place, and recovery changes nothing before it returns one
/// of those (11.8).
#[derive(Debug)]
pub enum JournalError {
    /// A file operation failed.
    Io { what: String, error: io::Error },
    /// The journal holds something that can't be there: a record with a valid CRC that
    /// doesn't fit, a header that disagrees with segment 0's, or nonzero bytes after the
    /// end outside the tail region (11.8). A person must look.
    Corrupt { at: JournalPosition, what: String },
    /// The journal's identity (segment 0's header) differs from this process's
    /// configuration (11.2). Lists every field that differs, with both values (the hash
    /// seed's values are left out: it is a secret in production).
    IdentityMismatch { differences: Vec<String> },
    /// A request that can't be carried out as asked: a fresh start on a journal that
    /// already has records, a segment size that differs from the journal's, a clock
    /// anchored below the journal's last timestamp, a batch limit above 4,096.
    Refused(String),
}

impl JournalError {
    /// An I/O error, with what was being done.
    pub fn io(what: impl Into<String>, error: io::Error) -> JournalError {
        JournalError::Io { what: what.into(), error }
    }

    pub fn corrupt(segment: u32, offset: u64, what: impl Into<String>) -> JournalError {
        JournalError::Corrupt { at: JournalPosition { segment, offset }, what: what.into() }
    }
}

impl fmt::Display for JournalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            JournalError::Io { what, error } => write!(f, "journal I/O error while {what}: {error}"),
            JournalError::Corrupt { at, what } => {
                write!(f, "the journal can't be recovered as is, at {at}: {what}; nothing was changed")
            }
            JournalError::IdentityMismatch { differences } => {
                write!(
                    f,
                    "the journal's identity differs from this configuration: {}",
                    differences.join("; ")
                )
            }
            JournalError::Refused(why) => f.write_str(why),
        }
    }
}

impl std::error::Error for JournalError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            JournalError::Io { error, .. } => Some(error),
            _ => None,
        }
    }
}

/// Compares the journal's identity with the configured one (11.2): every field but the
/// hash seed is printed with both values when it differs. `engine_semantics` is skipped when
/// `allow_engine_change` is set (`--allow-engine-change`).
pub fn check_identity(
    journal: &JournalIdentity,
    config: &JournalIdentity,
    allow_engine_change: bool,
) -> Result<(), JournalError> {
    let differences = journal.differences(config, allow_engine_change, ["journal", "configuration"]);
    if differences.is_empty() { Ok(()) } else { Err(JournalError::IdentityMismatch { differences }) }
}
