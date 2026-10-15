# BUSINESS.md: Exchange8:MD — Business Requirements & Functionality

> Audience: product owners, business analysts, and anyone evaluating what
> Exchange8:MD **does for the business**. For architecture read `README.md`;
> for wire-level detail read `DESIGN.md`.

## 1. Business Purpose

- **Give every market participant the same picture of the market.** Exchange8:MD
  turns matching-engine activity into the venue's public market data: the
  consolidated depth book, the trade tape, last-trade statistics, and market
  status — one authoritative publication per event, for every consumer.
- **Meet exchange-grade publication obligations.** An MTF/ATS must publish
  order-book depth, trade prints (including busts and corrections), and market
  state in standardized wire formats with sequence integrity; Exchange8:MD is the
  single place those obligations are discharged.
- **Turn raw activity into usable app experiences.** Consumers of the venue's
  web and mobile apps do not speak feed protocols; Exchange8:MD provides
  snapshots joined with live deltas so a user opening a security sees the
  current book immediately and stays current afterwards.
- **Keep trading safe when markets move fast.** Volatility interruptions
  (limit-up/limit-down bands, halts into auctions) and protected-quote
  enforcement are computed and published as market data, so trading
  restrictions are visible and consistent venue-wide.
- **Run the market day as a contract, not a chore.** Preopen, the open and
  close auctions, continuous trading, halts, and session end are a single
  state machine driving what every downstream system believes about the
  trading day.

## 2. Business Scope (What the Service Provides)

| Capability | What it delivers | Business consumer |
|---|---|---|
| Consolidated depth book | A per-market, per-security mirrored book derived from engine events, with exchange display policy applied | All feed consumers, internal desks, apps |
| ITCH wire publication | Order/trade/status events encoded as ITCH messages in the venue's wire dialect | External subscribers, gateways, viewers |
| Trade tape & last-trade stats | Every execution printed once, with busts and corrections applied | Retail/institutional apps, compliance, clearing |
| Snapshots & live deltas | Join-in-a-second snapshots plus live delta streams with reconnect | App users via the platform subscription proxy |
| Bust/amend fan-out | Broken trades and corrected prints, with book volume restored consistently | Market participants, surveillance, clearing |
| Market calendar & system events | Session phases (preopen → open auction → continuous → close auction → post-close), halts/resumes, public system-event signals | All trading systems, ops |
| LULD price bands & limit states | Per-security volatility thresholds, band breaches, and restricted-trading states | Risk, ME, OMS, regulators |
| Volatility interruption | Halt-and-auction decision on sustained breaches, plus the resumption arc | ME (auction execution), ops desk |
| Protected quotes | Best protected bid/offer cache for trade-through protection | ME order protection, apps |
| Market protection meters | Order-to-trade ratio and minimum order-rest time enforcement inputs | Compliance, surveillance |
| Surveillance alert pipeline | Observations → detectors → alerts → escalated cases (severity ≥ high) | Surveillance team |
| Index publication gate | Value-continuous divisor rebalancing and change-gated index publications | Margin, funding, data vendors |
| Conformance & reconstruction | Certification check runs over captured sessions; order/trade state rebuild from journals | Ops, onboarding clients, audit |
| Crypto market data shapes | SSE/WebSocket publication shapes for 24×7 crypto markets | Crypto app users, takers |
| Admin surfaces (RFW) | Feed-session health page and LULD bands page for operators | Env/org admins |

**What the service does NOT own (business boundary):**

| Delegated concern | Owning service |
|---|---|
| Order matching, order state, auction uncross execution | Exchange8:ME |
| Trade settlement and order/trade settlement state | Exchange8:Clearing |
| Venue/external market data consumption (upstream quotes) | Exchange8:VFeeds |
| Pre-trade risk and credit gates | Exchange8:Risk |
| Order routing and smart order decisions | Exchange8:OMS / Exchange8:SOR |
| Scheduled market events (what fires when) | Sentin8:Cron |
| Identity, credentials, policy administration | Sentin8:Auth / Sentin8:Policy (Shield) |
| Durable cross-service event delivery and audit delivery | Sentin8:Bus / Sentin8:Sys |
| App delivery of subscriptions (the user-facing transport) | Sentin8:BFF |

