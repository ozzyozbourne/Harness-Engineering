import sys
from decimal import Decimal
sys.path.insert(0, sys.argv[1])
from setup import data
customers, orders = data()
names = dict(customers)
tot = {}
for _id, cid, ts, amt, st in orders:
    if ts.startswith("2025") and st != "refunded":
        tot[names[cid]] = tot.get(names[cid], Decimal(0)) + Decimal(str(amt))
rows = sorted(((n, t) for n, t in tot.items() if t > 500), key=lambda r: (-r[1], r[0]))
want = ["name,total"] + [f"{n},{t:.2f}" for n, t in rows]
got = open("out.csv").read().strip().splitlines()
assert [l.strip() for l in got] == want, (got, want)
assert len(want) > 3
print("pass")
