// SPDX-FileCopyrightText: Copyright Logically Magic Inc. <hello@logicallymagic.com>
// SPDX-FileCopyrightText: Copyright Hamid Jahanzad <Hamid@Jahanzad.com>
// SPDX-FileCopyrightText: Copyright Contributors to the Exchange8:MD
// SPDX-License-Identifier: Apache-2.0

//! Exchange8:MD/Market-protection extensions + surveillance framework + index
//! publication + auction extensions + conformance & reconstruction
//! (see services/md/DESIGN.md §2 for the component map).
//!
//! - **Volatility interruption**: LULD band breach → halt the
//!   instrument and route it into a volatility auction.
//! - **Order-to-trade ratio limits**: per-connection OTR watermark.
//! - **Minimum quote life**: anti-spoofing - orders must rest a
//!   minimum lifetime before cancel/replace.
//! - **Auction order types**: MOO/MOC/LOO/LOC classification.
//! - **Random uncrossing window**: randomized start within a window.
//! - **Volatility auction integration**: halt → auction → resume.
//! - **Surveillance framework**: the alert pipeline (observe →
//!   detectors → alert → case escalation).
//! - **Index**: divisor rebalancing + publication event shape.
//! - **Conformance**: certification scenario runner (pure checks).
//! - **Reconstruction**: event-stream replay to rebuild order state.
//!
//! Design: `design/sentin8-exchange8-market-protection-technical-design.md`,
//! `design/sentin8-exchange8-auctions-technical-design.md`,
//! `design/sentin8-exchange8-surveillance-technical-design.md`,
//! `design/sentin8-exchange8-index-engine-technical-design.md`,
//! `design/sentin8-exchange8-connectivity-operations-technical-design.md` §3-§5.

use std::collections::HashMap;

use exchange8::fixed::Price;

use crate::luld::{LimitState, LuldEngine};

// ------------------------------------------------------------------------
// Volatility interruption (see services/md/DESIGN.md §2, §3)
// ------------------------------------------------------------------------

/// Volatility-auction trigger: a trade/print outside the LULD band.
#[derive(Clone, Debug, PartialEq)]
pub struct VolatilityTrigger {
    pub instrument_id: u32,
    /// The offending price.
    pub price: Price,
    /// Reference price the band is computed from.
    pub reference: Price,
}

/// Interruption decision + follow-on auction routing.
#[derive(Clone, Debug, PartialEq)]
pub enum InterruptionDecision {
    /// Within bands - no action.
    None,
    /// Breach: halt + enter volatility auction.
    HaltAndAuction {
        instrument_id: u32,
        /// Which band was breached.
        band: &'static str,
    },
}

/// Evaluate a print against the instrument's LULD engine →
/// interruption decision.
pub fn check_volatility(
    instrument_id: u32,
    price: Price,
    engine: &mut LuldEngine,
) -> InterruptionDecision {
    match engine.evaluate_trade(instrument_id, price.raw) {
        Some(LimitState::LimitUp) => InterruptionDecision::HaltAndAuction {
            instrument_id,
            band: "limit_up",
        },
        Some(LimitState::LimitDown) => InterruptionDecision::HaltAndAuction {
            instrument_id,
            band: "limit_down",
        },
        _ => InterruptionDecision::None,
    }
}

/// The volatility auction arc (halt → call → uncross → resume).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VolatilityArc {
    /// Continuous trading.
    Continuous,
    /// Halted pending auction call.
    Halted,
    /// Auction call collecting orders.
    Call,
    /// Uncross executed; cooling down before resume.
    Resuming,
}

pub struct VolatilityAuction {
    state: VolatilityArc,
}

impl VolatilityAuction {
    pub fn new() -> Self {
        VolatilityAuction {
            state: VolatilityArc::Continuous,
        }
    }

    pub fn state(&self) -> VolatilityArc {
        self.state
    }

    /// Breach → halt, then open the call.
    pub fn on_breach(&mut self) {
        self.state = VolatilityArc::Halted;
    }

    /// Open the auction call (ops or automatic).
    pub fn open_call(&mut self) {
        if self.state == VolatilityArc::Halted {
            self.state = VolatilityArc::Call;
        }
    }

    /// Uncross done → resume continuous.
    pub fn on_uncrossed(&mut self) {
        if self.state == VolatilityArc::Call {
            self.state = VolatilityArc::Resuming;
        }
    }

