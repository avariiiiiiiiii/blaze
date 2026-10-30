# Requests to speed-salsa

## From speed (coordinator's finding, 10-29 22:40): unsafe without a SAFETY comment

Every unsafe block and `unsafe impl` gets a `// SAFETY:` comment directly above it that states
the invariant and names where it is upheld (`docs/v2/plan/design/unsafe.md`). The sites left in
your paths (an impl of a trait's `unsafe fn` carries the trait's `# Safety` and needs none; a
block inside it still does):

- `vendor/salsa/src/tracked_struct/tracked_field.rs:71` `unsafe { (&(*data).revisions)[self.field_index].load() }`:
  why `data` from `data_raw(zalsa.table(), input)` is live and initialized for `input` here.
- `vendor/salsa-macros/src/db.rs:95` `unsafe impl #zalsa::HasStorage for #db`: `storage` and
  `storage_mut` return the one `#storage` field that `self` owns (storage.rs's `# Safety`).
- `vendor/salsa-macros/src/db.rs:148` the `transmute` of `downcast as fn(_) -> _` into the
  downcaster: why the two fn-pointer types have the same ABI and argument layout.
- `vendor/salsa-macro-rules/src/setup_input_struct.rs:187`,
  `setup_interned_struct.rs:301`, `setup_tracked_struct.rs:341`: `unsafe impl SalsaValue`:
  the contract of `salsa_value.rs:34`, and why the generated struct (an `Id` and a
  `PhantomData`) meets it.
- `setup_interned_struct.rs:232`, `:234`, `setup_tracked_struct.rs:329`, `:331`: `unsafe impl
  Send`/`Sync`: the struct holds only an `Id` and a `PhantomData` of the database lifetime, no
  data reached through it without the database, which is `Sync`.

Comments inside `quote!`/`macro_rules!` output are dropped from the expansion, so put the
comment in the macro source above the generated item (it is what the reader of the macro sees).
Mark this section `Status: done by speed-salsa` when written.
