# Requests from speed (and its children `speed-*`)

## 1. root `Cargo.toml`: `bumpalo` as a workspace dependency

What: in `[workspace.dependencies]`, `bumpalo = { version = "3.20", features = ["collections"] }`
(3.20.3 is already in `Cargo.lock`, pulled in transitively).

Why: the per-task bump arenas of principles.md ("Arenas, not the heap") in front-end crates that
can't depend on `blaze-graph` (whose `arena::Bump` is the daemon's). speed's children write
`bumpalo.workspace = true` in the crates they own once they use it.

> applied: `bumpalo = { version = "3.20", features = ["collections"] }` in the root `[workspace.dependencies]`, with its reason; 3.20.3 is in `Cargo.lock`.

Children append their own requests below, each under a heading naming the child.

## speed-link-edge-proof

### root `Cargo.toml`: `memmap2` as a workspace dependency

What: in `[workspace.dependencies]`, `memmap2 = "0.9"` (0.9.11 is in `Cargo.lock`; blaze-cargo,
blaze-engine, blaze-store and blaze-splice already name `memmap2 = "0.9"` directly and could move
to `workspace = true`).

Why: `crates/blaze-v2-proof` compares and hashes build outputs and reads query traces of
hundreds of MB in place (principles.md "Zero-copy"), instead of reading each into a `Vec`.
`crates/blaze-v2-proof/Cargo.toml` writes `memmap2.workspace = true`.

> applied: `memmap2 = "0.9"` in the root `[workspace.dependencies]` (0.9.11 in `Cargo.lock`). Moving blaze-cargo, blaze-engine, blaze-store and blaze-splice to `workspace = true` is their lanes' (their own manifests).

### `pyproject.toml` / `uv.lock`: the `blake3` Python package (optional, not blocking)

What: add `blake3` (the PyPI bindings of the Rust crate, with SIMD dispatch and a
`max_threads` option) to the Python dependencies that `uv run --frozen` uses for `harness/v2`.

Why: `harness/v2/engine.py` `digest` hashes every build output and every source file of a
corpus checkout (`tree_digest`) with hashlib's SHA-256: hardware SHA-2 is ~1.5-2.5 GB/s a core
and BLAKE3 ~3-6 GB/s. Once it is there I'd switch `digest` and `Kept.key` to BLAKE3 and bump
their domain to `"blaze v2 proof <kind> 2"`. Until then SHA-256 is correct, only slower.

## speed-hir-nameres

- `crates/hir-ide/src/tests/consteval/const_traits.rs:9-19` (coordinator's; removed at
  integration, so only while it is still built): `hir_def::nameres::EXPAND_BUILTIN_DERIVES_ON_THIS_THREAD`
  is gone (a `thread_local!` read inside `crate_def_map`, an untracked input whose answer depended
  on which thread collected). Replace `check_number_derived`'s body with a database set up by
  `hir_def::nameres::set_expand_builtin_derives(&mut db, true)` before the fixture is analysed,
  or, if `check_number` builds its database internally, keep a thread-local in the test module
  that `check_number`'s database setup reads and passes to `set_expand_builtin_derives`.
  `hir_def::nameres::EXPAND_BUILTIN_DERIVES` (the process-wide atomic set by blaze-engine and
  blaze-wasm at startup) is unchanged and now exists under `cfg(test)` too.

> applied: `crates/hir-ide/src/tests/consteval/const_traits.rs`: `check_number_derived` builds the fixture's database, calls `hir_def::nameres::set_expand_builtin_derives(&mut db, true)`, then `check_number_in`; `tests/consteval.rs` splits `check_number`/`check_answer` into `check_number_in`/`check_answer_in` over a database the caller set up.

## speed-hir-store: coordinator-owned consumers

### from speed-hir-store-tools (2026-10-29)

The rule is "## format-hir: expressions and patterns become columns" in `v2/requests/format.md`.

