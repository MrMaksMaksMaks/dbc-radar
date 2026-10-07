#!/usr/bin/env python3
"""Насколько часто встречаются две схемы первых секунд пула — по данным коллектора (без RPC).

Запуск из корня репозитория:
    python3 tools/early_patterns.py [--days=7] [--top=12]

A. «Приманка для снайпера» (как 6jb8HBip): создатель покупает в слоте создания, кто-то другой
   покупает в следующие 2 слота, создатель продаёт в первые 30 секунд дороже, чем купил.
B. «Налог на снайперов» (как FboheJ3f): покупки в первых 2 слотах платят торговую комиссию
   10% и выше — так работает планировщик комиссии (anti-sniper fee) конфига.

Для каждой схемы: сколько пулов, конфигов и создателей, сколько SOL, и насколько это повторяется
у одних и тех же создателей и конфигов (стратегия или случайность). Данные — только по
отслеживаемым пулам (выборка коллектора), так что абсолютные числа занижены примерно вдвое.
"""
import sqlite3
import sys
import time
from collections import Counter, defaultdict


def opt(name, default):
    for a in sys.argv[1:]:
        if a.startswith(f"--{name}="):
            return a.split("=", 1)[1]
    return default


DAYS = float(opt("days", "7"))
TOP = int(opt("top", "12"))
db = sqlite3.connect("dbc.sqlite")
since = int(time.time() - DAYS * 86400)

rows = db.execute(
    """SELECT p.pool, p.config, p.creator, p.created_slot, p.created_time,
              s.fee_payer, s.trade_direction, s.included_fee_input_amount, s.excluded_fee_input_amount,
              s.output_amount, s.trading_fee, s.protocol_fee, s.referral_fee, s.slot, s.block_time
       FROM pools p JOIN swaps s ON s.pool = p.pool
       WHERE p.tracked = 1 AND p.created_time >= ? AND s.block_time <= p.created_time + 60
       ORDER BY p.pool, s.slot, s.event_index""", (since,)).fetchall()
pools = defaultdict(list)
meta = {}
for r in rows:
    pools[r[0]].append(r[5:])
    meta[r[0]] = r[1:5]
tracked = db.execute("SELECT COUNT(*) FROM pools WHERE tracked = 1 AND created_time >= ?", (since,)).fetchone()[0]
print(f"last {DAYS:g} days: {tracked} tracked pools, {len(pools)} with trades in the first 60 s\n")


def fee_rate(inc, exc, out, tf, pf, rf):
    if inc and inc > exc:  # комиссия взята со входа (SOL при покупке)
        return (inc - exc) / inc
    fees = (tf or 0) + (pf or 0) + (rf or 0)  # комиссия в токене на выходе
    return fees / (out + fees) if (out + fees) else 0.0


A, B = [], []
for pool, sw in pools.items():
    config, creator, cslot, ctime = meta[pool]
    # --- A: приманка
    c_buy = sum(s[2] for s in sw if s[0] == creator and s[1] == 1 and s[8] <= cslot + 1)
    other_early = [s for s in sw if s[0] != creator and s[1] == 1 and s[8] <= cslot + 2]
    c_sell = [s for s in sw if s[0] == creator and s[1] == 0 and s[9] is not None and s[9] <= ctime + 30]
    if c_buy and other_early and c_sell:
        first_sell_slot = min(s[8] for s in c_sell)
        bait = [s for s in other_early if s[8] <= first_sell_slot]
        got = sum(s[4] for s in c_sell)
        if bait and got > c_buy:
            A.append((pool, config, creator, (got - c_buy) / 1e9, sum(s[2] for s in bait) / 1e9, len(bait)))
    # --- B: налог на снайперов
    early = [s for s in sw if s[1] == 1 and s[8] <= cslot + 1 and s[0] != creator]
    rates = [fee_rate(s[2], s[3], s[4], s[5], s[6], s[7]) for s in early]
    high = [(s, r) for s, r in zip(early, rates) if r >= 0.10]
    if high:
        paid = sum(r * s[2] / 1e9 for s, r in high if s[2] > s[3])  # SOL комиссии, где она со входа
        B.append((pool, config, creator, paid, max(r for _, r in high), len(high), sum(s[2] for s, _ in high) / 1e9))


def report(name, items, value_idx, value_name, extra):
    print(f"=== {name}: {len(items)} pools ({100 * len(items) / max(len(pools), 1):.1f}% of pools with early trades)")
    if not items:
        print()
        return
    cfg = Counter(x[1] for x in items)
    cre = Counter(x[2] for x in items)
    total = sum(x[value_idx] for x in items)
    rep_cre = sum(n for n in cre.values() if n >= 3)
    rep_cfg = sum(n for n in cfg.values() if n >= 3)
    print(f"  {len(cfg)} configs, {len(cre)} creators; {value_name}: total {total:.1f} SOL, "
          f"median {sorted(x[value_idx] for x in items)[len(items) // 2]:.3f} SOL per pool")
    print(f"  repeatable: {rep_cre} of {len(items)} pools come from creators with 3+ such pools, "
          f"{rep_cfg} from configs with 3+ such pools")
    print(f"  {extra}")
    print("  top creators (pools, SOL):")
    by_c = defaultdict(lambda: [0, 0.0, set()])
    for x in items:
        e = by_c[x[2]]
        e[0] += 1
        e[1] += x[value_idx]
        e[2].add(x[1][:8])
    for c, (n, v, cfgs) in sorted(by_c.items(), key=lambda kv: -kv[1][0])[:TOP]:
        print(f"    {c[:8]}…  {n:4d} pools  {v:8.2f} SOL  configs {', '.join(sorted(cfgs))[:60]}")
    print()


report("A. Sniper bait (creator buys, someone buys in the next slots, creator sells within 30 s at a profit)",
       A, 3, "creator profit",
       f"bait buyers: {sum(x[5] for x in A)} buys, {sum(x[4] for x in A):.1f} SOL bought before the creator's first sale")
report("B. Sniper tax (buys in the first 2 slots pay a trading fee >= 10%)",
       B, 3, "fees paid by those buys (where taken in SOL)",
       f"max fee rate seen: median {sorted(x[4] for x in B)[len(B) // 2] * 100 if B else 0:.0f}%; "
       f"{sum(x[5] for x in B)} high-fee buys worth {sum(x[6] for x in B):.1f} SOL")
print("notes: tracked pools only (the collector samples about every second pool); swaps in the first 60 s of each pool;"
      "\n       A counts only pools where the creator's sale came after at least one other buyer;"
      "\n       B fee rate = (amount before fee - after fee) / before, or the token-side fee share when the fee is taken in tokens.")
