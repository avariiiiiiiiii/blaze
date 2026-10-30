"""Finds duplicated and similar Rust code across a workspace's crates.

Input, one JSON object (inline, a path, or `-`): `{"root": ".", "out": "target/similar"}` plus any
other field of `Input`.

Every function, method, struct, enum, union, trait and `macro_rules!` outside test code is a
chunk (tree-sitter); a function longer than `max_lines` is also cut into windows of whole
statements. Three passes:

1. Clones: chunks with the same token sequence once identifiers and literals are blanked, so a
   copy with renamed variables still matches.
2. Near duplicates: each chunk's code embedding (on the GPU when there is one) and its nearest
   neighbours at cosine `threshold` or more, joined into groups.
3. Concepts: spherical k-means over the embeddings. A tight cluster whose members sit in
   `min_crates` crates or more is one idea written several times.

Writes `report.html` and `report.json` to `out`. Embeddings are kept in `out/cache.npz` by
content and model, so a rerun embeds only the chunks that changed.
"""

import hashlib
import html
import json
import os
import re
import sys
import time
from collections import Counter, defaultdict
from concurrent.futures import ProcessPoolExecutor
from dataclasses import asdict, dataclass
from pathlib import Path
from typing import Literal

import numpy as np
import tree_sitter_rust
from numpy.typing import NDArray
from tree_sitter import Language, Node, Parser
from typed_input import fail, parse

SCRIPT = "similar"
LANGUAGE = Language(tree_sitter_rust.language())
IDENTS = {"identifier", "field_identifier", "type_identifier", "shorthand_field_identifier", "label"}
LITERALS = {"integer_literal", "float_literal", "string_content", "char_literal", "boolean_literal"}
COMMENTS = {"line_comment", "block_comment", "doc_comment"}
WHOLE = {"struct_item", "enum_item", "union_item", "trait_item", "macro_definition"}
TEST_ATTR = re.compile(r"#\[\s*(cfg\s*\(\s*test\s*\)|(\w+::)*(test|bench)\b)")


@dataclass(frozen=True)
class Input:
    root: str
    out: str = "target/similar"
    dirs: tuple[str, ...] = ("crates",)
    # Only these crates when given (directory names).
    crates: tuple[str, ...] = ()
    # Crates left out: deleted at integration, or not ours.
    skip: tuple[str, ...] = ("ide", "ide-assists", "ide-completion", "ide-db", "ide-diagnostics",
                             "ide-ssr", "rust-analyzer", "test-fixture", "test-utils")
    model: str = "Alibaba-NLP/gte-modernbert-base"
    device: Literal["auto", "mps", "cuda", "cpu"] = "auto"
    max_tokens: int = 512
    batch: int = 64
    min_lines: int = 6
    max_lines: int = 80
    clone_tokens: int = 60
    threshold: float = 0.95
    neighbours: int = 10
    topic_size: int = 25
    min_crates: int = 3
    cohesion: float = 0.85
    shown: int = 300


@dataclass(frozen=True)
class Chunk:
    crate: str
    path: str
    start: int
    end: int
    kind: str
    name: str
    text: str
    shape: str
    tokens: int

    @property
    def lines(self) -> int:
        return self.end - self.start + 1


def is_test(node: Node) -> bool:
    sibling = node.prev_sibling
    while sibling is not None and sibling.type in {"attribute_item", *COMMENTS}:
        if sibling.type == "attribute_item" and TEST_ATTR.search(sibling.text.decode(errors="ignore") if sibling.text else ""):
            return True
        sibling = sibling.prev_sibling
    return False


def shape(node: Node) -> tuple[str, int]:
    """The node's leaf tokens with identifiers and literals blanked, hashed, and their count."""
    tokens: list[str] = []
    stack = [node]
    while stack:
        it = stack.pop()
        if it.type in COMMENTS:
            continue
        if it.child_count == 0:
            tokens.append("I" if it.type in IDENTS else "L" if it.type in LITERALS else it.type)
        else:
            stack.extend(reversed(it.children))
    digest = hashlib.blake2b("\x1f".join(tokens).encode(), digest_size=12).hexdigest()
    return digest, len(tokens)


