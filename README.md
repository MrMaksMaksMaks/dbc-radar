# DBC Radar

**A trust layer for Meteora DBC launches: see what a launch's config allows and who is behind it — before the first buy or before adding liquidity.**

Anyone can launch a token on Meteora's Dynamic Bonding Curve (DBC) under their own *config*: the rules for who gets the liquidity after graduation, how much of it is locked, and how much supply goes to a "leftover receiver". Buyers and liquidity providers never see those rules. They see a token with volume that "graduated" to a Meteora pool.

DBC Radar reads the rules from chain, watches what happens in the pools and gives a verdict for any launch — as a Telegram bot ([@DBC_Radar_bot](https://t.me/DBC_Radar_bot)) and as a public JSON API.

## Who it is for

- **Buyers** — paste a token, pool or link and see whether the launch is organic before the first buy.
- **Liquidity providers** — paste a Meteora DAMM v2 or DLMM pool address and see whether the creator can still pull unlocked liquidity or dump leftover supply into it.
- **Launchpads built on DBC** — show users that their configs are clean, and spot operators abusing their platform.
- **Terminals, wallets and bots** — one API call per token, the same verdict as in the bot.

## What it checks

1. **What does this config allow the creator to do after graduation?** Read from the config account, so it is known in the second the config is created — before anyone buys.
2. **Who is behind the launch, and is the trading real?** Trading in the pools is compared with the creator's transfers and with wallets that reappear across the config's launches. New configs are linked to known operators through shared addresses.

## Honest launches pass

The verdict separates good configs from bad ones; it is not a verdict on DBC. Weights were calibrated on real configs: **Meteora's default Invent template and launchpads that lock all liquidity come out GREEN.** A risky setting alone gives AMBER, not RED — RED needs observed evidence that trading is dominated by the creator and linked wallets.

| Verdict | Meaning |
|---|---|
| 🟢 **GREEN** | no red flags in the config or in observed trading |
| ⚪ **SELF-GRAD** | instant or self-funded graduation, no real market on the curve |
| 🟠 **AMBER** | the config allows pulling liquidity and dumping leftover supply; no abuse observed yet |
| 🔴 **RED** | synthetic launches: trading is dominated by the creator and wallets linked to it |
| 🔴 **RED-LINK** | a new config linked by addresses to a synthetic-launch operator — flagged before its first trade |

## What it finds on mainnet

The collector watches DBC launches on mainnet around the clock. Among them it finds a recurring pattern of synthetic launches:

- the creator funds a farm of wallets that trade the token with each other on the bonding curve, so the curve completes within about a minute;
- tokens are fanned out from the creator to many wallets in identical amounts and sold back into the pool;
- the config gives the creator most of the post-migration liquidity unlocked and sends most of the supply to the leftover receiver, and the liquidity is withdrawn right after migration;
- some operators run hundreds of launches through a single config, and unrelated operators use the same config template and farms of the same size.

External buyers in these pools are few: the volume is mostly the operator trading with itself. That volume still makes the launches look active in feeds and terminals — which is exactly what a buyer or LP provider needs to see through.

All of this is visible from the config account and the first trades, **before tools that need trading history** can see it. Operator addresses are intentionally not published.

<!-- Mainnet statistics for the final submission are added after the final collection (Oct 10–11). -->

## Telegram bot

Send a token, pool or config address — or a Solscan, DexScreener, Jupiter or Meteora link — and get the verdict with its reasons. Buttons open the config report, the operator cluster and the config's recent launches.

- **Anything, not only what the collector saw.** Unknown addresses are resolved on-chain: a DBC pool account gives its config and creator; a token's earliest transaction is its pool creation; Meteora DAMM v2 and DLMM pool addresses are resolved to their token. The result is added to the database and scored.
- **Other launchpads.** Tokens from pump.fun, Raydium LaunchLab or Moonshot are recognised and the bot says so.
- **Commands:** `/start`, `/check <address>`, `/latest` (latest RED / AMBER launches), `/stats`, `/how`.
- **Alerts (optional):** posts new RED / RED-LINK / AMBER configs and a periodic digest to a channel.
- Operator addresses are shown shortened; full addresses are available through the API with a key.

## Public API

The bot process also serves a JSON API with the same verdicts, for terminals, wallets, bots and launchpads.

| Endpoint | Returns |
|---|---|
| `GET /v1/check/{address}` | verdict for a token, DBC pool, config, or Meteora DAMM v2 / DLMM pool (resolved to its token) |
| `GET /v1/latest?hours=2&limit=20` | latest RED / RED-LINK / AMBER launches |
| `GET /v1/stats` | launches in the last 24 hours by verdict |
| `GET /v1/health` | service status and analysis age |

```bash
curl https://<api-host>/v1/check/<token-or-pool-address>
```

```json
{
  "kind": "launch",
  "verdict": "RED",
  "launch": { "pool": "…", "token": "…", "config": "…",
              "created_time": 1790000000, "graduated_time": 1790000072,
              "trades": { "count": 312, "wallets": 118, "creator_opening_buy_sol": 8.057, "fan_out_sellers": 21 } },
  "config": { "verdict": "RED", "capability": 60, "evidence": 100,
              "creator_unlocked_lp_pct": 89, "leftover_to_receiver_pct": 90.0,
              "capability_flags": [ … ], "evidence_flags": [ … ], "cluster": 12, "cluster_size": 1 }
}
```

A DAMM v2 or DLMM pool address returns the verdict of its token plus `"via": {"venue": "DAMM v2", "pool": "…"}`.

**Access.**
- Without a key: everything in the index, and Meteora DAMM v2 / DLMM pool addresses (one account read). Creator, fee-claimer and leftover-receiver addresses are omitted from responses.
- With a key (`X-API-Key` header): on-chain lookup of any token or pool that is not in the index yet, and full operator addresses. An invalid key returns `401`.
- Requests are rate-limited per client IP (`API_RATE_PUBLIC`, `API_RATE_KEY`).

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

**Operators and templates.** A *template* is a fingerprint of all economic config fields except addresses. An *operator cluster* links configs that share a creator, fee claimer, leftover receiver or at least three fan-out wallets. A new config of a known operator gets **RED-LINK** before it has a single trade.

## Architecture

```
Solana mainnet
   │  logsSubscribe from several websocket sources + direct polling of watched operators
   │  + pool accounts (graduation state)
   ▼
dbc-collector ─► SQLite: pools, decoded DBC swap events, graduations, config accounts
   │
   ▼
dbc-replay  ─► risk, operator clusters, templates, activity reports, bit-exact replay
   │  risk --json
   ▼
dbc-radar-bot ─► Telegram: checks, latest risky launches, stats, alerts
              └► public JSON API
```

| Component | Path | What it does |
|---|---|---|
| Collector | `src/` | discovers DBC pools, decodes swap / graduation events, reads graduation state from pool accounts, stores config accounts, builds per-pool evidence reports |
| Engine | `replay/` | risk scoring, farm detection, operator clusters, parameter templates, activity reports, replay and counterfactuals |
| Bot + API | `bot/` | Telegram interface and public JSON API on top of the engine, with on-chain lookup for anything not in the database |

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

### Collector commands

```bash
./target/release/dbc-collector decode <signature>                    # print DBC events of one transaction
./target/release/dbc-collector trace <pool-prefix> [--csv export.csv] > trace.md   # per-pool evidence report
./target/release/dbc-collector grad-sweep <hours>                    # one-off graduation check from pool accounts
```

The evidence report covers the opening buy, where sellers' tokens came from, sells back into the pool, graduation and, with a Solscan DeFi export of the creator, the exit — with Solscan links.

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
| `GRAD_CHECK_SECS`, `GRAD_CHECK_HOURS` | collector | how often to check graduation from pool accounts, and how far back (default 120 s, 24 h) |
| `STATUS_RPC_URL`, `STATUS_RPC_RPS` | collector | RPC for that check (`getMultipleAccounts`, 100 accounts per call) and its rate limit; empty = `RPC_URL` |
| `DB_PATH` | all | SQLite database (default `dbc.sqlite`) |
| `TELEGRAM_BOT_TOKEN` | bot | token from @BotFather |
| `TELEGRAM_BOT_USERNAME` | bot | used for links from the alert channel |
| `REPLAY_BIN` | bot | path to `dbc-replay` (default `replay/target/release/dbc-replay`) |
| `REFRESH_SECS` | bot | how often verdicts are recomputed (default 300) |
| `HISTORY_RPC_URL` | bot | RPC with full transaction history, only for finding the pool of an older token |
| `ALERT_CHAT`, `DIGEST_SECS` | bot | alert channel (`@name` or id) and digest period |
| `API_BIND` | bot | API listen address, e.g. `127.0.0.1:8080` (empty = off) |
| `API_KEYS` | bot | comma-separated API keys (on-chain lookups, full addresses, higher limit) |
| `API_RATE_PUBLIC`, `API_RATE_KEY` | bot | requests per minute per IP without / with a key |

## Validation

- The replay engine uses the DBC program crate itself (`update_pre_swap → get_swap_result_* → apply_swap_result`) and matched **741 of 741** recorded swaps of a real pool to the lamport, including the order of transactions within a slot.
- Evidence reports traced **173 of 173** fan-out sellers in six launches of one operator directly to the creator's transfers, each of the same size.
- The collector's discovery coverage is measured continuously against operators whose launches are polled directly.

## Limitations

- Trading evidence comes from sampled pools; config capability is computed for every config.
- An operator that uses fresh wallets for every launch would weaken the farm signals; config capability and address links still apply.
- Post-migration liquidity withdrawal is confirmed through transaction traces, not yet through decoded DAMM v2 events.
- Graduation comes from the decoded `EvtCurveComplete` event; when the completing transaction was not collected, from the pool account's `finish_curve_timestamp`.
- Verdicts are heuristic analysis of public on-chain data, not financial advice.

## Roadmap

- A streaming feed of new risky launches for terminals (websocket / webhooks).
- Decoding DAMM v2 events to confirm liquidity withdrawal automatically.
- A "clean config" attestation that honest launchpads can show their users.

## License

MIT
