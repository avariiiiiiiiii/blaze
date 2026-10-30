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

## items-form: `VTableMap::tys` (`crates/hir-ty/src/mir.rs`)

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

## items-form: `tait_hidden_types` public (`crates/hir-ty/src/opaques.rs`), optional

What: `pub(crate) fn tait_hidden_types` → `pub fn`.

Why: `Cx::hidden` gets a TAIT's hidden type through
`DbInterner::type_of_opaque_hir_typeck(OpaqueTyIdWrapper(..))`, which reads it. Reading the
query itself would say what is meant. Not blocking.

## items-form, for lane items: who observes a type's name through a generic wrapper

`Cx` spells the `item_key` and path of nominal types only where `type_name` / `type_id` (the
intrinsics) are called directly, and for the `TypeId`s a constant holds. A body that calls
`std::any::type_name::<A>()` (a generic fn that calls the intrinsic) gets `A` as a slot, and
the slot's hash does not include `A`'s name, so renaming `A` keeps that body's hash while the
program's output changes. Fixing it needs a transitive fact: "this generic fn, instantiated
with these parameters, observes their names", computed over the call graph, and the form
spelling names for a call to any fn with that fact. That is a cross-item fact, which is the
parent's (`hash`/`facts`), not something one form can see.

## items-form, for lane items: share type forms across items and revisions

`form::Cx` memoizes a type's `Sub` in a `RefCell<FxHashMap<Ty, Rc<Sub>>>` that lives only as
long as one `Cx`. Every item's form re-spells the types it shares with others (`Vec<T>`,
`Option<&str>`, every std type in a signature), and every revision re-spells them all. A
salsa-tracked `ty_form(db, Ty) -> Sub` would compute each once per revision and keep it across
edits that don't touch it. That needs `Sub: salsa::Update` (its fields are `ContentId`,
`Vec<ItemId>`, `Option<&'static str>`), and the `spelling` cycle guard turned into salsa cycle
handling. I didn't make the change without a compiler to check it; the parent, which builds,
should decide where it goes and measure it.

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

Why: otherwise a rename keeps a stale `type_name` string or `TypeId` (docs/v2/items.md 6.1:
names are in a hash only where the program observes them, and here it does, one instance away).

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
