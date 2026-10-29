want = sorted({"ann.lee@example.com", "bob@example.com", "cara+news@mail.co.uk", "jo@good-domain.io",
               "mo_m-ax@sub.domain.org", "rae@example.com", "ted@example.museum", "uma@123.example.travel"})
got = open("emails.txt").read().split()
assert got == want, {"missing": sorted(set(want) - set(got)), "extra": sorted(set(got) - set(want))}
print("pass")