def name_of(node: Node) -> str:
    named = node.child_by_field_name("name")
    return named.text.decode(errors="ignore") if named is not None and named.text else node.type


def chunks_of(args: tuple[str, str, str, int, int]) -> list[Chunk]:
    path, rel, crate, min_lines, max_lines = args
    source = Path(path).read_bytes()
    tree = Parser(LANGUAGE).parse(source)
    out: list[Chunk] = []

    def emit(node: Node, kind: str, name: str, first: Node | None = None, last: Node | None = None) -> None:
        a, b = first or node, last or node
        start, end = a.start_point[0] + 1, b.end_point[0] + 1
        if end - start + 1 < min_lines:
            return
        text = source[a.start_byte:b.end_byte].decode(errors="ignore")
        if first is None:
            digest, count = shape(node)
        else:
            parts = [shape(n) for n in _between(a, b)]
            digest = hashlib.blake2b("".join(d for d, _ in parts).encode(), digest_size=12).hexdigest()
            count = sum(c for _, c in parts)
        out.append(Chunk(crate, rel, start, end, kind, name, text, digest, count))

    def function(node: Node) -> None:
        name = name_of(node)
        emit(node, "fn", name)
        body = node.child_by_field_name("body")
        if body is None or node.end_point[0] - node.start_point[0] + 1 <= max_lines:
            return
        window: list[Node] = []
        part = 0
        for statement in body.named_children:
            if window and statement.end_point[0] - window[0].start_point[0] + 1 > max_lines:
                emit(node, "fn-part", f"{name}#{part}", window[0], window[-1])
                part += 1
                window = []
            window.append(statement)
        if window and part:
            emit(node, "fn-part", f"{name}#{part}", window[0], window[-1])

    stack = [tree.root_node]
    while stack:
        node = stack.pop()
        if node.type in {"mod_item", "function_item", "impl_item", "trait_item"} and is_test(node):
            continue
        if node.type == "function_item":
            function(node)
            continue
        if node.type in WHOLE:
            emit(node, node.type.removesuffix("_item").removesuffix("_definition"), name_of(node))
            if node.type != "trait_item":
                continue
        stack.extend(reversed(node.named_children))
    return out


def _between(first: Node, last: Node) -> list[Node]:
    nodes = [first]
    while nodes[-1] != last and nodes[-1].next_named_sibling is not None:
        nodes.append(nodes[-1].next_named_sibling)
    return nodes


def collect(inp: Input, root: Path) -> list[Chunk]:
    jobs: list[tuple[str, str, str, int, int]] = []
    for top in inp.dirs:
        for crate_dir in sorted((root / top).iterdir()):
            if not crate_dir.is_dir() or crate_dir.name in inp.skip or (inp.crates and crate_dir.name not in inp.crates):
                continue
            for dirpath, dirnames, filenames in os.walk(crate_dir):
                dirnames[:] = [d for d in dirnames if d not in {"tests", "target", "test_data", "fixtures"}]
                for f in filenames:
                    if f.endswith(".rs") and f != "tests.rs" and not f.endswith("_tests.rs"):
                        p = Path(dirpath) / f
                        jobs.append((str(p), str(p.relative_to(root)), crate_dir.name, inp.min_lines, inp.max_lines))
    with ProcessPoolExecutor() as pool:
        return [c for cs in pool.map(chunks_of, jobs, chunksize=32) for c in cs]


def device_of(choice: str) -> str:
    import torch

    if choice != "auto":
        return choice
    if torch.cuda.is_available():
        return "cuda"
    return "mps" if torch.backends.mps.is_available() else "cpu"


