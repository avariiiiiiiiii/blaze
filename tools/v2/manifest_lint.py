"""Checks the v2 workspace's manifests without building anything: the one check every v2 agent may run.

Input: `{"root": "<workspace>"}` (inline, a path, or `-`). Prints one line per problem and exits 1
when there is any:

- a `workspace = true` dependency the root `Cargo.toml` doesn't declare;
- a `path` dependency (in the root or a crate) whose directory has no `Cargo.toml`;
- a workspace member with no target file (`src/lib.rs`, `src/main.rs` or a `path =`).
"""

import sys
import tomllib
from dataclasses import dataclass
from pathlib import Path
from typing import Any

from typed_input import parse

SCRIPT = "manifest_lint.py"
DEP_TABLES = ("dependencies", "dev-dependencies", "build-dependencies")


@dataclass(frozen=True)
class Input:
    root: str


def dep_tables(manifest: dict[str, Any]) -> list[dict[str, Any]]:
    tables = [manifest.get(k, {}) for k in DEP_TABLES]
    for target in manifest.get("target", {}).values():
        tables.extend(target.get(k, {}) for k in DEP_TABLES)
    return tables


def has_target(crate: Path, manifest: dict[str, Any]) -> bool:
    if (crate / "src/lib.rs").is_file() or (crate / "src/main.rs").is_file():
        return True
    if "path" in manifest.get("lib", {}):
        return True
    return any("path" in b for b in manifest.get("bin", []))


def lint(root: Path) -> list[str]:
    problems: list[str] = []
    root_manifest = tomllib.loads((root / "Cargo.toml").read_text())
    workspace = root_manifest["workspace"]
    declared: dict[str, Any] = workspace.get("dependencies", {})
    for name, spec in declared.items():
        if isinstance(spec, dict) and "path" in spec and not (root / spec["path"] / "Cargo.toml").is_file():
            problems.append(f"Cargo.toml: workspace dependency `{name}` points at `{spec['path']}`, which has no Cargo.toml")
    excluded = {(root / e).resolve() for e in workspace.get("exclude", [])}
    members = [c for pattern in workspace.get("members", []) for c in sorted(root.glob(pattern))]
    for crate in members:
        if crate.resolve() in excluded or not (crate / "Cargo.toml").is_file():
            continue
        rel = crate.relative_to(root)
        try:
            manifest = tomllib.loads((crate / "Cargo.toml").read_text())
        except tomllib.TOMLDecodeError as e:
            problems.append(f"{rel}/Cargo.toml: does not parse: {e}")
            continue
        if not has_target(crate, manifest):
            problems.append(f"{rel}: no target file (src/lib.rs, src/main.rs or a `path =`)")
        for table in dep_tables(manifest):
            for name, spec in table.items():
                if not isinstance(spec, dict):
                    continue
                if spec.get("workspace") and name not in declared:
                    problems.append(f"{rel}/Cargo.toml: `{name}` uses `workspace = true` but the root doesn't declare it")
                if "path" in spec and not (crate / spec["path"] / "Cargo.toml").is_file():
                    problems.append(f"{rel}/Cargo.toml: `{name}` points at `{spec['path']}`, which has no Cargo.toml")
    return problems


def main() -> None:
    match sys.argv[1:]:
        case [text]:
            problems = lint(Path(parse(Input, text, SCRIPT).root))
        case _:
            print(f"usage: {SCRIPT} '{{\"root\": \"<workspace>\"}}'", file=sys.stderr)
            sys.exit(2)
    for p in problems:
        print(p)
    sys.exit(1 if problems else 0)


if __name__ == "__main__":
    main()
