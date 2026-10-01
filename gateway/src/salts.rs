//! The EIP-712 scheme's replay table: every request a gateway accepted in the last 5
//! minutes, so that no signed request is accepted twice (`docs/PIPELINE.md` 5.8;
//! `docs/DECISIONS.md` D-033). The nonce scheme needs one number per account (section 6);
//! this scheme needs one entry per request.
//!
//! **What a request is.** A message of the EIP-712 scheme carries a salt (any `u64` the
//! client likes) and a timestamp `ts` in milliseconds, and the gateway refuses it if `ts`
//! is more than 5 minutes behind its clock or more than 60 s ahead (`check.rs`). The same
//! `(account, salt, ts)` is the same signed request (the rest of what is signed is the
//! command, and a client never signs two commands with one salt and timestamp). The table's
//! key adds the command's market, for the reason below: a request is
//! `(account, salt, ts, market)`, and the gateway accepts each request once. Keying by the
//! salt alone would refuse a busy client's honest collisions: the official SDKs draw 32-bit
//! salts at random.
//!
//! **Why the market is in the key** (the owner's choice, 2026-09-30; D-033, "Trade-offs").
//! Polymarket's cancel and modify sign the order id, not the market (`eip712.rs`), but our
//! engine finds an order through its market, so the market travels unsigned. With the key
//! `(account, salt, ts)`, a copy of a signed cancel or modify with another market passed
//! every check (its signature is valid) and used up the request, so the genuine message,
//! sent after it, was refused as `ReusedRequest`. With the market in the key, that copy is
//! another request:
//! - the gateway accepts it, and the engine rejects it: `UnknownMarket` for a market that
//!   doesn't exist, `UnknownOrder` for one that does (an order id's sequence is unique per
//!   account across all markets, so the order rests in its own market only);
//! - the genuine message is still accepted, whether it comes before the copy or after it;
//! - a copy with the *same* market is the same request: `ReusedRequest`.
//!
//! So whoever holds the signed bytes of a cancel or a modify can make the engine reject up
//! to one junk copy per market id (the other 65,535 values of a `u16`, while the `ts` is in
//! the window), but can never make the genuine message `ReusedRequest`, and never reach
//! another order. Each copy is still an accepted message: it costs a recovery, takes a slot
//! here, a lane slot and a journal record, like any other. So copies count against the
//! table's room: a flood of them could fill a table sized for the honest requests only
//! (`SaltTableFull`, "Full" below), which a network gateway's per-session limits would have
//! to bound (`docs/PIPELINE.md` 7.4). A place's market is signed: a copy with another
//! market fails the signer check (`WrongSigner`) and never takes a slot, before or after
//! the genuine place. The one difference for places: such a copy of a place already
//! accepted used to be refused as `ReusedRequest`, before the recovery; it is now another
//! request, refused as `WrongSigner` after one, as any forgery is.
//!
//! **Contract.**
//! - [`SaltTable::find`] looks a request up: `ReusedRequest` if it was accepted before, and
//!   still inside the window; `SaltTableFull` if it wasn't, but the table has no room for
//!   it; otherwise the [`Vacancy`] where it would go. It changes nothing.
//! - [`SaltTable::insert`] puts the request in its vacancy. The gateway calls it only once
//!   the message passed every check, the signature included, so a forged message never
//!   uses up a request and never takes a slot.
//! - Nothing allocates after [`SaltTable::new`], which allocates the table and writes every
//!   slot once, so that no page of it is first touched inside the measured window (the
//!   rings' `pre_touch` does the same; 15.4). The gateway's constructor runs it on the
//!   run's main thread, before the gateway threads start.
//!
//! **The table.** Open addressing with linear probing, in a power-of-two number of slots of
//! 24 bytes each (`salt` and `ts`, 8 bytes each; `account`, 4; `market`, 2; and a used
//! flag, 1: 23 bytes, padded to 24 for the alignment of the `u64`s, so the market took 2 of
//! the 3 bytes of padding and the slot stayed at 24). A request's first slot is its hash of
//! all four fields modulo the capacity, and a lookup walks on from there until it finds the
//! request or an unused slot. At most half the slots are ever used, so a walk is short
//! (about 2.5 slots on average when half full, Knuth's analysis of linear probing) and
//! always ends.
//!
//! **Why a seeded hash.** Salts are chosen by clients. With a public hash, a client could
//! pick salts that all start at one slot, and each lookup would walk the whole cluster
//! (hash flooding). The hash is the engine's, `splitmix64` of the key mixed with a secret
//! seed (`engine::id_hash`, D-011), with a seed per gateway ([`random_seed`]). The seed
//! changes only where requests sit, never which ones are accepted, except through
//! `SaltTableFull` (below).
//!
//! **Expiry, lazily.** A request whose `ts` is more than 5 minutes behind the clock can
//! never be accepted again: check 7 refuses it first. So its slot may be reused, but only
//! lazily: an insert takes the first expired slot on the request's walk, if there is one,
//! before the unused slot at its end. An expired slot never ends a lookup, as an unused one
//! does: requests further along the walk are still found. Nothing is ever deleted, so the
//! count of used slots never falls.
//!
//! **Full.** An insert that would take an unused slot when half the slots are used is
//! refused (`SaltTableFull`), rather than grow the table, which would allocate. Size the
//! table for the run: [`SaltTable::new`] with `requests` at least the requests the gateway
//! may accept in the run never refuses one (a run shorter than 5 minutes expires nothing).
//! In a longer run, expired slots are reused only where an insert's walk passes one, so a
//! table smaller than the run's requests can refuse some once it is half used; a long-lived
//! deployment would need to delete expired requests (backward-shift deletion keeps linear
//! probing correct), which v1 doesn't do.
//!
//! **Complexity.** `find`: O(the walk), a few slots on average. `insert`: O(1). Memory: 24
//! bytes per slot, 48 to 96 bytes per request it has room for.