- **`crates/hir-ide/src/diagnostics/expr.rs`, needed by `crates/hir/`**: `crates/hir/` still
  depends on `hir-ide` (its `Cargo.toml`: `hir-ide.workspace = true`), and
  `crates/hir/src/source_analyzer.rs` calls `hir_ide::diagnostics::record_literal_missing_fields`
  (`expr.rs:647`) and `record_pattern_missing_fields` (`expr.rs:690`). The hir edits filed in
  `v2/requests/db.md` ("## speed-hir-store: hir onto the HIR column views") pass
  `body.expr(expr_id)` / `body.pat(pat_id)`, so in the same change:
  - `expr.rs:651`  `    expr: &Expr,`  ->  `    expr: Expr<'_>,`
  - `expr.rs:694`  `    pat: &Pat,`  ->  `    pat: Pat<'_>,`
  - and the bodies' `*` on id or `Copy` fields go per the rule (a `match expr { Expr::RecordLit
    { fields, spread, .. } .. }` keeps `fields: &[RecordLitField]`; `Pat::Record { ellipsis, .. }`
    binds `ellipsis: bool` by value).
  If `hir-ide` is removed before the columns land, `hir` must move these two calls to whatever
  replaces them; until then the rest of `hir-ide` must compile against the views too, or `hir`
  does not build.
- **Site counts, no edits written** (removed at integration): `crates/hir-ide/` about 100
  candidate sites in 10 files that import `hir_def::hir` (an upper bound: it counts `TypeRef`
  indexing too); `crates/ide-db/` none.
- No other coordinator-owned crate (`crates/test-fixture/`) reads the HIR views.

> applied: `hir-ide` is on the views (`hir-def` has them). `diagnostics/expr.rs`: `record_literal_missing_fields` takes `expr: Expr<'_>`, `record_pattern_missing_fields` `pat: Pat<'_>` (binding `ellipsis` by value), `validate_call` and `check_for_unnecessary_else` take `Expr<'_>`, and every `body[e]` is `body.expr(e)`. Same edits in `unsafe_check.rs`, `match_check.rs` (`ref` dropped, `Expr::Literal(&Bool(value))`), `decl_check.rs` and `tests.rs`. The rest of the counted sites index bindings, type refs or MIR locals, which keep their arenas. `ide-db` and `test-fixture` have none.

## speed-link-edge-elf: `object` with its `unaligned` feature

What: in the root `[workspace.dependencies]`, add `"unaligned"` to `object`'s features:
`object = { version = "0.37", default-features = false, features = ["read_core", "std", "archive", "elf", "macho", "coff", "pe", "wasm", "unaligned"] }`.

Why: without it `object`'s `U16`/`U32`/`U64` are naturally aligned, so its ELF reader refuses
bytes that aren't 8-aligned, and an archive member starts on any even offset. The ELF reader
(`crates/blaze-link/src/elf/read.rs` `aligned`) therefore copies about 3 of every 4 archive
members whole before parsing them. With the feature the types are byte arrays (x86-64 and
AArch64 load unaligned at no cost), `align_of::<FileHeader64<_>>()` is 1, and `aligned` never
copies: no code change is needed, it reads the alignment from the type. The Mach-O and COFF
readers gain the same. No caller relies on `object` types being aligned (`rg 'align_of' crates/`
finds none on them).

> applied: `"unaligned"` added to `object`'s features in the root `[workspace.dependencies]`; 0.37 is in the lock, so nothing new.

## speed-hir-store: coordinator-owned consumers

### from speed-hir-store-tools: blaze-doc onto the type-ref view (2026-10-29)

`TypeRef` becomes a view (`store.type_ref(t)`; API in the addendum of
`~/restart-2026-10-28/briefs/v2/speed-hir-store-consumers.md`). `crates/blaze-doc/` is the
coordinator's (`crates.md`, "Every other path"). 18 sites, all in `crates/blaze-doc/src/sig.rs`;
no other blaze-doc file reads a type ref, expression or pattern. `LifetimeRef` (~L286, ~L542,
~L680) and `PathId` (`store[*path]`, `store[trait_ref.path]`) indexing stay. Every store is
`&'a ExpressionStore`, so the views borrow it and the `self.ty(..)` calls in the arms still
compile.

> forwarded to coordinator: `crates/blaze-doc/` is "every other path" in `crates.md`, not among the paths `requests` applies, and `hir-def` has no type-ref view yet. `hir-ide`'s 18 `TypeRef` lines (`display.rs`) are mine; they move once the view lands.

