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

> applied: by db: `Cx::dir(&mut self, path) -> Result<Option<&'db [(String, bool)]>, Cancelled>` (`crates/blaze-query/src/lib.rs:379`): a borrowed slice, not an owned `Vec`; `ViewInputs::dir` must take that shape.

## 2. (withdrawn: `blaze-build-script` is already a workspace dependency)

> applied: withdrawn by usercode; nothing to do.

## 3. core: ask the confine of every sandboxed run (crates/blaze-core/src/process.rs)

What: in `Finish::exited`, ask `confine.traced(&command, scratch)` for every `Sandbox::On` run,
`trace` or not; when `trace` is false, use only its error (drop the `Traced`).
Why: blaze's launcher exits 125 when it could not make the sandbox, and a program can exit
125 too. The launcher's report says which; `Launcher::traced` (crates/blaze-usercode/src/confine.rs)
returns `Ok(Traced::default())` for a run that does not trace and ran, and an error for one
whose sandbox was not made, so the spawn fails instead of passing the launcher's 125 off as
the program's (no-fallback).

> forwarded to core: `Finish::exited` (`crates/blaze-core/src/process.rs:226`) still asks `traced` only for `trace: true`.

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

> forwarded to cargo: the daemon and `crates/blaze-cargo/` are lane cargo's (crates.md), not the coordinator's.

## 5. coordinator: `blaze-cargo` depends on `blaze-usercode`

What: `blaze-usercode.workspace = true` in `crates/blaze-cargo/Cargo.toml`.
Why: the launcher (`crates/blaze-cargo/src/sandbox*`) reads `blaze_usercode::confine::Spec` and
writes `blaze_usercode::trace` lines and reports. (usercode-sandbox may ask the same.)

> applied: `crates/blaze-cargo/Cargo.toml` has `blaze-usercode.workspace = true` (line 131).

## 6. cargo: blaze's std says when it walks the environment (library/std/src/sys/env/unix.rs)