    pub fn resume(&mut self) {
        if self.state == VolatilityArc::Resuming {
            self.state = VolatilityArc::Continuous;
        }
    }
}

impl Default for VolatilityAuction {
    fn default() -> Self {
        Self::new()
    }
}

// ------------------------------------------------------------------------
// Order-to-trade ratio (see services/md/DESIGN.md §2)
// ------------------------------------------------------------------------

/// Per-connection OTR counters.
pub struct OtrGuard {
    /// orders submitted / trades executed per connection.
    counters: HashMap<u16, (u64, u64)>,
    /// Max ratio (orders per trade) allowed.
    max_ratio: u64,
    /// Grace trades before enforcement (new sessions).
    grace: u64,
}

impl OtrGuard {
    pub fn new(max_ratio: u64, grace: u64) -> Self {
        OtrGuard {
            counters: HashMap::new(),
            max_ratio,
            grace,
        }
    }

    pub fn on_order(&mut self, connection_id: u16) {
        let e = self.counters.entry(connection_id).or_insert((0, 0));
        e.0 += 1;
    }

    pub fn on_trade(&mut self, connection_id: u16) {
        let e = self.counters.entry(connection_id).or_insert((0, 0));
        e.1 += 1;
    }

    /// Is the connection over its OTR limit? (grace applies until the
    /// trade count reaches it)
    pub fn over_limit(&self, connection_id: u16) -> bool {
        match self.counters.get(&connection_id) {
            Some((orders, trades)) => {
                if *trades < self.grace {
                    return false;
                }
                *orders / *trades > self.max_ratio
            }
            None => false,
        }
    }

    /// Current ratio (diagnostics).
    pub fn ratio(&self, connection_id: u16) -> Option<u64> {
        self.counters
            .get(&connection_id)
            .map(|(o, t)| if *t == 0 { u64::MAX } else { o / t })
    }
}

// ------------------------------------------------------------------------
// Minimum quote life (see services/md/DESIGN.md §2)
// ------------------------------------------------------------------------

/// Anti-spoofing: orders must rest a minimum lifetime before cancel.
pub struct QuoteLifeGuard {
    /// minimum lifetime in µs.
    min_life_us: u64,
    /// order_id → (rested_at, is_quote).
    resting: HashMap<u64, (u64, bool)>,
}

impl QuoteLifeGuard {
    pub fn new(min_life_us: u64) -> Self {
        QuoteLifeGuard {
            min_life_us,
            resting: HashMap::new(),
        }
    }

    /// An order rested (booked) at `ts_us`. Quote-class orders (post-only
    /// displayed liquidity providing) are guarded.
    pub fn on_booked(&mut self, order_id: u64, ts_us: u64, is_quote: bool) {
        self.resting.insert(order_id, (ts_us, is_quote));
    }

    /// Order left the book.
    pub fn on_gone(&mut self, order_id: u64) {
        self.resting.remove(&order_id);
    }

    /// May this order be cancelled at `ts_us`? Non-quote orders always
    /// may; quotes must have rested `min_life_us`.
    pub fn may_cancel(&self, order_id: u64, ts_us: u64) -> bool {
        match self.resting.get(&order_id) {
            Some((rested, true)) => ts_us.saturating_sub(*rested) >= self.min_life_us,
            _ => true,
        }
    }
}

// ------------------------------------------------------------------------
// Auction order classification
// ------------------------------------------------------------------------

/// Auction order kinds (FIX 40-style subset).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuctionOrderType {
    /// Market-on-open.
    Moo,
    /// Market-on-close.
    Moc,
    /// Limit-on-open.
    Loo,
    /// Limit-on-close.
    Loc,
    /// Regular limit (not auction-bound).
    Limit,
}

impl AuctionOrderType {
    /// Classify from (auction_session_open?, has_limit, on_close?).
    pub fn classify(session_open: bool, has_limit: bool, on_close: bool) -> Self {
        match (session_open, has_limit, on_close) {
            (true, false, _) => AuctionOrderType::Moo,
            (true, true, _) => AuctionOrderType::Loo,
            (false, false, true) => AuctionOrderType::Moc,
            (false, true, true) => AuctionOrderType::Loc,
            (false, _, false) => AuctionOrderType::Limit,
        }
    }

