import pathlib, subprocess, sys
suite = pathlib.Path(sys.argv[1])
assert pathlib.Path("test_stats.py").read_text() == (suite / "fixture/test_stats.py").read_text(), "test_stats.py was modified"
r = subprocess.run([sys.executable, "test_stats.py"], capture_output=True, text=True)
assert r.returncode == 0, r.stdout + r.stderr
sys.path.insert(0, ".")
from stats import median, mode, variance, mean
assert median([5]) == 5
assert median([1, 2, 3, 4, 5, 6]) == 3.5
assert mode([9, 9, 2, 2, 5, 5, 1]) == 2
assert mode([7]) == 7
assert abs(variance([1, 2, 3, 4]) - 1.25) < 1e-9
assert mean([2]) == 2
print("pass")