What: in `env()` (behind `env::vars`/`vars_os`) and wherever std reads `environ` whole
(`process::Command`'s spawn with the environment not cleared), call a weak, undefined,
dynamically resolved C symbol before the walk, when it resolves:

```rust
unsafe extern "C" {
    #[linkage = "extern_weak"]
    static __blaze_environ_walked: Option<unsafe extern "C" fn()>;
}
```

and export a marker the executable is built with, `#[unsafe(no_mangle)] pub static
__blaze_std_says_walks: u8 = 1;`. Neither changes what a program does: unresolved, the call is
skipped. Why: a traced run is keyed by every variable it could have seen. A process can walk
`environ` with no call the sandbox sees, so the launcher now records `E *` (the whole
environment) for every traced run (`crates/blaze-cargo/src/sandbox/v2.rs`, its module doc). That
is sound, but it puts every passed-through variable (`SHLVL`, `OLDPWD`, `_`, a terminal's own)
in every build script's and proc macro's key, which undoes #805 and much of #1450. With the hook,
the interposers (`linux_interpose.c`, `macos_interpose.c`, usercode's) define
`__blaze_environ_walked` to append `E <scope> *`, and record `E *` at load only for a process
whose executable lacks `__blaze_std_says_walks` (a C program or a toolchain-built one), so a
blaze-built build script is keyed by the variables it read by name. usercode makes the
interposer and launcher change when this lands.

> forwarded to cargo: `library/` is lane cargo's (crates.md); lane cargo is still running.

## From concurrency: lock audit (2026-10-29)

Read-only audit of `crates/blaze-expand/`, `proc-macro-api/`, `proc-macro-srv/`,
`blaze-macro-wasm/`, `blaze-wasi/`, `blaze-build-script/`, `blaze-usercode/` and
`crates/blaze-cargo/src/sandbox*` against docs/v2/plan/design/concurrency.md, by
concurrency-audit-build. The hot path here is one proc-macro call, which runs inside a salsa
query (`expand_proc_macro`) on a worker, many at once.

1. **`crates/blaze-expand/src/lib.rs:114`: `Expansions::store: Arc<RwLock<Store>>`, and the request for it in v2/requests/usercode-macros.md:21.**
   Every proc-macro call takes the read side at `:404`, and every kept expansion takes the
   write side at `:459`, from inside the query on a worker. The read side is a shared-counter
   write per call, the write side stops every other worker's lookup while `index::keep` runs, and
   it is a writer lock on a hot write path. **Instead:** `store: blaze_cas::Reader` (lane
   concurrency is writing it: reads are acquire loads of the published index), and the keep goes
   through the store's owner. The expansion returns its put (key, kind, bytes, traced inputs) to
   the plane task's own buffer, published with the task's result as `Compiled::puts` is in
   `blaze-cargo/src/serve/work.rs`, or into the worker's own pack segment that a commit flips.
   The daemon then passes no `Arc<RwLock<blaze_cas::Store>>` to `blaze_expand::install`; please
   withdraw that part of usercode-macros.md's request.
2. **`crates/blaze-expand/src/lib.rs:126` (taken at `:405`): `Installed::disk: Mutex<Disk>`, process-wide.**
   It is held, together with the store's read guard, across `index::find` (`:407`), which stats
   and BLAKE3-hashes every file the run key was traced to read. So all proc-macro lookups in the
   process take turns on one mutex, holding it across blocking file I/O: a lock on a read path,
   held while computing. `set_env(seen)` on the shared value is sound only because it stays under
   the same guard. **Instead:** make `blaze_usercode::inputs::Disk`'s cache `&self`: an
   append-only table from path hash to `(Stamp, [u8; 32])`. A lookup is acquire loads, a new or
   changed stamp publishes a new entry with one release store, and the file is hashed outside
   any lock (two workers hashing one file at once both get the same value; the second publish
   is dropped). Take the environment as an argument of `index::find`'s `Inputs` value (a
   per-call `Now { disk: &Disk, env: &seen }`) instead of `set_env` on shared state. That lets
   `Inputs::file` take `&self` or work on a per-call wrapper. The same shared `Disk` in
   `crates/blaze-cargo/src/serve/unit.rs:68` is in v2/requests/cargo.md.
3. **`crates/blaze-macro-wasm/src/lib.rs:1462` (taken at `:1526`): `static DISK: Mutex<Option<Disk>>`, held across a query call.**
   `expand` holds `DISK` and the store's read guard (`:1525`) across `index::find(&store, &run,
   &mut now)`, and `Now::env` (`:1439`) calls `load_cargo::answer(db, ...)`, a database query,
   for a `\0` span variable. That is a guard held across a query call. If the query needs
   another wasm expansion on the same thread (resolving a span can expand the macros of the
   file it is in), it locks `DISK` again: std's `Mutex` is not reentrant, so the thread
   deadlocks. On another thread it is a worker waiting on another's work. It is also the same
   global lock on the read path as finding 2. **Instead:** the shared `&self` table of finding
   2, with no guard alive while `Now` answers. Answer the `db` questions after `find` has
   returned the candidate key's recorded inputs, or by a `Now` that holds no lock, since the
   table needs none. The store read becomes `blaze_cas::Reader` as in finding 1.
4. **`crates/blaze-macro-wasm/src/lib.rs:1593`: `keeping.expansions.store.write()` on a worker.**
   This is finding 1's write side, from wasm macros: a global writer lock per kept expansion.
   **Instead:** the same put through the store's owner.
5. **`crates/blaze-macro-wasm/src/lib.rs:1670` (taken at `:1903`): the per-crate session `Slot: Arc<Mutex<Option<Result<Session, String>>>>`, held across a query call.**
   The guard lives across `session.expand(self.index, &self.listed, Some(db), &call)` (`:1911`),
   which answers the module's span questions from `db`. A query that re-enters a stateful macro
   of the same calling crate (directly, or by expanding the file the span is in) locks the same
   slot on the same thread and deadlocks. Other workers of that crate block on a thread rather
   than parking their task. **Instead:** the session has to serialize, since its state is
   ordered, but it should do so as a plane-owned serial queue per `(Nonce, Crate)`. An expansion
   posts to it and its task parks, so no thread waits on another's work (concurrency.md,
   "Waiting"), and the session runs one call at a time with no guard alive while it asks `db`.
   A re-entrant call from inside the session's own query then runs inline instead of
   deadlocking.
6. **`crates/blaze-macro-wasm/src/lib.rs:1666` (taken at `:1898`): `Lazy::sessions: Mutex<FxHashMap<...>>`.**
   Every stateful expansion takes it to look up its slot, and does a `retain` over the whole map
   under the lock: a global lock on a read path, with O(sessions) work. **Instead:** an
   append-only table keyed by the hash of `(Nonce, Crate)`. A slot is published once per
   revision with one release store, and stale revisions are dropped at a task boundary
   (quiescent-state reclamation), not on every lookup.
7. **`crates/proc-macro-api/src/warm.rs:322` (read at `:358`): `Environment::passed: RwLock<Vec<(String, String)>>`.**
   It is read and the whole `Vec` cloned on every proc-macro call (`blaze-expand/src/lib.rs:402`,
   `proc-macro-api/src/process.rs:404`), so the read side's shared counter is written and the
   global allocator used per call, including a store hit. Writes (`pass`, once per request) are
   rare. **Instead:** publish an immutable `Arc<[(String, String)]>` (or an interned `u32` id
   of it) through one atomic pointer swapped by `pass`, with the old value freed at the plane's
   task boundary. `passed()` then hands out a borrowed slice or the id, with no clone.
8. **`crates/proc-macro-api/src/warm.rs:156` (taken at `:185`, `:220`; set at `:263`): `ended: Arc<Mutex<Option<String>>>`.**
   It is locked, and the string cloned, before every `send` and `recv` of every expansion's
   round trip to a warm server: a mutex on a read path. It only ever goes from `None` to
   `Some`. **Instead:** `Arc<OnceLock<String>>`. `ended()` is `get()`, one acquire load, and
   `:263` is `set` (the first reason wins).
9. **`crates/proc-macro-api/src/process.rs:36` (taken at `:472`, `:515`): `ProcMacroServerProcess::state: Mutex<ProcessSrvState>`, held across a query call.**
   `run_expansion` holds it across `bidirectional_protocol::run_conversation(.., callback)`, and
   the callback answers the server's sub-requests from the database
   (`load-cargo/src/lib.rs:955`–`:990`: `file_source_root`, `resolve_sub_span`, `file_text`,
   `line_column`). That is a guard held across a query call: a query that expands a proc macro
   served by the same server, on the same thread, deadlocks on this mutex, and on other threads
   it is workers blocked behind another's I/O. The worker also blocks on the server's pipe or
   the bridge's `mpsc` (`warm.rs:167`) under the guard, which "no blocking call on a worker"
   rules out. **Instead:** make a conversation a parked plane task on the driver's effects (the
   `Bridge` already is sans-IO). A sub-request completes the server's effect, the task answers
   it from its own query context with no guard alive, and the server's sequential protocol is
   kept by the driver's per-connection queue, not by a mutex around the whole conversation. A
   warm pool with one connection per in-flight expansion also takes away the need to serialize.
10. **Bookkeeping on the read path (not a lock, same rule):** `crates/blaze-expand/src/lib.rs:657`
    (`Count::add`, the `COUNTS` static at `:690`) and `crates/blaze-macro-wasm/src/lib.rs:1752`
    (`RAN`, `FOUND`, `KEPT`, ...) each do two `fetch_add`s on one process-wide cache line per
    proc-macro call, including a store hit. concurrency.md says statistics on a read stay in
    the worker's own buffer and are merged at task end. **Instead:** per-worker counters padded
    to 128 bytes, summed by `report()`.

Off the hot path, so nothing to change (hits read, one line per group):

- **`blaze-expand/src/compare.rs:47`, `:173`:** the opt-in comparison mode against rustc's
  server only.
- **`blaze-expand/src/native/mod.rs:487`, `:796`, `:899`:** reached from `native::find`, once
  per macro when a library is loaded and wrapped, not per call.
- **`blaze-expand/src/lib.rs:189`, `DYLIBS`:** once per expander wrapped at load. The
  library's hash is a `OnceLock` after that.
- **`blaze-macro-wasm/src/lib.rs:711`, `COMPILED`:** once per module. `Lazy::compiled` is a
  `OnceLock` after that, and nothing compiles under the guard.
- **`blaze-macro-wasm/src/lib.rs:1644`, `MODULES`:** at load, `report` and `flush`.
- **`proc-macro-srv/src/lib.rs:66`, `:308`:** inside the server process, whose protocol is one
  request at a time. `ENV_LOCK` is needed because the environment is process-global.
- **`blaze-wasi/std/process.rs`:** the guest std's per-child state inside one wasm instance.
- **`blaze-cargo/src/sandbox/windows.rs:828`, `:831`:** a counter bump per spawn.
  Separately from the lock rules, `JOB` holds one job handle that every concurrent run
  overwrites (`Registered::new`) and any run's drop zeroes, so with runs in parallel the
  console's interrupt reaches at most the last one. Please check.
- **`proc-macro-api/src/warm.rs:482`–`:484`, `process.rs:143`–`:145`, `blaze-expand/src/native/tests.rs`:**
  tests only.

> forwarded to usercode: findings 1-10 are in lane usercode's crates (`blaze-expand`, `blaze-macro-wasm`, `proc-macro-api`, `blaze-usercode`, the sandbox); finding 9's database callback is in `crates/load-cargo/` (cargo's). Lane usercode is still running.

## From speed-link-edge-usercode (2026-10-29): `trace::assemble` over any lines

Where: `crates/blaze-usercode/src/trace.rs`, `pub fn assemble`. What: take
`lines: impl IntoIterator<Item = &'l Line>` (with `<'l, I: Inputs>`) in place of `&[Line]`; the body
already only iterates them (`lines.iter().filter(..)` becomes `lines.into_iter().filter(..)`).
Every caller compiles unchanged (`&Vec<Line>` and `&[Line]` are such iterators): `proc-macro-api/src/warm.rs:447`,
the tests in `trace.rs`, and `test::queue::Trace::traced`.

Why: the test queue gives every test of a fork server the server's own lines (scope empty: the
libraries and files it read as it started) plus the test's. Today that is a `Vec<Line>` cloned
per test, two `String`s per shared line per test. With `assemble` over an iterator, `Trace::Lines`
becomes `{ common: Arc<[Line]>, own: Vec<Line> }` and `traced` chains the two, so a test costs
nothing for the shared lines but their hashing (and `Disk` caches that by stat). I have already
cut the clone to each shared line once (they were repeated per child); this takes it to zero.
I will make the `Trace` change in `test/queue.rs` once this is in; nothing of mine needs it to compile.

> forwarded to usercode: `crates/blaze-usercode/src/trace.rs` is lane usercode's.
