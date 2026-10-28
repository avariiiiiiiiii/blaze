#!/usr/bin/env bash
# What a build shows, as text (lane tui): captures.sh BLAZE OUT RIPGREP
# BLAZE is the blaze built from the change, RIPGREP a checkout of ripgrep 15.1.0 (copied, not
# written). On a terminal (under `script`, 100 columns) and piped, a cold build of ripgrep and
# then one of its functions' bodies edited, `blaze why` after it, and the records of a no-op
# build as JSON.
set -euo pipefail
blaze=$1 out=$2 ripgrep=$3
mkdir -p "$out"
work=$(mktemp -d)
cp -r "$ripgrep" "$work/ripgrep"
rm -rf "$work/ripgrep/target"
export BLAZE_HOME="$work/home"
cd "$work/ripgrep"
tty() { script -qfec "stty cols 100 rows 30; $*" "$out/$name.tty" >/dev/null; }
name=cold tty "$blaze" build
edit() {
  python3 - "$1" <<'EOF'
import sys
path = "crates/core/main.rs"
opening = "fn search(args: &HiArgs, mode: SearchMode) -> anyhow::Result<bool> {"
text = open(path).read().replace(opening + "\n    let _tui_edit = 0u32;", opening)
open(path, "w").write(text.replace(opening, opening + f"\n    let _tui_edit = {sys.argv[1]}u32;"))
EOF
}
edit 1
name=edit tty "$blaze" build
edit 2
"$blaze" build >"$out/edit.piped" 2>&1
"$blaze" why >"$out/why.txt" 2>&1
"$blaze" why search >"$out/why-search.txt" 2>&1
"$blaze" build --events json >"$out/noop-events.jsonl" 2>"$out/noop-events.stderr"
"$blaze" stop >/dev/null 2>&1 || true
rm -rf "$work"
