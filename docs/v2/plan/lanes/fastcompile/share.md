---
status: plan
summary: blaze v2 fastcompile idea 2, shared code across generic instances (crates/hir-ty-share): the share key, a salsa query over the generic and monomorphized MIR that is equal for two instances exactly when their code is; what it reads, why it is sound, the state of the art (mergefunc, COMDAT folding, ICF, Swift, .NET, Go, polymorphization) and why deciding before codegen beats folding after.
date: 2026-10-29
---
# fastcompile-ident-share: shared code across generic instances

Idea 2 of [compile-time.md](../../design/compile-time.md). `Vec<&A>::push` and `Vec<&B>::push`
are one machine function: nothing in them tells `&A` from `&B`. blaze decides this before
codegen, as a key: two instances of one body with one **share key** are compiled once. The
code is `crates/hir-ty-share/`; the boundaries are in [notes.md](notes.md).

## The key

`hir_ty_share::share_key(db, instance, env, levels) -> Result<ShareKey, Unshared>` is a salsa
query. A hit is one lookup. The key is a BLAKE3 hash with no dense id in it: items by their
`blaze-items` keys, types by content. Two databases give one key for one source.

The key is the body's identity (its item key and code hash, or its closure's or coroutine's
code), the values of the instance's const arguments, and for each MIR optimization level the
build may compile the body at, a walk of the body.

The walk goes over the generic body and its monomorphized body in lockstep. They have one
shape, because monomorphization only puts the arguments in. Where the generic body names a
type or const parameter, it reads what the monomorphized body has there, and nothing else:

| where the generic body names a parameter | what is read |
|---|---|
| a local's type | its **fact** |
| a place that goes through a pointer | the fact of each type a `Deref` reaches |
| a constant | its value, evaluated as codegen's `eval_const` does; its type's fact |
| a promoted constant | its value, as codegen's `promoted_constant` makes it |
| a call | the callee: its own share key (below) |
| `Unsize`, `ReifyFnPointer`, `ClosureFnPointer`, `DynStar` casts | both types' full identities: the vtable or shim they make is named by them |
| another cast, an aggregate | the target's fact |
| a drop | none, or the type's identity when it needs drop: drop glue is an instance of its own |
| `asm!` `sym` | its identity |

Whether a record is written depends only on the generic body, so the records of two instances
line up one for one. A record is a tag byte and a 32-byte id.

A type's **fact** is what code that does not tell types apart reads of it (`fact.rs`):

- An ADT is its definition's identity (key, interface, layout hash, the `Drop` impls its glue
  reaches), its layout, its discriminants and its fields' facts. So `Vec<&A>` and `Vec<&B>`
  have one fact, and `A` and `B` do not, even when both are one `u32`. `repr`,
  `transparent`, lang-item ADTs and `Drop` are all in the definition.
- A tuple, an array or a closure is its layout and its parts' facts.
- A pointer is its mutability and a summary of its pointee: kind, size, alignment, and
  whether it is `Freeze`, `Unpin` and plain data. These are the facts codegen reads to say
  what a reference argument is the only way to. A fact never holds the pointee's fact, so it
  never cycles: a type reaches itself only through a pointer.
- A scalar is its identity. A function item is a value of no size, keyed where it is called or
  cast. A function pointer, a trait object or a coroutine is its full identity.
- Each fact ends with whether the type needs drop and whether it is `Freeze`.

A **callee** is keyed as codegen's walk asks for it (`walk.rs`, after `v2/refs.rs`):

- An intrinsic is classified by one exhaustive `match` over `hir_ty::intrinsic::Intrinsic`
  (`intrinsics.rs`):
  - **Layout** (most of them) reads the facts of the type arguments and the values of the
    const arguments, plus the fallback body's key where the intrinsic has one.
  - **Identity** (`type_id`, `type_name`, the `TypeId*` reflection family, the field-
    representing-type family, the `assert_*_valid` panics, `const_eval_select`, the contract
    checks) reads full identities.
  - **PerInstance** (`autodiff`, `offload`) is never shared.
  - An unknown `#[rustc_intrinsic]` is `Unshared::UnknownIntrinsic`.
- `drop_in_place` gives the drop key.
- `Fn*::call*` of a closure gives the closure body's share key; of a function item, that
  function's.
- `Future::poll`, a `dyn` method, a builtin derive, and a builtin impl's method with no body
  (`Clone` of a tuple, `FnPtr::addr`) give the full identity of the callee type.
- Any other call resolves through `lookup_impl_method`. A non-generic callee is its item key and
  interface. A generic one is its share key, or the identity of its arguments when it has none.
- A call of the instance itself is a "self" record, so direct recursion shares. Any other cycle
  of instances gives `Unshared::Recursive` to every instance in it (salsa `cycle_result`), the
  same whichever is asked first.

## Why equal keys mean equal code

Codegen compiles `monomorphized(generic_body_at(level), args)`. Its output for an instance is a
function of the reads codegen makes of that body. The walk records each read that can differ
between instances of one body:

- The places with no parameter are equal by construction.
- The places with one are recorded at least as finely as codegen reads them:
  - layouts and ABI from facts;
  - constants by their bytes;
  - each reference to another instance by that instance's own key, which is what codegen's
    `Described::args` becomes with the hook below;
  - vtables, shims and drop glue by the full identities codegen names them by.

