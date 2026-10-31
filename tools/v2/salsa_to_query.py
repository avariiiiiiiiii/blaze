"""Rewrites the mechanical part of a crate's move from the salsa fork to `blaze_query::db`.

Input: `{"root": ".", "crates": ["crates/hir-def", ..], "write": false}` (inline, a path, or `-`).
Each crate is a directory relative to the root holding a `Cargo.toml`.

What it rewrites, in code only (never in comments or string literals):

- `#[salsa::tracked|input|interned]`, `salsa::Supertype` -> `blaze_query::..` (the macros),
  without their `revisions = ..` (nothing is collected, and the macros refuse it);
- `salsa::{Id, Durability, Setter, Cycle, Reads, ReadKind, Event, EventKind, DatabaseKeyIndex,
  Cancelled, Database, capture_reads, replay_reads, unshareable_reads, report_untracked_read,
  attach}`, `salsa::plumbing::{AsId, FromId, Id, attach}` and
  `salsa::plumbing::interned::NamesIds` -> `blaze_query::db::..`;
- `salsa::Revision` -> `blaze_query::db::View`; `SalsaValue` -> `blaze_query::db::Value`, and
  `blaze_query::Value` inside a `#[derive(..)]`;
- `base_db::salsa::..` as `salsa::..`, and `use salsa::..` trees leaf by leaf;
- on salsa IDs, `.as_bits()` -> `.as_u32()` and `Id::from_bits(..)` -> `Id::from_u32(..)`
  (`u64::from(x)` arguments unwrapped);
- in the manifest, `salsa.workspace = true` -> `blaze-query.workspace = true` (dropped when the
  table already has `blaze-query`).

With `"write": false` it prints the unified diff it would make; with `true` it writes the files.

The port replays: `"base": "<git ref>"` reads each crate's files from that commit instead of the
working tree, and `"hand": "tools/v2/salsa_port"` then applies `<hand>/<crate dir name>.json`,
the crate's recorded hand edits (exact replacements in order, files created or deleted), on
the mechanical rewrite. An edit that no longer matches exactly once stops the run naming the
file and edit: redo that case on the new base and record again; never resolve it in the JSON.
`"record": true` (with `base` and `hand`) writes those files: each crate's working files against
the mechanical rewrite of `base`'s, checked to replay byte for byte. After a change to this
script's rules, record every crate again (its working files are the truth).
Either way it then prints every salsa use it left, per file and line: `hand` (code that needs a
person: plumbing, zalsa, events, generation bits, `salsa::Database` methods, u64-to-u32 ID
types), `string` (salsa in a string literal: test expectations, `cfg(feature = "salsa")`) and
`comment`. Exits 0 when nothing is left and 1 when something is; bad input exits naming what is
wrong, before anything is read or written.
"""

import difflib
import json
import re
import subprocess
import sys
from dataclasses import dataclass, replace
from pathlib import Path

from hot_lint import LITERAL
from typed_input import fail, parse

SCRIPT = "salsa_to_query.py"


@dataclass(frozen=True)
class Input:
    root: str
    crates: tuple[str, ...]
    write: bool = False
    base: str = ""
    hand: str = ""
    record: bool = False


