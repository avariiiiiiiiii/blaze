---
status: plan
summary: blaze v2 reads dependency crates from images of their def map, lang items and signatures keyed by every input, parses only the bodies codegen or const eval asks for, and reparses and re-lowers only the items an edit touches (frontend.md items 5 and 14).
date: 2026-10-29
---
# blaze v2: dependencies by signature, and per-item reparse (frontend-deps)

Design notes of lane `frontend-deps` for items 5 and 14 of
[the front end's design](../../design/frontend.md), as written on 2026-10-29. Nothing was built
or run: every claim below is what the code is written to do, and each test named is what proves
it once it runs.

## What is matched

- **Item 5, dependencies.** The benchmark is rust-analyzer's and rustc's treatment of
  dependencies: rustc reads a dependency's `.rmeta` (its def map, signatures and MIR of
  generic and `#[inline]` items) and never its source; rust-analyzer parses every file of every
  dependency and lowers it in full. blaze v2 matches rustc's "a dependency costs a read of its
  metadata" and keeps rust-analyzer's laziness: the image is read in place, a signature is
  decoded only when asked, and a body is parsed only when codegen or const eval asks for it,
  one function at a time (outline parse, #1294). Measured by the time to first diagnostic of a
  workspace on a warm store against rustc's `cargo check` with a warm `target/` (#1293).
- **Item 14, edits.** The benchmark is rust-analyzer's incremental reparse
  (`syntax::parsing::reparsing`, a token or a `{}` block relexed) and Roslyn's
  red-green incremental parse: an edit costs work in the size of what it touches, not of the
  file. blaze v2 reparses the smallest covering item or block (`Reparser`) and lowers item trees
  per item, so items an edit does not touch keep their memo.

## Item 5: dependency images (`crates/hir-def/src/deps/`)

- **Which crates.** `deps::eligible`: a crate whose origin is not local and none of whose
  transitive dependencies is local. Workspace members, and anything built on one, resolve from
  source; `blaze check` of the workspace checks them fully.
- **Key** (`deps/key.rs`). A hash of `build` (the blaze reading, from the driver), the encodings'
  versions (`PRODUCER`: image schema, `frozen::ENCODING`, `signatures::SCHEMA`),
  `Crate::content` (root path, edition, cfg and features, env, crate attributes, target and
  toolchain, dependencies' content IDs), the content of the crate's source root (every file's
  path and text, `OUT_DIR` files included, since the v2 load puts them there), the process env
  (`env!` reads it when the crate's env has no value) and each dependency's own key (its texts,
  which `Crate::content` leaves out).
- **What the key does not cover is refused** (`deps::store`, `Refused`): a crate with an item
  from a procedural macro (its expansion may read files and variables the expansion server
  traces per expansion, not in hir), a def map that uses the session's macros, a module file or
  `include!` outside the crate's source root. Such a crate resolves from source every build,
  and the driver shows why under `-v`.
- **Image** (`deps/image.rs`). One `blaze_format` buffer: a `Root { key, parts }` and three
  parts (`Part::DefMap`, `LangItems`, `Signatures`), each 16-aligned so the signatures part,
  a buffer itself, is opened in place. `ImageRef::open` checks the header and schema, that the
  root's key is the key it was found by, and each part's bounds, tag order and alignment; the
  store checks the content hash on every read.
- **Serve** (`deps/serve.rs`). `deps::serve(db, crates, build, store, mode)` keys the crates,
  reads each image the store has, checks the def map part names only files the crate has
  (`frozen::unreadable`), and gives the database the def maps and lang items
  (`frozen::set`, `set_lang_items`) and the images (`DepImages`, which signatures read through
  `deps::image` and `deps::mode`). An image that does not read under its own key is an error
  naming the crate: never a crate resolved instead.
- **Store.** `deps::store(db, served, store)` writes the image of each eligible crate not
  served, after the refusals; a store that keeps other bytes under the key is an error.
- **Modes.** `Mode::Serve` reads the image; `Mode::Check` resolves as well and records each
  difference; `Mode::Verify` re-encodes what was read and records a difference from the bytes.

## Item 14: per-item reparse and lowering (lane `frontend-deps-reparse`)

The parse query keeps the previous tree and the edit and reparses with `Reparser`; item trees
are built per item and assembled byte-equal to a whole-file lowering, so an edit inside one
item reruns only that item's lowering. Its notes and tests are in its report
(`/tmp/blaze-lanes/v2-frontend-deps-reparse.md`); its hooks into `base-db` go to db.

## Tests and what each proves

In `crates/hir-def/src/deps/tests.rs`:

- `only_crates_outside_the_workspace_are_eligible`, `a_library_on_a_member_is_not_eligible`:
  workspace crates are never served.
- `keys_are_the_same_in_every_database`, `a_key_covers_every_input`: a key is deterministic and
  changes with a non-root file, a dependency's text, cfg, edition and `build`, and not with a
  dependent's or a member's text.
- `an_image_reads_back_its_parts`, `an_image_under_another_key_does_not_read`,
  `damaged_images_read_within_bounds_or_not_at_all`: the format round-trips and refuses
  misfiled or damaged buffers without panicking.
- `every_dependency_is_stored_and_served`, `a_served_dependency_resolves_as_a_fresh_one`
  (each mode), `an_unresolved_import_of_a_dependency_is_still_reported`: a database that reads
  every dependency from its image has the same def maps, diagnostics and lang items, byte for
  byte in the frozen encoding, as one that resolved them; check and verify modes record no
  difference.
- `an_edit_to_a_dependency_misses_its_image_and_its_dependents`,
  `an_image_written_under_another_key_is_an_error`,
  `a_crate_with_an_item_from_a_proc_macro_is_refused`,
  `a_crate_with_no_outside_input_is_not_refused`, `serving_asks_the_store_once_a_crate`.
- Outline signatures against full-parse signatures (fixtures and a proptest) and served
  signatures against computed ones are `frontend-deps-sigs`'s, in `deps/signatures/`.

## Left, with who

- db: declare `pub mod deps;` in `crates/hir-def/src/lib.rs` (`v2/requests/db.md`).
- cargo-daemon: the `ImageStore` adapter over `blaze-cas`, `serve` once inputs are final,
  `store` after the build, a daily `Mode::Check` run (`v2/requests/cargo.md`).
- usercode and db: a content ID of each proc-macro expansion's traced inputs, kept with the
  expansion, so crates with proc-macro items can be imaged too (today they are refused).
- store: an aligned in-place read of an entry, so `ImageStore::get` need not copy the image
  once per crate (`Aligned::copy_from` in the adapter today).
- Keys hold absolute paths (`Crate::content` and source-root content name files by path), so
  images are shared between checkouts at one path, not across machines: a portable content ID
  (paths from the package root) is db's to add before the global cache serves images.
