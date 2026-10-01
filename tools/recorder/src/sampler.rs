//! Keeps one frame per second of each high-rate channel (`docs/DECISIONS.md` D-007).
//!
//! Book snapshots and tickers arrive about every 100 ms per instrument; keeping them all
//! is roughly 55 GiB a day. For each such channel the sampler keeps the **last frame of
//! every one-second bucket of Polymarket's own clock** (the frame's `ts`). That matches how
//! Polymarket buckets its published mark history: bucket T holds the last value in
//! [T, T + 1 s). Sampling by the server's second, not by elapsed time, gives exactly one
//! sample per bucket, so no bucket is skipped.
//!
//! A bucket's last frame is only known once the next bucket starts, so the sampler *holds*
//! the latest frame of each channel and releases it when a frame from a later bucket
//! arrives. Book frames are full snapshots, not deltas, so the frames it drops carry no
//! information the kept ones need.
//!
//! Everything else (trades, subscription replies) passes straight through.

use std::collections::BTreeMap;

use crate::frames::{channel_of, server_ts};

/// Channels sampled to one frame per second; all others are kept in full.
pub fn is_sampled(channel: &str) -> bool {
    channel.starts_with("book::") || channel.starts_with("tickers::")
}

/// A frame waiting for its bucket to close.
#[derive(Debug, PartialEq, Eq)]
pub struct Held {
    pub recv_ms: u64,
    pub frame: String,
    /// `ts / 1000` of the frame.
    bucket: u64,
}

/// What to do with a frame just received.
#[derive(Debug, PartialEq, Eq)]
pub enum Offer {
    /// Not a sampled channel: write it now.
    PassThrough,
    /// Held (or dropped); nothing to write yet.
    Held,
    /// The previous bucket closed: write this earlier frame. The new one is now held.
    Released(Held),
}

/// See the module docs. One sampler per connection; each channel lives on one connection.
#[derive(Debug, Default)]
pub struct Sampler {
    held: BTreeMap<String, Held>,
}

impl Sampler {
    pub fn offer(&mut self, recv_ms: u64, frame: &str) -> Offer {
        let Some(channel) = channel_of(frame).filter(|ch| is_sampled(ch)) else {
            return Offer::PassThrough;
        };
        let Some(ts) = server_ts(frame) else { return Offer::PassThrough };
        let bucket = ts / 1000;
        let fresh = || Held { recv_ms, frame: frame.to_string(), bucket };

        match self.held.get_mut(channel) {
            None => {
                self.held.insert(channel.to_string(), fresh());
                Offer::Held
            }
            // Same bucket: this frame is now the latest; the previous one is dropped.
            Some(held) if held.bucket == bucket => {
                held.recv_ms = recv_ms;
                held.frame.clear();
                held.frame.push_str(frame);
                Offer::Held
            }
            // A late frame from an older bucket: already superseded, drop it.
            Some(held) if bucket < held.bucket => Offer::Held,
            // A new bucket: the held frame was its bucket's last.
            Some(held) => Offer::Released(std::mem::replace(held, fresh())),
        }
    }

    /// Everything still held, e.g. when a connection drops. Each is the latest frame seen
    /// for its channel, though its bucket may not have closed.
    pub fn release_all(&mut self) -> Vec<Held> {
        std::mem::take(&mut self.held).into_values().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn book(ts: u64) -> String {
        format!(r#"{{"ch":"book::1","ts":{ts},"data":{{"b":[],"a":[]}}}}"#)
    }

    #[test]
    fn keeps_the_last_frame_of_each_second() {
        let mut sampler = Sampler::default();
        let mut written = Vec::new();
        // Frames every 100 ms from 10.000 s to 12.000 s of server time.
        for ts in (10_000..=12_000).step_by(100) {
            if let Offer::Released(held) = sampler.offer(ts, &book(ts)) {
                written.push(held.recv_ms);
            }
        }
        // The last frame of bucket 10 and of bucket 11; bucket 12's is still held.
        assert_eq!(written, vec![10_900, 11_900]);
        let rest = sampler.release_all();
        assert_eq!(rest.len(), 1);
        assert_eq!(rest[0].recv_ms, 12_000);
    }

    #[test]
    fn trades_and_replies_pass_straight_through() {
        let mut sampler = Sampler::default();
        let trade = r#"{"ch":"trades::1","ts":10000,"data":{}}"#;
        assert_eq!(sampler.offer(0, trade), Offer::PassThrough);
        assert_eq!(sampler.offer(0, trade), Offer::PassThrough);
        assert_eq!(sampler.offer(0, r#"{"id":3,"data":[{"status":"ok"}]}"#), Offer::PassThrough);
    }

    #[test]
    fn channels_are_sampled_independently_and_late_frames_are_dropped() {
        let mut sampler = Sampler::default();
        let other = |ts: u64| format!(r#"{{"ch":"tickers::2","ts":{ts},"data":{{}}}}"#);
        assert_eq!(sampler.offer(1, &book(10_500)), Offer::Held);
        assert_eq!(sampler.offer(2, &other(10_600)), Offer::Held);
        assert_eq!(sampler.offer(3, &book(9_900)), Offer::Held, "an older bucket is dropped");
        assert!(matches!(sampler.offer(4, &book(11_000)), Offer::Released(h) if h.recv_ms == 1));
    }
}