    /// Does it execute only in the auction uncross?
    pub fn is_auction_only(self) -> bool {
        !matches!(self, AuctionOrderType::Limit)
    }
}

// ------------------------------------------------------------------------
// Random uncrossing window
// ------------------------------------------------------------------------

/// Randomized uncross start within [open_us, open_us + window_us) -
/// deterministic xorshift from a per-day seed (reproducible ops replay).
pub fn uncross_time(day_seed: u64, open_us: u64, window_us: u64) -> u64 {
    if window_us == 0 {
        return open_us;
    }
    let mut x = day_seed | 1;
    x ^= x >> 12;
    x ^= x << 25;
    x ^= x >> 27;
    let r = x.wrapping_mul(0x2545F4914F6CDD1D);
    open_us + (r % window_us)
}

// ------------------------------------------------------------------------
// Surveillance framework (alert pipeline)
// ------------------------------------------------------------------------

/// Severity (mirrors surveillance.rs).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Info,
    Low,
    Medium,
    High,
    Critical,
}

/// A surveillance observation entering the pipeline.
#[derive(Clone, Debug, PartialEq)]
pub struct FeedObservation {
    pub instrument_id: u32,
    pub account: String,
    pub ts_us: u64,
    pub kind: &'static str,
}

/// An alert raised by a detector.
#[derive(Clone, Debug, PartialEq)]
pub struct FrameworkAlert {
    pub detector: &'static str,
    pub severity: Severity,
    pub instrument_id: u32,
    pub account: String,
    pub ts_us: u64,
}

/// The alert pipeline: observations → detector closures → alerts →
/// case escalation when severity ≥ High.
pub struct SurveillancePipeline {
    /// Raised alerts (drained by the case manager).
    alerts: Vec<FrameworkAlert>,
    /// Open cases (escalated alerts).
    cases: Vec<FrameworkAlert>,
}

impl SurveillancePipeline {
    pub fn new() -> Self {
        SurveillancePipeline {
            alerts: Vec::new(),
            cases: Vec::new(),
        }
    }

    /// Run one observation through the detector set.
    pub fn observe(
        &mut self,
        o: &FeedObservation,
        detectors: &[(
            &'static str,
            Box<dyn Fn(&FeedObservation) -> Option<Severity>>,
        )],
    ) {
        for (name, det) in detectors {
            if let Some(sev) = det(o) {
                let alert = FrameworkAlert {
                    detector: name,
                    severity: sev,
                    instrument_id: o.instrument_id,
                    account: o.account.clone(),
                    ts_us: o.ts_us,
                };
                if alert.severity >= Severity::High {
                    self.cases.push(alert.clone());
                }
                self.alerts.push(alert);
            }
        }
    }

    /// Drain alerts.
    pub fn drain_alerts(&mut self) -> Vec<FrameworkAlert> {
        std::mem::take(&mut self.alerts)
    }

    /// Open cases (escalated alerts consumed by the case surface).
    pub fn open_cases(&self) -> &[FrameworkAlert] {
        &self.cases
    }
}

impl Default for SurveillancePipeline {
    fn default() -> Self {
        Self::new()
    }
}

// ------------------------------------------------------------------------
// Index - divisor rebalancing + publication
// ------------------------------------------------------------------------

/// Divisor rebalancing on a constituent change: keep the index
/// value continuous across add/remove.
///
/// new_divisor = old_divisor × new_sum / old_sum (value continuity).
pub fn rebalance_divisor(old_divisor: f64, old_sum: f64, new_sum: f64) -> f64 {
    if old_sum == 0.0 {
        return old_divisor;
    }
    old_divisor * new_sum / old_sum
}

/// Index publication event (consumed by margin/funding).
#[derive(Clone, Debug, PartialEq)]
pub struct IndexPublication {
    pub index_id: u32,
    pub value: f64,
    pub ts_us: u64,
}

/// Publish when the value moved ≥ min_change since the last publication.
pub fn should_publish(last: Option<f64>, next: f64, min_change: f64) -> bool {
    match last {
        None => true,
        Some(l) => (next - l).abs() >= min_change,
    }
}

// ------------------------------------------------------------------------
// Conformance scenario runner
// ------------------------------------------------------------------------

/// One conformance check result.
#[derive(Clone, Debug, PartialEq)]
pub struct ConformanceResult {
    pub scenario: &'static str,
    pub passed: bool,
    pub detail: String,
}

