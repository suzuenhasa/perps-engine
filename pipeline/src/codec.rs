//! The binary encodings of commands (CMD40) and events (EVT56), used everywhere: in client
//! messages, in every ring, in the journal and in event captures (`docs/PIPELINE.md`
//! section 4; `docs/DECISIONS.md` D-023).
//!
//! **Contract.** [`encode_command`] turns a [`Command`] into five `u64` words (40 bytes),
//! and [`encode_event`] an [`Event`] into seven (56 bytes). The byte form is the words'
//! little-endian bytes, word after word ([`to_le_bytes`]). Decoding accepts exactly the
//! byte strings that encoding produces, so each command and event has **one** encoding
//! (canonical form, 4.4): `decode(encode(x)) == x` for every value, and for every input
//! that decodes, `encode(decode(b)) == b`. That is what lets the journal store a decoded
//! command and still rebuild the exact bytes a client signed (13.4). Decoding checks the
//! tag, every reserved byte and every enum code; it does not check business rules (a
//! quantity of 0 decodes fine, and the engine rejects it).
//!
//! **Layout** (4.2, 4.3). Word 0 is the *head*: byte 0 the tag (commands 1 to 9, events 1
//! to 14; 0 is never a tag, so a zeroed buffer is never a valid record), bytes 1 to 3 small
//! fields `a`, `b`, `c`, bytes 4..6 the market (`u16`) and bytes 6..8 a field `x` (`u16`).
//! Then four (commands) or six (events) 8-byte fields `f1`, `f2`, ... Integers are
//! little-endian; an `i64` is its two's-complement bits, and so is a price, a quantity or an
//! amount: its number of ticks, lots or micro-dollars, read with `.ticks()`, `.lots()` or
//! `.micros()` and rebuilt with `::new`. An id is its number, read with `.get()` and
//! rebuilt with `::new`: a market's `u16` in the head, an account's `u32` or an order's
//! `u64` in a field. A `u32` in an 8-byte field takes the low 4 bytes and the others are
//! zero. The tables in 4.2 and 4.3 say which field holds what, and mark
//! the reserved bytes, which must be zero, with a dash.
//!
//! **Field by field.** The engine's structs are `repr(C)` with padding bytes whose contents
//! are undefined, so their memory is never copied: every field is placed explicitly
//! (D-005).
//!
//! **How decoding checks the reserved bytes.** It reads each field from its own bytes and
//! never looks at the dashes; then it encodes what it decoded and compares with the input.
//! The encoder writes zeros in every dash, so that one comparison checks every reserved
//! byte, and it makes canonical form true by construction rather than by a list of masks.
//!
//! **Enum codes** are the engine's declaration order (every enum is `#[repr(u8)]` with
//! implicit discriminants), converted with explicit `match`es rather than `as u8`. A test
//! pins every code both to the numbers in PIPELINE.md 4.1 and to the engine's
//! discriminants, so reordering an engine enum breaks a test instead of the journal.
//!
//! **Complexity.** O(1): a `match` on the tag and a handful of shifts per word. Decoding
//! also encodes once, for the comparison.

use std::fmt;

use engine::command::{
    CancelOrder, Command, Deposit, ModifyOrder, PlaceOrder, SetLeverage, SetMark, SetMarketParams,
    SetRiskTier, Withdraw,
};
use engine::event::{
    Ack, BalanceChanged, CancelReason, Cancelled, Event, Fill, InsuranceAbsorb, InsuranceShortfall,
    LeverageSet, Liquidation, MarkPrice, Modified, PositionChanged, Reject, RejectReason,
};
use engine::types::{AccountId, MarketId, Micros, OrderId, Price, Qty, Side, TimeInForce};

/// Words in a CMD40 encoding.
pub const COMMAND_WORDS: usize = 5;
/// Bytes in a CMD40 encoding.
pub const COMMAND_BYTES: usize = 40;
/// Words in an EVT56 encoding.
pub const EVENT_WORDS: usize = 7;
/// Bytes in an EVT56 encoding.
pub const EVENT_BYTES: usize = 56;

/// Command tags: byte 0 of CMD40 (4.2).
pub mod command_tags {
    pub const PLACE_ORDER: u8 = 1;
    pub const CANCEL_ORDER: u8 = 2;
    pub const MODIFY_ORDER: u8 = 3;
    pub const DEPOSIT: u8 = 4;
    pub const WITHDRAW: u8 = 5;
    pub const SET_LEVERAGE: u8 = 6;
    pub const SET_MARK: u8 = 7;
    pub const SET_MARKET_PARAMS: u8 = 8;
    pub const SET_RISK_TIER: u8 = 9;
}

/// Event tags: byte 0 of EVT56 (4.3). Tag 255 is the pipeline's trailer (`records.rs`),
/// never an engine event.
pub mod event_tags {
    pub const ACK: u8 = 1;
    pub const REJECT: u8 = 2;
    pub const FILL: u8 = 3;
    pub const CANCELLED: u8 = 4;
    pub const MODIFIED: u8 = 5;
    pub const POSITION_CHANGED: u8 = 6;
    pub const BALANCE_CHANGED: u8 = 7;
    pub const MARK_PRICE: u8 = 8;
    pub const LIQUIDATION: u8 = 9;
    pub const INSURANCE_ABSORB: u8 = 10;
    pub const INSURANCE_SHORTFALL: u8 = 11;
    pub const LEVERAGE_SET: u8 = 12;
    pub const MARKET_PARAMS_SET: u8 = 13;
    pub const RISK_TIER_SET: u8 = 14;
}

/// Why some words are not a valid encoding.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecodeError {
    /// Byte 0 is not a tag of this encoding.
    UnknownTag(u8),
    /// An enum code or a flag is out of range.
    BadCode { field: &'static str, code: u8 },
    /// A byte the layout reserves (a dash in the tables of 4.2 and 4.3) is not zero.
    ReservedNotZero,
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DecodeError::UnknownTag(tag) => write!(f, "unknown tag {tag}"),
            DecodeError::BadCode { field, code } => write!(f, "{field} code {code} is out of range"),
            DecodeError::ReservedNotZero => write!(f, "a reserved byte is not zero"),
        }
    }
}

impl std::error::Error for DecodeError {}

// ---------------------------------------------------------------------------------------
// The head word, shared by both encodings.

