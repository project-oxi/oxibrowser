#!/usr/bin/env python3
"""Fresh-process SPA acceptance launcher.

Per suite repetition: start a fresh OxiBrowser `serve` process (own process group,
per-run logs), wait for /json/version readiness, then run runner.mjs once (--runs 1)
against it. Truthful results only — no retries, no masking — and only children this
launcher spawned are ever signalled. Exit 0 only when every run passed (and the
optional --chrome reference passed first); 1 on any failure; 2 on bad arguments.
"""

import argparse, hashlib, json, os, platform, shutil, signal, socket, statistics, subprocess, sys, time
from datetime import datetime, timezone
from pathlib import Path
from urllib.request import urlopen

RUNNER = Path(__file__).resolve().parent / "runner.mjs"
SERVER_ARGS = ("--no-audit", "serve", "--allow-private-ips")
READINESS_S, SUITE_TIMEOUT_S, KILL_GRACE_S = 20.0, 15 * 60, 10.0

def utcnow(): return datetime.now(timezone.utc).isoformat(timespec="seconds")

def positive_int(text):
    try: value = int(text)
    except ValueError: raise argparse.ArgumentTypeError(f"not an integer: {text!r}")
    if value < 1: raise argparse.ArgumentTypeError(f"must be a positive integer: {text!r}")
    return value

def sha256_file(path):
    digest = hashlib.sha256()
    with open(path, "rb") as fh:
        for chunk in iter(lambda: fh.read(1 << 20), b""): digest.update(chunk)
    return digest.hexdigest()

def reserve_port():
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]

def spawn_logged(cmd, run_dir, name):
    """New session (own process group) so we can later stop exactly this child."""
    out = open(run_dir / f"{name}-stdout.log", "wb")
    err = open(run_dir / f"{name}-stderr.log", "wb")
    try: return subprocess.Popen(cmd, stdout=out, stderr=err, start_new_session=True)
    finally: out.close(); err.close()

def stop_owned(proc):
    """SIGTERM the group of a child WE spawned, wait, then SIGKILL. Nothing else."""
    if proc.poll() is not None: return
    for sig in (signal.SIGTERM, signal.SIGKILL):
        try: os.killpg(os.getpgid(proc.pid), sig)
        except OSError: proc.send_signal(sig)
        try: return proc.wait(timeout=KILL_GRACE_S)
        except subprocess.TimeoutExpired: continue

def wait_ready(port, proc):
    url = f"http://127.0.0.1:{port}/json/version"
    deadline = time.monotonic() + READINESS_S
    while time.monotonic() < deadline:
        if proc.poll() is not None: return False, f"server exited during startup (code {proc.returncode})"
        try:
            with urlopen(url, timeout=2) as resp:
                if resp.status == 200: return True, None
        except Exception: pass
        time.sleep(0.25)
    return False, f"no 200 from {url} within {READINESS_S:.0f}s"

def run_suite(cmd, suite_dir):
    """One runner.mjs invocation -> (status, exit_code, parsed report | None)."""
    suite_dir.mkdir(parents=True, exist_ok=True)
    (suite_dir / "report.json").unlink(missing_ok=True)
    proc = spawn_logged(cmd, suite_dir, "runner")
    try: rc = proc.wait(timeout=SUITE_TIMEOUT_S)
    except subprocess.TimeoutExpired: stop_owned(proc); return "timeout", None, None
    except KeyboardInterrupt: stop_owned(proc); raise
    report, path = None, suite_dir / "report.json"
    if path.exists():
        try: report = json.loads(path.read_text())
        except (OSError, ValueError): report = None
    return ("pass" if rc == 0 and report is not None else "fail"), rc, report

def summarize_target(rec, status, rc, report, kind):
    """PASS only when the runner exited 0, its report parsed, and the verdict is PASS."""
    target = next((t for t in (report or {}).get("targets", []) if t.get("kind") == kind), None)
    cases = [c for run in (target or {}).get("runs", []) for c in run.get("cases", [])]
    verdict = (target or {}).get("summary", {}).get("verdict")
    rec["exitCode"] = rc
    if status == "timeout":
        rec["status"], rec["error"] = "timeout", f"runner exceeded {SUITE_TIMEOUT_S}s"
    elif status == "pass" and verdict == "PASS":
        rec["status"] = "pass"
    else:
        why = (("runner report.json missing or invalid" if report is None else f"runner exited {rc}")
               if status != "pass" else (f"verdict {verdict}" if verdict else f"report.json has no {kind} target"))
        first = next((c["errors"][0] for c in cases if c.get("status") != "pass" and c.get("errors")), None)
        rec["status"], rec["error"] = "fail", why + (f": {first}" if first else "")
    if target is not None:
        rec.update(verdict=target["summary"]["verdict"], caseRunsPassed=target["summary"]["caseRunsPassed"],
                   caseRunsFailed=target["summary"]["caseRunsFailed"])
    if cases:
        rec["cases"] = [{"name": c.get("name"), "status": c.get("status"), "ms": c.get("ms")} for c in cases]
    return rec

def run_reference(node, chrome, out_root):
    ref_dir = out_root / "reference"
    status, rc, report = run_suite(
        [node, str(RUNNER), "--chrome", str(chrome), "--runs", "1", "--out", str(ref_dir)], ref_dir)
    return summarize_target({"target": "chrome-reference", "dir": ref_dir.name}, status, rc, report, "chrome")

