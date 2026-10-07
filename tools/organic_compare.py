#!/usr/bin/env python3
"""Вердикты DBC Radar против Organic Score Jupiter на одних и тех же токенах.

Запуск из корня репозитория (нужны dbc.sqlite, свежий /tmp/risk_new.json и ключ Jupiter
в .env: JUPITER_API_KEY=..., ключ выдают на portal.jup.ag):

    replay/target/release/dbc-replay dbc.sqlite risk --json > /tmp/risk_new.json
    python3 tools/organic_compare.py [--per-verdict=30] [--min-hours=2] [--max-hours=48] [--min-trades=5]

Выборка честная по возрасту и активности: из каждого вердикта (RED, RED-LINK, AMBER, SELF-GRAD,
GREEN) берутся случайные токены отслеживаемых пулов одного окна возраста и не меньше
min-trades сделок в базе. Organic Score относительный (нормирован по экосистеме), поэтому
сравниваются группы одного возраста, а не отдельные числа.

Пишет out/organic_compare.csv (по строке на токен) и печатает сводку по вердиктам и токены,
где оценки расходятся сильнее всего.
"""
import csv
import json
import os
import random
import sqlite3
import statistics
import sys
import time
import urllib.request

VERDICTS = ["RED", "RED-LINK", "AMBER", "SELF-GRAD", "GREEN"]
API = "https://api.jup.ag/tokens/v2/search?query="
BATCH = 50


def opt(name, default):
    for a in sys.argv[1:]:
        if a.startswith(f"--{name}="):
            return a.split("=", 1)[1]
    return default


def env(key):
    try:
        for line in open(".env"):
            if line.startswith(key + "="):
                return line.split("=", 1)[1].strip()
    except OSError:
        pass
    return os.environ.get(key, "")


def jupiter(mints, key):
    out = {}
    for i in range(0, len(mints), BATCH):
        part = mints[i:i + BATCH]
        req = urllib.request.Request(API + ",".join(part), headers={"x-api-key": key, "User-Agent": "dbc-radar"})
        for attempt in range(4):
            try:
                with urllib.request.urlopen(req, timeout=30) as r:
                    for t in json.load(r) or []:
                        if t.get("id") in part:
                            out[t["id"]] = t
                break
            except urllib.error.HTTPError as e:
                if e.code == 401:
                    sys.exit("Jupiter: 401 — check JUPITER_API_KEY in .env (key from portal.jup.ag)")
                time.sleep(2 * (attempt + 1))
            except Exception:  # noqa: BLE001
                time.sleep(2 * (attempt + 1))
        time.sleep(1.1)  # бережно к лимитам бесплатного ключа
    return out


def main():
    key = env("JUPITER_API_KEY")
    if not key:
        sys.exit("set JUPITER_API_KEY=... in .env (key from portal.jup.ag)")
    per = int(opt("per-verdict", "30"))
    min_h, max_h, min_tr = float(opt("min-hours", "2")), float(opt("max-hours", "48")), int(opt("min-trades", "5"))
    random.seed(int(opt("seed", "7")))

    verdict_of = {r["config"]: r["verdict"] for r in json.load(open("/tmp/risk_new.json"))}
    db = sqlite3.connect("dbc.sqlite")
    now = int(time.time())
    rows = db.execute(
        """SELECT p.base_mint, p.config, p.created_time,
                  (SELECT COUNT(*) FROM swaps s WHERE s.pool = p.pool) AS trades
           FROM pools p WHERE p.tracked = 1 AND p.created_time BETWEEN ? AND ?""",
        (now - int(max_h * 3600), now - int(min_h * 3600))).fetchall()
    groups = {v: [] for v in VERDICTS}
    for mint, cfg, created, trades in rows:
        v = verdict_of.get(cfg)
        if v in groups and trades >= min_tr:
            groups[v].append((mint, cfg, created, trades))
    sample = []
    for v in VERDICTS:
        random.shuffle(groups[v])
        sample += [(v, *x) for x in groups[v][:per]]
    print(f"tokens {min_h:g}-{max_h:g} h old with >= {min_tr} trades: "
          + ", ".join(f"{v} {len(groups[v])}" for v in VERDICTS) + f"; sampling up to {per} per verdict")

    info = jupiter([s[1] for s in sample], key)
    os.makedirs("out", exist_ok=True)
    with open("out/organic_compare.csv", "w", newline="") as f:
        w = csv.writer(f)
        w.writerow(["verdict", "mint", "config", "hours_old", "trades_in_dbc_radar", "found_in_jupiter", "organic_score",
                    "organic_label", "is_sus", "is_verified", "holders", "liquidity_usd", "mcap_usd"])
        for v, mint, cfg, created, trades in sample:
            t = info.get(mint, {})
            w.writerow([v, mint, cfg, round((now - created) / 3600, 1), trades, bool(t), t.get("organicScore"),
                        t.get("organicScoreLabel"), "isSus" in (t.get("audit") or {}), t.get("isVerified"),
                        t.get("holderCount"), t.get("liquidity"), t.get("mcap")])

    print(f"\n{'verdict':<10} {'tokens':>6} {'in Jup':>6} | {'median':>6} {'mean':>6} | {'<10':>4} {'10-30':>5} {'30-60':>5} {'60+':>4} | "
          f"{'label low/med/high':>18} | {'isSus':>5}")
    for v in VERDICTS:
        ts = [info[s[1]] for s in sample if s[0] == v and s[1] in info]
        n = sum(1 for s in sample if s[0] == v)
        sc = [t["organicScore"] for t in ts if t.get("organicScore") is not None]
        if not n:
            continue
        b = [sum(1 for x in sc if lo <= x < hi) for lo, hi in ((0, 10), (10, 30), (30, 60), (60, 101))]
        lab = [sum(1 for t in ts if t.get("organicScoreLabel") == l) for l in ("low", "medium", "high")]
        sus = sum(1 for t in ts if "isSus" in (t.get("audit") or {}))
        med = f"{statistics.median(sc):6.1f}" if sc else "     -"
        mean = f"{statistics.mean(sc):6.1f}" if sc else "     -"
        print(f"{v:<10} {n:>6} {len(ts):>6} | {med} {mean} | {b[0]:>4} {b[1]:>5} {b[2]:>5} {b[3]:>4} | "
              f"{'/'.join(map(str, lab)):>18} | {sus:>5}")

    # расхождения: что разбирать вручную
    red = [(info[s[1]].get("organicScore") or 0, s) for s in sample if s[0] in ("RED", "RED-LINK") and s[1] in info]
    green = [(info[s[1]].get("organicScore") or 0, s) for s in sample if s[0] == "GREEN" and s[1] in info]
    print("\nRED / RED-LINK with the highest Organic Score (check: what Jupiter counts as organic here):")
    for sc, s in sorted(red, reverse=True)[:5]:
        print(f"  {s[1]}  score {sc:5.1f}  config {s[2][:8]}  trades {s[4]}")
    print("GREEN with the lowest Organic Score:")
    for sc, s in sorted(green)[:5]:
        print(f"  {s[1]}  score {sc:5.1f}  config {s[2][:8]}  trades {s[4]}")
    print("\nnotes: Organic Score is relative (normalised across the ecosystem) — compare groups, not single numbers;"
          "\n       'in Jup' = tokens Jupiter returned; tokens it does not index have no score;"
          "\n       full rows: out/organic_compare.csv")


if __name__ == "__main__":
    main()
