#!/usr/bin/env python3
"""Eval runner: runs every harness on the same suites and compares them with Prime Agent.

A suite is a directory under evals/suites/ with suite.json:
  {"track": "coding" | "learning",
   "episodes": [{"prompt": str, "fixture": dir?, "setup": file?, "check": file,
                 "transfer": bool?}]}
Episodes of one suite run in order, each in a fresh working directory and a fresh
process, sharing one harness state directory. A check script runs afterwards with
cwd = the working directory: `python3 check.py <suite_dir> <answer_file>`; exit 0
means pass. Fixtures, setup and checks are hidden from the agent.

Every model request goes through a local logging proxy to an Anthropic-compatible
upstream (--upstream opencode, the default, or openrouter), so every harness uses the
same model and key and token usage is measured the same way.
"""
import argparse, concurrent.futures as cf, datetime, http.client, http.server, json, os
import pathlib, shutil, subprocess, sys, tempfile, threading, time, urllib.parse, uuid

ROOT = pathlib.Path(__file__).resolve().parent
REPO = ROOT.parent

# ---------------------------------------------------------------- proxy

class Usage:
    def __init__(self):
        self.lock = threading.Lock()
        self.by_tag = {}

    def add(self, tag, status, usage):
        with self.lock:
            u = self.by_tag.setdefault(tag, {"requests": 0, "errors": 0, "input": 0, "output": 0,
                                             "cache_read": 0, "cache_write": 0})
            u["requests"] += 1
            if status != 200:
                u["errors"] += 1
            for k, src in (("input", "input_tokens"), ("output", "output_tokens"),
                           ("cache_read", "cache_read_input_tokens"),
                           ("cache_write", "cache_creation_input_tokens")):
                u[k] += int(usage.get(src) or 0)

    def take(self, tag):
        with self.lock:
            return self.by_tag.pop(tag, {"requests": 0, "errors": 0, "input": 0, "output": 0,
                                         "cache_read": 0, "cache_write": 0})


def usage_of(body: bytes) -> dict:
    """Usage from a JSON or SSE Messages response (message_start + message_delta)."""
    text = body.decode("utf-8", "replace")
    try:
        return json.loads(text).get("usage") or {}
    except ValueError:
        pass
    total = {}
    for line in text.splitlines():
        if not line.startswith("data:"):
            continue
        try:
            ev = json.loads(line[5:])
        except ValueError:
            continue
        u = (ev.get("message") or {}).get("usage") if ev.get("type") == "message_start" else ev.get("usage")
        for k, v in (u or {}).items():
            if isinstance(v, int):
                total[k] = v if k != "output_tokens" else max(total.get(k, 0), v)
    return total


def secret(env_name, file_name):
    """A key from the environment, else from ~/.config/cmh/<file_name> (mode 600)."""
    value = os.environ.get(env_name)
    if not value:
        path = pathlib.Path.home() / ".config/cmh" / file_name
        value = path.read_text().strip() if path.exists() else ""
    if not value:
        sys.exit(f"missing {env_name}")
    return value


# Upstreams speaking the Anthropic Messages API: (host, path prefix, key env, key file).
UPSTREAMS = {
    "openrouter": ("openrouter.ai", "/api", "OPENROUTER_API_KEY", "openrouter-key"),
    "opencode": ("opencode.ai", "/inference/go", "OPENCODE_API_KEY", "opencode-key"),
}