MACROS = {"tracked", "input", "interned", "Supertype"}
DB = {
    "Id", "Durability", "Setter", "Cycle", "Reads", "ReadKind", "Read", "Event", "EventKind",
    "DatabaseKeyIndex", "Cancelled", "Database", "capture_reads", "replay_reads",
    "unshareable_reads", "report_untracked_read", "attach",
}
RENAMED = {"Revision": "View", "SalsaValue": "Value"}
PLUMBING = {"AsId", "FromId", "Id", "attach"}
# The contract's `EventKind` variants; any other variant a crate matches on is a hand case.
EVENTS = {"WillExecute", "DidValidateMemoizedValue", "DidInternValue", "WillIterateCycle", "WillCheckCancellation"}
# `salsa::Database` methods: free functions or `Storage` methods in the contract.
DB_METHODS = {
    "unwind_if_revision_cancelled": "`blaze_query::db::unwind_if_cancelled(db)`",
    "synthetic_write": "`db.storage_mut().synthetic_write(durability)`",
    "report_untracked_read": "`blaze_query::db::report_untracked_read(db)`",
    "ingredient_debug_name": "`db.storage().ingredient_name(key.ingredient_index())`",
    "zalsa": "no zalsa: `Storage` (`view`, `owner`, `ingredient_name`, `ingredient_file`, `ids_of`), `FromIdIn::from_id_in`",
    "zalsa_mut": "no zalsa: `db.storage_mut()` (`keep_every_value`, `synthetic_write`)",
    "trigger_lru_eviction": "eviction runs at the view boundary; `evict_values` for on-demand",
    "trigger_cancellation": "a write clears the stop: `let _ = db.storage_mut();`",
    "stop_readers": "`db.storage().stop_readers()` (base-db's `Shared::stop_readers` stays)",
    "readers_stopped": "`db.storage().readers_stopped()`",
    "task_depth": "goes: the engine's frames push a barrier at each pumping wait",
    "memory_usage": "`db.storage().memory_usage(&mut out)` (`Vec<MemoryUsage>`)",
}
# What the salsa paths this script leaves become (by longest prefix), for its report.
HINTS = {
    "db": "`#[salsa::db]` goes: `impl blaze_query::db::Database for Db` (`storage`, `storage_mut`)",
    "Storage": "`blaze_query::db::Storage::new(hook)`, `hook: Option<Hook>`",
    "DatabaseImpl": "no generic database: a test makes a `base_db::Db`",
    "StableHasher": "blaze_ids' stable hasher",
    "Update": "no `Update`: values stay in place; `blaze_query::db::Value` for `'db` types",
    "IngredientIndex": "an ingredient is a `u32` (`DatabaseKeyIndex::ingredient_index`)",
    "IngredientInfo": "`blaze_query::db::MemoryUsage` (name, rows, row_bytes, value_bytes)",
    "Zalsa": "no zalsa: `Storage` (`view`, `owner`, `ingredient_name`, `ingredient_file`, `ids_of`), `FromIdIn::from_id_in`",
    "plumbing::Zalsa": "no zalsa: `Storage` (`view`, `owner`, `ingredient_name`, `ingredient_file`, `ids_of`), `FromIdIn::from_id_in`",
    "plumbing::ZalsaDatabase": "no zalsa: `Database::storage()` (`view()` for the revision)",
    "plumbing::SalsaStructInDb": "`T: blaze_query::db::FromIdIn`, `T::from_id_in(db.storage(), id)`",
    "plumbing::interned_arguments": "the generated `f::arguments(db, id) -> Option<(A, ..)>` of the rule's function",
    "plumbing": "salsa plumbing: see the cases file",
    "image": "the interned image section (`SectionId::QUERY_INTERNED`)",
    "graph": "no graph image in the contract: see the cases file",
}

PATH = re.compile(r"(?<![\w:])(?:::)?(?:base_db::)?salsa::([A-Za-z_]\w*(?:::[A-Za-z_]\w*)*)")
USE = re.compile(r"(?<![\w:])use\s+(?:::)?(?:base_db::)?salsa::")
TOKEN = re.compile(r"\s*(::|[{},*]|[A-Za-z_]\w*)")
FROM_BITS = re.compile(r"(?<![\w:])((?:::)?blaze_query::db::Id|Id)::from_bits\(")
AS_ID_BITS = re.compile(r"\bas_id\((?:[^()]|\([^()]*\))*\)\.as_bits\(\)")
AS_BITS = re.compile(r"\.as_bits\(\)")
U64_FROM = re.compile(r"^u64::from\((.*)\)(\s*\+\s*\d+)?$", re.DOTALL)
LEFT = re.compile(r"salsa|Salsa|SALSA")
EVENT = re.compile(r"\bEventKind::(\w+)")
INDEX = re.compile(r"\bas_id\(\)\.index\(\)")
READ_KIND = re.compile(r"\bReadKind::(\w+)")
READ_KINDS = {"Function", "InputField", "TrackedField", "Interned"}
CANCELLED = re.compile(r"\bCancelled::(\w+)")
CANCELS = {"PendingWrite", "WouldWaitOnItself", "PropagatedPanic", "catch", "throw"}
FOR_EACH_READ = re.compile(r"\.for_each_read\(")
ATTR = re.compile(r"#\[blaze_query::(?:tracked|input|interned)\(")
REVISIONS = re.compile(r"\s*revisions\s*=")
MANIFEST_DEP = re.compile(r'^salsa\.workspace\s*=\s*true\s*$')
TABLE = re.compile(r"^\s*\[([^\]]+)\]\s*$")


