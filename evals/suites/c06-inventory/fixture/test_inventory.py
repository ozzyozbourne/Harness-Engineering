import unittest
from inventory import Inventory


class InventoryTest(unittest.TestCase):
    def setUp(self):
        self.inv = Inventory()
        self.inv.add("a", 5, price=2.0)
        self.inv.add("b", 1, price=10.0)
        self.inv.add("c", 3)

    def test_remove_too_many(self):
        with self.assertRaises(ValueError):
            self.inv.remove("a", 6)
        self.assertEqual(self.inv.count("a"), 5)

    def test_remove_unknown(self):
        with self.assertRaises(KeyError):
            self.inv.remove("zzz", 1)

    def test_low_stock_inclusive(self):
        self.assertEqual(self.inv.low_stock(3), ["b", "c"])

    def test_total_value_skips_unpriced(self):
        self.assertEqual(self.inv.total_value(), 20.0)


if __name__ == "__main__":
    unittest.main()
