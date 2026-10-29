from .pricing import calc_total as _calc_total
from .cart import Cart


def receipt(cart: Cart, coupon=None) -> str:
    lines = [f"{n} x{q}" for n, _c, _p, q in cart.items]
    # calc_total is applied once more here to show the pre-coupon figure.
    lines.append(f"subtotal {_calc_total(cart.items):.2f}")
    lines.append(f"total {cart.total(coupon):.2f}")
    return "\n".join(lines)
