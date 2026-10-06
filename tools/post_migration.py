#!/usr/bin/env python3
"""Кто вносит и кто забирает SOL в пулах Meteora DAMM v2 после миграции с DBC.

Запуск из корня репозитория (нужны dbc.sqlite, .env и свежий risk --json):
    replay/target/release/dbc-replay dbc.sqlite risk --json > /tmp/risk_new.json
    python3 post_migration.py [cluster_id] [pools] [max_tx_per_pool] [-v]
    (-v: печатать каждую транзакцию после завершения кривой)

Для выборки выпустившихся пулов кластера:
  * по истории токена (минта) после graduation находится пул DAMM v2 (аккаунт программы DAMM v2
    в первой его транзакции), затем загружается полная история пула: создание при миграции,
    выводы ликвидности, свопы (в истории минта часть транзакций отсутствует — минт бывает
    в lookup-таблице; DexScreener не показывает пулы, из которых выведена ликвидность);
  * по каждой транзакции считается изменение SOL (включая wSOL) и токена у подписанта:
    покупка, продажа, добавление или вывод ликвидности;
  * подписанты делятся на связанных (создатели пулов кластера, получатели комиссий и остатка,
    кошельки фермы — торговали на кривой в 3+ пулах кластера) и внешних.
Декодер DAMM v2 не нужен: используются только балансы до и после транзакции.
"""
import json
import os
import sqlite3
import sys
import time
import urllib.request
from collections import defaultdict

VERBOSE = "-v" in sys.argv
ARGS = [a for a in sys.argv[1:] if a != "-v"]
CLUSTER = ARGS[0] if len(ARGS) > 0 else "5"
SAMPLE = int(ARGS[1]) if len(ARGS) > 1 else 15
MAX_TX = int(ARGS[2]) if len(ARGS) > 2 else 600
RPS = float(os.environ.get("POST_RPS", "8"))  # Helius free: до 10 запросов в секунду
MIN_SOL = 5_000_000  # 0,005 SOL: меньшие изменения — комиссии сети и рента, а не сделка
DAMM_V2 = "cpamdpZCGKUy5JxQXB4dcpGPiikHawvSWAd6mEn1sGG"
DBC = "dbcij3LWUppWqq96dh6gJWwBifmcGfLSB5D4DuSMaqN"
WSOL = "So11111111111111111111111111111111111111112"
B58 = "123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz"


def b58(b: bytes) -> str:
    n = int.from_bytes(b, "big")
    s = ""
    while n:
        n, r = divmod(n, 58)
        s = B58[r] + s
    return "1" * (len(b) - len(b.lstrip(b"\0"))) + s


def env(key, default):
    try:
        for line in open(".env"):
            if line.startswith(key + "="):
                v = line.split("=", 1)[1].strip()
                if v:
                    return v
    except OSError:
        pass
    return default


# нужна полная история: публичные узлы (PublicNode) хранят историю адресов лишь 1–2 суток,
# поэтому сначала HISTORY_RPC_URL (Helius), иначе RPC_URL
RPC = env("HISTORY_RPC_URL", env("RPC_URL", "https://solana-rpc.publicnode.com"))
RPC_HOST = RPC.split("://", 1)[-1].split("/", 1)[0].split("?", 1)[0]  # без ключа — для вывода
_last = [0.0]


def http_json(url, body=None):
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(url, data=data, headers={"Content-Type": "application/json", "User-Agent": "dbc-radar"})
    for attempt in range(4):
        try:
            with urllib.request.urlopen(req, timeout=30) as r:
                return json.load(r)
        except Exception as e:  # noqa: BLE001
            if attempt == 3:
                raise
            time.sleep(1.5 * (attempt + 1))


def rpc(method, params):
    wait = 1.0 / RPS - (time.time() - _last[0])
    if wait > 0:
        time.sleep(wait)
    _last[0] = time.time()
    v = http_json(RPC, {"jsonrpc": "2.0", "id": 1, "method": method, "params": params})
    if "error" in v:
        raise RuntimeError(f"{method}: {v['error']}")
    return v.get("result")


def signatures_after(address, since):
    """Успешные транзакции адреса не раньше `since`, от старых к новым (до 3 страниц по 1000)."""
    out, before = [], None
    for _ in range(3):
        opts = {"limit": 1000}
        if before:
            opts["before"] = before
        page = rpc("getSignaturesForAddress", [address, opts]) or []
        if not page:
            break
        for s in page:
            if (s.get("blockTime") or 0) >= since and s.get("err") is None:
                out.append(s)
        if (page[-1].get("blockTime") or 0) < since or len(page) < 1000:
            break
        before = page[-1]["signature"]
    out.reverse()
    return out


