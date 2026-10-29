import re, sys
from collections import Counter
sys.path.insert(0, sys.argv[1])
from setup import lines
counts = Counter()
for l in lines():
    m = re.match(r'^(\S+) \S+ \S+ \[[^\]]+\] "[^"]*" (\d{3}) ', l)
    if m and m.group(2).startswith("5"):
        counts[m.group(1)] += 1
# Explicit sort, not most_common(): ties must break by IP ascending.
want = sorted(counts.items(), key=lambda kv: (-kv[1], kv[0]))[:3]
got = [l.split() for l in open("answer.txt").read().strip().splitlines()]
assert got == [[ip, str(c)] for ip, c in want], (got, want)
print("pass")