/// Word 0 of both encodings: `tag | a << 8 | b << 16 | c << 24 | market << 32 | x << 48`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Head {
    tag: u8,
    a: u8,
    b: u8,
    c: u8,
    market: u16,
    x: u16,
}

impl Head {
    /// A head with only the tag set; the other fields are filled in with `..Head::of(tag)`.
    fn of(tag: u8) -> Head {
        Head { tag, ..Head::default() }
    }

    fn pack(self) -> u64 {
        u64::from(self.tag)
            | u64::from(self.a) << 8
            | u64::from(self.b) << 16
            | u64::from(self.c) << 24
            | u64::from(self.market) << 32
            | u64::from(self.x) << 48
    }

    fn unpack(word: u64) -> Head {
        Head {
            tag: word as u8,
            a: (word >> 8) as u8,
            b: (word >> 16) as u8,
            c: (word >> 24) as u8,
            market: (word >> 32) as u16,
            x: (word >> 48) as u16,
        }
    }
}

/// The two fees of `SetMarketParams` share one 8-byte field: maker in the low 4 bytes,
/// taker in the high 4, each as its two's-complement bits.
fn pack_fees(maker_fee_ppm: i32, taker_fee_ppm: i32) -> u64 {
    u64::from(maker_fee_ppm as u32) | u64::from(taker_fee_ppm as u32) << 32
}

fn unpack_fees(field: u64) -> (i32, i32) {
    (field as u32 as i32, (field >> 32) as u32 as i32)
}

// ---------------------------------------------------------------------------------------
// Commands (CMD40).

/// A command's tag (4.2).
pub fn command_tag(command: &Command) -> u8 {
    match command {
        Command::PlaceOrder(_) => command_tags::PLACE_ORDER,
        Command::CancelOrder(_) => command_tags::CANCEL_ORDER,
        Command::ModifyOrder(_) => command_tags::MODIFY_ORDER,
        Command::Deposit(_) => command_tags::DEPOSIT,
        Command::Withdraw(_) => command_tags::WITHDRAW,
        Command::SetLeverage(_) => command_tags::SET_LEVERAGE,
        Command::SetMark(_) => command_tags::SET_MARK,
        Command::SetMarketParams(_) => command_tags::SET_MARKET_PARAMS,
        Command::SetRiskTier(_) => command_tags::SET_RISK_TIER,
    }
}

/// Encodes a command as CMD40 (the table in 4.2).
pub fn encode_command(command: &Command) -> [u64; COMMAND_WORDS] {
    let tag = command_tag(command);
    let (head, fields) = match *command {
        Command::PlaceOrder(o) => (
            Head {
                a: side_code(o.side),
                b: tif_code(o.tif),
                c: u8::from(o.post_only),
                market: o.market.get(),
                ..Head::of(tag)
            },
            [o.order_id.get(), o.price.ticks() as u64, o.qty.lots() as u64, 0],
        ),
        Command::CancelOrder(c) => {
            (Head { market: c.market.get(), ..Head::of(tag) }, [c.order_id.get(), 0, 0, 0])
        }
        Command::ModifyOrder(m) => (
            Head { market: m.market.get(), ..Head::of(tag) },
            [m.order_id.get(), m.new_price.ticks() as u64, m.new_size.lots() as u64, 0],
        ),
        Command::Deposit(d) => (Head::of(tag), [d.amount.micros() as u64, u64::from(d.account.get()), 0, 0]),
        Command::Withdraw(w) => (Head::of(tag), [w.amount.micros() as u64, u64::from(w.account.get()), 0, 0]),
        Command::SetLeverage(l) => (
            Head { market: l.market.get(), x: l.leverage, ..Head::of(tag) },
            [u64::from(l.account.get()), 0, 0, 0],
        ),
        Command::SetMark(m) => {
            (Head { market: m.market.get(), ..Head::of(tag) }, [m.price.ticks() as u64, 0, 0, 0])
        }
        Command::SetMarketParams(p) => market_params_fields(tag, &p),
        Command::SetRiskTier(r) => risk_tier_fields(tag, &r),
    };
    [head.pack(), fields[0], fields[1], fields[2], fields[3]]
}

/// Decodes CMD40 (4.4). Accepts exactly what [`encode_command`] produces.
pub fn decode_command(words: &[u64; COMMAND_WORDS]) -> Result<Command, DecodeError> {
    let head = Head::unpack(words[0]);
    let [_, f1, f2, f3, f4] = *words;
    let market = MarketId::new(head.market);
    let command = match head.tag {
        command_tags::PLACE_ORDER => Command::PlaceOrder(PlaceOrder {
            order_id: OrderId::new(f1),
            price: Price::new(f2 as i64),
            qty: Qty::new(f3 as i64),
            market,
            side: side_from_code(head.a)?,
            tif: tif_from_code(head.b)?,
            post_only: flag_from_code(head.c)?,
        }),
        command_tags::CANCEL_ORDER => {
            Command::CancelOrder(CancelOrder { order_id: OrderId::new(f1), market })
        }
        command_tags::MODIFY_ORDER => Command::ModifyOrder(ModifyOrder {
            order_id: OrderId::new(f1),
            new_price: Price::new(f2 as i64),
            new_size: Qty::new(f3 as i64),
            market,
        }),
        command_tags::DEPOSIT => {
            Command::Deposit(Deposit { amount: Micros::new(f1 as i64), account: AccountId::new(f2 as u32) })
        }
        command_tags::WITHDRAW => {
            Command::Withdraw(Withdraw { amount: Micros::new(f1 as i64), account: AccountId::new(f2 as u32) })
        }
        command_tags::SET_LEVERAGE => {
            Command::SetLeverage(SetLeverage { account: AccountId::new(f1 as u32), market, leverage: head.x })
        }
        command_tags::SET_MARK => Command::SetMark(SetMark { price: Price::new(f1 as i64), market }),
        command_tags::SET_MARKET_PARAMS => {
            Command::SetMarketParams(decode_market_params(head, [f1, f2, f3, f4]))
        }
        command_tags::SET_RISK_TIER => Command::SetRiskTier(decode_risk_tier(head, f1)),
        tag => return Err(DecodeError::UnknownTag(tag)),
    };
    // The comparison that checks every reserved byte (module docs).
    if encode_command(&command) != *words {
        return Err(DecodeError::ReservedNotZero);
    }
    Ok(command)
}