def mask(text: str) -> tuple[str, list[tuple[int, int, str]]]:
    """`text` with comments and literals blanked to spaces (newlines and the quotes kept), and
    each blanked span as `(start, end, "comment" | "string")`."""
    out = list(text)
    spans: list[tuple[int, int, str]] = []
    for m in LITERAL.finditer(text):
        kind = "comment" if m.group(0).startswith("/") else "string"
        spans.append((m.start(), m.end(), kind))
        lo, hi = m.start(), m.end()
        if kind == "string":
            lo, hi = text.index('"', lo) + 1 if '"' in m.group(0) else lo + 1, hi - 1
        for i in range(lo, hi):
            if out[i] != "\n":
                out[i] = " "
    return "".join(out), spans


def line_of(text: str, pos: int) -> int:
    return text.count("\n", 0, pos) + 1


def map_path(segs: list[str], in_derive: bool) -> str | None:
    """The `blaze_query` path for `salsa::<segs>`, or `None` when it is a hand case."""
    head, rest = segs[0], segs[1:]
    if head in MACROS:
        return "::".join(["blaze_query", *segs])
    if head == "SalsaValue":
        return "blaze_query::Value" if in_derive and not rest else "::".join(["blaze_query::db::Value", *rest])
    if head in RENAMED:
        return "::".join(["blaze_query::db", RENAMED[head], *rest])
    if head == "Database" and rest:
        return None
    if head in DB:
        return "::".join(["blaze_query::db", *segs])
    if head == "plumbing" and rest and rest[0] in PLUMBING:
        return "::".join(["blaze_query::db", *rest])
    if segs[:3] == ["plumbing", "interned", "NamesIds"]:
        return "::".join(["blaze_query::db", *segs[2:]])
    return None


def in_derive(masked: str, pos: int) -> bool:
    start = masked.rfind("#[", 0, pos)
    return start >= 0 and masked.startswith("#[derive(", start) and "]" not in masked[start:pos]


def parse_tree(tokens: list[str], i: int, prefix: list[str], leaves: list[tuple[list[str], str | None]]) -> int:
    """Parses one use tree at `tokens[i]` into `leaves` (path, alias); returns the next index."""
    if tokens[i] == "{":
        i += 1
        while tokens[i] != "}":
            i = parse_tree(tokens, i, prefix, leaves)
            if tokens[i] == ",":
                i += 1
        return i + 1
    if tokens[i] == "*":
        leaves.append(([*prefix, "*"], None))
        return i + 1
    path = [*prefix, tokens[i]]
    i += 1
    while tokens[i] == "::":
        if tokens[i + 1] == "{":
            return parse_tree(tokens, i + 1, path, leaves)
        path.append(tokens[i + 1])
        i += 2
    alias = None
    if tokens[i] == "as":
        alias = tokens[i + 1]
        i += 2
    leaves.append((path, alias))
    return i


def render(leaves: list[tuple[str, str | None]]) -> str:
    """One use tree for `leaves` (full paths under `blaze_query::`, aliases)."""
    groups: dict[str, list[str]] = {}
    for path, alias in leaves:
        parent, _, name = path.rpartition("::")
        groups.setdefault(parent, []).append(name + (f" as {alias}" if alias else ""))
    parts = []
    for parent in sorted(groups):
        names = sorted(set(groups[parent]))
        inner = names[0] if len(names) == 1 else "{" + ", ".join(names) + "}"
        parts.append(inner if parent == "blaze_query" else parent.removeprefix("blaze_query::") + "::" + inner)
    return parts[0] if len(parts) == 1 else "{" + ", ".join(parts) + "}"


@dataclass
class Left:
    line: int
    kind: str  # "hand", "string" or "comment"
    why: str