So equal keys give equal refs, equal `built_in` and equal store keys. Codegen's own spelling
of an instance's arguments (`v2/hashes.rs`) is copied in `identity.rs`, so it is never coarser
than codegen's.

The tests check this as a property (`equal_keys_mean_one_monomorphized_body`): for each body and
each pair of arguments from a pool, equal keys mean the two monomorphized bodies print the same
once the argument's name is blanked.

## What Rust allows and what stays per instance

- **Function addresses.** Two shared instances have one address. Rust does not guarantee
  distinct addresses: rustc marks functions `unnamed_addr`, and LLVM and the linkers may merge
  them. A pointer to one instance still equals itself. `programs/fn_pointers.rs` prints only
  what is guaranteed.
- **Statics.** The brief and [compile-time.md](../../design/compile-time.md) say statics stay
  per instance. They are not per instance in Rust: a `static` in a generic function is one item
  for every instance (the Reference, "Statics & generics"). Sharing changes nothing about them,
  and the walk reads nothing for an `OperandKind::Static`. `programs/statics.rs` checks that a
  count goes on across types under rustc. (Request: fix the sentence in compile-time.md.)
- **`TypeId`, `type_name`, vtables, `dyn` calls, closures and coroutines** are read by full
  identity, so they stay per instance.
- **Debug info and names.** A shared body carries the first instance's debug info. Nothing in
  the program can see it. Generic instances have no symbols in v2 (`symbol_of` gives `None`).

## State of the art, and why before codegen

- **LLVM MergeFunctions** (`-mergefunc`) compares functions after optimization and replaces
  duplicates with aliases or thunks. rustc enables it by default (`-Z merge-functions=aliases`)
  in optimized builds only. Every copy is still generated and optimized before it is found.
- **MSVC `/OPT:ICF` and COMDAT folding** fold byte-identical COMDAT sections at link time.
  **lld `--icf=safe|all`, gold `--icf`, mold `--icf`** do the same, iterating over relocation
  graphs. Every copy is still generated, optimized, emitted and written to an object. Folding
  saves binary size and link input, not compile time.
- **Swift** compiles one generic body per function and passes type metadata and witness tables
  at run time, specializing only as an optimization. This shares everything, at the cost of
  dynamic dispatch, which Rust semantics and performance rule out.
- **.NET** shares one body for all reference-type instantiations (`__Canon`) with a runtime
  generic dictionary, and specializes value types. **Go 1.18+** uses "GC shape stenciling": one
  body per GC shape (all pointers share), plus a dictionary. Both are the closest to this design.
  The share key is a shape test with no dictionary: whatever would need a dictionary (type
  identity, a resolved method) is in the key instead, so shared code never looks anything up at
  run time.
- **rustc polymorphization** (`-Zpolymorphize`, removed in late 2024) shared instances whose
  *unused* parameters differ. It never shared instances whose used parameters had one layout.
  It was removed because it was buggy and gained little. The share key subsumes it: an unused
  parameter is never read.

Deciding before codegen beats folding after:

- **No duplicate is ever compiled.** Codegen, optimization, the store and the link see one body.
  Folding needs every copy to exist first.
- **The decision is a cache key.** It is reused across builds, crates and machines, so the global
  cache (idea 8) and lazy compilation (idea 1) get it for free. ICF happens once per link.
- **It is semantic, not bytewise.** Two instances share when their callees share, transitively,
  even though their relocations name different instances. ICF has to iterate to find that, and
  then only when every byte agrees.
- **It works in debug builds.** That is where compile time matters, and where rustc runs no
  mergefunc.
- **It keeps Rust's semantics exactly.** Only what Rust leaves unspecified (addresses) changes.
  ICF `all` can fold functions whose addresses C++ guarantees distinct.

Folding after is still a complement. Two *different* generic functions with identical code are
not one key here, and `--icf=safe` at link time could catch them.

## The hook (requested of codegen)

`v2/requests/fastcompile.md`, `fastcompile-ident-share 1`:

- `hir-ty-codegen`'s `v2/frontend.rs` `described()` puts `share_key` in `Described::args` in
  place of `args_of`, hashed under its own domain, when the key is `Ok`.
- An `Err` keeps `args_of`, as now: that is the per-instance path, not a fallback, because both
  give exact code.
- The levels passed are those the build may compile at (`lang: {Release}`,
  `other: {Debug, Release}`, plus `Off` under `BLAZE_MIR_OPT=0`).

## Left for later

- **Drop glue and shims** (`DropGlue`, `CallOnceShim`, `VtableShim`, `FnPointerShim`,
  `ReifyShim`) are `Unshared::Shim`. Drop glue could share by a structural key (the facts of
  each field it drops, recursively), which would also let a drop inside a shared body key by
  that instead of by identity.
- **Vtables** are keyed by full identity. A finer key (the shared keys of each method in it)
  would share `&dyn Trait` coercions of types whose methods share.
- **`Future::poll` of a coroutine** is keyed by identity, as codegen calls the coroutine's body.
- **Mutual recursion** is unshared. A fixpoint over the cycle would share it.
- **The MIR levels.** `hir-ty-codegen` uses `MirOptions`, which is not in `hir-ty` on disk. The
  key is over `MirOptLevel`s, the interface `HirDatabase::optimized_mir_body` has. If codegen's
  body also depends on ub-checks or other options, they go into `MirLevels`.
