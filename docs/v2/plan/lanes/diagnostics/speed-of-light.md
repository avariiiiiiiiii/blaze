---
status: plan
summary: lane diagnostics' speed of light: for each component, the bound its work and the hardware set, the arithmetic, how the design reaches 90% of it, and what stands in the way where it does not yet.
date: 2026-10-29
---
# Lane diagnostics: speed of light

The bar is "Within 10% of the theoretical maximum, everywhere" in
[principles.md](../../goals/principles.md). Each part of lane diagnostics has a section here,
written by the agent that owns it; a sibling adds its own section below rather than rewriting
the file.

The numbers use one core of the target machines in [hardware.md](../../design/hardware.md) at
about 3.5 GHz: an L1 hit in 4 to 5 cycles, L2 in about 14, DRAM at 20 to 30 GB/s, an IPC of
about 2 for branchy code and about 4 for straight-line integer code, and about 15 cycles per
mispredicted branch. Nothing here is measured; `proof`'s budgets measure it later. None of
these passes does I/O or makes a syscall: they read what the front end has already
computed, so every bound below is compute or memory.

## `dead_code` (`diagnostics-lints-names-dead`)

`crates/blaze-diagnostics/src/lints/names/dead/`: rustc's `rustc_passes/src/dead.rs`. It
numbers the crate's definitions (`Defs`), reads their lint levels (`level`), works out what is
reachable from outside (`reach`), seeds the worklist (`seed`), walks each live definition's
signature and body to mark what it names (`mark`), and reports what is left (`report`).

**The reference crate.** About 100,000 lines, which gives:

- about 800,000 syntax nodes, at about 8 per line;
- about 500,000 expression, pattern and type nodes in bodies, at about 5 per line;
- about 120,000 paths in bodies, plus about 60,000 method calls and field accesses;
- about 10,000 definitions, counting fields and variants;
- about 3,000 bodies.

Only the ratios matter: each cost below scales with one of these counts.

**The state of the art** is rustc's own pass. `MarkSymbolVisitor` walks HIR once per live
body. Each path in HIR already carries its `Res` from AST lowering, and type-relative paths,
methods and fields are one lookup in `TypeckResults`. Lint levels come from
`ShallowLintLevelMap`, one per owner. It runs on one thread. The bound below is that design
with nothing extra, which is also the most rustc can do.

### Bounds and where the pass stands

| Component | Work | Bound | Now |
| --- | --- | --- | --- |
| `Defs` | one visit per definition | about 0.6 ms | about 10 ms |
| `level` | one lookup per owner | about 0.15 ms | about 1.4 ms |
| `reach` | one visit per definition and per type in a public signature | the same as `items` | the same, maybe computed twice |
| `seed` | one attribute-bit test per definition | about 0.3 ms | about 0.6 ms |
| `mark` (bodies) | one visit per body node and one read per resolution | about 5.6 ms | about 20 ms (was about 150 ms) |
| `report` | one bit test per definition, text only for what fires | under 0.1 ms | at the bound, except that it runs when the lint can't fire |

The arithmetic behind each row follows.

**`Defs`: the crate's definitions, numbered.** The work is the definitions themselves:
10,000 of them at about 200 cycles each (a few memoized queries and one hash insert) is
2 million cycles, about 0.6 ms. Their ids, names and kinds are about 64 bytes each, 0.64 MB
read, well under a millisecond at DRAM bandwidth.

The pass is not there yet. `Defs::of` finds definitions by walking all the syntax, because
items in blocks and anonymous constants appear only there. That is 800,000 nodes at about 45
cycles each (a rowan cursor allocated and freed, plus a `SyntaxKind` dispatch): 36 million
cycles, about 10 ms, or 17 times the bound.

What removes it: enumerate items from hir-def's `ItemTree`, which name resolution has
already lowered, including each block's tree. Take anonymous constants and closures from the
expression store, which `mark` already walks. The syntax walk is then left with the
definitions' own nodes. This is a rewrite of `defs.rs` that this lane can do; I did not do it
blind, without a build. `Defs::in_anon` already skips the anonymous-constant work in a crate
that has none.

**`level`: lint levels.** The bound is rustc's: one map per owner and one lookup per query.
That is 10,000 definitions plus the closures and anonymous constants `mark` asks about, at
about 50 cycles each, about 0.15 ms.

Now: levels are memoized per syntax node that has attributes. Each query walks up to the
nearest memoized node, about 5 to 10 ancestors at about 50 cycles each, so about 500 cycles
per query, 5 million cycles, about 1.4 ms. What removes it: a lint-level column keyed by
`AttrDefId` and computed once per crate, as `ShallowLintLevelMap` is. That is for the lane
that owns `Cx`'s levels; this pass would read it unchanged through `Levels::at`.