def start_proxy(usage: Usage, log_path: pathlib.Path, upstream: str) -> int:
    log_lock = threading.Lock()
    host, prefix, key_env, key_file = UPSTREAMS[upstream]
    key = secret(key_env, key_file)
    log_lock = threading.Lock()

    class Handler(http.server.BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def do_POST(self):
            # Path is /t/<tag>/v1/messages...; the tag attributes usage to an episode.
            parts = self.path.split("/", 3)
            tag, rest = (parts[2], "/" + parts[3]) if len(parts) == 4 and parts[1] == "t" else ("untagged", self.path)
            body = self.rfile.read(int(self.headers.get("content-length", 0)))
            headers = {k: v for k, v in self.headers.items()
                       if k.lower() not in ("host", "content-length", "accept-encoding", "connection")}
            out, status, resp_headers = b"", 502, []
            # Every harness authenticates to the proxy with a placeholder API key; the proxy
            # swaps in the real upstream key, so no harness ever sees it.
            headers = {k: v for k, v in headers.items() if k.lower() not in ("authorization", "x-api-key", "anthropic-beta")}
            headers["x-api-key"] = key
            if upstream == "opencode":
                # OpenCode Go rejects requests without a session id; one stable id per episode.
                headers["authorization"] = f"Bearer {key}"
                headers["x-opencode-session"] = str(uuid.uuid5(uuid.NAMESPACE_URL, "cmh-eval/" + tag))
            try:
                conn = http.client.HTTPSConnection(host, timeout=900)
                conn.request("POST", prefix + rest, body=body, headers=headers)
                resp = conn.getresponse()
                status, resp_headers = resp.status, resp.getheaders()
                self.send_response(status)
                for k, v in resp_headers:
                    if k.lower() not in ("transfer-encoding", "content-length", "connection", "content-encoding"):
                        self.send_header(k, v)
                self.send_header("transfer-encoding", "chunked")
                self.end_headers()
                while True:
                    chunk = resp.read1(65536)
                    if not chunk:
                        break
                    out += chunk
                    self.wfile.write(b"%x\r\n%s\r\n" % (len(chunk), chunk))
                    self.wfile.flush()
                self.wfile.write(b"0\r\n\r\n")
            except Exception as e:  # upstream or client failure; record it
                out = str(e).encode()
            usage.add(tag, status, usage_of(out))
            if status != 200:
                with log_lock, open(log_path, "a") as f:
                    f.write(json.dumps({"tag": tag, "status": status, "resp": out[:2000].decode("utf-8", "replace")}) + "\n")

        def log_message(self, *a):
            pass

    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    return server.server_address[1]

# ---------------------------------------------------------------- harnesses

def load_harnesses():
    return json.loads((ROOT / "harnesses.json").read_text())


def expand(value, ctx):
    if isinstance(value, list):
        return [expand(v, ctx) for v in value]
    value = os.path.expanduser(value.replace("{repo}", str(REPO)))
    for k, v in ctx.items():
        value = value.replace("{" + k + "}", v)
    if value.startswith("$"):
        return os.environ.get(value[1:], "")
    return value

# ---------------------------------------------------------------- episodes

def run_episode(h, suite_dir, ep, idx, state, model, proxy_port, usage, tag, timeout, logs_dir):
    work = pathlib.Path(tempfile.mkdtemp(prefix="cmh-eval-work-"))
    try:
        if ep.get("fixture"):
            shutil.copytree(suite_dir / ep["fixture"], work, dirs_exist_ok=True)
        if ep.get("setup"):
            subprocess.run([sys.executable, str(suite_dir / ep["setup"]), str(suite_dir)], cwd=work, check=True,
                           capture_output=True, timeout=120, env={**os.environ, "EVAL_EPISODE": str(idx + 1)})
        base = f"http://127.0.0.1:{proxy_port}/t/{tag}"
        ctx = {"state": str(state), "model": model, "prompt": ep["prompt"], "work": str(work), "base_url": base}
        env = {k: v for k, v in os.environ.items() if k not in ("OPENROUTER_API_KEY", "OPENCODE_API_KEY", "CLAUDE_CODE_OAUTH_TOKEN")}
        for k, v in h.get("env", {}).items():
            env[k] = expand(v, ctx)
        if h.get("models_json"):  # Prime Agent: base URL override through its models.json
            (state / "models.json").write_text(json.dumps({"providers": {"anthropic": {"baseUrl": base}}}))
        cmd = expand(h["cmd"], ctx)
        t0 = time.time()
        try:
            p = subprocess.run(cmd, cwd=work, env=env, capture_output=True, text=True, timeout=timeout,
                               stdin=subprocess.DEVNULL)
            rc, out, err = p.returncode, p.stdout, p.stderr
        except subprocess.TimeoutExpired as e:
            text = lambda b: b.decode("utf-8", "replace") if isinstance(b, bytes) else (b or "")
            rc, out, err = "timeout", text(e.stdout), text(e.stderr)
        secs = time.time() - t0
        answer = logs_dir / f"{tag}.stdout.txt"
        answer.write_text(out)
        (logs_dir / f"{tag}.stderr.txt").write_text(err[-200_000:])
        chk = subprocess.run([sys.executable, str(suite_dir / ep["check"]), str(suite_dir), str(answer)],
                             cwd=work, capture_output=True, text=True, timeout=300,
                             env={**os.environ, "EVAL_EPISODE": str(idx + 1)})
        return {"passed": chk.returncode == 0, "check_output": (chk.stdout + chk.stderr)[-1500:],
                "exit": rc, "seconds": round(secs, 1), "usage": usage.take(tag)}
    finally:
        shutil.rmtree(work, ignore_errors=True)


def run_suite(hname, h, suite_dir, trial, model, port, usage, timeout, logs_dir, fresh_state=False):
    spec = json.loads((suite_dir / "suite.json").read_text())
    state = pathlib.Path(tempfile.mkdtemp(prefix=f"cmh-eval-state-{hname}-"))
    rows = []
    try:
        for i, ep in enumerate(spec["episodes"]):
            if fresh_state:  # control: no memory carried between episodes
                shutil.rmtree(state, ignore_errors=True)
                state.mkdir()
            tag = f"{hname}.{suite_dir.name}.t{trial}.e{i + 1}"
            r = run_episode(h, suite_dir, ep, i, state, model, port, usage, tag, timeout, logs_dir)
            r.update({"harness": hname, "suite": suite_dir.name, "track": spec["track"], "trial": trial,
                      "episode": i + 1, "transfer": bool(ep.get("transfer"))})
            rows.append(r)
            print(f"[{hname}] {suite_dir.name} t{trial} e{i + 1}: {'PASS' if r['passed'] else 'FAIL'} "
                  f"({r['seconds']}s, {r['usage']['requests']} req)", flush=True)
    finally:
        shutil.rmtree(state, ignore_errors=True)
    return rows

# ---------------------------------------------------------------- report

def summarize(rows, baseline, threshold):
    def rate(sel):
        return (sum(r["passed"] for r in sel) / len(sel)) if sel else 0.0

    harnesses = sorted({r["harness"] for r in rows}, key=lambda n: (n != baseline, n))
    table = []
    for n in harnesses:
        mine = [r for r in rows if r["harness"] == n]
        coding = [r for r in mine if r["track"] == "coding"]
        learn = [r for r in mine if r["track"] == "learning" and r["transfer"]]
        tokens = sum(r["usage"]["input"] + r["usage"]["output"] + r["usage"]["cache_read"] + r["usage"]["cache_write"] for r in mine)
        table.append({"harness": n, "episodes": len(mine), "overall": rate(mine), "coding": rate(coding),
                      "learning_transfer": rate(learn), "median_seconds": sorted(r["seconds"] for r in mine)[len(mine) // 2],
                      "total_tokens": tokens, "api_errors": sum(r["usage"]["errors"] for r in mine)})
    base = next((t for t in table if t["harness"] == baseline), None)
    for t in table:
        for k in ("overall", "coding", "learning_transfer"):
            t[f"{k}_vs_{baseline}"] = (t[k] / base[k]) if base and base[k] else None
        t["meets_threshold"] = bool(base) and t[f"overall_vs_{baseline}"] is not None and t[f"overall_vs_{baseline}"] >= threshold
    return table


def markdown(table, baseline, threshold, meta):
    pct = lambda x: "n/a" if x is None else f"{x * 100:.0f}%"
    lines = [f"# Eval results {meta['started']}", "",
             f"Model `{meta['model']}`, {meta['trials']} trial(s), baseline `{baseline}`, "
             f"pass bar = {threshold * 100:.0f}% of the baseline's overall pass rate.", "",
             f"| Harness | Overall | vs {baseline} | Coding | Learning transfer | Median s/episode | Tokens | API errors | Meets bar |",
             "|---|---|---|---|---|---|---|---|---|"]
    for t in table:
        lines.append(f"| {t['harness']} | {pct(t['overall'])} | {pct(t[f'overall_vs_{baseline}'])} | {pct(t['coding'])} | "
                     f"{pct(t['learning_transfer'])} | {t['median_seconds']} | {t['total_tokens']:,} | {t['api_errors']} | "
                     f"{'yes' if t['meets_threshold'] else 'NO'} |")
    return "\n".join(lines) + "\n"


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--harness", default="prime,clojure,rust,racket,lean")
    ap.add_argument("--suite", default="", help="comma-separated suite names (default: all)")
    ap.add_argument("--trials", type=int, default=2)
    ap.add_argument("--upstream", choices=sorted(UPSTREAMS), default="opencode")
    ap.add_argument("--model", default="deepseek-v4.1-flash")
    ap.add_argument("--jobs", type=int, default=6)
    ap.add_argument("--timeout", type=int, default=900, help="seconds per episode")
    ap.add_argument("--baseline", default="prime")
    ap.add_argument("--threshold", type=float, default=0.8)
    ap.add_argument("--out", default="")
    ap.add_argument("--fresh-state", action="store_true",
                    help="control run: wipe harness state between episodes, so nothing can be learned")
    ap.add_argument("--report-only", default="", help="re-summarize an existing results.jsonl")
    a = ap.parse_args()

    if a.report_only:
        rows = [json.loads(l) for l in open(a.report_only)]
        table = summarize(rows, a.baseline, a.threshold)
        print(markdown(table, a.baseline, a.threshold, {"started": "(re-report)", "model": rows[0].get("model", "?"), "trials": "?"}))
        return

    started = datetime.datetime.now().strftime("%Y%m%d-%H%M%S")
    out = pathlib.Path(a.out or ROOT / "results" / started)
    if (out / "summary.md").exists():
        # A supervised service reruns its command when it exits; never append a second run.
        sys.exit(f"{out} already holds a finished run")
    logs = out / "logs"
    logs.mkdir(parents=True, exist_ok=True)
    all_h = load_harnesses()
    names = [n for n in a.harness.split(",") if n]
    suites = sorted(p for p in (ROOT / "suites").iterdir() if (p / "suite.json").exists())
    if a.suite:
        wanted = set(a.suite.split(","))
        suites = [s for s in suites if s.name in wanted]
    usage = Usage()
    port = start_proxy(usage, out / "proxy-errors.jsonl", a.upstream)
    jobs = [(n, s, t) for t in range(1, a.trials + 1) for s in suites for n in names]
    rows = []
    with cf.ThreadPoolExecutor(a.jobs) as pool, open(out / "results.jsonl", "a") as f:
        futs = [pool.submit(run_suite, n, all_h[n], s, t, a.model, port, usage, a.timeout, logs,
                            a.fresh_state) for n, s, t in jobs]
        for fut in cf.as_completed(futs):
            for r in fut.result():
                r["model"] = a.model
                rows.append(r)
                f.write(json.dumps(r) + "\n")
                f.flush()
    table = summarize(rows, a.baseline, a.threshold)
    md = markdown(table, a.baseline, a.threshold, {"started": started, "model": a.model, "trials": a.trials})
    (out / "summary.md").write_text(md)
    (out / "summary.json").write_text(json.dumps(table, indent=2))
    print(md)


if __name__ == "__main__":
    main()
