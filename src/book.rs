// SPDX-FileCopyrightText: Copyright Logically Magic Inc. <hello@logicallymagic.com>
// SPDX-FileCopyrightText: Copyright Hamid Jahanzad <Hamid@Jahanzad.com>
// SPDX-FileCopyrightText: Copyright Contributors to the Exchange8:MD
// SPDX-License-Identifier: Apache-2.0

//! Ex8:MD consolidated depth book + ITCH VER2 event mapper.
//!
//! Mirrors ME order lifecycle events into a per-(book, instrument)
//! consolidated depth book (the platform spec) and
//! maps each delta to the proprietary ITCH VER2 wire messages.
//!
//! Rules preserved from the feed handler:
//! - **Hidden orders never print an Add** — hidden liquidity surfaces only
//!   as `P` Trade / `Q` Cross prints.
//! - **Odd-lot volume is stripped from displayed depth** (board part only,
//! VER2 display policy).
//! - **Replace = kill(old) + add(new)** internally, emitting `U` on the wire.
//! - Partial reduces emit `X` Order Cancel with the *remaining* shares.
//!
//! Design: `design/sentin8-exchange8-market-data-technical-design.md` §4.1–4.2.

use std::collections::BTreeMap;

use exchange8::contracts::enums::{InstrumentStatus, LotType, Side};
use exchange8::fixed::{Price, Qty};

/// A booked (resting) entry in the consolidated mirror.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BookedEntry {
    pub price: Price,
    pub qty: Qty,
    pub side: Side,
    /// True when the ME booked the order hidden (never displayed).
    pub hidden: bool,
    /// True when the resting portion is odd-lot (stripped from depth).
    pub odd: bool,
}

/// One aggregated level (price + total displayed volume).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Level {
    pub price: Price,
    pub qty: Qty,
}

/// ITCH VER2 message produced by the mapper (encoded bytes + type char).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ItchOut {
    /// Message type char (`A`/`E`/`U`/`D`/`X`/`P`/`Q`/`B`/…).
    pub msg_type: u8,
    /// Encoded VER2 payload (including the type byte).
    pub bytes: Vec<u8>,
}

/// Per-(book, instrument) consolidated book + event→ITCH mapper.
///
/// Feeding an event returns the wire messages for that delta (possibly none —
/// e.g. hidden orders produce no Add).
pub struct ConsolidatedBook {
    pub book_id: u8,
    /// instrument id (kept for journal replay and diagnostics).
    #[allow(dead_code)]
    pub instrument_id: u32,
    /// ITCH instrument ids are u16 on the wire (field width).
    pub itch_instrument_id: u16,
    /// order_id → entry.
    orders: std::collections::HashMap<u64, BookedEntry>,
    /// Aggregated *displayed* volume per price per side (odd/hidden stripped).
    bids: BTreeMap<i128, u64>,
    asks: BTreeMap<i128, u64>,
    /// Broker suppression: anonymous orders publish the default broker id.
    #[allow(dead_code)]
    pub default_broker: u16,
}

impl ConsolidatedBook {
    pub fn new(book_id: u8, instrument_id: u32, itch_instrument_id: u16) -> Self {
        ConsolidatedBook {
            book_id,
            instrument_id,
            itch_instrument_id,
            orders: std::collections::HashMap::new(),
            bids: BTreeMap::new(),
            asks: BTreeMap::new(),
            default_broker: 0,
        }
    }

    /// ME `OrderBooked` → `A` Add Order (skipped when hidden or odd-stripped).
    pub fn on_booked(
        &mut self,
        order_id: u64,
        price: Price,
        qty: Qty,
        side: Side,
        lot_type: LotType,
        hidden: bool,
        broker: u16,
    ) -> Vec<ItchOut> {
        let odd = lot_type == LotType::Odd;
        self.orders.insert(
            order_id,
            BookedEntry {
                price,
                qty,
                side,
                hidden,
                odd,
            },
        );
        if hidden || odd {
            // Hidden liquidity and odd portions never add displayed depth.
            return Vec::new();
        }
        self.level_map(side)
            .entry(price.raw)
            .and_modify(|v| *v += qty.raw)
            .or_insert(qty.raw);
        vec![ItchOut {
            msg_type: exchange8::itch::msg_type::ADD_ORDER,
            bytes: exchange8::itch::AddOrder {
                side: side_char(side),
                instrument_id: self.itch_instrument_id,
                order_id: order_id as u32,
                shares: qty.raw as u32,
                price: price.raw as u32,
                broker,
                book_id: self.book_id,
            }
            .encode(),
        }]
    }

