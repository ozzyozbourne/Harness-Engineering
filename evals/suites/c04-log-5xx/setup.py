import random

def lines():
    rnd = random.Random(4)
    ips = [f"10.0.{rnd.randint(0, 3)}.{rnd.randint(1, 60)}" for _ in range(40)]
    paths = ["/", "/api/items", "/api/items/7", "/login", "/static/app.js", "/health"]
    agents = ['"Mozilla/5.0 (X11; Linux x86_64)"', '"curl/8.4.0"', '"Go-http-client/1.1 \\"bot\\""']
    statuses = [200] * 30 + [301, 304, 404, 404, 500, 502, 503, 504]
    out = []
    for i in range(6000):
        ip = rnd.choice(ips)
        st = rnd.choice(statuses)
        # Some request lines contain a quoted " 500 " so naive splitting miscounts.
        path = rnd.choice(paths) + ("?q=%22 500 %22" if rnd.random() < 0.03 else "")
        out.append(f'{ip} - - [12/Mar/2026:10:{i // 600:02d}:{i % 60:02d} +0000] "GET {path} HTTP/1.1" {st} {rnd.randint(0, 9000)} "-" {rnd.choice(agents)}')
    return out

if __name__ == "__main__":
    open("access.log", "w").write("\n".join(lines()) + "\n")
