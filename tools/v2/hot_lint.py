"""Flags allocation, dynamic dispatch and cloning in v2's hot crates without building anything.

Input: `{"root": "<workspace>"}`, optionally `"crates": [...]` to narrow the set (inline, a path,
or `-`). Prints `path:line: what` per finding, then a count per crate, and exits 1 when there is
any. A line is exempt when it, or the line above it, has a `// alloc: <why>`, `// dyn: <why>` or
`// clone: <why>` comment naming why the cost is needed there. Test code is skipped: `tests/`,
`*tests.rs`, items under a `cfg` that only test or fuzz builds set (`test`, `all(test, ..)`,
`any(test, feature = "fuzz")`), the files of such out-of-line `mod x;`, and sources a test pulls
in with `#[path]`.
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
# String, raw string and char literals, so braces inside them don't count.
LITERAL = re.compile(r"""b?r(#*)".*?"\1|b?"(\\.|[^"\\])*"|b?'(\\.|[^'\\])'""")
CFG = re.compile(r"#\[cfg\((.*)\)\]")
MOD_DECL = re.compile(r"^\s*(pub(\([^)]*\))?\s+)?mod\s+(\w+)\s*;")
PATH_ATTR = re.compile(r'#\[path\s*=\s*"([^"]+)"\]')
# `cfg` atoms that only test or fuzz builds set.
TEST_ATOMS = {"test", 'feature = "fuzz"', "fuzzing"}


@dataclass(frozen=True)
class Input:
    root: str
    crates: tuple[str, ...] = HOT


def code_of(line: str) -> str:
    """The line without literals and its trailing comment."""
    return LITERAL.sub('""', line).split("//", 1)[0]


def cfg_args(expr: str) -> list[str]:
    """The top-level comma-separated arguments of a `cfg` predicate list."""
    args, depth, cur = [], 0, ""
    for ch in expr:
        if ch == "(":
            depth += 1
        elif ch == ")":
            depth -= 1
        if ch == "," and depth == 0:
            args.append(cur.strip())
            cur = ""
        else:
            cur += ch
    if cur.strip():
        args.append(cur.strip())
    return args


def test_only(expr: str) -> bool:
    """Whether a `cfg` predicate holds only in test (or fuzz) builds."""
    expr = expr.strip()
    if expr in TEST_ATOMS:
        return True
    if expr.startswith("all(") and expr.endswith(")"):
        return any(test_only(arg) for arg in cfg_args(expr[4:-1]))
    if expr.startswith("any(") and expr.endswith(")"):
        args = cfg_args(expr[4:-1])
        return bool(args) and all(test_only(arg) for arg in args)
    return False


def test_attr(line: str) -> bool:
    match = CFG.search(line.split("//", 1)[0])
    return bool(match) and test_only(match.group(1))


def in_test_module(lines: list[str]) -> list[bool]:
    """Which lines sit inside a test-only `cfg` item, by brace depth outside literals."""
    inside = [False] * len(lines)
    depth = 0
    start: int | None = None
    pending = False
    for i, line in enumerate(lines):
        if start is None and test_attr(line):
            pending = True
        code = code_of(line)
        opens, closes = code.count("{"), code.count("}")
        if pending and opens:
            start, depth, pending = i, 0, False
        elif pending and code.rstrip().endswith((";", ",")):
            # A bodiless item (`mod x;`, `use`, a field): it alone is test code.
            inside[i], pending = True, False
            continue
        if start is not None:
            depth += opens - closes
            inside[i] = True
            if depth <= 0 and i > start:
                start = None
    return inside


def module_dir(path: Path) -> Path:
    """Where `mod x;` in this file looks for `x`."""
    return path.parent if path.name in ("lib.rs", "main.rs", "mod.rs") else path.with_suffix("")


def module_tree(target: Path) -> tuple[Path, Path]:
    """A module's file and the directory holding its submodules."""
    return target, target.parent if target.name == "mod.rs" else target.with_suffix("")


def test_files(base: Path) -> list[tuple[Path, Path]]:
    """Files only test builds compile: out-of-line modules declared under a test `cfg`, and
    sources a test, bench or example pulls in with `#[path]`."""
    found: list[tuple[Path, Path]] = []
    for path in base.rglob("*.rs"):
        parts = set(path.relative_to(base).parts)
        from_tests = bool({"tests", "benches", "examples"} & parts)
        lines = path.read_text(errors="ignore").splitlines()
        for i, line in enumerate(lines):
            decl = MOD_DECL.match(line.split("//", 1)[0].split("]")[-1])
            if not decl:
                continue
            attrs = [line]
            j = i - 1
            while j >= 0 and lines[j].strip().startswith("#["):
                attrs.append(lines[j])
                j -= 1
            if not (from_tests or any(test_attr(attr) for attr in attrs)):
                continue
            explicit = next((m.group(1) for a in attrs if (m := PATH_ATTR.search(a))), None)
            if explicit is not None:
                found.append(module_tree((path.parent / explicit).resolve()))
            elif not from_tests:
                name = decl.group(3)
                for target in (module_dir(path) / f"{name}.rs", module_dir(path) / name / "mod.rs"):
                    found.append(module_tree(target.resolve()))
    return found


def is_test_file(path: Path, tests: list[tuple[Path, Path]]) -> bool:
    path = path.resolve()
    return any(path == file or path.is_relative_to(tree) for file, tree in tests)


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
        tests = test_files(base)
        for path in sorted(src.rglob("*.rs")):
            name = path.name
            parts = path.relative_to(base).parts
            if {"tests", "benches", "examples", "target"} & set(parts) or name.endswith("tests.rs"):
                continue
            if is_test_file(path, tests):
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
