#!/usr/bin/env python3
"""Multi-project greplm benchmark orchestrator.

Runs the two reproducible benchmarks (content-search + context-pack) against a
set of real, large codebases defined in projects.json, then writes a single
"selling" report (RESULTS.md + results.json) with headline token-savings,
recall, and latency numbers per project and in aggregate.

For each project it shells out to the existing, self-contained benchmark
scripts so the methodology is identical to `bench/run_bench.py`:

  baseline  = ripgrep finds the matching files, the agent reads each in full
  greplm    = `greplm search` / `greplm pack`, agent consumes only the payload
  tokens   ~= characters / 4   (applied identically to both engines)

On huge repos a single identifier can match thousands of files, so search runs
with --max-per-file 1 (one hit per file) and a high --k: greplm surfaces the
same *files* ripgrep does (recall stays meaningful) without a hit explosion.

Usage:
  cargo build --release            # or set $GREPLM_BIN
  # index each project once (the script can do it for you):
  python3 bench/projects/run_all.py --index
  # or, if already indexed, just run the benchmarks:
  python3 bench/projects/run_all.py
  # a single project:
  python3 bench/projects/run_all.py --only linux
"""
import argparse
import json
import os
import shutil
import subprocess
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
BENCH = os.path.dirname(HERE)
REPO_ROOT = os.path.dirname(BENCH)
RUN_BENCH = os.path.join(BENCH, "run_bench.py")
PACK_BENCH = os.path.join(BENCH, "context", "pack_bench.py")


def resolve_greplm():
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
    sys.exit("could not find greplm; build with `cargo build --release` or set $GREPLM_BIN")


def expand(root):
    return os.path.abspath(os.path.expanduser(root))


def human(n):
    for unit in ("", "k", "M"):
        if abs(n) < 1000:
            return f"{n:.0f}{unit}" if unit == "" else f"{n:.1f}{unit}"
        n /= 1000
    return f"{n:.1f}G"


def run_index(greplm, root):
    print(f"  indexing {root} ...", flush=True)
    t = time.perf_counter()
    out = subprocess.run([greplm, "index", "--force", "-C", root],
                         capture_output=True, text=True)
    dt = time.perf_counter() - t
    sys.stdout.write("    " + (out.stdout.strip() or out.stderr.strip()) + "\n")
    return dt


def run_search_bench(greplm, root, queries, k, max_per_file, out_path):
    cmd = [sys.executable, RUN_BENCH, "--root", root, "--queries", queries,
           "--k", str(k), "--max-per-file", str(max_per_file),
           "--fixed-strings", "--out", out_path]
    env = dict(os.environ, GREPLM_BIN=greplm)
    subprocess.run(cmd, env=env, check=True)
    return json.load(open(out_path))


