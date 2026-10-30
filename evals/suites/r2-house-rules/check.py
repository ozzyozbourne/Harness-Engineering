import pathlib, sys
sys.path.insert(0, sys.argv[1])
from setup import expected, episode
name, want = expected(episode())
assert pathlib.Path(name).exists(), f"{name} missing"
got = [l.rstrip() for l in pathlib.Path(name).read_text().strip().splitlines()]
assert got == want, ("\n".join(got[:4]), "\n".join(want[:4]), next((i for i, (a, b) in enumerate(zip(got, want)) if a != b), None), len(got), len(want))
print("pass")
