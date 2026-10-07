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
| 🟢 **GREEN** | no major red flags in the config or in observed trading (minor signals are still listed) |
| ⚪ **SELF-GRAD** | instant or self-funded graduation, no real market on the curve |
| 🟠 **AMBER** | risky config: the creator or launchpad can take most of the buyers' SOL or liquidity at or after migration (unlocked liquidity with a large leftover supply, or a migration fee of 50% and more); no abuse observed yet |
| 🔴 **RED** | synthetic launches: trading is dominated by the creator and wallets linked to it |
| 🔴 **RED-LINK** | a new config linked by addresses to a synthetic-launch operator — flagged before its first trade |

## What it finds on mainnet

The collector watches DBC launches on mainnet around the clock. Besides ordinary launches, it keeps finding a few recurring patterns. We traced each of them end to end — every transaction of the DBC pool and, after graduation, of the Meteora DAMM v2 pool, with SOL counted per owner across all accounts of each transaction.

**1. Volume made by a wallet farm.** The creator and a farm of about 200 wallets trade the token with each other, so the curve completes within a minute or two. Tokens are fanned out from the creator in identical amounts and sold back into the pool. The config gives the creator most of the post-migration liquidity unlocked, and the operator withdraws it in the same second as migration. In most launches we traced, the same farm keeps trading in the DAMM v2 pool after migration — tens to hundreds of SOL of volume, for hours or days.

**2. One wallet trading through proxy signers.** Dozens of wallets sign the trades, but the SOL for every buy comes from — and every sale pays back to — one wallet that never signs. A session moves about 100 SOL of buys through the curve in two or three minutes. To a tool that looks at signers, these are dozens of independent traders.

**3. The creator sells to the first buyer.** The creator buys first with a round amount, a bot buys in the next slot, and the creator sells to it a second later. These configs set a 99% migration fee and an 85 SOL threshold, so the curves never graduate; the activity is over within minutes.

**4. Configs that take the buyers' SOL at migration.** Hundreds of configs set a migration fee of 99%: at graduation, 99% of the SOL raised on the curve goes to the fee receiver and 1% goes into the post-migration pool. Buyers would hold most of the supply with almost no liquidity to sell into.

**Who pays for it.** In the farm and proxy patterns, external buyers are nearly absent: the operator's SOL goes around in a circle (farm → curve → liquidity → back to the operator), and in the launches we traced the operator ends at about zero, paying roughly 0.1–0.2 SOL per launch in fees. The product of these launches is the statistics themselves — launches, graduations and volume that look like a market in feeds and terminals. Why someone pays for that is not visible on chain, and we do not guess. In the sell-to-the-first-buyer pattern the money comes from whoever buys right after the creator, mostly bots.

All of this is visible from the config account and the first trades, **before tools that need trading history** can see it. Operator addresses are intentionally not published.

<!-- Mainnet statistics for the final submission are added after the final collection (Oct 10–11). -->

## Cross-check with Jupiter

Jupiter publishes its own token signals through the Tokens API: `organicScore` (0–100) and an `audit.isSus` flag for suspicious tokens. We compared them with DBC Radar verdicts on the same tokens (`tools/organic_compare.py`). The sample is balanced across operator clusters — up to five tokens from each independent cluster, tokens 2 hours to 7 days old — so the largest operator cannot dominate it. As of October 7, 2026:

| DBC Radar verdict | Flagged `isSus` by Jupiter |
|---|---|
| 🔴 RED — operator clusters where Jupiter flags most tokens | **5 of 6** (24 of 30 tokens) |
| 🟠 AMBER | 0 of 100 |
| ⚪ SELF-GRAD | 15 of 20 |
| 🟢 GREEN | 7 of 100 |

- **Where we agree.** Synthetic launches by wallet farms are flagged by both, independently.
- **Where we see more.** The sixth RED cluster — 100% of curve volume from creator-linked wallets, tokens with a transfer hook and retained mint authority — is not flagged by Jupiter; we verified it transaction by transaction. Trading through proxy signers (pattern 2) is not flagged either: each proxy looks like a separate wallet.
- **Where we see something else.** Config risks such as a 99% migration fee are not about trading activity, so an activity-based signal does not flag them (AMBER: 0 of 100). They are known before the first buy.
- **Organic Score** is near zero for almost all DBC launches of this age, in every group, so it does not separate them.

The comparison is refreshed with the final collection.

## Telegram bot

Send a token, pool or config address — or a Solscan, DexScreener, Jupiter or Meteora link — and get the verdict with its reasons. Buttons open the config report, the operator cluster and the config's recent launches.

- **Anything, not only what the collector saw.** Unknown addresses are resolved on-chain: a DBC pool account gives its config and creator; a token's earliest transaction is its pool creation; Meteora DAMM v2 and DLMM pool addresses are resolved to their token. The result is added to the database and scored.
- **Other launchpads.** Tokens from pump.fun, Raydium LaunchLab or Moonshot are recognised and the bot says so.
- **Commands:** `/start`, `/check <address>`, `/latest` (latest RED / AMBER launches), `/stats`, `/how`.
- **Alerts (optional):** posts new RED / RED-LINK / AMBER configs and a periodic digest to a channel.
- Each card lists the reasons in order: **Why** (or **Minor signals** for GREEN) — the flags that scored, strongest first; **In its favour** — locked liquidity, small leftover supply, no linked trading observed; **Notes** — context that does not score.
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
- migration fee: the share of SOL raised on the curve that is taken at graduation instead of going into the post-migration pool (50% and more makes the config risky on its own);
- mint authority, transfer hooks, instant graduation (migration threshold ≈ 0).

