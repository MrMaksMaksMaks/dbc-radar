# DBC Radar

**A trust layer for Meteora DBC launches: see what a launch's config allows and who is behind it — before the first buy or before adding liquidity.**

*Русская версия: [README_RUS.md](README_RUS.md)*

Anyone can launch a token on Meteora's Dynamic Bonding Curve (DBC) under their own *config*: the rules for who gets the liquidity after graduation, how much of it is locked, how much supply goes to a "leftover receiver", and how the trading fee is set and shared. Buyers and liquidity providers never see those rules. They see a token with volume that "graduated" to a Meteora pool.

DBC Radar reads the rules from chain, watches what happens in the pools and gives a verdict for any launch — as a Telegram bot ([@DBC_Radar_bot](https://t.me/DBC_Radar_bot)) and as a public JSON API.

## Who it is for

- **Buyers** — paste a token, pool or link and see whether the launch is organic before the first buy.
- **Liquidity providers** — paste a Meteora DAMM v2 or DLMM pool address and see whether the creator can still pull unlocked liquidity or dump leftover supply into it.
- **Launchpads built on DBC** — show users that their configs are clean, and spot operators abusing their platform.
- **Terminals, wallets and bots** — one API call per token, the same verdict as in the bot.

## What it checks

1. **What does this config allow the creator or launchpad to do?** Read from the config account, so it is known in the second the config is created — before anyone buys.
2. **Who is behind the launch, and is the trading real?** Trading in the pools is compared with the creator's transfers and with wallets that reappear across the config's launches. New configs are linked to known operators through shared addresses.

## Verdicts

| Verdict | Meaning |
|---|---|
| 🟢 **GREEN** | no major red flags in the config or in observed trading (minor signals are still listed) |
| ⚪ **SELF-GRAD** | instant or self-funded graduation, no real market on the curve; the risk moves to the post-migration pool |
| 🟠 **AMBER** | caution: a risky config, or suspicious trading that is not conclusive. The creator or launchpad can take most of the buyers' SOL or liquidity during trading or at/after migration — unlocked liquidity with a large leftover supply, a migration fee of 50% and more, or a "sniper fee" that sends most of the early buyers' SOL to one address |
| 🔴 **RED** | synthetic launches: trading is dominated by the creator and wallets linked to it |
| 🔴 **RED-LINK** | a config linked by specific addresses to a synthetic-launch operator — flagged before its first trade |

## Labeling principles

DBC is a permissionless market: in every launch someone wins and someone loses, and money does not appear from nowhere. **Losses alone are never a reason for a negative verdict.** A verdict says what a config *allows* and what was actually *observed* in its pools.

- **A risky setting gives AMBER, never RED.** Unlocked liquidity, a large leftover, a high migration fee or a sniper fee are visible in the config — the buyer can check them before buying.
- **RED needs observed behaviour.** Points from several weak signals are not enough: RED needs a strong trading signal — at least 80% of curve volume from linked wallets that include a farm or a wallet trading through proxy signers, or mass fan-out (10+ wallets per pool selling tokens they never bought).
- **Links go through specific addresses only.** RED-LINK comes from a direct link to a RED config: shared pool creators that make up at least 30% of the config's pools, an operator's own fee or leftover address, or shared fan-out wallets. A launchpad address shared by many mostly clean configs is not a link.
- **Never by launchpad, template or losses.** A verdict is about one config, not about the launchpad that issued it; the bot card says how many configs the launchpad has and how many of them are RED. A config that merely has the same parameters as RED configs gets a note, not points.
- **Launchpad configs need direct evidence.** When a launchpad has 5+ configs and fewer than half of them are RED, its config used by 10+ creators gets RED only on direct evidence (mass fan-out or trading through proxy wallets); otherwise AMBER with the signals listed. Such a config does not pass risk on to other configs.
- **Default templates come out clean.** Meteora's default Invent template and launchpads that lock all liquidity are GREEN; configs where the launchpad holds the liquidity are not RED anywhere in the data.

Every threshold above can be tuned without a rebuild (see [Tuning thresholds](#tuning-thresholds)) — operators change their setups, and the rules should follow.

## What it finds on mainnet

The collector watches DBC launches on mainnet around the clock. **From October 8, 21:00 UTC to October 11, 17:30 UTC (about 68 hours) it saw 11,557 launches under 2,909 configs:**

| Verdict of the launch's config | Launches | Share |
|---|---|---|
| 🔴 RED | 4,326 | 37% |
| 🔴 RED-LINK | 48 | 0.4% |
| 🟠 AMBER | 516 | 4.5% |
| ⚪ SELF-GRAD | 3,468 | 30% |
| 🟢 GREEN | 3,199 | 28% |

Trading evidence comes from a sample of every second pool; the capability of every config is read from chain. Besides ordinary launches, the same few patterns keep recurring. We traced each of them end to end — every transaction of the DBC pool and, after graduation, of the Meteora DAMM v2 pool, with SOL counted per owner across all accounts of each transaction.

**1. Bundle launch: a curve bought out in seconds.** One wallet buys 60–90% of the migration threshold in the first five seconds — either a service wallet that does this for many creators, or the creator in a separate transaction with the same amount every time. Within seconds the tokens are spread in equal parts across dozens of fresh wallets that never bought in the pool, and those wallets sell back into it. The curve (11–14 SOL) completes in 30–130 seconds. The config gives the creator 89% of the post-migration liquidity unlocked; in every one of the 30 launches we checked across six such configs, the creator withdrew it within ten minutes of migration. **17 configs, 3,486 launches in the window above — 30% of all launches.** 16 of the 17 are RED on observed trading, the last one is AMBER.

**2. One wallet trading through proxy signers.** Dozens of wallets sign the trades, but the SOL for every buy comes from — and every sale pays back to — one wallet that never signs. A session moves about 100 SOL of buys through the curve in two or three minutes. To a tool that looks at signers, these are dozens of independent traders.

**3. Instant graduation.** The migration threshold is about zero: the token moves straight to a DAMM v2 pool with the creator's own SOL, and there is no market on the curve at all. 1,288 configs, 3,459 launches in the window. This is SELF-GRAD, not RED: the risk is in the post-migration pool, where the creator holds most of the liquidity (in the pools we checked, it was not withdrawn).

**4. Sniper fee.** The trading fee starts at 70–99% and decays over minutes, and 80–100% of it goes to one address — the creator or the launchpad. Whoever buys in the first minutes pays most of their SOL as a fee to that address. About 100 configs in the database, almost all used for a single launch. AMBER: the fee schedule is in the config and visible before the first buy.

**5. Closed arenas.** On one launchpad, a few hundred creators and a pool of about 800 wallets traded each other's tokens within seconds of launch, with the creator buying in the first second and selling on a schedule. Two configs, more than 600 launches a day on the larger one — until the launchpad stopped launching on October 8. AMBER, not RED: the configs are used by hundreds of creators, and there is no fan-out or proxy trading.

**6. Configs that take the buyers' SOL at migration.** Hundreds of configs set a migration fee of 99%: at graduation, 99% of the SOL raised on the curve goes to the fee receiver and 1% goes into the post-migration pool. Buyers would hold most of the supply with almost no liquidity to sell into.

All of this is visible from the config account and the first trades, **before tools that need trading history** can see it. Operator addresses are intentionally not published.

## Cross-check with Jupiter

Jupiter publishes its own token signals through the Tokens API: `organicScore` (0–100) and an `audit.isSus` flag for suspicious tokens. We compared them with DBC Radar verdicts on the same tokens, with a research script run next to the collector. The sample is balanced across operator clusters — up to five tokens from each independent cluster, tokens 2 hours to 7 days old — so the largest operator cannot dominate it. As of October 7, 2026:

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

## Telegram bot

Send a token, pool or config address — or a Solscan, DexScreener, Jupiter or Meteora link — and get the verdict with its reasons. Buttons open the config report, the operator cluster and the config's recent launches.

- **Anything, not only what the collector saw.** Unknown addresses are resolved on-chain: a DBC pool account gives its config and creator; a token's earliest transaction is its pool creation; Meteora DAMM v2 and DLMM pool addresses are resolved to their token. The result is added to the database and scored.
- **Other launchpads.** Tokens from pump.fun, Raydium LaunchLab or Moonshot are recognised and the bot says so.
- **Commands:** `/start`, `/check <address>`, `/latest` (latest RED / AMBER launches), `/stats`, `/how`.
- **Alerts (optional):** posts new RED / RED-LINK / AMBER configs and a periodic digest to a channel.
- Each card lists the reasons in order: **Why** (or **Minor signals** for GREEN) — the flags that scored, strongest first; **In its favour** — locked liquidity, small leftover supply, no linked trading observed; **Notes** — context that does not score.
- The config report says how many configs its launchpad has and how many of them are RED: the verdict is about the config, not the launchpad.
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
              "capability_flags": [ … ], "evidence_flags": [ … ], "cluster": 12, "cluster_size": 1,
              "partner_configs": 1, "partner_red_configs": 1 }
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
- **sniper fee**: the trading fee starts at 70% or more, decays for 90 seconds or longer, and 80% or more of it (after the protocol share) goes to one address — the creator or the launchpad. This alone makes the config AMBER;
- mint authority, transfer hooks, instant graduation (migration threshold ≈ 0).

**Evidence** — observed in tracked pools:
- **linked volume**: share of curve volume from the creator, fan-out sellers, the operator's wallet farm and master wallets;
- **master wallets**: a wallet that pays for, or receives the proceeds of, trades signed by at least three other wallets in a pool (the collector records who actually paid or received the SOL of each swap, not only the signer);
- **fan-out**: wallets that sell tokens they never bought in the pool;
- **wallet farm**: wallets that trade in at least 30% of a config's pools and have at least 80% of their activity there (this separates a farm from generic bots that buy every new token);
- **constant first buyers**, **identical opening buys**, **launch-to-migration time**, single-creator configs.

**Verdict order.** RED if evidence reaches 50 points *and* there is a strong trading signal (see [Labeling principles](#labeling-principles)); otherwise SELF-GRAD for instant graduation; otherwise AMBER if capability reaches 50 points; otherwise GREEN. The platform guard then turns RED into AMBER for launchpad configs without direct evidence. When nearly all volume is the creator's own trades in a pool nobody else trades, that is shown as a signal, not as RED.

**Operators and templates.** A *template* is a fingerprint of all economic config fields except addresses. An *operator cluster* links configs that share a creator, fee claimer, leftover receiver or at least three fan-out wallets. Addresses of launchpads are not links: an address used by 10+ configs where most show no synthetic trading is a platform address, and burn addresses never link. A config gets **RED-LINK** only through a direct link to a RED config:
- shared pool creators that account for at least 30% of its pools (a new config of the same operator, not a launchpad where a couple of users also launched elsewhere);
- a shared fee or leftover receiver that is the operator's own — at least half of the configs using it are RED;
- shared fan-out wallets, if the config is risky itself.

A new config of a known operator gets RED-LINK before it has a single trade. Sharing a parameter template with RED configs is shown as a note and scores nothing.

### Tuning thresholds

The verdict thresholds are read from the environment (or from `.env` in the working directory); an empty value keeps the default. The bot passes its environment to the engine, so restart the bot after a change.

| Variable | Default | Meaning |
|---|---|---|
| `RISK_RED_MIN_EVIDENCE` | 50 | evidence points needed for RED (plus a strong trading signal) |
| `RISK_AMBER_MIN_CAPABILITY` | 50 | capability points needed for AMBER |
| `RISK_STRONG_LINKED_SHARE` | 0.8 | share of volume from creator-linked wallets that counts as a strong trading signal |
| `RISK_STRONG_FANOUT_WALLETS` | 10 | median fan-out wallets per pool that counts as a strong trading signal |
| `RISK_FAST_MIGRATION_SECS` | 300 | a curve filled faster than this (median, seconds) is flagged |
| `RISK_FEE_TRAP_MIN_START_PCT` | 70 | sniper fee: minimum starting fee, % |
| `RISK_FEE_TRAP_MIN_SECS` | 90 | sniper fee: minimum decay duration, seconds |
| `RISK_FEE_TRAP_MIN_SHARE_PCT` | 80 | sniper fee: minimum share of the fee taken by one address, % |
| `RISK_FEE_TRAP_POINTS` | 50 | capability points for the sniper fee |
| `RISK_TEMPLATE_POINTS` | 0 | evidence points for sharing a template with RED configs (0 = note only) |
| `RISK_PLATFORM_GUARD_MIN_CONFIGS` | 5 | platform guard: launchpad with at least this many configs |
| `RISK_PLATFORM_GUARD_MIN_CREATORS` | 10 | platform guard: config with at least this many creators |

`dbc-replay thresholds` prints the values in effect and marks the changed ones. An invalid value prints a warning and keeps the default.

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
./replay/target/release/dbc-replay dbc.sqlite risk                   # all configs, grouped by verdict
./replay/target/release/dbc-replay dbc.sqlite risk <config-prefix>   # full report for one config
./replay/target/release/dbc-replay dbc.sqlite risk --json            # machine-readable (used by the bot)
./replay/target/release/dbc-replay dbc.sqlite clusters [<prefix>]    # operator clusters
./replay/target/release/dbc-replay dbc.sqlite templates              # parameter templates shared by several configs
./replay/target/release/dbc-replay dbc.sqlite impact [--since-hours N]   # linked vs external activity per RED operator
./replay/target/release/dbc-replay dbc.sqlite fee-traps [--since-hours 72] [--all] [--json]   # configs with a sniper fee
./replay/target/release/dbc-replay dbc.sqlite config-dump <mint | pool | config>   # every field of the config and pool
./replay/target/release/dbc-replay thresholds                        # verdict thresholds in effect
./replay/target/release/dbc-replay dbc.sqlite <pool-prefix> [--cf]   # bit-exact replay of a pool, optional counterfactual fees
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
| `BACKUP_RPC_URL`, `BACKUP_RPC_RPS`, `BACKUP_MAX_PER_HOUR` | collector | backup RPC for transactions the main RPC no longer serves (empty = `HISTORY_RPC_URL`), its rate and an hourly cap (default 2 rps, 1000 per hour); without it such a transaction is skipped with a warning |
| `GRAD_CHECK_SECS`, `GRAD_CHECK_HOURS` | collector | how often to check graduation from pool accounts, and how far back (default 120 s, 24 h) |
| `STATUS_RPC_URL`, `STATUS_RPC_RPS` | collector | RPC for that check (`getMultipleAccounts`, 100 accounts per call) and its rate limit; empty = `RPC_URL` |
| `DB_PATH` | all | SQLite database (default `dbc.sqlite`) |
| `TELEGRAM_BOT_TOKEN` | bot | token from @BotFather |
| `TELEGRAM_BOT_USERNAME` | bot | used for links from the alert channel |
| `REPLAY_BIN` | bot | path to `dbc-replay` (default `replay/target/release/dbc-replay`) |
| `REFRESH_SECS` | bot | how often verdicts are recomputed (default 300) |
| `HISTORY_RPC_URL` | bot | RPC with full transaction history, for finding the pool of an older token |
| `ALERT_CHAT`, `DIGEST_SECS` | bot | alert channel (`@name` or id) and digest period |
| `API_BIND` | bot | API listen address, e.g. `127.0.0.1:8080` (empty = off) |
| `API_KEYS` | bot | comma-separated API keys (on-chain lookups, full addresses, higher limit) |
| `API_RATE_PUBLIC`, `API_RATE_KEY` | bot | requests per minute per IP without / with a key |
| `RISK_*` | engine, bot | verdict thresholds, see [Tuning thresholds](#tuning-thresholds) |

## Validation

- The replay engine uses the DBC program crate itself (`update_pre_swap → get_swap_result_* → apply_swap_result`) and matched **741 of 741** recorded swaps of a real pool to the lamport, including the order of transactions within a slot.
- Evidence reports traced **173 of 173** fan-out sellers in six launches of one operator directly to the creator's transfers, each of the same size.
- Liquidity withdrawal in the bundle pattern was checked by RPC in 30 launches of six configs: withdrawn in all 30, within ten minutes of migration. In instant-graduation configs it was withdrawn in none of the pools checked.
- Every RED verdict on a config used by 20+ creators was reviewed by hand against the launchpad principles above; configs where the launchpad holds the liquidity have no RED.
- The collector's discovery coverage is measured continuously against operators whose launches are polled directly.
- End-to-end traces balance: in every traced pool, what all participants gained and lost adds up to the SOL left in the pool plus fees.
- Jupiter independently flags most tokens of the RED operator clusters (see Cross-check with Jupiter).

## Limitations

- Trading evidence comes from sampled pools (every second pool); config capability is computed for every config.
- **Data gap, October 4–8.** Under load, the collector lost the late trades of some pools: the main RPC no longer served older transactions, and polling rounds stretched to hours. The first 5–30 minutes of every pool are complete; later trades of that period are not. Since October 8, 21:00 UTC the collector falls back to a backup RPC for such transactions and skips them with a warning otherwise. The statistics above use only the window after the fix.
- The actual payer of each swap is recorded from October 7, 2026; for older swaps the engine falls back to the signer, so trading through proxy signers is recognised only in newer pools.
- An operator that uses fresh, unrelated wallets for every launch weakens the farm signals; config capability, address links and master-wallet detection still apply.
- The sniper fee is judged by the full length of the fee schedule, not by how long the fee stays above 70%; whether the fee was actually collected is not checked.
- Post-migration activity in DAMM v2, including liquidity withdrawal, is checked in transaction-level research traces, not yet by the engine.
- Graduation comes from the decoded `EvtCurveComplete` event; when the completing transaction was not collected, from the pool account's `finish_curve_timestamp`.
- Verdicts are heuristic analysis of public on-chain data, not financial advice.

## Roadmap

- Post-migration check in the engine: liquidity withdrawal and the share of DAMM v2 volume that comes from the curve's linked wallets, so terminals can discount the volume of graduated tokens.
- Funding graph: who funds farm wallets and first-slot buyers, to link operators that rotate wallets.
- A streaming feed of new risky launches for terminals (websocket / webhooks).
- A "clean config" attestation that honest launchpads can show their users.

## License

MIT
