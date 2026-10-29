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
import pathlib, shutil, statistics, subprocess, sys, tempfile, threading, time, urllib.parse, uuid

ROOT = pathlib.Path(__file__).resolve().parent
REPO = ROOT.parent

# ---------------------------------------------------------------- proxy

# Our usage field -> the Anthropic Messages API usage field it is summed from.
TOKEN_FIELDS = {"input": "input_tokens", "output": "output_tokens",
                "cache_read": "cache_read_input_tokens", "cache_write": "cache_creation_input_tokens"}


def empty_usage():
    return {"requests": 0, "errors": 0, **{field: 0 for field in TOKEN_FIELDS}}


class Usage:
    """Request and token counts per tag, filled in by the proxy threads."""

    def __init__(self):
        self.lock = threading.Lock()
        self.by_tag = {}

    def add(self, tag, status, usage):
        with self.lock:
            u = self.by_tag.setdefault(tag, empty_usage())
            u["requests"] += 1
            if status != 200:
                u["errors"] += 1
            for field, api_field in TOKEN_FIELDS.items():
                u[field] += int(usage.get(api_field) or 0)

    def take(self, tag):
        with self.lock:
            return self.by_tag.pop(tag, empty_usage())


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


# Upstreams speaking the Anthropic Messages API. extra_headers are added to every
# forwarded request; {key} is the upstream key and {session} a stable id per tag.
UPSTREAMS = {
    "openrouter": {"host": "openrouter.ai", "prefix": "/api",
                   "key_env": "OPENROUTER_API_KEY", "key_file": "openrouter-key", "extra_headers": {}},
    "opencode": {"host": "opencode.ai", "prefix": "/inference/go",
                 "key_env": "OPENCODE_API_KEY", "key_file": "opencode-key",
                 # OpenCode Go rejects requests without a session id; one stable id per episode.
                 "extra_headers": {"authorization": "Bearer {key}", "x-opencode-session": "{session}"}},
}

# Credentials a harness must never see; it reaches the model only through the proxy.
SECRET_ENV_VARS = frozenset({u["key_env"] for u in UPSTREAMS.values()} | {"CLAUDE_CODE_OAUTH_TOKEN"})

# Hop-by-hop and length headers are recomputed per connection; the client's own
# credentials and beta flags are replaced or dropped before forwarding.
DROP_REQUEST_HEADERS = {"host", "content-length", "accept-encoding", "connection",
                        "authorization", "x-api-key", "anthropic-beta"}
DROP_RESPONSE_HEADERS = {"transfer-encoding", "content-length", "connection", "content-encoding"}


def parse_tag(path):
    """Split /t/<tag>/v1/messages... into (tag, /v1/messages...); the tag attributes usage to an episode."""
    parts = path.split("/", 3)
    if len(parts) == 4 and parts[1] == "t":
        return parts[2], "/" + parts[3]
    return "untagged", path


