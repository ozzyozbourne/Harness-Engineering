import random

def build():
    """Return (ini_text, conflicting_key), the key as SECTION.KEY."""
    rnd = random.Random(12)
    words = ["alpha", "beta", "cache", "delta", "edge", "flux", "gate", "hub", "ion", "jet", "kite", "lane"]
    sections = []
    for s in range(60):
        name = f"{rnd.choice(words)}_{s}"
        keys = [(f"{rnd.choice(words)}_{k}", str(rnd.randint(0, 999))) for k in range(45)]
        keys = list(dict(keys).items())
        sections.append([name, keys])
    # Same key + same value twice (not a conflict), and same key in other sections.
    sec = sections[7]; sec[1].insert(30, sec[1][3])
    sections[20][1].append(sections[21][1][0])
    # The real conflict, far from the first definition, value differs only in a trailing char.
    tgt = sections[41]; ck, cv = tgt[1][5]
    tgt[1].insert(38, (ck, cv + "0"))
    lines = []
    for name, keys in sections:
        lines.append(f"[{name}]")
        for i, (k, v) in enumerate(keys):
            if i % 9 == 0:
                lines.append(f"; note for {k}")
            lines.append(f"{k} = {v}" if i % 2 else f"{k}={v}")
        lines.append("")
    return "\n".join(lines), f"{tgt[0]}.{ck}"

if __name__ == "__main__":
    open("settings.ini", "w").write(build()[0])
