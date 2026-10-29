import pathlib, subprocess, sys
suite = pathlib.Path(sys.argv[1])
assert pathlib.Path("test_inventory.py").read_text() == (suite / "fixture/test_inventory.py").read_text(), "tests modified"
r = subprocess.run([sys.executable, "-m", "unittest", "test_inventory"], capture_output=True, text=True)
assert r.returncode == 0, r.stderr
sys.path.insert(0, ".")
from inventory import Inventory
i = Inventory(); i.add("x", 2, price=1.5); i.add("y", 4)
i.remove("x", 2); assert i.count("x") == 0
try:
    i.remove("y", 5); raise AssertionError("no ValueError")
except ValueError:
    pass
assert i.count("y") == 4
assert i.low_stock(0) == ["x"]
assert i.total_value() == 0
print("pass")
