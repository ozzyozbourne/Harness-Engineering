import csv, random

def rows():
    rnd = random.Random(3)
    regions = ["North", " north", "SOUTH", "South ", "east", " East ", "West"]
    products = ["widget", "gadget", "doohickey", "sprocket", "gizmo"]
    out = []
    for i in range(400):
        out.append({"date": f"2026-{rnd.randint(1, 12):02d}-{rnd.randint(1, 28):02d}",
                    "region": rnd.choice(regions), "product": rnd.choice(products),
                    "units": rnd.randint(1, 40), "unit_price": f"{rnd.uniform(1, 99):.2f}"})
    return out

if __name__ == "__main__":
    with open("sales.csv", "w", newline="") as f:
        w = csv.DictWriter(f, ["date", "region", "product", "units", "unit_price"])
        w.writeheader()
        w.writerows(rows())
