//! What a pipeline thread does when a pass of its loop found no work (`docs/PIPELINE.md`
//! 2.2 and 19.1; `docs/DECISIONS.md` D-026).
//!
//! **Contract.** [`IdleStrategy::idle`] returns after a short pause and never blocks, so
//! the thread notices new work within about one pause. Every loop calls it only after
//! finding nothing to do (`if n == 0 { ...; idle(); continue }`), never while work waits.
//!
//! - [`IdleStrategy::Spin`], for runs: one `spin_loop` hint (the x86 `pause` instruction),
//!   so the thread keeps its CPU and reacts within about 100 ns. Waking a sleeping thread
//!   takes tens of microseconds, which would land in the latency tail, so pinned threads
//!   never sleep; each one uses a whole CPU even when idle, which is the price.
//! - [`IdleStrategy::SpinThenYield`], for tests: `spins` hints, then `yield_now`, so that a
//!   test's threads share the CPUs politely with the other tests running at the same time.
//!
//! **Complexity.** `Spin`: one instruction. `SpinThenYield`: `spins` instructions and one
//! system call.

/// How an idle thread waits for work. See the module docs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IdleStrategy {
    /// Busy-poll: one pause per idle pass.
    Spin,
    /// Pause `spins` times, then give the CPU to another thread if one is ready.
    SpinThenYield { spins: u32 },
}

impl IdleStrategy {
    /// One idle pass.
    #[inline]
    pub fn idle(self) {
        match self {
            IdleStrategy::Spin => std::hint::spin_loop(),
            IdleStrategy::SpinThenYield { spins } => {
                for _ in 0..spins {
                    std::hint::spin_loop();
                }
                std::thread::yield_now();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[test]
    fn idle_returns_for_both_strategies() {
        for strategy in [
            IdleStrategy::Spin,
            IdleStrategy::SpinThenYield { spins: 0 },
            IdleStrategy::SpinThenYield { spins: 100 },
        ] {
            for _ in 0..1_000 {
                strategy.idle();
            }
        }
    }

    #[test]
    fn a_thread_waiting_with_spin_then_yield_sees_another_threads_store() {
        let ready = AtomicBool::new(false);
        std::thread::scope(|scope| {
            scope.spawn(|| ready.store(true, Ordering::Release));
            while !ready.load(Ordering::Acquire) {
                IdleStrategy::SpinThenYield { spins: 10 }.idle();
            }
        });
    }
}
