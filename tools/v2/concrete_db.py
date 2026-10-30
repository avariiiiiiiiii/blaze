"""Moves a front-end crate's code from `dyn` databases to the one concrete `base_db::Db`.

Input: `{"root": "<workspace>", "crates": ["hir-def", ...]}` (inline, a path, or `-`), and
`"write": true` to change the files (without it, only what would change is printed). For each
crate's `.rs` files it rewrites what is mechanical (docs/v2/plan/lanes/frontend/db.md, "The
migration"):

- `&dyn HirDatabase`, `&dyn SourceDatabase`, `&dyn salsa::Database` (any path, with or without
  a lifetime or `mut`) become `&Db`;
- `.as_dyn()` is dropped;
- `SourceDatabase::` becomes `Db::`, and `SourceDatabase` leaves `use` lists;
- a file that now names `Db` and doesn't import it gets `use base_db::Db;` (`use crate::Db;` in
  base-db).

Then it prints, per file and line, what it left for a person: `impl SourceDatabase for`, generic
bounds on the database traits, casts `as &dyn`, a `dyn` database in any other form, a crate whose
manifest lacks base-db, and a file with an item of its own named `Db`. It exits 1 when it left any.
"""

import re
import sys
from dataclasses import dataclass, field
from pathlib import Path

from typed_input import parse

SCRIPT = "concrete_db.py"

TRAITS = r"(?:HirDatabase|SourceDatabase|DefDatabase|ExpandDatabase|RootQueryDb|Database)"
# A path to one of the traits: `HirDatabase`, `hir::db::HirDatabase`, `salsa::Database`.
TRAIT_PATH = rf"(?:(?:crate|self|super|[a-z_][a-z0-9_]*)::)*{TRAITS}\b"
DYN_REF = re.compile(rf"&(?P<lt>\s*'[a-z_][a-z0-9_]*\s+)?(?P<mut>mut\s+)?dyn\s+(?P<path>{TRAIT_PATH})")
AS_DYN = re.compile(r"\.as_dyn\(\)")
UFCS = re.compile(r"\b(?:base_db::)?SourceDatabase::")
USE = re.compile(r"^(?P<indent>\s*)(?:pub(?:\([^)]*\))?\s+)?use\s[^;]*;", re.MULTILINE | re.DOTALL)
IN_USE = re.compile(r"\bSourceDatabase(?:\s+as\s+_)?\s*,?\s*")
NAMES_DB = re.compile(r"(?<![\w:'])Db\b")
OWN_DB = re.compile(r"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:struct|enum|type|trait)\s+Db\b|use\s[^;]*\bDb\b", re.MULTILINE)

LEFT = (
    (re.compile(r"\bimpl(?:<[^>]*>)?\s+(?:[\w:]+::)?SourceDatabase\s+for\b"), "a database type of its own: make it `TestDB { db: Db }` with `Deref<Target = Db>`, from `Db::for_tests()`"),
    (re.compile(r"\bimpl(?:<[^>]*>)?\s+(?:[\w:]+::)?HirDatabase\s+for\b"), "an impl of `HirDatabase`: hir-ty's one impl for `Db` is the only one"),
    (re.compile(r"impl_hir_database!"), "`impl_hir_database!`: gone; call `hir_ty_mir::provide()` where the database is made"),
    (re.compile(r"(?:(?<!:):(?!:)|\+)\s*\??\s*(?:[\w:]+::)?(?:HirDatabase|SourceDatabase)\b(?!::)"), "a generic bound on a database trait: take `&Db`"),
    (re.compile(r"\bas\s+&\s*dyn\b"), "a cast to a `dyn` database: pass `&Db`, or deref a test database (`&*db`)"),
    (re.compile(rf"\bdyn\s+{TRAIT_PATH}"), "a `dyn` database in another form (a `Box`, a pointer, a macro): `Db`"),
    (re.compile(r"hir_database_is_dyn_compatible"), "the dyn-compatibility test: `dyn HirDatabase` must not compile now"),
)


@dataclass(frozen=True)
class Input:
    root: str
    crates: tuple[str, ...]
    write: bool = False


@dataclass
class FileResult:
    rewrites: int = 0
    left: list[str] = field(default_factory=list)


