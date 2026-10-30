"""Reference solution: applies the house rules in force at EVAL_EPISODE to the JSON in cwd."""
import collections, datetime, json, os, pathlib

ep = int(os.environ["EVAL_EPISODE"])
rules = {1, 2, 3} | ({4, 5, 6} if ep >= 3 else set())
if ep >= 5:
    rules = (rules - {3}) | {"3b", 7}

def date(s):
    try:
        return datetime.datetime.strptime(s, "%b %d, %Y").date().isoformat()
    except ValueError:
        return s[:10]

def money(c):
    return str(c) if "3b" in rules else f"{c / 100:.2f}"

def load(*names):
    recs = [r for n in names for r in json.loads(pathlib.Path(n).read_text())]
    return [r for r in recs if not (4 in rules and r.get("test"))]

jobs = {1: ("orders.csv", ["orders.json"], ["id", "date", "customer", "total"], "customer", "total_cents"),
        2: ("refunds.csv", ["refunds.json"], ["id", "date", "order_id", "amount"], "order", "amount_cents"),
        3: ("invoices.csv", ["invoices.json"], ["id", "date", "customer", "amount"], "customer", "amount_cents"),
        4: ("payments.csv", ["payments.json"], ["id", "date", "method", "amount"], "method", "amount_cents"),
        5: ("orders.csv", ["orders.json"], ["id", "date", "customer", "total"], "customer", "total_cents"),
        6: ("shipments.csv", ["shipments.json"], ["id", "date", "carrier", "cost"], "carrier", "cost_cents"),
        8: ("orders.csv", ["orders_jan.json", "orders_feb.json"], ["id", "date", "customer", "total"], "customer", "total_cents")}
if ep == 7:
    out, header = "totals.csv", ["customer", "invoices", "amount"]
    agg = collections.defaultdict(lambda: [0, 0])
    for r in load("invoices.json"):
        agg[r["customer"]][0] += 1
        agg[r["customer"]][1] += r["amount_cents"]
    rows = [[k, str(n), money(c)] for k, (n, c) in agg.items()]
else:
    out, names, header, col, cents = jobs[ep]
    rows = [[r["id"], date(r["when"]), r[col], money(r[cents])] for r in load(*names)]
if 5 in rules:
    rows.sort(key=lambda r: r[0])
if 7 in rules:
    header = [h.upper() for h in header]
lines = [";".join(header)] + [";".join(r) for r in rows] + ([f"# rows: {len(rows)}"] if 6 in rules else [])
pathlib.Path(out).write_text("\n".join(lines) + "\n")
