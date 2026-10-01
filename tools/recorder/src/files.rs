//! Hourly output files: `<dir>/YYYY-MM-DD/HH.jsonl` (UTC), gzipped once the hour is over.
//!
//! Lines are routed by the *current* time, not by the time a frame was received, so a
//! frame released late by the sampler (up to about a second) near the top of the hour
//! lands in the new hour's file rather than reopening one that is being compressed.
//! Consumers should order by the frame's own `ts`.
//!
//! Hours only move forward. If the clock steps back (WSL2 does this after sleep), lines
//! keep going to the current file. An hour that is already compressed is never reopened:
//! a restart that lands in such an hour writes `HH.1.jsonl` (then `HH.2.jsonl`, ...)
//! next to it, because compressing a reopened `HH.jsonl` would replace the full
//! `HH.jsonl.gz` with the few new lines.
//!
//! Every file starts with a `file_opened` marker carrying the recorder's build and
//! sampling policy, so each file says on its own how it was produced.

use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::frames::json_escape;

/// Buffered lines reach the disk at least this often.
const FLUSH_EVERY: Duration = Duration::from_secs(1);

pub struct HourlyFiles {
    dir: PathBuf,
    /// Written as the `detail` of each file's `file_opened` marker.
    header: String,
    current: Option<OpenFile>,
    last_flush: Instant,
}

struct OpenFile {
    /// Hours since the Unix epoch.
    hour: u64,
    path: PathBuf,
    writer: BufWriter<File>,
}

impl HourlyFiles {
    pub fn new(dir: PathBuf, header: String) -> Self {
        HourlyFiles { dir, header, current: None, last_flush: Instant::now() }
    }

    /// Appends one line (without its newline) to the current hour's file.
    pub fn write_line(&mut self, line: &str) {
        self.write_line_at(unix_ms() / 3_600_000, line);
    }

    /// `write_line` with the current hour passed in, so tests can move the clock.
    fn write_line_at(&mut self, now: u64, line: &str) {
        if self.current.as_ref().is_none_or(|open| now > open.hour) {
            self.rotate(now);
        }
        if let Some(open) = &mut self.current {
            open.append(line);
        }
    }

    pub fn flush(&mut self) {
        if let Some(open) = &mut self.current
            && let Err(e) = open.writer.flush()
        {
            eprintln!("recorder: flush of {} failed: {e}", open.path.display());
        }
        self.last_flush = Instant::now();
    }

    pub fn flush_if_due(&mut self) {
        if self.last_flush.elapsed() >= FLUSH_EVERY {
            self.flush();
        }
    }

    /// Compresses `.jsonl` files left uncompressed by an earlier run (e.g. after a crash).
    /// The current hour's files are left alone: this run may append to them.
    pub fn compress_finished_hours(&self) {
        let hour = unix_ms() / 3_600_000;
        let current_day = self.dir.join(day_dir_name(hour));
        let current_prefix = format!("{:02}.", hour % 24);
        let Ok(days) = fs::read_dir(&self.dir) else { return };
        for day in days.flatten() {
            let Ok(files) = fs::read_dir(day.path()) else { continue };
            for file in files.flatten() {
                let path = file.path();
                let current = day.path() == current_day
                    && file.file_name().to_string_lossy().starts_with(&current_prefix);
                if path.extension().is_some_and(|e| e == "jsonl") && !current {
                    compress_in_background(path);
                }
            }
        }
    }

    /// Closes the current file (compressing it in the background) and opens `hour`'s file.
    fn rotate(&mut self, hour: u64) {
        if let Some(mut open) = self.current.take() {
            let _ = open.writer.flush();
            drop(open.writer);
            compress_in_background(open.path);
        }
        let path = self.dir.join(uncompressed_hour_file(&self.dir, hour));
        match open_for_append(&path) {
            Ok(file) => {
                let mut open = OpenFile { hour, path, writer: BufWriter::new(file) };
                open.append(&format!(
                    r#"{{"recv_ms":{},"marker":"file_opened","link":"file","detail":"{}"}}"#,
                    unix_ms(),
                    json_escape(&self.header)
                ));
                self.current = Some(open);
            }
            Err(e) => eprintln!("recorder: cannot open {}: {e}", path.display()),
        }
    }
}

impl OpenFile {
    fn append(&mut self, line: &str) {
        // One write per line, newline included, so a line is never split across flushes.
        let mut bytes = Vec::with_capacity(line.len() + 1);
        bytes.extend_from_slice(line.as_bytes());
        bytes.push(b'\n');
        if let Err(e) = self.writer.write_all(&bytes) {
            eprintln!("recorder: write to {} failed: {e}", self.path.display());
        }
    }
}