@dataclass
class FileResult:
    text: str
    left: list[Left]


def rewrite_uses(text: str, masked: str, edits: list[tuple[int, int, str]], left: list[Left]) -> tuple[bool, bool]:
    """Rewrites each `use salsa::..;`; returns whether it imported `Id` and `SalsaValue` bare."""
    imports_id = imports_value = False
    for m in USE.finditer(masked):
        end = masked.index(";", m.end())
        body = masked[m.end():end]
        if masked[m.end():end] != text[m.end():end]:
            left.append(Left(line_of(text, m.start()), "hand", "a comment inside a `use salsa::` tree"))
            continue
        tokens = TOKEN.findall(body) + ["<end>"]
        leaves: list[tuple[list[str], str | None]] = []
        if parse_tree(tokens, 0, [], leaves) != len(tokens) - 1:
            left.append(Left(line_of(text, m.start()), "hand", "a `use salsa::` tree this script can't read"))
            continue
        mapped: list[tuple[str, str | None]] = []
        kept: list[str] = []
        for path, alias in leaves:
            if path == ["SalsaValue"] and alias is None:
                imports_value = True
                continue
            if path[-1] == "Id" and alias is None and path in (["Id"], ["plumbing", "Id"]):
                imports_id = True
            new = map_path(path, False) if path[-1] not in ("*", "self") else None
            if new is None:
                kept.append("::".join(path) + (f" as {alias}" if alias else ""))
            else:
                mapped.append((new, alias))
        statements = []
        if mapped:
            statements.append(f"blaze_query::{render(mapped)}")
        if kept:
            tree = kept[0] if len(kept) == 1 else "{" + ", ".join(kept) + "}"
            statements.append(f"salsa::{tree}")
            left.append(Left(line_of(text, m.start()), "hand", f"`use salsa::{tree}`: no blaze_query form"))
        use_start = masked.index("use", m.start())
        if not statements:
            # Drop the whole statement with its line when it was alone on it.
            line_start = text.rfind("\n", 0, use_start) + 1
            line_end = text.find("\n", end)
            line_end = len(text) if line_end < 0 else line_end + 1
            prefix = text[line_start:use_start]
            if text[line_start:m.start()].strip() == "" and re.fullmatch(r"\s*(pub(\([^)]*\))?\s+)?", prefix):
                edits.append((line_start, line_end, ""))
            else:
                edits.append((m.start(), end + 1, ""))
            continue
        indent = text[text.rfind("\n", 0, use_start) + 1:use_start]
        indent = indent if indent.strip() == "" else ""
        joined = f";\n{indent}".join(f"use {s}" for s in statements)
        edits.append((use_start, end, joined))
    return imports_id, imports_value


def rewrite_paths(text: str, masked: str, edits: list[tuple[int, int, str]], left: list[Left], taken: list[tuple[int, int]]) -> None:
    for m in PATH.finditer(masked):
        if any(lo <= m.start() < hi for lo, hi in taken):
            continue
        segs = m.group(1).split("::")
        new = map_path(segs, in_derive(masked, m.start()))
        if new is None:
            left.append(Left(line_of(text, m.start()), "hand", f"`salsa::{m.group(1)}`"))
            continue
        edits.append((m.start(), m.end(), new))


def apply(text: str, edits: list[tuple[int, int, str]]) -> str:
    out, at = [], 0
    for lo, hi, new in sorted(edits):
        if lo < at:
            raise AssertionError(f"overlapping edits at {lo}")
        out.append(text[at:lo])
        out.append(new)
        at = hi
    out.append(text[at:])
    return "".join(out)


def balanced(masked: str, open_at: int) -> int:
    """The index of the `)` closing the `(` at `open_at`."""
    depth = 0
    for i in range(open_at, len(masked)):
        if masked[i] == "(":
            depth += 1
        elif masked[i] == ")":
            depth -= 1
            if depth == 0:
                return i
    raise ValueError("unbalanced parentheses")