def run_oxi_once(index, node, binary, out_root):
    run_dir = out_root / f"run-{index:02d}"
    run_dir.mkdir(parents=True, exist_ok=True)
    port = reserve_port()
    rec = {"run": index, "port": port, "dir": run_dir.name, "startedAt": utcnow()}
    launched_at = time.monotonic()
    server = spawn_logged([str(binary), *SERVER_ARGS, "--port", str(port)], run_dir, "server")
    try:
        ok, why = wait_ready(port, server)
        rec["observedReadyMs"] = round((time.monotonic() - launched_at) * 1000)
        rec["readinessPollMs"] = 250
        if not ok:
            rec["status"], rec["error"] = "startup-fail", why
            return rec
        started = time.monotonic()
        status, rc, report = run_suite(
            [node, str(RUNNER), "--endpoint", f"http://127.0.0.1:{port}", "--runs", "1", "--out", str(run_dir)],
            run_dir)
        summarize_target(rec, status, rc, report, "endpoint")
        rec["suiteS"] = round(time.monotonic() - started, 1)
        return rec
    finally:
        stop_owned(server)  # fresh server per repetition; never touches other processes
        rec["serverExitCode"] = server.returncode
        rec["endedAt"] = utcnow()

def aggregate(records):
    statuses, per_case = {}, {}
    for rec in records:
        statuses[rec["status"]] = statuses.get(rec["status"], 0) + 1
        for case in rec.get("cases", []):
            e = per_case.setdefault(case["name"], {"name": case["name"], "pass": 0, "fail": 0, "okMs": []})
            if case["status"] == "pass":
                e["pass"] += 1
                if isinstance(case.get("ms"), (int, float)): e["okMs"].append(case["ms"])
            else: e["fail"] += 1
    perCase = [{"name": e["name"], "pass": e["pass"], "fail": e["fail"],
                "samples": len(e["okMs"]),
                "medianMs": round(statistics.median(e["okMs"])) if e["okMs"] else None,
                "p95Ms": round(statistics.quantiles(e["okMs"], n=20, method="inclusive")[18]) if len(e["okMs"]) > 1 else (e["okMs"][0] if e["okMs"] else None)} for e in per_case.values()]
    return statuses, perCase

def main():
    parser = argparse.ArgumentParser(
        description="Fresh-process SPA acceptance launcher: one new OxiBrowser serve process per suite repetition.")
    parser.add_argument("--binary", required=True, help="OxiBrowser executable (absolute or relative)")
    parser.add_argument("--runs", type=positive_int, default=10, help="suite repetitions (default 10)")
    parser.add_argument("--out", required=True, help="output directory (per-run dirs + summary.json)")
    parser.add_argument("--chrome", help="reference Chrome; run once first — must pass for any PASS verdict")
    args = parser.parse_args()
    binary = Path(args.binary).expanduser().resolve()
    if not binary.is_file() or not os.access(binary, os.X_OK): parser.error(f"--binary is not an executable file: {binary}")
    node = shutil.which("node")
    if node is None: parser.error("node not found on PATH")
    node_version = subprocess.run([node, "--version"], capture_output=True, text=True).stdout.strip()
    out_root = Path(args.out).expanduser().resolve(); out_root.mkdir(parents=True, exist_ok=True)
    binary_sha_start = sha256_file(binary)
    reference = None
    if args.chrome:
        chrome = Path(args.chrome).expanduser().resolve()
        if not chrome.is_file() or not os.access(chrome, os.X_OK): parser.error(f"--chrome is not an executable file: {chrome}")
        print(f"[reference] running Chrome reference suite once: {chrome}", flush=True)
        reference = run_reference(node, chrome, out_root)
        print(f"[reference] status={reference['status']}", flush=True)
    records = []
    candidate_runs = args.runs if reference is None or reference["status"] == "pass" else 0
    for index in range(1, candidate_runs + 1):
        print(f"[run {index}/{args.runs}] starting fresh server process", flush=True)
        rec = run_oxi_once(index, node, binary, out_root); records.append(rec)
        print(f"[run {index}/{args.runs}] status={rec['status']}"
              + (f" verdict={rec['verdict']}" if "verdict" in rec else ""), flush=True)
    statuses, perCase = aggregate(records)
    passed = statuses.get("pass", 0)
    binary_unchanged = sha256_file(binary) == binary_sha_start
    verdict = "PASS" if binary_unchanged and passed == args.runs and (reference is None or reference["status"] == "pass") else "FAIL"
    summary = {
        "generatedAt": utcnow(), "verdict": verdict, "binaryUnchanged": binary_unchanged, "runsRequested": args.runs, "runsPassed": passed,
        "statuses": statuses, "perCase": perCase, "reference": reference, "runs": records,
        "environment": {"binary": str(binary), "binarySha256": binary_sha_start, "os": platform.platform(),
                        "machine": platform.machine(), "python": platform.python_version(), "node": node_version},
    }
    path = out_root / "summary.json"; path.write_text(json.dumps(summary, indent=2) + "\n")
    print(f"verdict: {verdict} — {passed}/{args.runs} runs passed"
          + (f", reference={reference['status']}" if reference else "") + f"\nsummary: {path}")
    sys.exit(0 if verdict == "PASS" else 1)

if __name__ == "__main__":
    def terminate_owned_run(_signal, _frame):
        raise KeyboardInterrupt
    signal.signal(signal.SIGTERM, terminate_owned_run)
    try: main()
    except KeyboardInterrupt: sys.exit(130)