#### crates/blaze-doc/src/sig.rs  (18 sites)
- ~L70 (in `fn ty`)  `match &self.store[ty] {`  ->  `match self.store.type_ref(ty) {`
- ~L96 (`RawPtr` arm) and ~L118 (`Slice` arm), the only two in the file:  `self.ty(out, *inner);`  ->  `self.ty(out, inner);` (`match mutability` in the `RawPtr` arm is unchanged: it now matches a `Mutability` value)
- ~L153  `if !matches!(&self.store[*ret], TypeRef::Tuple(it) if it.is_empty()) {`  ->  `if !matches!(self.store.type_ref(*ret), TypeRef::Tuple(it) if it.is_empty()) {` (`ret` is from `fn_.params`, an owned `FnType`, so `*ret` stays)
- ~L170  `if let Some((_, code)) = self.places.subst.iter().find(|(it, _)| it == param) {`  ->  `if let Some((_, code)) = self.places.subst.iter().find(|(it, _)| *it == param) {` (`TypeRef::TypeParam(param)` now binds `TypeParamId` by value; `it` is still `&TypeParamId`)
- ~L193  `if matches!(store[*target], TypeRef::TypeParam(id) if id == *param) =>`  ->  `if matches!(store.type_ref(*target), TypeRef::TypeParam(id) if id == param) =>`
- ~L209  `TypeRef::PatternType(inner, _) => self.ty(out, *inner),`  ->  `TypeRef::PatternType(inner, _) => self.ty(out, inner),`
- ~L465  `match &self.store[*tuple] {`  ->  `match self.store.type_ref(*tuple) {` (`_ => self.ty(out, *tuple)` stays: `tuple` is from an owned `GenericArg`)
- ~L479  `&& !matches!(&self.store[ret], TypeRef::Tuple(it) if it.is_empty())`  ->  `&& !matches!(self.store.type_ref(ret), TypeRef::Tuple(it) if it.is_empty())`
- ~L625  `matches!(self.store[target], TypeRef::TypeParam(p) if p.local_id() == id)`  ->  `matches!(self.store.type_ref(target), TypeRef::TypeParam(p) if p.local_id() == id)`
- ~L637  `&& match self.store[*target] {`  ->  `&& match self.store.type_ref(*target) {`
- ~L815  `&& !matches!(&data.store[ret_ty], TypeRef::Tuple(it) if it.is_empty())`  ->  `&& !matches!(data.store.type_ref(ret_ty), TypeRef::Tuple(it) if it.is_empty())`
- ~L852 (in `fn return_type`)  `match &data.store[ret] {`  ->  `match data.store.type_ref(ret) {` (`bounds.first()` and `data.store[*path]` are unchanged)
- ~L867 (in `fn self_param`)  `match &store[ty] {`  ->  `match store.type_ref(ty) {`
- ~L871  `TypeRef::Reference(reference) if matches!(&store[reference.ty], TypeRef::Path(p) if p.is_self_type()) =>`  ->  `TypeRef::Reference(reference) if matches!(store.type_ref(reference.ty), TypeRef::Path(p) if p.is_self_type()) =>`
- ~L1200  `if matches!(data.store[*target], TypeRef::TypeParam(p)`  ->  `if matches!(data.store.type_ref(*target), TypeRef::TypeParam(p)`
- ~L1272  `let TypeRef::Path(path) = &data.store[data.ty?] else { return None };`  ->  `let TypeRef::Path(path) = data.store.type_ref(data.ty?) else { return None };`

`crates/hir-ide/` (removed at integration, no edits written): 18 `TypeRef` pattern lines in 1
file. `crates/ide-db/`: none.

## speed-runtime-vfs: `blaze-reclaim` as a workspace dependency

What: in the root `[workspace.dependencies]`, under `# v2`, the line
`blaze-reclaim = { path = "crates/blaze-reclaim" }` (alphabetically after `blaze-query`).

Why: `crates/blaze-vfs` now reads its node table with no lock and frees a collection's old
table at task boundaries through `blaze_reclaim::{enter, Limbo}` (docs/v2/plan/design/
concurrency.md "Memory"), the same crate `blaze-cas`'s `table.rs` already uses, rather than an
epoch scheme of its own. `crates/blaze-vfs/Cargo.toml` writes `blaze-reclaim.workspace = true`;
until this line is in, the manifest lint names it and blaze-vfs does not build. The concurrency
lane's report lists the same entry as still to write.

> applied: `blaze-reclaim = { path = "crates/blaze-reclaim" }` under `# v2`, after `blaze-query`.

## speed-backend-intrinsics

> forwarded to codegen and speed: 1 is in `crates/hir-ty-codegen/`. > forwarded to coordinator: 2 vendors `cranelift-codegen` under `vendor/` with a `[patch.crates-io]` entry, as codegen.md 5 asks too; the patch line goes in with the vendored tree.

### 1. Cranelift ISA per `#[target_feature]` set (x86-64 SSSE3/SSE4/AVX2 vector code)

