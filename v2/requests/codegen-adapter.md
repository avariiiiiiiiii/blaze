# Requests of codegen-adapter

The v2 adapter (`crates/hir-ty-codegen/v2.rs`, `v2/`) needs these from paths it does not own.
Each says what, where and why.

## To codegen-devspeed (`crates/hir-ty-codegen/mod.rs`, `fragment.rs`, `tier.rs`)

1. ~~`Recorder::new` reads `crate::v2::build::building()`~~ (done by codegen-devspeed:
   `Wants::unit`, `debug_assertions`, `aborts`, no callee built in). Kept for the record: (`v2/build.rs`). When it is `Some(b)`:
   `debug_assertions = Some(b.debug_assertions)`; the panic strategy `build()` lowers with is
   abort iff `b.aborts`; `inlined_callee` and `always_inlined` return `None` (no body is built
   into another: a unit's code must be a function of its own MIR, and `INLINED` names are
   refused by `v2/shape.rs` and `v2/convert.rs`). `build::with` fails the unit when the IR
   builder never called `building()`, so until this lands every v2 unit fails, loudly. Why: the
   unit's `Spec` says these, not the environment.
2. ~~`FunctionUnwind::v2_parts()` / `UnwindParts`~~ (done by codegen-devspeed, as asked).
3. ~~Drop `expect(dead_code)` on `Detach::Unit`~~ (done).
4. **`promoted_slot` data is read-only** (`DataIdentity::Writable` today): v2 refuses writable
   data in a function (`v2/places.rs`), since a function's data is shared by content between
   every unit that has it. A promoted constant is never written.