def rewrite_bits(text: str, salsa_id: bool, had_salsa: bool, left: list[Left]) -> str:
    masked, _ = mask(text)
    edits: list[tuple[int, int, str]] = []
    for m in FROM_BITS.finditer(masked):
        if m.group(1) == "Id" and not salsa_id:
            continue
        open_at = m.end() - 1
        close = balanced(masked, open_at)
        arg = text[open_at + 1:close]
        line = line_of(text, m.start())
        if "<<" in masked[open_at:close]:
            left.append(Left(line, "hand", "`Id::from_bits` with generation bits: blaze_query IDs have no generation"))
            continue
        u64 = U64_FROM.match(arg.strip())
        if u64:
            arg = u64.group(1) + (u64.group(2) or "")
        elif not re.fullmatch(r"\s*[\d_]+\s*", arg):
            left.append(Left(line, "hand", "`Id::from_u32` now takes a `u32`: check the argument's type"))
        edits.append((m.start(), close + 1, f"{m.group(1)}::from_u32({arg})"))
    for m in AS_ID_BITS.finditer(masked):
        end = m.end()
        edits.append((end - len(".as_bits()"), end, ".as_u32()"))
        left.append(Left(line_of(text, m.start()), "hand", "`as_u32()` is a `u32` where `as_bits()` was a `u64`: check the use"))
    done = {e[0] for e in edits}
    if had_salsa:
        for m in AS_BITS.finditer(masked):
            if m.start() not in done:
                left.append(Left(line_of(text, m.start()), "hand", "`.as_bits()`: on a salsa `Id` it is `.as_u32()` (a `u32`)"))
    return apply(text, edits)


def rewrite_value(text: str, masked: str, edits: list[tuple[int, int, str]], taken: list[tuple[int, int]]) -> None:
    """Bare `SalsaValue` (the file imported it) -> the trait, or the derive inside `derive(..)`."""
    for m in re.finditer(r"(?<![\w:])SalsaValue\b", masked):
        if any(lo <= m.start() < hi for lo, hi in taken):
            continue
        edits.append((m.start(), m.end(), "blaze_query::Value" if in_derive(masked, m.start()) else "blaze_query::db::Value"))


def hint(path: str) -> str:
    """What a salsa path this script leaves becomes, by its longest known prefix."""
    segs = path.split("::")
    if segs[0] == "Database" and len(segs) > 1 and segs[1] in DB_METHODS:
        return DB_METHODS[segs[1]]
    for n in range(len(segs), 0, -1):
        if (it := HINTS.get("::".join(segs[:n]))) is not None:
            return it
    return "no blaze_query form: see the cases file"


def drop_revisions(text: str) -> str:
    """`revisions = ..` out of each `#[blaze_query::{tracked,input,interned}(..)]`: there is no GC."""
    masked, _ = mask(text)
    edits: list[tuple[int, int, str]] = []
    for m in ATTR.finditer(masked):
        open_at = m.end() - 1
        close = balanced(masked, open_at)
        items, depth, start = [], 0, open_at + 1
        for i in range(open_at + 1, close + 1):
            ch = masked[i]
            if ch in "([{":
                depth += 1
            elif ch in ")]}" and depth:
                depth -= 1
            elif (ch == "," and depth == 0) or i == close:
                items.append((start, i))
                start = i + 1
        kept = [(lo, hi) for lo, hi in items if not REVISIONS.match(masked[lo:hi]) and masked[lo:hi].strip()]
        if len(kept) == len([it for it in items if masked[it[0]:it[1]].strip()]):
            continue
        if kept:
            edits.append((open_at + 1, close, ", ".join(text[lo:hi].strip() for lo, hi in kept)))
        else:
            edits.append((open_at, close + 1, ""))
    return apply(text, edits)


# span's optional engine feature, `salsa` before and `query` now (tt and the proc-macro crates
# take span without it).
FEATURE = re.compile(r'\bfeature\s*=\s*"salsa"')


