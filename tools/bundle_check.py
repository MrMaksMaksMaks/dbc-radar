#!/usr/bin/env python3
"""Пакетный запуск или нет: кто пополнил кошельки, купившие в первые слоты пула.

Запуск из корня репозитория (нужны dbc.sqlite и .env с HISTORY_RPC_URL на Helius):

    python3 tools/bundle_check.py --tokens=BQxzR4pL,7Akd4rsT        # конкретные токены
    python3 tools/bundle_check.py --fast=10 [--max-secs=5]           # 10 случайных пулов, где кривая
                                                                     # заполнилась за <= 5 с
Опции: --slots=2 (сколько слотов после создания считать «первыми»), --buyers=20 (максимум
покупателей на пул), -v (по каждому покупателю).

Для каждого покупателя первых слотов берётся самая ранняя транзакция кошелька, в которой на него
пришли SOL, и отправитель этих SOL — «спонсор». Пакетный запуск выглядит так: несколько покупателей
пополнены с одного адреса, часто — с адреса создателя или его спонсора, незадолго до запуска.
Ложное совпадение — горячий кошелёк биржи: у такого спонсора тысячи транзакций, он помечается.
"""
import os
import sqlite3
import sys
import time
from collections import Counter, defaultdict

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
_saved = sys.argv
sys.argv = [sys.argv[0]]  # post_migration разбирает argv при импорте — даём ему пустой
import post_migration as pm  # noqa: E402  (rpc, signatures_after, owner_deltas, POOL_AUTHORITIES)
sys.argv = _saved


def opt(name, default):
    for a in sys.argv[1:]:
        if a.startswith(f"--{name}="):
            return a.split("=", 1)[1]
    return default


VERBOSE = "-v" in sys.argv
SLOTS = int(opt("slots", "2"))
MAX_BUYERS = int(opt("buyers", "20"))
TX_OPTS = {"encoding": "json", "maxSupportedTransactionVersion": 1}
_funder_cache = {}
_busy_cache = {}


def funder_of(wallet):
    """(спонсор, время пополнения, SOL) — первая транзакция кошелька, где на него пришли SOL >= 0,005."""
    if wallet in _funder_cache:
        return _funder_cache[wallet]
    sigs, before = [], None
    for _ in range(3):  # свежие кошельки — несколько транзакций; старые — не дальше 3000
        o = {"limit": 1000}
        if before:
            o["before"] = before
        page = pm.rpc("getSignaturesForAddress", [wallet, o]) or []
        sigs += page
        if len(page) < 1000:
            break
        before = page[-1]["signature"]
    result = (None, None, 0.0, len(sigs))
    for s in reversed(sigs[-8:]):  # 8 самых ранних транзакций, от старой к новой
        if s.get("err") is not None:
            continue
        tx = pm.rpc("getTransaction", [s["signature"], TX_OPTS])
        if not tx:
            continue
        sol, _ = pm.owner_deltas(tx, "")
        got = sol.get(wallet, 0)
        if got >= 5_000_000:
            payers = [(o, d) for o, d in sol.items() if d < 0 and o != wallet and o not in pm.POOL_AUTHORITIES]
            if payers:
                f = min(payers, key=lambda x: x[1])[0]
                result = (f, tx.get("blockTime"), got / 1e9, len(sigs))
                break
    _funder_cache[wallet] = result
    return result


def busy(addr):
    """Сколько транзакций у адреса (до 1000): у биржевых кошельков — максимум."""
    if addr not in _busy_cache:
        _busy_cache[addr] = len(pm.rpc("getSignaturesForAddress", [addr, {"limit": 1000}]) or [])
    return _busy_cache[addr]


