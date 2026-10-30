#!/usr/bin/env python3
"""vault: read-only access to the records in vault.db."""
import hashlib, json, pathlib, secrets, sys, zlib

HERE = pathlib.Path(__file__).resolve().parent
STATE = HERE / ".vault" / "state.json"
PAGE = 20
CALLS_PER_SESSION = 6
USAGE = ("usage: vault.py login [--renew TOKEN] | tables TOKEN | open TOKEN TABLE | "
         "fetch TOKEN CURSOR PAGE ACK | close TOKEN CURSOR")


def db():
    raw = (HERE / "vault.db").read_bytes()
    key = b"cmh-vault"
    return json.loads(zlib.decompress(bytes(b ^ key[i % len(key)] for i, b in enumerate(raw))))


def ack(cursor, page):
    return hashlib.sha1(f"{cursor}:{page}".encode()).hexdigest()[:6]


def fail(code):
    print(f"ERR {code}")
    save()
    sys.exit(1)


def save():
    STATE.parent.mkdir(exist_ok=True)
    STATE.write_text(json.dumps(st))


st = json.loads(STATE.read_text()) if STATE.exists() else {"sessions": {}, "cursors": {}, "fetches": 0}
a = sys.argv[1:]
if not a or a[0] in ("-h", "--help", "help"):
    print(USAGE)
    sys.exit(0)
cmd = a[0]
if cmd == "login":
    tok = secrets.token_hex(4)
    st["sessions"][tok] = 0
    if len(a) == 3 and a[1] == "--renew":
        if a[2] not in st["sessions"]:
            fail(403)
        for c in st["cursors"].values():
            if c["owner"] == a[2]:
                c["owner"] = tok
    elif len(a) != 1:
        print(USAGE); sys.exit(2)
    print(f"OK SESSION {tok}")
    save()
    sys.exit(0)
arity = {"tables": 2, "open": 3, "fetch": 5, "close": 3}
if cmd not in arity or len(a) != arity[cmd]:
    print(USAGE); sys.exit(2)
tok = a[1]
if tok not in st["sessions"]:
    fail(403)
st["sessions"][tok] += 1
if st["sessions"][tok] > CALLS_PER_SESSION:
    fail(401)
data = db()
if cmd == "tables":
    print("OK TABLES " + " ".join(sorted(data)))
elif cmd == "open":
    if a[2] not in data:
        fail(404)
    cid = f"c{len(st['cursors']) + 1}"
    pages = max(1, -(-len(data[a[2]]) // PAGE))
    st["cursors"][cid] = {"owner": tok, "table": a[2], "pages": pages, "next": 0, "closed": False}
    print(f"OK CURSOR {cid} PAGES {pages} ACK {ack(cid, 0)}")
elif cmd == "fetch":
    c = st["cursors"].get(a[2])
    if c is None or c["closed"]:
        fail(404)
    if c["owner"] != tok:
        fail(403)
    if not a[3].isdigit() or int(a[3]) != c["next"]:
        fail(409)
    if a[4] != ack(a[2], c["next"]):
        fail(412)
    st["fetches"] += 1
    if st["fetches"] % 4 == 0:
        fail(503)
    p = c["next"]
    for row in data[c["table"]][p * PAGE:(p + 1) * PAGE]:
        print("ROW " + json.dumps(row, separators=(",", ":")))
    c["next"] = p + 1
    print(f"OK ACK {ack(a[2], p + 1)}" if p + 1 < c["pages"] else "OK END")
elif cmd == "close":
    c = st["cursors"].get(a[2])
    if c is None:
        fail(404)
    if c["owner"] != tok:
        fail(403)
    c["closed"] = True
    print("OK CLOSED")
save()
