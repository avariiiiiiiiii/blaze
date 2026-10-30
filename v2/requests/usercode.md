# Requests of lane `usercode`

## 1. db: `Cx::dir` (crates/blaze-query/src/lib.rs)

What: on `Cx`, beside `Cx::file`:

```rust
/// A directory of the view: its entries, each a name and whether it is a directory, sorted
/// by name; `None` for no directory there. Records the edge (`Input::Dir`).
pub fn dir(&mut self, path: &str) -> Result<Option<Vec<(String, bool)>>, Cancelled>
```

`""` is the view's root. Why: a build script's or proc macro's traced inputs include directory
listings (`Traced::dirs`), and the check that reuses a result reads them in the view and must
depend on them (`crates/blaze-usercode/src/inputs.rs`, `ViewInputs::dir` and `names`, already
written against this signature). `Vfs::list` has the shape; `Input::Dir` the edge.

## 2. (withdrawn: `blaze-build-script` is already a workspace dependency)

## 3. core: ask the confine of every sandboxed run (crates/blaze-core/src/process.rs)

What: in `Finish::exited`, ask `confine.traced(&command, scratch)` for every `Sandbox::On` run,
`trace` or not; when `trace` is false, use only its error (drop the `Traced`).
Why: blaze's launcher exits 125 when it could not make the sandbox, and a program can exit
125 too. The launcher's report says which; `Launcher::traced` (crates/blaze-usercode/src/confine.rs)
returns `Ok(Traced::default())` for a run that does not trace and ran, and an error for one
whose sandbox was not made, so the spawn fails instead of passing the launcher's 125 off as
the program's (no-fallback).

## 4. coordinator: wiring user code into the daemon (crates/blaze-cargo/, the daemon)

What, at integration:
- Give every driver `blaze_usercode::confine::Launcher { program: <the blaze executable> }`
  (`Threads::confined`, and the io_uring driver's equivalent), so every `Sandbox::On` spawn
  runs in blaze's launcher (`blaze sandbox --v2 SPEC`, implemented in
  `crates/blaze-cargo/src/sandbox*` by usercode-sandbox).
- Build scripts: `blaze_usercode::script::prepare(unit, inherited, &Place { code: <content ID of
  the script binary the daemon compiled>, .. })`, then `run::Run::begin` / `complete` with
  `inputs::ViewInputs` (in a query of a kind in `blaze_query::kinds::USER`) or `inputs::Disk`
  set to `Prepared::env`, `script::cargo_rule` and `|traced| script::keyed(&prepared, traced)`.
  Before compiling a script, `script::inert(root, sources, flags)` (the flags blaze compiles
  the script with): `Some(value)` is its result and nothing is compiled; with
  `BLAZE_ELIDE=verify` run it as well and check with `script::verify_elided`. The value goes
  to `blaze_build_script::run::elided` so cargo's output files are written as for a run.
- Replace the v1 paths this supersedes: `cargo/script_sandbox.rs`, `cargo/sandboxed.rs`'s
  per-binary test run, and the build-script fingerprint by package in `cargo/script.rs`.
Why: these files are the coordinator's; the pieces above are written and tested in usercode's
paths.

## 5. coordinator: `blaze-cargo` depends on `blaze-usercode`

What: `blaze-usercode.workspace = true` in `crates/blaze-cargo/Cargo.toml`.
Why: the launcher (`crates/blaze-cargo/src/sandbox*`) reads `blaze_usercode::confine::Spec` and
writes `blaze_usercode::trace` lines and reports. (usercode-sandbox may ask the same.)
