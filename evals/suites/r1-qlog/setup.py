"""QLOG files for each episode, and the exact expected answer, from seeded RNGs.

QLOG layout (the same text is in the first episode's prompt):
  header: b"QLOG", version byte 1, base time (uvarint, Unix ms),
          string count (uvarint), then each string as uvarint length + UTF-8 bytes
  records until end of file: tag byte, body length (uvarint), body, checksum
          (2 bytes big-endian Fletcher-16 of the body)
  tag 0xA5 log record: time delta in ms from the previous valid log record (or the
          base time) as zigzag varint, level byte (0 DEBUG 1 INFO 2 WARN 3 ERROR),
          service string index, message string index, field count (uvarints), then
          per field a key string index (uvarint) and a value (zigzag varint)
  tag 0xB6 string append: uvarint length + UTF-8 bytes; the next string index
  tag 0xC7 heartbeat: ignore the body
  A record whose checksum does not match is corrupt and is skipped entirely.
"""
import datetime, os, random
from collections import Counter

LEVELS = ["DEBUG", "INFO", "WARN", "ERROR"]
SERVICES = ["api", "auth", "billing", "gateway", "search", "worker", "ledger", "mailer"]
MESSAGES = ["request done", "cache miss", "retrying upstream", "timeout talking to db", "user not found",
            "token refreshed", "payment declined", "rate limited", "queue drained", "slow query",
            "disk almost full", "connection reset", "invalid signature", "job finished", "config reloaded",
            "upstream 502", "lock contention", "schema mismatch"]
KEYS = ["latency_ms", "status", "bytes", "user", "drift"]


def uvarint(n):
    out = bytearray()
    while True:
        b = n & 0x7F
        n >>= 7
        if n:
            out.append(b | 0x80)
        else:
            out.append(b)
            return bytes(out)


def svarint(n):
    return uvarint(n << 1 if n >= 0 else ((-n) << 1) - 1)


def fletcher16(data):
    s1 = s2 = 0
    for b in data:
        s1 = (s1 + b) % 255
        s2 = (s2 + s1) % 255
    return (s2 << 8) | s1


def string_bytes(s):
    raw = s.encode()
    return uvarint(len(raw)) + raw


def frame(tag, body, corrupt_rnd=None):
    """One framed record; with corrupt_rnd, one byte of body or checksum is damaged."""
    ck = fletcher16(body).to_bytes(2, "big")
    if corrupt_rnd is not None:
        while True:
            buf = bytearray(body + ck)
            i = corrupt_rnd.randrange(len(buf))
            buf[i] ^= corrupt_rnd.randrange(1, 256)
            nb, nck = bytes(buf[:len(body)]), bytes(buf[len(body):])
            if fletcher16(nb).to_bytes(2, "big") != nck:
                body, ck = nb, nck
                break
    return bytes([tag]) + uvarint(len(body)) + body + ck


def make_file(seed, n_records):
    """(file bytes, valid log records as dicts, number of corrupt records)."""
    rnd = random.Random(seed)
    base = int(datetime.datetime(2026, 3, rnd.randint(1, 28), rnd.randint(0, 20),
                                 tzinfo=datetime.timezone.utc).timestamp() * 1000) + rnd.randrange(1000)
    services, messages = SERVICES[:], MESSAGES[:]
    rnd.shuffle(services)
    rnd.shuffle(messages)
    table = KEYS[:] + services[:4] + messages[:10]
    rnd.shuffle(table)
    pending = services[4:] + messages[10:]  # introduced later by string appends
    rnd.shuffle(pending)
    out = bytearray(b"QLOG" + bytes([1]) + uvarint(base) + uvarint(len(table)))
    for s in table:
        out += string_bytes(s)
    defined_services = [s for s in table if s in SERVICES]
    defined_messages = [s for s in table if s in MESSAGES]
    append_at = set(rnd.sample(range(50, n_records - 50), len(pending)))
    t, valid, corrupt = base, [], 0
    for i in range(n_records):
        if i in append_at:
            s = pending.pop()
            out += frame(0xB6, string_bytes(s))
            table.append(s)
            (defined_services if s in SERVICES else defined_messages).append(s)
        if rnd.random() < 0.02:
            out += frame(0xC7, bytes(rnd.randrange(256) for _ in range(rnd.randint(0, 12))))
        delta = rnd.randint(-1500, -1) if rnd.random() < 0.05 else rnd.randint(0, 4000)
        level = rnd.choices(range(4), weights=[20, 50, 20, 10])[0]
        service = rnd.choice(defined_services)
        message = rnd.choice(defined_messages)
        fields = {}
        for k in KEYS:
            if rnd.random() < 0.6:
                fields[k] = {"latency_ms": rnd.randint(1, 5000), "status": rnd.choice([200, 201, 204, 400, 404, 500, 503]),
                             "bytes": rnd.randint(0, 2_000_000), "user": rnd.randint(1, 400),
                             "drift": rnd.randint(-900, 900)}[k]
        keys = list(fields)
        rnd.shuffle(keys)
        body = svarint(delta) + bytes([level]) + uvarint(table.index(service)) + uvarint(table.index(message))
        body += uvarint(len(keys))
        for k in keys:
            body += uvarint(table.index(k)) + svarint(fields[k])
        if rnd.random() < 0.02:
            out += frame(0xA5, body, corrupt_rnd=rnd)
            corrupt += 1
        else:
            out += frame(0xA5, body)
            t += delta
            valid.append({"ts": t, "level": LEVELS[level], "service": service, "message": message, "fields": fields})
    return bytes(out), valid, corrupt