    /// ME `OrderExecuted` (visible) → `E` Order Executed at the resting price.
    pub fn on_executed(
        &mut self,
        order_id: u64,
        shares: u32,
        match_number: u32,
        contra_broker: u16,
        lot_type: LotType,
    ) -> Vec<ItchOut> {
        let mut out = Vec::new();
        // Snapshot entry fields first to release the borrow before the
        // level-map mutation below.
        let snap = self.orders.get_mut(&order_id).map(|entry| {
            let was_displayed = !entry.hidden && !entry.odd;
            let remaining = entry.qty.raw.saturating_sub(shares as u64);
            entry.qty = Qty::units(remaining);
            (was_displayed, remaining, entry.side, entry.price)
        });
        if let Some((was_displayed, remaining, side, price)) = snap {
            if was_displayed {
                let m = self.level_map(side);
                if let Some(v) = m.get_mut(&price.raw) {
                    *v = v.saturating_sub(shares as u64);
                    if *v == 0 {
                        m.remove(&price.raw);
                    }
                }
                // `E` only for displayed board executions; odd/hidden → trade print.
                if lot_type == LotType::Board {
                    out.push(ItchOut {
                        msg_type: exchange8::itch::msg_type::ORDER_EXECUTED,
                        bytes: exchange8::itch::OrderExecuted {
                            marker: exchange8::itch::ITCH_SPACE,
                            contra_broker,
                            order_id: order_id as u32,
                            shares,
                            match_number,
                        }
                        .encode(),
                    });
                }
            }
            if remaining == 0 {
                self.orders.remove(&order_id);
            }
        }
        out
    }

    /// Partial reduce (ME kill-with-remaining / cancel part) → `X` Order
    /// Cancel with the cancelled shares.
    pub fn on_reduce(&mut self, order_id: u64, remaining: Qty) -> Vec<ItchOut> {
        let mut out = Vec::new();
        let snap = self.orders.get_mut(&order_id).map(|entry| {
            let was_displayed = !entry.hidden && !entry.odd;
            let before = entry.qty;
            entry.qty = remaining;
            (was_displayed, before, entry.side, entry.price)
        });
        if let Some((was_displayed, before, side, price)) = snap {
            let delta = before.raw.saturating_sub(remaining.raw);
            if was_displayed {
                let m = self.level_map(side);
                if let Some(v) = m.get_mut(&price.raw) {
                    *v = v.saturating_sub(delta);
                    if *v == 0 {
                        m.remove(&price.raw);
                    }
                }
                out.push(ItchOut {
                    msg_type: exchange8::itch::msg_type::ORDER_CANCEL,
                    bytes: exchange8::itch::OrderCancel {
                        order_id: order_id as u32,
                        shares: delta as u32,
                    }
                    .encode(),
                });
            }
        }
        out
    }

    /// ME kill → `D` Order Delete.
    pub fn on_delete(&mut self, order_id: u64) -> Vec<ItchOut> {
        let mut out = Vec::new();
        if let Some(entry) = self.orders.remove(&order_id) {
            if !entry.hidden && !entry.odd {
                let m = self.level_map(entry.side);
                if let Some(v) = m.get_mut(&entry.price.raw) {
                    *v = v.saturating_sub(entry.qty.raw);
                    if *v == 0 {
                        m.remove(&entry.price.raw);
                    }
                }
                out.push(ItchOut {
                    msg_type: exchange8::itch::msg_type::ORDER_DELETE,
                    bytes: exchange8::itch::OrderDelete {
                        order_id: order_id as u32,
                    }
                    .encode(),
                });
            }
        }
        out
    }

