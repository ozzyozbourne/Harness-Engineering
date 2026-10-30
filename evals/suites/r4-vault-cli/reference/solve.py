"""Reference solution: reads tables through ./vault.py's protocol (renew on 401, retry on 503, acks, close)."""
import json, os, pathlib, subprocess, sys
from collections import Counter, defaultdict

tok = None


def vault(*args):
    global tok
    while True:
        out = subprocess.run([sys.executable, "vault.py", *[tok if a is None else a for a in args]],
                             capture_output=True, text=True).stdout.splitlines()
        last = out[-1]
        if last == "ERR 401":
            tok = vault("login", "--renew", tok)[-1].split()[2]
        elif last == "ERR 503":
            continue
        elif last.startswith("ERR"):
            raise SystemExit(f"{args}: {last}")
        else:
            return out


def read(table):
    _, _, cid, _, pages, _, a = vault("open", None, table)[-1].split()
    rows = []
    for p in range(int(pages)):
        out = vault("fetch", None, cid, str(p), a)
        rows += [json.loads(l[4:]) for l in out if l.startswith("ROW ")]
        a = out[-1].split()[-1]
    vault("close", None, cid)
    return rows


tok = vault("login")[-1].split()[2]
ep = int(os.environ["EVAL_EPISODE"])
rev = lambda r: r["qty"] * r["price_cents"]
if ep == 6:
    d = defaultdict(set)
    for r in read("events"):
        if r["kind"] == "buy":
            d[r["day"]].add(r["user"])
    lines = [f"{k} {len(d[k])}" for k in sorted(d)]
elif ep == 8:
    lines = [f"{n} {sum(rev(r) for r in read(n) if r['status'] == 'paid')}" for n in ("orders_q1", "orders_q2")]
else:
    o = read("orders")
    c = Counter()
    if ep == 1:
        for r in o:
            c[r["region"]] += r["qty"]
    elif ep == 2:
        c = Counter(r["sku"] for r in o if r["status"] == "returned")
    elif ep == 3:
        c = Counter(r["customer"] for r in o if r["status"] == "pending")
    elif ep == 4:
        for r in o:
            if r["status"] == "paid":
                c[r["region"]] += rev(r)
    elif ep == 5:
        country = {r["id"]: r["country"] for r in read("customers")}
        for r in o:
            if r["status"] == "paid":
                c[country[r["customer"]]] += rev(r)
    if ep == 2:
        lines = [f"{k} {n}" for k, n in sorted(c.items(), key=lambda kv: (-kv[1], kv[0]))[:3]]
    elif ep == 7:
        b = min((r for r in o if r["status"] == "paid"), key=lambda r: (-rev(r), r["id"]))
        lines = [f"{b['id']} {rev(b)}"]
    else:
        lines = [f"{k} {c[k]}" for k in sorted(c)]
pathlib.Path("answer.txt").write_text("\n".join(lines) + "\n")