/// `SetMarketParams`, as a command (tag 8) or as its echo event (tag 13): `x` the maximum
/// leverage, f1 and f2 the price range, f3 the two fees, f4 the price band (a `u32`).
fn market_params_fields(tag: u8, p: &SetMarketParams) -> (Head, [u64; 4]) {
    let head = Head { market: p.market.get(), x: p.max_leverage, ..Head::of(tag) };
    let fields = [
        p.min_price.ticks() as u64,
        p.max_price.ticks() as u64,
        pack_fees(p.maker_fee_ppm, p.taker_fee_ppm),
        u64::from(p.price_band_ppm),
    ];
    (head, fields)
}

fn decode_market_params(head: Head, fields: [u64; 4]) -> SetMarketParams {
    let (maker_fee_ppm, taker_fee_ppm) = unpack_fees(fields[2]);
    SetMarketParams {
        min_price: Price::new(fields[0] as i64),
        max_price: Price::new(fields[1] as i64),
        maker_fee_ppm,
        taker_fee_ppm,
        price_band_ppm: fields[3] as u32,
        market: MarketId::new(head.market),
        max_leverage: head.x,
    }
}

/// `SetRiskTier`, as a command (tag 9) or as its echo event (tag 14): `a` the row index,
/// `b` the row count, `x` the maximum leverage, f1 the lower bound.
fn risk_tier_fields(tag: u8, r: &SetRiskTier) -> (Head, [u64; 4]) {
    let head = Head { a: r.index, b: r.count, market: r.market.get(), x: r.max_leverage, ..Head::of(tag) };
    (head, [r.lower_bound.micros() as u64, 0, 0, 0])
}

fn decode_risk_tier(head: Head, f1: u64) -> SetRiskTier {
    SetRiskTier {
        lower_bound: Micros::new(f1 as i64),
        market: MarketId::new(head.market),
        max_leverage: head.x,
        index: head.a,
        count: head.b,
    }
}

// ---------------------------------------------------------------------------------------
// Events (EVT56).

/// An event's tag (4.3).
pub fn event_tag(event: &Event) -> u8 {
    match event {
        Event::Ack(_) => event_tags::ACK,
        Event::Reject(_) => event_tags::REJECT,
        Event::Fill(_) => event_tags::FILL,
        Event::Cancelled(_) => event_tags::CANCELLED,
        Event::Modified(_) => event_tags::MODIFIED,
        Event::PositionChanged(_) => event_tags::POSITION_CHANGED,
        Event::BalanceChanged(_) => event_tags::BALANCE_CHANGED,
        Event::MarkPrice(_) => event_tags::MARK_PRICE,
        Event::Liquidation(_) => event_tags::LIQUIDATION,
        Event::InsuranceAbsorb(_) => event_tags::INSURANCE_ABSORB,
        Event::InsuranceShortfall(_) => event_tags::INSURANCE_SHORTFALL,
        Event::LeverageSet(_) => event_tags::LEVERAGE_SET,
        Event::MarketParamsSet(_) => event_tags::MARKET_PARAMS_SET,
        Event::RiskTierSet(_) => event_tags::RISK_TIER_SET,
    }
}

/// Encodes an event as EVT56 (the table in 4.3). Byte 3 of the head (`c`) is always zero.
pub fn encode_event(event: &Event) -> [u64; EVENT_WORDS] {
    let tag = event_tag(event);
    let (head, fields): (Head, [u64; 6]) = match *event {
        Event::Ack(e) => (Head::of(tag), [e.order_id.get(), 0, 0, 0, 0, 0]),
        Event::Reject(e) => (
            Head { a: reject_reason_code(e.reason), ..Head::of(tag) },
            [e.order_id.get(), u64::from(e.account.get()), 0, 0, 0, 0],
        ),
        Event::Fill(e) => (
            Head { a: side_code(e.taker_side), market: e.market.get(), ..Head::of(tag) },
            [
                e.maker_order.get(),
                e.taker_order.get(),
                e.price.ticks() as u64,
                e.qty.lots() as u64,
                e.maker_fee.micros() as u64,
                e.taker_fee.micros() as u64,
            ],
        ),
        Event::Cancelled(e) => (
            Head {
                a: cancel_reason_code(e.reason),
                b: side_code(e.side),
                market: e.market.get(),
                ..Head::of(tag)
            },
            [e.order_id.get(), e.remaining.lots() as u64, 0, 0, 0, 0],
        ),
        Event::Modified(e) => (
            Head { market: e.market.get(), ..Head::of(tag) },
            [e.order_id.get(), e.price.ticks() as u64, e.qty.lots() as u64, 0, 0, 0],
        ),
        Event::PositionChanged(e) => (
            Head { market: e.market.get(), ..Head::of(tag) },
            [
                e.position.lots() as u64,
                e.cost_basis.micros() as u64,
                e.locked.micros() as u64,
                u64::from(e.account.get()),
                0,
                0,
            ],
        ),
        Event::BalanceChanged(e) => {
            (Head::of(tag), [e.free.micros() as u64, u64::from(e.account.get()), 0, 0, 0, 0])
        }
        Event::MarkPrice(e) => {
            (Head { market: e.market.get(), ..Head::of(tag) }, [e.price.ticks() as u64, 0, 0, 0, 0, 0])
        }
        Event::Liquidation(e) => (
            Head { market: e.market.get(), ..Head::of(tag) },
            [e.position.lots() as u64, u64::from(e.account.get()), 0, 0, 0, 0],
        ),
        Event::InsuranceAbsorb(e) => (
            Head { market: e.market.get(), ..Head::of(tag) },
            [e.position.lots() as u64, e.cost_basis.micros() as u64, e.collateral.micros() as u64, 0, 0, 0],
        ),
        Event::InsuranceShortfall(e) => (Head::of(tag), [e.uncovered.micros() as u64, 0, 0, 0, 0, 0]),
        Event::LeverageSet(e) => (
            Head { market: e.market.get(), x: e.leverage, ..Head::of(tag) },
            [u64::from(e.account.get()), 0, 0, 0, 0, 0],
        ),
        Event::MarketParamsSet(p) => with_six_fields(market_params_fields(tag, &p)),
        Event::RiskTierSet(r) => with_six_fields(risk_tier_fields(tag, &r)),
    };
    let [f1, f2, f3, f4, f5, f6] = fields;
    [head.pack(), f1, f2, f3, f4, f5, f6]
}