/// A conformance scenario: name + the check closure over the session's
/// message log (ordered FIX/OUCH wire strings).
pub struct ConformanceSuite {
    pub scenarios: Vec<(&'static str, Box<dyn Fn(&[String]) -> ConformanceResult>)>,
}

impl ConformanceSuite {
    /// Run all scenarios against a captured session log.
    pub fn run(&self, session_log: &[String]) -> Vec<ConformanceResult> {
        self.scenarios
            .iter()
            .map(|(_name, check)| check(session_log))
            .collect()
    }
}

/// Built-in scenario: logon handshake completed in order.
pub fn scenario_logon_order(log: &[String]) -> ConformanceResult {
    let logon = log.iter().position(|m| m.contains("35=A"));
    let heartbeat = log.iter().position(|m| m.contains("35=0"));
    let ok = match (logon, heartbeat) {
        (Some(l), Some(h)) => l < h,
        (Some(_), None) => true,
        _ => false,
    };
    ConformanceResult {
        scenario: "logon_order",
        passed: ok,
        detail: format!("logon@{logon:?} heartbeat@{heartbeat:?}"),
    }
}

/// Built-in scenario: every NewOrderSingle received exactly one
/// ExecutionReport (or reject).
pub fn scenario_order_acks(log: &[String]) -> ConformanceResult {
    let news: Vec<usize> = log
        .iter()
        .enumerate()
        .filter(|(_, m)| m.contains("35=D"))
        .map(|(i, _)| i)
        .collect();
    let execs = log.iter().filter(|m| m.contains("35=8")).count();
    let rejects = log.iter().filter(|m| m.contains("35=9")).count();
    let ok = news.len() == execs + rejects;
    ConformanceResult {
        scenario: "order_acks",
        passed: ok,
        detail: format!("new={} exec={} reject={}", news.len(), execs, rejects),
    }
}

// ------------------------------------------------------------------------
// Order/trade reconstruction
// ------------------------------------------------------------------------

/// A journaled lifecycle event (input to reconstruction).
#[derive(Clone, Debug, PartialEq)]
pub enum ReconEvent {
    Submitted { order_id: u64, qty: u64 },
    Booked { order_id: u64 },
    Executed { order_id: u64, fill_qty: u64 },
    Killed { order_id: u64 },
}

/// Reconstructed order state at a point in the stream.
#[derive(Clone, Debug, PartialEq)]
pub struct ReconOrder {
    pub order_id: u64,
    pub qty: u64,
    pub filled: u64,
    pub live: bool,
}

/// Replay events up to (and including) `upto` index - rebuild order state
/// (replays the event stream to rebuild any order/trade state).
pub fn reconstruct(events: &[ReconEvent], upto: usize) -> HashMap<u64, ReconOrder> {
    let mut orders: HashMap<u64, ReconOrder> = HashMap::new();
    for e in events.iter().take(upto) {
        match e {
            ReconEvent::Submitted { order_id, qty } => {
                orders.insert(
                    *order_id,
                    ReconOrder {
                        order_id: *order_id,
                        qty: *qty,
                        filled: 0,
                        live: true,
                    },
                );
            }
            ReconEvent::Booked { order_id } => {
                if let Some(o) = orders.get_mut(order_id) {
                    o.live = true;
                }
            }
            ReconEvent::Executed { order_id, fill_qty } => {
                if let Some(o) = orders.get_mut(order_id) {
                    o.filled += fill_qty;
                    if o.filled >= o.qty {
                        o.live = false;
                    }
                }
            }
            ReconEvent::Killed { order_id } => {
                if let Some(o) = orders.get_mut(order_id) {
                    o.live = false;
                }
            }
        }
    }
    orders
}

// ------------------------------------------------------------------------
// Order protection (no trade-through of protected quotes)
// ------------------------------------------------------------------------

/// A protected quote (Reg NMS NBO/NBB - best bid/offer of the protected
/// venue, e.g. the primary listing exchange).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ProtectedQuote {
    pub bid: Option<(i128, u64)>,
    pub ask: Option<(i128, u64)>,
}

/// Trade-through decision for one incoming lockable order.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ProtectionVerdict {
    /// No protected quote or price is at-or-better - trade freely.
    Allowed,
    /// Buying through the protected bid (price < protected bid) - blocked
    /// unless an exception applies (sweep, cross, IPO, etc.).
    TradeThroughBid { protected: i128 },
    /// Selling through the protected ask (price > protected ask).
    TradeThroughAsk { protected: i128 },
}

