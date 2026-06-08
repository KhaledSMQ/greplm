#!/usr/bin/env python3
"""Token-efficiency benchmark for greplm context packs.

Measures how much context a task-driven `greplm pack` delivers versus the
grep+read-whole-files baseline an agent would otherwise pay.

Methodology (mirrors greplm's `savings` accounting):

  baseline = the agent reads, in full, every unique file the pack drew from.
             Cost = total characters of those files.
  pack     = the agent receives only the pack payload (per item: a bounded
             code snippet + signature) — the bytes it actually consumes.
  tokens   = characters / 4   (the same 4-chars-per-token estimate)
  saved    = baseline_tokens - pack_tokens
  pct      = saved / baseline_tokens

Usage:
  # On greplm itself (build release first, or set GREPLM_BIN):
  python3 pack_bench.py --root ../.. --budget 8000 \
      --tasks-inline "how does incremental indexing work" \
                     "how are call graph edges resolved" \
                     "structural AST search with tree-sitter queries"

  # From a JSON file of tasks ({"tasks": [{"id":..., "task":...}, ...]}):
  python3 pack_bench.py --root <repo> --tasks tasks.json --budget 8000
"""
import argparse
import json
import os
import shutil
import subprocess
import sys

# bench/context/pack_bench.py -> repo root is two levels up.
REPO_ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
CHARS_PER_TOKEN = 4


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


GREPLM = resolve_greplm()


def norm(p):
    p = p.strip()
    while p.startswith("./"):
        p = p[2:]
    return p.lstrip("/")


def file_chars(root, rel):
    """Full character count of a file (the grep+read baseline cost)."""
    try:
        with open(os.path.join(root, rel), "r", encoding="utf-8", errors="replace") as fh:
            return len(fh.read())
    except OSError:
        return 0


def run_pack(task, root, budget):
    cmd = [GREPLM, "pack", task, "--no-daemon", "-C", root,
           "--budget", str(budget), "--json"]
    out = subprocess.run(cmd, capture_output=True, text=True)
    if out.returncode != 0:
        sys.stderr.write(f"[greplm pack] {task!r} failed: {out.stderr[:200]}\n")
        return None
    return json.loads(out.stdout or "{}")


def item_chars(item):
    """Characters the agent consumes for one pack item: snippet + signature."""
    chars = sum(len(l.get("text", "")) + 1 for l in item.get("snippet", []))
    chars += len(item.get("signature") or "")
    return chars


def human(n):
    for unit in ("", "k", "M"):
        if abs(n) < 1000:
            return f"{n:.0f}{unit}" if unit == "" else f"{n:.1f}{unit}"
        n /= 1000
    return f"{n:.1f}G"


def load_tasks(args):
    if args.tasks_inline:
        return [{"id": t, "task": t} for t in args.tasks_inline]
    data = json.load(open(args.tasks))
    return data["tasks"]


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--root", default=REPO_ROOT,
                    help="repo to pack from (default: the greplm repo itself)")
    ap.add_argument("--budget", type=int, default=8000)
    ap.add_argument("--tasks", default=os.path.join(os.path.dirname(__file__), "tasks.json"))
    ap.add_argument("--tasks-inline", nargs="*",
                    help="Use these task strings directly instead of a JSON file.")
    ap.add_argument("--out", help="optional path to write a JSON results file")
    args = ap.parse_args()

    root = os.path.abspath(os.path.expanduser(args.root))
    tasks = load_tasks(args)

    print(f"root={root}  budget={args.budget}  n={len(tasks)} tasks\n")
    hdr = f"{'task':<40} {'items':>5} {'baseline':>9} {'pack':>7} {'saved':>7}"
    print(hdr)
    print("-" * len(hdr))

    agg_base = agg_pack = 0.0
    rows = []
    for t in tasks:
        pack = run_pack(t["task"], root, args.budget)
        if not pack:
            continue
        items = pack.get("items", [])
        files = {norm(i["path"]) for i in items if i.get("path")}
        baseline = sum(file_chars(root, f) for f in files) / CHARS_PER_TOKEN
        returned = sum(item_chars(i) for i in items) / CHARS_PER_TOKEN
        agg_base += baseline
        agg_pack += returned
        pct = (1 - returned / baseline) * 100 if baseline else 0.0
        rows.append({
            "id": t["id"], "task": t["task"], "items": len(items),
            "files": len(files), "baseline_tokens": baseline,
            "pack_tokens": returned, "saved_pct": pct,
        })
        print(f"{str(t['id'])[:40]:<40} {len(items):>5} "
              f"{human(baseline):>9} {human(returned):>7} {pct:>6.1f}%")

    print("\n=== CONTEXT-PACK TOKEN EFFICIENCY (tokens ~= chars/4) ===")
    saved = agg_base - agg_pack
    pct = (saved / agg_base * 100) if agg_base else 0.0
    print(f"baseline={human(agg_base)} tokens  pack={human(agg_pack)} tokens  "
          f"saved={human(saved)} ({pct:.1f}% fewer)")
    print("baseline = read whole files the pack drew from; pack = snippets+signatures returned.")

    if args.out:
        with open(args.out, "w") as fh:
            json.dump({
                "root": root, "budget": args.budget, "rows": rows,
                "summary": {
                    "baseline_tokens": agg_base, "pack_tokens": agg_pack,
                    "saved_pct": pct,
                },
            }, fh, indent=2)
        print(f"\nwrote {args.out}")


if __name__ == "__main__":
    main()
