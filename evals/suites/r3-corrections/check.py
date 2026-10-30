import pathlib, re, sys
sys.path.insert(0, sys.argv[1])
from setup import expected, episode
want = expected(episode())
got = [" ".join(l.split()) for l in pathlib.Path("answer.txt").read_text().strip().splitlines()]
if got and re.fullmatch(r"[A-Z][A-Z_]*( [A-Z][A-Z_]*)+", got[0]):
    got = got[1:]  # tolerate the line template written as a header row
assert got == want, (got[:8], want[:8])
print("pass")
