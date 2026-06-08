#!/usr/bin/env python3
"""Reproducible benchmark for greplm — runs against this repository itself.

It needs nothing beyond a release build of greplm and `ripgrep` (`rg`) on PATH:
no external corpus, no embedding model, no third-party search engine. That makes
the numbers something anyone can reproduce from a clean checkout.

What it measures
----------------
For each query (a literal identifier a coding agent would realistically grep
for, see queries.json) we compare two ways an agent could find the code:

  baseline (grep + read)
      The agent runs `rg` to find which files match, then reads each matching
      file *in full* to get the context. Cost = total characters of those files.

  greplm
      The agent runs `greplm search` and consumes only the compact result
      payload — the matched line text per hit. Cost = characters of that payload.

  tokens ~= characters / 4   (a common rough estimate)
  saved  = baseline_tokens - greplm_tokens
  pct    = saved / baseline_tokens

We also report:
  recall      fraction of the files ripgrep found that greplm also surfaced
              (a correctness sanity check — greplm should not miss matches)
  latency     wall-clock time for each engine's query (greplm runs cold, with
              --no-daemon; the warm daemon is faster still)

Usage
-----
  # Build the binary first (or set GREPLM_BIN):
  cargo build --release
  python3 bench/run_bench.py

  # Options:
  python3 bench/run_bench.py --root /path/to/repo --k 500 \
      --queries bench/queries.json --out bench/results.json
"""
import argparse
import json
import os
import shutil
import subprocess
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
REPO_ROOT = os.path.dirname(HERE)
CHARS_PER_TOKEN = 4
# Exclude build/index dirs and the bench dir itself: a query like "SegmentWriter"
# should be judged on the source, not on the queries.json that defines it.
EXCLUDES = ["target", ".greplm", ".git", "node_modules", "bench"]


def resolve_greplm():
    """Find the greplm binary without assuming any machine-specific path."""
    env = os.environ.get("GREPLM_BIN")
    if env and os.path.exists(env):
        return env
    for rel in ("target/release/greplm", "target/debug/greplm"):
        cand = os.path.join(REPO_ROOT, rel)
        if os.path.exists(cand):
            return cand
    found = shutil.which("greplm")
    if found:
        return found
    sys.exit(
        "could not find the greplm binary.\n"
        "  build it with `cargo build --release`, or set $GREPLM_BIN to its path."
    )


def norm(p):
    p = p.strip()
    while p.startswith("./"):
        p = p[2:]
    return p.lstrip("/")


def file_chars(root, rel):
    try:
        with open(os.path.join(root, rel), "r", encoding="utf-8", errors="replace") as fh:
            return len(fh.read())
    except OSError:
        return 0


def run_ripgrep(query, root):
    """Files matching `query` (ground truth) plus how long the search took."""
    cmd = ["rg", "--files-with-matches", "--no-messages"]
    for ex in EXCLUDES:
        cmd += ["--glob", f"!{ex}/**"]
    cmd += ["--", query, "."]
    t = time.perf_counter()
    out = subprocess.run(cmd, capture_output=True, text=True, cwd=root)
    dt = time.perf_counter() - t
    # rg exits 1 when there are simply no matches; that is not an error here.
    if out.returncode not in (0, 1):
        sys.stderr.write(f"[rg] {query!r} failed: {out.stderr[:200]}\n")
    files = {norm(l) for l in out.stdout.splitlines() if l.strip()}
    return files, dt


def run_greplm(greplm, query, root, k):
    """greplm's hits (compact payload) plus how long the cold search took."""
    cmd = [greplm, "search", query, "--no-daemon", "-C", root,
           "--limit", str(k), "--json"]
    t = time.perf_counter()
    out = subprocess.run(cmd, capture_output=True, text=True)
    dt = time.perf_counter() - t
    if out.returncode != 0:
        sys.stderr.write(f"[greplm] {query!r} failed: {out.stderr[:200]}\n")
        return set(), 0, dt
    data = json.loads(out.stdout or "[]")
    # Mirror ripgrep's excludes so the two engines are compared on the same set.
    data = [h for h in data
            if not any(norm(h["path"]).startswith(f"{ex}/") for ex in EXCLUDES)]
    files = {norm(h["path"]) for h in data}
    # Characters the agent actually consumes: the matched line text per hit.
    returned_chars = sum(len(h.get("text", "")) + 1 for h in data)
    return files, returned_chars, dt


