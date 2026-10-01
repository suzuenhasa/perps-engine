//! `summary.txt`: everything one run measured, as plain `key = value` lines
//! (`docs/PIPELINE.md` 15.5 and 15.11).
//!
//! **Why a text file per run.** A session on a rented box can die (the box is reclaimed, a
//! run aborts the process by design, 2.8). Every run writes its summary when it is done,
//! and every command skips the runs whose summary exists, so a session continues where it
//! stopped (15.5, "Resumable"). The reports are built only from summaries, never from
//! memory, so a resumed session renders exactly what an uninterrupted one would.
//!
//! **Format.** One `key = value` per line, in the order written; keys are dotted names
//! (`stage.client.core_path.p99`), values are integers or short text without newlines.
//! Latencies are nanoseconds, with `none` for a histogram that recorded nothing and `inf`
//! for "never served" (15.3, 15.4). A line starting with `#` is a comment.
//!
//! **Invariant.** A summary file is complete or absent: it is written under a temporary
//! name and renamed into place, so a crash while writing leaves no half file that a resumed
//! session would take for a finished run.
//!
//! **Complexity.** Lookups are linear in the number of keys (a few hundred): summaries are
//! read once per report.

use std::fmt;
use std::fs;
use std::io;
use std::path::Path;

use pipeline::histogram::LatencyHistogram;

/// One run's results (module docs).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Summary {
    entries: Vec<(String, String)>,
}

impl Summary {
    pub fn new() -> Summary {
        Summary::default()
    }

    /// Sets `key` to `value`, replacing an earlier value. Panics on a key or value that
    /// would break the format (a newline, or `=` in a key): a bug in the caller.
    pub fn put(&mut self, key: impl Into<String>, value: impl fmt::Display) {
        let (key, value) = (key.into(), value.to_string());
        assert!(
            !key.contains(['=', '\n']) && !key.trim().is_empty() && !value.contains('\n'),
            "summary key {key:?} or value {value:?} would break the format"
        );
        match self.entries.iter_mut().find(|(k, _)| *k == key) {
            Some(entry) => entry.1 = value,
            None => self.entries.push((key, value)),
        }
    }

    /// A latency or percentile: `none`, `inf`, or nanoseconds.
    pub fn put_ns(&mut self, key: impl Into<String>, ns: Option<u64>) {
        let value = match ns {
            None => "none".to_string(),
            Some(u64::MAX) => "inf".to_string(),
            Some(ns) => ns.to_string(),
        };
        self.put(key, value);
    }

    /// `<prefix>.count`, `.p50`, `.p99`, `.p999` and `.max` of a histogram (15.4: every
    /// histogram is reported as count, p50, p99, p99.9 and max).
    pub fn put_histogram(&mut self, prefix: &str, histogram: &LatencyHistogram) {
        self.put(format!("{prefix}.count"), histogram.count());
        self.put_ns(format!("{prefix}.p50"), histogram.percentile(50, 100));
        self.put_ns(format!("{prefix}.p99"), histogram.percentile(99, 100));
        self.put_ns(format!("{prefix}.p999"), histogram.percentile(999, 1_000));
        self.put_ns(format!("{prefix}.max"), histogram.max());
    }

    /// Adds every entry of `other` under `prefix.`.
    pub fn put_all(&mut self, prefix: &str, other: &Summary) {
        for (key, value) in &other.entries {
            self.put(format!("{prefix}.{key}"), value);
        }
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.entries.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }

    /// An integer value; `None` if absent or not an integer.
    pub fn u64(&self, key: &str) -> Option<u64> {
        self.get(key)?.parse().ok()
    }

    pub fn i128(&self, key: &str) -> Option<i128> {
        self.get(key)?.parse().ok()
    }

    /// A latency written by [`Summary::put_ns`]: `Some(None)` for `none`, `Some(Some(MAX))`
    /// for `inf`; `None` if the key is absent.
    pub fn ns(&self, key: &str) -> Option<Option<u64>> {
        match self.get(key)? {
            "none" => Some(None),
            "inf" => Some(Some(u64::MAX)),
            value => value.parse().ok().map(Some),
        }
    }

    /// `true` or `false`.
    pub fn flag(&self, key: &str) -> Option<bool> {
        match self.get(key)? {
            "true" => Some(true),
            "false" => Some(false),
            _ => None,
        }
    }