## 3. Business Capabilities

### Market data publication

| Capability | Business rule |
|---|---|
| Book mirroring | The published book is an exact projection of the matching engine's book for that market and security; any divergence is detectable and must never become visible depth |
| Display policy | Hidden (non-displayed) orders never appear as book depth — they surface only as trade prints; odd-lot volume is excluded from displayed depth |
| Order lifecycle reporting | Additions, partial reductions, cancels, and replacements are each reported in the venue's message forms; a replacement is one report, not two |
| Trade prints | Every trade prints with a unique match number; executions against displayed depth reduce that depth immediately |
| Last & volume | The last traded price and cumulative volume per security are maintained continuously and exposed in snapshots and stats |
| Snapshot integrity | Snapshots carry the publication sequence at the moment they are taken; a client resuming from an older position can rejoin exactly from the deltas after its cursor |
| Gap detection | Sequence gaps on any published stream are detected and surfaced (alert + resync request), never silently swallowed |
| Multi-stream parity | Depth (L2), quote/trade (L1), and directory streams are sequenced independently per session, consistent with venue dialect conventions |

### Protection (market integrity)

| Capability | Business rule |
|---|---|
| LULD bands | Each security has upper/lower thresholds around a reference price (prior close or last auction price) in tiered widths; a trade at or beyond a threshold puts the security into a limit state |
| Directional restriction | In a limit-up state, buy orders are blocked; in a limit-down state, sell orders are blocked — and in both cases orders priced outside the band are rejected |
| Automatic release | A return to in-band trading releases the limit state; trading resumes unrestricted |
| Volatility interruption | A sustained band breach halts the security and routes it into a volatility auction (halt → call → uncross → resume) with a randomized call end, reproducible for ops replay |
| Trade-through protection | An order may not execute through the protected best bid/offer of a protected venue; a crossing sweep that reaches a better price is exempt |
| Order-to-trade ratio | A connection whose order submission rate massively exceeds its executed trades (beyond a grace period for new sessions) exceeds its meter — the input to order-flow restrictions |
| Minimum quote life | A displayed order must rest for a minimum time before it may be cancelled — an anti-spoofing control on quote flicker |
| Surveillance pipeline | Feed observations run through configured detectors; alerts of high severity or above are escalated into open cases in the same pipeline |

### Calendar (trading-day operations)

| Capability | Business rule |
|---|---|
| Session phases | The trading day moves through closed → preopen → opening auction → continuous → closing auction → post-close, in that order only |
| System events | Each phase change publishes the venue's system-event codes in order, so all consumers share one timeline of the day |
| Instrument status | Securities are tradable (open) only during continuous trading; during auctions they are quotation-only; outside sessions and during halts they are halted |
| Halts & resumes | An operational halt moves the session to halted semantics without ending the day; resumption returns straight to continuous trading |
| Idempotent replays | Calendar event replays outside the valid transition are ignored — re-delivering a scheduled fire cannot double-publish a phase change |
| Bust & amend handling | A busted trade retracts the original print and returns its volume to the book; a corrected trade retracts the original, restores volume, and reprints with corrected terms |

## 4. Business Events (Outbound)

| Event family | When it fires |
|---|---|
| Book deltas (`md.book`, newest-wins) | Every displayed order add/execute/reduce/delete/replace mirrored from the engine |
| Trade tape (`md.trade`, newest-wins) | Every execution print (board, cross, hidden-liquidity trade) |
| Durable market-data events (Bus) | Book/tape/state changes staged in the service's outbox for cross-service consumption |
| LULD state changes | Entering/releasing a limit state; band breach → halt decision |
| System events (`S` codes) | Trading-day phase transitions driven by the calendar |
| Bust/amend fan-outs | Clearing confirms a bust or correction |
| Index publications | Index value moved at least the configured minimum since last publication |
| Surveillance alerts/cases | Detector match; high-severity alerts auto-escalate to open cases |
| Admin audit trail | Operator actions on the policy/session/bands admin surfaces |

