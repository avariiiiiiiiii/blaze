"""Flags allocation, dynamic dispatch and cloning in v2's hot crates without building anything.

Input: `{"root": "<workspace>"}`, optionally `"crates": [...]` to narrow the set (inline, a path,
or `-`). Prints `path:line: what` per finding, then a count per crate, and exits 1 when there is
any. A line is exempt when it, or the line above it, has a `// alloc: <why>`, `// dyn: <why>` or
`// clone: <why>` comment naming why the cost is needed there. Test code (`tests/`, `*tests.rs`,
`#[cfg(test)]` modules) is skipped.
"""

import re
import sys
from dataclasses import dataclass
from pathlib import Path

from typed_input import parse

SCRIPT = "hot_lint.py"
HOT = (
    "blaze-query", "blaze-ids", "intern", "blaze-graph", "blaze-cas", "blaze-vfs", "blaze-core",
    "blaze-link", "blaze-codegen", "blaze-stencil", "blaze-lazy", "blaze-tier", "blaze-items",
    "parser", "syntax-tree", "span", "tt", "mbe", "hir-def", "hir-expand", "hir-ty",
    "hir-ty-mir", "hir-ty-codegen", "base-db", "blaze-diagnostics",
)
CHECKS = (
    ("clone", re.compile(r"\.clone\(\)"), "`.clone()`: borrow, copy an ID, or make the type not `Clone`"),
    ("dyn", re.compile(r"\bdyn\s+[A-Z]"), "`dyn`: a concrete type, a generic, or a tag `match`"),
    ("alloc", re.compile(r"\bString::(new|from)|\.to_string\(\)|\.to_owned\(\)|format!\("), "string allocation: intern it, or format only at the edge"),
    ("alloc", re.compile(r"\bVec::new\(\)|vec!\[|\.to_vec\(\)"), "fresh `Vec`: arena, reused buffer, `with_capacity`, or `SmallVec`"),
    ("alloc", re.compile(r"\.collect::<Vec|\.collect\(\)"), "`collect()`: write into a reused buffer or iterate lazily"),
    ("alloc", re.compile(r"\bBox::new\(|\bArc::new\(|\bRc::new\("), "per-value box: arena slot and `u32` ID"),
    ("alloc", re.compile(r"std::collections::Hash(Map|Set)"), "SipHash map: `FxHashMap`, or direct index by content hash"),
)
EXEMPT = re.compile(r"//\s*(alloc|dyn|clone):\s*\S")


@dataclass(frozen=True)
class Input:
    root: str
    crates: tuple[str, ...] = HOT


def in_test_module(lines: list[str]) -> list[bool]:
    """Which lines sit inside a `#[cfg(test)]` item, by brace depth."""
    inside = [False] * len(lines)
    depth = 0
    start: int | None = None
    pending = False
    for i, line in enumerate(lines):
        if start is None and "#[cfg(test)]" in line:
            pending = True
        opens, closes = line.count("{"), line.count("}")
        if pending and opens:
            start, depth, pending = i, 0, False
        if start is not None:
            depth += opens - closes
            inside[i] = True
            if depth <= 0 and i > start:
                start = None
    return inside


def lint_file(path: Path, root: Path) -> list[str]:
    lines = path.read_text(errors="ignore").splitlines()
    tests = in_test_module(lines)
    out: list[str] = []
    for i, line in enumerate(lines):
        code = line.split("//", 1)[0]
        if tests[i] or not code.strip():
            continue
        if EXEMPT.search(line) or (i and EXEMPT.search(lines[i - 1])):
            continue
        for _, pattern, why in CHECKS:
            if pattern.search(code):
                out.append(f"{path.relative_to(root)}:{i + 1}: {why}")
    return out


def main() -> None:
    match sys.argv[1:]:
        case [text]:
            cfg = parse(Input, text, SCRIPT)
        case _:
            print(f"usage: {SCRIPT} '{{\"root\": \"<workspace>\"}}'", file=sys.stderr)
            sys.exit(2)
    root = Path(cfg.root)
    total = 0
    counts: list[tuple[str, int]] = []
    for crate in cfg.crates:
        # A crate may keep its sources at its root (`hir-ty-codegen` does) rather than in `src/`.
        base = root / "crates" / crate
        src = base / "src" if (base / "src").is_dir() else base
        if not src.is_dir():
            continue
        found: list[str] = []
        for path in sorted(src.rglob("*.rs")):
            name = path.name
            parts = path.relative_to(base).parts
            if {"tests", "benches", "examples", "target"} & set(parts) or name.endswith("tests.rs"):
                continue
            found.extend(lint_file(path, root))
        for line in found:
            print(line)
        counts.append((crate, len(found)))
        total += len(found)
    for crate, n in counts:
        print(f"{n:6} {crate}", file=sys.stderr)
    sys.exit(1 if total else 0)


if __name__ == "__main__":
    main()
