import sys, os, unittest
sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from shop import calc_total
from shop.cart import Cart
from shop.checkout import receipt
from shop.report import daily


class ShopTest(unittest.TestCase):
    def test_total(self):
        self.assertEqual(calc_total([("apple", "food", 2.0, 3)]), 6.3)

    def test_coupon(self):
        c = Cart(); c.add("pen", "office", 10.0, 2)
        self.assertEqual(c.total(coupon=50), 12.0)

    def test_receipt(self):
        c = Cart(); c.add("pen", "office", 10.0)
        self.assertIn("subtotal 12.00", receipt(c))

    def test_daily(self):
        a = Cart(); a.add("pen", "office", 10.0)
        b = Cart(); b.add("apple", "food", 2.0)
        self.assertEqual(daily([a, b]), 14.1)


if __name__ == "__main__":
    unittest.main()