/// Order-protection check: an order may not execute through the
/// protected quote. `sweep_exception`: a crossing order that sweeps to a
/// better price than the protected quote is exempt (Reg NMS sweep).
pub fn check_order_protection(
    is_buy: bool,
    price: i128,
    protected: &ProtectedQuote,
    sweep_exception: bool,
) -> ProtectionVerdict {
    if is_buy {
        if let Some((ask_px, _)) = protected.ask {
            // Buy below the protected ask = fine (price improvement).
            if price > ask_px && !sweep_exception {
                return ProtectionVerdict::TradeThroughAsk { protected: ask_px };
            }
        }
    } else if let Some((bid_px, _)) = protected.bid {
        // Sell above the protected bid = fine.
        if price < bid_px && !sweep_exception {
            return ProtectionVerdict::TradeThroughBid { protected: bid_px };
        }
    }
    ProtectionVerdict::Allowed
}

// ------------------------------------------------------------------------
// Crypto corporate actions (forks, airdrops, staking rewards)
// ------------------------------------------------------------------------

/// Crypto corporate-action kinds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CryptoActionKind {
    /// Chain fork: holders receive the forked asset 1:1 at snapshot.
    Fork { new_asset: &'static str },
    /// Airdrop: fixed qty per holder (or per unit held).
    Airdrop,
    /// Staking reward: accrued per-unit-held rate at snapshot.
    StakingReward,
}

/// One crypto CA.
#[derive(Clone, Debug, PartialEq)]
pub struct CryptoAction {
    pub kind: CryptoActionKind,
    /// Asset the action applies to.
    pub asset: String,
    /// Snapshot block/height (positions at snapshot participate).
    pub snapshot: u64,
    /// Fork: 1:1; airdrop: qty per holder; staking: rate per unit.
    pub per_unit: Option<f64>,
}

/// Apply one crypto CA to a position (qty at snapshot).
pub fn apply_crypto_action(action: &CryptoAction, position_qty: u64) -> Vec<(String, f64)> {
    // (asset, delta) pairs.
    match action.kind {
        CryptoActionKind::Fork { new_asset } => {
            // 1:1 fork credit (qty unchanged on the original).
            vec![(new_asset.to_string(), position_qty as f64)]
        }
        CryptoActionKind::Airdrop => {
            let per = action.per_unit.unwrap_or(0.0);
            // Airdrop: per-unit when set, else a flat credit modeled as
            // per_unit × qty by the caller (policy row).
            vec![(action.asset.clone(), per * position_qty as f64)]
        }
        CryptoActionKind::StakingReward => {
            let rate = action.per_unit.unwrap_or(0.0);
            vec![(action.asset.clone(), rate * position_qty as f64)]
        }
    }
}

// ------------------------------------------------------------------------
// Crypto market-data publication (SSE/WS shapes)
// ------------------------------------------------------------------------

/// One SSE/WS market-data event for the crypto feed (no Mold/ITCH).
#[derive(Clone, Debug, PartialEq)]
pub enum CryptoFeedEvent {
    /// BBO update (instrument, bid, ask).
    Bbo {
        instrument_id: u32,
        bid: Option<i128>,
        ask: Option<i128>,
    },
    /// Depth level delta (instrument, price, qty, side, is_snapshot).
    Depth {
        instrument_id: u32,
        price: i128,
        qty: u64,
        side_bid: bool,
    },
    /// Trade print.
    Trade {
        instrument_id: u32,
        price: i128,
        qty: u64,
    },
}

impl CryptoFeedEvent {
    /// SSE wire form (`event:` + `data:` lines).
    pub fn to_sse(&self) -> String {
        match self {
            CryptoFeedEvent::Bbo {
                instrument_id,
                bid,
                ask,
            } => format!(
                "event: bbo\ndata: {{\"i\":{},\"b\":{},\"a\":{}}}",
                instrument_id,
                bid.map(|p| p.to_string()).unwrap_or_else(|| "null".into()),
                ask.map(|p| p.to_string()).unwrap_or_else(|| "null".into())
            ),
            CryptoFeedEvent::Depth {
                instrument_id,
                price,
                qty,
                side_bid,
            } => format!(
                "event: depth\ndata: {{\"i\":{},\"p\":{},\"q\":{},\"s\":\"{}\"}}",
                instrument_id,
                price,
                qty,
                if *side_bid { 'b' } else { 'a' }
            ),
            CryptoFeedEvent::Trade {
                instrument_id,
                price,
                qty,
            } => format!(
                "event: trade\ndata: {{\"i\":{},\"p\":{},\"q\":{}}}",
                instrument_id, price, qty
            ),
        }
    }

