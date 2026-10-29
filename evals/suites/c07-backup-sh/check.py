import os, pathlib, subprocess, tempfile
assert pathlib.Path("backup.sh").exists(), "no backup.sh"
script = str(pathlib.Path("backup.sh").resolve())
with tempfile.TemporaryDirectory() as t:
    src, dest = pathlib.Path(t, "my src"), pathlib.Path(t, "out dir/nested")
    files = {"a.txt": "1", "notes/b.txt": "2", "notes/deep er/c d.txt": "3", "x.md": "no", "img/p.png": "no",
             "notes/fake.txt.bak": "no", ".hidden/h.txt": "4"}
    for rel, body in files.items():
        p = src / rel; p.parent.mkdir(parents=True, exist_ok=True); p.write_text(body)
    os.mkdir(src / "dir.txt")
    r = subprocess.run(["bash", script, str(src), str(dest)], capture_output=True, text=True)
    assert r.returncode == 0, r.stderr
    assert r.stdout.strip() == "4", repr(r.stdout)
    got = sorted(str(p.relative_to(dest)) for p in dest.rglob("*") if p.is_file())
    assert got == sorted([".hidden/h.txt", "a.txt", "notes/b.txt", "notes/deep er/c d.txt"]), got
    assert (dest / "notes/deep er/c d.txt").read_text() == "3"
    for args in ([], [str(src)], [str(src / "a.txt"), str(dest)]):
        r = subprocess.run(["bash", script, *args], capture_output=True, text=True)
        assert r.returncode == 2, (args, r.returncode)
        assert r.stderr.strip(), "no usage on stderr"
print("pass")