    /// ME replace = internal kill + add → `U` Order Replaced (single wire msg).
    pub fn on_replaced(
        &mut self,
        orig_order_id: u64,
        new_order_id: u64,
        shares: u32,
        price: Price,
        side: Side,
        hidden: bool,
        odd: bool,
    ) -> Vec<ItchOut> {
        // Internal kill of the original.
        self.on_delete(orig_order_id);
        // Internal add of the new.
        self.orders.insert(
            new_order_id,
            BookedEntry {
                price,
                qty: Qty::units(shares as u64),
                side,
                hidden,
                odd,
            },
        );
        if !hidden && !odd {
            self.level_map(side)
                .entry(price.raw)
                .and_modify(|v| *v += shares as u64)
                .or_insert(shares as u64);
        }
        vec![ItchOut {
            msg_type: exchange8::itch::msg_type::ORDER_REPLACED,
            bytes: exchange8::itch::OrderReplaced {
                orig_order_id: orig_order_id as u32,
                order_id: new_order_id as u32,
                shares,
                price: price.raw as u32,
            }
            .encode(),
        }]
    }

    /// Best bid/ask with displayed sizes (L1 derivation, `W` source).
    pub fn bbo(&self) -> (Option<Level>, Option<Level>) {
        let best_bid = self.bids.iter().next_back().map(|(p, q)| Level {
            price: Price { raw: *p, scale: 4 },
            qty: Qty::units(*q),
        });
        let best_ask = self.asks.iter().next().map(|(p, q)| Level {
            price: Price { raw: *p, scale: 4 },
            qty: Qty::units(*q),
        });
        (best_bid, best_ask)
    }

    /// Full displayed depth (bids desc, asks asc).
    pub fn depth(&self) -> (Vec<Level>, Vec<Level>) {
        let bids = self
            .bids
            .iter()
            .rev()
            .map(|(p, q)| Level {
                price: Price { raw: *p, scale: 4 },
                qty: Qty::units(*q),
            })
            .collect();
        let asks = self
            .asks
            .iter()
            .map(|(p, q)| Level {
                price: Price { raw: *p, scale: 4 },
                qty: Qty::units(*q),
            })
            .collect();
        (bids, asks)
    }

    fn level_map(&mut self, side: Side) -> &mut BTreeMap<i128, u64> {
        match side {
            Side::Bid => &mut self.bids,
            Side::Ask => &mut self.asks,
        }
    }

    /// Trading action for an instrument status transition → `H` payload.
    pub fn trading_action(status: InstrumentStatus) -> u8 {
        match status {
            InstrumentStatus::Open => b' ',
            InstrumentStatus::Halted => b'H',
            InstrumentStatus::QuotationOnly => b'Q',
        }
    }
}

