"""Sensor readings for each episode and the exact expected answer under the corrections known so far.

Corrections, as later prompts report them (chosen so a careful agent cannot guess them):
  A (ep3): rows with q = C (calibration) and the value -999.0 (dropout) are never counted,
           and every h-sensor reads 1.5 too high, so 1.5 is subtracted from its values
  B (ep5): spellings of a sensor id are one sensor, written as lower-case letter + 3-digit
           number (t-7, T-07, t7, T007 -> t007)
  C (ep7): a repeated (sensor, ts) row is a resend with a corrected value; count the
           reading once, with the value of the last such row in file order
Values are handled as integer tenths so every answer is exact.
"""
import datetime, os, random, re
from collections import defaultdict

def episode():
    """The current 1-based episode number."""
    return int(os.environ.get("EVAL_EPISODE", "1"))


def corrections(ep):
    return {c for c, start in (("A", 3), ("B", 5), ("C", 7)) if ep >= start}


def canon(sid, guessable=False):
    m = re.fullmatch(r"([a-zA-Z])-?0*(\d+)", sid)
    if guessable:  # the form an agent might pick without being told; used only to test the suite
        return m.group(1).upper() + m.group(2)
    return f"{m.group(1).lower()}{int(m.group(2)):03d}"


def spellings(letter, num, mixed, rnd):
    if not mixed:
        return f"{letter}-{num:03d}"
    return rnd.choice([f"{letter}-{num:03d}", f"{letter.upper()}-{num:02d}", f"{letter}{num}",
                       f"{letter.upper()}{num:03d}"])


def rows(ep):
    """[(sensor as written, ts, value in tenths, q)] in file order."""
    rnd = random.Random(900 + ep)
    sensors = [(rnd.choice("th"), n) for n in rnd.sample(range(1, 40), rnd.randint(5, 7))]
    start = datetime.datetime(2026, 4, rnd.randint(1, 20), tzinfo=datetime.timezone.utc)
    out = []
    for i in range(rnd.randint(260, 340)):
        letter, num = rnd.choice(sensors)
        ts = start + datetime.timedelta(minutes=17 * i + rnd.randint(0, 9))
        v = rnd.randint(-120, 380)
        q = "ok"
        x = rnd.random()
        if x < 0.06:
            q = "C"
            v = rnd.randint(390, 520)  # calibration pulses: highest values in the file
        elif x < 0.10:
            v = -9990
        out.append((spellings(letter, num, ep >= 5, rnd), ts.strftime("%Y-%m-%dT%H:%M:%SZ"), v, q))
        if rnd.random() < 0.07:  # duplicate transmission, sometimes delayed, value re-quantized
            dup = out[-1]
            if q == "ok" and v != -9990:
                dup = dup[:2] + (v + rnd.choice([-2, -1, 1, 2]),) + dup[3:]
            out.insert(len(out) - rnd.randint(0, 2), dup)
    return out


def tenths(t):
    return f"{'-' if t < 0 else ''}{abs(t) // 10}.{abs(t) % 10}"


def expected(ep, cs=None):
    """The exact answer.txt lines for episode ep under correction set cs (default: current).

    For testing the suite, cs may also hold the guessable variants A0 (drop rows, no
    offset), B0 (canonical form T7) and C0 (keep the first resend).
    """
    cs = corrections(ep) if cs is None else cs
    kept = {}  # (sensor, ts, row number unless deduplicating) -> value
    for n, (sid, ts, v, q) in enumerate(rows(ep)):
        if cs & {"B", "B0"}:
            sid = canon(sid, "B0" in cs)
        if cs & {"A", "A0"}:
            if q == "C" or v == -9990:
                continue
            if "A" in cs and sid[0] in "hH":
                v -= 15
        key = (sid, ts) if cs & {"C", "C0"} else (sid, ts, n)
        if "C0" in cs and key in kept:
            continue
        kept[key] = v
    recs = [(k[0], k[1], v) for k, v in kept.items()]
    by = defaultdict(list)
    day = defaultdict(list)
    for sid, ts, v in recs:
        by[sid].append(v)
        day[ts[:10]].append((sid, v))
    S = sorted(by)
    if ep == 1:
        return [f"{s} {len(by[s])}" for s in S]
    if ep == 2:
        return [f"{s} {sum(v > 300 for v in by[s])}" for s in S]
    if ep == 3:
        return [f"{s} {tenths(sum(by[s]))}" for s in S]
    if ep == 4:
        return [f"{d} {tenths(max(v for _, v in day[d]))}" for d in sorted(day)]
    if ep == 5:
        return [f"{s} {tenths(max(by[s]))}" for s in S]
    if ep == 6:
        return [f"{s} {tenths(min(by[s]))}" for s in S]
    if ep == 7:
        return [f"{s} {len(by[s])} {tenths(sum(by[s]))}" for s in S]
    if ep == 8:
        out = []
        for d in sorted(day):
            s, v = min(day[d], key=lambda sv: (sv[1], sv[0]))
            out.append(f"{d} {len(day[d])} {s} {tenths(v)}")
        return out


if __name__ == "__main__":
    with open("readings.csv", "w") as f:
        f.write("sensor,ts,value,q\n")
        for sid, ts, v, q in rows(episode()):
            f.write(f"{sid},{ts},{tenths(v)},{q}\n")
