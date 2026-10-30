"""vault.db for each episode (read through the fixture's vault.py) and the exact expected answers."""
import json, os, random, zlib
from collections import Counter, defaultdict

REGIONS = ["north", "south", "east", "west", "central"]
SKUS = [f"SKU-{n}" for n in (101, 117, 204, 233, 310, 342, 418, 455, 507, 560)]
STATUSES = ["paid"] * 6 + ["returned", "cancelled", "pending"]
COUNTRIES = ["NO", "SE", "DK", "FI", "DE", "NL", "PL"]
KINDS = ["login", "view", "search", "buy", "logout"]


def episode():
    """The current 1-based episode number."""
    return int(os.environ.get("EVAL_EPISODE", "1"))


def orders(rnd, n, start_id, customers):
    ids = rnd.sample(range(start_id, start_id + 9000), n)
    return [{"id": i, "customer": rnd.choice(customers), "region": rnd.choice(REGIONS), "sku": rnd.choice(SKUS),
             "qty": rnd.randint(1, 40), "price_cents": rnd.randint(150, 90_000), "status": rnd.choice(STATUSES)}
            for i in ids]


def tables(ep):
    rnd = random.Random(1300 + ep)
    custs = [f"C{n}" for n in rnd.sample(range(100, 999), 30)]
    t = {"orders": orders(rnd, rnd.randint(180, 320), 10_000, custs),
         "customers": [{"id": c, "name": f"Customer {c}", "country": rnd.choice(COUNTRIES)} for c in custs]}
    if ep == 6:
        t["events"] = [{"user": f"u{rnd.randint(1, 90)}", "day": f"2026-05-{rnd.randint(1, 9):02d}",
                        "kind": rnd.choice(KINDS)} for _ in range(rnd.randint(250, 330))]
    if ep == 8:
        t["orders_q1"] = orders(rnd, rnd.randint(150, 250), 20_000, custs)
        t["orders_q2"] = orders(rnd, rnd.randint(150, 250), 30_000, custs)
    return t


NEEDED = {1: ["orders"], 2: ["orders"], 3: ["orders"], 4: ["orders"], 5: ["orders", "customers"],
          6: ["events"], 7: ["orders"], 8: ["orders_q1", "orders_q2"]}


def expected(ep):
    t = tables(ep)
    o = t["orders"]
    rev = lambda r: r["qty"] * r["price_cents"]
    if ep == 1:
        c = Counter()
        for r in o:
            c[r["region"]] += r["qty"]
        return [f"{k} {c[k]}" for k in sorted(c)]
    if ep == 2:
        c = Counter(r["sku"] for r in o if r["status"] == "returned")
        return [f"{k} {n}" for k, n in sorted(c.items(), key=lambda kv: (-kv[1], kv[0]))[:3]]
    if ep == 3:
        c = Counter(r["customer"] for r in o if r["status"] == "pending")
        return [f"{k} {c[k]}" for k in sorted(c)]
    if ep == 4:
        c = Counter()
        for r in o:
            if r["status"] == "paid":
                c[r["region"]] += rev(r)
        return [f"{k} {c[k]}" for k in sorted(c)]
    if ep == 5:
        country = {r["id"]: r["country"] for r in t["customers"]}
        c = Counter()
        for r in o:
            if r["status"] == "paid":
                c[country[r["customer"]]] += rev(r)
        return [f"{k} {c[k]}" for k in sorted(c)]
    if ep == 6:
        d = defaultdict(set)
        for r in t["events"]:
            if r["kind"] == "buy":
                d[r["day"]].add(r["user"])
        return [f"{k} {len(d[k])}" for k in sorted(d)]
    if ep == 7:
        best = min((r for r in o if r["status"] == "paid"), key=lambda r: (-rev(r), r["id"]))
        return [f"{best['id']} {rev(best)}"]
    if ep == 8:
        return [f"{n} {sum(rev(r) for r in t[n] if r['status'] == 'paid')}" for n in ("orders_q1", "orders_q2")]


if __name__ == "__main__":
    raw = zlib.compress(json.dumps(tables(episode())).encode())
    key = b"cmh-vault"
    open("vault.db", "wb").write(bytes(b ^ key[i % len(key)] for i, b in enumerate(raw)))
