#!/usr/bin/env python3
"""scripts/rename.py: renames ra-jit to blaze throughout a checkout (#270).

usage: python3 scripts/rename.py [--env] [--check] [--dry-run]

Run from anywhere inside a checkout of blaze, on a branch with nothing uncommitted. It rewrites
every tracked text file and `git mv`s every tracked path whose name says ra-jit, then prints the
places a rule may have made two names one (an identifier the file already had). It changes
nothing it has already changed, so a branch made before the rename runs it on itself and then
rebases: `python3 scripts/rename.py && git commit -am 'rename.py' && git rebase origin/main`.

  --env      only the settings: `RA_JIT_<NAME>` becomes `BLAZE_<NAME>`, and nothing else
  --check    changes nothing; lists what still says ra-jit, and fails when anything does
  --dry-run  changes nothing; says how many lines of which files it would change

The names (docs/rename.md has the reasons):

  crates/ra-jit            the crate blaze-engine (published as cargo-blaze, as before)
  crates/ra-jit-<name>     the crate blaze-<name>; ra-jit-cache's library `ra_jit` is `blaze`
  ra-jit (the program)     blaze-engine, which `blaze` and `cargo-blaze` run
  ra-jit-rustc             blaze-rustc
  RA_JIT_<NAME>            BLAZE_<NAME>
  ra-jit, ra_jit, RaJit    blaze, blaze, Blaze, in everything else

CHANGELOG.md keeps its history, and this script and docs/rename.md the old names they map.
"""

import argparse
import os
import re
import subprocess
import sys

# Kept as they are: history, the map itself, and names outside the repository that are renamed
# where they live first (a secret's value cannot be read to make it again under another name).
SKIPPED = {
    "CHANGELOG.md",
    "scripts/rename.py",
    "docs/rename.md",
    # What blaze says of the old names, which it has to name.
    "crates/ra-jit/src/renamed.rs",
    "crates/blaze-engine/src/renamed.rs",
}
KEPT = ["secrets.RA_JIT_BOT_TOKEN"]

ENV = [(r"RA_JIT_(?=[A-Z0-9])", "BLAZE_")]

# The program the other binaries run, where a name means it rather than blaze as a whole.
ENGINE = "blaze-engine"
COMMANDS = (
    "cargo|std|cache|closure|mir|daemon|rustc-wrapper|cargo-answer|compile-time-deps|deps|"
    "borrowck|weaken|--check|--spare|--version|-V|build-script|proc-macro-wasm"
)
BIN_DIRS = r"release|debug|dist|dev-rel|profiling|bin|shipped|unpacked|\$NAME|\$bin|\$unpacked|\$out/bin|\$\{bin\}"
END = r"(?=(?:\.exe)?(?![\w.-]))"

RULES = ENV + [
    # Other spellings of the settings' prefix and of the name.
    (r"RA_JIT", "BLAZE"),
    (r"RAJIT", "BLAZE"),
    (r"RA-JIT", "BLAZE"),
    (r"RA JIT", "BLAZE"),
    (r"RaJit", "Blaze"),
    (r"Ra[-_ ]jit", "Blaze"),
    # The repository's old name.
    (r"indexable-inc/ra-jit(?![\w-])", "indexable-inc/blaze"),
    # `cargo ra-jit`, the command's name before `cargo blaze`.
    (r"cargo-ra-jit", "cargo-blaze"),
    (r"cargo_ra_jit", "cargo_blaze"),
    (r"\bcargo ra-jit\b", "cargo blaze"),
    (r'"cargo", "ra-jit"', '"cargo", "blaze"'),
    (r'"cargo-blaze", "ra-jit"', '"cargo-blaze", "blaze"'),
    # The engine's crate, its directory and its program.
    (r"crates/ra-jit(?![\w-])", "crates/" + ENGINE),
    (r"(-p|--package|--bin|--exclude)([ =])ra-jit(?![\w-])", r"\1\2" + ENGINE),
    (r'"(-p|--package|--bin|--exclude)", "ra-jit"', r'"\1", "' + ENGINE + '"'),
    (r'(?m)^(name|package) = "ra-jit"', r'\1 = "' + ENGINE + '"'),
    (r'(package = )"ra-jit"', r'\1"' + ENGINE + '"'),
    (r"(?m)^ra-jit = ", ENGINE + " = "),
    (r'"ra-jit\{', '"' + ENGINE + "{"),
    (r"(?<![\w.-])((?:" + BIN_DIRS + r")/)ra-jit" + END, r"\1" + ENGINE),
    (r'((?:"release"|"debug"|"dist"|"bin"|\bbin|"shipped")(?:, | / ))"ra-jit"', r'\1"' + ENGINE + '"'),
    (r'(?<="BLAZE_BIN", )"ra-jit"', '"' + ENGINE + '"'),
    (r'(?<=\[)"ra-jit"(?=, )', '"' + ENGINE + '"'),
    (r"(?<![\w/.-])ra-jit(?= (?:" + COMMANDS + r")(?![\w-]))", ENGINE),
    # Everything else: the crates `ra-jit-<name>` and their paths, then the name itself.
    (r"ra-jit-", "blaze-"),
    (r"ra_jit_", "blaze_"),
    (r"rajit", "blaze"),
    (r"ra-jit", "blaze"),
    (r"ra_jit", "blaze"),
    (r"(?i)ra[-_ ]?jit", "blaze"),
]

