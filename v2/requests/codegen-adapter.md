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
2. **`FunctionUnwind::v2_parts()`** in `crates/hir-ty-codegen/unwind.rs`, which
   `v2/unwind.rs` reads (the fields of `FunctionUnwind` are private to `unwind`). Not there yet,
   so `v2/unwind.rs` does not build until it is:

   ```rust
   /// What a v2 build keeps of a function's unwind information, apart from where it is.
   pub(super) enum UnwindParts<'a> {
       Dwarf { fde: FrameDescriptionEntry, landing_pads: Option<&'a [u8]> },
       Windows { len: u32, info: &'a [u8], landing_pads: Option<&'a [u8]> },
   }

   impl FunctionUnwind {
       pub(super) fn v2_parts(&self) -> UnwindParts<'_> {
           let landing_pads = self.landing_pads.as_deref();
           match &self.frame {
               Frame::Windows(frame) => {
                   UnwindParts::Windows { len: frame.len, info: &frame.info, landing_pads }
               }
               Frame::Cranelift(info) => {
                   UnwindParts::Dwarf { fde: info.to_fde(Address::Constant(0)), landing_pads }
               }
               Frame::Rows(rows) => {
                   let mut fde = FrameDescriptionEntry::new(Address::Constant(0), rows.len);
                   for &(offset, rule) in &rows.rows {
                       fde.add_instruction(offset, rule.instruction());
                   }
                   UnwindParts::Dwarf { fde, landing_pads }
               }
           }
       }
   }
   ```

   (`fde()` already does the two DWARF arms; `v2_parts` can call it.) Why: v2 keeps an FDE's
   instructions by content and the linker places it, so the address is a constant 0.
3. **Drop `#[cfg_attr(not(feature = "llvm"), expect(dead_code))]` on `Detach::Unit`**
   (`mod.rs`): `v2/backend.rs` builds with it in every configuration, so the expectation is
   unfulfilled without `llvm`.
4. **`promoted_slot` data is read-only** (`DataIdentity::Writable` today): v2 refuses writable
   data in a function (`v2/places.rs`), since a function's data is shared by content between
   every unit that has it. A promoted constant is never written.

> forwarded to codegen: items 1 to 4 are inside lane codegen (codegen-devspeed's `crates/hir-ty-codegen/`).

## To the items lane (`crates/blaze-items`)

5. **Implement `hir_ty_codegen::v2::Hashes`** (all seven functions; `v2.rs` documents each).
   `ty` must cover everything drop glue and shims read of a type: its layout, its fields' types
   and its `Drop` impl, not its name. The tests in `v2/tests.rs` use a stand-in by database id.

> forwarded to items: the same as `v2/requests/codegen.md` 1.
> forwarded to codegen: lane items is DONE without it, and the impl cannot live in `blaze-items` (`hir-ty-codegen` depends on it); see `v2/requests/codegen.md` 1.

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
