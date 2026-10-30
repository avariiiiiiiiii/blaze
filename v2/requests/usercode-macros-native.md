# Requests from usercode-macros-native

Native expanders for serde_derive, thiserror-impl 1 and 2 and tokio-macros (#1297), in
`crates/blaze-expand/src/native/` (child of `usercode-macros`). Design:
`docs/v2/plan/lanes/usercode/macros-native.md`. Written, not built. The code reads the vendored
copies below with `include_bytes!`, so it does not compile until request 1 is applied.

## 1. To the coordinator: vendor nine crates under `vendor/native-macros/`

What: one directory per package, named `<name>-<version>`, holding exactly the files of its
`.crate` as published on crates.io (the same as `~/.cargo/registry/src/index.crates.io-*/<name>-<version>/`
less `.cargo-ok` and `.cargo-checksum.json`), with the changes below and nothing else:

| directory | package |
| --- | --- |
| `unicode-ident-1.0.27` | unicode-ident 1.0.27 |
| `proc-macro2-1.0.107` | proc-macro2 1.0.107 |
| `quote-1.0.47` | quote 1.0.47 |
| `syn-2.0.119` | syn 2.0.119 |
| `syn-3.0.9` | syn 3.0.9 |
| `serde_derive-1.0.229` | serde_derive 1.0.229 |
| `thiserror-impl-1.0.69` | thiserror-impl 1.0.69 |
| `thiserror-impl-2.0.21` | thiserror-impl 2.0.21 |
| `tokio-macros-2.7.2` | tokio-macros 2.7.2 |

Changes, in every directory:

- Move the published `Cargo.toml` to `.published/Cargo.toml`, byte for byte, and write blaze's
  `Cargo.toml` in its place (below).
- In the four macro crates (serde_derive, both thiserror-impl, tokio-macros) also copy the
  published `src/lib.rs` to `.published/src/lib.rs`, byte for byte, and in `src/lib.rs` delete
  every line whose text after leading whitespace starts with `#[proc_macro_derive(` or is exactly
  `#[proc_macro_attribute]` or `#[proc_macro]` (2, 1, 1 and 8 lines). Nothing else changes.
- `git add -f` the whole directory: unicode-ident's, proc-macro2's and quote's own `.gitignore`
  ignore `/Cargo.lock`, which they ship and blaze includes; unicode-ident's `tests/fst/.gitignore`
  ignores `/prop_list.rs` (not shipped, nothing to add).

`crates/blaze-expand/src/native/tests.rs` (`every_package_is_its_vendored_copy`,
`a_macro_crate_is_built_from_its_published_source_less_its_attributes`) fails on any other
difference.

Why: the expanders are the macros' own published code compiled into blaze as libraries
against blaze's `proc_macro` (`crates/blaze-expand/src/native/proc_macro/`), and the table of
verified closures is the hash of every published file, so the vendored copy is both the code
and the table.

> forwarded to coordinator: `vendor/native-macros/` is the coordinator's (crates.md, "Every other path"), not among the paths `requests` applies, and request 1 needs `git add -f`, which agents may not run. All nine published sources are in `~/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/`.

### Blaze's `Cargo.toml` of each

Each is the published `Cargo.toml` with these edits, so edition, rust-version, `[lints]`,
`[features]` and `build` stay as published:

- `[package]`: `name = "blaze-native-<published name>"` (e.g. `blaze-native-syn` would collide
  between the two syns: use `blaze-native-syn-2` and `blaze-native-syn-3`, and
  `blaze-native-thiserror-impl-1` / `-2`), `publish = false`. Keep `version` as published:
  serde_derive and thiserror-impl 2 read `env!("CARGO_PKG_VERSION_PATCH")` into their output.
  Drop `description`, `documentation`, `readme`, `homepage`, `repository`, `keywords`,
  `categories`, `license`, `include`, `exclude` (they don't change the build; keep them if
  simpler).
- `[lib]`: keep `name` and `path`; delete `proc-macro = true`; add `test = false` and
  `doctest = false`.
- Delete every `[dev-dependencies*]`, `[[test]]`, `[[bench]]`, `[[example]]`, `[[bin]]`,
  `[package.metadata*]` and `[badges]` table (the packages join the workspace as path
  dependencies, and their tests' dependencies are not here).
- Each `[dependencies.X]` on another package of the table: keep its `version`, `features`,
  `default-features` and `optional` as published, add `package = "blaze-native-…"` and
  `path = "../<directory>"` (syn 2 for thiserror-impl 1, syn 3 for the rest).
- Every package but unicode-ident links `proc_macro` (`extern crate proc_macro` or
  `proc_macro::` in a proc-macro crate), so add:

  ```toml
  [dependencies.proc_macro]
  package = "blaze-native-proc-macro"
  path = "../../../crates/blaze-expand/src/native/proc_macro"
  ```

  If cargo refuses the dependency name `proc_macro`, say so (to me through the parent): the
  sources can't be changed to name it otherwise without breaking the byte-for-byte rule.

Features end up as `verified.rs` has them only through blaze-expand's dependencies (request 3)
and these specs: syn 3 gets clone-impls, default, derive, full, parsing, printing, proc-macro;
syn 2 the default set; proc-macro2 and quote default and proc-macro; serde_derive default.
`native/tests.rs`'s `the_table_is_well_formed` and `the_verified_copy_matches_itself` check them.

> forwarded to coordinator: with request 1.

## 2. To the coordinator: workspace dependencies (root `Cargo.toml`)

```toml
native-proc-macro = { package = "blaze-native-proc-macro", path = "crates/blaze-expand/src/native/proc_macro" }
native-proc-macro2 = { package = "blaze-native-proc-macro2", path = "vendor/native-macros/proc-macro2-1.0.107" }
native-serde-derive = { package = "blaze-native-serde_derive", path = "vendor/native-macros/serde_derive-1.0.229" }
native-thiserror-impl-1 = { package = "blaze-native-thiserror-impl-1", path = "vendor/native-macros/thiserror-impl-1.0.69" }
native-thiserror-impl-2 = { package = "blaze-native-thiserror-impl-2", path = "vendor/native-macros/thiserror-impl-2.0.21" }
native-tokio-macros = { package = "blaze-native-tokio-macros", path = "vendor/native-macros/tokio-macros-2.7.2" }
```

`blaze-native-proc-macro` (`crates/blaze-expand/src/native/proc_macro/Cargo.toml`, its lib named
`proc_macro`) and the vendored packages become workspace members as path dependencies inside
the workspace; nothing in `members` or `exclude` needs to change. `blake3`, `intern`,
`ra-ap-rustc_lexer` and `serde_json` are already workspace dependencies.

> applied: `native-proc-macro` (its crate exists) in the root `[workspace.dependencies]`. The five entries on `vendor/native-macros/*` wait for request 1: a workspace dependency on a directory that does not exist stops the workspace from loading once blaze-expand names it, so they land with the vendoring (`requests` adds them the pass after the directories appear).

## 3. To the parent (usercode-macros): `crates/blaze-expand/Cargo.toml`

Add to `[dependencies]`:

```toml
blake3.workspace = true
intern.workspace = true
ra-ap-rustc_lexer.workspace = true
serde_json.workspace = true
native-proc-macro.workspace = true
native-proc-macro2.workspace = true
native-serde-derive.workspace = true
native-thiserror-impl-1.workspace = true
native-thiserror-impl-2.workspace = true
native-tokio-macros.workspace = true
```

and in `crates/blaze-expand/src/lib.rs`'s `report()`, append `native::report()` (the count of
libraries that are verified copies, and each first difference that made a verified crate's
library miss), e.g.
`if let Some(native) = native::report() { said += "; "; said += &native; }`.

> forwarded to usercode: `crates/blaze-expand/` is lane usercode's; its five `native-*` lines wait for request 1.

## 4. To the coordinator: register the description hook

Where: `crates/blaze-engine/src/lib.rs`, `held_wide()`, beside `blaze_expand::install(…)`
(blaze-cargo does not depend on blaze-expand; blaze-engine depends on both), and a function in
`crates/blaze-cargo/src/cargo/own_macro.rs` that knows where it writes a description.

Why: `native::find` must know each proc-macro library's crate and dependency closure (each
package's name, version, the BLAKE3 of every file of its directory, its cfg and its
dependencies' names); `own_macro.rs` writes that as the `rust-project.json` beside each module
it builds (`described_at`: `<library dir>/blaze-wasm/<stem>-<digest>/rust-project.json`, the
module `<stem>.wasm` in the same directory). Without the hook `find` returns an error for every
macro, which fails the load (no-fallback): it must be registered before the first load.

What, in `own_macro.rs`:

```rust
/// Sets `into` to the `rust-project.json` blaze built the proc-macro module at `module` from,
/// beside it ([`described_at`]), and says `true`; `false` when `module` is not a module blaze
/// built from a description (a library it runs natively).
pub fn description_of(module: &Path, into: &mut PathBuf) -> bool {
    let (Some(dir), Some("wasm")) = (module.parent(), module.extension().and_then(OsStr::to_str))
    else {
        return false;
    };
    into.as_mut_os_string().clear();
    into.push(dir);
    into.push("rust-project.json");
    true
}
```

It allocates nothing: the buffer is blaze-expand's, kept per thread and reused.

and in `held_wide()`:

```rust
blaze_expand::native::describe_with(Box::new(|dylib, into| {
    Ok(cargo::own_macro::description_of(dylib.as_ref(), into))
}))
.expect("held_wide runs once per process");
```

(`cargo` as blaze-engine already names `blaze_cargo::cargo`; `describe_with` errs only when
called twice.) blaze-expand reads the description itself and keeps what it read by the file's
path and stat stamp, so after the first load a library costs one `stat` of its description and
one of each file and directory of its closure; a `.wasm` module whose description is missing is
an error naming both.

> forwarded to codegen and cargo: `held_wide()` is in `crates/blaze-engine/src/lib.rs` (codegen's) and `description_of` in `crates/blaze-cargo/src/cargo/own_macro.rs` (cargo's); both lanes are still running.

## 5. To db (forwarded by the coordinator): no hook for `bitflags!`

Nothing to add to `hir-expand`'s `macro_rules` expansion: `bitflags!` gets no native expander.
Each step of a `macro_rules!` expansion is already done in process by `mbe`, and replacing
more than one step (bitflags' `__impl_*` macros are separate expansions, each with its own
`MacroCallId` and syntax-context mark) would drop those marks from the output's syntax
contexts, so it could not be exact. The reasoning is in the notes.

> forwarded to db: a note, nothing to change in `hir-expand`.