def migrate(text: str) -> FileResult:
    masked, _ = mask(text)
    edits: list[tuple[int, int, str]] = []
    use_spans = [(m.start(), masked.index(";", m.end())) for m in USE.finditer(masked)]
    # Hand cases are read off the rewritten text below, so their lines are the new file's.
    salsa_id, imports_value = rewrite_uses(text, masked, edits, [])
    rewrite_paths(text, masked, edits, [], use_spans)
    if imports_value:
        rewrite_value(text, masked, edits, use_spans)
    had_salsa = PATH.search(masked) is not None or bool(use_spans)
    new = apply(text, edits)
    left: list[Left] = []
    new = drop_revisions(new)
    new = rewrite_bits(new, salsa_id, had_salsa, left)
    new = FEATURE.sub('feature = "query"', new)
    left.extend(scan(new, had_salsa or "blaze_query::db" in new))
    return FileResult(new, left)


def scan(new: str, shapes: bool) -> list[Left]:
    """Every salsa `new` still names; with `shapes`, also the fork's API shapes the contract
    changed (`salsa::Database` methods, `ReadKind`, ..), which only a mechanical rewrite's
    output is read for: after the hand edits, the contract's own `synthetic_write` is no case."""
    left: list[Left] = []
    masked, spans = mask(new)
    named: set[int] = set()
    for m in USE.finditer(masked):
        named.update(range(m.start(), m.end()))
        left.append(Left(line_of(new, m.start()), "hand", "a `use salsa::` this script left (no blaze_query form, or a comment in the tree)"))
    for m in PATH.finditer(masked):
        if m.start() in named:
            continue
        named.update(range(m.start(), m.end()))
        left.append(Left(line_of(new, m.start()), "hand", f"`salsa::{m.group(1)}`: {hint(m.group(1))}"))
    for m in LEFT.finditer(masked):
        line = line_of(new, m.start())
        if m.start() not in named and not any(it.line == line and it.kind == "hand" for it in left):
            left.append(Left(line, "hand", "salsa named in code"))
    for lo, hi, kind in spans:
        if new[lo:hi] == '"salsa"':
            left.append(Left(line_of(new, lo), "hand", "the `salsa` cargo feature"))
        elif LEFT.search(new, lo, hi):
            left.append(Left(line_of(new, lo), kind, f"salsa in a {kind}"))
    if shapes:
        # A call on the storage (`db.storage().stop_readers()`) is the contract's own method.
        for m in re.finditer(r"(?<!storage\(\))(?<!storage_mut\(\))(?<!\.storage)\.(\w+)\(", masked):
            if m.group(1) in DB_METHODS:
                left.append(Left(line_of(new, m.start()), "hand", f"`salsa::Database::{m.group(1)}`: {DB_METHODS[m.group(1)]}"))
        for m in EVENT.finditer(masked):
            if m.group(1) not in EVENTS:
                left.append(Left(line_of(new, m.start()), "hand", f"`EventKind::{m.group(1)}` is not in the contract"))
        for m in READ_KIND.finditer(masked):
            if m.group(1) not in READ_KINDS:
                left.append(Left(line_of(new, m.start()), "hand", f"`ReadKind::{m.group(1)}`: the contract's are {', '.join(sorted(READ_KINDS))}"))
        for m in CANCELLED.finditer(masked):
            if m.group(1) not in CANCELS:
                left.append(Left(line_of(new, m.start()), "hand", f"`Cancelled::{m.group(1)}` is not in the contract"))
        for m in FOR_EACH_READ.finditer(masked):
            left.append(Left(line_of(new, m.start()), "hand", "`for_each_read(storage, |read: Read| ..)`: one `Read { key, kind }`, the name from `ingredient_name`"))
        for m in INDEX.finditer(masked):
            left.append(Left(line_of(new, m.start()), "hand", "`Id::index()` is `as_u32() - 1`"))
    return left


def migrate_manifest(text: str) -> tuple[str, list[Left]]:
    lines = text.split("\n")
    left: list[Left] = []
    table = ""
    tables: dict[str, list[str]] = {}
    for line in lines:
        if t := TABLE.match(line):
            table = t.group(1)
        tables.setdefault(table, []).append(line)
    out = []
    table = ""
    for i, line in enumerate(lines, 1):
        if t := TABLE.match(line):
            table = t.group(1)
        if MANIFEST_DEP.match(line):
            has = any(re.match(r"^blaze-query\s*[.=]", it) for it in tables[table])
            if not has:
                out.append("blaze-query.workspace = true")
            continue
        if LEFT.search(line):
            left.append(Left(i, "hand", "salsa in the manifest"))
        out.append(line)
    return "\n".join(out), left