def embed(inp: Input, chunks: list[Chunk], out: Path, device: str) -> NDArray[np.float32]:
    keys = [hashlib.blake2b(f"{inp.model}\x00{inp.max_tokens}\x00{c.text}".encode(), digest_size=12).hexdigest() for c in chunks]
    cache_path = out / "cache.npz"
    cached: dict[str, NDArray[np.float32]] = {}
    if cache_path.exists():
        with np.load(cache_path) as saved:
            cached = dict(zip(saved["keys"].tolist(), saved["vectors"], strict=True))
    missing = sorted({k for k in keys if k not in cached})
    log(f"embedding {len(missing)} new texts for {len(chunks)} chunks on {device}")
    if missing:
        from sentence_transformers import SentenceTransformer

        model = SentenceTransformer(inp.model, device=device, trust_remote_code=False)
        model.max_seq_length = inp.max_tokens
        text_of = {k: c.text for k, c in zip(keys, chunks, strict=True)}
        vectors = model.encode([text_of[k] for k in missing], batch_size=inp.batch, normalize_embeddings=True,
                               convert_to_numpy=True, show_progress_bar=True)
        cached.update(zip(missing, np.asarray(vectors, dtype=np.float32), strict=True))
        live = sorted(set(keys))
        np.savez(cache_path, keys=np.array(live), vectors=np.stack([cached[k] for k in live]))
    return np.stack([cached[k] for k in keys]).astype(np.float32)


def overlaps(a: Chunk, b: Chunk) -> bool:
    return a.path == b.path and a.start <= b.end and b.start <= a.end


class Groups:
    def __init__(self, n: int) -> None:
        self.parent = list(range(n))

    def find(self, i: int) -> int:
        while self.parent[i] != i:
            self.parent[i] = self.parent[self.parent[i]]
            i = self.parent[i]
        return i

    def join(self, a: int, b: int) -> None:
        self.parent[self.find(a)] = self.find(b)

    def sets(self, members: set[int]) -> list[list[int]]:
        by: dict[int, list[int]] = defaultdict(list)
        for i in sorted(members):
            by[self.find(i)].append(i)
        return [g for g in by.values() if len(g) > 1]


def clones(inp: Input, chunks: list[Chunk]) -> list[list[int]]:
    by: dict[str, list[int]] = defaultdict(list)
    for i, c in enumerate(chunks):
        if c.tokens >= inp.clone_tokens:
            by[c.shape].append(i)
    groups: list[list[int]] = []
    for members in by.values():
        kept: list[int] = []
        for i in members:
            if not any(overlaps(chunks[i], chunks[j]) for j in kept):
                kept.append(i)
        if len(kept) > 1:
            groups.append(kept)
    return groups


def near(inp: Input, chunks: list[Chunk], vectors: NDArray[np.float32], device: str) -> tuple[list[list[int]], dict[tuple[int, int], float]]:
    import torch

    e = torch.from_numpy(vectors).to(device)
    k = min(inp.neighbours + 1, len(chunks))
    groups = Groups(len(chunks))
    joined: set[int] = set()
    score: dict[tuple[int, int], float] = {}
    for lo in range(0, len(chunks), 2048):
        sims, idx = torch.topk(e[lo:lo + 2048] @ e.T, k, dim=1)
        for row, (ss, js) in enumerate(zip(sims.cpu().tolist(), idx.cpu().tolist(), strict=True)):
            i = lo + row
            for s, j in zip(ss, js, strict=True):
                if s < inp.threshold:
                    break
                if j == i or overlaps(chunks[i], chunks[j]):
                    continue
                groups.join(i, j)
                joined.update((i, j))
                score[(min(i, j), max(i, j))] = s
    return groups.sets(joined), score


