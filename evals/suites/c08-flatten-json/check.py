import json, pathlib, sys
src = json.load(open(pathlib.Path(sys.argv[1], "fixture/config.json")))
want = {}
def walk(v, path):
    if isinstance(v, dict) and v:
        for k, x in v.items():
            walk(x, path + [f"[{k}]" if "." in k else k])
    elif isinstance(v, list) and v:
        for i, x in enumerate(v):
            walk(x, path + [str(i)])
    else:
        want[".".join(path)] = v
walk(src, [])
got = json.load(open("flat.json"))
assert got == want, {"missing_or_wrong": {k: v for k, v in want.items() if got.get(k, "<absent>") != v}, "extra": sorted(set(got) - set(want))}
assert all(type(got[k]) is type(want[k]) for k in want)
print("pass")