    /// WS JSON form (same payload, single frame).
    pub fn to_ws_json(&self) -> String {
        // Identical payload to SSE data (the WS frame carries it raw).
        self.to_sse()
            .lines()
            .find(|l| l.starts_with("data: "))
            .map(|l| l[6..].to_string())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::luld::Tier;

    fn px(raw: i128) -> Price {
        Price::equity(raw)
    }

    #[test]
    fn volatility_breach_halts_into_auction() {
        let mut eng = LuldEngine::new();
        eng.set_tier(3, Tier::Tier3);
        eng.set_reference(3, 10_000);
        match check_volatility(3, px(12_000), &mut eng) {
            InterruptionDecision::HaltAndAuction {
                instrument_id,
                band,
            } => {
                assert_eq!(instrument_id, 3);
                assert_eq!(band, "limit_up");
            }
            other => panic!("expected halt, got {other:?}"),
        }
        // Within band → none (fresh engine state).
        let mut eng2 = LuldEngine::new();
        eng2.set_tier(4, Tier::Tier3);
        eng2.set_reference(4, 10_000);
        assert_eq!(
            check_volatility(4, px(10_500), &mut eng2),
            InterruptionDecision::None
        );
    }

    #[test]
    fn volatility_arc_full_cycle() {
        let mut v = VolatilityAuction::new();
        assert_eq!(v.state(), VolatilityArc::Continuous);
        v.on_breach();
        assert_eq!(v.state(), VolatilityArc::Halted);
        v.open_call();
        assert_eq!(v.state(), VolatilityArc::Call);
        v.on_uncrossed();
        assert_eq!(v.state(), VolatilityArc::Resuming);
        v.resume();
        assert_eq!(v.state(), VolatilityArc::Continuous);
        // Invalid transitions are no-ops.
        let mut v2 = VolatilityAuction::new();
        v2.open_call(); // not halted
        assert_eq!(v2.state(), VolatilityArc::Continuous);
    }

    #[test]
    fn otr_guard_enforces_ratio_with_grace() {
        let mut g = OtrGuard::new(10, 5); // 10 orders/trade max, 5-trade grace
        for _ in 0..60 {
            g.on_order(7);
        }
        for _ in 0..5 {
            g.on_trade(7);
        }
        // 60/5 = 12 > 10 → over.
        assert!(g.over_limit(7));
        assert_eq!(g.ratio(7), Some(12));
        // Under grace another connection is fine despite ratio.
        for _ in 0..30 {
            g.on_order(9);
        }
        g.on_trade(9); // 1 trade < grace 5
        assert!(!g.over_limit(9));
        // Unknown connection fine.
        assert!(!g.over_limit(99));
    }

    #[test]
    fn quote_life_blocks_early_cancel() {
        let mut q = QuoteLifeGuard::new(500_000); // 0.5s
        q.on_booked(1, 1_000_000, true); // quote order
        q.on_booked(2, 1_000_000, false); // aggressive
                                          // Quote cancelled at +200ms → blocked.
        assert!(!q.may_cancel(1, 1_200_000));
        // At +500ms → allowed.
        assert!(q.may_cancel(1, 1_500_000));
        // Non-quote always cancellable.
        assert!(q.may_cancel(2, 1_000_001));
        // Unknown order (already gone) → allowed.
        q.on_gone(2);
        assert!(q.may_cancel(2, 1_000_001));
    }

    #[test]
    fn auction_order_classification() {
        assert_eq!(
            AuctionOrderType::classify(true, false, false),
            AuctionOrderType::Moo
        );
        assert_eq!(
            AuctionOrderType::classify(true, true, false),
            AuctionOrderType::Loo
        );
        assert_eq!(
            AuctionOrderType::classify(false, false, true),
            AuctionOrderType::Moc
        );
        assert_eq!(
            AuctionOrderType::classify(false, true, true),
            AuctionOrderType::Loc
        );
        assert_eq!(
            AuctionOrderType::classify(false, false, false),
            AuctionOrderType::Limit
        );
        assert!(AuctionOrderType::Moc.is_auction_only());
        assert!(!AuctionOrderType::Limit.is_auction_only());
    }

    #[test]
    fn uncross_time_within_window_deterministic() {
        let t1 = uncross_time(42, 1_000_000, 300_000);
        let t2 = uncross_time(42, 1_000_000, 300_000);
        assert_eq!(t1, t2); // deterministic per seed
        assert!((1_000_000..1_300_000).contains(&t1));
        // Different seed → (almost surely) different time.
        let t3 = uncross_time(43, 1_000_000, 300_000);
        assert!((1_000_000..1_300_000).contains(&t3));
        // Zero window → open time.
        assert_eq!(uncross_time(1, 5_000, 0), 5_000);
    }

    #[test]
    fn surveillance_pipeline_escalates_high() {
        let mut p = SurveillancePipeline::new();
        let detectors: Vec<(
            &'static str,
            Box<dyn Fn(&FeedObservation) -> Option<Severity>>,
        )> = vec![
            (
                "spoofing",
                Box::new(|o| (o.kind == "layering").then_some(Severity::High)),
            ),
            (
                "tape_watch",
                Box::new(|o| (o.kind == "large_print").then_some(Severity::Info)),
            ),
        ];
        p.observe(
            &FeedObservation {
                instrument_id: 3,
                account: "A".into(),
                ts_us: 1,
                kind: "layering",
            },
            &detectors,
        );
        p.observe(
            &FeedObservation {
                instrument_id: 3,
                account: "B".into(),
                ts_us: 2,
                kind: "large_print",
            },
            &detectors,
        );
        let alerts = p.drain_alerts();
        assert_eq!(alerts.len(), 2);
        assert_eq!(p.open_cases().len(), 1); // only the High escalated
        assert_eq!(p.open_cases()[0].detector, "spoofing");
        assert!(p.drain_alerts().is_empty());
        // Cases persist.
        assert_eq!(p.open_cases().len(), 1);
    }

    #[test]
    fn divisor_rebalance_preserves_value() {
        // Old: sum 800, divisor 4 → value 200. Remove a 200-weighted
        // constituent → new sum 600; divisor must become 3 to keep 200.
        let d = rebalance_divisor(4.0, 800.0, 600.0);
        assert!((d - 3.0).abs() < 1e-9);
        // Zero old sum → unchanged.
        assert_eq!(rebalance_divisor(4.0, 0.0, 100.0), 4.0);
    }

    #[test]
    fn index_publication_gate() {
        assert!(should_publish(None, 100.0, 0.1)); // first always
        assert!(!should_publish(Some(100.0), 100.05, 0.1));
        assert!(should_publish(Some(100.0), 100.2, 0.1));
        assert!(should_publish(Some(100.0), 99.85, 0.1)); // down move
    }

    #[test]
    fn conformance_scenarios() {
        let log = vec![
            "8=FIX.4.2|35=A|553=U|554=P".into(),
            "8=FIX.4.2|35=0".into(),
            "8=FIX.4.2|35=D|11=1|55=EX8A".into(),
            "8=FIX.4.2|35=8|11=1|150=0".into(),
        ];
        assert!(scenario_logon_order(&log).passed);
        assert!(scenario_order_acks(&log).passed);
        // Order without ack fails.
        let bad = vec!["8=FIX.4.2|35=A".into(), "8=FIX.4.2|35=D|11=2".into()];
        assert!(!scenario_order_acks(&bad).passed);
        // No logon fails.
        let nologon = vec!["8=FIX.4.2|35=D|11=3".into()];
        assert!(!scenario_logon_order(&nologon).passed);
    }

    #[test]
    fn conformance_suite_runs_all() {
        let suite = ConformanceSuite {
            scenarios: vec![
                (
                    "logon_order",
                    Box::new(|log: &[String]| scenario_logon_order(log)),
                ),
                (
                    "order_acks",
                    Box::new(|log: &[String]| scenario_order_acks(log)),
                ),
            ],
        };
        let log = vec!["35=A".into(), "35=D|11=1".into(), "35=8|11=1".into()];
        let results = suite.run(&log);
        assert_eq!(results.len(), 2);
        assert!(results.iter().all(|r| r.passed));
    }

    #[test]
    fn reconstruction_replays_state() {
        let events = vec![
            ReconEvent::Submitted {
                order_id: 1,
                qty: 100,
            },
            ReconEvent::Booked { order_id: 1 },
            ReconEvent::Executed {
                order_id: 1,
                fill_qty: 40,
            },
            ReconEvent::Executed {
                order_id: 1,
                fill_qty: 60,
            }, // complete
            ReconEvent::Submitted {
                order_id: 2,
                qty: 50,
            },
            ReconEvent::Killed { order_id: 2 },
        ];
        // Mid-stream (after 3 events): order 1 partially filled, live.
        let mid = reconstruct(&events, 3);
        let o1 = &mid[&1];
        assert_eq!((o1.filled, o1.live), (40, true));
        assert!(mid.get(&2).is_none());
        // End of stream: order 1 complete (not live), order 2 killed.
        let end = reconstruct(&events, events.len());
        assert_eq!(end[&1].filled, 100);
        assert!(!end[&1].live);
        assert!(!end[&2].live);
        assert_eq!(end[&2].filled, 0);
    }

    #[test]
    fn order_protection_blocks_trade_through() {
        let pq = ProtectedQuote {
            bid: Some((99_000, 100)),
            ask: Some((101_000, 100)),
        };
        assert_eq!(
            check_order_protection(true, 101_000, &pq, false),
            ProtectionVerdict::Allowed
        );
        assert!(matches!(
            check_order_protection(true, 101_500, &pq, false),
            ProtectionVerdict::TradeThroughAsk { protected: 101_000 }
        ));
        assert_eq!(
            check_order_protection(false, 99_000, &pq, false),
            ProtectionVerdict::Allowed
        );
        assert!(matches!(
            check_order_protection(false, 98_500, &pq, false),
            ProtectionVerdict::TradeThroughBid { protected: 99_000 }
        ));
        // Sweep exception lifts the block.
        assert_eq!(
            check_order_protection(true, 101_500, &pq, true),
            ProtectionVerdict::Allowed
        );
        // One-sided protected quote: only that side enforced.
        let bid_only = ProtectedQuote {
            bid: None,
            ask: Some((101_000, 1)),
        };
        assert_eq!(
            check_order_protection(false, 50_000, &bid_only, false),
            ProtectionVerdict::Allowed
        );
    }

    #[test]
    fn crypto_actions_credit_positions() {
        let fork = CryptoAction {
            kind: CryptoActionKind::Fork { new_asset: "BTG" },
            asset: "BTC".into(),
            snapshot: 800_000,
            per_unit: None,
        };
        assert_eq!(apply_crypto_action(&fork, 3), vec![("BTG".into(), 3.0)]);
        let drop = CryptoAction {
            kind: CryptoActionKind::Airdrop,
            asset: "ETH".into(),
            snapshot: 1,
            per_unit: Some(0.5),
        };
        assert_eq!(apply_crypto_action(&drop, 4), vec![("ETH".into(), 2.0)]);
        let stake = CryptoAction {
            kind: CryptoActionKind::StakingReward,
            asset: "ETH".into(),
            snapshot: 2,
            per_unit: Some(0.03),
        };
        assert_eq!(apply_crypto_action(&stake, 2), vec![("ETH".into(), 0.06)]);
    }

    #[test]
    fn crypto_feed_events_render_sse_and_ws() {
        let bbo = CryptoFeedEvent::Bbo {
            instrument_id: 1,
            bid: Some(950_000_000),
            ask: None,
        };
        assert!(bbo.to_sse().starts_with("event: bbo\ndata: "));
        assert!(bbo.to_sse().contains("\"b\":950000000"));
        assert!(bbo.to_sse().contains("\"a\":null"));
        assert_eq!(bbo.to_ws_json(), "{\"i\":1,\"b\":950000000,\"a\":null}");
        let d = CryptoFeedEvent::Depth {
            instrument_id: 1,
            price: 951_000_000,
            qty: 100,
            side_bid: true,
        };
        assert!(d.to_ws_json().contains("\"s\":\"b\""));
        let t = CryptoFeedEvent::Trade {
            instrument_id: 1,
            price: 950_000_000,
            qty: 5,
        };
        assert!(t.to_sse().starts_with("event: trade\ndata: "));
    }
}
