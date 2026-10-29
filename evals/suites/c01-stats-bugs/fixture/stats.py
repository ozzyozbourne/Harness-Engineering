def mean(xs):
    return sum(xs) / len(xs)


def median(xs):
    s = sorted(xs)
    return s[len(s) // 2]


def mode(xs):
    """Most common value; ties go to the smallest value."""
    counts = {}
    for x in xs:
        counts[x] = counts.get(x, 0) + 1
    return max(counts, key=lambda k: counts[k])


def variance(xs):
    """Population variance."""
    m = mean(xs)
    return sum((x - m) ** 2 for x in xs) / (len(xs) - 1)
