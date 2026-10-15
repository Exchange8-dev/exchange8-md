// SPDX-FileCopyrightText: Copyright Logically Magic Inc. <hello@logicallymagic.com>
// SPDX-FileCopyrightText: Copyright Hamid Jahanzad <Hamid@Jahanzad.com>
// SPDX-FileCopyrightText: Copyright Contributors to the Exchange8:MD
// SPDX-License-Identifier: Apache-2.0

//! Exchange8:MD snapshot API + SSE delta shapes (see services/md/DESIGN.md §2).
//!
//! Market-data design §2: app-grade feeds are SSE deltas + snapshot served
//! through the BFF subscription. The snapshot is the client's join point:
//! `GET /v1/snapshot/{instrument}` returns the full displayed book with the
//! MD journal sequence; afterwards the client applies SSE deltas from that
//! sequence. Gap between snapshot seq and first received delta → resync.
//!
//! Pure core: `SnapshotBuilder` consumes the same ITCH-out deltas the book
//! mapper produces and maintains a compact per-instrument snapshot state;
//! `Snapshot` is the wire shape; `SseDelta` is the SSE event shape.
//!
//! Design: `design/sentin8-exchange8-market-data-technical-design.md` §2, §6.

/// One price level in a snapshot (displayed only).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SnapshotLevel {
    pub price_raw: i128,
    pub qty: u64,
}

/// Instrument snapshot (the SSE join point).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub instrument_id: u32,
    pub book_id: u8,
    /// MD journal sequence at snapshot time — the client's resync cursor.
    pub seq: u64,
    /// Displayed bids, best first (desc).
    pub bids: Vec<SnapshotLevel>,
    /// Displayed asks, best first (asc).
    pub asks: Vec<SnapshotLevel>,
    /// Last trade px (raw) if any today.
    pub last_trade_raw: Option<i128>,
    /// Total volume traded today.
    pub total_volume: u64,
}

impl Snapshot {
    /// Best bid/offer as raw pairs (None when side empty).
    pub fn bbo(&self) -> (Option<(i128, u64)>, Option<(i128, u64)>) {
        (
            self.bids.first().map(|l| (l.price_raw, l.qty)),
            self.asks.first().map(|l| (l.price_raw, l.qty)),
        )
    }

    /// JSON body for the snapshot endpoint (BFF-serializable shape).
    pub fn to_json(&self) -> String {
        let bids: Vec<String> = self
            .bids
            .iter()
            .map(|l| format!("{{\"p\":{},\"q\":{}}}", l.price_raw, l.qty))
            .collect();
        let asks: Vec<String> = self
            .asks
            .iter()
            .map(|l| format!("{{\"p\":{},\"q\":{}}}", l.price_raw, l.qty))
            .collect();
        format!(
            "{{\"instrument_id\":{},\"book_id\":{},\"seq\":{},\"bids\":[{}],\"asks\":[{}],\"last_trade\":{},\"total_volume\":{}}}",
            self.instrument_id,
            self.book_id,
            self.seq,
            bids.join(","),
            asks.join(","),
            self.last_trade_raw.map(|p| p.to_string()).unwrap_or_else(|| "null".into()),
            self.total_volume
        )
    }
}

/// SSE delta event (one per book mutation; `seq` is the client cursor).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SseDelta {
    pub seq: u64,
    pub instrument_id: u32,
    pub kind: DeltaKind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeltaKind {
    /// Level added or volume increased at `price_raw` by `qty`.
    Add {
        side_bid: bool,
        price_raw: i128,
        qty: u64,
    },
    /// Level volume decreased by `qty` (0 → level removed).
    Reduce {
        side_bid: bool,
        price_raw: i128,
        qty: u64,
    },
    /// Order removed from the displayed book.
    Delete { side_bid: bool, price_raw: i128 },
    /// Trade print: qty at price.
    Trade { price_raw: i128, qty: u64 },
}

impl SseDelta {
    /// SSE event payload (id = seq for Last-Event-ID resumption).
    pub fn to_sse(&self) -> String {
        let data = match &self.kind {
            DeltaKind::Add {
                side_bid,
                price_raw,
                qty,
            } => {
                format!(
                    "add {} {} {}",
                    if *side_bid { 'B' } else { 'S' },
                    price_raw,
                    qty
                )
            }
            DeltaKind::Reduce {
                side_bid,
                price_raw,
                qty,
            } => {
                format!(
                    "reduce {} {} {}",
                    if *side_bid { 'B' } else { 'S' },
                    price_raw,
                    qty
                )
            }
            DeltaKind::Delete {
                side_bid,
                price_raw,
            } => {
                format!("delete {} {}", if *side_bid { 'B' } else { 'S' }, price_raw)
            }
            DeltaKind::Trade { price_raw, qty } => {
                format!("trade {} {}", price_raw, qty)
            }
        };
        format!(
            "id: {}\nevent: delta\ndata: {{\"i\":{},\"d\":\"{}\"}}",
            self.seq, self.instrument_id, data
        )
    }
}