def warm_latency(greplm, root, queries, k, max_per_file, repeats=3):
    """Median per-query latency against a warm in-process daemon.

    This is the real agent-loop scenario: the index is built once and stays
    hot, so every query is a socket round-trip instead of a fresh tree scan.
    Starts a dedicated `greplm serve` for `root`, warms it, then times queries.
    """
    qs = [q["query"] for q in json.load(open(queries))["queries"]]
    daemon = subprocess.Popen([greplm, "serve", "-C", root],
                              stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        # give the daemon time to bind its socket and load the index
        def one(q):
            out = subprocess.run(
                [greplm, "search", q, "-C", root, "--limit", str(k),
                 "--max-per-file", str(max_per_file), "--json"],
                capture_output=True, text=True)
            return out.returncode == 0
        deadline = time.time() + 30
        while time.time() < deadline and not one(qs[0]):
            time.sleep(0.5)
        for q in qs:  # warm the cache for every query once
            one(q)
        samples = []
        for q in qs:
            best = None
            for _ in range(repeats):
                t = time.perf_counter()
                one(q)
                dt = (time.perf_counter() - t) * 1000
                best = dt if best is None else min(best, dt)
            samples.append(best)
        samples.sort()
        n = len(samples)
        median = samples[n // 2] if n % 2 else (samples[n // 2 - 1] + samples[n // 2]) / 2
        return {"median_ms": median, "min_ms": min(samples), "max_ms": max(samples)}
    finally:
        daemon.terminate()
        try:
            daemon.wait(timeout=5)
        except subprocess.TimeoutExpired:
            daemon.kill()


def run_pack_bench(greplm, root, tasks, budget, out_path):
    cmd = [sys.executable, PACK_BENCH, "--root", root, "--tasks", tasks,
           "--budget", str(budget), "--out", out_path]
    env = dict(os.environ, GREPLM_BIN=greplm)
    subprocess.run(cmd, env=env, check=True)
    return json.load(open(out_path))


def md_table(headers, rows):
    out = ["| " + " | ".join(headers) + " |",
           "|" + "|".join("---" for _ in headers) + "|"]
    for r in rows:
        out.append("| " + " | ".join(str(c) for c in r) + " |")
    return "\n".join(out)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--config", default=os.path.join(HERE, "projects.json"))
    ap.add_argument("--only", help="run a single project by name")
    ap.add_argument("--index", action="store_true",
                    help="(re)build each project's index with --force first")
    ap.add_argument("--no-warm", action="store_true",
                    help="skip the warm-daemon latency probe")
    ap.add_argument("--out-dir", default=os.path.join(HERE, "out"))
    args = ap.parse_args()

    if not shutil.which("rg"):
        sys.exit("ripgrep (`rg`) is required for the baseline; install it and retry.")

    greplm = resolve_greplm()
    cfg = json.load(open(args.config))
    defaults = cfg.get("defaults", {})
    projects = cfg["projects"]
    if args.only:
        projects = [p for p in projects if p["name"] == args.only]
        if not projects:
            sys.exit(f"no project named {args.only!r}")
    os.makedirs(args.out_dir, exist_ok=True)

    print(f"greplm  : {greplm}")
    print(f"projects: {', '.join(p['name'] for p in projects)}\n")

    results = []
    for p in projects:
        root = expand(p["root"])
        if not os.path.isdir(root):
            print(f"!! skipping {p['name']}: {root} not found\n")
            continue
        k = p.get("k", defaults.get("k", 20000))
        mpf = p.get("max_per_file", defaults.get("max_per_file", 1))
        budget = p.get("budget", defaults.get("budget", 8000))

        print(f"========== {p['label']} ({root}) ==========")
        index = dict(p.get("index", {}))
        if args.index:
            index["seconds"] = round(run_index(greplm, root), 2)

        search = run_search_bench(
            greplm, root, os.path.join(HERE, p["queries"]), k, mpf,
            os.path.join(args.out_dir, f"{p['name']}.search.json"))
        print()
        pack = run_pack_bench(
            greplm, root, os.path.join(HERE, p["tasks"]), budget,
            os.path.join(args.out_dir, f"{p['name']}.pack.json"))
        print()

        warm = None
        if not args.no_warm:
            warm = warm_latency(greplm, root, os.path.join(HERE, p["queries"]), k, mpf)
            print(f"  warm daemon latency: median {warm['median_ms']:.1f} ms "
                  f"(min {warm['min_ms']:.1f}, max {warm['max_ms']:.1f})\n")

        results.append({
            "name": p["name"], "label": p["label"],
            "language": p.get("language", ""), "root": root,
            "index": index, "search": search, "pack": pack, "warm": warm,
        })

    # ---- combined report ----------------------------------------------------
    report = ["# greplm — real-world benchmarks",
              "",
              "Token cost of finding code two ways, on three large real codebases:",
              "",
              "- **baseline** — ripgrep finds the matching files, the agent reads each "
              "file *in full* (what an agent does today with grep + read).",
              "- **greplm** — `greplm search` / `greplm pack`; the agent consumes only "
              "the compact payload (matched lines / budgeted snippets).",
              "",
              "`tokens ~= characters / 4`, applied identically to both engines. "
              "ripgrep runs in literal mode (`-F`) so both engines look for the "
              "same string. `recall` = fraction of ripgrep's files greplm also "
              "surfaced (search runs `--max-per-file 1`, so greplm returns the "
              "same files without a hit explosion). Latency in the headline is the "
              "warm daemon — index built once, then queried hot, which is how an "
              "agent actually uses it.",
              ""]

    # headline table
    hl_rows = []
    tot_sb = tot_sg = tot_pb = tot_pg = 0.0
    for r in results:
        s, pk, idx = r["search"]["summary"], r["pack"]["summary"], r["index"]
        tot_sb += s["baseline_tokens"]; tot_sg += s["greplm_tokens"]
        tot_pb += pk["baseline_tokens"]; tot_pg += pk["pack_tokens"]
        warm_ms = f"{r['warm']['median_ms']:.1f}" if r.get("warm") else "—"
        hl_rows.append([
            r["label"], r["language"],
            f"{idx.get('files', '?'):,}".replace(",", " ") if isinstance(idx.get("files"), int) else "?",
            f"{idx.get('seconds', '?')}s",
            f"{s['saved_pct']:.1f}%", f"{s['mean_recall']*100:.0f}%",
            f"{pk['saved_pct']:.1f}%",
            warm_ms, f"{s['rg_ms']:.0f}",
        ])
    report.append("## Headline")
    report.append("")
    report.append(md_table(
        ["Project", "Lang", "Files", "Index once", "Search saved",
         "Recall", "Pack saved", "Warm query ms", "ripgrep ms"], hl_rows))
    report.append("")
    report.append("*Warm query ms = median per-query latency against the always-on "
                  "daemon (the real agent-loop scenario: index built once, stays hot). "
                  "ripgrep ms re-scans the whole tree on every single query.*")
    report.append("")
    s_pct = (1 - tot_sg / tot_sb) * 100 if tot_sb else 0
    p_pct = (1 - tot_pg / tot_pb) * 100 if tot_pb else 0
    report.append(f"**Aggregate** — content search: {human(tot_sb)} → {human(tot_sg)} "
                  f"tokens (**{s_pct:.1f}% fewer**). context packs: {human(tot_pb)} → "
                  f"{human(tot_pg)} tokens (**{p_pct:.1f}% fewer**).")
    report.append("")

    # per-project detail
    for r in results:
        report.append(f"## {r['label']}")
        report.append("")
        idx = r["index"]
        if idx:
            report.append(
                f"Index: {idx.get('files','?'):,} files · "
                f"{idx.get('symbols','?'):,} symbols · built in "
                f"{idx.get('seconds','?')}s · {idx.get('size','?')} on disk."
                if isinstance(idx.get("files"), int) else "")
            report.append("")
        # search detail
        report.append("**Content search** (`greplm search` vs ripgrep + read whole files)")
        report.append("")
        srows = []
        for row in r["search"]["rows"]:
            srows.append([
                f"`{row['query']}`", row["rg_files"],
                f"{row['recall']*100:.0f}%",
                human(row["baseline_tokens"]), human(row["greplm_tokens"]),
                f"{row['saved_pct']:.1f}%",
            ])
        report.append(md_table(
            ["query", "files", "recall", "baseline", "greplm", "saved"], srows))
        report.append("")
        # pack detail
        report.append("**Context packs** (`greplm pack` vs read every file the pack drew from)")
        report.append("")
        prows = []
        for row in r["pack"]["rows"]:
            prows.append([
                row["id"], row["items"], row["files"],
                human(row["baseline_tokens"]), human(row["pack_tokens"]),
                f"{row['saved_pct']:.1f}%",
            ])
        report.append(md_table(
            ["task", "items", "files", "baseline", "pack", "saved"], prows))
        report.append("")

    report_path = os.path.join(HERE, "RESULTS.md")
    with open(report_path, "w") as fh:
        fh.write("\n".join(report) + "\n")
    with open(os.path.join(HERE, "results.json"), "w") as fh:
        json.dump(results, fh, indent=2)

    print("=" * 70)
    print(f"content search : {human(tot_sb)} -> {human(tot_sg)} tokens  ({s_pct:.1f}% fewer)")
    print(f"context packs  : {human(tot_pb)} -> {human(tot_pg)} tokens  ({p_pct:.1f}% fewer)")
    print(f"wrote {report_path}")
    print(f"wrote {os.path.join(HERE, 'results.json')}")


if __name__ == "__main__":
    main()
