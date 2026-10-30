# Requests of codegen-adapter

The v2 adapter (`crates/hir-ty-codegen/v2.rs`, `v2/`) needs these from paths it does not own.
Each says what, where and why.

## To codegen-devspeed (`crates/hir-ty-codegen/mod.rs`, `fragment.rs`, `tier.rs`)

1. **`Recorder::new` reads `crate::v2::build::building()`** (`v2/build.rs`). When it is `Some(b)`:
   `debug_assertions = Some(b.debug_assertions)`; the panic strategy `build()` lowers with is
   abort iff `b.aborts`; `inlined_callee` and `always_inlined` return `None` (no body is built
   into another: a unit's code must be a function of its own MIR, and `INLINED` names are
   refused by `v2/shape.rs` and `v2/convert.rs`). `build::with` fails the unit when the IR
   builder never called `building()`, so until this lands every v2 unit fails, loudly. Why: the
   unit's `Spec` says these, not the environment.
2. **`FunctionUnwind::v2_parts() -> UnwindParts<'_>`**: `Dwarf { fde: &FrameDescriptionEntry,
   landing_pads: Option<&[u8]> }` or `Windows { len: u32, info: &[u8], landing_pads:
   Option<&[u8]> }`, the FDE at `v2::unwind::ANYWHERE` (`Address::Constant(0)`). `v2/unwind.rs`
   reads it. Why: v2 keeps unwind info by content, not at a module's symbol.
3. **Drop `#[cfg_attr(not(feature = "llvm"), expect(dead_code))]` on `Detach::Unit`**
   (`mod.rs`): `v2/backend.rs` builds with it in every configuration, so the expectation is
   unfulfilled without `llvm`.
4. **`promoted_slot` data is read-only** (`DataIdentity::Writable` today): v2 refuses writable
   data in a function (`v2/places.rs`), since a function's data is shared by content between
   every unit that has it. A promoted constant is never written.

## To the items lane (`crates/blaze-items`)

5. **Implement `hir_ty_codegen::v2::Hashes`** (all seven functions; `v2.rs` documents each).
   `ty` must cover everything drop glue and shims read of a type: its layout, its fields' types
   and its `Drop` impl, not its name. The tests in `v2/tests.rs` use a stand-in by database id.

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

## Done for siblings

- codegen-release: `Function::{hint, release}`, `backend::emitted`, the LLVM arms of
  `summarize` and `emit`.
- codegen-fold: `Function::foldable` (no symbol of its own, not C-variadic), `masked` with the
  refs, `merge` through the same Cranelift emit, `fold::BODY` at the last ref, thunks without
  lines or named locals, and only the data the compiled IR names (`convert::OwnData::requests`).