/// Opens a file for appending. If an earlier run died mid-line, starts on a fresh line so
/// the half line can't swallow the next one.
fn open_for_append(path: &Path) -> std::io::Result<File> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut file = OpenOptions::new().create(true).read(true).append(true).open(path)?;
    if file.metadata()?.len() > 0 {
        let mut last = [0u8; 1];
        file.seek(SeekFrom::End(-1))?;
        file.read_exact(&mut last)?;
        if last[0] != b'\n' {
            file.write_all(b"\n")?;
        }
    }
    Ok(file)
}

/// Gzips a finished hour file. `-f` because the `.jsonl` is the source of truth: a partial
/// `.gz` left by an interrupted run is replaced rather than blocking compression forever.
fn compress_in_background(path: PathBuf) {
    std::thread::spawn(move || match Command::new("gzip").arg("-f").arg(&path).status() {
        Ok(status) if status.success() => {}
        other => eprintln!("recorder: gzip {} failed: {other:?}", path.display()),
    });
}

/// `YYYY-MM-DD/HH.jsonl` for an hour counted from the Unix epoch, in UTC, or
/// `HH.N.jsonl` with the smallest N whose file hasn't been compressed yet.
fn uncompressed_hour_file(dir: &Path, hour: u64) -> PathBuf {
    let day = Path::new(&day_dir_name(hour)).to_path_buf();
    (0..)
        .map(|n| match n {
            0 => day.join(format!("{:02}.jsonl", hour % 24)),
            n => day.join(format!("{:02}.{n}.jsonl", hour % 24)),
        })
        .find(|name| !dir.join(name).with_added_extension("gz").exists())
        .expect("some suffix is free")
}

/// `YYYY-MM-DD` for an hour counted from the Unix epoch, in UTC.
fn day_dir_name(hour: u64) -> String {
    let (year, month, day) = civil_from_days((hour / 24) as i64);
    format!("{year:04}-{month:02}-{day:02}")
}

/// Converts days since 1970-01-01 to a (year, month, day) date.
///
/// Howard Hinnant's `civil_from_days` algorithm
/// (<https://howardhinnant.github.io/date_algorithms.html>): exact for the proleptic
/// Gregorian calendar, and saves a date/time dependency for one file name.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era = (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153; // March = 0
    let day = (day_of_year - (153 * shifted_month + 2) / 5 + 1) as u32;
    let month = (if shifted_month < 10 { shifted_month + 3 } else { shifted_month - 9 }) as u32;
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

pub fn unix_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_from_days_matches_known_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
        assert_eq!(civil_from_days(19_723), (2024, 1, 1));
        assert_eq!(civil_from_days(20_725), (2026, 9, 29));
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("recorder-test-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn hour_files_are_named_by_utc_day_and_hour_and_never_reopened_once_compressed() {
        let dir = temp_dir("names");
        let hour = 20_725 * 24 + 14;
        assert_eq!(uncompressed_hour_file(&dir, hour), PathBuf::from("2026-09-29/14.jsonl"));
        fs::create_dir_all(dir.join("2026-09-29")).unwrap();
        fs::write(dir.join("2026-09-29/14.jsonl.gz"), "").unwrap();
        assert_eq!(uncompressed_hour_file(&dir, hour), PathBuf::from("2026-09-29/14.1.jsonl"));
        fs::write(dir.join("2026-09-29/14.1.jsonl.gz"), "").unwrap();
        assert_eq!(uncompressed_hour_file(&dir, hour), PathBuf::from("2026-09-29/14.2.jsonl"));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_clock_step_back_keeps_writing_to_the_current_file() {
        let dir = temp_dir("step");
        let hour = 20_725 * 24 + 14;
        let mut files = HourlyFiles::new(dir.clone(), "test".to_string());
        files.write_line_at(hour, "{\"a\":1}");
        files.write_line_at(hour - 1, "{\"b\":2}");
        files.flush();
        let lines: Vec<String> =
            fs::read_to_string(dir.join("2026-09-29/14.jsonl")).unwrap().lines().map(String::from).collect();
        assert!(lines[0].contains(r#""marker":"file_opened","link":"file""#));
        assert_eq!(&lines[1..], ["{\"a\":1}", "{\"b\":2}"]);
        assert!(!dir.join("2026-09-29/13.jsonl").exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn reopening_a_file_that_ends_mid_line_starts_a_new_line() {
        let dir = temp_dir("reopen");
        let path = dir.join("x.jsonl");
        fs::write(&path, "{\"complete\":1}\n{\"half").unwrap();
        let mut file = open_for_append(&path).unwrap();
        file.write_all(b"{\"next\":2}\n").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "{\"complete\":1}\n{\"half\n{\"next\":2}\n");
        fs::remove_dir_all(&dir).unwrap();
    }
}
