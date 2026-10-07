#!/usr/bin/env python3
"""Сколько оператор забирает у остальных на кривой — по вердиктам DBC Radar. Только для исследований.

Запуск из корня репозитория:
    replay/target/release/dbc-replay dbc.sqlite risk --json --research > /tmp/risk_research.json
    # только пулы, созданные после того, как сборщик начал записывать фактического плательщика:
    replay/target/release/dbc-replay dbc.sqlite risk --json --research --research-since <unix> > /tmp/risk_research.json
    python3 tools/external_losses.py [--min-pools=3] [--top=15]

Метрика — «изъятие оператора»: сколько SOL связанные кошельки (создатель, раздача, ферма,
мастер-кошельки) вывели с кривой сверх вложенного. В невыпустившемся пуле деньги никуда не уходят,
поэтому плюс оператора — это ровно то, что заплатили остальные участники (внешние и боты).
У выпустившихся пулов часть SOL ушла в ликвидность DAMM v2, и оператор возвращает её выводом
ликвидности, — там на кривой он обычно в минусе, это не потеря внешних.

Ограничения: только конфиги в SOL; мастер-кошельки с прокси распознаются лишь по свопам,
записанным после 7 октября 2026 (в старых данных такой мастер считается внешним).

Главный вопрос: есть ли GREEN и AMBER-конфиги, где оператор систематически забирает деньги
остальных на кривой, — опасные пулы, которые вердикт не выделяет.
"""
import json
import sys

WSOL = "So11111111111111111111111111111111111111112"


def opt(name, default):
    for a in sys.argv[1:]:
        if a.startswith(f"--{name}="):
            return a.split("=", 1)[1]
    return default


MIN_POOLS = int(opt("min-pools", "3"))
TOP = int(opt("top", "15"))
allrows = [r for r in json.load(open("/tmp/risk_research.json")) if r.get("research")]
rows = [r for r in allrows if r.get("quote_mint") == WSOL]
VERDICTS = ["RED", "RED-LINK", "AMBER", "SELF-GRAD", "GREEN"]

print(f"configs with research data: {len(allrows)}, of them quoted in SOL: {len(rows)} (others excluded)\n")
print("operator net on the curve, NOT graduated pools (positive = operator took other participants' SOL):")
print(f"{'verdict':<10} {'configs':>7} {'open pools':>10} | {'op net SOL':>10} {'per pool':>8} | "
      f"{'cfg median >= 0.5':>17} {'>= 1':>5} | {'pools >= 1 SOL':>14} | {'graduated: op median':>20}")
for v in VERDICTS:
    g = [r for r in rows if r["verdict"] == v]
    if not g:
        continue
    x = [r["research"] for r in g]
    pools = sum(e["op_open_pools"] for e in x)
    net = sum(e["op_open_net_sol"] for e in x)
    big = [e for e in x if e["op_open_pools"] >= MIN_POOLS]
    m05 = sum(1 for e in big if e["op_open_median"] >= 0.5)
    m1 = sum(1 for e in big if e["op_open_median"] >= 1.0)
    p1 = sum(e["op_open_pools_over_1"] for e in x)
    gr = sorted(e["op_grad_median"] for e in x if e["op_grad_pools"] > 0)
    grm = f"{gr[len(gr) // 2]:+.2f}" if gr else "-"
    print(f"{v:<10} {len(g):>7} {pools:>10} | {net:>+10.1f} {net / max(pools, 1):>+8.3f} | "
          f"{m05:>9} of {len(big):<5} {m1:>5} | {p1:>14} | {grm:>20}")


def show(title, items):
    print(f"\n{title}")
    print(f"  {'config':<10} {'verdict':<9} {'open':>4} {'op net':>7} {'median':>7} {'>=1':>4} | {'ext wal':>7} "
          f"{'linked':>6} {'farm':>4} {'mast':>4} | main flags")
    for r in items:
        x = r["research"]
        flags = "; ".join(f["text"][:46] for f in (r["capability_flags"] + r["evidence_flags"]) if f["points"] > 0)[:110]
        ls = f"{x['median_linked_share']:.0%}" if x["median_linked_share"] is not None else "-"
        print(f"  {r['config'][:8]:<10} {r['verdict']:<9} {x['op_open_pools']:>4} {x['op_open_net_sol']:>+7.1f} "
              f"{x['op_open_median']:>+7.2f} {x['op_open_pools_over_1']:>4} | {x['external_wallets']:>7} {ls:>6} "
              f"{x['farm_wallets']:>4} {x['master_wallets']:>4} | {flags or '-'}")


cand = [r for r in rows if r["verdict"] in ("GREEN", "AMBER", "SELF-GRAD") and r["research"]["op_open_pools"] >= MIN_POOLS]
show(f"GREEN / AMBER / SELF-GRAD where the operator takes the most per open pool (median, >= {MIN_POOLS} open pools):",
     sorted(cand, key=lambda r: -r["research"]["op_open_median"])[:TOP])
show("GREEN / AMBER / SELF-GRAD where the operator takes the most in total on open pools:",
     sorted([r for r in rows if r["verdict"] in ("GREEN", "AMBER", "SELF-GRAD")], key=lambda r: -r["research"]["op_open_net_sol"])[:TOP])
show("RED / RED-LINK for comparison:",
     sorted([r for r in rows if r["verdict"] in ("RED", "RED-LINK")], key=lambda r: -r["research"]["op_open_net_sol"])[:8])
print("\nnotes: op net = what creator, fan-out, farm and master wallets withdrew from the curve beyond what they put in;"
      "\n       on a pool that has not graduated this equals what everyone else paid in (externals and bots);"
      "\n       master wallets behind proxies are recognised only in swaps recorded after Oct 7, 2026.")
