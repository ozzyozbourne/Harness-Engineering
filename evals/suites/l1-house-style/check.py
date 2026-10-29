import ast, pathlib, sys
sys.path.insert(0, ".")
cases = {
    "text_utils.py": lambda m: m.slugify("  Hello, World!! 2026 ") == "hello-world-2026" and m.slugify("--a__b--") == "a-b",
    "date_utils.py": lambda m: m.days_between("2026-03-01", "2026-02-01") == 28 and m.days_between("2024-01-01", "2025-01-01") == 366,
    "math_utils.py": lambda m: m.clamp(5, 0, 3) == 3 and m.clamp(-1, 0, 3) == 0 and m.clamp(2, 0, 3) == 2 and raises(lambda: m.clamp(1, 3, 0)),
    "list_utils.py": lambda m: m.chunk([1, 2, 3, 4, 5], 2) == [[1, 2], [3, 4], [5]] and m.chunk([], 3) == [] and raises(lambda: m.chunk([1], 0)),
}
def raises(f):
    try:
        f()
    except ValueError:
        return True
    return False
present = [f for f in cases if pathlib.Path(f).exists()]
assert len(present) == 1, f"expected exactly one target file, found {present}"
name = present[0]
src = pathlib.Path(name).read_text()
first_line = src.splitlines()[0]
assert first_line == "# owner: team-orion", f"first line is {first_line!r}"
for node in ast.walk(ast.parse(src)):
    if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef)):
        assert ast.get_docstring(node), f"function {node.name} has no docstring"
for p in pathlib.Path(".").glob("*.py"):
    # An agent-written check.py is exempt from the header rule.
    if p.name != "check.py":
        assert p.read_text().splitlines()[:1] == ["# owner: team-orion"], f"{p} lacks the owner header"
import importlib
assert cases[name](importlib.import_module(name[:-3])), "function behaviour wrong"
print("pass")
