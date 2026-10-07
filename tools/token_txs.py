#!/usr/bin/env python3
"""Все транзакции одного токена DBC с маркировкой участников и текущее состояние пула.

Запуск из корня репозитория (нужны dbc.sqlite, .env с HISTORY_RPC_URL на Helius и
свежий /tmp/risk_new.json):

    python3 tools/token_txs.py <начало адреса минта> [max_tx]

Пишет в out/:
  <mint8>_txs.csv     — по строке на участника транзакции: время, подпись, владелец, роль,
                        действие, изменение SOL и токена, фаза (кривая / миграция / DAMM v2);
  <mint8>_state.json  — пул, конфиг, создатель, текущие резервы, порог миграции, прогресс,
                        время с создания, параметры конфига из отчёта движка;
  <mint8>_risk.txt    — полный отчёт движка по конфигу.

Методика та же, что в post_migration.py: изменения считаются по владельцам всех аккаунтов
транзакции, хранилища пулов исключены, ферма — кошельки в 3+ пулах кластера оператора,
и это не меньше 80% всех пулов, где кошелёк торговал во всей базе.
"""
import csv
import json
import os
import sqlite3
import subprocess
import sys
import time
from collections import defaultdict

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import post_migration as pm  # noqa: E402  (rpc, signatures_after, owner_deltas, uses, kind, b58)

PREFIX = sys.argv[1] if len(sys.argv) > 1 else sys.exit("usage: token_txs.py <mint prefix> [max_tx]")
MAX_TX = int(sys.argv[2]) if len(sys.argv) > 2 else 5000
OFF_BASE_RESERVE, OFF_QUOTE_RESERVE, OFF_FINISH = 8 + 224, 8 + 232, 8 + 336


def u64(d, off):
    return int.from_bytes(d[off:off + 8], "little")


