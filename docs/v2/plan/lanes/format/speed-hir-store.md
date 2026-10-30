---
status: plan
summary: The HIR expression store as columns (exprs, pats, type refs, source maps), lowering that allocates per body, and how it compares with Zig's ZIR/AIR, Carbon's SemIR and rustc's arena HIR.
date: 2026-10-29
---
# speed-hir-store: the HIR store against the state of the art

Code: `crates/hir-def/src/{hir.rs,hir/,expr_store.rs,expr_store/}`. The columns for expressions
and patterns are format-hir's (contract `format-hir-columns-api.md`); type refs, source maps,
lowering allocation and the budgets are this lane's pass 2 (`speed-hir-store-pass2.md`). Both
briefs live in `~/restart-2026-10-28/briefs/v2/`.

## Benchmarks

| system | node | lists | allocation | spans |
|---|---|---|---|---|
| Zig ZIR / AIR | `Inst`: 1-byte tag + 8-byte data (9 bytes, tags and data in separate arrays of a `MultiArrayList`) | `extra: []u32`, payload structs read out of it | one `ArrayList` per column, per file (ZIR) / per function (AIR) | ZIR: a source offset in the data word, relative to the decl; no line or column info |
| Carbon SemIR | 12 bytes: kind + two 32-bit args | value blocks: `u32` ranges into shared arrays | `ValueStore`s per file | a parse-node id per instruction, kept apart |
| rustc HIR | `hir::Expr<'hir>` 64 bytes on x86_64 (`HirId`, `ExprKind`, `Span`) | `&'hir [Expr]` slices | `DroplessArena`: one bump per crate, never freed | in every node; incremental hashes them relative to the owner |

## Before (commit `5179f7cb5e`'s parent)

- **Bytes**: `Expr` 48 (asserted), `Pat` 48 (largest variant `Record`: `Path` 24 +
  `Box<[_]>` 16 + `bool`; not asserted), `TypeRef` 24, `TypeBound` 16, `Path` 24, `Statement` 32,
  `MatchArm` 16, `Binding` 16, `Label` 8, in `Arena`s (`Vec`s) of those enums.
- **Allocations per body**: one `Box<[_]>` per non-empty child list (calls, method calls,
  blocks, matches, record literals, closures twice, tuples, arrays; tuple, or, record, slice twice
  and tuple-struct patterns), each built in a `Vec`/`SmallVec` first and shrunk; one `Box` per
  `&T` and fn-pointer type and one `ThinVec` per tuple type and bound list; one `Box` per path
  with generic arguments plus its `GenericArgs` boxes; and the growth reallocations of every
  arena, `ArenaMap` and hash map (about \(\log_2 n\) each).
- **Hashes**: lowering hashed every expression, pattern, label, type and lifetime into its
  forward source map (`FxHashMap<InFile<AstPtr>, _>` insert, with rehashes as it grew), and paths
  with hygiene into two more maps. `store[e]` was a plain index (0 hashes). An IDE lookup
  `node_expr` was one hash.

## After

- **Bytes per expression**: `expr_kind` (1) + `expr_data` (4) = **5 bytes**, `const` asserted
  \(\le 12\) in `hir/columns.rs`; the words each variant takes in `extra` are tabled in that
  module's doc. Patterns the same, 5 bytes. Type refs 5 bytes (`TypeColumns` in
  `hir/type_ref.rs`) with `RefType`/`ArrayType`/`FnType`/`Path`/`TypeBound` in side tables. That
  beats ZIR's 9 and Carbon's 12 in the hot columns, and rustc's 64 by 12x. The cost is decoding:
  a `store.expr(e)` view is two loads, a `match` on the tag and, for multi-word variants, a read
  of `extra`. ZIR pays the same for its payloads.
- **Lists**: CSR ranges into one array per element type (`expr_lists`, `pat_lists`,
  `statements`, `arms`, `record_fields`, `field_pats`, `type_lists`, and for types `lists` and
  `bounds`), as ZIR's `extra` and Carbon's blocks.
- **Allocations per body**: lowering builds in per-thread pooled scratch columns, with children
  collected on a list stack (Zig AstGen's scratch pattern), so after a thread's first large body
  it allocates **nothing per node**. `finish` copies each non-empty column into an exact
  `Box<[T]>`: **one allocation per non-empty column** (about 8 to 20 per body), with no shrink
  reallocation. That matches rustc's bump arena on count, and unlike it a body's store is freed
  on its own when salsa evicts it. What still allocates per node: a `Path` with generic arguments
  (`Box<NormalPath>` and its `GenericArgs`), fn-pointer types (`FnType`'s two boxes), byte-string
  literals, `asm!` and `offset_of!`: all rare, cold side-table values.
- **Hashes**: lowering hashes nothing per node. The forward source maps are pushed to and sorted
  once per body by `(file, start, end, kind)`, which is nearly sorted already because lowering is
  preorder. `store[e]` is still 0 hashes. An IDE lookup is a binary search
  (\(\log_2 n\) compares, no hash), and iteration order is deterministic.
- **Invalidation, finer than rustc's red-green graph**: the store holds no spans (positions live
  in the source map, relative to the body's anchor), and bodies are lowered from their own
  syntax (`Body::relative`). So a formatting or comment edit inside a body re-lowers it to an
  `==` store with equal encoded bytes, and salsa backdates it: inference, MIR and borrowck don't
  run again. rustc's incremental hashes spans relative to the owner, so the same edit changes the
  owner's HIR hash and re-runs `typeck`. An edit elsewhere in the file doesn't even re-lower,
  only moves the source map. This is the per-definition granularity of Unison's content-addressed
  code, with positions kept apart. `expr_store/tests/invalidation.rs` tests the three cases.

## Still short, and what it takes

- **`Option<Idx>` is 8 bytes.** `la_arena::Idx` has no niche, so `Statement` stays 32 bytes,
  `MatchArm` 16 and `RefType` 16. A raw index that can't be `u32::MAX` (a `NonMaxU32`-style
  niche) makes them 20/12/12 with no API change. That needs `la-arena` vendored (root
  `Cargo.toml`, coordinator).
- **Paths.** A `Path` is a 24-byte owned value in a side table, and its generic arguments are
  boxed. Making it columns means `Path`'s methods take the store, which changes
  `segments()`/`generic_args()` at every resolver and `hir-ty` caller. That is the next round,
  not this one.
- **Bindings and labels** keep their arenas (16 and 8 bytes, flat records, no per-node heap).
