def to_roman(n: int) -> str:
    """Convert an integer in 1..3999 to a standard (subtractive) Roman numeral.

    Raise ValueError for anything outside 1..3999 or a non-int (bool is not an int here).
    """
    raise NotImplementedError


def from_roman(s: str) -> int:
    """Parse a standard Roman numeral (uppercase only) into an integer.

    Accept only canonical numerals, i.e. exactly the strings to_roman produces.
    Raise ValueError for anything else, such as "", "IIII", "IC", "MMMM", "iv" or "VX".
    """
    raise NotImplementedError