fn side_char(side: Side) -> u8 {
    match side {
        Side::Bid => b'B',
        Side::Ask => b'S',
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn book() -> ConsolidatedBook {
        ConsolidatedBook::new(b'M', 1, 1)
    }

    #[test]
    fn add_order_emits_itch_a() {
        let mut cb = book();
        let out = cb.on_booked(
            10,
            Price::equity(10_000),
            Qty::units(100),
            Side::Bid,
            LotType::Board,
            false,
            7,
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].msg_type, b'A');
        assert_eq!(out[0].bytes.len(), exchange8::itch::msg_len::ADD_ORDER);
        let (bid, ask) = cb.bbo();
        assert_eq!(bid.unwrap().qty.raw, 100);
        assert!(ask.is_none());
    }

    #[test]
    fn hidden_add_no_message() {
        let mut cb = book();
        let out = cb.on_booked(
            11,
            Price::equity(10_000),
            Qty::units(100),
            Side::Bid,
            LotType::Board,
            true,
            7,
        );
        assert!(out.is_empty());
        assert!(cb.bbo().0.is_none());
    }

    #[test]
    fn odd_lot_stripped_from_depth() {
        let mut cb = book();
        let out = cb.on_booked(
            12,
            Price::equity(10_000),
            Qty::units(50),
            Side::Bid,
            LotType::Odd,
            false,
            7,
        );
        assert!(out.is_empty());
        assert!(cb.bbo().0.is_none());
    }

    #[test]
    fn execution_reduces_level() {
        let mut cb = book();
        cb.on_booked(
            10,
            Price::equity(10_000),
            Qty::units(100),
            Side::Bid,
            LotType::Board,
            false,
            7,
        );
        let out = cb.on_executed(10, 60, 500, 3, LotType::Board);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].msg_type, b'E');
        let (bid, _) = cb.bbo();
        assert_eq!(bid.unwrap().qty.raw, 40);
        // Fully consumed → level gone.
        let out = cb.on_executed(10, 40, 501, 3, LotType::Board);
        assert_eq!(out.len(), 1);
        let (bid, _) = cb.bbo();
        assert!(bid.is_none());
    }

    #[test]
    fn hidden_execution_no_e_but_no_level_hit() {
        let mut cb = book();
        cb.on_booked(
            11,
            Price::equity(10_000),
            Qty::units(100),
            Side::Bid,
            LotType::Board,
            true,
            7,
        );
        let out = cb.on_executed(11, 100, 502, 3, LotType::Board);
        // Hidden order: no E message, no depth change (it never had any).
        assert!(out.is_empty());
    }

    #[test]
    fn delete_emits_d_and_clears() {
        let mut cb = book();
        cb.on_booked(
            10,
            Price::equity(10_000),
            Qty::units(100),
            Side::Bid,
            LotType::Board,
            false,
            7,
        );
        let out = cb.on_delete(10);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].msg_type, b'D');
        assert!(cb.bbo().0.is_none());
    }

    #[test]
    fn partial_reduce_emits_x() {
        let mut cb = book();
        cb.on_booked(
            10,
            Price::equity(10_000),
            Qty::units(100),
            Side::Bid,
            LotType::Board,
            false,
            7,
        );
        let out = cb.on_reduce(10, Qty::units(60));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].msg_type, b'X');
        let (bid, _) = cb.bbo();
        assert_eq!(bid.unwrap().qty.raw, 60);
    }

    #[test]
    fn replace_is_kill_plus_add_single_u() {
        let mut cb = book();
        cb.on_booked(
            10,
            Price::equity(10_000),
            Qty::units(100),
            Side::Bid,
            LotType::Board,
            false,
            7,
        );
        let out = cb.on_replaced(10, 20, 200, Price::equity(10_100), Side::Bid, false, false);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].msg_type, b'U');
        // New level has the new price and qty.
        let (bid, _) = cb.bbo();
        assert_eq!(bid.unwrap().price.raw, 10_100);
        assert_eq!(bid.unwrap().qty.raw, 200);
    }

    #[test]
    fn depth_aggregates_same_price() {
        let mut cb = book();
        cb.on_booked(
            1,
            Price::equity(10_000),
            Qty::units(100),
            Side::Bid,
            LotType::Board,
            false,
            7,
        );
        cb.on_booked(
            2,
            Price::equity(10_000),
            Qty::units(200),
            Side::Bid,
            LotType::Board,
            false,
            7,
        );
        cb.on_booked(
            3,
            Price::equity(9_900),
            Qty::units(50),
            Side::Bid,
            LotType::Board,
            false,
            7,
        );
        let (bids, _) = cb.depth();
        assert_eq!(bids.len(), 2);
        assert_eq!(bids[0].price.raw, 10_000);
        assert_eq!(bids[0].qty.raw, 300);
        assert_eq!(bids[1].price.raw, 9_900);
    }

    #[test]
    fn asks_ascending_bids_descending() {
        let mut cb = book();
        cb.on_booked(
            1,
            Price::equity(10_200),
            Qty::units(100),
            Side::Ask,
            LotType::Board,
            false,
            7,
        );
        cb.on_booked(
            2,
            Price::equity(10_100),
            Qty::units(100),
            Side::Ask,
            LotType::Board,
            false,
            7,
        );
        let (_, asks) = cb.depth();
        assert_eq!(asks[0].price.raw, 10_100);
        assert_eq!(asks[1].price.raw, 10_200);
    }

    #[test]
    fn trading_action_codes() {
        assert_eq!(
            ConsolidatedBook::trading_action(InstrumentStatus::Open),
            b' '
        );
        assert_eq!(
            ConsolidatedBook::trading_action(InstrumentStatus::Halted),
            b'H'
        );
        assert_eq!(
            ConsolidatedBook::trading_action(InstrumentStatus::QuotationOnly),
            b'Q'
        );
    }
}
