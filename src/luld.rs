// SPDX-FileCopyrightText: Copyright Logically Magic Inc. <hello@logicallymagic.com>
// SPDX-FileCopyrightText: Copyright Hamid Jahanzad <Hamid@Jahanzad.com>
// SPDX-FileCopyrightText: Copyright Contributors to the Exchange8:MD
// SPDX-License-Identifier: Apache-2.0

//! Ex8:MD LULD price-band engine.
//!
//! Per-instrument limit-up/limit-down bands around a reference price
//! (previous close or last auction price) with tiered widths. A trade that
//! would print outside the band moves the instrument into a limit state;
//! sustained breaches trigger a volatility interruption (auction — see the
//! auctions design).
//!
//! Design: `design/sentin8-exchange8-market-protection-technical-design.md` §2.

/// LULD tier (double-auction-period and opening/closing bands differ).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tier {
    /// Tier 1: ±5% (S&P 500 / select securities).
    Tier1,
    /// Tier 2: ±10% (all other NMS stocks).
    Tier2,
    /// Wide-band instruments (e.g. leveraged ETFs): ±20%.
    Tier3,
}

impl Tier {
    /// Band width in basis points from the reference.
    pub fn band_bps(self) -> i64 {
        match self {
            Tier::Tier1 => 500,
            Tier::Tier2 => 1_000,
            Tier::Tier3 => 2_000,
        }
    }
}

/// Instrument limit state (the persisted rows live in `md.limit_state`; see
/// services/md/DESIGN.md §3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LimitState {
    /// Trading unrestricted.
    None,
    /// Price at or above the upper band — buy orders restricted.
    LimitUp,
    /// Price at or below the lower band — sell orders restricted.
    LimitDown,
    /// Both bands crossed (should not occur; defensive).
    Straddle,
}

/// Per-instrument LULD band computation.
pub struct LuldEngine {
    /// Tiered width configuration per instrument.
    tiers: std::collections::HashMap<u32, Tier>,
    /// Reference prices (previous close or last auction price).
    references: std::collections::HashMap<u32, i128>,
    /// Current limit state per instrument.
    states: std::collections::HashMap<u32, LimitState>,
}

impl LuldEngine {
    pub fn new() -> Self {
        LuldEngine {
            tiers: std::collections::HashMap::new(),
            references: std::collections::HashMap::new(),
            states: std::collections::HashMap::new(),
        }
    }

    /// Configure an instrument's tier.
    pub fn set_tier(&mut self, instrument_id: u32, tier: Tier) {
        self.tiers.insert(instrument_id, tier);
    }

    /// Set the reference price (prior close / last auction).
    pub fn set_reference(&mut self, instrument_id: u32, reference: i128) {
        self.references.insert(instrument_id, reference);
    }

    /// Compute the band for an instrument: (upper, lower) raw ticks.
    pub fn band(&self, instrument_id: u32) -> Option<(i128, i128)> {
        let tier = *self.tiers.get(&instrument_id)?;
        let reference = self.references.get(&instrument_id)?;
        let width = reference * tier.band_bps() as i128 / 10_000;
        Some((reference + width, reference - width))
    }

    /// Classify a would-be trade price against the bands. Updates the
    /// instrument's limit state. Returns the state after the evaluation.
    ///
    /// - Inside the band → `None` state (unrestricted).
    /// - At/above upper → `LimitUp`.
    /// - At/below lower → `LimitDown`.
    pub fn evaluate_trade(&mut self, instrument_id: u32, price: i128) -> Option<LimitState> {
        let Some((upper, lower)) = self.band(instrument_id) else {
            return None;
        };
        let state = if price >= upper {
            LimitState::LimitUp
        } else if price <= lower {
            LimitState::LimitDown
        } else {
            LimitState::None
        };
        self.states.insert(instrument_id, state);
        (state != LimitState::None).then_some(state)
    }

    /// Check an incoming order's price against the bands (order-protection
    /// layer): orders priced outside the band on the wrong side are blocked.
    ///
    /// Returns true when the order may rest/execute at `price`.
    pub fn order_allowed(&self, instrument_id: u32, price: i128, is_buy: bool) -> bool {
        let Some((upper, lower)) = self.band(instrument_id) else {
            return true; // unconfigured → unrestricted (shell gates config)
        };
        let Some(state) = self.states.get(&instrument_id) else {
            return true;
        };
        // Direction gating by limit state.
        let direction_ok = match state {
            LimitState::LimitUp => !is_buy, // in limit-up, block buy orders
            LimitState::LimitDown => is_buy,
            LimitState::None => true,
            LimitState::Straddle => false,
        };
        // Additionally the order itself must be inside the band.
        direction_ok && price <= upper && price >= lower
    }

    /// Current limit state (diagnostics; `None` = unrestricted/unconfigured).
    pub fn state(&self, instrument_id: u32) -> Option<LimitState> {
        self.states.get(&instrument_id).copied()
    }

    /// Clear the limit state (volatility auction completed).
    pub fn clear_state(&mut self, instrument_id: u32) {
        self.states.remove(&instrument_id);
    }
}

impl Default for LuldEngine {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tier_band_widths() {
        assert_eq!(Tier::Tier1.band_bps(), 500);
        assert_eq!(Tier::Tier2.band_bps(), 1_000);
        assert_eq!(Tier::Tier3.band_bps(), 2_000);
    }

