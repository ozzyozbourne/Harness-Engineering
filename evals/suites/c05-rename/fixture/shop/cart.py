from . import pricing


class Cart:
    def __init__(self):
        self.items = []

    def add(self, name, category, price, qty=1):
        self.items.append((name, category, price, qty))

    def total(self, coupon=None):
        return pricing.calc_total(self.items, coupon)