# A path's new name: the engine's crate directory first, then the rules of text.
PATH_RULES = [(r"(^|/)crates/ra-jit(?=/)", r"\1crates/" + ENGINE)] + RULES

# Words a rule turns into another word a file may already have.
IDENTS = {
    "ra_jit": "blaze",
    "cargo_ra_jit": "cargo_blaze",
    "RA_JIT": "BLAZE",
    "RaJit": "Blaze",
}


def git(*args, check=True):
    return subprocess.run(["git", *args], check=check, capture_output=True, text=True).stdout


def rewrite(text, rules):
    kept = {}
    for n, token in enumerate(KEPT):
        mark = f"\x00kept{n}\x00"
        kept[mark] = token
        text = text.replace(token, mark)
    for pattern, new in rules:
        text = re.sub(pattern, new, text)
    for mark, token in kept.items():
        text = text.replace(mark, token)
    return text


def collisions(path, text):
    said = []
    for old, new in IDENTS.items():
        word = lambda w: re.compile(r"(?<![A-Za-z0-9_])" + re.escape(w) + r"(?![A-Za-z0-9_])")
        if word(old).search(text) and word(new).search(text):
            said.append(f"{path}: `{old}` becomes `{new}`, which it already has")
    return said


def check():
    out = git("grep", "-InP", r"(?i)ra[-_ ]?jit", "--", ".", *[f":!{it}" for it in SKIPPED], check=False)
    lines = [it for it in out.splitlines() if not any(k in it for k in KEPT)]
    for line in lines:
        print(line)
    print(f"{len(lines)} lines still say ra-jit", file=sys.stderr)
    return 1 if lines else 0


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--env", action="store_true")
    ap.add_argument("--check", action="store_true")
    ap.add_argument("--dry-run", action="store_true")
    args = ap.parse_args()
    os.chdir(git("rev-parse", "--show-toplevel").strip())
    if args.check:
        return check()
    if not args.dry_run and git("status", "--porcelain", "--untracked-files=no").strip():
        sys.exit("rename.py: commit or drop the changes of this checkout first")
    rules = ENV if args.env else RULES
    files = [it for it in git("ls-files", "-z").split("\0") if it and it not in SKIPPED]

    moves = []
    if not args.env:
        for path in files:
            new = rewrite(path, PATH_RULES)
            if new != path:
                if os.path.lexists(new):
                    sys.exit(f"rename.py: {path} would become {new}, which is there already")
                moves.append((path, new))

    changed, warned = 0, []
    for path in files:
        if not os.path.isfile(path) or os.path.islink(path):
            continue
        with open(path, "rb") as f:
            raw = f.read()
        if b"\0" in raw[:8192]:
            continue
        try:
            text = raw.decode()
        except UnicodeDecodeError:
            continue
        new = rewrite(text, rules)
        if new == text:
            continue
        changed += 1
        if not args.env:
            warned += collisions(path, text)
        if args.dry_run:
            lines = sum(a != b for a, b in zip(text.splitlines(), new.splitlines()))
            print(f"{lines:6} {path}")
            continue
        with open(path, "w", newline="") as f:
            f.write(new)

    for old, new in moves:
        if args.dry_run:
            print(f"  move {old} -> {new}")
            continue
        os.makedirs(os.path.dirname(new) or ".", exist_ok=True)
        git("mv", old, new)
    for line in warned:
        print("check:", line, file=sys.stderr)
    print(f"rename.py: {changed} files rewritten, {len(moves)} moved", file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
