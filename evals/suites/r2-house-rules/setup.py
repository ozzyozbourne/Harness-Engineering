"""Input JSON for each episode and the exact expected CSV under the house rules in force.

House rules as the prompts give them:
  ep1: 1 `;` separator and a header row; 2 dates YYYY-MM-DD; 3 money in units with
       exactly two decimals, no symbol (inputs are cents)
  ep3: 4 drop records with "test": true; 5 sort data rows by the first column (plain
       string order); 6 last line `# rows: N`
  ep5: rule 3 replaced: money as integer cents; 7 header names in upper case
"""
import datetime, json, os, random

CUSTOMERS = ["Acme Ltd", "Borealis GmbH", "Cobalt & Sons", "Delta Freight", "Ember Labs", "Fjord AS",
             "Granite Co", "Helix Bio", "Iris Media", "Juniper Farms"]
METHODS = ["card", "wire", "sepa", "cash"]
CARRIERS = ["DHL", "UPS", "PostNord", "FedEx"]


def episode():
    """The current 1-based episode number."""
    return int(os.environ.get("EVAL_EPISODE", "1"))


def rules(ep):
    """The set of house-rule ids in force during episode ep."""
    r = {1, 2, 3}
    if ep >= 3:
        r |= {4, 5, 6}
    if ep >= 5:
        r = (r - {3}) | {"3b", 7}
    return r


def day(rnd):
    return datetime.date(2026, 1, 1) + datetime.timedelta(days=rnd.randrange(150))


def date_text(d, rnd, style):
    if style == "text":
        return d.strftime("%b %d, %Y")
    return f"{d.isoformat()}T{rnd.randint(0, 23):02d}:{rnd.randint(0, 59):02d}:00Z"


def records(rnd, n, prefix, style, fields):
    """n input records with ids out of order, a few marked test, and their true values."""
    ids = rnd.sample(range(1000, 9999), n)
    out = []
    for i in ids:
        d = day(rnd)
        rec = {"id": f"{prefix}-{i}", "_date": d, "when": date_text(d, rnd, style)}
        for k, gen in fields.items():
            rec[k] = gen(rnd)
        if rnd.random() < 0.15:
            rec["test"] = True
        elif rnd.random() < 0.3:
            rec["test"] = False
        out.append(rec)
    return out


cents = lambda rnd: rnd.choice([rnd.randint(100, 99_999), rnd.randint(100, 2_000_000), rnd.randint(1, 99) * 100])
customer = lambda rnd: rnd.choice(CUSTOMERS)

# Per episode: output file, header, input files {name: (prefix, date style, extra fields, rename)},
# and how each input record becomes an output row. Money columns are marked with "$".
EPISODES = {
    1: ("orders.csv", ["id", "date", "customer", "total"],
        {"orders.json": ("O", "text", {"customer": customer, "total_cents": cents})},
        lambda r: [r["id"], "D", r["customer"], ("$", r["total_cents"])]),
    2: ("refunds.csv", ["id", "date", "order_id", "amount"],
        {"refunds.json": ("R", "iso", {"order": lambda rnd: f"O-{rnd.randint(1000, 9999)}", "amount_cents": cents})},
        lambda r: [r["id"], "D", r["order"], ("$", r["amount_cents"])]),
    3: ("invoices.csv", ["id", "date", "customer", "amount"],
        {"invoices.json": ("INV", "text", {"customer": customer, "amount_cents": cents})},
        lambda r: [r["id"], "D", r["customer"], ("$", r["amount_cents"])]),
    4: ("payments.csv", ["id", "date", "method", "amount"],
        {"payments.json": ("P", "iso", {"method": lambda rnd: rnd.choice(METHODS), "amount_cents": cents})},
        lambda r: [r["id"], "D", r["method"], ("$", r["amount_cents"])]),
    5: ("orders.csv", ["id", "date", "customer", "total"],
        {"orders.json": ("O", "iso", {"customer": customer, "total_cents": cents})},
        lambda r: [r["id"], "D", r["customer"], ("$", r["total_cents"])]),
    6: ("shipments.csv", ["id", "date", "carrier", "cost"],
        {"shipments.json": ("S", "text", {"carrier": lambda rnd: rnd.choice(CARRIERS), "cost_cents": cents})},
        lambda r: [r["id"], "D", r["carrier"], ("$", r["cost_cents"])]),
    7: None,  # per-customer totals of invoices.json, built in expected()
    8: ("orders.csv", ["id", "date", "customer", "total"],
        {"orders_jan.json": ("O", "text", {"customer": customer, "total_cents": cents}),
         "orders_feb.json": ("O", "iso", {"customer": customer, "total_cents": cents})},
        lambda r: [r["id"], "D", r["customer"], ("$", r["total_cents"])]),
}


def inputs(ep):
    """{file name: list of records (with private _date)}."""
    rnd = random.Random(500 + ep)
    if ep == 7:
        return {"invoices.json": records(rnd, 40, "INV", "text", {"customer": customer, "amount_cents": cents})}
    _, _, files, _ = EPISODES[ep]
    return {name: records(rnd, rnd.randint(14, 22), prefix, style, fields)
            for name, (prefix, style, fields) in files.items()}


def money(c, rs):
    return str(c) if "3b" in rs else f"{c // 100}.{c % 100:02d}"


def expected(ep, rs=None):
    """(output file name, exact lines) for episode ep under rule set rs (default: current)."""
    rs = rules(ep) if rs is None else rs
    ins = inputs(ep)
    recs = [r for v in ins.values() for r in v if not (4 in rs and r.get("test"))]
    if ep == 7:
        name, header = "totals.csv", ["customer", "invoices", "amount"]
        tot = {}
        for r in recs:
            n, c = tot.get(r["customer"], (0, 0))
            tot[r["customer"]] = (n + 1, c + r["amount_cents"])
        rows = [[k, str(n), money(c, rs)] for k, (n, c) in sorted(tot.items())]
    else:
        name, header, _, conv = EPISODES[ep]
        rows = []
        for r in recs:
            row = []
            for v in conv(r):
                if v == "D":
                    v = r["_date"].isoformat()
                elif isinstance(v, tuple):
                    v = money(v[1], rs)
                row.append(v)
            rows.append(row)
    if 5 in rs:
        rows.sort(key=lambda row: row[0])
    if 7 in rs:
        header = [h.upper() for h in header]
    lines = [";".join(header)] + [";".join(row) for row in rows]
    if 6 in rs:
        lines.append(f"# rows: {len(rows)}")
    return name, lines


if __name__ == "__main__":
    for name, recs in inputs(episode()).items():
        with open(name, "w") as f:
            json.dump([{k: v for k, v in r.items() if k != "_date"} for r in recs], f, indent=1)
