import sys
sys.path.insert(0, ".")
from roman import to_roman, from_roman

def ref(n):
    vals = [(1000, "M"), (900, "CM"), (500, "D"), (400, "CD"), (100, "C"), (90, "XC"), (50, "L"),
            (40, "XL"), (10, "X"), (9, "IX"), (5, "V"), (4, "IV"), (1, "I")]
    out = ""
    for v, s in vals:
        while n >= v:
            out += s; n -= v
    return out

def raises(f, errors):
    try:
        f()
    except errors:
        return True
    return False

for n in range(1, 4000):
    assert to_roman(n) == ref(n), n
    assert from_roman(ref(n)) == n, n
for bad in (0, -1, 4000, 2.5, True, "5"):
    assert raises(lambda: to_roman(bad), (ValueError, TypeError)), f"to_roman({bad!r}) did not raise"
for bad in ("", "IIII", "IC", "MMMM", "iv", "VX", "IIV", "XM", "CMC", "VV", "LXL", "ABC", "MCMXCIVI"):
    assert raises(lambda: from_roman(bad), ValueError), f"from_roman({bad!r}) did not raise"
print("pass")