def uses(tx, program):
    return program in tx_keys(tx)


def tx_keys(tx):
    keys = list(tx["transaction"]["message"]["accountKeys"])
    loaded = tx["meta"].get("loadedAddresses") or {}
    return keys + loaded.get("writable", []) + loaded.get("readonly", [])


def find_damm_pool(tx):
    """Пул DAMM v2 в транзакции: единственный аккаунт, принадлежащий программе DAMM v2."""
    for k in tx_keys(tx):
        if k in (DAMM_V2, DBC, WSOL):
            continue
        info = rpc("getAccountInfo", [k, {"encoding": "base64", "dataSlice": {"offset": 0, "length": 0}}])
        v = (info or {}).get("value")
        if v and v.get("owner") == DAMM_V2 and v.get("space", 0) >= 1000:
            return k
    return None


def deltas(tx, signer, mint):
    """(изменение SOL с учётом wSOL, изменение токена) у подписанта, в лампортах и единицах токена."""
    meta = tx["meta"]
    sol = meta["postBalances"][0] - meta["preBalances"][0]

    def owned(entries, m):
        return sum(int(e["uiTokenAmount"]["amount"]) for e in entries or [] if e.get("owner") == signer and e.get("mint") == m)

    sol += owned(meta.get("postTokenBalances"), WSOL) - owned(meta.get("preTokenBalances"), WSOL)
    tok = owned(meta.get("postTokenBalances"), mint) - owned(meta.get("preTokenBalances"), mint)
    return sol, tok


def kind(sol, tok):
    if abs(sol) < MIN_SOL:
        return "transfer" if tok else "other"
    if tok > 0 and sol < 0:
        return "buy"
    if tok < 0 and sol > 0:
        return "sell"
    if tok > 0 and sol > 0:
        return "remove_liq"
    if tok < 0 and sol < 0:
        return "add_liq"
    return "sol_only"


