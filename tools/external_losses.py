#!/usr/bin/env python3
"""Сколько SOL внешние кошельки оставляют на кривой — по вердиктам DBC Radar. Только для исследований.

Запуск из корня репозитория:
    replay/target/release/dbc-replay dbc.sqlite risk --json --research > /tmp/risk_research.json
    python3 tools/external_losses.py [--min-pools=3] [--top=15]

«Внешние» — кошельки, не связанные с оператором конфига (не создатель, не раздача, не ферма,
не мастер-кошелёк); сюда входят и общие боты. «Оставили на кривой» — их покупки минус продажи
в активных пулах конфига: верхняя оценка потерь (у них остаются токены, которые могут чего-то
стоить, особенно после миграции). Отрицательное значение — внешние вывели больше, чем внесли.

Главный вопрос: есть ли GREEN и AMBER-конфиги, где внешние систематически теряют деньги, —
то есть опасные пулы, которые вердикт не выделяет.
"""
import json
import sys


def opt(name, default):
    for a in sys.argv[1:]:
        if a.startswith(f"--{name}="):
            return a.split("=", 1)[1]
    return default


MIN_POOLS = int(opt("min-pools", "3"))
TOP = int(opt("top", "15"))
rows = [r for r in json.load(open("/tmp/risk_research.json")) if r.get("research")]
VERDICTS = ["RED", "RED-LINK", "AMBER", "SELF-GRAD", "GREEN"]

print(f"configs with research data: {len(rows)}; per-config figures below use configs with >= {MIN_POOLS} active pools\n")
print(f"{'verdict':<10} {'configs':>7} {'pools':>6} | {'ext net SOL':>11} {'per pool':>8} | {'cfg median/pool >= 0.5':>22} "
      f"{'>= 2':>5} | {'pools >= 1 SOL':>14} {'>= 5 SOL':>8}")
for v in VERDICTS:
    g = [r for r in rows if r["verdict"] == v]
    if not g:
        continue
    pools = sum(r["research"]["active_pools"] for r in g)
    net = sum(r["research"]["external_net_sol"] for r in g)
    big = [r for r in g if r["research"]["active_pools"] >= MIN_POOLS]
    m05 = sum(1 for r in big if r["research"]["external_net_median"] >= 0.5)
    m2 = sum(1 for r in big if r["research"]["external_net_median"] >= 2.0)
    p1 = sum(r["research"]["pools_external_net_over_1"] for r in g)
    p5 = sum(r["research"]["pools_external_net_over_5"] for r in g)
    print(f"{v:<10} {len(g):>7} {pools:>6} | {net:>11.1f} {net / max(pools, 1):>8.3f} | {m05:>14} of {len(big):<5} {m2:>5} | "
          f"{p1:>14} {p5:>8}")


def show(title, items):
    print(f"\n{title}")
    print(f"  {'config':<10} {'verdict':<9} {'pools':>5} {'ext net':>8} {'median':>7} {'p90':>6} {'max':>6} {'ext wal':>7} "
          f"{'linked':>6} {'farm':>4} | main flags")
    for r in items:
        x = r["research"]
        flags = "; ".join(f["text"][:48] for f in (r["capability_flags"] + r["evidence_flags"]) if f["points"] > 0)[:110]
        ls = f"{x['median_linked_share']:.0%}" if x["median_linked_share"] is not None else "-"
        print(f"  {r['config'][:8]:<10} {r['verdict']:<9} {x['active_pools']:>5} {x['external_net_sol']:>8.1f} "
              f"{x['external_net_median']:>7.2f} {x['external_net_p90']:>6.2f} {x['external_net_max']:>6.1f} "
              f"{x['external_wallets']:>7} {ls:>6} {x['farm_wallets']:>4} | {flags or '-'}")


# кандидаты на пропуск: внешние стабильно оставляют деньги, а вердикт не RED
cand = [r for r in rows if r["verdict"] in ("GREEN", "AMBER", "SELF-GRAD") and r["research"]["active_pools"] >= MIN_POOLS]
show(f"GREEN / AMBER / SELF-GRAD where externals leave the most SOL per pool (median, >= {MIN_POOLS} active pools):",
     sorted(cand, key=lambda r: -r["research"]["external_net_median"])[:TOP])
show("GREEN / AMBER / SELF-GRAD where externals leave the most SOL in total:",
     sorted(cand, key=lambda r: -r["research"]["external_net_sol"])[:TOP])
show("RED / RED-LINK for comparison (most SOL left by externals in total):",
     sorted([r for r in rows if r["verdict"] in ("RED", "RED-LINK")], key=lambda r: -r["research"]["external_net_sol"])[:8])
print("\nnotes: ext net = externals' buys minus sells on the curve, an upper bound of their losses (they keep tokens);"
      "\n       externals include generic bots; per-config median is over active pools (>= 3 non-creator trades).")
