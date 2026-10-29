import random, sqlite3

def data():
    rnd = random.Random(11)
    names = ["Ada", "Ben", "Cy", "Dee", "Eli", "Fay", "Gus", "Hal", "Ivy", "Jon", "Kai", "Lou", "Max", "Ned", "Ola"]
    customers = [(i + 1, n) for i, n in enumerate(names)]
    orders = []
    for oid in range(1, 501):
        cid = rnd.randint(1, len(names))
        year = rnd.choice([2024, 2025, 2025, 2026])
        ts = f"{year}-{rnd.randint(1, 12):02d}-{rnd.randint(1, 28):02d} {rnd.randint(0, 23):02d}:00:00"
        orders.append((oid, cid, ts, round(rnd.uniform(5, 120), 2), rnd.choice(["paid"] * 6 + ["refunded"])))
    return customers, orders

if __name__ == "__main__":
    customers, orders = data()
    db = sqlite3.connect("shop.db")
    db.execute("create table customers (id integer primary key, name text)")
    db.execute("create table orders (id integer primary key, customer_id integer, placed_at text, amount real, status text)")
    db.executemany("insert into customers values (?, ?)", customers)
    db.executemany("insert into orders values (?, ?, ?, ?, ?)", orders)
    db.commit()
