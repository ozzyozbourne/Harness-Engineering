from stats import mean, median, mode, variance


def test_mean():
    assert mean([1, 2, 3, 4]) == 2.5


def test_median_odd():
    assert median([3, 1, 2]) == 2


def test_median_even():
    assert median([4, 1, 3, 2]) == 2.5


def test_mode_tie_smallest():
    assert mode([3, 3, 1, 1, 2]) == 1


def test_variance_population():
    assert variance([2, 4, 4, 4, 5, 5, 7, 9]) == 4


if __name__ == "__main__":
    failed = 0
    for name, fn in list(globals().items()):
        if name.startswith("test_"):
            try:
                fn()
                print("ok  ", name)
            except AssertionError:
                failed += 1
                print("FAIL", name)
    raise SystemExit(1 if failed else 0)
