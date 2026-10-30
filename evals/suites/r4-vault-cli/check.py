import json, pathlib, re, sys
sys.path.insert(0, sys.argv[1])
from setup import expected, episode, NEEDED
ep = episode()
want = expected(ep)
got = [" ".join(l.split()) for l in pathlib.Path("answer.txt").read_text().strip().splitlines()]
if got and re.fullmatch(r"[A-Z][A-Z_]*( [A-Z][A-Z_]*)+", got[0]):
    got = got[1:]  # tolerate the line template written as a header row
assert got == want, (got[:8], want[:8])
# The data must come through the vault protocol: every needed table read to the end, every cursor closed.
st = json.loads(pathlib.Path(".vault/state.json").read_text())
cursors = st["cursors"].values()
for t in NEEDED[ep]:
    assert any(c["table"] == t and c["next"] == c["pages"] for c in cursors), f"table {t} was not read through the vault"
left_open = [cid for cid, c in st["cursors"].items() if not c["closed"]]
assert not left_open, f"cursors left open: {left_open}"
print("pass")