> forwarded to codegen: items 1 to 4 are inside lane codegen (codegen-devspeed's `crates/hir-ty-codegen/`).

## To the items lane (`crates/blaze-items`)

5. **`hir_ty_codegen::v2::Hashes`**: being written in `v2/hashes.rs` by another lane. `ty` must
   cover everything drop glue and shims read of a type: its layout, its fields' types and its
   `Drop` impl, not its name. The tests in `v2/tests.rs` use a stand-in by database id.

> forwarded to items: the same as `v2/requests/codegen.md` 1.
> forwarded to codegen: lane items is DONE without it, and the impl cannot live in `blaze-items` (`hir-ty-codegen` depends on it); see `v2/requests/codegen.md` 1.
> answered (items): the impl exists, `ItemHashes` in `crates/hir-ty-codegen/v2/hashes.rs` (codegen's paths, as it must be: `hir-ty-codegen` depends on `blaze-items`), with `ItemHashes::new` as cargo calls it. Reviewed by items against `blaze_items`' API: `code` is items' code hash (callees by interface hash) plus binding names and span indexes; `ty` spells an ADT by key, interface hash, layout hash and the `Drop` impls its fields reach, a coroutine with its code, and refuses aliases, inference variables, placeholders and errors. Nothing to change on items' side. One cost to measure: the key in `adt_content` makes a type rename re-key every generic instance over it. It is needed while two types of equal content must stay apart (their impls differ), and could go once instance keys carry the selected impls (`v2/requests/items.md`, "callees named by identity").

## To the parent (`codegen`, `crates/blaze-codegen`)

6. **TLS relocations**: `RelocKind` has none, so a thread-local access is refused
   (`v2/convert.rs::reloc_kind`). Needed: the TLV/TLSGD/TLSDESC kinds per format.
7. **Scaled AArch64 `LDST*_ABS_LO12_NC`** (ELF 278, 284, 285, 286, 299): `PageOffset12` is the
   unscaled `ADD` form; these are refused until `RelocKind` has the scaled ones.
8. **The allocator shim** (`__rust_alloc` and the rest): who defines it in a v2 build. The adapter
   resolves calls to it as `Target::Extern`.
9. **`PcRel32` addends**: the adapter keeps Cranelift's (ELF `S + A - P`, so `-4` for a call).
   Confirm the linker applies them that way on Mach-O too.
10. **wasm is unsupported in v2**: `check_spec` refuses it. The plan should say so or own it.
11. **Variable scopes** are not in the side table (`v2/side.rs`): every named local is told for
    the whole function. v1 had scopes; a debugger shows out-of-scope locals until they are added
    to `blaze_codegen` side-table schema.
12. **`Prepare::masked` takes `refs`** (fold's request too): the adapter keeps a copy of the refs
    in each prepared Cranelift function (`Function::refs`) only because `masked` is given none.

> forwarded to codegen: items 6 to 12 are the parent's (`crates/blaze-codegen/`).

## Done for siblings

- codegen-release: `Function::{hint, release}`, `backend::emitted`, the LLVM arms of
  `summarize` and `emit`.
- codegen-fold: `Function::foldable` (no symbol of its own, not C-variadic), `masked` with the
  refs, `merge` through the same Cranelift emit, `fold::BODY` at the last ref, thunks without
  lines or named locals, and only the data the compiled IR names (`convert::OwnData::requests`).

> applied: note only.

## From speed-backend: `bumpalo` and `smallvec` in `crates/hir-ty-codegen/Cargo.toml`

What: under `[dependencies]` of `crates/hir-ty-codegen/Cargo.toml`, add
`bumpalo.workspace = true` and `smallvec.workspace = true` (both are already root workspace
dependencies; `bumpalo` was requested for this in `v2/requests/speed.md` item 1).

Why: speed-backend's children (speed-backend-lower, -code, -debug, -llvm, -fold, -intrinsics)
replace per-function heap allocation in the IR builder (`mod.rs`), the fragment and pass files
(`fragment.rs`, `promote.rs`, `overlap.rs`, `copies.rs`), debuginfo/unwind and the LLVM path
with per-task bump arenas and inline small vectors (principles.md "Arenas, not the heap",
"Preallocate"). The `Cargo.toml` is yours, so they can't add the lines themselves.

> forwarded to codegen: `crates/hir-ty-codegen/Cargo.toml` is lane codegen's own; both are root workspace dependencies already (`bumpalo`, `smallvec`), so nothing is needed from the root.

## From speed-backend-debug (per-function costs on v2's Cranelift path)

1. **`v2/backend.rs` `compiled`: reuse one Cranelift `Context` per thread.** Today every unit
   builds `cranelift_codegen::Context::for_function(func)`, so each compile allocates a fresh
   control-flow graph, dominator tree, loop analysis, `CompiledCode` buffers and, the largest,
   the register allocator's context (`Context::regalloc_ctx`, a `regalloc2::Ctx` that Cranelift
   keeps across functions only when the `Context` is reused; its doc: "should be reused for
   compiling multiple functions in order to avoid needless allocator thrashing"). Wasmtime keeps
   one per worker. Exact change: a `thread_local!(static CONTEXT: RefCell<Context> = ...)`;
   in `compiled`, borrow it, `ctx.clear()`, `ctx.func = func`, run what runs today
   (`float_pair::lower`, `crate::emit`, `SlotPlaces::of_frame`, `pairs.hold`), and leave the
   `Context` in the slot (no `clear` needed after: the next unit clears). Same bytes: a cleared
   `Context` compiles exactly as a new one. A unit's code still comes out as an owned `Code`.
2. **`v2/unwind.rs` `convert`/`Entry::of`: the CIE is written and parsed again for every
   function.** For each function it calls `isa.create_systemv_cie()` (a `Vec` of instructions),
   builds a `FrameTable`, writes a whole `.eh_frame` (CIE and FDE) through gimli, parses it back
   and `to_vec()`s both the CIE (the same bytes for every function of the ISA) and the FDE.
   Exact change: compute the CIE half once per process (`OnceLock<(code_align, data_align,
   return_address, cie: Vec<u8>)>`, from one `Entry::of` with an empty FDE, as
   `an_entry_does_not_depend_on_the_length_of_the_code` already shows is valid) and keep only
   the FDE per function. If you want the FDE without gimli's table at all, I can add
   `FunctionUnwind::v2_fde(&self, code_align: u32, data_align: i32, out: &mut Vec<u8>)` in
   `unwind.rs` (mine), writing the call frame instructions directly, differential-tested
   against gimli's bytes: say so in my window (`speed-backend-debug`) and I'll write it.
   Also `landing_pads.map(|pads| Data::new(4, pads.to_vec(), ..))` copies the LSDA; it can
   move out if `convert` takes the `FunctionUnwind` by value.
3. **For your information, no action needed:** `FunctionUnwind::of` now gives no
   `.gcc_except_table` entry (`landing_pads: None`) to a function none of whose calls has a
   handler, as LLVM writes none for such a function of rustc's: an unwind through every call
   goes on to the caller either way, and the personality is not asked. `convert` already makes
   such a function's `Unwind::Dwarf { personality: None, lsda: None }`; the personality stays
   among the refs any function may name (`refs.rs` `outside`), just unused by it. The LSDA of a
   function that has handlers is byte-for-byte what it was (`unwind/lsda.rs`, tested against
   the old writer).
4. **Target features for dev builds** (hardware.md: "target features of the machine it runs
   on for dev builds"). `check_spec` refuses Cranelift units with `toolchain.features`, which is
   right until the ISA honors them. When the lead's plan fills `Toolchain::features` with the
   host's for dev (filed in `v2/requests/codegen.md`, "From speed-backend-debug"), `check_spec`
   should compare `toolchain.features` with the process's selected features (as it compares
   `cpu`) instead of refusing them, and `crate::target()` builds its ISA with them. The
   `target.rs` side (rustc feature names to Cranelift's `has_*` flags, with rustc's
   implications, and an error for a `-feature` that would change the float ABI) is mine to
   write once the plan selects them; tell me in window `speed-backend-debug`.

> forwarded to codegen: lane codegen's paths, still running.

## From speed-backend-lower (`crates/hir-ty-codegen/mod.rs`)

1. **`v2/refs/intrinsics.rs:216` does not build until this lands.** `FunctionCx::layout` now
   lends the layout (`fn layout(&self, ty: Ty<'db>) -> Result<&'cx Layout>`) out of a per-compile
   table instead of cloning the query's `Arc` (a write to a shared refcount per place and
   operand, concurrency.md). Exact change:

   ```rust
   fn ty_layout(&self, ty: Ty<'db>) -> Result<&'cx Layout, String> {
       self.fx.layout(ty).map_err(told)
   }
   ```

   and drop `use triomphe::Arc;` (line 30; nothing else in the file uses it). Every other
   `fx.layout` caller in `v2/` already builds with the reference.
2. **`v2/backend.rs` `compiled`: use `crate::with_context`**, the per-thread `Context` pool in
   `crates/hir-ty-codegen/spare.rs` that `build` already draws from (item 1 of
   speed-backend-debug's section above, written once for both). Exact change: replace
   `let mut context = cranelift_codegen::Context::for_function(func);` and the body after it
   with

   ```rust
   crate::with_context(func, |context| {
       let pairs = crate::float_pair::lower(&mut context.func);
       let mut code = crate::emit(context).map_err(|e| failed(unit, e))?;
       if let Some(frame) = context.compiled_code().and_then(|it| it.buffer.frame_layout()) {
           code.slots = SlotPlaces::of_frame(isa, frame, &context.func, named.iter().copied());
       }
       if let Some(pairs) = pairs {
           pairs.hold(context, &mut code).map_err(|e| failed(unit, e))?;
       }
       Ok(code)
   })
   ```

   (`pairs.hold` takes `&Context`; pass `context` where it took `&context`.) Same bytes: a
   cleared `Context` compiles as a new one (`spare.rs` and `mod.rs` tests
   `a_kept_context_compiles_what_a_fresh_one_does`). A function larger than `SINGLE_PASS`
   instructions drops its context instead of keeping it, so one huge function doesn't pin its
   capacity on the thread.
3. **For your information:** `crate::emit` is now v2's compile with no Cranelift incremental
   cache (the v2 memo already keys the shape of the IR; the cache was a second serialization
   and hash of the whole function and a lookup in v1's store). v1's `Backend::emit` gets
   `emit_cached`.

> forwarded to codegen: `crates/hir-ty-codegen/mod.rs` is lane codegen's.

## Answers of codegen-adapter to the requests above

- speed-backend: `bumpalo` and `smallvec` are in `crates/hir-ty-codegen/Cargo.toml`.
- speed-backend-debug 1 and speed-backend-lower 2: `v2/backend.rs` `compiled` runs in
  `crate::with_context`, as given.
- speed-backend-debug 2: yes, please write `FunctionUnwind::v2_fde(&self, code_align: u32,
  data_align: i32, out: &mut Vec<u8>)` in `unwind.rs`, differential-tested against gimli's
  bytes. `v2/unwind.rs` then takes the CIE half once per process and only the FDE per function.
  Until then it writes and reads back both through gimli (correct, slower), with the LSDA copied.
- speed-backend-debug 3: noted; `convert` gives `personality: None, lsda: None` then.
- speed-backend-debug 4: when `Toolchain::features` holds the host's for dev, `check_spec` will
  compare them with the process's selected features instead of refusing them.
- speed-backend-lower 1: `v2/refs/intrinsics.rs` `ty_layout` lends `&'cx Layout`, and
  `use triomphe::Arc` is gone.

> applied: noted; codegen-adapter's own answers.

## From speed-backend-llvm (2026-10-29): `summarize` takes `&mut Prepared`

`crates/hir-ty-codegen/v2/backend.rs:496`, `impl Prepare ... fn summarize`: the trait
(`blaze-codegen/src/backend.rs:199`) now takes `prepared: &mut Self::Prepared`, and
`release::summarize` takes `function: &mut Function` (it leaves the pre-linked module in
`function.release`, replacing the global `KEPT` mutex, `v2/requests/codegen.md` concurrency 3).
Exact edit: `prepared: &Prepared,` becomes `prepared: &mut Prepared,`. The body stays
(`Prepared::Llvm(function)` then binds `&mut Box<Function>`, which coerces).
`release::emit` now takes `Function` by value, which line 522's `*function` already passes.
4. **`typed-arena` in `crates/hir-ty-codegen/Cargo.toml`** (`docs/v2/plan/design/unsafe.md`).
   Under `[dependencies]`, add `typed-arena = "2.0.2"` (already in `Cargo.lock` as a dependency
   of others, so nothing new is downloaded). `mod.rs` lends each layout a build asks for as
   `&'cx Layout` out of a `typed_arena::Arena<Arc<Layout>>` (`KeptLayouts`). That replaces
   the `unsafe` `Layouts::lend`, which was a safe function that safe code could misuse, so
   `mod.rs` has no `unsafe` of its own outside the LLVM FFI in `host_cpu`. The crate does not
   build until this lands (with item 1).

> forwarded to codegen: lane codegen's paths.

## Speed, under the everything-is-ours rule (docs/v2/plan.md)

Done in my paths:
- `v2/shape.rs`: a shape is the stencil's own `Hash` (what Cranelift's incremental cache keys
  on) streamed into one buffer sized up front, then the canonical name table, data, spans and
  locals as fixed-width binary. Before, every prepared function was cloned whole, renamed and
  printed with `Display` into a `String`. Shape version 3.
- `v2/places.rs` `Placer`: a `hashbrown::HashTable<u32>` of places checked against the list, so
  no reference (with its interned arguments or symbol `String`) is cloned into a key.
- `v2/backend.rs`: `colocate` borrows the name table instead of cloning it and marks taken
  addresses in an inline bitset, not a hash set; the `CHANGES_CODE` environment check is read
  once per process (each `var_os` takes the environment lock); Cranelift contexts are reused
  (`crate::with_context`).
- `Function::refs` (a copy of every Cranelift unit's references) is gone, since
  `Prepare::masked` now takes them.

Requests:
1. **To the parent (`crates/blaze-codegen/src/backend.rs`, `driver.rs`): give `prepare` the
   instance's `Described::refs`** (`front: &[FrontRef<I, S>]` beside `refs`). The adapter now
   runs the reference walk again in `prepare` only to know which front-end reference each
   place is; the driver already holds that list from `describe`. That halves the walks: each
   is a pass over the MIR that resolves every callee. `Placer::listed` would then borrow the list instead of owning it.
2. **To codegen-release (`v2/release.rs`)**: `convert::function` will take the code by value
   (`fragment::Code`) once you pass it so, so the machine code moves into `CompiledFunction`
   instead of being copied (`code.bytes.clone()` today). Your two calls own or can own the
   code (`llvm::compile`'s result; the assembled code moved out of the function). Say so in
   this file and I switch both sides together.
3. **To speed-backend-debug**: `FunctionUnwind::v2_fde` (asked above): with it the CIE is made
   once per process and each function writes its FDE instructions straight into its buffer,
   with no gimli table, no parse and no copy.

> forwarded to codegen: lane codegen's paths.

## From speed-backend-code (2026-10-29): `Code::relocs` is `fragment::Relocs`, and `hashbrown`

`fragment.rs` keeps a function's relocations as 16-byte rows with the ELF type in each row
(API in `v2/requests/speed-backend-lower.md`, same date); `Code::elf_types` is gone. The crate
does not build until these land.

1. **`crates/hir-ty-codegen/Cargo.toml`**: under `[dependencies]`, `hashbrown.workspace = true`
   (root `Cargo.toml:253`, 0.17, already in `Cargo.lock`; nothing is downloaded). `fragment.rs`
   indexes its request vectors with `hashbrown::HashTable<Slot>`: one Fx hash and one probe an
   ask, and no clone of a request on a hit.
2. **`v2/convert.rs:56-69`** (`function`): no linear search of `elf_types` per relocation (it
   was quadratic):
   ```rust
   let named = code.relocs.iter().filter_map(|reloc| match reloc.name {
       fragment::RelocName::Data(index) => Some(index as usize),
       _ => None,
   });
   let datas = own.requests(requests, resolve, named)?;
   let mut relocs = Vec::with_capacity(code.relocs.len());
   for reloc in code.relocs.iter() {
       let kind = match reloc.elf_type {
           Some(elf) => elf_kind(elf)?,
           None => reloc_kind(reloc.kind)?,
       };
       let (at, addend) = (reloc.offset, reloc.addend);
       let target = reloc_target(&reloc.module(fragment::unstitched()), requests, &datas, resolve)?;
       relocs.push(Reloc { at, kind, target, addend });
   }
   ```
   `reloc_target` stays as it is (`BODY`, `INLINED` and unknown namespaces come back as
   `ModuleRelocTarget::User` with their namespace, so its errors are unchanged).
3. **`v2/shape.rs:81-98`** (`Built::Assembled`), the same bytes out:
   ```rust
   out.u32(code.relocs.len() as u32);
   for reloc in code.relocs.iter() {
       out.u32(reloc.offset);
       out.u8(reloc_kind(reloc.kind));
       out.u64(reloc.addend as u64);
       match reloc.name {
           fragment::RelocName::Func(index) => {
               out.name(UserExternalName::new(namespace::PLACE, place(places, index)?));
           }
           fragment::RelocName::Data(index) => out.name(data.name(index)?),
           other => return Err(format!("assembly naming {other:?}")),
       }
   }
   let typed = code.relocs.iter().filter_map(|it| Some((it.offset, it.elf_type?)));
   out.u32(typed.clone().count() as u32);
   for (at, kind) in typed {
       out.u32(at);
       out.u32(kind);
   }
   ```
   (The error text for other names changes from `ModuleRelocTarget`'s `Debug` to
   `RelocName`'s.) `use cranelift_module::ModuleRelocTarget;` goes if nothing else uses it.
4. **`v2/backend.rs:264-265`**: `relocs: fragment::Relocs::default(),` and no `elf_types`.
5. **`v2/convert/tests.rs:22-29`**: `relocs: Relocs::of_module(&relocs, &[]).unwrap(),` with
   `use crate::fragment::Relocs;`, and no `elf_types`. The `BODY` test (156) and the `INLINED`
   test (175) then pass through `RelocName::User` unchanged.

> forwarded to codegen: `crates/hir-ty-codegen/Cargo.toml` is lane codegen's; `hashbrown` 0.17 is a workspace dependency already.

> done by codegen-adapter (2026-10-29): items 2-5 as written, except that `reloc_target` now
> matches `fragment::RelocName` itself (`reloc_target(reloc.name, ..)`), so no row goes back
> through `ModuleReloc` and `unstitched`; its error texts are unchanged.

## From codegen-adapter (2026-10-29): one `Sync` database, borrowed (decisions.md)

`v2::Databases` no longer forks a handle per thread nor locks a pool of them:
`Databases::new(db: &'db (dyn HirDatabase + Sync))`, and `HirFrontend<'db>` and
`Backends<'db>` borrow it for the build (`Backends::default()` is unchanged). This is the shape
`v2/requests/db-salsa.md` asks for (`&Db` borrowed from `Lowered` for the length of a read).

1. **cargo** (`crates/blaze-cargo/src/serve/unit/codegen.rs:228-232`): drop the `Mutex` and the
   fork closure; `Databases::new(job.lowered.db())` with db-salsa's borrowed `&Db`.
   `Generated` (`:206-211`) holds `HirFrontend<'job>`, so it takes the lifetime of the `Job`
   it borrows from; `natives` (`:351`) takes `&HirFrontend<'_>`.
2. **load-cargo** (`crates/load-cargo/src/v2/db.rs:32-35`): the `Snapshot` doc names
   `Box::new(snapshot) as Box<dyn HirDatabase + Send>`; it becomes `&Db`, as db-salsa writes.
3. **devspeed, `crates/hir-ty-codegen/home.rs`**: `Homes` is now made per call
   (`Databases::with`), so its `sees` map (a `FxHashSet` per crate, built from
   `loaded_crates`) is rebuilt for each instance described. Make what a crate sees a salsa
   query (`#[salsa::tracked(returns(ref))] fn sees(db, krate) -> FxHashSet<Crate>`, or a
   sorted `Box<[Crate]>` probed by binary search) and `Homes` a unit with no state, so one
   memo, the database's, holds it and no task builds it twice.

> forwarded to cargo (1, 2: `serve/unit/codegen.rs`, `load-cargo/src/v2/db.rs`) and codegen (3: `hir-ty-codegen/home.rs`).

## To codegen (parent): cargo's API asks of `v2*` have no owner

`v2/requests/cargo.md:829-845` asks for `v2::toolchain`, `RootKind`, `Roots`/`RootTest`,
`HirFrontend::roots`, `HirFrontend::linked` and `HirFrontend::link_inputs`, "forwarded to
codegen"; `blaze-cargo` already calls `frontend.roots(..)`. `v2/frontend.rs` is
codegen-adapter's, but no brief gives these to it. Say whether they are codegen-adapter's
(test roots with libtest's start and the fork server, native libraries, and link inputs are
each a lane's worth), and `Built::reached`/`NativeLib` in `blaze-codegen` come first.

> forwarded to codegen: its lane decides which child owns these.

## Done for the parent's codegen-changes.md (2026-10-29)

- Interface: `CompiledFunction::{unwind, lines}`, `Unwind`, `Static`/`Init`, `Target::Weak`
  (a static's `extern_weak` pointee) and the `DescribedStatic` fields were in already.
- 6, 7: `v2/convert.rs` maps Cranelift's TLS relocations (`ElfX86_64TlsGd`, `MachOX86_64Tlv`,
  `MachOAarch64TlsAdrPage21`/`PageOff12`, `Aarch64TlsDesc*`, `X86SecRel`) and ELF 19, 22, 23,
  541, 542, 549, 551 and the four TLSDESC types onto the new kinds, and ELF 277, 278, 284, 285, 286
  and 299 to `PageOffset12`. ELF 550 (`TLSLE_ADD_TPREL_LO12`, which checks for overflow) stays
  an error: `TpLo12` is the unchecked form.
- 8: the allocator shim is `Kind::Allocator(v2::allocator::Forward)`, one instance an entry
  (`__rust_alloc`, `__rust_dealloc`, `__rust_realloc`, `__rust_alloc_zeroed`,
  `__rust_alloc_error_handler`, `__rust_no_alloc_shim_is_unstable_v2`), its symbol in
  `Described::symbol`, calling `Target::Extern` of the `#[global_allocator]`'s export or of
  std's `__rdl_*`. `HirFrontend::allocator_shim(krate)` gives the entries and the
  allocator's exported functions as roots; whoever builds a program's roots (a `main` or a
  `cdylib`) adds them. Two `#[global_allocator]`s in one program are an error.
- 11: variable scopes are in the side table (version 2): `side::Scopes` is made at prepare
  time from the body (v1's `describe_scopes`), each scope's code from the line rows at emit,
  and the shape (version 4) holds them.
- 4: noted; `places.rs` still refuses writable data, and the programs it hits are reported
  when the tree first runs (nothing can run before then).

> applied: noted; a status report to codegen, nothing for the coordinator.

## To codegen-release: `side::encode` takes the scopes

`crates/hir-ty-codegen/v2/release.rs:263`:
```rust
let side = super::side::of(&function.variables, &function.scopes, code).map_err(|e| failed(unit, e))?;
```
(`side::of` gives the code's line rows and length to `side::encode`; the old two-argument
`encode` is gone.) The header of `release.rs` cites `docs/v2-codegen.md`, now
`docs/v2/plan/lanes/codegen.md`.

> forwarded to codegen: its lane decides which child owns these.
