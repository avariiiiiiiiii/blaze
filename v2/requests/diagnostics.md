# Requests of lane diagnostics

The lane writes `crates/blaze-diagnostics`: every error and warning rustc gives, as rustc
prints it (human, short, colour, JSON), each with the facts it read, from `hir`'s diagnostics
lowered to rustc's and the checks hir doesn't make. Its interface:

```rust
blaze_diagnostics::check_crate(db: &dyn HirDatabase, krate: hir::Crate, files: &dyn Files,
                               options: &Options) -> Vec<Diag>   // rustc's order, summary last
blaze_diagnostics::emit::{rendered, written, json, summary}(&Diag, &mut Sources, Style)
blaze_diagnostics::api::diagnostic(&Diag, &mut Sources, Style, Option<UnitOf>) -> blaze_api::Diagnostic
blaze_diagnostics::Diag::is_error()   // the build is refused when any is
```

Children add their own requests below, each saying which child it is from.

## 1. Root `Cargo.toml` and `Cargo.lock` (coordinator)

What: add the crate to the workspace and pin rustc's renderer.

```toml
# [workspace.dependencies]
blaze-diagnostics = { path = "crates/blaze-diagnostics", version = "0.0.0" }
annotate-snippets = "=0.12.16"   # was "0.12.16": the version rustc renders with, exactly
```

and `crates/blaze-diagnostics` in `members` if members are listed (they are globbed as
`crates/*` today: then only the dependency line). Why: rustc's layout of a snippet is
annotate-snippets 0.12.16's; another version lays some out differently, so it is pinned
(`crates/blaze-lints` pins it in its own manifest already). Its dependencies are all workspace
ones already: `either`, `rustc-hash`, `serde`, `serde_json`, `smallvec`, `base-db`,
`blaze-api`, `blaze-ids`, `blaze-items`, `blaze-query`, `cfg`, `hir`, `hir-def`, `hir-expand`,
`hir-ty`, `hir-ty-borrowck`, `intern`, `span`, `stdx`, `syntax`; dev: `expect-test`, `salsa`,
`test-fixture`, `triomphe`.

> applied: root `[workspace.dependencies]` gains `blaze-diagnostics = { path = "crates/blaze-diagnostics", version = "0.0.0" }`; `annotate-snippets = "=0.12.16"` with the reason. `Cargo.lock` already has annotate-snippets 0.12.16, so no lock change; members are globbed (`crates/*`).

## 2. `crates/blaze-api/src/lib.rs`: `Level::FailureNote` (ci)

What: a fifth level, after `Help`:

```rust
pub enum Level { Error, Warning, Note, Help, FailureNote }
```

Why: rustc's closing "For more information about this error, try `rustc --explain E0308`."
and "Some errors have detailed explanations: ..." are `failure-note`s (its JSON says
`"level":"failure-note"`, its text `For more information...` with no `note:` before it);
cargo shows them and an agent reads them. Giving them as `Note` would print them wrong.
`blaze_diagnostics::api` maps `Level::FailureNote` to it.

> applied: `Level::FailureNote` in `crates/blaze-api/src/lib.rs` (ci is DONE), with ci's matches changed with it: `blaze-api/src/session/best_of.rs` and `blaze-ci/src/service.rs` count it as neither error nor warning, `service.rs` names it `failure-note`, and `blaze-ui/src/api.rs` prints its message with no level before it.
> forwarded to cargo: `crates/blaze-cargo/src/cli/json.rs:224` `level` matches `Level` exhaustively and needs `Level::FailureNote => "failure-note"`.

## 3. `crates/blaze-engine` (codegen): builds refuse every error rustc gives (#1522, #1440, #780)

What: `lib.rs` `refusal`, `broken_in` (6487), `errors_of` (6593) and the allowlists `SURE`,
`BORROWCK`, `DENIED`, `DENIED_BORROWCK` (6439-6483); `doctest.rs:874-880`; `borrowck.rs:87,
206, 564-568`: replace `ide_diagnostics` by `blaze_diagnostics::check_crate` over each crate
of the build (every file, not the edited ones), refuse the build when any diagnostic
`is_error()`, and print them with `blaze_diagnostics::emit` (or hand them to cargo, 4). No
allowlist: every error is one. Then `ide-diagnostics` leaves `crates/blaze-engine/Cargo.toml`
and the workspace, and lane diagnostics deletes `crates/ide-diagnostics`.

