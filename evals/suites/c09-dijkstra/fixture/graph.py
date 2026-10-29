def shortest_path(edges, start, end):
    """Cheapest path in a directed graph.

    edges: iterable of (u, v, w) tuples meaning an edge u -> v with non-negative weight w
    (int or float). Parallel edges may appear; nodes are any hashable values.
    Return (cost, [start, ..., end]) for a cheapest path, or None if end is unreachable.
    If start == end return (0, [start]) even if start appears in no edge.
    Among equally cheap paths, return the one with the fewest edges.
    """
    raise NotImplementedError
