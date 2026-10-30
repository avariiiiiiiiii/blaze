---
status: plan
summary: speed-hir-defs-sigs: item signatures, VariantFields and EnumVariants in hir-def's signatures.rs brought to the fast-code bar (flags by shift and mask, one keyword pass per item, exact-size field and variant tables, variant bitset), with before and after numbers against rustc's collect queries.
date: 2026-10-29
---
# speed-hir-defs-sigs

Path: `crates/hir-def/src/signatures.rs`. The store it lowers into (`ExpressionStore`,
`ExprCollector`, `lower_*`) belongs to speed-hir-store. The flags it reads (`AttrFlags`) belong to
[speed-hir-defs](speed-hir-defs.md).

## Hot path

On a cold build, each item's signature is lowered once: fn, struct, union, enum, const, static,
trait, impl and type alias, plus every variant's `VariantFields` and every enum's
`EnumVariants`. hir-ty then reads the signature for every use of the item. A read is
`X::of(db, id)`, which is `returns(deref)`: one salsa memo lookup giving `&X`, with no `Arc`
clone and no lock. `of` clones the `Arc` out of `with_source_map` once, when the memo is filled.

## Benchmark: rustc's `rustc_hir_analysis::collect`

rustc's `generics_of`, `fn_sig`, `adt_def` and `trait_def` read arena HIR. Each result is
arena-allocated (`&'tcx Generics`, `&'tcx TraitDef`) or interned (`AdtDef`, `PolyFnSig`).
Callers get a `Copy` handle of 8 bytes, and the global allocator is called 0 times per
signature. rustc reads the keywords of an item (`unsafe`, `async`, `const`, ...) from HIR fields
the parser already set. It reads attribute flags from parsed attributes, for example
`find_attr!(.., Fundamental)`.

## Before and after

Sizes come from the `const` assertions in the file. The store and generic params are 72 bytes
each, as of 2026-10-29, before speed-hir-store's type columns. The assertions are written as
"store + generic params + own bytes", so they still hold when those sizes change.

| | before | after | rustc |
|---|---|---|---|
| handle a reader gets | `&X` from the memo | same | `Copy` 8-byte arena or interned pointer |
| bytes per signature (8-byte `Arc` header not counted) | struct, union, enum, trait, impl 160; fn, type alias 184; const, static 88 (unchanged, now asserted) | same | arena bytes only |
| bytes of flags | 1–2 per signature | same, asserted per flag type | bit fields of the HIR node |
| flags from `AttrFlags` | one `if contains` and `|=` per flag: 3 for a struct, 6 for a trait, 5 for a fn | a branch-free shift and mask per flag, constants checked at compile time (`single_bit`) | `find_attr!` scan per attribute kind |
| struct lang items | a chain of up to 8 `_ if lang == sym::..` guards | one table of 8 `(Symbol, StructFlags)` pairs; only runs when `LANG_ITEM` is set | `LangItem` enum compare |
| keyword scans of the syntax | fn 6 child walks (`unsafe_token()` .. `safe_token()`); static 3; impl 4; trait 4; a field 1 walk through `NodeOrToken` cursors | 1 walk per item over the flat tree's kind column (`child_tokens`), no cursor built | 0 (HIR fields) |
| `VariantFields` field arena, n fields | grown by doubling (about log2 n allocations), then `shrink_to_fit` (one more reallocation) | `Arena::with_capacity(n)` from the syntax's count: 1 allocation; shrinks only when a field is cfg'd out | 1 arena bump |
| `VariantFields` of a unit or empty variant | builds and finishes an empty `ExpressionStoreBuilder` (takes and returns a pooled scratch set) | `ExpressionStore::empty().clone()`, no list setup at all: 0 allocations | 0 |
| `EnumVariants`, n variants | `FxIndexMap` grown by doubling (entries and index table each about log2 n reallocations), `Vec<bool>` grown and then boxed, both shrunk: about 11 allocations for 10 variants | map preallocated from the syntax's count (2 allocations: entries and index); `explicit_discriminant` is a bitset inline up to 64 variants (`VariantBits`, 24 bytes): 0 allocations; shrinks only when a variant is cfg'd out | 1 arena bump (`AdtDef` variants) |
| `explicit_discriminant` bytes | 16-byte `Box<[bool]>` + n bytes on the heap | 24 bytes inline for up to 64 variants, n/8 bytes past that | — |

## Semantics fixed on the way

An `unsafe fn` with `#[rustc_deprecated_safe_2024]` (std's `env::set_var`) now keeps `UNSAFE` as
well as `DEPRECATED_SAFE_2024`. rustc's `fn_sig` of it is unsafe. Before, only the second flag
was set, so its body was checked as a safe function's body and it coerced to a safe `fn` pointer.
db-ty-lower moved the deprecated-safe test first in `is_fn_unsafe_to_call`
(`crates/hir-ty/src/utils.rs`), so a pre-2024 call outside `unsafe` still only lints, as in
rustc. This was diagnostics' request 6 (`v2/requests/diagnostics.md`).

## Where it still doesn't match rustc, and what it would take

- **One `Arc`, a store and generic params per signature.** rustc bump-allocates all of a crate's
  signatures in one arena. To match, signatures would be rows in a per-crate column table,
  lowered with the item tree, and `X::of` would return an index into it. That is the salsa memo
  layout (lane db) and the item tree's columns (format), not this file.
- **The source map is built for every signature.** A second lowering without it can't be
  shared. `collect_block_` makes a tracked `BlockId` for a block in a const argument
  (`[u8; { .. }]`), so two lowerings give unequal stores. `of` therefore stays a projection of
  `with_source_map`. It keeps the signature when the source map is evicted, and it backdates
  when only pointers move. The source map costs what speed-hir-store's sorted tables cost.
- **Keywords are read from syntax.** rustc's parser sets them on HIR. Matching that means the
  item tree storing a per-item keyword byte (format/items). `child_tokens` is already one pass
  over a `u16` column.
- **`InactiveCode { opts: CfgOptions }`** clones the crate's whole cfg set for each cfg'd-out
  field. The options are always the crate's, so the diagnostic can read them when it is
  reported. That edit is in speed-hir-store's types; it was filed with them as a finding.

## Tests (in `signatures.rs`)

- `flags_from_attrs_as_one_if_per_flag`: every signature's `from_attrs`, and the const and
  static macro uses, against the old one-`if`-per-flag code kept as the model. Inputs are all
  128 single bits of `AttrFlags`, none, all, and 4096 seeded random words.
- `struct_lang_items_as_a_match`: the table against the old guard chain, over every entry,
  other lang items, and freshly interned names.
- `child_tokens_as_the_accessors`: the one-pass scan against each `*_token()` accessor, for every
  fn, static, impl, trait and field of a fixture. The fixture covers each keyword, and the test
  asserts that each keyword is actually seen, so it can't pass vacuously.
- `variant_bits_as_bools`: the bitset against a `Vec<bool>` model across the 64-bit word
  boundaries (0, 1, 63, 64, 65, 127–129, 200, 1000), including `truncate`, and inline storage
  up to 64 variants.
- The `signatures_lower_no_body` test in `nameres/demand_tests.rs` (db-expand-split) still holds:
  no signature query reads a body. `HAS_BODY` and `explicit_discriminant` are read off the syntax.