# File names of each episode and their seeds; every file is generated from its own seed.
EPISODES = [
    ["app.qlog"],
    ["edge-1.qlog", "edge-2.qlog"],
    ["night.qlog"],
    ["cluster.qlog"],
    ["east.qlog", "west.qlog", "north.qlog"],
    ["burst.qlog"],
    ["pay-a.qlog", "pay-b.qlog"],
    ["d1.qlog", "d2.qlog", "d3.qlog", "d4.qlog"],
]


def episode():
    """The current 1-based episode number."""
    return int(os.environ.get("EVAL_EPISODE", "1"))


def files(ep):
    out = {}
    for j, name in enumerate(EPISODES[ep - 1]):
        rnd = random.Random(ep * 100 + j)
        out[name] = make_file(7000 + ep * 100 + j, rnd.randint(1500, 2600))
    return out


def iso(ms):
    d = datetime.datetime.fromtimestamp(ms / 1000, datetime.timezone.utc)
    return d.strftime("%Y-%m-%dT%H:%M:%S.") + f"{ms % 1000:03d}Z"


def expected(ep):
    """The exact answer.txt lines for episode ep."""
    fs = files(ep)
    recs = [r for _, v, _ in fs.values() for r in v]
    if ep == 1:
        c = Counter(r["level"] for r in recs)
        return [f"{lv} {c[lv]}" for lv in LEVELS] + [f"CORRUPT {sum(x[2] for x in fs.values())}"]
    if ep == 2:
        tot = Counter()
        for r in recs:
            tot[r["service"]] += r["fields"].get("bytes", 0)
        return [f"{s} {tot[s]}" for s in sorted({r["service"] for r in recs})]
    if ep == 3:
        ts = sorted(r["ts"] for r in recs if r["level"] == "ERROR")
        return [iso(ts[0]), iso(ts[-1])]
    if ep == 4:
        mx = {}
        for r in recs:
            if r["level"] in ("WARN", "ERROR") and "latency_ms" in r["fields"]:
                mx[r["service"]] = max(mx.get(r["service"], 0), r["fields"]["latency_ms"])
        return [f"{s} {m}" for s, m in sorted(mx.items(), key=lambda kv: (-kv[1], kv[0]))]
    if ep == 5:
        return [str(len({r["fields"]["user"] for r in recs if r["level"] == "ERROR" and "user" in r["fields"]}))]
    if ep == 6:
        c = Counter(iso(r["ts"])[:16] for r in recs)
        best = sorted(c.items(), key=lambda kv: (-kv[1], kv[0]))[0]
        return [f"{best[0]} {best[1]}"]
    if ep == 7:
        c = Counter(r["message"] for r in recs if r["level"] == "ERROR" and r["service"] == "billing")
        best = sorted(c.items(), key=lambda kv: (-kv[1], kv[0]))[0]
        return [f"{best[1]} {best[0]}"]
    if ep == 8:
        return [f"{n} {len(v)} {k} {sum(r['fields'].get('drift', 0) for r in v)}" for n, (_, v, k) in sorted(fs.items())]


if __name__ == "__main__":
    for name, (data, _, _) in files(episode()).items():
        open(name, "wb").write(data)
