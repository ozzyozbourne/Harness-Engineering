"""Reference solution: answers EVAL_EPISODE's question from readings.csv with the corrections known by then."""
import collections, csv, os, pathlib, re
from decimal import Decimal

ep = int(os.environ["EVAL_EPISODE"])
fix_a, fix_b, fix_c = ep >= 3, ep >= 5, ep >= 7
kept = {}
for n, r in enumerate(csv.DictReader(open("readings.csv"))):
    sid, v = r["sensor"], Decimal(r["value"])
    if fix_b:
        m = re.fullmatch(r"([a-zA-Z])-?0*(\d+)", sid)
        sid = f"{m.group(1).lower()}{int(m.group(2)):03d}"
    if fix_a:
        if r["q"] == "C" or v == Decimal("-999.0"):
            continue
        if sid[0] in "hH":
            v -= Decimal("1.5")
    kept[(sid, r["ts"]) if fix_c else (sid, r["ts"], n)] = v  # later resends overwrite
by, day = collections.defaultdict(list), collections.defaultdict(list)
for k, v in kept.items():
    by[k[0]].append(v)
    day[k[1][:10]].append((k[0], v))
S = sorted(by)
f = lambda v: f"{v:.1f}"
if ep == 1:
    lines = [f"{s} {len(by[s])}" for s in S]
elif ep == 2:
    lines = [f"{s} {sum(v > 30 for v in by[s])}" for s in S]
elif ep == 3:
    lines = [f"{s} {f(sum(by[s]))}" for s in S]
elif ep == 4:
    lines = [f"{d} {f(max(v for _, v in day[d]))}" for d in sorted(day)]
elif ep == 5:
    lines = [f"{s} {f(max(by[s]))}" for s in S]
elif ep == 6:
    lines = [f"{s} {f(min(by[s]))}" for s in S]
elif ep == 7:
    lines = [f"{s} {len(by[s])} {f(sum(by[s]))}" for s in S]
else:
    lines = []
    for d in sorted(day):
        s, v = min(day[d], key=lambda sv: (sv[1], sv[0]))
        lines.append(f"{d} {len(day[d])} {s} {f(v)}")
pathlib.Path("answer.txt").write_text("\n".join(lines) + "\n")