Why: the allowlists are why E0063 and E0277 surfaced only as codegen refusals (#1522), E0015
wasn't refused (#1440), and a `-p` build passed a type error (#780): an error not on a list
was not an error, and errors of files the edit didn't touch weren't looked for. salsa keeps
the whole-crate check cheap: an unedited body's diagnostics are memoized.

> forwarded to codegen

## 4. `crates/blaze-cargo` (cargo): the CLI prints what rustc and cargo print

What: `errors.rs` (its one-span renderer) and `cargo/messages.rs`: for each unit, print
`blaze_diagnostics::emit::written` (human; `Style { short, ansi, width }` from
`--message-format=short`, `--color` and the terminal), drop the "aborting due to" diagnostic as
cargo does and keep the failure-notes, and for `--message-format=json*` emit cargo's
`compiler-message` around `blaze_diagnostics::emit::json` (`rendered` with ANSI for
`json-diagnostic-rendered-ansi`, short for `json-diagnostic-short`), then cargo's "warning:
`x` (lib) generated N warnings" and "error: could not compile `x` (lib) due to N previous
errors". Lint levels from the command line and `[lints]`: `blaze_diagnostics::lints::
CommandLine::parse` of the `-A/-W/-D/-F/--cap-lints` cargo would pass (`--cap-lints allow`
for non-local packages, as cargo does).

Why: rustc's text exactly is the lane's goal; the old renderer has one span and no children.

> forwarded to cargo

## 5. `crates/hir`, `crates/hir-ty-borrowck` (db): an unchecked body is always reported

What: `AnyDiagnostic::BorrowckSkipped` given for every body borrowck does not check, always
(not only under an environment variable), with what it could not do.

Why: no-fallback. A body borrowck skipped is a body blaze accepts that rustc might reject;
blaze-diagnostics refuses it with ``blaze cannot borrow-check `<path>`: <reason>`` until
borrowck checks it.

> forwarded to db

## diagnostics-bodies: the front end's values of `hir::Type`, `hir::TraitRef`, `hir::TraitPredicate` (db)

Where: `crates/hir/src/lib.rs`. What, exactly:

```rust
impl<'db> Type<'db> {
    /// The type as the front end has it, for rustc's printing and checks outside `hir`.
    pub fn to_next_solver(&self) -> Ty<'db> {
        self.ty.skip_binder()
    }

    /// A type of the same owner as `self`.
    pub fn derived_from_next_solver(&self, ty: Ty<'db>) -> Type<'db> {
        self.derived(ty)
    }
}

impl<'db> TraitRef<'db> {
    pub fn to_next_solver(&self) -> hir_ide::next_solver::TraitRef<'db> {
        self.trait_ref
    }

    /// A type of the same owner as the trait ref.
    pub fn derived_from_next_solver(&self, ty: Ty<'db>) -> Type<'db> {
        Type { owner: self.owner, ty: EarlyBinder::bind(ty) }
    }
}

impl<'db> TraitPredicate<'db> {
    pub fn to_next_solver(&self) -> hir_ide::next_solver::TraitPredicate<'db> {
        self.inner
    }
}
```

Why: rustc's printing of types in diagnostics (`blaze-diagnostics/src/print.rs`, from
`rustc_middle/src/ty/print/pretty.rs`) walks the type's structure: integer and float variables as
`{integer}`/`{float}`, closures by where they are written, generic arguments equal to their
defaults left out, trimmed paths, regions as rustc shows them. `HirDisplay` is rust-analyzer's
printing, not rustc's, and `hir::Type` has no public way to the structure (`TyKind`) it holds.
The E0308/E0277 lowering needs the same of trait refs and predicates.

> forwarded to db: `crates/hir/` is lane db's.

## diagnostics-bodies: `rustc_type_ir` for blaze-diagnostics (the parent, `diagnostics`)

Where: `crates/blaze-diagnostics/Cargo.toml`: `rustc_type_ir.workspace = true` under
`[dependencies]`. Why: `print.rs` matches on `rustc_type_ir::TyKind` and uses its `inherent`
traits on `hir_ty::next_solver` types.

> applied: `rustc_type_ir` is already a root workspace dependency; `crates/blaze-diagnostics/Cargo.toml` is lane diagnostics' own to add it to.

## diagnostics-bodies: names used through a glob import (db, hir-def)

Where: `crates/hir-def/src/item_scope.rs` (or a query beside it). What: for each glob import of
a crate, the names that some path of the crate resolved through it (rustc's
`Resolutions::glob_map`). Why: rustc's `trimmed_def_paths` counts a name used through a glob as
not unique (so `use a::*;` with `a::S` used makes `S` print as `a::S`), and only the names used.
Until hir has it, `print.rs` counts every name a local glob import brings into a scope (more than
rustc: such a name prints by its path where rustc may trim it).

> forwarded to db