use std::fmt;
use std::hash::{BuildHasher, Hasher, RandomState};

use engine::command::Command;
use engine::id_hash::IdBuildHasher;
use engine::types::{AccountId, MarketId};

use crate::check::{GatewayReject, is_too_old};

/// One signed request: the account it is for, its salt and timestamp, and the market of
/// its command (module docs, "What a request is"). The audit keys its own check by the
/// same four fields (`audit.rs`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Request {
    pub account: AccountId,
    pub salt: u64,
    /// Milliseconds since the UNIX epoch.
    pub ts_ms: u64,
    /// The command's market: signed in a place, not in a cancel or a modify (module docs,
    /// "Why the market is in the key").
    pub market: MarketId,
}

impl Request {
    /// The request of `command`, sent for `account` with `salt` and `ts_ms`: a place's,
    /// a cancel's or a modify's market goes in the key. `None` for an operator command,
    /// which no client may send (it has no signed form, `eip712.rs`).
    pub fn of(account: AccountId, salt: u64, ts_ms: u64, command: &Command) -> Option<Request> {
        let market = match command {
            Command::PlaceOrder(place) => place.market,
            Command::CancelOrder(cancel) => cancel.market,
            Command::ModifyOrder(modify) => modify.market,
            _ => return None,
        };
        Some(Request { account, salt, ts_ms, market })
    }
}

/// One slot of the table: a request, if `used`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Slot {
    salt: u64,
    ts_ms: u64,
    account: AccountId,
    market: MarketId,
    /// False until a request is put here; never false again after that.
    used: bool,
}

/// Bytes per slot (module docs, "The table"): 24, as before the market was added, since
/// its 2 bytes fit in what was padding.
pub const SLOT_BYTES: usize = size_of::<Slot>();
const _: () = assert!(SLOT_BYTES == 24);

impl Slot {
    /// A slot never used: all zeros.
    const UNUSED: Slot = Slot { salt: 0, ts_ms: 0, account: 0, market: 0, used: false };

    fn holding(request: Request) -> Slot {
        Slot {
            salt: request.salt,
            ts_ms: request.ts_ms,
            account: request.account,
            market: request.market,
            used: true,
        }
    }

    /// True if this slot holds `request`: all four fields equal.
    fn holds(&self, request: Request) -> bool {
        self.used
            && self.salt == request.salt
            && self.ts_ms == request.ts_ms
            && self.account == request.account
            && self.market == request.market
    }
}

/// Where [`SaltTable::find`] would put a request, for [`SaltTable::insert`].
#[derive(Debug, PartialEq, Eq)]
#[must_use]
pub struct Vacancy {
    request: Request,
    /// The slot: the first expired one on the request's walk, or the unused one at its end.
    index: usize,
    /// The clock of the lookup, for the insert's check that the slot is still free.
    now_ms: u64,
}