**Evidence** — observed in tracked pools:
- **linked volume**: share of curve volume from the creator, fan-out sellers, the operator's wallet farm and master wallets;
- **master wallets**: a wallet that pays for, or receives the proceeds of, trades signed by at least three other wallets in a pool (the collector records who actually paid or received the SOL of each swap, not only the signer);
- **fan-out**: wallets that sell tokens they never bought in the pool;
- **wallet farm**: wallets that trade in at least 30% of a config's pools and have at least 80% of their activity there (this separates a farm from generic bots that buy every new token);
- **constant first buyers**, **identical opening buys**, **launch-to-migration time**, single-creator configs.

**When it is RED.** Points from several weak signals are not enough. RED needs strong trading evidence: at least 80% of curve volume from linked wallets that include a farm (3+ wallets) or a master wallet, or mass fan-out (10+ wallets per pool selling tokens they never bought). When nearly all volume is the creator's own trades in a pool nobody else trades, that is shown as a signal, not as RED. Without strong evidence, the verdict follows the config: AMBER or GREEN, with every signal still listed.

**Operators and templates.** A *template* is a fingerprint of all economic config fields except addresses. An *operator cluster* links configs that share a creator, fee claimer, leftover receiver or at least three fan-out wallets. Addresses of launchpads are not links: an address used by 10+ configs where most show no synthetic trading is a platform address, and burn addresses never link. A config gets **RED-LINK** only through a direct link to a RED config:
- shared pool creators that account for at least 30% of its pools (a new config of the same operator, not a launchpad where a couple of users also launched elsewhere);
- a shared fee or leftover receiver that is the operator's own — most configs using it are RED;
- shared fan-out wallets, if the config is risky itself.

A new config of a known operator gets RED-LINK before it has a single trade.

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
| Collector | `src/` | discovers DBC pools, decodes swap / graduation events and records who actually paid or received the SOL and the tokens of each swap, reads graduation state from pool accounts, stores config accounts, builds per-pool evidence reports |
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

### Research tools

Python scripts in `tools/` (standard library only) for checks that need full transaction history; they read `HISTORY_RPC_URL` and never print it.

```bash
python3 tools/post_migration.py <cluster> <pools> <max_tx> [--tokens=...] [--ungraduated] [-v]   # full cycle per token: curve, migration, DAMM v2
python3 tools/token_txs.py <mint-prefix>                     # every transaction of one token with labelled participants
python3 tools/bundle_check.py --fast=10                      # who funded the wallets that bought in the first slots
python3 tools/organic_compare.py --per-cluster=5             # DBC Radar verdicts vs Jupiter Organic Score / isSus
python3 tools/token_meta.py 15 cluster=<id> verdict=GREEN    # token metadata by group: hosting, platform, links
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
| `JUPITER_API_KEY` | tools | key for the Jupiter Tokens API, used only by `tools/organic_compare.py` |

## Validation

- The replay engine uses the DBC program crate itself (`update_pre_swap → get_swap_result_* → apply_swap_result`) and matched **741 of 741** recorded swaps of a real pool to the lamport, including the order of transactions within a slot.
- Evidence reports traced **173 of 173** fan-out sellers in six launches of one operator directly to the creator's transfers, each of the same size.
- The collector's discovery coverage is measured continuously against operators whose launches are polled directly.
- End-to-end traces balance: in every traced pool, what all participants gained and lost adds up to the SOL left in the pool plus fees.
- Jupiter independently flags most tokens of the RED operator clusters (see Cross-check with Jupiter).

## Limitations

- Trading evidence comes from sampled pools; config capability is computed for every config.
- The actual payer of each swap is recorded from October 7, 2026; for older swaps the engine falls back to the signer, so trading through proxy signers is recognised only in newer pools.
- An operator that uses fresh, unrelated wallets for every launch weakens the farm signals; config capability, address links and master-wallet detection still apply.
- Bundled launches (the creator funding the first-slot buyers in advance) were checked on fast-filled pools: the first-slot buyers we checked were independent bots funded from unrelated sources, so there is no bundle signal yet.
- Post-migration activity in DAMM v2 is analysed by the research tools, not yet by the engine; liquidity withdrawal is confirmed through transaction traces, not decoded DAMM v2 events.
- Graduation comes from the decoded `EvtCurveComplete` event; when the completing transaction was not collected, from the pool account's `finish_curve_timestamp`.
- Verdicts are heuristic analysis of public on-chain data, not financial advice.

## Roadmap

- Post-migration check in the engine: the share of DAMM v2 volume that comes from the curve's linked wallets, so terminals can discount the volume of graduated tokens.
- Funding graph: who funds farm wallets and first-slot buyers, to link operators that rotate wallets.
- A per-config measure of what external buyers actually lost, shown next to every verdict.
- A streaming feed of new risky launches for terminals (websocket / webhooks).
- A "clean config" attestation that honest launchpads can show their users.

## License

MIT