def main():
    db = sqlite3.connect("dbc.sqlite")
    row = db.execute(
        """SELECT p.pool, p.base_mint, p.config, p.creator, p.created_time, c.block_time
           FROM pools p LEFT JOIN curve_complete c ON c.pool = p.pool WHERE p.base_mint LIKE ? LIMIT 1""",
        (PREFIX + "%",)).fetchone()
    if not row:
        sys.exit(f"token {PREFIX}… not found in dbc.sqlite")
    pool, mint, config, creator, created, grad_time = row

    # роли: создатель, получатели конфига, ферма и создатели кластера оператора
    risk = json.load(open("/tmp/risk_new.json"))
    me = next((r for r in risk if r["config"] == config), None)
    cluster = me["cluster"] if me else None
    cfgs = [r["config"] for r in risk if r["cluster"] == cluster] if cluster else [config]
    ph = ",".join("?" * len(cfgs))
    fee_claimer, leftover = None, None
    craw = db.execute("SELECT fee_claimer, raw FROM configs WHERE config = ?", (config,)).fetchone()
    if craw:
        fee_claimer = craw[0]
        if craw[1] and len(craw[1]) >= 104:
            leftover = pm.b58(bytes(craw[1][72:104]))
    cluster_creators = {r[0] for r in db.execute(f"SELECT DISTINCT creator FROM pools WHERE config IN ({ph})", cfgs)}
    in_cluster = dict(db.execute(
        f"SELECT fee_payer, COUNT(DISTINCT pool) FROM swaps WHERE config IN ({ph}) GROUP BY fee_payer "
        f"HAVING COUNT(DISTINCT pool) >= 3", cfgs))
    farm = set()
    if in_cluster:
        wph = ",".join("?" * len(in_cluster))
        for w, total in db.execute(f"SELECT fee_payer, COUNT(DISTINCT pool) FROM swaps WHERE fee_payer IN ({wph}) "
                                   f"GROUP BY fee_payer", list(in_cluster)):
            if in_cluster[w] >= 0.8 * total:
                farm.add(w)

    def role(o):
        if o == creator:
            return "creator"
        if o in (fee_claimer, leftover):
            return "fee/leftover receiver"
        if o in farm:
            return "farm"
        if o in cluster_creators:
            return "operator (other pool creator)"
        return "external"

    # история: пул DBC, затем пул DAMM v2 (если был graduation)
    sigs = pm.signatures_after(pool, (created or 0) - 120, MAX_TX)[:MAX_TX]
    damm = None
    if grad_time:
        for s0 in pm.signatures_after(mint, grad_time)[:200]:
            tx0 = pm.rpc("getTransaction", [s0["signature"], {"encoding": "json", "maxSupportedTransactionVersion": 1}])
            if tx0 and pm.uses(tx0, pm.DAMM_V2):
                damm = pm.find_damm_pool(tx0)
                if damm:
                    break
        if damm:
            sigs += pm.signatures_after(damm, grad_time - 600, MAX_TX)[:MAX_TX]
    print(f"{mint}: DBC pool {pool}, {len(sigs)} transactions" + (f", DAMM v2 pool {damm}" if damm else ""))

    rows, seen = [], set()
    bought = defaultdict(int)  # сколько токена владелец купил в этом пуле (для пометки fan-out)
    for s in sigs:
        if s["signature"] in seen:
            continue
        seen.add(s["signature"])
        tx = pm.rpc("getTransaction", [s["signature"], {"encoding": "json", "maxSupportedTransactionVersion": 1}])
        if not tx:
            continue
        signer = tx["transaction"]["message"]["accountKeys"][0]
        in_damm, in_dbc = pm.uses(tx, pm.DAMM_V2), pm.uses(tx, pm.DBC)
        phase = "migration" if (in_dbc and in_damm) else ("DAMM v2" if in_damm else "curve")
        t = tx.get("blockTime") or 0
        sol_by, tok_by = pm.owner_deltas(tx, mint)
        for o in set(sol_by) | set(tok_by):
            if o in pm.POOL_AUTHORITIES or o in (pm.DBC, pm.DAMM_V2):
                continue
            sol, tok = sol_by.get(o, 0), tok_by.get(o, 0)
            if tok == 0 and abs(sol) < 1_000_000:
                continue  # плата сети, рента — не показываем
            r = role(o)
            if tok > 0 and sol < 0:
                action = "buy"
                bought[o] += tok
            elif tok < 0 and sol > 0:
                action = "sell"
            elif tok > 0 and sol > 0:
                action = "remove liquidity"
            elif tok < 0 and sol < 0:
                action = "add liquidity"
            elif tok > 0:
                action = "receive tokens"
            elif tok < 0:
                action = "send tokens"
            else:
                action = "SOL transfer"
            rows.append({
                "time_utc": time.strftime("%Y-%m-%d %H:%M:%S", time.gmtime(t)) if t else "",
                "seconds_from_creation": t - created if (t and created) else "",
                "slot": tx.get("slot", ""),
                "signature": s["signature"],
                "phase": phase,
                "owner": o,
                "fee_payer_is_owner": o == signer,
                "role": r,
                "action": action,
                "sol": round(sol / 1e9, 9),
                "tokens": tok,
            })
    # продал в пуле, ничего здесь не купив — получил токены переводом (раздача)
    sold_without_buy = {x["owner"] for x in rows if x["action"] == "sell" and bought.get(x["owner"], 0) == 0}
    for x in rows:
        x["sold_without_buying_here"] = x["owner"] in sold_without_buy and x["role"] != "creator"
        if x["role"] == "external" and x["sold_without_buying_here"]:
            x["role"] = "external (sells tokens it never bought here)"

    os.makedirs("out", exist_ok=True)
    base = f"out/{mint[:8]}"
    with open(base + "_txs.csv", "w", newline="") as f:
        w = csv.DictWriter(f, fieldnames=list(rows[0].keys()) if rows else ["signature"])
        w.writeheader()
        w.writerows(rows)

    # текущее состояние пула
    info = pm.rpc("getAccountInfo", [pool, {"encoding": "base64"}]) or {}
    v = (info or {}).get("value") or {}
    import base64
    data = base64.b64decode(v["data"][0]) if v.get("data") else b""
    state = {
        "mint": mint, "dbc_pool": pool, "damm_v2_pool": damm, "config": config, "creator": creator,
        "fee_claimer": fee_claimer, "leftover_receiver": leftover, "cluster": cluster,
        "cluster_configs": len(cfgs), "created_utc": time.strftime("%Y-%m-%d %H:%M:%S", time.gmtime(created)) if created else None,
        "hours_since_creation": round((time.time() - created) / 3600, 1) if created else None,
        "graduated_utc": time.strftime("%Y-%m-%d %H:%M:%S", time.gmtime(grad_time)) if grad_time else None,
        "now_utc": time.strftime("%Y-%m-%d %H:%M:%S", time.gmtime()),
        "transactions": len(seen),
    }
    if len(data) >= OFF_FINISH + 8:
        state.update({
            "base_reserve_now": u64(data, OFF_BASE_RESERVE),
            "quote_reserve_now_sol": u64(data, OFF_QUOTE_RESERVE) / 1e9,
            "finish_curve_timestamp": u64(data, OFF_FINISH),
            "is_migrated": data[8 + 297] if len(data) > 8 + 297 else None,
        })
    # последний своп из базы коллектора: порог миграции и резерв
    last = db.execute("SELECT quote_reserve_amount, migration_threshold, block_time FROM swaps WHERE pool = ? "
                      "ORDER BY slot DESC, event_index DESC LIMIT 1", (pool,)).fetchone()
    if last:
        state["migration_threshold_sol"] = last[1] / 1e9
        state["last_collected_swap_utc"] = time.strftime("%Y-%m-%d %H:%M:%S", time.gmtime(last[2])) if last[2] else None
        if "quote_reserve_now_sol" in state and last[1]:
            state["curve_progress_pct"] = round(100 * state["quote_reserve_now_sol"] / (last[1] / 1e9), 2)
    if me:
        state["verdict"] = me["verdict"]
        state["capability_flags"] = [f["text"] for f in me["capability_flags"]]
        state["evidence_flags"] = [f["text"] for f in me["evidence_flags"]]
    with open(base + "_state.json", "w") as f:
        json.dump(state, f, indent=2, ensure_ascii=False)

    rep = subprocess.run(["replay/target/release/dbc-replay", "dbc.sqlite", "risk", config[:12]],
                         capture_output=True, text=True)
    with open(base + "_risk.txt", "w") as f:
        f.write(rep.stdout + rep.stderr)

    print(f"rows: {len(rows)}; files: {base}_txs.csv, {base}_state.json, {base}_risk.txt")
    print(json.dumps({k: state.get(k) for k in ("quote_reserve_now_sol", "migration_threshold_sol", "curve_progress_pct",
                                                 "hours_since_creation", "graduated_utc")}, ensure_ascii=False))


if __name__ == "__main__":
    main()
