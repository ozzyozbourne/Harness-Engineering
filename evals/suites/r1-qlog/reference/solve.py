"""Reference solution: parses the QLOG files in cwd from the format description alone.

Never copied into work dirs; used only by the positive control (python3 solve.py with
cwd = the work dir and EVAL_EPISODE set).
"""
import datetime, os, pathlib
from collections import Counter

LEVELS = ["DEBUG", "INFO", "WARN", "ERROR"]


def parse(data):
    pos = 0

    def uv():
        nonlocal pos
        n = shift = 0
        while True:
            b = data[pos]
            pos += 1
            n |= (b & 0x7F) << shift
            shift += 7
            if not b & 0x80:
                return n

    def body_uv(buf, i):
        n = shift = 0
        while True:
            b = buf[i]
            i += 1
            n |= (b & 0x7F) << shift
            shift += 7
            if not b & 0x80:
                return n, i

    def zz(z):
        return z >> 1 if z % 2 == 0 else -((z + 1) >> 1)

    assert data[:5] == b"QLOG\x01"
    pos = 5
    t = uv()
    table = []
    for _ in range(uv()):
        n = uv()
        table.append(data[pos:pos + n].decode())
        pos += n
    recs, corrupt = [], 0
    while pos < len(data):
        tag = data[pos]
        pos += 1
        n = uv()
        body, ck = data[pos:pos + n], data[pos + n:pos + n + 2]
        pos += n + 2
        s1 = s2 = 0
        for b in body:
            s1 = (s1 + b) % 255
            s2 = (s2 + s1) % 255
        if s2 * 256 + s1 != int.from_bytes(ck, "big"):
            corrupt += 1
            continue
        if tag == 0xB6:
            k, i = body_uv(body, 0)
            table.append(body[i:i + k].decode())
        elif tag == 0xA5:
            d, i = body_uv(body, 0)
            level = body[i]
            svc, i = body_uv(body, i + 1)
            msg, i = body_uv(body, i)
            nf, i = body_uv(body, i)
            fields = {}
            for _ in range(nf):
                k, i = body_uv(body, i)
                v, i = body_uv(body, i)
                fields[table[k]] = zz(v)
            t += zz(d)
            recs.append({"ts": t, "level": LEVELS[level], "service": table[svc], "message": table[msg], "fields": fields})
    return recs, corrupt


def iso(ms):
    d = datetime.datetime.fromtimestamp(ms // 1000, datetime.timezone.utc)
    return d.strftime("%Y-%m-%dT%H:%M:%S") + f".{ms % 1000:03d}Z"


ep = int(os.environ["EVAL_EPISODE"])
parsed = {p.name: parse(p.read_bytes()) for p in sorted(pathlib.Path(".").glob("*.qlog"))}
recs = [r for v, _ in parsed.values() for r in v]
if ep == 1:
    c = Counter(r["level"] for r in recs)
    lines = [f"{lv} {c[lv]}" for lv in LEVELS] + [f"CORRUPT {sum(k for _, k in parsed.values())}"]
elif ep == 2:
    tot = Counter()
    for r in recs:
        tot[r["service"]] += r["fields"].get("bytes", 0)
    lines = [f"{s} {tot[s]}" for s in sorted(tot)]
elif ep == 3:
    ts = sorted(r["ts"] for r in recs if r["level"] == "ERROR")
    lines = [iso(ts[0]), iso(ts[-1])]
elif ep == 4:
    mx = {}
    for r in recs:
        if r["level"] in ("WARN", "ERROR") and "latency_ms" in r["fields"]:
            mx[r["service"]] = max(mx.get(r["service"], 0), r["fields"]["latency_ms"])
    lines = [f"{s} {m}" for s, m in sorted(mx.items(), key=lambda kv: (-kv[1], kv[0]))]
elif ep == 5:
    lines = [str(len({r["fields"]["user"] for r in recs if r["level"] == "ERROR" and "user" in r["fields"]}))]
elif ep == 6:
    c = Counter(iso(r["ts"])[:16] for r in recs)
    m, n = min(c.items(), key=lambda kv: (-kv[1], kv[0]))
    lines = [f"{m} {n}"]
elif ep == 7:
    c = Counter(r["message"] for r in recs if r["level"] == "ERROR" and r["service"] == "billing")
    m, n = min(c.items(), key=lambda kv: (-kv[1], kv[0]))
    lines = [f"{n} {m}"]
else:
    lines = [f"{name} {len(v)} {k} {sum(r['fields'].get('drift', 0) for r in v)}" for name, (v, k) in parsed.items()]
pathlib.Path("answer.txt").write_text("\n".join(lines) + "\n")
