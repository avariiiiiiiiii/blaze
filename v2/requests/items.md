# Requests from lane items

## items-facts: `InferenceResult::defined_anon_consts` (`crates/hir-ty/src/infer.rs`)

What: a public accessor for the anonymous consts a body's inference defined (its inline
`const { .. }` blocks, array lengths and const arguments written in the body):

```rust
impl<'db> InferenceResult<'db> {
    /// The anonymous consts of the body this inferred, including those inside them.
    pub fn defined_anon_consts(&self) -> &[AnonConstId<'db>] {
        &self.defined_anon_consts
    }
}
```

Where: `crates/hir-ty/src/infer.rs`, on `InferenceResult` (the field `defined_anon_consts` is
private, line ~862).

Why: `Fact::Captures(key)` resolves its key to `Resolved::Closure { store, expr, kind }` and
has to rebuild the closure's `InternedClosureId`, whose owner is the `InferBodyId` that
inferred it: the body, or the anonymous const the closure is inside (`infer_anon_const_query`
infers those apart and `merge_anon_consts` merges them). Guessing the body as owner would name
a closure id inference never made, so its captures would be wrong. For signatures and fields
`AnonConstId::all_from_signature` and `field_types_with_diagnostics(..).defined_anon_consts()`
already list them; for bodies only the inference result has the list.
`crates/blaze-items/src/facts.rs` (`closure_owner`) calls
`InferenceResult::of(db, body).defined_anon_consts()` as written above; it does not compile
until this lands. Read-only, no behavior change.

> forwarded to db: `crates/hir-ty/src/infer.rs` is db's (db-ty).

## items-form: `VTableMap::tys` (`crates/hir-ty/src/mir.rs`)

Applied by speed-types (2026-10-29). `form.rs` `Cx::allocation` now reads `map.vtable.tys()`;
the `facts.rs` half of speed-types' note below is items-facts'.

What: a public iterator over a vtable map's types in id order:

```rust
impl<'db> VTableMap<'db> {
    /// The types of the vtables, in the order of their ids.
    pub fn tys(&self) -> impl ExactSizeIterator<Item = Ty<'db>> + '_ {
        self.id_to_ty.iter().copied()
    }
}
```

Why: `form.rs` (`Cx::allocation`) spells a constant's vtables. `OFFSET` and `id_to_ty` are
private, so it finds the first id `ty(id)` accepts by counting up from 0 and then reads on
until one fails. That is correct but leans on how ids are laid out, and it costs about 1000
failed `ty` calls (each building a `MirEvalError`) per constant that holds a vtable, in every
form that spells one. With `tys` it is one line and no wasted work. Read-only, no behavior
change.

> forwarded to db: `crates/hir-ty/src/mir.rs` is db's (db-ty).

## items-form: `tait_hidden_types` public (`crates/hir-ty/src/opaques.rs`), optional

What: `pub(crate) fn tait_hidden_types` → `pub fn`.

Why: `Cx::hidden` gets a TAIT's hidden type through
`DbInterner::type_of_opaque_hir_typeck(OpaqueTyIdWrapper(..))`, which reads it. Reading the
query itself would say what is meant. Not blocking.

> forwarded to items: `crates/hir-ty/src/opaques.rs` is items' own file (crates.md).
> applied: lane items is DONE; `tait_hidden_types` is `pub` and re-exported beside `rpit_hidden_types` (`hir_ty::tait_hidden_types`), since `mod opaques` is private.

## items-form, for lane items: who observes a type's name through a generic wrapper

`Cx` spells the `item_key` and path of nominal types only where `type_name` / `type_id` (the
intrinsics) are called directly, and for the `TypeId`s a constant holds. A body that calls
`std::any::type_name::<A>()` (a generic fn that calls the intrinsic) gets `A` as a slot, and
the slot's hash does not include `A`'s name, so renaming `A` keeps that body's hash while the
program's output changes. Fixing it needs a transitive fact: "this generic fn, instantiated
with these parameters, observes their names", computed over the call graph, and the form
spelling names for a call to any fn with that fact. That is a cross-item fact, which is the
parent's (`hash`/`facts`), not something one form can see.

> forwarded to items: settled by items below.

## items-form, for lane items: share type forms across items and revisions

`form::Cx` memoizes a type's `Sub` in a `RefCell<FxHashMap<Ty, Rc<Sub>>>` that lives only as
long as one `Cx`. Every item's form re-spells the types it shares with others (`Vec<T>`,
`Option<&str>`, every std type in a signature), and every revision re-spells them all. A
salsa-tracked `ty_form(db, Ty) -> Sub` would compute each once per revision and keep it across
edits that don't touch it. That needs `Sub: salsa::Update` (its fields are `ContentId`,
`Vec<ItemId>`, `Option<&'static str>`), and the `spelling` cycle guard turned into salsa cycle
handling. I didn't make the change without a compiler to check it; the parent, which builds,
should decide where it goes and measure it.

> forwarded to items: the parent's call, in its own crate.
> forwarded to coordinator: lane items is DONE and left this among its "costs that need profiling"; a tracked `ty_form` changes items' own crate, not an interface type, and wants measuring after the first compile.

## items: an instance of a name-observing fn is keyed on its arguments' names; a const's data on its value (for `codegen`)

This settles items-form's "who observes a type's name through a generic wrapper" above. A
body hashes a direct call of `type_name`/`type_id` with the key and path of the nominal types
in its arguments (`form`), so only the instance that calls the intrinsic embeds a name: with
callees named by identity (the request above), `std::any::type_name::<A>`'s caller calls that
instance and holds no name itself. What codegen must do:

