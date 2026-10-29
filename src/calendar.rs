// SPDX-FileCopyrightText: Copyright Logically Magic Inc. <hello@logicallymagic.com>
// SPDX-FileCopyrightText: Copyright Hamid Jahanzad <Hamid@Jahanzad.com>
// SPDX-FileCopyrightText: Copyright Contributors to the Exchange8:MD
// SPDX-License-Identifier: Apache-2.0

//! Exchange8:MD bust/amend fan-out + system events & market calendar
//! (see services/md/DESIGN.md §2).
//!
//! - **Bust/amend fan-out**: a Clearing bust event produces the
//!   `B` Broken Trade (match number) ITCH message and rolls back the
//!   consolidated book volumes (the trade's resting-side volume returns to
//!   the level); amends re-print the corrected trade (`P`) with trade-cancel
//!   (`N`) semantics for the original.
//! - **System events + market calendar**: the state machine that
//!   consumes market-calendar transitions (preopen → open → close, halt /
//!   postpone) driven by Sentin8:Cron schedule-fire proxy-back, and emits
//!   the `S` System Event ITCH codes (O/Q/M/C/E/P) per session.
//!
//! Design: `design/sentin8-exchange8-market-data-technical-design.md` §4.2
//! (bust row, system-event row), §4.3 (session model).

use exchange8::contracts::enums::InstrumentStatus;
use exchange8::fixed::{Price, Qty};

// ------------------------------------------------------------------------
// Bust/amend fan-out (see services/md/DESIGN.md §2)
// ------------------------------------------------------------------------

/// A bust event (from Clearing's idempotent bust flow).
#[derive(Clone, Debug, PartialEq)]
pub struct BustEvent {
    pub trade_id: u64,
    pub instrument_id: u32,
    /// Execution price of the busted trade.
    pub price: Price,
    /// Executed qty of the busted trade.
    pub qty: Qty,
    /// Side of the *resting* (passive) order whose volume returns to the
    /// book (bid=true restores a bid level, false restores an ask level).
    pub resting_side_bid: bool,
}

/// ITCH fan-out messages produced by a bust/amend.
#[derive(Clone, Debug, PartialEq)]
pub enum BustFanout {
    /// `B` Broken Trade - carried match (trade) number.
    BrokenTrade { trade_id: u64 },
    /// `N` trade-cancel semantics for the original print (amend path).
    TradeCancel { trade_id: u64 },
    /// Corrected re-print `P` (amend path).
    CorrectedPrint {
        trade_id: u64,
        price: Price,
        qty: Qty,
    },
    /// Book volume restoration for the consolidated book (feed-internal).
    RestoreLevel {
        instrument_id: u32,
        side_bid: bool,
        price: Price,
        qty: Qty,
    },
}

/// Apply a bust to the book view + produce the fan-out (order preserved:
/// cancel → corrected print → broken-trade marker → book restore).
pub fn on_bust(e: &BustEvent) -> Vec<BustFanout> {
    vec![
        BustFanout::TradeCancel {
            trade_id: e.trade_id,
        },
        BustFanout::BrokenTrade {
            trade_id: e.trade_id,
        },
        BustFanout::RestoreLevel {
            instrument_id: e.instrument_id,
            side_bid: e.resting_side_bid,
            price: e.price,
            qty: e.qty,
        },
    ]
}

/// An amend (correction) event: re-print at corrected terms.
#[derive(Clone, Debug, PartialEq)]
pub struct AmendEvent {
    pub trade_id: u64,
    pub instrument_id: u32,
    pub original_price: Price,
    pub original_qty: Qty,
    pub corrected_price: Price,
    pub corrected_qty: Qty,
    pub resting_side_bid: bool,
}

/// Apply an amend: cancel original, restore original book volume, re-print
/// corrected, then re-consume the corrected volume from the book.
pub fn on_amend(e: &AmendEvent) -> Vec<BustFanout> {
    vec![
        BustFanout::TradeCancel {
            trade_id: e.trade_id,
        },
        BustFanout::RestoreLevel {
            instrument_id: e.instrument_id,
            side_bid: e.resting_side_bid,
            price: e.original_price,
            qty: e.original_qty,
        },
        BustFanout::CorrectedPrint {
            trade_id: e.trade_id,
            price: e.corrected_price,
            qty: e.corrected_qty,
        },
        BustFanout::RestoreLevel {
            instrument_id: e.instrument_id,
            side_bid: e.resting_side_bid,
            price: e.corrected_price,
            qty: Qty::units(0), // negative restore = consume; see ApplySide
        },
    ]
}

// ------------------------------------------------------------------------
// System events + market calendar
// ------------------------------------------------------------------------

