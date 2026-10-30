# Requests from codegen-release

What release LLVM per function (`crates/hir-ty-codegen/v2/release.rs`, `llvm/release.rs`,
`crates/blaze-codegen/src/imports.rs`) needs from paths other lanes own. Report:
`/tmp/blaze-lanes/v2-codegen-release.md`. Requests on the parent's types are also in
`~/restart-2026-10-28/briefs/v2/codegen-release.requests.md`.

## codegen-adapter (`v2.rs`, `v2/backend.rs`)

1. **`Function::hint`.** Add `pub(crate) hint: fragment::Hint` to `v2::Function`, filled in
   `prepared` from the database:
   - `AttrFlags::INLINE_NEVER` gives `Never`, `INLINE_ALWAYS` gives `Always`, `IS_INLINE`
     gives `Inline`, anything else `None`;
   - closures and drop glue get `None`.

   Until it lands, `release::inline_hint` returns `Hint::None`, so release ignores
   `#[inline(always)]` and `#[inline(never)]`. That changes speed, never behavior. When the
   field lands, `inline_hint` reads `function.hint`.

> forwarded to codegen: inside lane codegen (codegen-adapter's `v2.rs`).

## codegen-devspeed (`fragment.rs`)

1. **A keep-mask from `Wants::referenced`.** Return which requests were kept, or the old-to-new
   index maps, beside the pruned `Requests`. Then release `emit` can drop the data of imported
   bodies that LLVM left unused, along with their `Places`. Today every such blob is converted
   and stored.

> forwarded to codegen: inside lane codegen (`fragment.rs`).

## codegen (parent: the driver, `blaze-codegen`)

1. **The driver calls `imports::close`** before `decide` (the parent confirms it does).
2. **Symbols LLVM calls on its own accord.** `llvm/emitted.rs::read` asks for functions no
   request named:
   - `memcpy`, `memmove` and `memset` from loop-idiom recognition, and `bcmp`;
   - libm for float intrinsics;
   - compiler-rt for 128-bit operations (`__udivti3`, ...);
   - stack probes.

   `release::emit` places each at `Target::Extern(name)` in the unit's refs (`placed_since`).
   Where the refs lack it, the unit fails with an error naming the symbol. I recommend that
   `Emitted` carry `extra: Vec<Target>` (only `Extern`), which the driver appends to the refs
   before `Template::of`. The memo key stays as it is, since the symbols follow from the code
   and the settings.
3. **Speed: run `emit` on the thread that ran `summarize`.** Then the pre-linked LLVM module
   can stay a `Module` in that thread's context, which saves a bitcode write and parse per
   function. Today it is kept as bitcode in a map by instance key.

> forwarded to codegen: every item is inside lane codegen (codegen-adapter, codegen-devspeed, the parent).