/// A gateway's replay table (module docs).
pub struct SaltTable {
    /// A power of two of them.
    slots: Box<[Slot]>,
    /// Slots that hold a request, expired or not: at most half of them.
    used: usize,
    hasher: IdBuildHasher,
}

impl SaltTable {
    /// A table with room for at least `requests` requests, hashed with `seed`: the smallest
    /// power of two of slots that is at least twice `requests` (module docs, "Full"). It is
    /// allocated and every slot written once, here (module docs, "Contract").
    pub fn new(requests: usize, seed: u64) -> SaltTable {
        let capacity = SaltTable::capacity_for(requests);
        let mut slots = vec![Slot::UNUSED; capacity].into_boxed_slice();
        pre_touch(&mut slots);
        SaltTable { slots, used: 0, hasher: IdBuildHasher::new(seed) }
    }

    /// The slots a table with room for `requests` requests has (at least 2).
    pub fn capacity_for(requests: usize) -> usize {
        requests
            .max(1)
            .checked_mul(2)
            .and_then(usize::checked_next_power_of_two)
            .expect("a table that fits memory")
    }

    /// Slots.
    pub fn capacity(&self) -> usize {
        self.slots.len()
    }

    /// The most slots that may be used: half of them.
    pub fn room(&self) -> usize {
        self.slots.len() / 2
    }

    /// Slots that hold a request, expired or not.
    pub fn used(&self) -> usize {
        self.used
    }

    /// True if no request was ever put in.
    pub fn is_empty(&self) -> bool {
        self.used == 0
    }

    /// Looks `request` up at the gateway's clock `now_ms` (module docs, "Contract"):
    /// `ReusedRequest` if it is in the table, `SaltTableFull` if it isn't and has no room,
    /// otherwise where it would go. Changes nothing.
    pub fn find(&self, request: Request, now_ms: u64) -> Result<Vacancy, GatewayReject> {
        let mask = self.slots.len() - 1;
        let mut index = self.first_slot(request);
        let mut expired = None; // the first expired slot on the walk
        loop {
            let slot = self.slots[index];
            if !slot.used {
                // The end of the walk: the request was never put in.
                return match expired {
                    Some(reusable) => Ok(Vacancy { request, index: reusable, now_ms }),
                    None if self.used < self.room() => Ok(Vacancy { request, index, now_ms }),
                    None => Err(GatewayReject::SaltTableFull),
                };
            }
            if slot.holds(request) {
                return Err(GatewayReject::ReusedRequest);
            }
            // An expired slot doesn't end the walk (module docs, "Expiry, lazily").
            if expired.is_none() && is_too_old(slot.ts_ms, now_ms) {
                expired = Some(index);
            }
            index = (index + 1) & mask;
        }
    }

    /// Puts the request of `vacancy` in its slot. Call it before anything else changes the
    /// table: the gateway finds and inserts within one message's checks.
    pub fn insert(&mut self, vacancy: Vacancy) {
        let slot = &mut self.slots[vacancy.index];
        debug_assert!(!slot.used || is_too_old(slot.ts_ms, vacancy.now_ms), "the vacancy was taken");
        if !slot.used {
            self.used += 1;
        }
        *slot = Slot::holding(vacancy.request);
        debug_assert!(self.used <= self.room());
    }

    /// Where `request`'s walk starts: its seeded hash, modulo the capacity.
    fn first_slot(&self, request: Request) -> usize {
        // The engine's hasher is made for one integer key, but each write folds its value
        // into the state, `state = splitmix64(state ^ value)`, starting from the seed; so
        // the four writes chain into one hash of the whole request. (Unlike one key's hash,
        // this is not a bijection: two requests may share a hash, which only puts them on
        // the same walk.)
        let mut hasher = self.hasher.build_hasher();
        hasher.write_u32(request.account);
        hasher.write_u64(request.salt);
        hasher.write_u64(request.ts_ms);
        // The hasher has no `u16` write of its own (std's default would feed it the two
        // bytes one at a time), so the market goes in widened, as one value.
        hasher.write_u64(u64::from(request.market));
        // The capacity is a power of two, so this keeps the hash's low bits, which the
        // splitmix64 scramble makes depend on every bit of the key.
        hasher.finish() as usize & (self.slots.len() - 1)
    }
}

