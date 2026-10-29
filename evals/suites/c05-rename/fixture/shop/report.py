import shop


def daily(carts):
    """Sum of every cart's total, using shop.calc_total directly."""
    return round(sum(shop.calc_total(c.items) for c in carts), 2)