/// Maintains snapshot state for one instrument from ME events and produces
/// both the snapshot and the SSE delta stream.
///
/// Sequence contract: every applied event increments `seq` by exactly 1;
/// the snapshot carries the seq of the last applied event. A client whose
/// Last-Event-ID < snapshot.seq - (deltas it has) must resync.
pub struct SnapshotBuilder {
    instrument_id: u32,
    book_id: u8,
    seq: u64,
    bids: std::collections::BTreeMap<i128, u64>,
    asks: std::collections::BTreeMap<i128, u64>,
    last_trade_raw: Option<i128>,
    total_volume: u64,
    /// Recent deltas (bounded ring for the SSE replay window).
    recent: std::collections::VecDeque<SseDelta>,
    replay_window: usize,
}

impl SnapshotBuilder {
    pub fn new(instrument_id: u32, book_id: u8, replay_window: usize) -> Self {
        SnapshotBuilder {
            instrument_id,
            book_id,
            seq: 0,
            bids: std::collections::BTreeMap::new(),
            asks: std::collections::BTreeMap::new(),
            last_trade_raw: None,
            total_volume: 0,
            recent: std::collections::VecDeque::new(),
            replay_window,
        }
    }

    fn bump(&mut self, kind: DeltaKind) {
        self.seq += 1;
        let d = SseDelta {
            seq: self.seq,
            instrument_id: self.instrument_id,
            kind,
        };
        self.recent.push_back(d);
        while self.recent.len() > self.replay_window {
            self.recent.pop_front();
        }
    }

    /// ME OrderBooked (displayed only — hidden/odd are stripped upstream).
    pub fn on_booked(&mut self, side_bid: bool, price_raw: i128, qty: u64) {
        let m = if side_bid {
            &mut self.bids
        } else {
            &mut self.asks
        };
        m.entry(price_raw).and_modify(|v| *v += qty).or_insert(qty);
        self.bump(DeltaKind::Add {
            side_bid,
            price_raw,
            qty,
        });
    }

    /// Execution against displayed liquidity: reduce the level; remove at 0.
    pub fn on_executed(&mut self, side_bid: bool, price_raw: i128, qty: u64) {
        let m = if side_bid {
            &mut self.bids
        } else {
            &mut self.asks
        };
        let removed = match m.get_mut(&price_raw) {
            Some(v) => {
                let new = v.saturating_sub(qty);
                *v = new;
                new == 0
            }
            None => false,
        };
        if removed {
            m.remove(&price_raw);
            self.bump(DeltaKind::Delete {
                side_bid,
                price_raw,
            });
        } else {
            self.bump(DeltaKind::Reduce {
                side_bid,
                price_raw,
                qty,
            });
        }
        // Trade print regardless of side.
        self.last_trade_raw = Some(price_raw);
        self.total_volume += qty;
        self.bump(DeltaKind::Trade { price_raw, qty });
    }

    /// Cancel/remove of displayed qty (partial or full).
    pub fn on_removed(&mut self, side_bid: bool, price_raw: i128, qty: u64) {
        let m = if side_bid {
            &mut self.bids
        } else {
            &mut self.asks
        };
        let removed = match m.get_mut(&price_raw) {
            Some(v) => {
                let new = v.saturating_sub(qty);
                *v = new;
                new == 0
            }
            None => false,
        };
        if removed {
            m.remove(&price_raw);
            self.bump(DeltaKind::Delete {
                side_bid,
                price_raw,
            });
        } else {
            self.bump(DeltaKind::Reduce {
                side_bid,
                price_raw,
                qty,
            });
        }
    }

    /// Reprice (cancel-replace): remove old, add new.
    pub fn on_replaced(&mut self, side_bid: bool, old_raw: i128, new_raw: i128, qty: u64) {
        self.on_removed(side_bid, old_raw, qty);
        self.on_booked(side_bid, new_raw, qty);
    }

    /// Current snapshot.
    pub fn snapshot(&self) -> Snapshot {
        let bids = self
            .bids
            .iter()
            .rev()
            .map(|(p, q)| SnapshotLevel {
                price_raw: *p,
                qty: *q,
            })
            .collect();
        let asks = self
            .asks
            .iter()
            .map(|(p, q)| SnapshotLevel {
                price_raw: *p,
                qty: *q,
            })
            .collect();
        Snapshot {
            instrument_id: self.instrument_id,
            book_id: self.book_id,
            seq: self.seq,
            bids,
            asks,
            last_trade_raw: self.last_trade_raw,
            total_volume: self.total_volume,
        }
    }

    /// Deltas after `from_seq` (exclusive) up to now — the SSE replay for a
    /// reconnecting client with Last-Event-ID. Empty when the client is
    /// current; the caller detects a too-old cursor (first delta seq >
    /// from_seq + 1) and forces a snapshot resync.
    pub fn deltas_after(&self, from_seq: u64) -> Vec<SseDelta> {
        self.recent
            .iter()
            .filter(|d| d.seq > from_seq)
            .cloned()
            .collect()
    }

