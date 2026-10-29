import pathlib, subprocess, sys
for p in pathlib.Path(".").rglob("*.py"):
    assert "calc_total" not in p.read_text(), f"calc_total still in {p}"
r = subprocess.run([sys.executable, "-m", "unittest", "discover", "-s", "tests"], capture_output=True, text=True)
assert r.returncode == 0, r.stderr
sys.path.insert(0, ".")
import shop
from shop.pricing import compute_total
from shop.cart import Cart
from shop.checkout import receipt
from shop.report import daily
assert shop.compute_total is compute_total
assert compute_total([("x", "food", 10.0, 2)], coupon=10) == 18.9
c = Cart(); c.add("pen", "office", 10.0, 2)
assert "total 12.00" in receipt(c, coupon=50)
assert daily([c]) == 24.0
print("pass")
