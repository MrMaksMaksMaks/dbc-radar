#!/usr/bin/env python3
"""Кто вносит и кто забирает SOL в пулах Meteora DAMM v2 после миграции с DBC.

Запуск из корня репозитория (нужны dbc.sqlite, .env и свежий risk --json):
    replay/target/release/dbc-replay dbc.sqlite risk --json > /tmp/risk_new.json
    python3 post_migration.py [cluster_id] [pools] [max_tx_per_pool] [-v] [--tokens=PREFIX1,PREFIX2] [--ungraduated]
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
# --tokens=7Akd4rsT,AwmnQC3U — разобрать конкретные токены (начала адресов минтов)
# --ungraduated — брать и невыпустившиеся пулы (для них разбирается только кривая)
UNGRADUATED = "--ungraduated" in sys.argv
TOKENS = next((a.split("=", 1)[1].split(",") for a in sys.argv[1:] if a.startswith("--tokens=")), [])
ARGS = [a for a in sys.argv[1:] if a not in ("-v", "--ungraduated") and not a.startswith("--tokens=")]
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


def signatures_after(address, since, limit=3000):
    """Успешные транзакции адреса не раньше `since`, от старых к новым (страницами по 1000)."""
    out, before = [], None
    for _ in range(limit // 1000 + 1):
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
    # ферма — как в движке: торговал в 3+ пулах кластера, и это не меньше 80% всех пулов,
    # где кошелёк торговал во всей базе (иначе это общий бот, покупающий все новые токены)
    in_cluster = dict(db.execute(
        f"SELECT fee_payer, COUNT(DISTINCT pool) FROM swaps WHERE config IN ({ph}) GROUP BY fee_payer HAVING COUNT(DISTINCT pool) >= 3", cfgs))
    farm = set()
    if in_cluster:
        wph = ",".join("?" * len(in_cluster))
        for w, total_pools in db.execute(
                f"SELECT fee_payer, COUNT(DISTINCT pool) FROM swaps WHERE fee_payer IN ({wph}) GROUP BY fee_payer", list(in_cluster)):
            if in_cluster[w] >= 0.8 * total_pools:
                farm.add(w)
    bots = len(in_cluster) - len(farm)
    linked |= farm
    print(f"cluster #{CLUSTER}: {len(cfgs)} configs, {len(linked)} linked addresses ({len(farm)} farm wallets; "
          f"{bots} wallets active in 3+ cluster pools but mostly elsewhere treated as external); RPC host {RPC_HOST}")

    now = int(time.time())
    if TOKENS:
        sample = []
        for pre in TOKENS:
            sample += db.execute(
                """SELECT p.pool, p.base_mint, c.block_time, p.created_time FROM pools p
                   LEFT JOIN curve_complete c ON c.pool = p.pool WHERE p.base_mint LIKE ? LIMIT 1""", (pre + "%",)).fetchall()
    else:
        sample = db.execute(
        f"""SELECT p.pool, p.base_mint, c.block_time, p.created_time FROM pools p
            {"LEFT JOIN" if UNGRADUATED else "JOIN"} curve_complete c ON c.pool = p.pool
            WHERE p.config IN ({ph}) AND p.tracked = 1 AND p.created_time BETWEEN ? AND ?
            ORDER BY RANDOM() LIMIT ?""",
        cfgs + [now - 5 * 86400, now - 86400, SAMPLE]).fetchall()

    total = defaultdict(float)
    print(f"\n{'token':<9} {'curve tx':>8} {'damm tx':>8} {'span':>8} | {'op curve':>8} {'LP out':>7} {'op damm':>8} "
          f"{'farm vol':>8} | {'OPERATOR':>8} | {'ext curve':>9} {'ext damm':>8} {'ext buys':>8}")
    for pool, mint, grad_time, created in sample:
        graduated = grad_time is not None
        if not graduated:
            grad_time = int(time.time())  # кривая не завершена: разбираем только её
        try:
            # полный цикл: история пула DBC (создание, кривая, миграция) и пула DAMM v2 после неё
            curve_sigs = signatures_after(pool, (created or grad_time) - 120, MAX_TX)[:MAX_TX]
            sigs = signatures_after(mint, grad_time)[:200] if graduated else []
            damm = None
            for s0 in sigs:
                tx0 = rpc("getTransaction", [s0["signature"], {"encoding": "json", "maxSupportedTransactionVersion": 1}])
                if tx0 and uses(tx0, DAMM_V2):
                    damm = find_damm_pool(tx0)
                    if damm:
                        break
            damm_sigs = signatures_after(damm, grad_time - 600, MAX_TX)[:MAX_TX] if damm else []
            if VERBOSE:
                print(f"  {mint[:8]}: DBC pool {pool}, {len(curve_sigs)} tx; DAMM v2 pool {damm or 'not found'}, {len(damm_sigs)} tx")
        except Exception as e:  # noqa: BLE001
            print(f"{mint[:8]:<9} rpc error: {e}")
            continue
        st = defaultdict(float)
        seen_sig = set()
        last_t = grad_time
        for s in curve_sigs + damm_sigs:
            if s["signature"] in seen_sig:
                continue  # пакет миграции есть в обеих историях
            seen_sig.add(s["signature"])
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
            v = sol / 1e9
            phase = "migr" if (in_dbc and in_damm) else ("damm" if in_damm else "curve")
            is_l = signer in linked
            if VERBOSE:
                print(f"    t{t - grad_time:>+8}s  {s['signature'][:10]}  {signer[:8]} {'L' if is_l else 'E'}  "
                      f"{phase:<5}  SOL {v:+9.4f}  token {tok:+d}  {k}")
            if phase != "curve":
                last_t = max(last_t, t)
            if is_l:
                if phase == "curve":
                    st["op_curve"] += v  # всё, включая создание пула, торговлю фермы и комиссии сети
                elif phase == "migr" or k == "remove_liq":
                    st["lp_out"] += v
                else:
                    st["op_damm"] += v
                    if k in ("buy", "sell"):
                        st["farm_vol"] += abs(v)
            elif k in ("buy", "sell"):
                st["ext_" + ("curve" if phase == "curve" else "damm")] += v
                st["ext_buys"] += k == "buy" and phase != "curve"
        op = st["op_curve"] + st["lp_out"] + st["op_damm"]
        for k2 in ("op_curve", "lp_out", "op_damm", "farm_vol", "ext_curve", "ext_damm", "ext_buys", "errors"):
            total[k2] += st[k2]
        total["op"] += op
        total["pools"] += 1
        tc = f"{len(curve_sigs)}{'+' if len(curve_sigs) >= MAX_TX else ''}"
        td = f"{len(damm_sigs)}{'+' if len(damm_sigs) >= MAX_TX else ''}"
        span = f"{last_t - grad_time:>7}s" if graduated else "  no grad"
        print(f"{mint[:8]:<9} {tc:>8} {td:>8} {span} | {st['op_curve']:>+8.3f} {st['lp_out']:>7.3f} "
              f"{st['op_damm']:>+8.3f} {st['farm_vol']:>8.1f} | {op:>+8.3f} | {st['ext_curve']:>+9.3f} {st['ext_damm']:>+8.3f} "
              f"{int(st['ext_buys']):>8}")

    if total["pools"]:
        n = total["pools"]
        print(f"\nTOTAL over {int(n)} pools: operator net {total['op']:+.3f} SOL ({total['op'] / n:+.3f} per launch) = "
              f"curve {total['op_curve']:+.2f} + LP out {total['lp_out']:+.2f} + DAMM trading {total['op_damm']:+.2f}; "
              f"farm volume in DAMM v2 {total['farm_vol']:.1f} SOL; externals net {total['ext_curve'] + total['ext_damm']:+.3f} SOL "
              f"(curve {total['ext_curve']:+.3f}, DAMM {total['ext_damm']:+.3f}, {int(total['ext_buys'])} DAMM buys)"
              + (f"; {int(total['errors'])} tx errors" if total["errors"] else ""))
    print("notes: full cycle per token: DBC pool history (creation, curve, migration bundle) + DAMM v2 pool history;"
          "\n       SOL is the signer's balance change incl. network fees; positive = received, negative = paid;"
          "\n       OPERATOR = all linked signers together (creator, receivers, farm); externals = everyone else;"
          "\n       '+' after a tx count = limit reached (raise max_tx); span = last DAMM/migration tx after completion;"
          "\n       farm funding transfers and leftover tokens (unsold supply) are not counted.")

if __name__ == "__main__":
    main()
