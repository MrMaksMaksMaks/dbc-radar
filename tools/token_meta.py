#!/usr/bin/env python3
"""Метаданные токенов по группам: откуда запуски (платформа, домен метаданных, ссылки).

Запуск из корня репозитория (нужны dbc.sqlite, .env с HISTORY_RPC_URL на Helius и
свежий /tmp/risk_new.json от `replay/target/release/dbc-replay dbc.sqlite risk --json`):

    python3 tools/token_meta.py [N] GROUP [GROUP ...] [-v]

GROUP:
    cluster=5          — токены конфигов кластера оператора #5
    verdict=GREEN      — токены конфигов с этим вердиктом (GREEN, RED, AMBER, SELF-GRAD, RED-LINK)
    creator=ADDRESS    — токены одного создателя пулов
N — сколько токенов брать из каждой группы (по умолчанию 20), -v — построчно.

Имя, символ и адрес JSON берутся из DAS API Helius (getAssetBatch), сам JSON загружается
по ссылке; из него извлекаются поле платформы (createdOn и подобные), домены ссылок
(сайт, Twitter, Telegram) и домен, где хранятся метаданные.
"""
import collections
import json
import random
import sqlite3
import sys
import urllib.parse
import urllib.request

VERBOSE = "-v" in sys.argv
ARGS = [a for a in sys.argv[1:] if a != "-v"]
N = int(ARGS[0]) if ARGS and ARGS[0].isdigit() else 20
GROUPS = [a for a in ARGS if "=" in a]
IPFS_GATEWAYS = ("https://ipfs.io/ipfs/", "https://dweb.link/ipfs/", "https://gateway.pinata.cloud/ipfs/",
                 "https://nftstorage.link/ipfs/")
# управляющие и невидимые символы: смена направления текста, нулевой ширины
SUSPICIOUS_CHARS = {chr(c) for c in list(range(0x202A, 0x202F)) + list(range(0x2066, 0x206A)) + [0x200B, 0x200C, 0x200D, 0x200E, 0x200F, 0xFEFF]}
PLATFORM_KEYS = ("createdOn", "created_on", "platform", "launchpad", "source", "createdBy", "created_by")


def env(key, default=""):
    try:
        for line in open(".env"):
            if line.startswith(key + "="):
                v = line.split("=", 1)[1].strip()
                if v:
                    return v
    except OSError:
        pass
    return default


RPC = env("HISTORY_RPC_URL")
if "helius" not in RPC:
    sys.exit("HISTORY_RPC_URL must point to Helius (DAS API getAssetBatch)")


def post(body):
    req = urllib.request.Request(RPC, data=json.dumps(body).encode(), headers={"Content-Type": "application/json"})
    with urllib.request.urlopen(req, timeout=30) as r:
        return json.load(r)


def fetch_json(uri):
    """JSON метаданных; для IPFS — по очереди через несколько шлюзов."""
    if not uri:
        return None
    if uri.startswith("ipfs://"):
        cands = [g + uri[len("ipfs://"):] for g in IPFS_GATEWAYS]
    elif "/ipfs/" in uri:
        cid = uri.split("/ipfs/", 1)[1]
        cands = [uri] + [g + cid for g in IPFS_GATEWAYS if not uri.startswith(g)]
    else:
        cands = [uri]
    for u in cands:
        try:
            req = urllib.request.Request(u, headers={"User-Agent": "dbc-radar"})
            with urllib.request.urlopen(req, timeout=12) as r:
                return json.loads(r.read(200_000))
        except Exception:  # noqa: BLE001
            continue
    return None


def domain(url):
    try:
        if url.startswith("ipfs://"):
            return "ipfs"
        host = urllib.parse.urlparse(url).netloc.lower()
        return host[4:] if host.startswith("www.") else host
    except Exception:  # noqa: BLE001
        return ""


def urls_in(obj, out):
    """Все строки-ссылки во вложенном JSON (кроме картинок)."""
    if isinstance(obj, dict):
        for k, v in obj.items():
            if k in ("image", "animation_url", "uri", "files"):
                continue
            urls_in(v, out)
    elif isinstance(obj, list):
        for v in obj:
            urls_in(v, out)
    elif isinstance(obj, str) and obj.startswith(("http://", "https://")):
        out.append(obj)


def group_mints(db, risk, g):
    key, val = g.split("=", 1)
    if key == "creator":
        rows = db.execute("SELECT base_mint FROM pools WHERE creator = ?", (val,)).fetchall()
    else:
        if key == "cluster":
            cfgs = [r["config"] for r in risk if str(r["cluster"]) == val]
        elif key == "verdict":
            cfgs = [r["config"] for r in risk if r["verdict"] == val.upper()]
        else:
            sys.exit(f"unknown group {g}")
        if not cfgs:
            return []
        random.shuffle(cfgs)
        cfgs = cfgs[:300]  # для больших групп — случайные конфиги
        ph = ",".join("?" * len(cfgs))
        rows = db.execute(f"SELECT base_mint FROM pools WHERE config IN ({ph})", cfgs).fetchall()
    mints = [r[0] for r in rows]
    random.shuffle(mints)
    return mints[:N]


def main():
    db = sqlite3.connect("dbc.sqlite")
    risk = json.load(open("/tmp/risk_new.json"))
    for g in GROUPS:
        mints = group_mints(db, risk, g)
        if not mints:
            print(f"\n== {g}: no tokens")
            continue
        assets = post({"jsonrpc": "2.0", "id": 1, "method": "getAssetBatch", "params": {"ids": mints}}).get("result") or []
        uri_dom, platforms, link_doms, names = collections.Counter(), collections.Counter(), collections.Counter(), []
        no_links = no_json = suspicious = 0
        print(f"\n== {g}: {len(mints)} tokens")
        for a in assets:
            if not a:
                continue
            content = a.get("content") or {}
            meta = content.get("metadata") or {}
            uri = content.get("json_uri") or ""
            j = fetch_json(uri)
            uri_dom[domain(uri) or "-"] += 1
            if j is None:
                no_json += 1
                j = {}
            plat = next((str(j.get(k)) for k in PLATFORM_KEYS if j.get(k)), "")
            platforms[domain(plat) or plat or "-"] += 1
            links = []
            urls_in({k: v for k, v in j.items() if k not in PLATFORM_KEYS}, links)
            urls_in(content.get("links") or {}, links)  # ссылки, которые Helius извлёк из JSON
            if any(ch in SUSPICIOUS_CHARS for ch in str(meta.get("name", "")) + str(meta.get("symbol", ""))):
                suspicious += 1
            doms = sorted({domain(u) for u in links if domain(u)})
            for d in doms:
                link_doms[d] += 1
            if not doms:
                no_links += 1
            names.append(repr(meta.get("symbol", ""))[1:-1])  # управляющие символы видны как \u202e
            if VERBOSE:
                print(f"  {a.get('id', '')[:8]}  {meta.get('symbol', '')[:12]:<12} {meta.get('name', '')[:24]:<24} "
                      f"uri@{domain(uri) or '-':<22} platform {plat[:30] or '-':<30} links {', '.join(doms)[:60]}")
        print(f"  metadata hosted at: {uri_dom.most_common(5)}")
        print(f"  platform field:     {platforms.most_common(5)}")
        print(f"  link domains:       {link_doms.most_common(8)}")
        print(f"  without links: {no_links}, metadata JSON unavailable: {no_json}, "
              f"names/symbols with invisible or direction-control characters: {suspicious}")
        print(f"  symbols: {', '.join(names[:15])}")


if __name__ == "__main__":
    main()