def strip_from_uses(text: str) -> tuple[str, int]:
    """`text` with `SourceDatabase` out of its `use` items, and how many it took out."""
    count = 0

    def one(m: re.Match[str]) -> str:
        nonlocal count
        item = m.group(0)
        if "SourceDatabase" not in item or "SourceDatabase::" in item:
            return item
        new, n = IN_USE.subn("", item)
        count += n
        new = re.sub(r"\{\s*\}", "{}", new)
        new = re.sub(r",\s*\}", "}", new)
        new = re.sub(r"\{\s*,", "{", new)
        # Nothing left to import: `use base_db::{};`, `use base_db::;` or `use base_db::{}::x;`.
        if re.fullmatch(r"\s*(?:pub(?:\([^)]*\))?\s+)?use\s+[\w:]*(?:::)?\{\}\s*;", new) or re.fullmatch(
            r"\s*(?:pub(?:\([^)]*\))?\s+)?use\s+[\w:]*::\s*;", new
        ):
            return ""
        return new

    return USE.sub(one, text), count


def add_db_import(text: str, path: str) -> str:
    """`text` with `use <base_db>::Db;` before its first top-level `use`, or after its inner docs."""
    line = "use crate::Db;\n" if path == "base-db" else "use base_db::Db;\n"
    lines = text.splitlines(keepends=True)
    at = next((i for i, it in enumerate(lines) if it.startswith("use ") or it.startswith("pub use ")), None)
    if at is None:
        at = 0
        while at < len(lines) and (lines[at].startswith("//!") or lines[at].startswith("#![") or not lines[at].strip()):
            at += 1
        lines.insert(at, line + "\n")
    else:
        lines.insert(at, line)
    return "".join(lines)


def code_only(line: str) -> str:
    return line.split("//", 1)[0]


def migrate(text: str, crate: str) -> tuple[str, FileResult]:
    result = FileResult()
    had_own_db = bool(OWN_DB.search(text))

    def to_db(m: re.Match[str]) -> str:
        lt = (m.group("lt") or "").strip()
        mut = "mut " if m.group("mut") else ""
        return f"&{lt + ' ' if lt else ''}{mut}Db"

    new_lines: list[str] = []
    for line in text.splitlines(keepends=True):
        if re.search(r"\bas\s+&\s*dyn\b", line):
            new_lines.append(line)
            continue
        line, n1 = DYN_REF.subn(to_db, line)
        line, n2 = AS_DYN.subn("", line)
        line, n3 = UFCS.subn("Db::", line)
        result.rewrites += n1 + n2 + n3
        new_lines.append(line)
    new = "".join(new_lines)
    new, n = strip_from_uses(new)
    result.rewrites += n

    names_db = any(NAMES_DB.search(code_only(it)) for it in new.splitlines() if not it.lstrip().startswith("use "))
    imports_db = re.search(r"use\s[^;]*\bDb\b", new) is not None
    if result.rewrites and names_db and not imports_db:
        if had_own_db:
            result.left.append("0: an item of its own named `Db`: the rewrite's `Db` would name it; rename one")
        else:
            new = add_db_import(new, crate)
    for i, line in enumerate(new.splitlines(), 1):
        code = code_only(line)
        for pattern, why in LEFT:
            if pattern.search(code):
                result.left.append(f"{i}: {why}")
                break
    return new, result


def depends_on_base_db(manifest: Path) -> bool:
    return manifest.name == "Cargo.toml" and re.search(r"^base-db\s*[.=]", manifest.read_text(), re.MULTILINE) is not None


def main() -> None:
    match sys.argv[1:]:
        case [text]:
            cfg = parse(Input, text, SCRIPT)
        case _:
            print(f"usage: {SCRIPT} '{{\"root\": \".\", \"crates\": [\"hir-def\"]}}'", file=sys.stderr)
            sys.exit(2)
    root = Path(cfg.root)
    left_total = 0
    for crate in cfg.crates:
        base = root / "crates" / crate
        if not base.is_dir():
            print(f"{crate}: no such crate under {root / 'crates'}", file=sys.stderr)
            sys.exit(2)
        rewrites = 0
        left: list[str] = []
        for path in sorted(base.rglob("*.rs")):
            if "target" in path.parts:
                continue
            text = path.read_text()
            new, result = migrate(text, crate)
            rewrites += result.rewrites
            rel = path.relative_to(root)
            left.extend(f"{rel}:{it}" for it in result.left)
            if cfg.write and new != text:
                path.write_text(new)
        if rewrites and crate != "base-db" and not depends_on_base_db(base / "Cargo.toml"):
            left.append(f"crates/{crate}/Cargo.toml: no `base-db` dependency for `base_db::Db`")
        for line in left:
            print(line)
        print(f"{rewrites:6} rewritten, {len(left):4} left  {crate}", file=sys.stderr)
        left_total += len(left)
    sys.exit(1 if left_total else 0)


if __name__ == "__main__":
    main()