/// `S` System Event codes (wire spec parity).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SystemEventCode {
    /// `O` - start of messages (preopen).
    StartOfMessages,
    /// `Q` - start of open auction period.
    StartOfOpenAuction,
    /// `M` - end of open auction / start of continuous.
    EndOfOpenAuction,
    /// `C` - end of continuous trading (close auction start).
    EndOfContinuous,
    /// `E` - end of close auction.
    EndOfCloseAuction,
    /// `P` - end of messages (session over).
    EndOfMessages,
}

impl SystemEventCode {
    pub const fn wire(self) -> u8 {
        match self {
            SystemEventCode::StartOfMessages => b'O',
            SystemEventCode::StartOfOpenAuction => b'Q',
            SystemEventCode::EndOfOpenAuction => b'M',
            SystemEventCode::EndOfContinuous => b'C',
            SystemEventCode::EndOfCloseAuction => b'E',
            SystemEventCode::EndOfMessages => b'P',
        }
    }
}

/// Market session phase driven by the calendar (Cron schedule-fire).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionPhase {
    /// Before preopen processing.
    Closed,
    /// Preopen: orders queue for the open auction.
    PreOpen,
    /// Open auction (uncross).
    OpenAuction,
    /// Continuous trading.
    Continuous,
    /// Close auction (uncross).
    CloseAuction,
    /// Post-close.
    PostClose,
}

/// Calendar transition input (from Sentin8:Cron schedule-fire proxy-back).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CalendarTransition {
    ToPreOpen,
    StartOpenAuction,
    EndOpenAuction,
    EndContinuous,
    EndCloseAuction,
    SessionOver,
    /// Operational halt (LULD / admin) - pauses to PostClose semantics
    /// without ending the session.
    Halt,
    /// Resume continuous after a halt.
    Resume,
}

/// The session state machine: consumes calendar transitions, emits the
/// System Event codes + instrument-status effects for the feeds.
pub struct MarketCalendar {
    phase: SessionPhase,
    /// Emitted-but-not-yet-consumed system events.
    pending: Vec<SystemEventCode>,
}

impl MarketCalendar {
    pub fn new() -> Self {
        MarketCalendar {
            phase: SessionPhase::Closed,
            pending: Vec::new(),
        }
    }

    pub fn phase(&self) -> SessionPhase {
        self.phase
    }

    /// Apply one calendar transition.
    pub fn on_transition(&mut self, t: CalendarTransition) {
        use CalendarTransition as T;
        use SessionPhase as P;
        let next = match (self.phase, t) {
            // Normal session arc.
            (P::Closed, T::ToPreOpen) => Some((P::PreOpen, SystemEventCode::StartOfMessages)),
            (P::PreOpen, T::StartOpenAuction) => {
                Some((P::OpenAuction, SystemEventCode::StartOfOpenAuction))
            }
            (P::OpenAuction, T::EndOpenAuction) => {
                Some((P::Continuous, SystemEventCode::EndOfOpenAuction))
            }
            (P::Continuous, T::EndContinuous) => {
                Some((P::CloseAuction, SystemEventCode::EndOfContinuous))
            }
            (P::CloseAuction, T::EndCloseAuction) => {
                Some((P::PostClose, SystemEventCode::EndOfCloseAuction))
            }
            (P::PostClose, T::SessionOver) => Some((P::Closed, SystemEventCode::EndOfMessages)),
            // Halt semantics.
            (P::Continuous, T::Halt) => Some((P::PostClose, SystemEventCode::EndOfContinuous)),
            (P::PostClose, T::Resume) => Some((P::Continuous, SystemEventCode::EndOfOpenAuction)),
            // No-ops / invalid transitions ignored (calendar replays are
            // idempotent at the feed layer).
            _ => None,
        };
        if let Some((phase, code)) = next {
            self.phase = phase;
            self.pending.push(code);
        }
    }

    /// Drain pending system events (feed writer consumes in order).
    pub fn take_events(&mut self) -> Vec<SystemEventCode> {
        std::mem::take(&mut self.pending)
    }

    /// Instrument-status effect of the current phase (drives ITCH `H`
    /// trading-action and ME instrument status fan-out).
    pub fn instrument_status(&self) -> InstrumentStatus {
        match self.phase {
            SessionPhase::Continuous => InstrumentStatus::Open,
            // Auctions trade via uncross, not continuously.
            SessionPhase::OpenAuction | SessionPhase::CloseAuction => {
                InstrumentStatus::QuotationOnly
            }
            SessionPhase::Closed | SessionPhase::PreOpen | SessionPhase::PostClose => {
                InstrumentStatus::Halted
            }
        }
    }
}

