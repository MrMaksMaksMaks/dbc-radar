#!/usr/bin/env python3
"""
Токены по префиксам mint: пул, конфигурация, создатель, поведение на кривой, связь с известным оператором.

  cd ~/dbc-collector/replay
  ~/Projects/Wallet_check/.venv/bin/python inspect_tokens.py Bvagw9uW 4UPbg63i 4ZgDXMCa ...

Для каждого токена: конфигурация и fee claimer, время до миграции, трейдеры, дев-покупка создателя,
доля объёма постоянных кошельков конфигурации (>= 30% её отслеживаемых пулов), инсайдеры
(продали, не купив), внешние кошельки и их нетто SOL. Затем — сводка по конфигурациям и создателям
и пересечения с operator_wallets.txt и с постоянными кошельками конфигурации 2toDx (наш оператор).
Пишет inspect_tokens.csv.
"""
import argparse, os, sqlite3
import pandas as pd

LAMP = 1e9
ap = argparse.ArgumentParser()
ap.add_argument("prefixes", nargs="+")
ap.add_argument("--db", default="../dbc.sqlite")
ap.add_argument("--operator", default=os.path.expanduser("~/Projects/Wallet_check/operator_wallets.txt"))
a = ap.parse_args()

c = sqlite3.connect(a.db)
pools = pd.read_sql("SELECT pool, config, creator, base_mint, created_time, tracked FROM pools", c)
cfgs = pd.read_sql("SELECT config, fee_claimer FROM configs", c)
cc = pd.read_sql("SELECT pool, block_time AS mig_time FROM curve_complete", c)
sw = pd.read_sql("SELECT pool, fee_payer, trade_direction AS dir, included_fee_input_amount AS inp, "
                 "output_amount AS outp, block_time, slot FROM swaps", c)
sw["sol"] = sw.inp.where(sw.dir == 1, -sw.outp) / -LAMP          # buy: −SOL, sell: +SOL
sw["tok"] = sw.outp.where(sw.dir == 1, -sw.inp)
OP = set(open(a.operator).read().split()) if os.path.exists(a.operator) else set()


def recurring_of(config_set):
    p = set(pools[pools.config.isin(config_set)].pool)
    s = sw[sw.pool.isin(p)]
    n = s.pool.nunique()
    per = s.groupby("fee_payer").pool.nunique()
    return set(per[per >= max(2, 0.3 * n)].index), n


our_rec, _ = recurring_of(set(pools[pools.config.str.startswith("2toDxUnv")].config))
rows, rec_cache = [], {}
for pre in a.prefixes:
    hit = pools[pools.base_mint.str.startswith(pre)]
    if hit.empty:
        rows.append({"prefix": pre, "status": "нет в базе DBC Radar"}); continue
    p = hit.iloc[0]
    if p.config not in rec_cache:
        rec_cache[p.config] = recurring_of({p.config})
    rec, n_tr = rec_cache[p.config]
    x = sw[sw.pool == p.pool]
    w = x.groupby("fee_payer").agg(b=("tok", lambda t: t[t > 0].sum()), s=("tok", lambda t: -t[t < 0].sum()))
    insiders = set(w[(w.b == 0) & (w.s > 0)].index) - {p.creator}
    linked = rec | insiders | {p.creator}
    ext = ~x.fee_payer.isin(linked)
    mig = cc[cc.pool == p.pool].mig_time
    t0 = p.created_time or (x.block_time.min() if len(x) else None)
    vol = x.sol.abs().sum()
    rows.append({
        "prefix": pre, "status": "ok" if len(x) else "в базе, свопов нет",
        "mint": p.base_mint, "pool": p.pool, "config": p.config, "creator": p.creator,
        "fee_claimer": (cfgs[cfgs.config == p.config].fee_claimer.tolist() or [""])[0],
        "launch_utc": pd.to_datetime(t0, unit="s").strftime("%Y-%m-%d %H:%M") if t0 else "",
        "mig_s": int(mig.iloc[0] - t0) if len(mig) and t0 else None,
        "traders": x.fee_payer.nunique(),
        "creator_buy_sol": round(-x[(x.fee_payer == p.creator) & (x.dir == 1)].sol.sum(), 3),
        "linked_vol_pct": round(100 * x[~ext].sol.abs().sum() / vol, 1) if vol else None,
        "insiders": len(insiders), "ext_wallets": x[ext].fee_payer.nunique(),
        "ext_net_sol": round(x[ext].sol.sum(), 3),
        "cfg_recurring": len(rec), "cfg_tracked_pools": n_tr,
        "creator_in_operator": p.creator in OP,
        "recurring_overlap_2toDx": len(rec & our_rec), "recurring_overlap_operator": len(rec & OP),
    })

df = pd.DataFrame(rows)
df.to_csv("inspect_tokens.csv", index=False)
pd.set_option("display.width", 250); pd.set_option("display.max_colwidth", 14)
cols = ["prefix", "status", "launch_utc", "mig_s", "traders", "creator_buy_sol", "linked_vol_pct",
        "insiders", "ext_wallets", "ext_net_sol"]
print(df[[c for c in cols if c in df]].to_string(index=False))
ok = df[df.status != "нет в базе DBC Radar"]
if not ok.empty:
    print("\n=== Конфигурации ===")
    print(ok.groupby("config").agg(tokens=("prefix", "size"), creators=("creator", "nunique"),
          fee_claimer=("fee_claimer", "first"), recurring=("cfg_recurring", "first"),
          tracked_pools=("cfg_tracked_pools", "first"), overlap_2toDx=("recurring_overlap_2toDx", "first"),
          overlap_operator=("recurring_overlap_operator", "first")).to_string())
    print("\n=== Создатели ===")
    print(ok.groupby("creator").agg(tokens=("prefix", "size"), configs=("config", "nunique"),
          in_operator=("creator_in_operator", "first")).to_string())
    print("\nПолные адреса — inspect_tokens.csv. Параметры конфигурации: ./target/release/dbc-replay ../dbc.sqlite risk <config>")