- An instance of a fn for which `blaze_items::observes_names(db, func)` holds, or whose body
  calls one with its own generic parameters in the arguments, is keyed on the
  `Fact::Name` of every nominal type in its instance arguments as well as its code hash. An
  instance that inlines one (release, deep hash) carries the same.
- A const's or static's data is keyed on its `Fact::ConstValue` (the evaluated bytes), never
  on its code hash: `const C: &str = f::<A>()` keeps its code hash when `A` is renamed while
  its bytes change.

Why: otherwise a rename keeps a stale `type_name` string or `TypeId` (docs/v2/plan/lanes/items.md 6.1:
names are in a hash only where the program observes them, and here it does, one instance away).

> forwarded to codegen

## items: `ContentId::of_many` and one derived key per domain (for `format`, `crates/blaze-ids`)

What, both bit-identical to today's output on every path:

1. `ContentId::of_many(domain: &'static str, inputs: &[&[u8]]) -> Vec<ContentId>`, exactly
   `inputs.iter().map(|it| ContentId::of(domain, it))`, hashing many small inputs per SIMD
   pass (16 lanes with AVX-512, 8 with AVX2, 4 with NEON and SSE4.1), picked at run time by
   CPU feature. This is store's request 5 (`v2/requests/store.md`); items calls it too.
   `crates/blaze-items/src/key.rs` (`hash_all`) hashes the keys of a module's, an impl's, a
   trait's and an enum's items in one call, and doesn't build until it lands.
2. `Hasher::new(domain)` derives the domain's key (`new_derive_key` hashes the context
   string, one compression) on every call. Derive it once per domain and clone a prototype
   `blake3::Hasher` (it is `Clone`; the output is unchanged): a per-domain cache keyed by
   the `&'static str`'s address, or a `Domain` const type made once. Items makes millions of
   small hashes (two per `form::Form`, one per key, fact and component), so this is up to
   half their cost.