/// The echo events reuse the commands' four fields; the events' fields 5 and 6 are zero.
fn with_six_fields((head, [f1, f2, f3, f4]): (Head, [u64; 4])) -> (Head, [u64; 6]) {
    (head, [f1, f2, f3, f4, 0, 0])
}

/// Decodes EVT56 (4.4). Accepts exactly what [`encode_event`] produces.
pub fn decode_event(words: &[u64; EVENT_WORDS]) -> Result<Event, DecodeError> {
    let head = Head::unpack(words[0]);
    let [_, f1, f2, f3, f4, f5, f6] = *words;
    let market = MarketId::new(head.market);
    let event = match head.tag {
        event_tags::ACK => Event::Ack(Ack { order_id: OrderId::new(f1) }),
        event_tags::REJECT => Event::Reject(Reject {
            order_id: OrderId::new(f1),
            account: AccountId::new(f2 as u32),
            reason: reject_reason_from_code(head.a)
                .ok_or(DecodeError::BadCode { field: "reject reason", code: head.a })?,
        }),
        event_tags::FILL => Event::Fill(Fill {
            maker_order: OrderId::new(f1),
            taker_order: OrderId::new(f2),
            price: Price::new(f3 as i64),
            qty: Qty::new(f4 as i64),
            maker_fee: Micros::new(f5 as i64),
            taker_fee: Micros::new(f6 as i64),
            market,
            taker_side: side_from_code(head.a)?,
        }),
        event_tags::CANCELLED => Event::Cancelled(Cancelled {
            order_id: OrderId::new(f1),
            remaining: Qty::new(f2 as i64),
            market,
            reason: cancel_reason_from_code(head.a)
                .ok_or(DecodeError::BadCode { field: "cancel reason", code: head.a })?,
            side: side_from_code(head.b)?,
        }),
        event_tags::MODIFIED => Event::Modified(Modified {
            order_id: OrderId::new(f1),
            price: Price::new(f2 as i64),
            qty: Qty::new(f3 as i64),
            market,
        }),
        event_tags::POSITION_CHANGED => Event::PositionChanged(PositionChanged {
            position: Qty::new(f1 as i64),
            cost_basis: Micros::new(f2 as i64),
            locked: Micros::new(f3 as i64),
            account: AccountId::new(f4 as u32),
            market,
        }),
        event_tags::BALANCE_CHANGED => Event::BalanceChanged(BalanceChanged {
            free: Micros::new(f1 as i64),
            account: AccountId::new(f2 as u32),
        }),
        event_tags::MARK_PRICE => Event::MarkPrice(MarkPrice { price: Price::new(f1 as i64), market }),
        event_tags::LIQUIDATION => Event::Liquidation(Liquidation {
            position: Qty::new(f1 as i64),
            account: AccountId::new(f2 as u32),
            market,
        }),
        event_tags::INSURANCE_ABSORB => Event::InsuranceAbsorb(InsuranceAbsorb {
            position: Qty::new(f1 as i64),
            cost_basis: Micros::new(f2 as i64),
            collateral: Micros::new(f3 as i64),
            market,
        }),
        event_tags::INSURANCE_SHORTFALL => {
            Event::InsuranceShortfall(InsuranceShortfall { uncovered: Micros::new(f1 as i64) })
        }
        event_tags::LEVERAGE_SET => {
            Event::LeverageSet(LeverageSet { account: AccountId::new(f1 as u32), market, leverage: head.x })
        }
        event_tags::MARKET_PARAMS_SET => Event::MarketParamsSet(decode_market_params(head, [f1, f2, f3, f4])),
        event_tags::RISK_TIER_SET => Event::RiskTierSet(decode_risk_tier(head, f1)),
        tag => return Err(DecodeError::UnknownTag(tag)),
    };
    // The comparison that checks every reserved byte (module docs).
    if encode_event(&event) != *words {
        return Err(DecodeError::ReservedNotZero);
    }
    Ok(event)
}

// ---------------------------------------------------------------------------------------
// Enum codes (4.1): the engine's declaration order, written out.

pub fn side_code(side: Side) -> u8 {
    match side {
        Side::Buy => 0,
        Side::Sell => 1,
    }
}

fn side_from_code(code: u8) -> Result<Side, DecodeError> {
    match code {
        0 => Ok(Side::Buy),
        1 => Ok(Side::Sell),
        _ => Err(DecodeError::BadCode { field: "side", code }),
    }
}

pub fn tif_code(tif: TimeInForce) -> u8 {
    match tif {
        TimeInForce::Gtc => 0,
        TimeInForce::Ioc => 1,
    }
}

fn tif_from_code(code: u8) -> Result<TimeInForce, DecodeError> {
    match code {
        0 => Ok(TimeInForce::Gtc),
        1 => Ok(TimeInForce::Ioc),
        _ => Err(DecodeError::BadCode { field: "time in force", code }),
    }
}

/// `post_only`: 0 or 1, nothing else.
fn flag_from_code(code: u8) -> Result<bool, DecodeError> {
    match code {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(DecodeError::BadCode { field: "post_only", code }),
    }
}

pub fn cancel_reason_code(reason: CancelReason) -> u8 {
    match reason {
        CancelReason::UserRequested => 0,
        CancelReason::IocRemainder => 1,
        CancelReason::SelfTrade => 2,
        CancelReason::Liquidation => 3,
        CancelReason::SizeBelowFilled => 4,
        CancelReason::PriceBand => 5,
    }
}

pub fn cancel_reason_from_code(code: u8) -> Option<CancelReason> {
    Some(match code {
        0 => CancelReason::UserRequested,
        1 => CancelReason::IocRemainder,
        2 => CancelReason::SelfTrade,
        3 => CancelReason::Liquidation,
        4 => CancelReason::SizeBelowFilled,
        5 => CancelReason::PriceBand,
        _ => return None,
    })
}