def human(n):
    for unit in ("", "k", "M"):
        if abs(n) < 1000:
            return f"{n:.0f}{unit}" if unit == "" else f"{n:.1f}{unit}"
        n /= 1000
    return f"{n:.1f}G"


def main():
    ap = argparse.ArgumentParser(description="Reproducible greplm benchmark.")
    ap.add_argument("--root", default=REPO_ROOT,
                    help="repo to search (default: the greplm repo itself)")
    ap.add_argument("--k", type=int, default=500,
                    help="max greplm hits per query (default: 500)")
    ap.add_argument("--queries", default=os.path.join(HERE, "queries.json"))
    ap.add_argument("--out", help="optional path to write a JSON results file")
    args = ap.parse_args()

    if not shutil.which("rg"):
        sys.exit("ripgrep (`rg`) is required for the baseline; install it and retry.")

    greplm = resolve_greplm()
    root = os.path.abspath(os.path.expanduser(args.root))
    queries = json.load(open(args.queries))["queries"]

    print(f"greplm : {greplm}")
    print(f"root   : {root}")
    print(f"queries: {len(queries)}  (k={args.k})\n")

    hdr = (f"{'query':<18} {'rg files':>8} {'gl files':>8} {'recall':>7} "
           f"{'baseline':>9} {'greplm':>8} {'saved':>7} "
           f"{'rg ms':>7} {'gl ms':>7}")
    print(hdr)
    print("-" * len(hdr))

    rows = []
    tot_base = tot_ret = 0.0
    recalls, gl_lats, rg_lats = [], [], []
    for q in queries:
        rg_files, rg_dt = run_ripgrep(q["query"], root)
        gl_files, gl_chars, gl_dt = run_greplm(greplm, q["query"], root, args.k)

        base_tokens = sum(file_chars(root, f) for f in rg_files) / CHARS_PER_TOKEN
        ret_tokens = gl_chars / CHARS_PER_TOKEN
        saved_pct = (1 - ret_tokens / base_tokens) * 100 if base_tokens else 0.0
        recall = len(rg_files & gl_files) / len(rg_files) if rg_files else 1.0

        tot_base += base_tokens
        tot_ret += ret_tokens
        recalls.append(recall)
        gl_lats.append(gl_dt)
        rg_lats.append(rg_dt)
        rows.append({
            "id": q["id"], "query": q["query"],
            "rg_files": len(rg_files), "greplm_files": len(gl_files),
            "recall": recall, "baseline_tokens": base_tokens,
            "greplm_tokens": ret_tokens, "saved_pct": saved_pct,
            "rg_ms": rg_dt * 1000, "greplm_ms": gl_dt * 1000,
        })
        print(f"{q['id'][:18]:<18} {len(rg_files):>8} {len(gl_files):>8} "
              f"{recall:>6.0%} {human(base_tokens):>9} {human(ret_tokens):>8} "
              f"{saved_pct:>6.1f}% {rg_dt * 1000:>6.1f} {gl_dt * 1000:>6.1f}")

    def mean(xs):
        return sum(xs) / len(xs) if xs else 0.0

    saved = tot_base - tot_ret
    pct = (saved / tot_base * 100) if tot_base else 0.0
    print("\n=== SUMMARY (tokens ~= chars/4; baseline = ripgrep + read whole files) ===")
    print(f"baseline : {human(tot_base)} tokens")
    print(f"greplm   : {human(tot_ret)} tokens  ->  {pct:.1f}% fewer ({human(saved)} saved)")
    print(f"recall   : {mean(recalls):.0%} of ripgrep's files surfaced by greplm")
    print(f"latency  : greplm {mean(gl_lats) * 1000:.1f} ms cold  |  ripgrep {mean(rg_lats) * 1000:.1f} ms")

    if args.out:
        with open(args.out, "w") as fh:
            json.dump({
                "greplm": greplm, "root": root, "k": args.k,
                "rows": rows,
                "summary": {
                    "baseline_tokens": tot_base, "greplm_tokens": tot_ret,
                    "saved_pct": pct, "mean_recall": mean(recalls),
                    "greplm_ms": mean(gl_lats) * 1000, "rg_ms": mean(rg_lats) * 1000,
                },
            }, fh, indent=2)
        print(f"\nwrote {args.out}")


if __name__ == "__main__":
    main()