    /// Client resync decision: true when the replay window no longer covers
    /// `from_seq` (client must take a fresh snapshot).
    pub fn needs_resync(&self, from_seq: u64) -> bool {
        match self.recent.front() {
            Some(first) => first.seq > from_seq + 1 || from_seq > self.seq,
            None => from_seq < self.seq,
        }
    }

    /// Current sequence (diagnostics).
    pub fn seq(&self) -> u64 {
        self.seq
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_starts_empty() {
        let b = SnapshotBuilder::new(3, 0, 128);
        let s = b.snapshot();
        assert_eq!(s.bbo(), (None, None));
        assert_eq!(s.seq, 0);
        assert_eq!(s.total_volume, 0);
    }

    #[test]
    fn booked_builds_levels_and_bbo() {
        let mut b = SnapshotBuilder::new(3, 0, 128);
        b.on_booked(true, 9_900, 100);
        b.on_booked(true, 9_900, 200);
        b.on_booked(true, 9_850, 400);
        b.on_booked(false, 10_100, 300);
        let s = b.snapshot();
        // Best bid 9_900 qty 300; best ask 10_100 qty 300.
        assert_eq!(s.bbo(), (Some((9_900, 300)), Some((10_100, 300))));
        // Depth ordered: bids desc, asks asc.
        assert_eq!(s.bids[0].price_raw, 9_900);
        assert_eq!(s.bids[1].price_raw, 9_850);
        assert_eq!(s.bids.len(), 2);
        assert_eq!(s.asks.len(), 1);
        // Seq advanced once per event.
        assert_eq!(s.seq, 4);
    }

    #[test]
    fn executed_reduces_and_prints_trade() {
        let mut b = SnapshotBuilder::new(3, 0, 128);
        b.on_booked(false, 10_100, 300);
        // Aggressor buys; the resting ask is consumed — reduce side_bid=false.
        b.on_executed(false, 10_100, 300);
        let s = b.snapshot();
        assert_eq!(s.asks.len(), 0); // level removed
        assert_eq!(s.last_trade_raw, Some(10_100));
        assert_eq!(s.total_volume, 300);
        // Deltas: add + reduce/delete + trade.
        let deltas = b.deltas_after(0);
        assert_eq!(deltas.len(), 3); // add, delete, trade
        assert_eq!(
            deltas[2].kind,
            DeltaKind::Trade {
                price_raw: 10_100,
                qty: 300
            }
        );
    }

    #[test]
    fn replaced_moves_the_level() {
        let mut b = SnapshotBuilder::new(3, 0, 128);
        b.on_booked(true, 9_900, 100);
        b.on_replaced(true, 9_900, 9_950, 100);
        let s = b.snapshot();
        assert_eq!(s.bids.len(), 1);
        assert_eq!(s.bids[0].price_raw, 9_950);
    }

    #[test]
    fn sse_delta_format() {
        let mut b = SnapshotBuilder::new(3, 0, 128);
        b.on_booked(true, 9_900, 100);
        let d = &b.deltas_after(0)[0];
        let sse = d.to_sse();
        assert!(sse.starts_with("id: 1\nevent: delta\n"));
        assert!(sse.contains("\"i\":3"));
        assert!(sse.contains("add B 9900 100"));
    }

    #[test]
    fn snapshot_json_shape() {
        let mut b = SnapshotBuilder::new(3, 0, 128);
        b.on_booked(true, 9_900, 100);
        b.on_booked(false, 10_100, 300);
        let json = b.snapshot().to_json();
        assert!(json.contains("\"instrument_id\":3"));
        assert!(json.contains("\"seq\":2"));
        assert!(json.contains("\"p\":9900,\"q\":100"));
        assert!(json.contains("\"last_trade\":null"));
    }

    #[test]
    fn replay_window_and_resync() {
        let mut b = SnapshotBuilder::new(3, 0, 4);
        for i in 0..10 {
            b.on_booked(true, 9_900 + i as i128, 100);
        }
        // Window 4: only the last 4 deltas are replayable.
        assert_eq!(b.deltas_after(0).len(), 4);
        // Client at seq 6 → replay covers 7..=10 → no resync.
        assert!(!b.needs_resync(6));
        // Client at seq 2 → first replayable is 7 → resync needed.
        assert!(b.needs_resync(2));
        // Client ahead of us (stale state) → resync.
        assert!(b.needs_resync(99));
    }

    #[test]
    fn deltas_after_current_is_empty() {
        let mut b = SnapshotBuilder::new(3, 0, 128);
        b.on_booked(true, 9_900, 100);
        let seq = b.seq();
        assert!(b.deltas_after(seq).is_empty());
        assert!(!b.needs_resync(seq));
    }
}