def main():
    rows = json.load(open("/tmp/risk_new.json"))
    cfgs = [r["config"] for r in rows if str(r["cluster"]) == CLUSTER]
    if not cfgs:
        sys.exit(f"cluster #{CLUSTER} not found in /tmp/risk_new.json")
    db = sqlite3.connect("dbc.sqlite")
    ph = ",".join("?" * len(cfgs))

    # связанные адреса: создатели пулов, получатели комиссий и остатка, ферма (3+ пулов кластера)
    linked = {r[0] for r in db.execute(f"SELECT DISTINCT creator FROM pools WHERE config IN ({ph})", cfgs)}
    for fc, raw in db.execute(f"SELECT fee_claimer, raw FROM configs WHERE config IN ({ph})", cfgs):
        if fc:
            linked.add(fc)
        if raw and len(raw) >= 104:
            linked.add(b58(bytes(raw[72:104])))
    farm = {r[0] for r in db.execute(
        f"SELECT fee_payer FROM swaps WHERE config IN ({ph}) GROUP BY fee_payer HAVING COUNT(DISTINCT pool) >= 3", cfgs)}
    linked |= farm
    print(f"cluster #{CLUSTER}: {len(cfgs)} configs, {len(linked)} linked addresses ({len(farm)} farm wallets); RPC host {RPC_HOST}")

    now = int(time.time())
    sample = db.execute(
        f"""SELECT p.pool, p.base_mint, c.block_time FROM pools p JOIN curve_complete c ON c.pool = p.pool
            WHERE p.config IN ({ph}) AND p.tracked = 1 AND p.created_time BETWEEN ? AND ?
            ORDER BY RANDOM() LIMIT ?""",
        cfgs + [now - 5 * 86400, now - 86400, SAMPLE]).fetchall()

    total = defaultdict(float)
    print(f"\n{'token':<9} {'tx':>5} {'span':>7}  {'LP out':>7}  {'farm vol':>8} {'farm net':>8}  "
          f"{'ext vol':>7} {'ext net':>8} {'ext buys':>8}  {'linked share':>12}")
    for pool, mint, grad_time in sample:
        try:
            # 1) история минта после завершения кривой — чтобы найти пул DAMM v2;
            # 2) история самого пула: создание при миграции, выводы ликвидности и свопы
            sigs = signatures_after(mint, grad_time)[:MAX_TX]
            damm = None
            for s0 in sigs:
                tx0 = rpc("getTransaction", [s0["signature"], {"encoding": "json", "maxSupportedTransactionVersion": 1}])
                if tx0 and uses(tx0, DAMM_V2):
                    damm = find_damm_pool(tx0)
                    if damm:
                        break
            if damm:
                sigs = signatures_after(damm, grad_time - 600)[:MAX_TX]
            if VERBOSE:
                print(f"  {mint[:8]}: DAMM v2 pool {damm or 'not found'}; {len(sigs)} transactions")
        except Exception as e:  # noqa: BLE001
            print(f"{mint[:8]:<9} rpc error: {e}")
            continue
        st = defaultdict(float)
        last_t = grad_time
        for s in sigs:
            try:
                tx = rpc("getTransaction", [s["signature"], {"encoding": "json", "maxSupportedTransactionVersion": 1}])
            except Exception as e:  # noqa: BLE001
                st["errors"] += 1
                if VERBOSE:
                    print(f"    {s['signature'][:10]}  error: {str(e)[:80]}")
                continue
            if not tx:
                continue
            signer = tx["transaction"]["message"]["accountKeys"][0]
            in_damm = uses(tx, DAMM_V2)
            in_dbc = uses(tx, DBC)
            sol, tok = deltas(tx, signer, mint)
            k = kind(sol, tok)
            t = tx.get("blockTime") or 0
            if VERBOSE:
                progs = "+".join(n for n, f in (("DBC", in_dbc), ("DAMM", in_damm)) if f) or "-"
                print(f"    t{t - grad_time:>+7}s  {s['signature'][:10]}  {signer[:8]} {'L' if signer in linked else 'E'}  "
                      f"{progs:<8}  SOL {sol / 1e9:+9.4f}  token {tok:+d}  {k}")
            if in_dbc and not in_damm:
                continue  # сделка на кривой DBC, не после миграции
            last_t = max(last_t, t)
            v = sol / 1e9
            if signer in linked:
                if k == "remove_liq" or (in_dbc and v > 0):
                    st["lp_out"] += v  # вывод ликвидности (в т.ч. пакетом с миграцией)
                elif k in ("buy", "sell"):
                    st["farm_vol"] += abs(v)
                    st["farm_net"] += v
            elif k in ("buy", "sell"):
                st["ext_vol"] += abs(v)
                st["ext_net"] += v
                st["ext_buys"] += k == "buy"
        vol = st["farm_vol"] + st["ext_vol"]
        share = f"{100 * st['farm_vol'] / vol:.0f}%" if vol else "-"
        trunc = "+" if len(sigs) >= MAX_TX else ""
        for k2 in ("lp_out", "farm_vol", "farm_net", "ext_vol", "ext_net", "ext_buys", "errors"):
            total[k2] += st[k2]
        total["pools"] += 1
        print(f"{mint[:8]:<9} {len(sigs):>4}{trunc:<1} {last_t - grad_time:>6}s  {st['lp_out']:>7.3f}  "
              f"{st['farm_vol']:>8.2f} {st['farm_net']:>+8.3f}  {st['ext_vol']:>7.3f} {st['ext_net']:>+8.3f} "
              f"{int(st['ext_buys']):>8}  {share:>12}")

    if total["pools"]:
        n = total["pools"]
        vol = total["farm_vol"] + total["ext_vol"]
        print(f"\nTOTAL over {int(n)} pools: LP withdrawn by linked {total['lp_out']:.2f} SOL; "
              f"farm volume {total['farm_vol']:.1f} SOL (net {total['farm_net']:+.2f}); "
              f"external volume {total['ext_vol']:.2f} SOL (net {total['ext_net']:+.3f}, {int(total['ext_buys'])} buys); "
              f"linked share of DAMM v2 volume {100 * total['farm_vol'] / vol if vol else 0:.0f}%"
              + (f"; {int(total['errors'])} tx errors" if total["errors"] else ""))
    print("notes: history of the DAMM v2 pool from 10 min before curve completion; DBC-only transactions skipped;"
          "\n       span = last transaction counted, seconds after completion ('+' after tx = limit reached);"
          "\n       LP out = SOL received by linked addresses in liquidity removals (incl. the migration bundle);"
          "\n       farm/ext net = SOL received minus SOL spent in trades (negative = paid into the pool);"
          "\n       changes under 0.005 SOL are treated as fees or transfers, not trades.")

if __name__ == "__main__":
    main()