Where: `crates/hir-ty-codegen/target.rs` `Target::isa` (speed-backend-debug's), the one
`OwnedTargetIsa` per process (`crate::target()`), and its users `v2/backend.rs` `isa(unit)`
(codegen-adapter's) and `mod.rs` `compile_as` (speed-backend-lower's).

What: keep one ISA per distinct set of enabled x86 features (`AttrFlags::target_features` of
the function, plus the crate's `-C target-feature`), built once and cached by that set (a few
per process: baseline, `ssse3`, `sse4.1`, `avx2`, ...), and compile each function with the ISA
of its own set: `isa.enable("has_ssse3")`, `has_sse41`, `has_sse42`, `has_avx`, `has_avx2`,
`has_bmi1`, `has_bmi2`, `has_popcnt`, `has_lzcnt`, `has_fma` as the set says. The unit key
gains the set (it decides the bytes). Cranelift's settings are per ISA, so this is the only way
it emits `pshufb`, `pminud`, `roundps`, `pcmpgtq`, `palignr`.

Why: x86-64's ISA has SSE2 alone today, so Cranelift lowers `swizzle`, a byte `splat` and most
`shuffle`s to its `X86Pshufb` libcall (which blaze does not provide) and has no lowering of
`x86_pshufb` at all. So `wasm_simd.rs` admits only SSE2 ops on x86-64 (`admits`, `Vectors::Sse2`)
and `_mm_shuffle_epi8` (Teddy's inner loop, a `#[target_feature(enable = "ssse3")]` function)
stays 16 scalar loads, selects and stores (`mod.rs` `llvm_intrinsic`, `x86.ssse3.pshuf.b.128`).
LLVM emits one `pshufb`. With the per-function ISA, `admits(Vectors::Sse2, ..)` becomes
`admits(Vectors::X86(features), ..)` (mine to write) and that arm `x86_pshufb`.

### 2. Vendor cranelift-codegen and add vector reductions

What: vendor `cranelift-codegen` 0.136.2 (and its `-meta`, `-shared`) under `vendor/` with a
`[patch.crates-io]` entry in the root `Cargo.toml`, as `vendor/cranelift-object` is, and add
`vreduce_umax`, `vreduce_smax`, `vreduce_umin`, `vreduce_smin`, `vreduce_add` (an integer
vector to its lane type) to `shared/instructions.rs`, lowered on AArch64 to `umaxv`, `smaxv`,
`uminv`, `sminv`, `addv` (`uaddlv` not needed) and on x64 to the `pshufd`/`psrldq` + op ladder.

Why: `simd_reduce_max` (`vmaxvq_u8`, memchr's NEON hot loop `vector_is_zero`/`movemask` path)
is now `ext`+`umax` four times and a `umov` (9 instructions, `wasm_simd.rs` `simd_reduce`)
where LLVM emits `umaxv` + `fmov` (2). Without the op, Cranelift cannot express it.

> applied: `blaze-reclaim = { path = "crates/blaze-reclaim" }` is in the root `[workspace.dependencies]` (seen by speed-runtime-vfs at 2026-10-29; the manifest lint is clean for blaze-vfs).

## speed-backend-code

> forwarded to codegen: only `hashbrown.workspace = true` in `crates/hir-ty-codegen/Cargo.toml` is asked; the root has `hashbrown` 0.17 already.

### `hashbrown` in `crates/hir-ty-codegen/Cargo.toml` (2026-10-29)

The workspace already has `hashbrown` 0.17 (root `Cargo.toml:253`, in `Cargo.lock`); only
`hashbrown.workspace = true` under hir-ty-codegen's `[dependencies]` is asked, through
`v2/requests/codegen-adapter.md` (that file's owner). `fragment.rs` uses `HashTable` for its
request tables. Nothing new is downloaded.

## speed-runtime-vfs: `blaze-vfs`'s model-check dependency, once `blaze-check` exists

What: when the root `[workspace.dependencies]` has `blaze-check` (v2/requests/safety.md M1 1),
add to `crates/blaze-vfs/Cargo.toml`:

```toml
[target.'cfg(blaze_check)'.dependencies]
blaze-check.workspace = true
```

Why: `crates/blaze-vfs/src/sync.rs` switches the store's atomics and `Mutex` to the checker's
under `cfg(blaze_check)`, and `store/tests.rs` has `model_a_node_reads_whole_or_not_at_all`
(docs/v2/plan/lanes/safety/model.md). Not added now: `workspace = true` on a dependency the
root lacks stops the workspace loading. A normal build is unchanged without it.