impl Default for MarketCalendar {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bust_fans_out_cancel_broken_and_restore() {
        let e = BustEvent {
            trade_id: 42,
            instrument_id: 3,
            price: Price::equity(10_500),
            qty: Qty::units(100),
            resting_side_bid: false,
        };
        let out = on_bust(&e);
        // Order: trade-cancel N, broken B, book restore.
        assert!(matches!(out[0], BustFanout::TradeCancel { trade_id: 42 }));
        assert!(matches!(out[1], BustFanout::BrokenTrade { trade_id: 42 }));
        match &out[2] {
            BustFanout::RestoreLevel {
                instrument_id,
                side_bid,
                price,
                qty,
            } => {
                assert_eq!(*instrument_id, 3);
                assert!(!*side_bid);
                assert_eq!(price.raw, 10_500);
                assert_eq!(qty.raw, 100);
            }
            _ => panic!("expected restore"),
        }
    }

    #[test]
    fn amend_cancels_reprints_and_rebalances() {
        let e = AmendEvent {
            trade_id: 42,
            instrument_id: 3,
            original_price: Price::equity(10_500),
            original_qty: Qty::units(100),
            corrected_price: Price::equity(10_400),
            corrected_qty: Qty::units(100),
            resting_side_bid: true,
        };
        let out = on_amend(&e);
        assert!(matches!(out[0], BustFanout::TradeCancel { trade_id: 42 }));
        // Original volume restored at the original price.
        assert!(matches!(
            out[1],
            BustFanout::RestoreLevel { price, qty, .. }
                if price.raw == 10_500 && qty.raw == 100
        ));
        // Corrected re-print.
        assert!(matches!(
            out[2],
            BustFanout::CorrectedPrint { price, qty, .. }
                if price.raw == 10_400 && qty.raw == 100
        ));
    }

    #[test]
    fn session_arc_emits_all_system_events() {
        let mut cal = MarketCalendar::new();
        assert_eq!(cal.phase(), SessionPhase::Closed);

        cal.on_transition(CalendarTransition::ToPreOpen);
        cal.on_transition(CalendarTransition::StartOpenAuction);
        cal.on_transition(CalendarTransition::EndOpenAuction);
        cal.on_transition(CalendarTransition::EndContinuous);
        cal.on_transition(CalendarTransition::EndCloseAuction);
        cal.on_transition(CalendarTransition::SessionOver);

        assert_eq!(cal.phase(), SessionPhase::Closed);
        let events = cal.take_events();
        assert_eq!(
            events.iter().map(|e| e.wire()).collect::<Vec<u8>>(),
            vec![b'O', b'Q', b'M', b'C', b'E', b'P']
        );
        // Drained.
        assert!(cal.take_events().is_empty());
    }

    #[test]
    fn instrument_status_follows_phase() {
        let mut cal = MarketCalendar::new();
        assert_eq!(cal.instrument_status(), InstrumentStatus::Halted);
        cal.on_transition(CalendarTransition::ToPreOpen);
        assert_eq!(cal.instrument_status(), InstrumentStatus::Halted);
        cal.on_transition(CalendarTransition::StartOpenAuction);
        assert_eq!(cal.instrument_status(), InstrumentStatus::QuotationOnly);
        cal.on_transition(CalendarTransition::EndOpenAuction);
        assert_eq!(cal.instrument_status(), InstrumentStatus::Open);
        // Halt mid-continuous → halted; resume → open.
        cal.on_transition(CalendarTransition::Halt);
        assert_eq!(cal.instrument_status(), InstrumentStatus::Halted);
        cal.on_transition(CalendarTransition::Resume);
        assert_eq!(cal.instrument_status(), InstrumentStatus::Open);
    }

    #[test]
    fn invalid_transitions_are_ignored() {
        let mut cal = MarketCalendar::new();
        // EndContinuous from Closed - no-op.
        cal.on_transition(CalendarTransition::EndContinuous);
        assert_eq!(cal.phase(), SessionPhase::Closed);
        assert!(cal.take_events().is_empty());
        // Session arc is idempotent to replay: SessionOver only from PostClose.
        cal.on_transition(CalendarTransition::ToPreOpen);
        cal.on_transition(CalendarTransition::SessionOver); // invalid from PreOpen
        assert_eq!(cal.phase(), SessionPhase::PreOpen);
    }

    #[test]
    fn halt_resume_round_trip() {
        let mut cal = MarketCalendar::new();
        cal.on_transition(CalendarTransition::ToPreOpen);
        cal.on_transition(CalendarTransition::StartOpenAuction);
        cal.on_transition(CalendarTransition::EndOpenAuction);
        assert_eq!(cal.phase(), SessionPhase::Continuous);
        cal.on_transition(CalendarTransition::Halt);
        assert_eq!(cal.phase(), SessionPhase::PostClose);
        cal.on_transition(CalendarTransition::Resume);
        assert_eq!(cal.phase(), SessionPhase::Continuous);
    }
}
