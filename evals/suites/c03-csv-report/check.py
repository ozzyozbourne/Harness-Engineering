import json, sys
sys.path.insert(0, sys.argv[1])
from setup import rows
from decimal import Decimal
total, by_region, by_product = Decimal(0), {}, {}
for r in rows():
    rev = Decimal(r["units"]) * Decimal(r["unit_price"])
    total += rev
    k = r["region"].strip().lower()
    by_region[k] = by_region.get(k, 0) + rev
    by_product[r["product"]] = by_product.get(r["product"], 0) + rev
got = json.load(open("report.json"))
assert set(got) == {"total_revenue", "by_region", "top_product"}, set(got)
assert abs(float(got["total_revenue"]) - float(total)) < 0.006, (got["total_revenue"], total)
assert set(got["by_region"]) == set(by_region), got["by_region"]
for k, v in by_region.items():
    assert abs(float(got["by_region"][k]) - float(v)) < 0.006, (k, got["by_region"][k], v)
assert got["top_product"] == max(by_product, key=by_product.get)
print("pass")