**Inbound (consumed):** matching-engine order/trade/status events; gateway
internal cross and instrument-state events; clearing bust/amend events;
venue-feed CBBO quote events (protected quotes); Sentin8:Cron calendar
schedule-fires; platform policy hot-reload events.

## 5. Business Rules & Invariants

1. **One picture of the market.** For any (market, security) at any instant,
   exactly one displayed book is published, derived from the engine's own
   activity — participants never see diverging books from different surfaces.
2. **A hidden order is never displayed.** Non-displayed and odd-lot liquidity
   never adds depth, whatever the path that created or removed it.
3. **Every print is unique and permanent-or-corrected.** Each execution gets
   one match number; the only way a print changes is an explicit bust
   (retraction) or amend (correction), both announced on the tape.
4. **Snapshots are consistent with deltas.** A snapshot's sequence marker is
   the exact cursor into the delta stream; a client can always detect whether
   it needs to catch up or re-join. Deltas older than the replay window force
   a fresh snapshot rather than a guessing reconstruction.
5. **Sequence gaps are never silent.** A lost event on any stream is detected
   and reported; consumers are told to resynchronize rather than trusted to
   infer missing activity.
6. **Volatility boundaries are mechanical.** A threshold state exists only
   between configured bands around a defined reference; it releases on an
   in-band trade and clears after the volatility auction completes — never by
   manual bookkeeping.
7. **Orders cannot trade through protected quotes** unless an explicit
   exception (e.g. the sweep exception) applies.
8. **The trading day is a one-way machine.** Phase changes happen only along
   valid transitions; replays and out-of-order scheduled fires cannot move the
   day backwards or double-publish.
9. **Tenant data is isolated by environment.** Every market-data record
   belongs to one environment and is invisible to any other environment's
   context, regardless of the querying role.
10. **Operator actions are auditable and durable.** An audit outage delays,
    but never drops, the audit trail; event publication persists locally and
    retries until delivered, with deduplication keyed per event.
11. **Feed credentials are revocable per consumer.** Session access uses
    per-consumer credentials held as secret references — never shared
    passwords, never inline secrets in data tables.

## 6. Target-User Roles & Use Cases

| Persona | What they do with Exchange8:MD |
|---|---|
| Retail / institutional app user | Opens a security page: gets the current depth snapshot, then live deltas; sees the last price, volume, and market state |
| External data subscriber / vendor | Connects with venue feed credentials to the multicast/ticket-based streams; replays by sequence after disconnects |
| Prop / algo trader connectivity | Consumes depth and tape programmatically; relies on sequence continuity and gap alerts for feed integrity |
| Market operations desk | Watches feed session health (sessions, sequences, lag) and LULD bands on the admin pages; intervenes on halts |
| Compliance / surveillance officer | Reviews alerts and escalated cases raised by the protection meters and detectors on trading activity |
| Risk / margin systems (automated) | Consume band states, protected quotes, and index publications for their own checks |
| Env org admin | Administers tenant-authored policy for the service's surfaces via the Shield admin API |
| Auditor / regulator | Replays the tape and market state around an incident; relies on bust/corrected prints and journal durability |

## 7. KPIs / Success Criteria

- **Publication integrity:** mirrored book matches the engine book at all
  times; zero silent sequence gaps (all gaps alert + resync).
- **Tape correctness:** every execution printed exactly once; every
  bust/amend applied exactly once in the defined order (idempotent on replay).
- **Timeliness:** deltas and tape reach co-hosted consumers in-process
  (newest-wins) and external consumers within the venue's publication budget;
  reconnecting app clients catch up from the replay window or resync
  immediately.
- **Availability:** publisher restarts resume from persisted sequence state
  with no tape loss; platform outages (Bus/audit) degrade delivery latency,
  never correctness or durability.
- **Protection effectiveness:** band breaches are detected on the print that
  crosses them; wrong-side orders blocked within the defined gate; volatility
  auctions open after every sustained breach.
- **Calendar reliability:** every trading day's session arc publishes the
  full ordered set of system events; halts/resumes publish unambiguously.
- **Operational quality:** admin pages show session and band truth matching
  the database; a live database is never required for feed correctness
  (memory-first, DB for durability and replay).
