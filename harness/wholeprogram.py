#!/usr/bin/env python3
"""wholeprogram.py: programs blaze builds whole, with a `main` of its own, and runs.

usage: wholeprogram.py [CASE...] [--release] [--keep]

Each case is a program of the workspace `harness/wholeprogram`, copied to a directory of
its own. It is built with blaze as `RUSTC_WRAPPER` and `RUSTC_WORKSPACE_WRAPPER` as
`runbench.py` builds a program: rustc builds the registry's crates and stand-ins of no code for
the workspace's, and blaze compiles every crate of the program into one object, with the C
`main` of it, which is linked in their place. The program runs with no arguments, and what it
prints on either stream and the status it exits with have to be what Rust says of its source
(`EXPECTED`: a pattern where a count is the standard library's to choose). blaze then builds
the object once more, and it has to be the same bytes: a whole program's layout follows the
order its functions were asked for, not the order the threads finished them (#200).

  default_alloc  the default allocator; output on both streams and a status of its own
  custom_alloc   a `#[global_allocator]` of the program that counts what it is asked for
  lib_alloc      the `#[global_allocator]` is in a library of the workspace
  cfg_alloc      a `#[global_allocator]` under a `#[cfg]` that is false, as ripgrep's
  trait_block    a method a trait provides calls a function declared in its body
  c_main         `#![no_main]` and a `main` of C's, as bun's, with a body of 16 384 statements

The backend is `BLAZE_BACKEND`'s; `--release` builds both with the release profile. With
LLVM, a build of a whole program, whichever `main` it has, leaves no function to Cranelift
for being large or for its handlers, as a build that edits follow does.
"""
import argparse, os, re, shutil, sys, tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import apps

CASES = ["default_alloc", "custom_alloc", "lib_alloc", "cfg_alloc", "trait_block", "c_main"]
# What `backend::Backend::emit` leaves to Cranelift only in a build that is not of a whole program.
NOT_WHOLE = ("a body larger than LLVM is given in a build", "a handler LLVM lowers only for a tier")


def mixed(seed, rounds):
    """trait_block's `Mix::mixed`, in u32's wrapping arithmetic."""
    at = seed
    for _ in range(rounds):
        at = ((((at << 5) | (at >> 27)) & 0xFFFF_FFFF) ^ 0x9E37_79B9) + 17 & 0xFFFF_FFFF
    return at


def mix(seed):
    """c_main's `mix`: 4^7 rounds, in u64's arithmetic."""
    acc = seed
    for _ in range(4 ** 7):
        acc = (((acc << 5) | (acc >> 59)) & 0xFFFF_FFFF_FFFF_FFFF) ^ (acc >> 3)
    return acc


# Each case's (exit status, stdout, stderr), the streams as patterns matched whole: run with no
# arguments, `std::env::args().count()` and C's `argc` are 1.
EXPECTED = {
    "default_alloc": (3, re.escape('{"and": 1, "brown": 1, "dog": 1, "end": 1, "fox": 1, "jumps": 1, "lazy": 1, '
                                   '"over": 1, "quick": 1, "the": 3}\n'), re.escape("10 words\n")),
    "custom_alloc": (0, r"sum 332833500: [1-9]\d* allocations of [1-9]\d* bytes through the counting allocator\n", ""),
    "lib_alloc": (0, r"490 characters, [1-9]\d* allocations or reallocations\n", ""),
    "cfg_alloc": (0, re.escape("THE DEFAULT ALLOCATOR\n"), ""),
    "trait_block": (mixed(11, 7) % 7, re.escape(f"{mixed(3, 7)}\n"), ""),
    "c_main": (3, re.escape(f"A MAIN OF C'S {mix(1):x}\n"), re.escape("1 arguments\n")),
}


def check(ws, case, release):
    spec = {"package": case, "cargo_args": ["--release"] if release else []}
    fx = apps.App(ws, spec, alone=True)
    fx.blaze_args = ["--release"] if release else []
    try:
        fx.rustc_build()
        fx.blaze_build()
    except SystemExit as failed:
        return f"  blaze's build failed: {str(failed)[-800:]}"
    log = open(os.path.join(fx.work, "all.stderr")).read()
    left = [line.strip() for line in log.splitlines() if line.strip().endswith(NOT_WHOLE)]
    if left:
        return "  not built as a whole program, LLVM left to Cranelift:\n" + "\n".join(f"    {it}" for it in left)
    status, stdout, stderr = EXPECTED[case]
    got = fx.behaviour(fx.binary)
    lines = []
    if got[0] != status:
        lines.append(f"  status: {got[0]!r}, and Rust says {status}")
    for what, pattern, printed in (("stdout", stdout, got[1]), ("stderr", stderr, got[2])):
        if re.fullmatch(pattern, printed.decode(errors="replace")) is None:
            lines.append(f"  {what}: {printed!r}, not /{pattern}/")
    if lines:
        return "\n".join(lines)
    # The same source has to give the same object every build, whichever thread compiled
    # which function first (#200).
    first = open(fx.objects[0], "rb").read()
    try:
        fx.blaze_build()
    except SystemExit as failed:
        return f"  blaze's second build failed: {str(failed)[-800:]}"
    if open(fx.objects[0], "rb").read() != first:
        return f"  built differently the second time: {fx.objects[0]} ({len(first)} bytes the first)"
    return None


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("cases", nargs="*", default=CASES)
    parser.add_argument("--release", action="store_true")
    parser.add_argument("--keep", action="store_true", help="keep the copy of the workspace")
    args = parser.parse_args()
    work = tempfile.mkdtemp(prefix="blaze-wholeprogram-")
    ws = os.path.join(work, "ws")
    shutil.copytree(os.path.join(HERE, "wholeprogram"), ws, ignore=shutil.ignore_patterns("target*"))
    failed = 0
    for case in args.cases:
        wrong = check(ws, case, args.release)
        print(f"{case:15} {'WRONG' if wrong else 'ok'}")
        if wrong:
            print(wrong)
            failed += 1
    if args.keep:
        print(f"kept {ws}")
    else:
        shutil.rmtree(work, ignore_errors=True)
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