pub fn reject_reason_code(reason: RejectReason) -> u8 {
    match reason {
        RejectReason::InvalidPrice => 0,
        RejectReason::InvalidQty => 1,
        RejectReason::Duplicate => 2,
        RejectReason::UnknownOrder => 3,
        RejectReason::PostOnlyWouldCross => 4,
        RejectReason::UnknownMarket => 5,
        RejectReason::NoMark => 6,
        RejectReason::PriceBand => 7,
        RejectReason::MarginCall => 8,
        RejectReason::InsufficientMargin => 9,
        RejectReason::InsufficientBalance => 10,
        RejectReason::MarketNotEmpty => 11,
        RejectReason::InvalidLeverage => 12,
        RejectReason::ReservedAccount => 13,
        RejectReason::SizeLimit => 14,
        RejectReason::InvalidAmount => 15,
        RejectReason::InvalidParams => 16,
        RejectReason::WithdrawalReserve => 17,
        RejectReason::NoRiskTiers => 18,
    }
}

pub fn reject_reason_from_code(code: u8) -> Option<RejectReason> {
    Some(match code {
        0 => RejectReason::InvalidPrice,
        1 => RejectReason::InvalidQty,
        2 => RejectReason::Duplicate,
        3 => RejectReason::UnknownOrder,
        4 => RejectReason::PostOnlyWouldCross,
        5 => RejectReason::UnknownMarket,
        6 => RejectReason::NoMark,
        7 => RejectReason::PriceBand,
        8 => RejectReason::MarginCall,
        9 => RejectReason::InsufficientMargin,
        10 => RejectReason::InsufficientBalance,
        11 => RejectReason::MarketNotEmpty,
        12 => RejectReason::InvalidLeverage,
        13 => RejectReason::ReservedAccount,
        14 => RejectReason::SizeLimit,
        15 => RejectReason::InvalidAmount,
        16 => RejectReason::InvalidParams,
        17 => RejectReason::WithdrawalReserve,
        18 => RejectReason::NoRiskTiers,
        _ => return None,
    })
}

// ---------------------------------------------------------------------------------------
// Words and bytes.

/// The little-endian bytes of `W` words, word after word: word `k` is bytes `8k..8k+8`
/// (PIPELINE.md 3.3). `B` must be `8 * W` (checked at compile time).
pub fn to_le_bytes<const W: usize, const B: usize>(words: &[u64; W]) -> [u8; B] {
    const { assert!(B == 8 * W, "B must be 8 * W") };
    let mut bytes = [0; B];
    let (chunks, _) = bytes.as_chunks_mut::<8>();
    for (chunk, word) in chunks.iter_mut().zip(words) {
        *chunk = word.to_le_bytes();
    }
    bytes
}