def ported(rel: str) -> bool:
    """Whether the port reads `rel` (relative to its crate): its manifest, its Rust sources and
    the parser's expected trees of them."""
    parts = rel.split("/")
    return "target" not in parts and (rel == "Cargo.toml" or rel.endswith((".rs", ".rast")))


def mechanical(rel: str, text: str) -> tuple[str, list[Left]]:
    if rel.endswith("Cargo.toml"):
        return migrate_manifest(text)
    if rel.endswith(".rast"):
        return text, []
    result = migrate(text)
    return result.text, result.left


def git_files(root: Path, base: str, crate: str) -> dict[str, str]:
    """`crate`'s ported files at `base`, by path relative to the root (read-only git)."""
    names = subprocess.run(
        ["git", "-C", str(root), "ls-tree", "-r", "--name-only", base, "--", crate + "/"],
        check=True, capture_output=True, text=True,
    ).stdout.split()
    names = [it for it in names if ported(it[len(crate) + 1:])]
    out = subprocess.run(
        ["git", "-C", str(root), "cat-file", "--batch"],
        input="".join(f"{base}:{it}\n" for it in names).encode(), check=True, capture_output=True,
    ).stdout
    files, at = {}, 0
    for name in names:
        header_end = out.index(b"\n", at)
        size = int(out[at:header_end].split()[2])
        files[name] = out[header_end + 1:header_end + 1 + size].decode()
        at = header_end + 1 + size + 1
    return files


def work_files(root: Path, crate: str) -> dict[str, str]:
    base = root / crate
    return {
        p.relative_to(root).as_posix(): p.read_text()
        for p in sorted(base.rglob("*"))
        if p.is_file() and ported(p.relative_to(base).as_posix())
    }


def occurrences(text: str, old: str) -> int:
    """How often `old` occurs in `text`, overlapping ones counted (`str.count` skips them)."""
    n, at = 0, text.find(old)
    while at >= 0:
        n += 1
        at = text.find(old, at + 1)
    return n


def hand_edits(before: str, after: str) -> list[dict[str, str]]:
    """`after` as exact replacements on `before`, applied in order: each `old` is whole lines,
    widened by context until it occurs once in the text it is applied to."""
    a, b = before.splitlines(keepends=True), after.splitlines(keepends=True)
    cur, shift, edits = list(a), 0, []
    for tag, i1, i2, j1, j2 in difflib.SequenceMatcher(None, a, b, autojunk=False).get_opcodes():
        if tag == "equal":
            continue
        lo0, hi0 = i1 + shift, i2 + shift
        text = "".join(cur)
        for c in range(len(cur) + 1):
            lo, hi = max(0, lo0 - c), min(len(cur), hi0 + c)
            old = "".join(cur[lo:hi])
            if old and occurrences(text, old) == 1:
                break
        new_lines = cur[lo:lo0] + b[j1:j2] + cur[hi0:hi]
        edits.append({"old": old, "new": "".join(new_lines)})
        cur[lo:hi] = new_lines
        shift += (j2 - j1) - (i2 - i1)
    return edits


def replay(rel: str, text: str, edits: list[dict[str, str]]) -> str:
    for n, edit in enumerate(edits):
        if (found := occurrences(text, edit["old"])) != 1:
            fail(SCRIPT, f"{rel}: hand edit {n} no longer applies once (found {found} times); "
                 "redo it on this base and record again")
        text = text.replace(edit["old"], edit["new"])
    return text


def hand_path(hand: str, root: Path, crate: str) -> Path:
    return root / hand / f"{Path(crate).name}.json"


