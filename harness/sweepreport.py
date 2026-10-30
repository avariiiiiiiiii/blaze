#!/usr/bin/env python3
"""Summary of a crate sweep: sweepreport.py <dir of cratetest --json files> [--md].

One line per crate (passed, tests, share of functions blaze compiled, the tests that failed), then
the reasons functions were left to rustc, most common first, over all crates. `--md` prints the crates as a markdown
table, with the time blaze spent compiling each.
"""
import collections, glob, json, os, sys

def main():
    reasons, where = collections.Counter(), collections.defaultdict(set)
    compiled = roots = tests = 0
    markdown = "--md" in sys.argv
    if markdown:
        print("| crate | result | test binaries | tests | functions from blaze | codegen ms | left to rustc, or failing |")
        print("|---|---|---|---|---|---|---|")
    for path in sorted(glob.glob(os.path.join(sys.argv[1], "*.json"))):
        crate = json.load(open(path))
        done = total = 0
        took, own = 0.0, collections.Counter()
        seen = set()
        for binary in crate.get("binaries", []):
            for name, stats in binary["crates"].items():
                # A crate's objects are shared by its test binaries: count them once.
                if (name, stats["roots"], stats["compiled"]) in seen:
                    continue
                seen.add((name, stats["roots"], stats["compiled"]))
                done, total = done + stats["compiled"], total + stats["roots"]
                took += stats["codegen_ms"]
                for reason, n in stats["skipped"].items():
                    reasons[reason] += n
                    own[reason] += n
                    where[reason].add(crate["fixture"])
        failing = [t for b in crate.get("binaries", []) for t in b.get("failed", [])]
        errors = [b["error"].splitlines()[0][:80] for b in crate.get("binaries", []) if "error" in b]
        if markdown:
            notes = [f"{n} {reason[:60]}" for reason, n in own.most_common(3)]
            notes += [f"fails: {' '.join(failing[:6])}"] if failing else []
            notes += [f"error: {' '.join(errors[:2])}"] if errors else []
            result = "untested" if crate.get("untested") else "pass" if crate["passed"] else "FAIL"
            print(f"| {crate['fixture']} | {result} | {len(crate.get('binaries', []))} | {crate['tests_run']} "
                  f"| {done}/{total} | {took:.0f} | {'; '.join(notes).replace('|', '/')} |")
        else:
            mark = "none" if crate.get("untested") else "ok  " if crate["passed"] else "FAIL"
            print(f"{mark} {crate['fixture']:24} {crate['tests_run']:5} tests  "
                  f"{done:5}/{total:<5} functions  {' '.join(failing[:6])} {' '.join(errors[:2])}")
        compiled, roots, tests = compiled + done, roots + total, tests + crate["tests_run"]
    print(f"\n{tests} tests, {compiled}/{roots} functions from blaze\n")
    for reason, n in reasons.most_common(40):
        print(f"{n:6}  {reason}  ({', '.join(sorted(where[reason])[:4])})")


if __name__ == "__main__":
    main()