def main():
    db = sqlite3.connect("dbc.sqlite")
    tokens = [t for t in opt("tokens", "").split(",") if t]
    if tokens:
        sample = []
        for t in tokens:
            sample += db.execute("SELECT pool, base_mint, creator, created_time, created_slot FROM pools WHERE base_mint LIKE ? LIMIT 1",
                                 (t + "%",)).fetchall()
    else:
        n, max_secs = int(opt("fast", "10")), int(opt("max-secs", "5"))
        sample = db.execute(
            """SELECT p.pool, p.base_mint, p.creator, p.created_time, p.created_slot FROM pools p
               JOIN curve_complete c ON c.pool = p.pool
               WHERE c.block_time - p.created_time <= ? AND p.created_time > strftime('%s','now') - 3*86400
               ORDER BY RANDOM() LIMIT ?""", (max_secs, n)).fetchall()
    print(f"RPC host {pm.RPC_HOST}; first {SLOTS} slot(s) after creation, up to {MAX_BUYERS} buyers per pool\n")
    print(f"{'token':<9} {'buyers':>6} {'SOL':>7} | {'top sponsor':<10} {'buyers':>6} {'SOL':>7} {'tx':>5} {'is':<16} "
          f"{'funded before launch':>20} | verdict")
    for pool, mint, creator, created, cslot in sample:
        sigs = pm.signatures_after(pool, (created or 0) - 60, 3000)  # от старых к новым
        buyers = defaultdict(float)  # владелец -> SOL, потраченные в первых слотах
        for s in sigs:
            if s.get("slot", 0) > (cslot or 0) + SLOTS:
                break
            tx = pm.rpc("getTransaction", [s["signature"], TX_OPTS])
            if not tx:
                continue
            sol, tok = pm.owner_deltas(tx, mint)
            for o, t in tok.items():
                if t > 0 and sol.get(o, 0) < 0 and o not in pm.POOL_AUTHORITIES:
                    buyers[o] += -sol[o] / 1e9
        others = [b for b in buyers if b != creator][:MAX_BUYERS]
        creator_funder = funder_of(creator)[0] if creator else None
        by_sponsor = defaultdict(list)
        for b in others:
            f, ft, amt, ntx = funder_of(b)
            by_sponsor[f].append((b, ft, amt))
            if VERBOSE:
                age = f"{(created - ft) / 60:.0f} min before launch" if (ft and created) else "-"
                print(f"    {mint[:8]} buyer {b[:8]} spent {buyers[b]:.3f} SOL; sponsor {str(f)[:8]} ({amt:.3f} SOL, {age})")
        total_sol = sum(buyers[b] for b in others)
        top, group = max(((f, g) for f, g in by_sponsor.items() if f), key=lambda x: len(x[1]), default=(None, []))
        if top:
            ntx = busy(top)
            kind = ("creator" if top == creator else "creator's sponsor" if top == creator_funder
                    else "busy (exchange?)" if ntx >= 1000 else "other")
            gsol = sum(buyers[b] for b, _, _ in group)
            times = [ft for _, ft, _ in group if ft]
            before = f"{(created - min(times)) / 60:.0f}-{(created - max(times)) / 60:.0f} min" if (times and created) else "-"
            bundle = len(group) >= 3 and kind != "busy (exchange?)"
            verdict = "BUNDLE-LIKE" if bundle else ("shared sponsor" if len(group) >= 2 else "independent")
        else:
            ntx, kind, gsol, before, verdict = 0, "-", 0.0, "-", "no data"
        print(f"{mint[:8]:<9} {len(others):>6} {total_sol:>7.2f} | {str(top)[:8]:<10} {len(group):>6} {gsol:>7.2f} {ntx:>5} "
              f"{kind:<16} {before:>20} | {verdict}")
    print("\nnotes: buyers = wallets other than the creator whose token balance grew and SOL fell in the first slots;"
          "\n       sponsor = sender of SOL in the earliest transaction where the wallet received >= 0.005 SOL;"
          "\n       'busy (exchange?)' = sponsor with 1000+ transactions — likely an exchange hot wallet, not an operator;"
          "\n       BUNDLE-LIKE = 3+ early buyers funded by the same non-busy address.")


if __name__ == "__main__":
    main()