/// The words whose little-endian bytes are `bytes`; the inverse of [`to_le_bytes`].
pub fn from_le_bytes<const B: usize, const W: usize>(bytes: &[u8; B]) -> [u64; W] {
    const { assert!(B == 8 * W, "B must be 8 * W") };
    let mut words = [0; W];
    let (chunks, _) = bytes.as_chunks::<8>();
    for (word, chunk) in words.iter_mut().zip(chunks) {
        *word = u64::from_le_bytes(*chunk);
    }
    words
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine::types::{OrderSeq, order_id};

    /// A tiny deterministic generator (xorshift64), so the randomized tests need no
    /// dependency and always run the same inputs.
    struct XorShift(u64);

    impl XorShift {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }

        /// A value of every magnitude: random bits, shifted right by a random amount, and
        /// sometimes one of the extremes.
        fn any_u64(&mut self) -> u64 {
            match self.below(8) {
                0 => [0, 1, u64::MAX, i64::MAX as u64, i64::MIN as u64][self.below(5) as usize],
                _ => self.next() >> self.below(64),
            }
        }
    }

    const SIDES: [Side; 2] = [Side::Buy, Side::Sell];
    const TIFS: [TimeInForce; 2] = [TimeInForce::Gtc, TimeInForce::Ioc];
    const CANCEL_REASONS: [CancelReason; 6] = [
        CancelReason::UserRequested,
        CancelReason::IocRemainder,
        CancelReason::SelfTrade,
        CancelReason::Liquidation,
        CancelReason::SizeBelowFilled,
        CancelReason::PriceBand,
    ];
    const REJECT_REASONS: [RejectReason; 19] = [
        RejectReason::InvalidPrice,
        RejectReason::InvalidQty,
        RejectReason::Duplicate,
        RejectReason::UnknownOrder,
        RejectReason::PostOnlyWouldCross,
        RejectReason::UnknownMarket,
        RejectReason::NoMark,
        RejectReason::PriceBand,
        RejectReason::MarginCall,
        RejectReason::InsufficientMargin,
        RejectReason::InsufficientBalance,
        RejectReason::MarketNotEmpty,
        RejectReason::InvalidLeverage,
        RejectReason::ReservedAccount,
        RejectReason::SizeLimit,
        RejectReason::InvalidAmount,
        RejectReason::InvalidParams,
        RejectReason::WithdrawalReserve,
        RejectReason::NoRiskTiers,
    ];

    /// One of every command, with every 8-byte field set to `v` (as ticks, lots or micros
    /// where the field has a unit), every `u32` to `a`, every market to `m`, every small `u16`
    /// to `x` and every `u8` to `u`; `k` picks the enums.
    fn every_command(v: i64, a: u32, m: u16, x: u16, u: u8, k: usize) -> [Command; 9] {
        let (price, qty, amount) = (Price::new(v), Qty::new(v), Micros::new(v));
        let (id, account, market) = (OrderId::new(v as u64), AccountId::new(a), MarketId::new(m));
        [
            Command::PlaceOrder(PlaceOrder {
                order_id: id,
                price,
                qty,
                market,
                side: SIDES[k % 2],
                tif: TIFS[k / 2 % 2],
                post_only: k / 4 % 2 == 1,
            }),
            Command::CancelOrder(CancelOrder { order_id: id, market }),
            Command::ModifyOrder(ModifyOrder { order_id: id, new_price: price, new_size: qty, market }),
            Command::Deposit(Deposit { amount, account }),
            Command::Withdraw(Withdraw { amount, account }),
            Command::SetLeverage(SetLeverage { account, market, leverage: x }),
            Command::SetMark(SetMark { price, market }),
            Command::SetMarketParams(market_params(v, a, m, x)),
            Command::SetRiskTier(SetRiskTier {
                lower_bound: amount,
                market,
                max_leverage: x,
                index: u,
                count: u,
            }),
        ]
    }

    fn market_params(v: i64, a: u32, m: u16, x: u16) -> SetMarketParams {
        SetMarketParams {
            min_price: Price::new(v),
            max_price: Price::new(v),
            maker_fee_ppm: a as i32,
            taker_fee_ppm: !a as i32,
            price_band_ppm: a,
            market: MarketId::new(m),
            max_leverage: x,
        }
    }

    /// One of every event, filled like [`every_command`].
    fn every_event(v: i64, a: u32, m: u16, x: u16, u: u8, k: usize) -> [Event; 14] {
        let (price, qty, amount) = (Price::new(v), Qty::new(v), Micros::new(v));
        let (id, account, market) = (OrderId::new(v as u64), AccountId::new(a), MarketId::new(m));
        [
            Event::Ack(Ack { order_id: id }),
            Event::Reject(Reject { order_id: id, account, reason: REJECT_REASONS[k % 19] }),
            Event::Fill(Fill {
                maker_order: id,
                taker_order: OrderId::new(!v as u64),
                price,
                qty,
                maker_fee: amount,
                taker_fee: amount,
                market,
                taker_side: SIDES[k % 2],
            }),
            Event::Cancelled(Cancelled {
                order_id: id,
                remaining: qty,
                market,
                reason: CANCEL_REASONS[k % 6],
                side: SIDES[k / 6 % 2],
            }),
            Event::Modified(Modified { order_id: id, price, qty, market }),
            Event::PositionChanged(PositionChanged {
                position: qty,
                cost_basis: amount,
                locked: amount,
                account,
                market,
            }),
            Event::BalanceChanged(BalanceChanged { free: amount, account }),
            Event::MarkPrice(MarkPrice { price, market }),
            Event::Liquidation(Liquidation { position: qty, account, market }),
            Event::InsuranceAbsorb(InsuranceAbsorb {
                position: qty,
                cost_basis: amount,
                collateral: amount,
                market,
            }),
            Event::InsuranceShortfall(InsuranceShortfall { uncovered: amount }),
            Event::LeverageSet(LeverageSet { account, market, leverage: x }),
            Event::MarketParamsSet(market_params(v, a, m, x)),
            Event::RiskTierSet(SetRiskTier {
                lower_bound: amount,
                market,
                max_leverage: x,
                index: u,
                count: u,
            }),
        ]
    }

    /// Zero, typical and extreme values for every kind of field.
    const VALUES: [(i64, u32, u16, u16, u8); 6] = [
        (0, 0, 0, 0, 0),
        (102_998, 9, 3, 20, 2),
        (-1, u32::MAX, u16::MAX, u16::MAX, u8::MAX),
        (i64::MIN, 1 << 31, 1 << 15, 1, 7),
        (i64::MAX, u32::MAX - 1, u16::MAX - 1, u16::MAX - 1, 8),
        (1, 1, 1, 1, 1),
    ];

    #[test]
    fn every_command_round_trips_at_zero_typical_and_extreme_values() {
        for (v, a, m, x, u) in VALUES {
            for k in 0..8 {
                for command in every_command(v, a, m, x, u, k) {
                    let words = encode_command(&command);
                    assert_eq!(decode_command(&words), Ok(command), "{command:?}");
                    let bytes: [u8; COMMAND_BYTES] = to_le_bytes(&words);
                    assert_eq!(from_le_bytes(&bytes), words, "{command:?}");
                }
            }
        }
    }

    #[test]
    fn every_event_round_trips_at_zero_typical_and_extreme_values() {
        for (v, a, m, x, u) in VALUES {
            for k in 0..19 {
                for event in every_event(v, a, m, x, u, k) {
                    let words = encode_event(&event);
                    assert_eq!(decode_event(&words), Ok(event), "{event:?}");
                    let bytes: [u8; EVENT_BYTES] = to_le_bytes(&words);
                    assert_eq!(from_le_bytes(&bytes), words, "{event:?}");
                }
            }
        }
    }

    #[test]
    fn tags_follow_the_tables_and_the_engines_variant_order() {
        for (i, command) in every_command(1, 1, 1, 1, 1, 0).iter().enumerate() {
            assert_eq!(command_tag(command), i as u8 + 1, "{command:?}");
            assert_eq!(encode_command(command)[0] as u8, i as u8 + 1, "{command:?}");
        }
        for (i, event) in every_event(1, 1, 1, 1, 1, 0).iter().enumerate() {
            assert_eq!(event_tag(event), i as u8 + 1, "{event:?}");
            assert_eq!(encode_event(event)[0] as u8, i as u8 + 1, "{event:?}");
        }
    }

    #[test]
    fn every_enum_code_is_pinned_to_the_spec_and_to_the_engines_declaration_order() {
        // PIPELINE.md 4.1, written out; the second assertion of each pair fails if the
        // engine reorders an enum.
        let sides = [(Side::Buy, 0), (Side::Sell, 1)];
        for (side, code) in sides {
            assert_eq!(side_code(side), code);
            assert_eq!(side as u8, code, "{side:?}");
            assert_eq!(side_from_code(code), Ok(side));
        }
        for (tif, code) in [(TimeInForce::Gtc, 0), (TimeInForce::Ioc, 1)] {
            assert_eq!(tif_code(tif), code);
            assert_eq!(tif as u8, code, "{tif:?}");
            assert_eq!(tif_from_code(code), Ok(tif));
        }
        for (code, reason) in CANCEL_REASONS.into_iter().enumerate() {
            assert_eq!(cancel_reason_code(reason), code as u8);
            assert_eq!(reason as u8, code as u8, "{reason:?}");
            assert_eq!(cancel_reason_from_code(code as u8), Some(reason));
        }
        for (code, reason) in REJECT_REASONS.into_iter().enumerate() {
            assert_eq!(reject_reason_code(reason), code as u8);
            assert_eq!(reason as u8, code as u8, "{reason:?}");
            assert_eq!(reject_reason_from_code(code as u8), Some(reason));
        }
        // The spec's numbers, spot-checked by name so the list above can't drift.
        assert_eq!(cancel_reason_code(CancelReason::PriceBand), 5);
        assert_eq!(reject_reason_code(RejectReason::InsufficientMargin), 9);
        assert_eq!(reject_reason_code(RejectReason::NoRiskTiers), 18);
    }

    #[test]
    fn codes_out_of_range_are_rejected() {
        let place = |a: u8, b: u8, c: u8| {
            let mut words = encode_command(&every_command(5, 5, 5, 5, 5, 0)[0]);
            words[0] = Head { a, b, c, market: 5, ..Head::of(command_tags::PLACE_ORDER) }.pack();
            decode_command(&words)
        };
        assert_eq!(place(0, 0, 0).map(|c| command_tag(&c)), Ok(1));
        assert_eq!(place(2, 0, 0), Err(DecodeError::BadCode { field: "side", code: 2 }));
        assert_eq!(place(0, 2, 0), Err(DecodeError::BadCode { field: "time in force", code: 2 }));
        assert_eq!(place(0, 0, 2), Err(DecodeError::BadCode { field: "post_only", code: 2 }));

        let with_head = |tag: u8, a: u8, b: u8| {
            let mut words = [0; EVENT_WORDS];
            words[0] = Head { a, b, ..Head::of(tag) }.pack();
            decode_event(&words)
        };
        assert!(with_head(event_tags::REJECT, 18, 0).is_ok());
        assert_eq!(
            with_head(event_tags::REJECT, 19, 0),
            Err(DecodeError::BadCode { field: "reject reason", code: 19 })
        );
        assert!(with_head(event_tags::CANCELLED, 5, 1).is_ok());
        assert_eq!(
            with_head(event_tags::CANCELLED, 6, 0),
            Err(DecodeError::BadCode { field: "cancel reason", code: 6 })
        );
        assert_eq!(
            with_head(event_tags::CANCELLED, 0, 2),
            Err(DecodeError::BadCode { field: "side", code: 2 })
        );
        assert_eq!(with_head(event_tags::FILL, 2, 0), Err(DecodeError::BadCode { field: "side", code: 2 }));
    }

    #[test]
    fn tag_zero_and_tags_past_the_last_are_rejected() {
        assert_eq!(decode_command(&[0; COMMAND_WORDS]), Err(DecodeError::UnknownTag(0)));
        assert_eq!(decode_event(&[0; EVENT_WORDS]), Err(DecodeError::UnknownTag(0)));
        for tag in 10..=255u8 {
            assert_eq!(decode_command(&[u64::from(tag), 0, 0, 0, 0]), Err(DecodeError::UnknownTag(tag)));
        }
        for tag in 15..=255u8 {
            let words = [u64::from(tag), 0, 0, 0, 0, 0, 0];
            assert_eq!(decode_event(&words), Err(DecodeError::UnknownTag(tag)));
        }
    }

    /// The byte layout of each tag, transcribed from the tables of PIPELINE.md 4.2 and 4.3
    /// independently of the code: `T` the tag, `e` an enum or flag byte, `n` a byte of a
    /// number (any value decodes), `-` a reserved byte (must be zero).
    const COMMAND_LAYOUTS: [&str; 9] = [
        "Teeenn-- nnnnnnnn nnnnnnnn nnnnnnnn --------", // 1 PlaceOrder
        "T---nn-- nnnnnnnn -------- -------- --------", // 2 CancelOrder
        "T---nn-- nnnnnnnn nnnnnnnn nnnnnnnn --------", // 3 ModifyOrder
        "T------- nnnnnnnn nnnn---- -------- --------", // 4 Deposit
        "T------- nnnnnnnn nnnn---- -------- --------", // 5 Withdraw
        "T---nnnn nnnn---- -------- -------- --------", // 6 SetLeverage
        "T---nn-- nnnnnnnn -------- -------- --------", // 7 SetMark
        "T---nnnn nnnnnnnn nnnnnnnn nnnnnnnn nnnn----", // 8 SetMarketParams
        "Tnn-nnnn nnnnnnnn -------- -------- --------", // 9 SetRiskTier
    ];

    const EVENT_LAYOUTS: [&str; 14] = [
        "T------- nnnnnnnn -------- -------- -------- -------- --------", // 1 Ack
        "Te------ nnnnnnnn nnnn---- -------- -------- -------- --------", // 2 Reject
        "Te--nn-- nnnnnnnn nnnnnnnn nnnnnnnn nnnnnnnn nnnnnnnn nnnnnnnn", // 3 Fill
        "Tee-nn-- nnnnnnnn nnnnnnnn -------- -------- -------- --------", // 4 Cancelled
        "T---nn-- nnnnnnnn nnnnnnnn nnnnnnnn -------- -------- --------", // 5 Modified
        "T---nn-- nnnnnnnn nnnnnnnn nnnnnnnn nnnn---- -------- --------", // 6 PositionChanged
        "T------- nnnnnnnn nnnn---- -------- -------- -------- --------", // 7 BalanceChanged
        "T---nn-- nnnnnnnn -------- -------- -------- -------- --------", // 8 MarkPrice
        "T---nn-- nnnnnnnn nnnn---- -------- -------- -------- --------", // 9 Liquidation
        "T---nn-- nnnnnnnn nnnnnnnn nnnnnnnn -------- -------- --------", // 10 InsuranceAbsorb
        "T------- nnnnnnnn -------- -------- -------- -------- --------", // 11 InsuranceShortfall
        "T---nnnn nnnn---- -------- -------- -------- -------- --------", // 12 LeverageSet
        "T---nnnn nnnnnnnn nnnnnnnn nnnnnnnn nnnn---- -------- --------", // 13 MarketParamsSet
        "Tnn-nnnn nnnnnnnn -------- -------- -------- -------- --------", // 14 RiskTierSet
    ];

    fn layout(text: &str) -> Vec<u8> {
        text.bytes().filter(|&b| b != b' ').collect()
    }

    /// Sets each byte in turn: a reserved byte set to 1 must be refused; a number byte set
    /// to anything must decode and re-encode to the same bytes (canonical form).
    fn check_layout<const W: usize, const B: usize>(
        base: [u64; W],
        layout: &[u8],
        decode_then_encode: impl Fn(&[u64; W]) -> Result<[u64; W], DecodeError>,
        rng: &mut XorShift,
    ) {
        assert_eq!(layout.len(), B);
        let base: [u8; B] = to_le_bytes(&base);
        for (i, &kind) in layout.iter().enumerate() {
            let mut bytes = base;
            match kind {
                b'-' => {
                    bytes[i] = 1;
                    assert_eq!(
                        decode_then_encode(&from_le_bytes(&bytes)),
                        Err(DecodeError::ReservedNotZero),
                        "byte {i} is reserved"
                    );
                }
                b'n' => {
                    for _ in 0..8 {
                        bytes[i] = rng.next() as u8;
                        let words = from_le_bytes(&bytes);
                        assert_eq!(decode_then_encode(&words), Ok(words), "byte {i} is part of a number");
                    }
                }
                _ => {}
            }
        }
    }

    #[test]
    fn each_reserved_byte_set_to_one_is_rejected_and_number_bytes_take_any_value() {
        let mut rng = XorShift(0x2545_F491_4F6C_DD1D);
        let encode_decode = |w: &[u64; COMMAND_WORDS]| decode_command(w).map(|c| encode_command(&c));
        for (command, text) in every_command(0, 0, 0, 0, 0, 0).iter().zip(COMMAND_LAYOUTS) {
            check_layout::<COMMAND_WORDS, COMMAND_BYTES>(
                encode_command(command),
                &layout(text),
                encode_decode,
                &mut rng,
            );
        }
        let encode_decode = |w: &[u64; EVENT_WORDS]| decode_event(w).map(|e| encode_event(&e));
        for (event, text) in every_event(0, 0, 0, 0, 0, 0).iter().zip(EVENT_LAYOUTS) {
            check_layout::<EVENT_WORDS, EVENT_BYTES>(
                encode_event(event),
                &layout(text),
                encode_decode,
                &mut rng,
            );
        }
    }

    /// Random bytes that follow a tag's layout with some probability, and are fully random
    /// otherwise, so that many decode and many don't.
    fn random_input<const W: usize, const B: usize>(layouts: &[&str], rng: &mut XorShift) -> [u64; W] {
        let mut bytes = [0u8; B];
        for byte in &mut bytes {
            *byte = rng.next() as u8;
        }
        if rng.below(4) != 0 {
            let tag = rng.below(layouts.len() as u64) as usize;
            for (byte, kind) in bytes.iter_mut().zip(layout(layouts[tag])) {
                match kind {
                    b'T' => *byte = tag as u8 + 1,
                    b'e' => *byte = rng.below(3) as u8, // mostly in range
                    b'-' if rng.below(64) != 0 => *byte = 0,
                    _ => {}
                }
            }
        }
        from_le_bytes(&bytes)
    }

    #[test]
    fn random_bytes_either_fail_to_decode_or_re_encode_to_themselves() {
        let mut rng = XorShift(0x9E37_79B9_7F4A_7C15);
        let (mut decoded_commands, mut decoded_events) = (0, 0);
        for _ in 0..100_000 {
            let words = random_input::<COMMAND_WORDS, COMMAND_BYTES>(&COMMAND_LAYOUTS, &mut rng);
            if let Ok(command) = decode_command(&words) {
                assert_eq!(encode_command(&command), words);
                decoded_commands += 1;
            }
            let words = random_input::<EVENT_WORDS, EVENT_BYTES>(&EVENT_LAYOUTS, &mut rng);
            if let Ok(event) = decode_event(&words) {
                assert_eq!(encode_event(&event), words);
                decoded_events += 1;
            }
        }
        // The inputs are built so that a good share decode; this checks they did.
        assert!(decoded_commands > 20_000, "{decoded_commands}");
        assert!(decoded_events > 20_000, "{decoded_events}");
    }

    #[test]
    fn random_commands_and_events_round_trip() {
        let mut rng = XorShift(0x1234_5678_9ABC_DEF1);
        for _ in 0..20_000 {
            let (v, a) = (rng.any_u64() as i64, rng.any_u64() as u32);
            let (m, x, u) = (rng.any_u64() as u16, rng.any_u64() as u16, rng.any_u64() as u8);
            let k = rng.below(24) as usize;
            for command in every_command(v, a, m, x, u, k) {
                assert_eq!(decode_command(&encode_command(&command)), Ok(command));
            }
            for event in every_event(v, a, m, x, u, k) {
                assert_eq!(decode_event(&encode_event(&event)), Ok(event));
            }
        }
    }

    #[test]
    fn the_worked_example_encodes_to_the_specs_bytes() {
        // PIPELINE.md 5.6: account 9's post-only GTC bid on market 3, bytes 32..72 of the
        // signed message.
        let command = Command::PlaceOrder(PlaceOrder {
            order_id: order_id(AccountId::new(9), OrderSeq::new(1)),
            price: Price::new(102_998),
            qty: Qty::new(500_000),
            market: MarketId::new(3),
            side: Side::Buy,
            tif: TimeInForce::Gtc,
            post_only: true,
        });
        let expected: [u8; COMMAND_BYTES] = [
            0x01, 0x00, 0x00, 0x01, 0x03, 0x00, 0x00, 0x00, // tag 1, Buy, GTC, post_only, market 3
            0x01, 0x00, 0x00, 0x00, 0x09, 0x00, 0x00, 0x00, // order_id
            0x56, 0x92, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, // price 102,998
            0x20, 0xa1, 0x07, 0x00, 0x00, 0x00, 0x00, 0x00, // qty 500,000
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // f4 = 0
        ];
        assert_eq!(order_id(AccountId::new(9), OrderSeq::new(1)), OrderId::new(38_654_705_665));
        assert_eq!(to_le_bytes::<COMMAND_WORDS, COMMAND_BYTES>(&encode_command(&command)), expected);
        assert_eq!(decode_command(&from_le_bytes(&expected)), Ok(command));
    }

    #[test]
    fn fees_keep_their_sign_in_their_half_of_the_field() {
        for (maker, taker) in [(-250, 700), (i32::MIN, i32::MAX), (0, -1), (-1, 0)] {
            assert_eq!(unpack_fees(pack_fees(maker, taker)), (maker, taker));
        }
        // Maker in bytes 24..28 (the low half of f3), taker in 28..32.
        assert_eq!(pack_fees(-1, 0), 0x0000_0000_FFFF_FFFF);
        assert_eq!(pack_fees(0, 2), 0x0000_0002_0000_0000);
    }

    #[test]
    fn decode_errors_explain_themselves() {
        assert_eq!(DecodeError::UnknownTag(0).to_string(), "unknown tag 0");
        assert_eq!(
            DecodeError::BadCode { field: "side", code: 2 }.to_string(),
            "side code 2 is out of range"
        );
        assert_eq!(DecodeError::ReservedNotZero.to_string(), "a reserved byte is not zero");
    }
}
