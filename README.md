# DBC Radar

**Risk and origin check for Meteora DBC launches — before the first buy.**

A large share of new Solana tokens launch through Meteora's Dynamic Bonding Curve (DBC). Every launch follows the rules of a *config* account: who gets the liquidity after graduation, how much of it is locked, how much supply goes to a "leftover receiver", which fees apply. Configs are permissionless, so anyone can write their own rules, and the buyer never sees them.

DBC Radar reads those rules from chain, watches what happens in the pools, and answers two questions for any launch:

1. **What does this config let the creator do after graduation?**
2. **Who is behind this launch, and is the trading real?**

## What we found on mainnet

In a sample of ~5,700 DBC pools collected over a few days in October 2026:

| Verdict | Pools | Share |
|---|---|---|
| **RED** — synthetic launches (trading dominated by the creator and linked wallets) and **RED-LINK** (linked to such operators) | ~2,000 | ~35% |
| **SELF-GRAD** — instant / self-funded graduation, no real bonding-curve market | ~1,900 | ~34% |
| **AMBER** — config allows pulling liquidity and dumping leftover supply, no abuse observed yet | ~40 | ~1% |
| **GREEN** — no red flags | ~1,800 | ~31% |

- In the pools of RED operators, **98.4% of the 37,284 SOL traded on the curve came from the creator and wallets linked to it.** Outside wallets were a small minority.
- One operator ran **1,400+ launches through a single config**, each following the same scripted cycle: a fixed opening buy, tokens fanned out to the same pool of wallets in identical amounts, graduation in about 70 seconds, liquidity withdrawn and leftover supply sold within the same second.
- All of this is visible from the config account and the first trades, i.e. **before tools that rely on trading history** can see it.

Operator addresses are intentionally not published in this repository.

## How it works

```
 Solana mainnet
      │  logsSubscribe (several websocket sources) + direct polling of watched operators
      ▼
 dbc-collector ──► SQLite: pools, decoded DBC swap events, graduations, config accounts
      │
      ▼
 dbc-replay
   ├─ engine    bit-exact replay of pools through the DBC program's own math
   ├─ risk      capability score (config) + evidence score (behaviour) → verdict
   ├─ farm      creator-linked wallets, synthetic-volume share, external participants
   ├─ cluster   operator clusters (shared creators / fee & leftover receivers / wallets)
   │            and parameter templates
   └─ impact    activity report per operator
```

### Collector (`dbc-collector`)
- Discovers new DBC pools via `logsSubscribe` from several websocket sources in parallel (deduplicated, with a stall watchdog and automatic reconnects), plus direct polling of watched operator addresses for full coverage.
- Decodes DBC event-CPI payloads (`EvtInitializePool`, `EvtSwap2`, `EvtCurveComplete`, including transfer-hook variants) and stores raw config accounts.
- `trace <pool>` builds a transaction-level evidence report for one pool (opening buy, where sellers' tokens came from, sells back into the pool, graduation, exit), with Solscan links.

### Replay engine
- Uses the DBC program crate directly (`update_pre_swap → get_swap_result_* → apply_swap_result`), so results match on-chain events to the lamport. Validated on 741/741 recorded swaps of a real pool, including reconstruction of transaction order within a slot.
- Counterfactual mode: replay the same trades under a different fee schedule, validated against the program's own config rules.

### Risk scoring (`risk`)
Two axes instead of one number:

- **Capability** — what the config *allows*, known before the first trade: unlocked post-migration liquidity for the creator or partner, leftover supply and who receives it (with platform custody detection), vesting, migration fee, mint authority, transfer hooks, instant graduation.
- **Evidence** — what was *observed* in tracked pools: share of curve volume from creator-linked wallets, fan-out sellers (wallets selling tokens they never bought in the pool), recurring config-specific wallets, constant first buyers, fixed opening buys, launch-to-migration time, single-creator configs.

Weights were calibrated on real configs: Meteora's default Invent template and launchpads that lock 100% of liquidity score GREEN.

### Operators and templates (`clusters`, `templates`)
- **Template** = fingerprint of all economic config fields except addresses. Same template, same rules.
- **Operator cluster** = configs linked by a shared creator, fee claimer, leftover receiver or ≥3 shared wallets. A new config of a known operator is flagged **RED-LINK** before it has a single trade.

## Quick start

Requires Rust ≥ 1.91 and SQLite.

```bash
git clone https://github.com/MrMaksMaksMaks/dbc-radar.git
cd dbc-radar
cp .env.example .env        # set RPC / websocket URLs
cargo build --release
./target/release/dbc-collector
```

```bash
cd replay
cargo build --release       # first build pulls the DBC program crate and Anchor
./target/release/dbc-replay ../dbc.sqlite risk                  # all configs, by verdict
./target/release/dbc-replay ../dbc.sqlite risk <config-prefix>  # full report for one config
./target/release/dbc-replay ../dbc.sqlite clusters              # operator clusters
./target/release/dbc-replay ../dbc.sqlite templates             # shared parameter templates
./target/release/dbc-replay ../dbc.sqlite impact                # activity per RED operator
./target/release/dbc-replay ../dbc.sqlite risk --json           # machine-readable output
```

```bash
./target/release/dbc-collector trace <pool-prefix> [--csv solscan_defi_export.csv] > trace.md
```

### Configuration (`.env`)

| Variable | Meaning |
|---|---|
| `RPC_URL` | HTTP RPC endpoint |
| `WS_URLS` | comma-separated websocket endpoints for pool discovery |
| `WS_STALL_SECS` | reconnect if a source is silent this long (default 20) |
| `WATCH_ADDRESSES` | operator addresses polled directly; their pools are always tracked |
| `SAMPLE_EVERY` | track every N-th pool (1 = all) |
| `CONFIG_ALLOWLIST` | configs whose pools are always tracked |
| `TRACK_HOURS` | how long to collect swaps for each pool |
| `RPC_RPS` | HTTP request rate limit |

## Limitations

- Results come from sampled data (every N-th pool, plus watched operators). Discovery coverage was measured at ~93% with two websocket sources.
- A static replay cannot model how traders would react to different rules; counterfactual numbers are estimates.
- Farm detection relies on wallet recurrence and specificity; an operator using fresh wallets for every launch would be harder to catch.
- Post-migration liquidity withdrawal is currently confirmed through transaction traces, not yet through decoded DAMM v2 events.

## Roadmap

- **Telegram bot**: send a token, pool or config address and get the verdict with reasons; a channel with real-time alerts for new RED launches.
- Decoding DAMM v2 events to confirm liquidity withdrawal automatically.
- An API for terminals and launchpads, and a "clean config" attestation honest launchpads can show their users.

## License

MIT