    /// Every key starting with `prefix`, with its value, in the order written.
    pub fn with_prefix<'a>(&'a self, prefix: &'a str) -> impl Iterator<Item = (&'a str, &'a str)> + 'a {
        self.entries
            .iter()
            .filter(move |(key, _)| key.starts_with(prefix))
            .map(|(key, value)| (key.as_str(), value.as_str()))
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The file's text.
    pub fn text(&self) -> String {
        let mut text = String::new();
        for (key, value) in &self.entries {
            text.push_str(key);
            text.push_str(" = ");
            text.push_str(value);
            text.push('\n');
        }
        text
    }

    /// Reads the text [`Summary::text`] writes; blank lines and `#` comments are skipped.
    pub fn parse(text: &str) -> Result<Summary, String> {
        let mut summary = Summary::new();
        for (n, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            // Keys never hold `=`, so the first one separates the key from the value.
            let (key, value) =
                line.split_once('=').ok_or_else(|| format!("line {}: not `key = value`: {line:?}", n + 1))?;
            summary.put(key.trim(), value.trim());
        }
        Ok(summary)
    }

    /// Writes the file atomically (module docs, "Invariant").
    pub fn write(&self, path: &Path) -> io::Result<()> {
        let temporary = path.with_extension("txt.partial");
        fs::write(&temporary, self.text())?;
        fs::rename(&temporary, path)
    }

    pub fn read(path: &Path) -> io::Result<Summary> {
        let text = fs::read_to_string(path)?;
        Summary::parse(&text)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("{}: {e}", path.display())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_summary_round_trips_through_its_text_and_its_file() {
        let mut summary = Summary::new();
        summary.put("run.mode", "signed");
        summary.put("run.rate", 100_000);
        summary.put_ns("stage.client.core_path.p99", Some(812));
        summary.put_ns("stage.client.to_durable_ack.p99", Some(u64::MAX));
        summary.put_ns("stage.client.ingress_wait.p99", None);
        summary.put("run.rate", 20_000); // replaces
        summary.put("machine.layout", "core=2, sequencer=3");
        summary.put("machine.commit", "");
        let parsed = Summary::parse(&summary.text()).expect("parses");
        assert_eq!(parsed, summary);
        assert_eq!(parsed.u64("run.rate"), Some(20_000));
        assert_eq!(parsed.ns("stage.client.core_path.p99"), Some(Some(812)));
        assert_eq!(parsed.ns("stage.client.to_durable_ack.p99"), Some(Some(u64::MAX)));
        assert_eq!(parsed.ns("stage.client.ingress_wait.p99"), Some(None));
        assert_eq!(parsed.ns("absent"), None);
        assert_eq!(parsed.with_prefix("run.").count(), 2);
        assert_eq!(parsed.get("machine.layout"), Some("core=2, sequencer=3"), "values may hold `=`");
        assert_eq!(parsed.get("machine.commit"), Some(""), "and be empty");

        let path = std::env::temp_dir().join(format!("bench-summary-{}.txt", std::process::id()));
        summary.write(&path).expect("written");
        assert_eq!(Summary::read(&path).expect("read"), summary);
        assert!(!path.with_extension("txt.partial").exists(), "renamed into place");
        std::fs::remove_file(&path).expect("removed");
    }

    #[test]
    fn a_histogram_is_written_as_count_p50_p99_p999_and_max() {
        let mut histogram = LatencyHistogram::new();
        for ns in 1..=1_000 {
            histogram.record(ns);
        }
        let mut summary = Summary::new();
        summary.put_histogram("h", &histogram);
        summary.put_histogram("empty", &LatencyHistogram::new());
        assert_eq!(summary.u64("h.count"), Some(1_000));
        assert_eq!(summary.ns("h.p50"), Some(histogram.percentile(50, 100)));
        assert_eq!(summary.ns("h.max"), Some(Some(1_000)));
        assert_eq!(summary.ns("empty.p99"), Some(None), "no data");
    }

    #[test]
    fn bad_lines_are_refused_with_their_number() {
        assert!(Summary::parse("# comment\n\na = 1\n").is_ok());
        let error = Summary::parse("a = 1\nno equals sign\n").expect_err("refused");
        assert!(error.starts_with("line 2"), "{error}");
    }
}
