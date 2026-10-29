class Inventory:
    def __init__(self):
        self._stock = {}
        self._prices = {}

    def add(self, sku, qty, price=None):
        if qty <= 0:
            raise ValueError("qty must be positive")
        self._stock[sku] = self._stock.get(sku, 0) + qty
        if price is not None:
            self._prices[sku] = price

    def remove(self, sku, qty):
        """Remove qty units; raise ValueError if that would go below zero. Unknown sku is KeyError."""
        self._stock[sku] -= qty

    def count(self, sku):
        return self._stock.get(sku, 0)

    def total_value(self):
        return sum(q * self._prices[s] for s, q in self._stock.items())

    def low_stock(self, threshold):
        """SKUs whose count is at or below threshold, sorted."""
        return sorted(s for s, q in self._stock.items() if q < threshold)