**`reach`: what is reachable from outside.** The bound is rustc's
`effective_visibilities`, computed once per crate for every pass that reads it.

The fixpoint costs two or three bit tests per impl per round, and there are at most a few
rounds. Nominal visibility is one pass in definition order. Both are at the bound. What
stands in the way is that `items` may compute the same facts again, which halves the pass's
share of the bound. What removes it: `names::after_abort` passing one `Defs` and `Reach` to
both passes. That is requested of the parent in `v2/requests/diagnostics.md`.

**`seed`: the worklist's first entries.** The bound is one memoized `AttrFlags` read per
definition at about 100 cycles: 10,000 of them is 1 million cycles, about 0.3 ms.

Now: `rustc_main` and `rustc_canonical_symbol` still go through `AttrFlags::has_word`, which
reads the source. That is about 3,000 functions at about 300 cycles each, another 0.3 ms,
which puts this component at about half the bound. What removes it: the two `AttrFlags`
bits requested of db. With them, `has_word` becomes a bit test and this code doesn't change.

**`mark`: what bodies name.** This is the pass's main work, and the component this change
rewrote.

Before, bodies were walked over syntax and every name resolved through `hir::Semantics`. Each
`resolve_path`, `resolve_method_call`, `resolve_field` or `type_of_*` call rebuilt a
`SourceAnalyzer`:

- `find_container`: about 10 ancestors and dyn-map lookups, about 500 cycles;
- three memo fetches (`Body::with_source_map`, `ExprScopes::of`, `InferenceResult::of`),
  about 300 cycles;
- `scope_for`: about 10 ancestors, each hashed into the source map, about 600 cycles;
- a fresh `Resolver`: an allocation plus about 15 scopes, about 500 cycles;
- for a path, an `ExprCollector` and `lower_path` run again, about 1,000 cycles.

That is 2,000 to 3,000 cycles per call. 200,000 calls at about 2,500 cycles is 500 million
cycles, about 140 ms. A rowan cursor for each of about 600,000 body nodes at about 45 cycles
adds 27 million cycles, about 8 ms. The total was about 150 ms.

The bound is one visit per store node and one indexed read per resolution:

- **Visiting the store.** 500,000 nodes at about 35 cycles each: a column decode of about 10
  cycles, the kind dispatch with its share of a mispredict at about 15, and the visit at about
  10. That is 17.5 million cycles, about 5 ms. It is compute-bound: the columns are about
  16 bytes a node, 8 MB, which DRAM delivers in 0.3 ms.
- **Reading resolutions.** If each resolution were one load from a column indexed by
  `ExprOrPatId`, 180,000 of them at about 10 cycles would be 2 million cycles, about 0.6 ms.

So the bound is about 5.6 ms on one core.

Now (`mark/body.rs`), each body is walked over hir-def's `ExpressionStore` with its
`InferenceResult` and one `Resolver` kept at the walk's scope:

- **Visiting the store** costs what the bound says, about 5 ms.
- **Methods, fields, associated items and variants** are read from what inference recorded:
  hash-map lookups at about 50 cycles each. For 60,000 of them that is 3 million cycles,
  about 0.9 ms.
- **Value paths** (functions, statics, constants, unit structs, locals) are not recorded by
  inference, so they still go through the resolver:
  - `update_to_inner_scope` costs about 100 cycles per path: 120,000 paths is 12 million
    cycles, about 3.4 ms.
  - `resolve_path_in_value_ns_fully` scans the expression scopes, then the module's def
    map, at about 300 cycles: 36 million cycles, about 10 ms.
- **Per-body memo fetches** (3,000 of them) round to nothing.

That totals about 20 ms. It is 7.5 times faster than before, but 28% of the bound.

What stands in the way, and what removes it:

- **Value-path resolutions are not recorded.** This is 13.4 ms of the 14.4 ms gap.
  Inference already resolves every value path in order to type it, so recording the result
  costs one store per path. Requested of db in `v2/requests/diagnostics.md`: a value-path
  column in `InferenceResult` keyed by `ExprOrPatId`, as rustc's `qpath_res` reads it. With
  it, `mark` would be about 5 + 0.9 + 0.2 ≈ 6.1 ms, 92% of the bound. `Walk::resolve`
  already tries what inference recorded first, so the change here is one more read.
- **Recorded resolutions are hash maps, not columns.** This costs about 0.5 ms, under 10%
  once the column above lands. A dense column for methods and fields would close it.
- **The pass runs on one core.** rustc's does too, so this is not behind the state of the
  art, but the hardware bound is lower. A body's uses (the definitions it names, and whether
  each is named inside an `#[allow(dead_code)]` closure or anonymous constant) don't depend
  on liveness. Only which bodies are walked does. Split `mark` into a memoized per-body
  "uses" query, which is parallel and incremental, and a serial traversal of the definition
  graph. Each edge costs a few cycles, so about 200,000 edges is about 0.2 ms. On 16 cores the
  critical path is then the longest body plus the traversal, about 0.6 ms in total. This is
  the long-term design; it needs the column above first, so that a body's query reads no
  resolver state.