    #[test]
    fn band_computation_tier1() {
        let mut e = LuldEngine::new();
        e.set_tier(1, Tier::Tier1);
        e.set_reference(1, 10_000);
        // ±5% of 10_000 = ±500.
        assert_eq!(e.band(1), Some((10_500, 9_500)));
    }

    #[test]
    fn band_computation_tier2_tier3() {
        let mut e = LuldEngine::new();
        e.set_tier(2, Tier::Tier2);
        e.set_reference(2, 10_000);
        assert_eq!(e.band(2), Some((11_000, 9_000)));
        e.set_tier(3, Tier::Tier3);
        e.set_reference(3, 10_000);
        assert_eq!(e.band(3), Some((12_000, 8_000)));
    }

    #[test]
    fn unconfigured_instrument_no_band() {
        let e = LuldEngine::new();
        assert_eq!(e.band(99), None);
        let mut e2 = LuldEngine::new();
        e2.set_tier(99, Tier::Tier1);
        assert_eq!(e2.band(99), None); // no reference yet
    }

    #[test]
    fn inside_band_trade_unrestricted() {
        let mut e = LuldEngine::new();
        e.set_tier(1, Tier::Tier1);
        e.set_reference(1, 10_000);
        assert_eq!(e.evaluate_trade(1, 10_200), None);
        assert_eq!(e.state(1), Some(LimitState::None));
    }

    #[test]
    fn upper_breach_enters_limit_up() {
        let mut e = LuldEngine::new();
        e.set_tier(1, Tier::Tier1);
        e.set_reference(1, 10_000);
        // At the band edge → limit-up.
        assert_eq!(e.evaluate_trade(1, 10_500), Some(LimitState::LimitUp));
        assert_eq!(e.state(1), Some(LimitState::LimitUp));
    }

    #[test]
    fn lower_breach_enters_limit_down() {
        let mut e = LuldEngine::new();
        e.set_tier(1, Tier::Tier1);
        e.set_reference(1, 10_000);
        assert_eq!(e.evaluate_trade(1, 9_400), Some(LimitState::LimitDown));
        assert_eq!(e.state(1), Some(LimitState::LimitDown));
    }

    #[test]
    fn limit_up_blocks_buys_allows_sells() {
        let mut e = LuldEngine::new();
        e.set_tier(1, Tier::Tier1);
        e.set_reference(1, 10_000);
        e.evaluate_trade(1, 10_500); // limit-up
                                     // A buy order is blocked…
        assert!(!e.order_allowed(1, 10_400, true));
        // …a sell inside the band is allowed…
        assert!(e.order_allowed(1, 10_400, false));
        // …but a sell priced outside the band is blocked too.
        assert!(!e.order_allowed(1, 9_000, false));
    }

    #[test]
    fn limit_down_blocks_sells_allows_buys() {
        let mut e = LuldEngine::new();
        e.set_tier(1, Tier::Tier1);
        e.set_reference(1, 10_000);
        e.evaluate_trade(1, 9_400); // limit-down
        assert!(!e.order_allowed(1, 9_600, false)); // sell blocked
        assert!(e.order_allowed(1, 9_600, true)); // buy allowed
        assert!(!e.order_allowed(1, 10_900, true)); // buy outside band blocked
    }

    #[test]
    fn state_clears_after_auction() {
        let mut e = LuldEngine::new();
        e.set_tier(1, Tier::Tier1);
        e.set_reference(1, 10_000);
        e.evaluate_trade(1, 10_500);
        assert!(e.state(1).is_some());
        e.clear_state(1);
        assert!(e.state(1).is_none());
        // Unrestricted again.
        assert!(e.order_allowed(1, 10_200, true));
    }

    #[test]
    fn fresh_state_allows_everything_in_band() {
        let mut e = LuldEngine::new();
        e.set_tier(1, Tier::Tier1);
        e.set_reference(1, 10_000);
        // No breach recorded yet: both sides allowed inside the band.
        assert!(e.order_allowed(1, 10_200, true));
        assert!(e.order_allowed(1, 9_800, false));
    }

    #[test]
    fn in_band_trade_releases_the_limit_state() {
        let mut e = LuldEngine::new();
        e.set_tier(1, Tier::Tier2);
        e.set_reference(1, 10_000);
        e.evaluate_trade(1, 11_200); // outside the ±10% band → limit-up
        assert_eq!(e.state(1), Some(LimitState::LimitUp));
        // A later in-band trade indicates pressure passed → state released.
        e.evaluate_trade(1, 10_200);
        assert_eq!(e.state(1), Some(LimitState::None));
        // Buys allowed again inside the band.
        assert!(e.order_allowed(1, 10_200, true));
    }

    #[test]
    fn per_instrument_isolation() {
        let mut e = LuldEngine::new();
        e.set_tier(1, Tier::Tier1);
        e.set_reference(1, 10_000);
        e.set_tier(2, Tier::Tier1);
        e.set_reference(2, 20_000);
        e.evaluate_trade(1, 10_500); // instrument 1 limit-up
                                     // Instrument 2 is unaffected.
        assert_eq!(e.state(2), None);
        assert!(e.order_allowed(2, 20_200, true));
        assert!(!e.order_allowed(1, 10_200, true));
    }

    #[test]
    fn reference_price_changes_band() {
        let mut e = LuldEngine::new();
        e.set_tier(1, Tier::Tier1);
        e.set_reference(1, 10_000);
        assert_eq!(e.band(1), Some((10_500, 9_500)));
        e.set_reference(1, 20_000);
        assert_eq!(e.band(1), Some((21_000, 19_000)));
    }
}
