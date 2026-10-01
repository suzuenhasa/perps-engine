//! Minimal, read-only scanning of Polymarket frames.
//!
//! The recorder stores frames byte-for-byte and never parses them into a data structure,
//! so it needs no JSON library (`docs/DECISIONS.md` D-006). These helpers read just the
//! few fields it needs from the documented layout of a data frame,
//! `{"ch":"book::17","ts":1767225600000,"sq":...,"data":...}`, and of a subscription reply,
//! `{"id":3,"data":[{"status":"ok"}]}`.

/// The `"ch"` (channel) field, e.g. `book::17`. Subscription replies have none.
pub fn channel_of(frame: &str) -> Option<&str> {
    const KEY: &str = "\"ch\":\"";
    let start = frame.find(KEY)? + KEY.len();
    let len = frame[start..].find('"')?;
    Some(&frame[start..start + len])
}

/// The frame's server timestamp in Unix ms: the first `"ts":` field. In data frames that
/// is the top-level one, which comes right after `"ch"`.
pub fn server_ts(frame: &str) -> Option<u64> {
    number_after(frame, "\"ts\":")
}

/// Instrument ids mentioned in a ticker frame, e.g.
/// `{"ch":"tickers::17",...,"data":{"iid":17,...}}`. Other frames return nothing.
pub fn instrument_ids(frame: &str) -> Vec<u32> {
    const KEY: &str = "\"iid\":";
    if !channel_of(frame).is_some_and(|ch| ch.starts_with("tickers::")) {
        return Vec::new();
    }
    let mut ids = Vec::new();
    let mut rest = frame;
    while let Some(at) = rest.find(KEY) {
        rest = &rest[at + KEY.len()..];
        if let Some(id) = number_after(rest, "") {
            ids.push(id as u32);
        }
    }
    ids
}

/// A subscription reply that reports an error, e.g.
/// `{"id":51,"data":[{"status":"err","error":"subscription limit reached"}]}`.
pub fn is_error_reply(frame: &str) -> bool {
    channel_of(frame).is_none() && frame.contains("\"status\":\"err\"")
}

/// Whether a frame can be embedded in a JSONL line as it is: it must look like a single
/// JSON object or array, with no line breaks. Anything else is stored escaped instead.
pub fn embeddable(frame: &str) -> bool {
    (frame.starts_with('{') || frame.starts_with('[')) && !frame.contains(['\n', '\r'])
}

/// Escapes a string for use inside a JSON string literal.
pub fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if c.is_control() => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// The unsigned integer that follows the first occurrence of `key` (after optional
/// spaces). With an empty key, the integer at the start of `text`.
fn number_after(text: &str, key: &str) -> Option<u64> {
    let start = text.find(key)? + key.len();
    let rest = text[start..].trim_start();
    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
    rest[..digits].parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const TICKER: &str = r#"{"ch":"tickers::17","ts":1790692642002,"ets":1790692642000,"sq":5,"data":{"iid":17,"mark":"1.0"}}"#;
    const REPLY_OK: &str = r#"{"id":3,"data":[{"status":"ok"}]}"#;
    const REPLY_ERR: &str =
        r#"{"id":51,"data":[{"status":"ok"},{"status":"err","error":"subscription limit reached"}]}"#;

    #[test]
    fn reads_channel_and_server_time() {
        assert_eq!(channel_of(TICKER), Some("tickers::17"));
        assert_eq!(server_ts(TICKER), Some(1790692642002));
        assert_eq!(channel_of(REPLY_OK), None);
    }

    #[test]
    fn instrument_ids_come_only_from_ticker_frames() {
        assert_eq!(instrument_ids(TICKER), vec![17]);
        let many = r#"{"ch":"tickers::all","data":[{"iid":1},{"iid": 42}]}"#;
        assert_eq!(instrument_ids(many), vec![1, 42]);
        let trade = r#"{"ch":"trades::17","data":{"iid":17}}"#;
        assert_eq!(instrument_ids(trade), Vec::<u32>::new());
    }

    #[test]
    fn detects_subscription_errors() {
        assert!(!is_error_reply(REPLY_OK));
        assert!(is_error_reply(REPLY_ERR));
        assert!(!is_error_reply(TICKER));
    }

    #[test]
    fn only_single_line_json_is_embedded_as_is() {
        assert!(embeddable(TICKER));
        assert!(!embeddable("rate limited"));
        assert!(!embeddable("{\n\"pretty\": true\n}"));
        assert!(!embeddable(""));
    }

    #[test]
    fn json_escape_handles_quotes_backslashes_and_control_characters() {
        assert_eq!(json_escape(r#"a "b" \c"#), r#"a \"b\" \\c"#);
        assert_eq!(json_escape("line\nbreak"), "line\\u000abreak");
    }
}
