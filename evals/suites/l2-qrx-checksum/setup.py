import os, random

def blobs():
    rnd = random.Random(21)
    return {n: bytes(rnd.randrange(256) for _ in range(rnd.randint(3000, 9000)))
            for n in ["data_a.bin", "data_b.bin", "data_c.bin", "data_d.bin", "x_extra.bin"]}

def qrx(data):
    h = 7
    for i, b in enumerate(data):
        h = (h * 31 + b + i) % 65521
    return f"QRX-{h:04X}"

# Files each episode gets; the runner passes the 1-based episode number in EVAL_EPISODE.
EPISODES = [["data_a.bin"], ["data_b.bin"], ["data_c.bin"], ["data_d.bin", "data_a.bin", "x_extra.bin"]]

if __name__ == "__main__":
    ep = int(os.environ.get("EVAL_EPISODE", "1"))
    b = blobs()
    for n in EPISODES[ep - 1]:
        open(n, "wb").write(b[n])
