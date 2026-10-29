import csv, pathlib, re
src = [p for p in pathlib.Path(".").glob("*.csv")]
assert len(src) == 1, src
rows = list(csv.DictReader(open(src[0])))
report = pathlib.Path("report.md").read_text()
lines = [l.strip() for l in report.strip().splitlines() if l.strip()]
assert lines[-1] == "Prepared by: cmh-eval", f"last line is {lines[-1]!r}"
assert not re.search(r"\b\d{1,2}/\d{1,2}/\d{2,4}\b", report), "US-format date present"
assert not re.search(r"\bvery\b", report, re.I), "uses the word 'very'"
for r in rows:
    m, d, y = list(r.values())[0].split("/")
    assert f"{y}-{m}-{d}" in report, f"missing ISO date {y}-{m}-{d}"
    ident = r.get("id") or r.get("version")
    if ident:
        assert ident in report, f"missing {ident}"
print("pass")
