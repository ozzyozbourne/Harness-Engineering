TAX = {"food": 0.05, "default": 0.2}


def calc_total(items, coupon=None):
    """Total of (name, category, price, qty) items with tax and an optional percent coupon."""
    total = 0.0
    for _name, category, price, qty in items:
        total += price * qty * (1 + TAX.get(category, TAX["default"]))
    if coupon:
        total *= 1 - coupon / 100
    return round(total, 2)
