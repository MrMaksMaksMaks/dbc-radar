# DBC Radar

**Risk and origin check for Meteora DBC launches — before the first buy.**

Anyone can launch a token on Meteora's Dynamic Bonding Curve (DBC) under their own *config*: the rules for who gets the liquidity after graduation, how much of it is locked, and how much supply goes to a "leftover receiver". Buyers never see those rules. They see a token with volume that "graduated" to a Meteora pool.

DBC Radar reads the rules from chain, watches what happens in the pools and answers two questions about any launch:

1. **What does this config allow the creator to do after graduation?**
2. **Who is behind the launch, and is the trading real?**

It is available as a Telegram bot: [@DBC_Radar_bot](https://t.me/DBC_Radar_bot).

## What we see on mainnet

Over 24 hours (October 2026) the collector recorded about **6,000 DBC launches**:

| Verdict | Meaning | Share of launches |
|---|---|---|
| 🔴 **RED** | synthetic launches: trading is dominated by the creator and wallets linked to it | ~33% |
| 🔴 **RED-LINK** | linked by addresses to a synthetic-launch operator | ~1% |
| 🟠 **AMBER** | the config allows pulling liquidity and dumping leftover supply; no abuse observed yet | <1% |
| ⚪ **SELF-GRAD** | instant or self-funded graduation, no real market on the curve | ~32% |
| 🟢 **GREEN** | no red flags in the config or in observed trading | ~34% |

- In the pools of RED operators, **98% of the volume traded on the bonding curve came from the creator and linked wallets**.
- One operator ran **1,400+ launches through a single config**. Several unrelated operators run the same playbook: farms of about **200 wallets**, tokens fanned out from the creator in identical amounts and sold back in one trade, graduation in about **70 seconds**, liquidity withdrawn right after.
- All of this is visible from the config account and the first trades — **before tools that need trading history** can see it.

Operator addresses are intentionally not published here.

## Architecture

```
Solana mainnet
   │  logsSubscribe from several websocket sources + direct polling of watched operators
   ▼
dbc-collector ─► SQLite: pools, decoded DBC swap events, graduations, config accounts
   │
   ▼
dbc-replay  ─► risk, operator clusters, templates, activity reports, bit-exact replay
   │  risk --json
   ▼
dbc-radar-bot ─► Telegram: checks, latest risky launches, stats, alerts
```

| Component | Path | What it does |
|---|---|---|
| Collector | `src/` | discovers DBC pools, decodes swap / graduation events, stores config accounts, builds per-pool evidence reports |
| Engine | `replay/` | risk scoring, farm detection, operator clusters, parameter templates, activity reports, replay and counterfactuals |
| Bot | `bot/` | Telegram interface on top of the engine, with on-chain lookup for anything not in the database |

## How the verdict is made

Two independent axes, so "what the config allows" is never confused with "what actually happened".

**Capability** — read from the config account, known before the first buy:
- share of post-migration liquidity the creator or partner can withdraw at once, and how long vesting lasts;
- share of supply sent to the leftover receiver, and whether that receiver is a launchpad platform address (shared by many configs) or the operator's own;
- migration fee, mint authority, transfer hooks, instant graduation (migration threshold ≈ 0).

**Evidence** — observed in tracked pools:
- **linked volume**: share of curve volume from the creator, fan-out sellers and the operator's wallet farm;
- **fan-out**: wallets that sell tokens they never bought in the pool;
- **wallet farm**: wallets that trade in at least 30% of a config's pools and have at least 80% of their activity there (this separates a farm from generic bots that buy every new token);
- **constant first buyers**, **identical opening buys**, **launch-to-migration time**, single-creator configs.

Weights were calibrated on real configs: Meteora's default Invent template and launchpads that lock all liquidity come out GREEN.

**Operators and templates.** A *template* is a fingerprint of all economic config fields except addresses. An *operator cluster* links configs that share a creator, fee claimer, leftover receiver or at least three fan-out wallets. A new config of a known operator gets **RED-LINK** before it has a single trade.

## Telegram bot

Send a token, pool or config address — or a Solscan, DexScreener, Jupiter or Meteora link — and get the verdict with its reasons. Buttons open the config report, the operator cluster and the config's recent launches.

- **Anything, not only what the collector saw.** Unknown addresses are resolved on-chain: a DBC pool account gives its config and creator; a token's earliest transaction is its pool creation; Meteora DAMM v2 and DLMM pool addresses are resolved to their token. The result is added to the database and scored.
- **Other launchpads.** Tokens from pump.fun, Raydium LaunchLab or Moonshot are recognised and the bot says so.
- **Commands:** `/start`, `/check <address>`, `/latest` (latest RED / AMBER launches), `/stats`, `/how`.
- **Alerts (optional):** posts new RED / RED-LINK / AMBER configs and a periodic digest to a channel.

## Running it

Requires Rust ≥ 1.91 and SQLite. Build and run everything from the repository root, where `.env` and the database live.

```bash
git clone https://github.com/MrMaksMaksMaks/dbc-radar.git && cd dbc-radar
cp .env.example .env                                   # fill in RPC / websocket URLs and the bot token
cargo build --release                                  # collector
(cd replay && cargo build --release)                   # engine (first build pulls the DBC program crate)
(cd bot && cargo build --release)                      # bot

./target/release/dbc-collector                         # keep running (systemd recommended)
./bot/target/release/dbc-radar-bot                     # keep running
```

### Engine commands

```bash
cd replay
./target/release/dbc-replay ../dbc.sqlite risk                   # all configs, grouped by verdict
./target/release/dbc-replay ../dbc.sqlite risk <config-prefix>   # full report for one config
./target/release/dbc-replay ../dbc.sqlite risk --json            # machine-readable (used by the bot)
./target/release/dbc-replay ../dbc.sqlite clusters [<prefix>]    # operator clusters
./target/release/dbc-replay ../dbc.sqlite templates              # parameter templates shared by several configs
./target/release/dbc-replay ../dbc.sqlite impact [--since-hours N]   # linked vs external activity per RED operator
./target/release/dbc-replay ../dbc.sqlite <pool-prefix> [--cf]   # bit-exact replay of a pool, optional counterfactual fees
```

Per-pool evidence report (opening buy, where sellers' tokens came from, sells back into the pool, graduation, exit), with Solscan links:

```bash
./target/release/dbc-collector trace <pool-prefix> [--csv solscan_defi_export.csv] > trace.md
```

### Configuration (`.env`)

| Variable | Used by | Meaning |
|---|---|---|
| `RPC_URL` | collector, bot | HTTP RPC endpoint |
| `WS_URLS` | collector | comma-separated websocket endpoints for pool discovery |
| `WS_STALL_SECS` | collector | reconnect a source that is silent this long (default 20) |
| `WATCH_ADDRESSES` | collector | operator addresses polled directly; their pools are always tracked |
| `SAMPLE_EVERY` | collector | track every N-th new pool (1 = all) |
| `CONFIG_ALLOWLIST` | collector | configs whose pools are always tracked |
| `TRACK_HOURS` | collector | how long to collect swaps for each pool |
| `RPC_RPS` | collector | HTTP request rate limit |
| `DB_PATH` | all | SQLite database (default `dbc.sqlite`) |
| `TELEGRAM_BOT_TOKEN` | bot | token from @BotFather |
| `TELEGRAM_BOT_USERNAME` | bot | used for links from the alert channel |
| `REPLAY_BIN` | bot | path to `dbc-replay` (default `replay/target/release/dbc-replay`) |
| `REFRESH_SECS` | bot | how often verdicts are recomputed (default 300) |
| `HISTORY_RPC_URL` | bot | RPC with full transaction history, only for finding the pool of an older token |
| `ALERT_CHAT`, `DIGEST_SECS` | bot | alert channel (`@name` or id) and digest period |

## Validation

- The replay engine uses the DBC program crate itself (`update_pre_swap → get_swap_result_* → apply_swap_result`) and matched **741 of 741** recorded swaps of a real pool to the lamport, including the order of transactions within a slot.
- Evidence reports traced **173 of 173** fan-out sellers in six launches of one operator directly to the creator's transfers, each of the same size.
- The collector's discovery coverage is measured continuously against operators whose launches are polled directly.

## Limitations

- Trading evidence comes from sampled pools; config capability is computed for every config.
- An operator that uses fresh wallets for every launch would weaken the farm signals; config capability and address links still apply.
- Post-migration liquidity withdrawal is confirmed through transaction traces, not yet through decoded DAMM v2 events.
- Verdicts are heuristic analysis of public on-chain data, not financial advice.

## Roadmap

- Public API for terminals, bots and launchpads (`/check`, `/latest`, `/stats` as JSON).
- Decoding DAMM v2 events to confirm liquidity withdrawal automatically.
- A "clean config" attestation that honest launchpads can show their users.

## License

MIT