def start_proxy(usage: Usage, log_path: pathlib.Path, upstream: str) -> int:
    up = UPSTREAMS[upstream]
    key = secret(up["key_env"], up["key_file"])
    log_lock = threading.Lock()

    def upstream_headers(incoming, tag):
        # Every harness authenticates to the proxy with a placeholder API key; the proxy
        # swaps in the real upstream key, so no harness ever sees it.
        headers = {k: v for k, v in incoming.items() if k.lower() not in DROP_REQUEST_HEADERS}
        headers["x-api-key"] = key
        session = str(uuid.uuid5(uuid.NAMESPACE_URL, "cmh-eval/" + tag))
        for name, template in up["extra_headers"].items():
            headers[name] = template.format(key=key, session=session)
        return headers

    class Handler(http.server.BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def do_POST(self):
            tag, rest = parse_tag(self.path)
            body = self.rfile.read(int(self.headers.get("content-length", 0)))
            headers = upstream_headers(self.headers, tag)
            out, status = bytearray(), 502
            conn = http.client.HTTPSConnection(up["host"], timeout=900)
            try:
                conn.request("POST", up["prefix"] + rest, body=body, headers=headers)
                resp = conn.getresponse()
                status = resp.status
                self.send_response(status)
                for k, v in resp.getheaders():
                    if k.lower() not in DROP_RESPONSE_HEADERS:
                        self.send_header(k, v)
                self.send_header("transfer-encoding", "chunked")
                self.end_headers()
                # Relay each chunk as it arrives so streaming harnesses see tokens live.
                while True:
                    chunk = resp.read1(65536)
                    if not chunk:
                        break
                    out += chunk
                    self.wfile.write(b"%x\r\n%s\r\n" % (len(chunk), chunk))
                    self.wfile.flush()
                self.wfile.write(b"0\r\n\r\n")
            except Exception as e:  # upstream or client failure; record it
                out = bytearray(str(e).encode())
            finally:
                conn.close()
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

def expand(value, ctx):
    """Fill {name} placeholders from ctx into a harness cmd/env string or list of strings.

    Plain per-key replace rather than str.format, because prompts contain literal braces.
    """
    if isinstance(value, list):
        return [expand(v, ctx) for v in value]
    value = os.path.expanduser(value)
    for k, v in ctx.items():
        value = value.replace("{" + k + "}", v)
    return value

# ---------------------------------------------------------------- episodes

EPISODE_MEM_MB = 2500  # summed RSS of one episode's process session
PAGE_SIZE = os.sysconf("SC_PAGE_SIZE")


def session_rss_mb(sid):
    """Resident memory of every process in session `sid`, in MB."""
    total = 0
    for d in os.listdir("/proc"):
        if not d.isdigit():
            continue
        try:
            stat = pathlib.Path(f"/proc/{d}/stat").read_text()
            fields = stat[stat.rindex(")") + 2:].split()
            if int(fields[3]) == sid:  # field 6 of stat: session id
                total += int(fields[21]) * PAGE_SIZE  # field 24: rss pages
        except (OSError, ValueError, IndexError):
            continue
    return total // (1 << 20)


def kill_session(p):
    try:
        os.killpg(p.pid, 9)
    except ProcessLookupError:
        pass
    p.wait()


def prepare_workdir(work, suite_dir, ep, script_env):
    """Fill `work` with the episode's fixture, then run its setup script there."""
    if ep.get("fixture"):
        shutil.copytree(suite_dir / ep["fixture"], work, dirs_exist_ok=True)
    if ep.get("setup"):
        subprocess.run([sys.executable, str(suite_dir / ep["setup"]), str(suite_dir)], cwd=work, check=True,
                       capture_output=True, timeout=120, env=script_env)


def run_watched(cmd, cwd, env, out_f, err_f, timeout):
    """Run cmd in its own session; return its exit code, "timeout" or "memory-limit"."""
    t0 = time.time()
    p = subprocess.Popen(cmd, cwd=cwd, env=env, stdout=out_f, stderr=err_f,
                         stdin=subprocess.DEVNULL, start_new_session=True)
    rc = None
    while rc is None:
        try:
            # Poll rather than wait out the whole timeout, so memory is checked every 2 s.
            rc = p.wait(timeout=2)
        except subprocess.TimeoutExpired:
            if time.time() - t0 > timeout:
                rc = "timeout"
            elif session_rss_mb(p.pid) > EPISODE_MEM_MB:
                rc = "memory-limit"
            if rc is not None:
                kill_session(p)
    if rc == "memory-limit":
        err_f.write(f"\n[eval] killed: episode processes exceeded {EPISODE_MEM_MB} MB\n")
    return rc


def run_episode(h, suite_dir, ep, number, state, model, proxy_port, usage, tag, timeout, logs_dir):
    """Run episode `number` (1-based) of a suite with harness h and check its result."""
    script_env = {**os.environ, "EVAL_EPISODE": str(number)}  # setup and check scripts only
    work = pathlib.Path(tempfile.mkdtemp(prefix="cmh-eval-work-"))
    try:
        prepare_workdir(work, suite_dir, ep, script_env)
        base = f"http://127.0.0.1:{proxy_port}/t/{tag}"
        ctx = {"repo": str(REPO), "state": str(state), "model": model, "prompt": ep["prompt"], "base_url": base}
        env = {k: v for k, v in os.environ.items() if k not in SECRET_ENV_VARS}
        for k, v in h.get("env", {}).items():
            env[k] = expand(v, ctx)
        if h.get("models_json"):  # Prime Agent: base URL override through its models.json
            (state / "models.json").write_text(json.dumps({"providers": {"anthropic": {"baseUrl": base}}}))
        cmd = expand(h["cmd"], ctx)
        answer = logs_dir / f"{tag}.stdout.txt"
        errlog = logs_dir / f"{tag}.stderr.txt"
        t0 = time.time()
        # Output goes to files and the episode runs in its own session, so one runaway
        # episode can neither fill the runner's memory nor outlive its limits.
        with open(answer, "w") as out_f, open(errlog, "w") as err_f:
            rc = run_watched(cmd, work, env, out_f, err_f, timeout)
        secs = time.time() - t0
        chk = subprocess.run([sys.executable, str(suite_dir / ep["check"]), str(suite_dir), str(answer)],
                             cwd=work, capture_output=True, text=True, timeout=300, env=script_env)
        return {"passed": chk.returncode == 0, "check_output": (chk.stdout + chk.stderr)[-1500:],
                "exit": rc, "seconds": round(secs, 1), "usage": usage.take(tag)}
    finally:
        shutil.rmtree(work, ignore_errors=True)


def run_suite(hname, h, suite_dir, spec, trial, model, port, usage, timeout, logs_dir, fresh_state=False):
    state = pathlib.Path(tempfile.mkdtemp(prefix=f"cmh-eval-state-{hname}-"))
    rows = []
    try:
        for number, ep in enumerate(spec["episodes"], start=1):
            if fresh_state:  # control: no memory carried between episodes
                shutil.rmtree(state, ignore_errors=True)
                state.mkdir()
            tag = f"{hname}.{suite_dir.name}.t{trial}.e{number}"
            r = run_episode(h, suite_dir, ep, number, state, model, port, usage, tag, timeout, logs_dir)
            r.update({"harness": hname, "suite": suite_dir.name, "track": spec["track"], "trial": trial,
                      "episode": number, "transfer": bool(ep.get("transfer"))})
            rows.append(r)
            print(f"[{hname}] {suite_dir.name} t{trial} e{number}: {'PASS' if r['passed'] else 'FAIL'} "
                  f"({r['seconds']}s, {r['usage']['requests']} req)", flush=True)
    finally:
        shutil.rmtree(state, ignore_errors=True)
    return rows

# ---------------------------------------------------------------- results

def read_rows(path):
    with open(path) as f:
        return [json.loads(line) for line in f]


def load_recorded(results_path):
    """Rows already in results.jsonl, one per (harness, suite, trial, episode); the first copy wins."""
    recorded = {}
    if results_path.exists():
        for r in read_rows(results_path):
            recorded.setdefault((r["harness"], r["suite"], r["trial"], r["episode"]), r)
    return recorded


def pending_jobs(names, specs, trials, recorded):
    """(harness, suite_dir, trial) runs that do not yet have every episode recorded."""
    def done(name, suite_dir, trial):
        episodes = range(1, len(specs[suite_dir]["episodes"]) + 1)
        return all((name, suite_dir.name, trial, e) in recorded for e in episodes)
    return [(n, s, t) for t in range(1, trials + 1) for s in specs for n in names if not done(n, s, t)]

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
        tokens = sum(r["usage"][field] for r in mine for field in TOKEN_FIELDS)
        table.append({"harness": n, "episodes": len(mine), "overall": rate(mine), "coding": rate(coding),
                      "learning_transfer": rate(learn),
                      "median_seconds": statistics.median_high(r["seconds"] for r in mine),
                      "total_tokens": tokens, "api_errors": sum(r["usage"]["errors"] for r in mine)})
    base = next((t for t in table if t["harness"] == baseline), None)
    for t in table:
        for k in ("overall", "coding", "learning_transfer"):
            t[f"{k}_vs_{baseline}"] = (t[k] / base[k]) if base and base[k] else None
        ratio = t[f"overall_vs_{baseline}"]  # None without a baseline or when it passed nothing
        t["meets_threshold"] = ratio is not None and ratio >= threshold
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
        rows = read_rows(a.report_only)
        table = summarize(rows, a.baseline, a.threshold)
        print(markdown(table, a.baseline, a.threshold, {"started": "(re-report)", "model": rows[0].get("model", "?"), "trials": "?"}))
        return

    started = datetime.datetime.now().strftime("%Y%m%d-%H%M%S")
    out = pathlib.Path(a.out or ROOT / "results" / started)
    results_path = out / "results.jsonl"
    if (out / "summary.md").exists():
        # A supervised service reruns its command when it exits; never append a second run.
        sys.exit(f"{out} already holds a finished run")
    # Resume an interrupted run: whole suites already recorded are not rerun.
    recorded = load_recorded(results_path)
    logs = out / "logs"
    logs.mkdir(parents=True, exist_ok=True)
    harnesses = json.loads((ROOT / "harnesses.json").read_text())
    names = [n for n in a.harness.split(",") if n]
    suites = sorted(p for p in (ROOT / "suites").iterdir() if (p / "suite.json").exists())
    if a.suite:
        wanted = set(a.suite.split(","))
        suites = [s for s in suites if s.name in wanted]
    specs = {s: json.loads((s / "suite.json").read_text()) for s in suites}
    usage = Usage()
    port = start_proxy(usage, out / "proxy-errors.jsonl", a.upstream)
    jobs = pending_jobs(names, specs, a.trials, recorded)
    suite_names = {s.name for s in specs}
    rows = [r for r in recorded.values() if r["harness"] in names and r["suite"] in suite_names]
    if recorded:
        print(f"resuming: {len(rows)} recorded episodes kept, {len(jobs)} suite runs left", flush=True)
        # A partly recorded suite reruns from its first episode and appends rows again, so
        # rewrite the file with one row per episode before appending to it.
        with open(results_path, "w") as f:
            f.writelines(json.dumps(r) + "\n" for r in recorded.values())
    with cf.ThreadPoolExecutor(a.jobs) as pool, open(results_path, "a") as f:
        futs = [pool.submit(run_suite, n, harnesses[n], s, specs[s], t, a.model, port, usage, a.timeout, logs,
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
