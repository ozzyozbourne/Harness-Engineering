import re, sys
sys.path.insert(0, sys.argv[1])
from setup import lines
counts = {}
for l in lines():
    m = re.match(r'^(\S+) \S+ \S+ \[[^\]]+\] "[^"]*" (\d{3}) ', l)
    if m and m.group(2).startswith("5"):
        counts[m.group(1)] = counts.get(m.group(1), 0) + 1
want = sorted(counts.items(), key=lambda kv: (-kv[1], kv[0]))[:3]
got = [l.split() for l in open("answer.txt").read().strip().splitlines()]
assert got == [[ip, str(c)] for ip, c in want], (got, want)
print("pass")
