//! # engine
//!
//! Pure, deterministic exchange logic for a single-operator perpetual-futures exchange.
//!
//! **Contract.** The engine is a pure function of its command stream: the same commands
//! in the same order always produce the same events and the same final state. It does no
//! I/O, reads no clock, uses no randomness, and starts no threads. All time comes from
//! timestamps the sequencer stamps on commands. This is what makes replay, a hot standby
//! and state roots possible (INFO.md section 4, "Determinism rules").
//!
//! **Invariants** (`docs/RISK.md` section 12; checked by tests through
//! [`engine::Engine::assert_invariants`], and by cheap assertions in debug builds): the
//! book is never crossed; positions sum to zero per market; money is conserved to the
//! micro; no slot is left below maintenance margin at a command boundary.
//!
//! **Modules.**
//! - [`types`]: units (ticks, lots, micro-dollars) and ids.
//! - [`command`] / [`event`]: the engine's entire input and output, as fixed-size records.
//! - [`book`]: the order book interface and the production book: tick-indexed price
//!   levels, orders in a slab (Milestone 1).
//! - [`reference`]: the slow, obviously correct book that defines the semantics.
//! - [`engine`]: the engine itself: markets, accounts, isolated slots, the insurance fund,
//!   the pre-trade check, fills, fees, collateral release, liquidation and the price-band
//!   sweep (Milestone 2, `docs/RISK.md`).
//! - [`money`]: the risk layer's pure arithmetic: margins, equity, fees, the price band,
//!   position changes, liquidation keys, and how each rounds.
//! - [`mode`]: the compile-time switches between the fast engine and its naive references.
//! - `state` (private to the crate): accounts, slots and markets.
//! - `liquidation_index` (private to the crate): each market's slots ordered by the mark
//!   at which they must be liquidated.
//! - `level_index` (private to the crate): the bitmap the book uses to find the next
//!   non-empty price level without scanning.
//! - [`id_hash`]: the seeded hash for the id-keyed maps; public because the gateways'
//!   replay table of the EIP-712 scheme hashes with it too (`docs/DECISIONS.md` D-033).
//! - `prefault` (private to the crate): writing reserved memory once before a run, so its
//!   pages aren't first touched inside a measured window (`Engine::prefault`, Milestone 3).
//!
//! Decisions behind this layout: `docs/DECISIONS.md` D-004 (units), D-005 (record
//! layout), D-008 (book semantics), D-013 to D-020 (the risk layer).

pub mod book;
pub mod command;
pub mod engine;
pub mod event;
pub mod id_hash;
mod level_index;
mod liquidation_index;
pub mod mode;
pub mod money;
mod prefault;
pub mod reference;
mod state;
pub mod types;