- **Security posture:** all admin/subscriber access authenticated with
  venue-issued credentials; zero plaintext secrets; policy decisions fully
  mirroring the enforced gate set.

## 8. Dependencies & Reliance

- **Exchange8:ME / Exchange8:Gateway** — the source of order, trade, cross, and instrument
  state events; without them there is nothing to publish.
- **Exchange8:Clearing** — busts and corrections (the only legitimate tape
  retractions/corrections).
- **Exchange8:VFeeds** — upstream venue quotation data underlying protected quotes.
- **Sentin8:Cron** — the trading-calendar scheduling (preopen/open/close/
  shutdown fires that drive the session machine).
- **Sentin8:Bus** — durable inbound event consumption and outbound event
  delivery (idempotent produce).
- **Sentin8:Sys** — audit intake (`POST /v1/audit`) and hostname resolution
  for the TLS plane.
- **Sentin8:Auth / Sentin8:Policy (Shield)** — identity issuance, M2M
  registration, and the enforced policy engine + admin surface.
- **Sentin8:BFF** — user-facing subscription transport for snapshots/deltas.
- **Sentin8:DB (PostgreSQL 18)** — schema `md` persistence under RLS.
- **Platform shell + Captain + Proxy/tls** — boot, drain/upgrade health,
  context headers, and post-quantum termination.

## 9. Acceptance Criteria

1. A participant opening a security view receives a snapshot whose depth,
   last price, volume, and market state match the engine's current reality,
   then stays current via deltas without gaps.
2. A reconnecting subscriber with a recent cursor resumes without a fresh
   snapshot; one with an older cursor is cleanly redirected to a fresh
   snapshot — never served fabricated history.
3. All venue wire forms (add, execute, cancel, delete, replace, trade,
   broken trade, system events) encode byte-for-byte to the platform codec
   contracts, and session replay honors per-stream sequences.
4. Hidden and odd-lot liquidity are provably absent from displayed depth,
   including across executions, cancels, and replacements.
5. A band breach triggers the defined limit state and blocks wrong-side or
   out-of-band orders; an in-band trade releases the state; a volatility
   interruption runs the halt→call→uncross→resume arc.
6. Trade-through attempts against the protected quote are rejected, with the
   sweep exception honored.
7. A bust restores the pre-trade book volume exactly; an amend leaves the
   book consistent with the corrected terms only.
8. Each trading day publishes its phase-change system events in order and
   exactly once per transition, regardless of schedule-fire replays.
9. No tenant can read another environment's market data through any surface
   (RLS-matrix verified as the probe role).
10. Killing the service mid-stream and restarting loses no published events:
    the outbox drains, the audit journal pushes, and sequences resume to
    connected consumers.
11. An operator can see live feed session health and LULD bands on the admin
    pages and act through the authorized, audited admin surfaces.
12. A captured session log passes the certification checks for logon order
    and per-order reports — an onboarding-client readiness gate.

## 10. Open Business Questions

- **App subscription topic model.** The business wants one join point per
  security for apps (snapshot + deltas); the commercial topic naming and the
  per-topic entitlement packaging for the subscription surface are to be
  finalized with the marketplace/app-store side.
- **Multi-market coverage.** Crypto and derivatives markets publish through
  SSE/fleet shapes rather than the equities wire dialects; the bundled
  product definition of "one depth surface" across market types (and pricing
  tiers for takers) is a product decision outside this unit.
- **Surveillance productization.** Which detectors run by default per
  environment, and how alerts map to user-facing case workflows, is owned
  with the surveillance product.
- **Retention commitments.** How long replay history is contractually
  available per consumer class (hours vs days) needs a business retention
  policy before it becomes a configuration default.
- **Order-flow restriction enforcement point.** The ratio and quote-life
  meters are computed here; whether blocking happens at the gateway, the
  matching engine, or as post-trade penalties is a market-policy decision
  shared with Exchange8:Gateway/Exchange8:ME.
- **Index entitlements.** Real-time index publication consumption (frequency,
  redistribution rights) is an entitlement product question for margin,
  funding, and data-vendor consumers.

---

**Design attribution:** exchange8:md and its underlying concepts were designed by Hamid Jahanzad <Hamid@Jahanzad.com>.
