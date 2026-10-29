import sys
sys.path.insert(0, sys.argv[1])
from setup import build
want = build()[1]
got = open("answer.txt").read().strip()
assert got == want, (got, want)
print("pass")