impl fmt::Debug for SaltTable {
    /// The sizes only: not the requests, and not the seed (a secret, as the engine's is).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SaltTable")
            .field("capacity", &self.slots.len())
            .field("used", &self.used)
            .finish_non_exhaustive()
    }
}

/// A seed no client can predict, for a gateway's table: drawn from the operating system's
/// randomness through std's `RandomState` (the key of std's `HashMap`), so no crate is
/// added. Each call gives another seed.
pub fn random_seed() -> u64 {
    RandomState::new().hash_one(0u64)
}

/// Writes every slot once (module docs, "Contract"). `black_box` hides the value from the
/// compiler, so it can't drop the writes as storing what is already there (the engine's
/// `prefault` does the same).
fn pre_touch(slots: &mut [Slot]) {
    for slot in slots {
        *slot = std::hint::black_box(Slot::UNUSED);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::check::MAX_AGE_MS;
    use crate::test_support::XorShift;

    /// A clock in milliseconds since the UNIX epoch.
    const NOW: u64 = 1_790_000_000_000;
    const SEED: u64 = 0x5A17;
    /// The market of the tests' requests, where a test doesn't vary it.
    const MARKET: MarketId = 3;

    /// A request on market 3.
    fn request(account: AccountId, salt: u64, ts_ms: u64) -> Request {
        Request { account, salt, ts_ms, market: MARKET }
    }

    /// `count` different requests of account 9 with timestamp `ts_ms` whose walks all start
    /// at slot `first` of `table` (found by trying salts: about one in `capacity` does).
    fn colliding(table: &SaltTable, first: usize, ts_ms: u64, count: usize) -> Vec<Request> {
        (0..)
            .map(|salt| request(9, salt, ts_ms))
            .filter(|r| table.first_slot(*r) == first)
            .take(count)
            .collect()
    }

    #[test]
    fn the_capacity_is_a_power_of_two_at_least_twice_the_requests() {
        for (requests, capacity) in
            [(0, 2), (1, 2), (2, 4), (3, 8), (4, 8), (5, 16), (1_000, 2_048), (1 << 20, 1 << 21)]
        {
            assert_eq!(SaltTable::capacity_for(requests), capacity, "{requests} requests");
        }
        let table = SaltTable::new(3, SEED);
        assert_eq!((table.capacity(), table.room(), table.used()), (8, 4, 0));
        assert!(table.is_empty());
    }

    #[test]
    fn a_request_is_new_until_it_is_inserted_and_then_it_is_reused() {
        let mut table = SaltTable::new(16, SEED);
        let first = request(9, 42, NOW);
        let vacancy = table.find(first, NOW).expect("new");
        assert_eq!(table.used(), 0, "find changes nothing");
        table.insert(vacancy);
        assert_eq!(table.used(), 1);
        assert_eq!(table.find(first, NOW), Err(GatewayReject::ReusedRequest));
        // Another account, salt, timestamp or market is another request.
        let another_market = Request { market: MARKET + 1, ..first };
        for other in [request(10, 42, NOW), request(9, 43, NOW), request(9, 42, NOW + 1), another_market] {
            assert!(table.find(other, NOW).is_ok(), "{other:?}");
        }
        // Still reused at the edge of the window: 5 minutes exactly.
        assert_eq!(table.find(first, NOW + MAX_AGE_MS), Err(GatewayReject::ReusedRequest));
    }

    #[test]
    fn colliding_requests_line_up_and_are_all_found_even_across_the_end_of_the_table() {
        let mut table = SaltTable::new(8, SEED); // 16 slots, room for 8
        let last = table.capacity() - 1;
        let requests = colliding(&table, last, NOW, 5);
        for (i, r) in requests.iter().enumerate() {
            let vacancy = table.find(*r, NOW).expect("new");
            // They take slots 15, 0, 1, 2, 3: the walk wraps around.
            assert_eq!(vacancy.index, (last + i) % table.capacity(), "request {i}");
            table.insert(vacancy);
        }
        for r in &requests {
            assert_eq!(table.find(*r, NOW), Err(GatewayReject::ReusedRequest), "{r:?}");
        }
        assert_eq!(table.used(), 5);
    }

    #[test]
    fn an_expired_slot_is_reused_by_an_insert_but_never_ends_a_lookup() {
        let mut table = SaltTable::new(8, SEED);
        // Three requests whose walks start at slot 3: one a second old, one of now, and one
        // of `later`, when the first has left the window (5 minutes and 1 ms behind the
        // clock) and the second hasn't.
        let old = colliding(&table, 3, NOW - 1_000, 1)[0];
        let recent = colliding(&table, 3, NOW, 1)[0];
        let later = old.ts_ms + MAX_AGE_MS + 1;
        let newest = colliding(&table, 3, later, 1)[0];
        for r in [old, recent] {
            let vacancy = table.find(r, NOW).expect("new");
            table.insert(vacancy);
        }
        // A lookup of `recent` walks past the expired slot and still finds it.
        assert_eq!(table.find(recent, later), Err(GatewayReject::ReusedRequest));
        // A new request on the same walk takes the expired slot, not a new one.
        let vacancy = table.find(newest, later).expect("new");
        assert_eq!(vacancy.index, 3, "the expired slot");
        table.insert(vacancy);
        assert_eq!(table.used(), 2, "no slot more is used");
        assert_eq!(table.find(recent, later), Err(GatewayReject::ReusedRequest));
        assert_eq!(table.find(newest, later), Err(GatewayReject::ReusedRequest));
        // One millisecond earlier, `old` was not yet expired: its slot was not free.
        let mut fresh = SaltTable::new(8, SEED);
        fresh.insert(fresh.find(old, NOW).expect("new"));
        let vacancy = fresh.find(newest, later - 1).expect("new");
        assert_eq!(vacancy.index, 4, "the next unused slot");
    }

    #[test]
    fn a_half_used_table_refuses_a_new_request_unless_its_walk_passes_an_expired_slot() {
        let mut table = SaltTable::new(2, SEED); // 4 slots, room for 2
        let mut random = XorShift(7);
        let mut inserted = Vec::new();
        while table.used() < table.room() {
            let r = request(9, random.next(), NOW);
            table.insert(table.find(r, NOW).expect("room"));
            inserted.push(r);
        }
        for _ in 0..50 {
            let r = request(9, random.next(), NOW);
            assert_eq!(table.find(r, NOW), Err(GatewayReject::SaltTableFull), "{r:?}");
        }
        // A request already in is still reported as reused, not as a full table.
        assert_eq!(table.find(inserted[0], NOW), Err(GatewayReject::ReusedRequest));
        // Once both have expired, a request whose walk passes one of their slots gets it;
        // one whose walk starts at an unused slot is still refused (module docs, "Full").
        let later = NOW + MAX_AGE_MS + 1;
        let (mut reused, mut refused) = (0, 0);
        for _ in 0..50 {
            let r = request(9, random.next(), later);
            match table.find(r, later) {
                Ok(vacancy) => {
                    assert!(table.slots[vacancy.index].used, "an expired slot, never an unused one");
                    reused += 1;
                }
                Err(reason) => {
                    assert_eq!(reason, GatewayReject::SaltTableFull);
                    assert!(!table.slots[table.first_slot(r)].used, "a walk that starts at an unused slot");
                    refused += 1;
                }
            }
        }
        assert!(reused > 0 && refused > 0, "{reused} reused, {refused} refused");
    }

    #[test]
    fn random_requests_are_reused_exactly_when_they_were_inserted_and_the_table_never_passes_half() {
        // A model of 5.8's replay rule over a clock that moves on: a request is reused if
        // it was inserted before, and only then. Timestamps stay inside the window, as the
        // gateway's check 7 makes sure before any lookup.
        let mut table = SaltTable::new(300, SEED);
        let mut inserted = std::collections::HashSet::new();
        let mut random = XorShift(0x5A17_7AB1E);
        let mut now = NOW;
        let (mut reused, mut full) = (0, 0);
        for step in 0..20_000 {
            now += random.below(40); // about 400 s over the run: requests expire
            // A whole second inside the window, and few salts and markets, so that requests
            // repeat, and some differ only in their market.
            let ts_ms = (now / 1_000 - random.below(MAX_AGE_MS / 1_000)) * 1_000;
            let r = Request {
                account: random.below(4) as AccountId,
                salt: random.below(32),
                ts_ms,
                market: MARKET + random.below(2) as MarketId,
            };
            match table.find(r, now) {
                Err(GatewayReject::ReusedRequest) => {
                    assert!(inserted.contains(&r), "step {step}: {r:?}");
                    reused += 1;
                }
                Err(GatewayReject::SaltTableFull) => {
                    assert!(!inserted.contains(&r), "step {step}");
                    assert_eq!(table.used(), table.room(), "step {step}: full only when half used");
                    full += 1;
                }
                Err(other) => panic!("step {step}: {other}"),
                Ok(vacancy) => {
                    assert!(inserted.insert(r), "step {step}: {r:?} was in");
                    table.insert(vacancy);
                }
            }
            assert!(table.used() <= table.room(), "step {step}");
        }
        assert!(reused > 100, "{reused} reused: the model's claim is tested");
        assert!(full > 0 && table.used() == table.room(), "{full} full: the table filled");
    }

    #[test]
    fn copies_of_a_request_on_other_markets_are_other_requests_and_each_is_accepted_once() {
        // Module docs, "Why the market is in the key": the same account, salt and timestamp
        // on three markets are three requests, each accepted once, in any order.
        let mut table = SaltTable::new(16, SEED);
        let genuine = request(9, 42, NOW);
        let copies = [Request { market: 2, ..genuine }, Request { market: u16::MAX, ..genuine }];
        for r in [copies[0], genuine, copies[1]] {
            table.insert(table.find(r, NOW).expect("new"));
        }
        assert_eq!(table.used(), 3, "one slot each");
        for r in [genuine, copies[0], copies[1]] {
            assert_eq!(table.find(r, NOW), Err(GatewayReject::ReusedRequest), "{r:?}");
        }
    }

    #[test]
    fn the_request_of_a_command_takes_its_market_and_operator_commands_have_none() {
        use crate::test_support::{cancel, modify, place, with_market};
        for command in [place(9, 1), cancel(9, 1), modify(9, 1)] {
            let on_7 = with_market(&command, 7);
            assert_eq!(Request::of(9, 42, NOW, &command), Some(request(9, 42, NOW)));
            assert_eq!(Request::of(9, 42, NOW, &on_7), Some(Request { market: 7, ..request(9, 42, NOW) }));
        }
        let mark = Command::SetMark(engine::command::SetMark { price: 1, market: 3 });
        assert_eq!(Request::of(9, 42, NOW, &mark), None);
    }

    #[test]
    fn every_field_of_the_request_moves_where_its_walk_starts() {
        // The hash covers all four fields: changing any one of them alone moves the first
        // slot of almost every request (in 1,024 slots, about 1 in 1,024 stays by chance).
        let table = SaltTable::new(1 << 9, SEED);
        assert_eq!(table.capacity(), 1 << 10);
        let mut moved = [0; 4]; // for the account, the salt, the timestamp and the market
        for salt in 0..1_000 {
            let r = request(9, salt, NOW);
            let edited = [
                Request { account: 17, ..r },
                Request { salt: salt + 1_000_000, ..r },
                Request { ts_ms: NOW + 1, ..r },
                Request { market: MARKET + 1, ..r },
            ];
            for (count, other) in moved.iter_mut().zip(edited) {
                if table.first_slot(other) != table.first_slot(r) {
                    *count += 1;
                }
            }
        }
        for (field, count) in ["account", "salt", "ts", "market"].into_iter().zip(moved) {
            assert!(count > 990, "{field}: only {count} of 1,000 moved");
        }
    }

    #[test]
    fn the_seed_moves_where_requests_start() {
        let (a, b) = (SaltTable::new(1 << 10, 1), SaltTable::new(1 << 10, 2));
        let moved = (0..1_000)
            .filter(|&salt| a.first_slot(request(9, salt, NOW)) != b.first_slot(request(9, salt, NOW)))
            .count();
        assert!(moved > 990, "only {moved} of 1,000 moved");
        assert_ne!(random_seed(), random_seed(), "each call gives another seed");
        let text = format!("{:?}", SaltTable::new(1, 0xDEAD_BEEF));
        assert!(!text.to_lowercase().contains("deadbeef") && !text.contains("3735928559"), "{text}");
    }
}
