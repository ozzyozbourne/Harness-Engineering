import random, sys
sys.path.insert(0, ".")
from graph import shortest_path

def ref(edges, s, t):
    import heapq
    adj = {}
    for u, v, w in edges:
        adj.setdefault(u, []).append((v, w))
    best = {s: (0, 0)}
    pq = [(0, 0, 0, s)]
    n = 1
    while pq:
        c, h, _, u = heapq.heappop(pq)
        if best.get(u, (float("inf"), 0)) < (c, h):
            continue
        for v, w in adj.get(u, []):
            k = (c + w, h + 1)
            if k < best.get(v, (float("inf"), 0)):
                best[v] = k; n += 1
                heapq.heappush(pq, (k[0], k[1], n, v))
    return best.get(t)

def valid(edges, res, s, t, want):
    if want is None:
        return res is None
    cost, path = res
    if path[0] != s or path[-1] != t or abs(cost - want[0]) > 1e-9 or len(path) - 1 != want[1]:
        return False
    total = 0
    for a, b in zip(path, path[1:]):
        ws = [w for u, v, w in edges if u == a and v == b]
        if not ws:
            return False
        total += min(ws)
    return abs(total - cost) < 1e-9

assert shortest_path([], "x", "x") == (0, ["x"])
assert shortest_path([("a", "b", 1)], "b", "a") is None
e = [("a", "b", 1), ("b", "c", 1), ("a", "c", 2), ("a", "c", 5)]
assert shortest_path(e, "a", "c") == (2, ["a", "c"])
e = [(1, 2, 0), (2, 3, 0), (1, 3, 0.5)]
assert shortest_path(e, 1, 3) == (0, [1, 2, 3])
rnd = random.Random(9)
for _ in range(300):
    n = rnd.randint(2, 12)
    edges = [(rnd.randrange(n), rnd.randrange(n), rnd.choice([0, 1, 2, 3, 2.5])) for _ in range(rnd.randint(0, 40))]
    s, t = rnd.randrange(n), rnd.randrange(n)
    want = ref(edges, s, t)
    got = shortest_path(edges, s, t)
    assert valid(edges, got, s, t, want), (edges, s, t, got, want)
print("pass")