def record(cfg: Input, root: Path) -> None:
    """Writes each crate's hand edits: the working files against the mechanical rewrite of
    `base`'s, so a replay on `base` gives the working files byte for byte."""
    for crate in cfg.crates:
        before, after = git_files(root, cfg.base, crate), work_files(root, crate)
        files: list[dict[str, object]] = []
        for rel in sorted(set(before) | set(after)):
            if rel not in after:
                files.append({"path": rel, "delete": True})
            elif rel not in before:
                files.append({"path": rel, "create": after[rel]})
            else:
                mech = mechanical(rel, before[rel])[0]
                if mech != after[rel]:
                    edits = hand_edits(mech, after[rel])
                    if replay(rel, mech, edits) != after[rel]:
                        fail(SCRIPT, f"{rel}: its recorded edits don't give the working file")
                    files.append({"path": rel, "edits": edits})
        path = hand_path(cfg.hand, root, crate)
        if files:
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(json.dumps({"crate": crate, "base": cfg.base, "files": files}, indent=1) + "\n")
        elif path.exists():
            path.unlink()
        edits = sum(len(it.get("edits", [])) or 1 for it in files)
        print(f"{crate}: {len(files)} files, {edits} hand edits -> {path.relative_to(root)}")


def main() -> None:
    match sys.argv[1:]:
        case [text]:
            cfg = parse(Input, text, SCRIPT)
        case _:
            print(f"usage: {SCRIPT} '{{\"root\": \".\", \"crates\": [\"crates/span\"], \"write\": false}}'", file=sys.stderr)
            sys.exit(2)
    root = Path(cfg.root)
    if not cfg.crates:
        fail(SCRIPT, "crates is empty")
    for crate in cfg.crates:
        if not (root / crate / "Cargo.toml").is_file():
            fail(SCRIPT, f"{crate}: no Cargo.toml under {root / crate}")
    if cfg.record:
        if not cfg.base or not cfg.hand or cfg.write:
            fail(SCRIPT, "record takes base and hand, and no write")
        base = subprocess.run(["git", "-C", str(root), "rev-parse", "--verify", cfg.base + "^{commit}"],
                              check=True, capture_output=True, text=True).stdout.strip()
        record(replace(cfg, base=base), root)
        return
    if cfg.hand and not cfg.base:
        fail(SCRIPT, "hand edits replay on a base: give base")
    totals: dict[str, dict[str, int]] = {}
    report: list[str] = []
    for crate in cfg.crates:
        counts = totals.setdefault(crate, {"files": 0, "hand": 0, "string": 0, "comment": 0})
        sources = git_files(root, cfg.base, crate) if cfg.base else work_files(root, crate)
        recorded: dict[str, dict[str, object]] = {}
        if cfg.hand and (path := hand_path(cfg.hand, root, crate)).exists():
            recorded = {it["path"]: it for it in json.loads(path.read_text())["files"]}
        for rel in sorted(set(sources) | set(recorded)):
            entry = recorded.get(rel, {})
            target = root / rel
            if entry.get("delete"):
                counts["files"] += 1
                if cfg.write and target.exists():
                    target.unlink()
                continue
            if "create" in entry:
                new, left = str(entry["create"]), scan(str(entry["create"]), False)
            else:
                new, left = mechanical(rel, sources[rel])
                if "edits" in entry:
                    new = replay(rel, new, entry["edits"])  # type: ignore[arg-type]
                    left = scan(new, False) if not rel.endswith("Cargo.toml") else migrate_manifest(new)[1]
            old = target.read_text() if target.exists() else ""
            if new != old:
                counts["files"] += 1
                if cfg.write:
                    target.parent.mkdir(parents=True, exist_ok=True)
                    target.write_text(new)
                else:
                    sys.stdout.writelines(difflib.unified_diff(
                        old.splitlines(keepends=True), new.splitlines(keepends=True), f"a/{rel}", f"b/{rel}"))
            lines = new.split("\n")
            for it in sorted(left, key=lambda it: (it.line, it.kind)):
                counts[it.kind] += 1
                report.append(f"{rel}:{it.line}: [{it.kind}] {it.why}: {lines[it.line - 1].strip()}")
    print("\n# left by salsa_to_query.py")
    for line in report:
        print(line)
    print("\n# per crate: files changed, hand, string, comment")
    for crate, c in totals.items():
        print(f"{crate}: {c['files']} changed, {c['hand']} hand, {c['string']} string, {c['comment']} comment")
    sys.exit(1 if report else 0)


if __name__ == "__main__":
    main()
