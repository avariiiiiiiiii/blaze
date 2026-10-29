---
status: plan
summary: blaze v2 codegen lane's design: per-function units, instance keys, templates (the back-end memo up to callee renaming), code dedupe, folding by constants, settings and tiering, release summaries and imports.
date: 2026-10-29
---
# blaze v2: the codegen lane's design

How [codegen.md](v2/codegen.md) is built, as of 2026-10-29. The crate is `blaze-codegen`
(backend-agnostic, no front end); the backends are `hir-ty-codegen`'s `v2` module (Cranelift,
LLVM) over the code generator that exists.

## The unit is one function instance

A build ([`driver`](../crates/blaze-codegen/src/driver.rs)) starts from its roots (a `main`,
the tests, a library's exported functions) and walks what each instance refers to. Nothing is
grouped by crate: every instance is one task on all cores, and a crate's size never makes a
tail.

For each instance the front end ([`Frontend::describe`](../crates/blaze-codegen/src/backend.rs))
says, without compiling it: its item, its code hash, the content ID of its type arguments'
facts, whose code it is (`plan::Origin`), and everything it refers to in the order of its MIR
(`Described::refs`). The build's policy (`plan::Policy::choose`) gives it a backend and its
`settings::Spec`; `Spec::settings` hashes every field. Those make its `Instance` and
`InstanceKey`.

## Templates: why the store never holds a function's callees

A `CompiledFunction`'s relocations name callees by `InstanceKey`, which holds the callee's
code hash. The caller's key takes the callee by its interface hash. So a callee's body edit
changes the callee's key and not the caller's: anything kept under the caller's key that named
the callee would be stale.

So the store and the memo hold a [`Template`](../crates/blaze-codegen/src/template.rs): the
function with each function, static and outside symbol it refers to by its place in
`Described::refs`, and each of its own data objects by its place among its data. A build
instantiates the template with the keys this build gives the places. A callee edit renames
relocations of its callers, and compiles none of them.

The places come from the front end's walk of the MIR, not from the IR build. A backend that
needs a reference the list lacks fails the unit: the list is what makes templates exact.

`Target::Function` and `Target::Direct` are written as one kind of instruction (a direct
call, or a direct address); a `Function` target is the callee's jump-table stub, which the
linker owns. Which kind a place is changes no byte of the template.

## The back-end memo up to callee renaming (#1291)

Two instances whose IR, with references by place, is equal compile once: the backend gives
the prepared IR's canonical bytes (`Shape`), and the memo maps `Shape::key` (with the
settings and architecture) to the template. `Vec<A>::drop` and `Vec<B>::drop` differ only in
which `drop_in_place` they call: one shape, one compile, two instantiations. Constants,
layouts and symbols are in the shape, so the sharing is exact.

The store key of an instance's template is its `InstanceKey` (dev) or
`Imports::blob_key` (release), so a warm build builds no IR for an unchanged instance.

## Instance keys

`Instance::args` is the content ID of the type arguments, each written by content: an ADT by
its item hashes, never by name or path, so the same instance is one key across crates,
branches and projects. Where two instances read only facts their type arguments share
(`Vec<A>::push` and `Vec<B>::push` with one layout), the memo above makes them one compile.
Finer keys from the facts a generic's code reads are a refinement of `args` the items lane's
facts allow without changing anything else here.

## Code dedupe

After instantiation, functions whose `content::code_id` is equal are one function: callers'
relocations name one representative (the least key), to a fixpoint, as ICF merges. Never
merged: a function whose address is `Address::Significant` (an exported symbol), any static
(statics are `Target::Static`, by which item they are), and a patchable function of a live
build (each keeps its slot). Rust functions' addresses are not unique (rustc marks them
`unnamed_addr`), so merging the rest changes no program. Data objects are one per content ID.

## Folding functions that differ only in constants (#1292)

On Cranelift units, when `Spec::fold`: shapes with their constants masked are grouped; a group
whose members differ in at most 16 constants and saves more than the thunks cost becomes one
body that takes the differing constants as parameters, and each member a thunk that calls it.
Off by default for LLVM's levels. See [`fold`](../crates/blaze-codegen/src/fold.rs).

## Settings and the plan (#635, #807, #1404)

- The profile's `opt-level` with cargo's package overrides and `build-override` picks the
  backend per origin; tests are built at the profile's level.
- The standard library's own functions are compiled as rustc ships std (`-O3`, no overflow
  checks, no debug assertions, line tables); a generic of std as the crate instantiating it.
- Only members of a live build are reached through slots; everything else is called directly.
- A live build starts LLVM-level functions at Cranelift and replaces them in their slots when
  LLVM is done; a build whose output is a file compiles each function once at its level, so
  its bytes never depend on timing.
- `Spec::policy` is the policy's content ID: a caller's relocations name callees by keys made
  with the callees' settings, so a change to how anything is compiled is in every key.

## Release: summaries, then imports, then compiles (#899, #176)

1. Every LLVM unit's summary (`summary::Summary`, from its prepared IR after LLVM's pre-link
   pipeline) and export (that IR, as importers link it), stored under its instance key.
2. `imports::decide` over all summaries: which callees each function imports (ThinLTO's
   evolution factor, depth and budget), arguments every caller passes as one constant, callee
   attributes, dead arguments. Only functions that nothing outside the build calls get
   whole-program facts.
3. Each compile with its imports, its template stored under `Imports::blob_key`; the callees
   it built in are `CompiledFunction::inlined`.

## Queries

The codegen query kinds are in [`query`](../crates/blaze-codegen/src/query.rs), in
`blaze_query::kinds::BACK`.