def concepts(inp: Input, chunks: list[Chunk], vectors: NDArray[np.float32], device: str) -> list[tuple[list[int], float]]:
    import torch

    n = len(chunks)
    k = max(2, n // inp.topic_size)
    e = torch.from_numpy(vectors).to(device)
    g = torch.Generator().manual_seed(0)
    centroids = e[torch.randperm(n, generator=g)[:k].to(device)].clone()
    assign = torch.zeros(n, dtype=torch.long, device=device)
    for _ in range(30):
        assign = torch.cat([(e[lo:lo + 8192] @ centroids.T).argmax(dim=1) for lo in range(0, n, 8192)])
        sums = torch.zeros_like(centroids).index_add_(0, assign, e)
        counts = torch.bincount(assign, minlength=k).unsqueeze(1)
        fresh = torch.nn.functional.normalize(sums / counts.clamp(min=1), dim=1)
        centroids = torch.where(counts > 0, fresh, centroids)
    cos = (e * centroids[assign]).sum(dim=1).cpu().numpy()
    by: dict[int, list[int]] = defaultdict(list)
    for i, a in enumerate(assign.cpu().tolist()):
        by[a].append(i)
    out: list[tuple[list[int], float]] = []
    for members in by.values():
        crates = {chunks[i].crate for i in members}
        tight = float(np.mean(cos[members]))
        if len(crates) >= inp.min_crates and tight >= inp.cohesion:
            out.append((members, tight))
    return out


def words(chunks: list[Chunk], members: list[int]) -> str:
    seen: Counter[str] = Counter()
    for i in members:
        name = re.sub(r"#\d+$", "", chunks[i].name)
        seen.update({w.lower() for w in re.findall(r"[A-Z]?[a-z]+|[A-Z]+(?![a-z])|\d+", name) if len(w) > 2})
    return " ".join(w for w, _ in seen.most_common(4)) or "(unnamed)"


def saved_lines(chunks: list[Chunk], members: list[int]) -> int:
    lines = [chunks[i].lines for i in members]
    return sum(lines) - max(lines)


@dataclass(frozen=True)
class Group:
    kind: str
    title: str
    saved: int
    crates: list[str]
    score: float
    members: list[int]


def report(inp: Input, root: Path, out: Path, chunks: list[Chunk], groups: list[Group]) -> None:
    def member(i: int) -> dict[str, object]:
        c = chunks[i]
        return {"crate": c.crate, "path": c.path, "start": c.start, "end": c.end, "kind": c.kind, "name": c.name}

    (out / "report.json").write_text(json.dumps(
        {"input": asdict(inp), "chunks": len(chunks),
         "groups": [{**asdict(g), "members": [member(i) for i in g.members]} for g in groups]}, indent=1))

    def link(c: Chunk) -> str:
        where = f"{c.path}:{c.start}-{c.end}"
        return f'<a href="cursor://file/{html.escape(str(root / c.path))}:{c.start}">{html.escape(where)}</a>'

    def code(c: Chunk) -> str:
        lines = c.text.splitlines()
        more = f"\n… {len(lines) - 60} more lines" if len(lines) > 60 else ""
        return f"<pre>{html.escape(chr(10).join(lines[:60]) + more)}</pre>"

    sections: dict[str, list[str]] = defaultdict(list)
    counts: Counter[str] = Counter()
    for g in groups:
        counts[g.kind] += 1
        if len(sections[g.kind]) >= inp.shown:
            continue
        rows = "".join(
            f"<li><span class=crate>{html.escape(chunks[i].crate)}</span> {link(chunks[i])} "
            f"<code>{html.escape(chunks[i].name)}</code></li>" for i in g.members[:40])
        if len(g.members) > 40:
            rows += f"<li>… {len(g.members) - 40} more</li>"
        shown = "".join(code(chunks[i]) for i in g.members[:2])
        sections[g.kind].append(
            f"<section><h3>{html.escape(g.title)} <small>{len(g.members)} copies, {len(g.crates)} crates, "
            f"~{g.saved} lines, score {g.score:.3f}</small></h3><ul>{rows}</ul>"
            f"<details><summary>code</summary><div class=pair>{shown}</div></details></section>")
    heads = {"clone": "Clones: same code with names changed",
             "near": f"Near duplicates: embeddings at cosine ≥ {inp.threshold}",
             "concept": f"Concepts written in {inp.min_crates}+ crates (k-means, cohesion ≥ {inp.cohesion})"}
    body = "".join(
        f"<h2 id={kind}>{heads[kind]} <small>({counts[kind]})</small></h2>" + "".join(sections[kind])
        for kind in heads)
    total = sum(g.saved for g in groups if g.kind != "concept")
    (out / "report.html").write_text(f"""<!doctype html><meta charset=utf-8><title>similar code</title>
<style>
body{{font:14px/1.45 -apple-system,system-ui,sans-serif;max-width:1400px;margin:2em auto;padding:0 1em;color:#222}}
h2{{border-bottom:2px solid #ddd;padding-top:1em}} h3{{margin:.2em 0;font-size:15px}} small{{color:#777;font-weight:normal}}
section{{border:1px solid #e3e3e3;border-radius:6px;padding:.6em 1em;margin:.7em 0}}
ul{{margin:.3em 0;padding-left:1.2em}} .crate{{display:inline-block;min-width:11em;color:#555}}
a{{color:#0b62c4;text-decoration:none}} .pair{{display:grid;grid-template-columns:1fr 1fr;gap:.6em}}
pre{{background:#f7f7f7;padding:.6em;overflow:auto;font-size:12px;margin:0}}
@media (prefers-color-scheme:dark){{body{{background:#161616;color:#ddd}}section{{border-color:#333}}pre{{background:#202020}}a{{color:#6aa9ff}}}}
</style>
<h1>Similar code in {html.escape(str(root))}</h1>
<p>{len(chunks)} chunks from {len({c.crate for c in chunks})} crates. Clones and near duplicates
cover about {total} removable lines. Model <code>{html.escape(inp.model)}</code>.
<a href=#clone>clones ({counts['clone']})</a> · <a href=#near>near ({counts['near']})</a> ·
<a href=#concept>concepts ({counts['concept']})</a></p>{body}""")


def log(message: str) -> None:
    print(f"[{time.strftime('%H:%M:%S')}] {message}", file=sys.stderr, flush=True)


def main() -> None:
    if len(sys.argv) != 2:
        fail(SCRIPT, "takes one JSON input: inline, a path, or -")
    inp = parse(Input, sys.argv[1], SCRIPT)
    root = Path(inp.root).resolve()
    out = (root / inp.out).resolve()
    out.mkdir(parents=True, exist_ok=True)

    chunks = collect(inp, root)
    log(f"{len(chunks)} chunks from {len({c.crate for c in chunks})} crates")
    if len(chunks) < 2:
        fail(SCRIPT, f"found {len(chunks)} chunks under {root}: nothing to compare")
    device = device_of(inp.device)
    vectors = embed(inp, chunks, out, device)

    groups: list[Group] = []
    for members in clones(inp, chunks):
        groups.append(Group("clone", words(chunks, members), saved_lines(chunks, members),
                            sorted({chunks[i].crate for i in members}), 1.0, members))
    near_groups, score = near(inp, chunks, vectors, device)
    group_of = {i: g for g, members in enumerate(near_groups) for i in members}
    inside: dict[int, list[float]] = defaultdict(list)
    for (a, _), s in score.items():
        inside[group_of[a]].append(s)
    for g, members in enumerate(near_groups):
        groups.append(Group("near", words(chunks, members), saved_lines(chunks, members),
                            sorted({chunks[i].crate for i in members}), float(np.mean(inside[g])), members))
    log(f"{sum(g.kind == 'clone' for g in groups)} clone groups, {len(near_groups)} near-duplicate groups")
    for members, tight in concepts(inp, chunks, vectors, device):
        crates = sorted({chunks[i].crate for i in members})
        groups.append(Group("concept", words(chunks, members), saved_lines(chunks, members), crates, tight, members))
    groups.sort(key=lambda g: (g.kind, -(g.saved if g.kind != "concept" else len(g.crates) * g.score)))
    report(inp, root, out, chunks, groups)
    log(f"wrote {out / 'report.html'}")