Signatures (types, generics, discriminants, constants in types) are still walked over syntax
through `Semantics`. Their cost scales with signature nodes, not body nodes. On the reference
crate that is about 10,000 definitions at a few resolutions each, about 3 ms on the old
per-call cost. Moving them onto hir-def's `TypeRef` store is the same rewrite as for bodies,
and is next once bodies are proven.

**`report`: the diagnostics.** One bit test per definition (10,000 at about 5 cycles) plus
text only for what fires; this is at the bound. Under `-A dead_code`, or for the
allow-by-default `dead_code_pub_in_binary`, the bound is zero, but the pass still runs. What
removes it: `cx.lint_may_fire`, the imports sibling's request 8. With it, the whole pass is
skipped where nothing can fire.

### Allocation

`tools/v2/hot_lint.py` finds nothing in `lints/names/dead/`. What it flagged was fixed where
a borrow or a move would do:

- the syntax walk takes owned children;
- `Levels::at` takes `InFile<&SyntaxNode>`;
- a block's modifiers are read as tokens, with no cast;
- `reach` drops its module index, since the set of modules it reached is the set of public
  ones;
- `Bits` and `Live` no longer derive `Clone`. The one copy rustc's pass needs, the liveness
  before deferred seeding, is `Live::snapshot`, once per crate.

What is left is justified on its line, and is one of:

- the crate's tables and worklists, allocated once per crate;
- `SmallVec`s inline up to four;
- a diagnostic's text, built only when the lint fires;
- rowan's by-value `cast` in the signature walk, through `defs::cast`, which tests the kind
  first. A clone is a reference count there, not an allocation.

The allocation-budget tests are in `lints/names/dead/tests.rs`: a cached level or attribute
answer, a lookup in the numbering, and a bitset test and insert, each at zero. They wait on
the crate's dev-dependency on `blaze-alloc-count` (requested).

Not yet at zero: a level asked at a node that is not memoized walks ancestors, and rowan
allocates each ancestor's cursor. The per-owner level column named under `level` makes that
a lookup too.

### Memory

[memory.md](../../design/memory.md) settles allocators by lifetime, with zero
general-purpose allocations in the warm loop. A crate check is one task, and everything the
pass builds is dropped when it returns, except its `Vec<Found>`.

What the pass holds, per 100,000 lines:

| Structure | Size |
| --- | --- |
| `Def` column | 10,000 × about 70 B ≈ 700 KB |
| `Id` to `DefIdx` map | 10,000 × about 20 B, at about 0.9 load ≈ 220 KB |
| CSR | about 80 KB |
| Level and attribute cells | about 250 KB |
| Level memo maps | one entry per node that has attributes, about 100 KB |
| Four bitsets | 5 KB |
| Worklists | at most about 250 KB |

That is about 1.6 MB a run. On the target machines it fits in L2 plus a slice of L3, so the
bounds above, which assume cache-resident tables, hold.

The design puts the run in a `frame::task`, with each column and worklist a
`Vec<T, &Frame>`. The run's peak becomes a bump pointer reset at the end. No general
allocation happens when the frame is warm, and the bytes are counted under the lint's tag.
The output, a few diagnostics, is the one allocation left, and only where a lint fires.

Until then, the worklists are reserved from the numbering's counts, so they don't grow.
Each run now makes about 150 to 200 calls of its own into the global allocator, which are
the vectors and maps the frame will hold.

What stands in the way:

- `blaze-diagnostics` doesn't depend on `blaze-mem` or enable `allocator_api` (requested of
  the crate's owner).
- Whether the two hash maps take hashbrown with an allocator parameter, which tag a lint
  pass counts under, and whether the output belongs to the check's request arena (asked of
  mem).
- rowan allocates a cursor for each syntax node walked, and `Semantics` allocates per call.
  The store walk removes both from bodies. For signatures, and for `Defs` until it reads the
  `ItemTree`, they remain, and they are the syntax lane's to remove.

### The whole pass

| | One core | 16 cores |
| --- | --- | --- |
| Before this change | about 165 ms | the same |
| Now | about 32 ms | the same |
| With the value-path column | about 18 ms | the same |
| With `Defs` from the `ItemTree` as well | about 8 ms | the same |
| The bound | about 7 ms | about 1 ms, with per-body "uses" queries |

On one core, `Defs` from the `ItemTree` plus the value-path column put the pass at about 90%
of its bound. rustc's pass is also serial, so it is a peer of the one-core bound; the 16-core
bound is the per-body query design above.
