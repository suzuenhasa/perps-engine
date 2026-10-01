//! The run clock: one time base for every stamp and every journal timestamp
//! (`docs/PIPELINE.md` 9.2 and 15.1).
//!
//! **Contract.** A [`RunClock`] is a copyable value: an `Instant` taken when the run started
//! and the wall-clock time of that moment in nanoseconds since the UNIX epoch. Every thread
//! gets a copy, so all stamps are on the same clock. [`RunClock::now`] is nanoseconds since
//! the run started, and it is always at least 1: 0 means "not applicable" in the records
//! (3.3), so no real stamp may be 0.
//!
//! **Why `Instant`.** It is CLOCK_MONOTONIC, which never goes backwards, even when the
//! system clock is stepped. On Linux with a TSC clock source it is read through the vDSO,
//! with no system call, in about 20 ns. The probe (15.10) checks both, since a clock read by
//! system call would roughly double the core's cost per command.
//!
//! **Journal timestamps.** `ts = start_unix_ns + t_seq` ([`RunClock::unix_ns`]): anchored
//! to wall-clock time once, then moved only by the monotonic clock, so within one process
//! `ts` never goes backwards. A restarted process anchors itself above the journal's last
//! `ts` ([`RunClock::resume`]), so `ts` stays strictly increasing over the whole journal
//! even if the system clock was stepped back in between (9.2).
//!
//! **Complexity.** `now` is one clock read and a subtraction.

use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// The clock every pipeline thread stamps with. See the module docs.
#[derive(Clone, Copy, Debug)]
pub struct RunClock {
    start: Instant,
    start_unix_ns: u64,
}

impl RunClock {
    /// A clock for a new journal: the run starts now.
    pub fn start() -> RunClock {
        RunClock::anchored_at(unix_now_ns())
    }

    /// A clock for a process restarted on an existing journal whose last record has
    /// timestamp `last_ts`: anchored at `max(now, last_ts + 1)`, so every new `ts` is above
    /// every old one (9.2).
    pub fn resume(last_ts: u64) -> RunClock {
        RunClock::anchored_at(unix_now_ns().max(last_ts + 1))
    }

    /// A clock whose run starts now, at wall-clock time `start_unix_ns`.
    pub fn anchored_at(start_unix_ns: u64) -> RunClock {
        RunClock { start: Instant::now(), start_unix_ns }
    }

    /// Nanoseconds since the run started; at least 1 (module docs).
    pub fn now(&self) -> u64 {
        let elapsed = u64::try_from(self.start.elapsed().as_nanos()).unwrap_or(u64::MAX);
        elapsed.max(1)
    }

    /// The wall-clock time the run started, in nanoseconds since the UNIX epoch: the
    /// `run_start_unix_ns` of every segment header this process writes (11.2).
    pub fn start_unix_ns(&self) -> u64 {
        self.start_unix_ns
    }

    /// The wall-clock time of run time `t`, in nanoseconds since the UNIX epoch: a journal
    /// record's `ts` for a command sequenced at `t_seq`, and the time a gateway compares
    /// with a message's `expires_at`.
    pub fn unix_ns(&self, t: u64) -> u64 {
        self.start_unix_ns + t
    }
}

/// The system clock, in nanoseconds since the UNIX epoch.
fn unix_now_ns() -> u64 {
    let since_epoch =
        SystemTime::now().duration_since(UNIX_EPOCH).expect("the system clock is set before 1970");
    u64::try_from(since_epoch.as_nanos()).expect("the system clock is set past the year 2554")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn now_starts_above_zero_and_never_goes_backwards() {
        let clock = RunClock::start();
        let mut previous = clock.now();
        assert!(previous >= 1);
        for _ in 0..10_000 {
            let now = clock.now();
            assert!(now >= previous, "{now} after {previous}");
            previous = now;
        }
    }

    #[test]
    fn copies_share_the_same_time_base() {
        let clock = RunClock::start();
        let copy = clock;
        let (a, b) = (clock.now(), copy.now());
        assert!(b >= a);
        assert_eq!(copy.start_unix_ns(), clock.start_unix_ns());
    }

    #[test]
    fn the_anchor_is_the_wall_clock_at_start() {
        let before = unix_now_ns();
        let clock = RunClock::start();
        let after = unix_now_ns();
        assert!((before..=after).contains(&clock.start_unix_ns()));
        assert_eq!(clock.unix_ns(0), clock.start_unix_ns());
        assert_eq!(clock.unix_ns(1_500), clock.start_unix_ns() + 1_500);
    }

    #[test]
    fn a_resumed_clock_is_anchored_above_the_last_timestamp() {
        // A journal whose last ts is far in the future (the system clock was stepped back).
        let last_ts = unix_now_ns() + 3_600_000_000_000;
        let clock = RunClock::resume(last_ts);
        assert_eq!(clock.start_unix_ns(), last_ts + 1);
        assert!(clock.unix_ns(clock.now()) > last_ts);
        // A journal whose last ts is in the past: the anchor is simply now.
        let before = unix_now_ns();
        let clock = RunClock::resume(1_790_000_000_123_458_789.min(before - 1));
        assert!(clock.start_unix_ns() >= before);
    }
}