3. The root `Cargo.toml`: `blake3` with the `neon` feature (store's request 5), so aarch64
   (Graviton, Apple) gets NEON.

Why: docs/v2/plan/design/hardware.md, "BLAKE3 of sources, items and blobs ... one batched
call per many small inputs, not one call each".

> forwarded to format: items 1 and 2 are `crates/blaze-ids` (format's).
> applied: item 3, `blake3` with `neon` in the root `Cargo.toml` (store's request 5).

## items-facts, for lane items: the API of `crate::table::FactSet` (`crates/blaze-items/src/table.rs`)

What: the signatures of `FactSet` (and `HashColumns`, if facts.rs should use it), with how a
set is built, how a fact's subject and object are given (as `ContentId`s, mapped to `u32`
through the sorted dictionary?), and the order iteration yields. `table.rs` is not in the
tree yet and I may not touch it, so facts.rs cannot call it.

Where facts.rs gathers several facts, so where it would use it, and the order each value
hashes them in today, which must not change:
- `implements_value` (index.rs): the `Implements` run for the self type, then the blanket
  run, each sorted by key;
- `Vtable`: per supertrait in `all_super_traits` order, its interface and its `Implements`
  value; then its methods in declaration order;
- `Method` (`write_method`): inherent impls sorted by (interface hash, items), then traits
  sorted by (items, `Implements` value);
- `Coherence`: one `Implements` value, or every impl of the trait in index order.

If `FactSet` iterates in (kind, subject, object) order with the dictionary sorted by
`ContentId`, the declaration-order ones (Vtable's supertraits and methods) must still be
written from their own order, not the set's.

Why: docs/v2/plan/design/data.md, items row: "facts as `(kind, subject, object)` triples
sorted for range scans".

> answered (items): `table.rs` is in the tree. `FactSet` is `FromIterator<Fact>`; `iter()`
> yields facts sorted by (kind, subject, object) by content, distinct; `about(kind, subject:
> ContentId)` is one kind's range about one subject; `contains(&Fact)`. It is for a query's
> read set (which facts it depends on), not for a fact's value: the four places above hash
> in their own order and stay as they are.
> applied: answered by items above; nothing to change (facts.rs keeps its own orders).

## items-hirty: 4-byte interned ids (db, `vendor/salsa`)

`TraitImpls` and `InherentImpls` are now columns (`crates/hir-ty/src/method_resolution.rs`), but
every `ImplId`, `TraitId` and `BuiltinDeriveImplId` in them is 8 bytes: salsa's `Id` is an index
plus a generation. These ids are interned with `revisions = usize::MAX` (`impl_intern_key!` in
`base-db`, `BuiltinDeriveImplId` in `hir-def`), so the generation is never bumped and is always
the same. Asked: an id type of 4 bytes (`NonZeroU32` alone) for interned structs that are never
collected. That halves every impl column, and every other long-lived id array too. The asserts
beside the columns say `<= 8` today, so they keep holding when ids shrink; tighten them to
`== 4` then. Why: docs/v2/plan/design/data.md, "a type is a `u32`" and the size budgets.

> forwarded to db: `base-db`'s `impl_intern_key!`, `hir-def` and `vendor/salsa` are lane db's. The `<= 8` asserts in `method_resolution.rs` (items DONE, now mine) tighten to `== 4` once ids shrink.

## items-hirty: simplified self types as `u32` (db, type tables)

The self type column of the impl tables keeps the `SimplifiedType<SolverDefId>` (16 to 24 bytes)
plus a `u64` hash column that is searched, because `SimplifiedType` has no `Ord`. When db's
type tables give each simplified type a `u32` (a tag byte plus the def's 4-byte id would do),
the self type column becomes that `u32`, sorted directly, and the hash column goes away. Asked:
a `u32` encoding of `rustc_type_ir::fast_reject::SimplifiedType` from db, or `Ord` on it and on
`SolverDefId` in `vendor/rustc_type_ir`. Nothing waits on this; the tables are correct without it.

> forwarded to db: the type tables and `vendor/rustc_type_ir` are lane db's.

## speed-types: read vtables with `VTableMap::tys` (`form.rs` 2092-2099, `facts.rs` 497-510)

`crates/hir-ty/src/mir.rs` now has `VTableMap::tys()` (the types in id order, an
`ExactSizeIterator`) and `VTableMap::FIRST_ID` (items-form's request, applied by speed-types).
Both sites probe ids up from 0, building about 1000 `MirEvalError`s for each constant with a
vtable. Edits, bit-identical:

- `form.rs` `Cx::allocation`: `let vtables: Vec<_> = map.vtable.tys().collect();` replaces
  the `first` search and `map_while`. Or skip the `Vec`: `f.len(map.vtable.tys().len());
  for ty in map.vtable.tys() { .. }`.
- `facts.rs` `const_value`: for a non-empty map, `h.update(&(hir_ty::mir::VTableMap::FIRST_ID
  as u64).to_le_bytes())`, then `for ty in map.vtable.tys() { vtables.push(ty_hash(db, owner,
  ty)?) }`. That is the same `id` the loop found, so the value is unchanged.

speed-types takes these itself once items-form and items-facts are idle, unless you get there
first.

> applied by items-facts: the `facts.rs` half, as written (`FIRST_ID`, then `tys()`). The
> `form.rs` half is items-form's.
> applied: both halves are in: `facts.rs` by items-facts, `form.rs` `Cx::allocation` reads `f.len(map.vtable.tys().len())` then `tys()`.

## items-facts, for lane items: `#![forbid(unsafe_code)]` on blaze-items (`src/lib.rs`, `src/key.rs`)

What: `key.rs:334`, `Siblings::Module(unsafe { module.to_static() })`, is the crate's only
`unsafe`. It erases a lifetime so that the module can be compared with `siblings(db, id)`.
That buys no measured speed and reaches no kernel or hardware, so under
docs/v2/plan/design/unsafe.md it should not be `unsafe`. Compare without it, for example by
making `Siblings` generic over the module's lifetime, or by comparing the salsa id. Then
`lib.rs` can take `#![forbid(unsafe_code)]`: `facts.rs`, `index.rs` and `tests/facts.rs`
have no `unsafe` and need none.

Why: docs/v2/plan/design/unsafe.md, "Crates with no unsafe module get
`#![forbid(unsafe_code)]`".

> applied (items): `module_item_keys` compares the module by its salsa id
> (`salsa::plumbing::AsId::as_id`), and `lib.rs` has `#![forbid(unsafe_code)]`. salsa's
> macros expand to `unsafe` from `salsa-macro-rules`, an external macro, which the lint does not
> report; the first build confirms it.

## items-hirty: impl attribute bits in the item scope (format, `hir-def`)

Building `TraitImpls` (`method_resolution.rs`, `collect_def_map`) calls `AttrFlags::query` on
every trait impl of every crate to drop `#[rustc_reservation_impl]` ones, which is a memo
lookup and an edge per impl (std and windows-sys have tens of thousands). Asked: keep the
`ItemScope`'s impls as two CSR lists (trait impls, inherent impls) in place of `(ImplId, bool)`
filtered on each call, with a bitset column beside the trait impls for "reservation impl",
filled when the item tree is collected. Then the collection reads one bit per impl and adds no
edges. Same impls, same order; faster because it drops one query read per impl, the filter
branch, and the edges (docs/v2/plan/design/data.md: flags as bitsets, lists as CSR).

> forwarded to db: `ItemScope` (`hir-def/src/nameres`) is lane db's, not format's. The reader in `method_resolution.rs` `collect_def_map` (now mine) moves to the bitset once it exists.
> applied by db-expand: `ItemScope::impls` is a `ThinVec<ImplId>` in declaration order, beside two bitsets of its length, `trait_impl` and `reservation_impl` (`item_scope.rs` `Bits`, a `u64` word per 64 impls). `impls()`, `trait_impls()` and `inherent_impls()` keep their signatures and declaration order. New `trait_impls_without_reservation()`. The collector sets the bit from `attrs.by_key(sym::rustc_reservation_impl).exists()` on the cfg-expanded attrs, which agrees with `AttrFlags::RUSTC_RESERVATION_IMPL`. It is compared in `PartialEq`, shrunk, encoded and decoded in `nameres/frozen.rs` (`ENCODING` bumped to 8). I kept one ordered list with bitsets, not the two CSR lists, because three readers rely on declaration order. Test `impls_keep_their_order_and_their_reservation_bit` (`nameres/tests.rs`: a plain, a reserved and two `cfg_attr` impls; the frozen bytes round-trip under `Mode::Verify`). What waits: `method_resolution.rs:1655-1670` uses `trait_impls_without_reservation()` and drops its `AttrFlags::query` per impl.

## items-hirty: candidate assembly across crates (db, `next_solver`)

Following my earlier request (`next_solver/solver.rs:235-254`): the solver walks
`TraitImpls::for_crate_and_deps` and does one search per loaded crate for each goal, so a
binary with 300 dependencies makes 300 binary searches per trait goal. The narrow query
`TraitImpls::in_crate_and_deps_for_trait_and_self_ty(krate, trait, self_ty)` answers it with
one memo lookup and gives the same impls in the same order: each crate's impls, then its
derives, crate by crate, which is the order the walk gives today. Asked: use it (and
`blanket_in_crate_and_deps`) there. Faster per goal, and an edit adding an impl of another
trait no longer reruns the goal.

> forwarded to db: `next_solver/solver.rs` is lane db's; the narrow queries it should call are in `method_resolution.rs`.

## speed-hir-store: blaze-items onto the HIR column views (from speed-hir-store-tools)

From speed-hir-store-tools, 2026-10-29. 4 sites in 4 files of `crates/blaze-items/src/`, all the
same edit. The rule is "## format-hir: expressions and patterns become columns" in
`v2/requests/format.md` (contract: `~/restart-2026-10-28/briefs/v2/format-hir-columns-api.md`).
These land together with the format-hir change. `store.exprs()` / `body.exprs()` now yields
`(ExprId, Expr<'_>)`, so `Expr::Closure { closure_kind, .. }` binds `ClosureKind` by value. The
closure keys hash a closure's place in `exprs()` order (`key.rs`, `nth`); the columns keep ids
dense and in allocation order, so that order and the key bytes are unchanged. No site builds an
`Expr`, uses the builder, or stores, hashes or compares a view.

Files that import the HIR types and need no change: `facts.rs` (`ExprId` and
`walk_child_exprs` only), `form.rs` (`ClosureKind`, `MirSpan::ExprId`).

> applied: the four sites (`key.rs` `let kind = closure_kind;`, `tests/{facts,form,hashes}.rs` `kind: closure_kind`). They compile with format-hir's `exprs()` yielding `Expr<'_>`.

### crates/blaze-items/src/key.rs  (1 site)
- ~L889  `let kind = *closure_kind;`  ->  `let kind = closure_kind;`

### crates/blaze-items/src/tests/facts.rs  (1 site)
- ~L259  `InternedClosure { owner: owner.into(), expr, kind: *closure_kind },`  ->  `InternedClosure { owner: owner.into(), expr, kind: closure_kind },`

### crates/blaze-items/src/tests/form.rs  (1 site; file of speed-types-form)
- ~L40  `InternedClosure { owner: owner.into(), expr, kind: *closure_kind },`  ->  `InternedClosure { owner: owner.into(), expr, kind: closure_kind },`

### crates/blaze-items/src/tests/hashes.rs  (1 site)
- ~L322  `InternedClosure { owner: owner.into(), expr, kind: *closure_kind },`  ->  `InternedClosure { owner: owner.into(), expr, kind: closure_kind },`

## items-form, for lane items: auto-trait checks of one type in one inference context (`crates/hir-ty/src/traits.rs`)

What: next to `implements_trait_unique`:

```rust
/// For each of `traits`, whether `ty` implements it: what `implements_trait_unique` answers
/// for each, from one interner and one inference context.
pub fn implements_traits_unique<'db>(
    ty: Ty<'db>,
    db: &'db dyn HirDatabase,
    env: ParamEnvAndCrate<'db>,
    traits: &[TraitId],
) -> Vec<bool> {
    let interner = DbInterner::new_with(db, env.krate);
    let infcx = interner.infer_ctxt().build(TypingMode::non_body_analysis());
    traits
        .iter()
        .map(|&trait_| {
            infcx.probe(|_| {
                let args = infcx.fill_rest_fresh_args(Span::Dummy, trait_.into(), [ty.into()]);
                let trait_ref = rustc_type_ir::TraitRef::new_from_args(interner, trait_.into(), args);
                if trait_ref.self_ty().is_ty_error() {
                    return false;
                }
                let obligation =
                    Obligation::new(interner, ObligationCause::dummy(), env.param_env, trait_ref);
                infcx.predicate_must_hold_modulo_regions(&obligation)
            })
        })
        .collect()
}
```

Why faster: `form.rs` `Cx::auto_traits` asks every auto trait of the loaded crates (`Send`,
`Sync`, `Unpin`, `UnwindSafe`, `RefUnwindSafe`, `Freeze` and the crates' own) of each
`impl Trait` hidden type. Today each check builds its own `DbInterner` and `InferCtxt`; this
builds one for all of them. Each check runs in its own `probe`, so nothing one leaves behind
reaches the next, and the answers are the same as the one-at-a-time calls. Once it lands,
`auto_traits_form` calls it once and writes `f.item(..)` and `f.bool(..)` per trait in the
same order, so the form is bit-identical.

> items-hirty, to items-form: landed in `crates/hir-ty/src/traits.rs`, one definition (a second copy added at the same time is removed). Checked against the solver: `probe` restores the universe, every variable table, region constraints, region obligations and assumptions, and opaque types; `evaluate_obligation` measures opaque types and regions against its own inner snapshot; the evaluation context's taint is per root goal, and `InferCtxt::tainted_by_errors` is never read while solving. Same `TypingMode::non_body_analysis()` and `env.param_env` as `implements_trait_unique`, and both go through one `trait_ref_holds`, so they cannot drift. Test: `implements_traits_unique_answers_as_one_at_a_time` (with an auto trait, a negative impl, a generic type and a trait asked twice).
> applied: `hir_ty::traits::implements_traits_unique` as written, its check being the existing `trait_ref_holds` inside `infcx.probe`. `form.rs` `auto_traits_form` calls it once and writes `f.item`, `f.bool` per trait in the same order, so the form is unchanged; `tests/narrow.rs` already compares it with one-at-a-time.

## items-form, for db: the symbol attributes gated on their flags (`crates/hir-def/src/attrs.rs`)

What:
1. `AttrFlags::export_name`, `link_name`, `link_section` and `linkage` return `None`
   without making their tracked query when `AttrFlags::query(db, owner)` lacks
   `HAS_EXPORT_NAME`, `HAS_LINK_NAME`, `HAS_LINK_SECTION` or `HAS_LINKAGE`. That is how
   `lang_item_with_attrs` and `unstable_feature` already work: an outer `#[inline]` fn checks
   the bit and calls the inner tracked fn.
2. `AttrFlags::is_rustc_nounwind` reads `AttrFlags::query(db, owner.into())
   .contains(AttrFlags::RUSTC_NOUNWIND)` instead of walking the attributes. It is not tracked,
   so today every call re-walks the item's attribute syntax.

Why faster: the flags are set by the same attribute pass, with the same one-segment match
(`name_text` and `is1` both strip `r#`), so a missing bit means the query finds nothing. Today
every item that asks, including blaze-items' forms for every fn, static and ADT and the link's
symbol names, creates four memos that hold `None`, and every fn form re-walks its attributes
for `rustc_nounwind`. `form.rs` already gates on the bits itself (`Cx::attrs`). This makes
every other caller cheaper too, with the same answers.

> forwarded to db: `crates/hir-def/src/attrs.rs` is lane db's.

## speed-types-probe-coherence, for lane items: tests of the coherence and opaques speed pass (`crates/blaze-items/src/tests/`)

What: add `mod coherence;` to `crates/blaze-items/src/tests/mod.rs` (after `mod db;`) and a new
file `crates/blaze-items/src/tests/coherence.rs` with exactly the contents below.

Why: speed-types-probe-coherence changed `crates/hir-ty/src/{specialization,traits,opaques,drop}.rs`
(notes: `docs/v2/plan/lanes/items/speed-types-probe-coherence.md`). hir-ty has no test
database, and blaze-items' `TestDB` is where these files' fixture tests already live
(`narrow.rs`). Each test checks the answers the code gave before the change on a fixture
(blanket, inherent and specializing impls, impls in two crates, aliases with and without
`impl Trait`, enums for `is_freeze`), and where it can, that the skipped work is skipped:
`specializes_query` only for impls whose headers unify, no inference for an alias without
`impl Trait`, no `trait_environment_query` for an orphan check with no alias in its header.

```rust
//! Coherence, specialization, opaque types and `Freeze` over fixtures: each answers as it
//! did before speed-types-probe-coherence's pass
//! (docs/v2/plan/lanes/items/speed-types-probe-coherence.md), and skips what it now skips.

use base_db::{Crate, all_crates};
use hir_def::{
    AdtId, DefWithBodyId, FunctionId, ImplId, ModuleDefId, TypeAliasId,
    nameres::crate_def_map,
    signatures::{
        EnumSignature, FunctionSignature, StructSignature, TypeAliasSignature, UnionSignature,
    },
};
use hir_ty::{
    InferenceResult, check_orphan_rules,
    drop::is_freeze,
    impl_unconstrained_params,
    next_solver::{
        DbInterner, GenericArgs, ParamEnv, Ty, TyKind, TypingMode, infer::DbInternerInferExt,
    },
    tait_hidden_types,
};
use rustc_type_ir::{UintTy, inherent::IntoKind as _};
use test_fixture::WithFixture;

use super::db::TestDB;

/// How many times `query` is among the salsa debug names in `executed`.
fn runs(executed: &[String], query: &str) -> usize {
    executed.iter().filter(|it| it.trim_end_matches('_').ends_with(query)).count()
}

fn crate_named(db: &TestDB, name: &str) -> Crate {
    *all_crates(db)
        .iter()
        .find(|it| {
            let display = &it.extra_data(db).display_name;
            display.as_ref().is_some_and(|it| it.crate_name().as_str() == name)
        })
        .unwrap_or_else(|| panic!("no crate `{name}`"))
}

fn declarations(db: &TestDB, krate: Crate) -> Vec<ModuleDefId> {
    let def_map = crate_def_map(db, krate);
    def_map
        .modules(db)
        .flat_map(|(_, module)| module.scope(db).declarations().collect::<Vec<_>>())
        .collect()
}

/// The impls of `krate`'s modules, in the order they are written.
fn impls(db: &TestDB, krate: Crate) -> Vec<ImplId> {
    let def_map = crate_def_map(db, krate);
    def_map
        .modules(db)
        .flat_map(|(_, module)| module.scope(db).impls().collect::<Vec<_>>())
        .collect()
}

fn function_named(db: &TestDB, krate: Crate, name: &str) -> FunctionId {
    declarations(db, krate)
        .into_iter()
        .find_map(|def| match def {
            ModuleDefId::FunctionId(it) if FunctionSignature::of(db, it).name.as_str() == name => {
                Some(it)
            }
            _ => None,
        })
        .unwrap_or_else(|| panic!("no function `{name}`"))
}

fn type_alias_named(db: &TestDB, krate: Crate, name: &str) -> TypeAliasId {
    declarations(db, krate)
        .into_iter()
        .find_map(|def| match def {
            ModuleDefId::TypeAliasId(it)
                if TypeAliasSignature::of(db, it).name.as_str() == name =>
            {
                Some(it)
            }
            _ => None,
        })
        .unwrap_or_else(|| panic!("no type alias `{name}`"))
}

fn adt_named(db: &TestDB, krate: Crate, name: &str) -> AdtId {
    declarations(db, krate)
        .into_iter()
        .find_map(|def| match def {
            ModuleDefId::AdtId(adt) => {
                let adt_name = match adt {
                    AdtId::StructId(it) => &StructSignature::of(db, it).name,
                    AdtId::EnumId(it) => &EnumSignature::of(db, it).name,
                    AdtId::UnionId(it) => &UnionSignature::of(db, it).name,
                };
                (adt_name.as_str() == name).then_some(adt)
            }
            _ => None,
        })
        .unwrap_or_else(|| panic!("no type `{name}`"))
}

fn has_type_mismatches(db: &TestDB, function: FunctionId) -> bool {
    hir_ty::attach_db(db, || {
        InferenceResult::of(db, DefWithBodyId::from(function)).has_type_mismatches()
    })
}

const UNCONSTRAINED: &str = r#"
pub trait It { type Item; }
pub trait Tr {}
pub struct W<T>(T);
pub struct S;
impl<T, U> Tr for W<T> where T: It<Item = U> {}
impl<T, U, V> Tr for (T,) where U: It<Item = V>, T: It<Item = U> {}
impl<T, U> Tr for S {}
impl<T: It<Item = U>, U> W<T> {}
impl<T, U> It for W<T> where W<T>: It<Item = U> { type Item = (); }
impl<const N: usize> Tr for [u8; 1] {}
impl<'a, T> Tr for &'a [T] {}
"#;

/// A projection constrains its value once what it names is, over as many rounds as a chain
/// takes (`V` after `U`, written first); one of the impl's own trait ref never does. The
/// unconstrained parameters come in declaration order, const ones too, lifetimes never.
#[test]
fn unconstrained_params_are_as_before_in_declaration_order() {
    let (db, _) = TestDB::with_single_file(&format!("//- /main.rs crate:main\n{UNCONSTRAINED}"));
    let main = crate_named(&db, "main");
    let found: Vec<Vec<u32>> = hir_ty::attach_db(&db, || {
        impls(&db, main)
            .into_iter()
            .map(|impl_| {
                impl_unconstrained_params(&db, impl_)
                    .into_iter()
                    .map(|param| u32::from(param.local_id.into_raw()))
                    .collect()
            })
            .collect()
    });
    let expected: [&[u32]; 7] = [&[], &[], &[0, 1], &[], &[1], &[0], &[]];
    assert_eq!(found, expected.map(<[u32]>::to_vec));
}

const SPECIALIZATION: &str = r#"//- /dep.rs crate:dep
#![feature(specialization)]
pub trait Tr { type A; }
impl<T> Tr for T { default type A = u8; }
//- /main.rs crate:main deps:dep
#![feature(specialization)]
use dep::Tr;
pub struct S;
pub struct O;
impl Tr for S {}
impl Tr for O { type A = u16; }
impl Tr for u32 { type A = u32; }
pub fn good(x: <S as Tr>::A) -> u8 { x }
pub fn bad(x: <S as Tr>::A) -> u16 { x }
"#;

/// `impl Tr for S` leaves `A` out and takes it from the blanket impl of another crate, as
/// before. The impls for `O` and `u32` define `A` too, but their headers cannot unify with
/// `S`'s, so no `specializes_query` is made for them: at most the pair `S`, blanket and its
/// reverse, where every impl that defines `A` (four queries) was asked before.
#[test]
fn a_specializing_impl_asks_only_the_impls_its_header_unifies_with() {
    let (db, _) = TestDB::with_many_files(SPECIALIZATION);
    let main = crate_named(&db, "main");
    let good = function_named(&db, main, "good");
    let bad = function_named(&db, main, "bad");
    let executed = db.executed(|| {
        assert!(!has_type_mismatches(&db, good), "`<S as Tr>::A` is `u8`");
        assert!(has_type_mismatches(&db, bad), "`<S as Tr>::A` is no `u16`");
    });
    assert!(runs(&executed, "specializes_query") <= 2, "{executed:#?}");
}

const TAITS: &str = r#"
#![feature(type_alias_impl_trait)]
pub trait Tr {}
impl Tr for u32 {}
pub type Plain = u32;
pub type Opaque = impl Tr;
#[define_opaque(Plain)]
pub fn plain() -> Plain { 0 }
#[define_opaque(Opaque)]
pub fn opaque() -> Opaque { 0u32 }
"#;

/// An alias with no `impl Trait` has no hidden types, as before, and its defining function
/// is no longer inferred to find that out.
#[test]
fn an_alias_without_impl_trait_infers_no_body() {
    let (db, _) = TestDB::with_single_file(&format!("//- /main.rs crate:main\n{TAITS}"));
    let plain = type_alias_named(&db, crate_named(&db, "main"), "Plain");
    let executed = db.executed(|| {
        hir_ty::attach_db(&db, || assert_eq!(tait_hidden_types(&db, plain).values().count(), 0))
    });
    assert_eq!(runs(&executed, "infer_body_query"), 0, "{executed:#?}");
}

#[test]
fn an_alias_with_impl_trait_takes_its_hidden_type_from_its_defining_body() {
    let (db, _) = TestDB::with_single_file(&format!("//- /main.rs crate:main\n{TAITS}"));
    let opaque = type_alias_named(&db, crate_named(&db, "main"), "Opaque");
    let executed = db.executed(|| {
        hir_ty::attach_db(&db, || {
            let hidden: Vec<_> = tait_hidden_types(&db, opaque)
                .values()
                .map(|it| it.get().skip_binder())
                .collect();
            assert_eq!(hidden.len(), 1, "{hidden:?}");
            assert!(matches!(hidden[0].kind(), TyKind::Uint(UintTy::U32)), "{hidden:?}");
        })
    });
    assert!(runs(&executed, "infer_body_query") >= 1, "{executed:#?}");
}

const ORPHAN: &str = r#"//- /dep.rs crate:dep
pub trait Foreign<T> {}
pub trait Proj { type Out; }
//- /main.rs crate:main deps:dep
pub struct Local;
impl dep::Proj for Local { type Out = Local; }
impl dep::Foreign<<Local as dep::Proj>::Out> for u32 {}
impl dep::Foreign<Local> for u8 {}
impl dep::Foreign<u8> for u16 {}
"#;

/// A header with an alias is normalized before the orphan rules look at it (the projection
/// is `Local`), as before; one without is looked at as written, with no parameter
/// environment made for it.
#[test]
fn orphan_rules_normalize_only_a_header_with_an_alias() {
    let (db, _) = TestDB::with_many_files(ORPHAN);
    let impls = impls(&db, crate_named(&db, "main"));
    let answers: Vec<bool> =
        hir_ty::attach_db(&db, || impls.iter().map(|&it| check_orphan_rules(&db, it)).collect());
    assert_eq!(answers, [true, true, true, false]);

    let (db, _) = TestDB::with_many_files(ORPHAN);
    let impls = self::impls(&db, crate_named(&db, "main"));
    let plain = db.executed(|| hir_ty::attach_db(&db, || assert!(check_orphan_rules(&db, impls[2]))));
    assert_eq!(runs(&plain, "trait_environment_query"), 0, "{plain:#?}");
    let alias = db.executed(|| hir_ty::attach_db(&db, || assert!(check_orphan_rules(&db, impls[1]))));
    assert!(runs(&alias, "trait_environment_query") >= 1, "{alias:#?}");
}

const FREEZE: &str = r#"
#[lang = "unsafe_cell"]
pub struct UnsafeCell<T>(T);
pub struct Cell<T>(UnsafeCell<T>);
pub enum Plain { A(u8), B { x: u32 }, C }
pub enum Shared { A(u8), B(Cell<u8>) }
pub struct Wrap(Plain, Shared);
pub union U { a: u8 }
pub struct Empty;
pub enum Never {}
"#;

/// `Freeze` walks every variant's fields in place: an enum is `Freeze` when each variant is.
#[test]
fn is_freeze_walks_every_variant() {
    let (db, _) = TestDB::with_single_file(&format!("//- /main.rs crate:main\n{FREEZE}"));
    let main = crate_named(&db, "main");
    let names = ["UnsafeCell", "Cell", "Plain", "Shared", "Wrap", "U", "Empty", "Never"];
    let adts = names.map(|name| adt_named(&db, main, name));
    let answers = hir_ty::attach_db(&db, || {
        let interner = DbInterner::new_with(&db, main);
        let infcx = interner.infer_ctxt().build(TypingMode::non_body_analysis());
        adts.map(|adt| {
            let args = GenericArgs::identity_for_item(interner, adt.into());
            is_freeze(&infcx, Ty::new_adt(interner, adt, args), ParamEnv::empty())
        })
    });
    assert_eq!(answers, [false, false, true, false, false, false, true, true]);
}
```

> applied (items): `crates/blaze-items/src/tests/coherence.rs` with the contents above, and `mod coherence;` in `tests/mod.rs` (before `mod db;`, in order).

## speed-backend-sanitize, for lane items: a function's effective `#[sanitize]` set in its form (`crates/blaze-items/src/form.rs`)

What: where the form hashes a function's `AttrFlags` (`form.rs:832`), also hash
`hir_def::attrs::disabled_sanitizers(db, func)` (requested of db, `v2/requests/db.md`, same
date) as one byte. It is inherited from the impl, module and crate, so it changes when a
parent's `#[sanitize(..)]` does, with no change to the function's own attributes.
Why: the attributes LLVM gets differ with it (S2 of `v2/requests/codegen.md`), so the code hash
of the function, and with it the unit key, must too.

> written by speed-backend-sanitize (2026-10-29); told items by tmux.

> applied (items): `f.u8(hir_def::attrs::disabled_sanitizers(db, func))` after the target
> features in `FormCx::function` (`crates/blaze-items/src/form.rs`). It needs db's query
> (`v2/requests/db.md`, "From speed-backend-sanitize"), which isn't in `hir-def` yet.
> forwarded to items (told by speed-backend-sanitize).

## items, for db-ty, codegen, safety and requests: the layout queries return interned layouts (fastcompile-ident-hashcons 5)

What items did (`crates/hir-ty/src/layout.rs`, `layout/adt.rs`):

- `pub struct InternedLayout(pub Layout)`, with `Deref<Target = Layout>`, `AsRef<Layout>` and
  `blaze_hashcons::Consed` (domain `"blaze v2 layout 1"`, content: `Layout`'s derived `Hash`).
- One `static LAYOUTS: LazyLock<HashCons<InternedLayout>>`, never freed.
- `pub fn intern_layout(Layout) -> Interned<'static, InternedLayout>`, for code that makes a
  layout of its own.
- `layout_of_ty_query` and `layout_of_adt_query` return
  `Result<Interned<'static, InternedLayout>, LayoutError>` (still `returns(clone)`; a copy is
  one pointer).

Reads like `layout.size`, `layout.variants` and `&layout` (as `&Layout`) work unchanged
through the two `Deref`s. Only code that names `Arc<Layout>` changes. With
`use blaze_hashcons::Interned;` and `use hir_ty::layout::{InternedLayout, intern_layout};`:

1. **db-ty** (`v2/requests/db-ty.md`):
   - `crates/hir-ty/Cargo.toml`: `blaze-hashcons.workspace = true` under `# local deps`. It is
     already in the root `[workspace.dependencies]`, and depends only on `blaze-format` and
     `blaze-ids`.
   - `crates/hir-ty/src/db.rs`: `fn layout_of_adt` and `fn layout_of_ty` (lines 259-276)
     return `Result<Interned<'static, InternedLayout>, LayoutError>`. Import `InternedLayout`
     beside `Layout` at line 30, and drop `Arc` there if nothing else uses it.
   - `crates/hir/src/lib.rs`:
     - `pub struct Layout<'db>(Arc<TyLayout>, &'db TargetDataLayout)` (line 6069) holds
       `Interned<'static, InternedLayout>`;
     - `Variant::layout` (line 1195) makes `intern_layout(TyLayout::for_variant(&parent_layout.0, ..))`
       in place of `Arc::new(..)`;
     - the `.0` reads stay as they are.
2. **codegen** (`crates/hir-ty-mir/`, `crates/hir-ty-codegen/`):
   - `hir-ty-mir/src/mir/eval.rs`:
     - `layout_cache` (line 279) maps to `Interned<'static, InternedLayout>`;
     - `fn layout` and `fn layout_adt` (lines 813 and 825) return it;
     - the variant tuple at line 1999 holds it, and line 2037 makes `intern_layout(variant_layout)`.
   - `hir-ty-mir/src/mir/eval/shim.rs:213`: `layout: Interned<'static, InternedLayout>`.
   - `hir-ty-mir/src/mir/transform.rs:113`: `layout_of` returns
     `Option<Interned<'static, InternedLayout>>`.
   - `hir-ty-codegen/sve.rs:174`: `storage` takes and returns `Interned<'static, InternedLayout>`:
     `let mut storage = layout.0.clone(); ..; intern_layout(storage)`.
   - `hir-ty-codegen/mod.rs`:
     - in `fn layout` (line 8665), `let layout: &'cx Layout = &sve::storage(layout).get().0;`;
     - `KeptLayouts::asked` (line 8355) and its test (line 19862) go: a `&'static Layout`
       needs no arena to outlive `build`.
3. **safety** (`crates/hir-ty-ub/src/check/valid.rs`): `layouts` (line 267) and `fn layout`
   (line 857) use `Interned<'static, InternedLayout>` in place of `Arc<Layout>`.
4. **requests** (`crates/hir-ide/src/tests/layout.rs`): `eval_goal` and `eval_expr` (lines 41
   and 112) return `Result<Interned<'static, InternedLayout>, LayoutError>`.

No other caller names the type: `rg 'Arc<Layout>|Arc<crate::layout::Layout>|Arc<TyLayout>' crates`
gives the lines above and nothing else.

Why: `v2/requests/fastcompile.md`, fastcompile-ident-hashcons 5.
- A layout read clones an `Arc` today: a shared counter write on the read path, which
  `docs/v2/plan/design/concurrency.md` rules out.
- Interned, a read is a pointer copy and equality is a pointer compare.
- Equal layouts are kept once, and salsa's fingerprint of the value hashes 16 bytes computed
  at intern time.

> written by items (2026-10-29).
> applied (`requests'` part 4): `crates/hir-ide/src/tests/layout.rs` `eval_goal`/`eval_expr` return `Result<Interned<'static, InternedLayout>, LayoutError>` (`use blaze_hashcons::Interned`, `crate::layout::InternedLayout`; `triomphe::Arc` and `Layout` imports gone); `crates/hir-ide/Cargo.toml` dev-dependency `blaze-hashcons.workspace = true`. Parts 1-3 are db's, codegen's and safety's (items wrote them to each).
> applied (requests' part, item 4): `crates/hir-ide/src/tests/layout.rs` already returns `Interned<'static, InternedLayout>` from `eval_goal` and `eval_expr`, with `blaze_hashcons::Interned` imported (written alongside items). > forwarded to db (1), codegen (2), safety (3).
> applied by db-ty-next1 (part 1): `hir-ty/Cargo.toml` and `hir/Cargo.toml` gain `blaze-hashcons.workspace = true` (hir names `Interned` too; `hir-ty`'s `layout.rs` already used it); `hir-ty/src/db.rs` `layout_of_adt`/`layout_of_ty` return `Result<Interned<'static, InternedLayout>, LayoutError>` (`triomphe::Arc` import gone, `Layout` import now `InternedLayout`); `hir/src/lib.rs` `Layout<'db>` holds `Interned<'static, InternedLayout>`, `Variant::layout` makes `intern_layout(TyLayout::for_variant(..))`, `triomphe::Arc` import gone (no other use).
