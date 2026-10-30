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

### 6, revised (2026-10-29, under "everything is ours"): three parts, all needed before `E *` narrows

The std hook alone is not enough. The whole-environment line also covers what the C library
reads through its own internal calls, which no interposer sees: glibc's `localtime` reads `TZ`,
`setlocale` reads `LANG`, `LC_*` and `LOCPATH`, and there are also `TMPDIR`, `MALLOC_*` and
`GLIBC_TUNABLES`, the resolver's `RES_OPTIONS`, `LOCALDOMAIN` and `HOSTALIASES`, and
`GCONV_PATH`. A marker on the executable can't vouch for C code linked into it either. So:

1. **cargo (std, `library/std/src/sys/env/unix.rs`):** the hook above.
2. **link (`crates/blaze-link/`):** the linker exports `__blaze_std_says_walks` in an executable or
   dylib only when no object in it but std's references `environ`, `__environ`, `_environ` or
   `_NSGetEnviron`. The linker is ours, so the marker is proved, not assumed.
3. **usercode (sandbox, mine):** when std and link have landed, I will:
   - record `E *` for every scope at load for each loaded object without the marker, and check
     each `dlopen`ed object the same way;
   - make `__blaze_environ_walked` an `E <scope> *` line;
   - have the interposer record, at load, the C library's internal reads as `E` lines of every
     scope. The list comes from the C library the sandbox gives the run, pinned with the
     toolchain (a blaze-owned libc build, or a list checked against the glibc version the
     toolchain pins; lane cargo's toolchain decides which).

Until all three are in, every traced run keeps `E *`, which is sound.

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

## From safety-audit

From `safety-audit-usercode` (report `/tmp/blaze-lanes/v2-safety-audit-usercode.md`). The
workspace is about to deny `unsafe_op_in_unsafe_fn`, `clippy::undocumented_unsafe_blocks` and
`clippy::multiple_unsafe_ops_per_block`. Every file below was BUSY (this lane, `usercode-concurrency`),
so nothing was edited; comments and splits only, no behavior change. A comment goes on the line
right above the `unsafe` (for an `if unsafe {..} < 0` that is the line above the `if`). Splits bind
only `Copy` values and keep the order. Already done by me (free): `crates/blaze-wasi/src/lib.rs`
`#![forbid(unsafe_code)]`, and the 2-op block in `crates/blaze-wasi/std/process.rs` split.

Texts used many times below (paste the text, with the name filled in):
- [exit]: `// SAFETY: \`_exit\` takes only the code and is async-signal-safe; it ends this forked child without running the atexit handlers or flushing the stdio buffers copied from the parent, which would otherwise run or be written twice.`
- [wait]: `// SAFETY: \`status\` is a live \`c_int\` on this frame for the kernel to write the child's status into, borrowed by nothing else during the call.`
- [ids]: split `let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };` into `let uid = unsafe { libc::getuid() };` and `let gid = unsafe { libc::getgid() };`, each with `// SAFETY: \`getuid\`/\`getgid\` take no arguments, read no memory and cannot fail.`
- [fork]: `// SAFETY: this process is one thread: the sandbox's processes are forked from the \`blaze sandbox\` command's, which starts no thread (see MAY BE UNSOUND 2 for the tests), so the child may allocate and take locks; with another thread, a lock it held would stay held in the child forever.`
- [deathsig]: `// SAFETY: \`prctl(PR_SET_PDEATHSIG, SIGKILL)\` takes integers only and reads no memory; it only has the kernel kill this child when its parent ends.`
- [cstr]: `// SAFETY: every pointer is a NUL-terminated \`CString\` (or C-string literal) of this frame, alive for the call, or null where the call takes none; the kernel only reads them, and a dangling one would have it read freed memory.`
- [close X]: `// SAFETY: \`close\` takes the descriptor by value and reads no memory; X is this process's own and is not used after, so no descriptor that later reuses its number is closed by mistake.`
- [zstat]: `// SAFETY: \`libc::stat\` is integers only, for which all-zero bytes are a valid value.`
- [fstat]: `// SAFETY: \`stat\` is a live \`libc::stat\` on this frame for \`fstat\` to fill; a descriptor that is not open is an error return, not a write.`
- [zmsg]: `// SAFETY: \`msghdr\` is integers and raw pointers, for which all-zero bytes (null pointers, zero lengths) are a valid value.`
- [int C]: `// SAFETY: C takes integers only and reads or writes no memory of this process.`

### MAY BE UNSOUND

1. `crates/blaze-cargo/src/sandbox/linux.rs:1709` (`send`) and `:1733` (`received`): the control buffer is `let mut space = [0u8; 64]`, alignment 1, but `CMSG_FIRSTHDR(&msg)` returns its start as `*mut cmsghdr` and the code writes (`(*cmsg).cmsg_level = ..`, `cmsg_type`, `cmsg_len`) and reads (`(*cmsg).cmsg_type`) fields through it, which needs `align_of::<cmsghdr>()` (8). Nothing aligns `space`. Fix: `#[repr(C, align(8))] struct Control([u8; 64]);` (or `[u64; 8]`), used for `space` in both. The SAFETY comments proposed below for those derefs assume that fix.
2. `crates/blaze-cargo/src/sandbox/linux.rs` tests (`:2473` `probe`, `:2602` `run_as`, `:2636`, `:2687`, `:2834` `run`) and `crates/blaze-cargo/src/sandbox/macos.rs:801` (`probe`) call the sandbox in the test binary's own process, which libtest runs on many threads. The forked children then allocate, format and print (`confined`, `sandbox`, `first_process`, `eprintln!`, `sandbox_init_with_parameters`), which after a fork of a multi-threaded process may use a lock another thread held (a deadlock, or allocator state torn mid-update). The [fork] invariant holds for `blaze sandbox` only. Fix: have the tests run the sandbox through the command (`current_exe` + `sandbox`, as `probed` does), or in a child process of their own.
3. `crates/blaze-usercode/src/trace.rs:400` (`os_from`, `#[cfg(not(unix))]`): `OsStr::from_encoded_bytes_unchecked(bytes)` on bytes from `trace::decode` and `index::Shape::decode`, both safe `pub fn`s that take any `&[u8]` (a report file read back in `confine.rs:102`, a shape from the store). Bytes not from `as_encoded_bytes` of this platform and this Rust version are UB on Windows, and nothing checks them. Fix: encode and decode the Windows form as UTF-16 units (`encode_wide` / `OsString::from_wide`), which is safe for any bytes; or check the bytes before the call.
4. `crates/blaze-macro-wasm/src/lib.rs:742`: `Module::deserialize_file(engine(), &file)` loads `<module>.<key>.cwasm` beside the module as native code with no check of its bytes. The comment says it is "what `serialize` wrote for this key", but only the store's copy is checked against its checksum. Any other writer of that path (another process of the user's, a stale or foreign file of the same name) gets its bytes run as machine code in blaze. Fix: check the file's hash against what the store has under `key` (or load only from the store) before `deserialize_file`.
5. (text, not crate code) `crates/blaze-macro-wasm/guest/proc_macro.rs:1343`: `__blaze::input` does `slice::from_raw_parts(ptr, len)` for the safe `pub fn`s `__blaze::expand1` and `expand2`, which any crate built against this `proc_macro` can call with any pointer. The exports pass what the host wrote, but the safe API doesn't hold that. It stays inside the wasm instance. Fix: make `expand1`/`expand2` `unsafe fn` with `/// # Safety\n/// \`ptr\`..\`ptr + len\` (and \`attr\`..\`attr + attr_len\`) is memory of this module the host wrote the input into (\`__blaze::alloc\`), live until the call returns.` and an `unsafe {}` at the generated export's call.

### crates/blaze-cargo/src/sandbox.rs

- `crates/blaze-cargo/src/sandbox.rs:757` block: 3 ops (`mem::zeroed`, `sigemptyset`, `sigaction`). Split inside the `for` body: `// SAFETY: \`sigaction\` is plain C data for which all-zero bytes are a valid value (no handler, no flags, an empty mask).` `let mut action: libc::sigaction = unsafe { std::mem::zeroed() };` / `action.sa_sigaction = ..;` / `// SAFETY: \`action.sa_mask\` is a live \`sigset_t\` of this frame.` `unsafe { libc::sigemptyset(&mut action.sa_mask) };` / `// SAFETY: \`action\` is initialized and outlives the call, and its handler \`pass_on\` only loads an atomic and calls \`kill\`, both async-signal-safe; a null old action is allowed.` `unsafe { libc::sigaction(signal, &action, std::ptr::null_mut()) };`

### crates/blaze-cargo/src/sandbox/linux.rs

- `:462` block: 2 ops, no SAFETY. [ids].
- `:478` block (`failed`): no SAFETY. [exit].
- `:480` block: no SAFETY. `// SAFETY: \`unshare\` takes flags by value and reads no memory; this forked child is one thread, as CLONE_NEWUSER needs (else EINVAL, not UB).`
- `:484` block: no SAFETY. [cstr] (`file` is a C-string literal).
- `:485` block: no SAFETY. `// SAFETY: \`text\` is a live String whose buffer holds \`text.len()\` bytes for the call; \`fd\` is open (the \`||\` stops at \`fd < 0\`).`
- `:488` block: no SAFETY. [close `fd`].
- `:490` block: no SAFETY. [exit].
- `:493` block: no SAFETY. [wait].
- `:563` block: 2 ops, no SAFETY. [ids].
- `:568` block (fork): the comment says the child "only makes system calls and exits", but it makes the whole sandbox (allocates, writes files, forks again). Replace with [fork].
- `:576` block: 2 ops (`setsid`, `prctl`), no SAFETY. Split: `// SAFETY: \`setsid\` takes no arguments and reads no memory; in this forked child it only makes it a session leader.` `unsafe { libc::setsid() };` then [deathsig] `unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) };`
- `:580` block: no SAFETY. [close `sockets[0]`, the supervisor's end, which the child never uses].
- `:600` block: no SAFETY. [exit].
- `:603` block: no SAFETY. [close `sockets[1]`, the child's end].
- `:605` block: no SAFETY. [close `sockets[0]`; `received` returned the descriptor it was sent, a new one].
- `:614` block: no SAFETY. [close `listener`, once `supervise` returned].
- `:617` block: no SAFETY. [wait].
- `:727` block: no SAFETY. `// SAFETY: \`unshare\` takes flags by value and reads no memory; this is the sandbox's first process, one thread, as CLONE_NEWUSER needs.`
- `:745` block: no SAFETY. [cstr] (source, type and data null for a propagation change, mount(2)).
- `:758` block: no SAFETY. [int `unshare(CLONE_NEWPID)`].
- `:762` block: no SAFETY. [fork].
- `:768` block: no SAFETY. [deathsig].
- `:776` block: no SAFETY. [exit].
- `:778` block: no SAFETY. [close `made.socket`; the PID namespace's first process has its own copy].
- `:780` block: no SAFETY. [wait].
- `:794` block: no SAFETY. [int `prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0)`].
- `:797` block: no SAFETY. [int `prctl(PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0)`].
- `:807` block: no SAFETY. [cstr].
- `:813` block: no SAFETY. [fork].
- `:818` block: no SAFETY. [deathsig].
- `:821` block: no SAFETY. [exit].
- `:823` block: no SAFETY. [close `made.socket`; the program's process has its own copy].
- `:827` block: no SAFETY. [wait].
- `:838` block: no SAFETY. `// SAFETY: \`getpid\` takes no arguments, reads no memory and cannot fail.`
- `:843` block: no SAFETY. `// SAFETY: \`kill\` takes integers only and reads no memory; \`pid\` is listed as a child of this subreaper, which only this loop reaps, so its number is no other process's until the \`waitpid\` below.`
- `:845` block: no SAFETY. [wait].
- `:903` block: no SAFETY. `// SAFETY: with LANDLOCK_CREATE_RULESET_VERSION the attribute must be null and the size 0 (landlock_create_ruleset(2)): the kernel reads no memory and returns the ABI version.`
- `:957` block: no SAFETY. `// SAFETY: \`attr\` is a live #[repr(C)] \`struct landlock_ruleset_attr\` of 24 bytes and \`size\` is at most that (8, 16 or 24 for the ABI); a larger size would have the kernel read past it.`
- `:964` block: no SAFETY. [cstr].
- `:971` block: no SAFETY. [zstat].
- `:972` block: no SAFETY. [fstat] (`fd` was just opened and checked).
- `:978` block: no SAFETY. `// SAFETY: \`rule\` points at a live #[repr(C, packed)] \`struct landlock_path_beneath_attr\` of this frame, as RULE_PATH_BENEATH wants, which the kernel only reads; \`ruleset\` and \`fd\` are open.`
- `:980` block: no SAFETY. [close `fd`; the rule holds the kernel's own reference to the path].
- `:989` block: no SAFETY. [int `landlock_restrict_self(ruleset, 0)`].
- `:992` block: no SAFETY. [close `ruleset`, which this process is restricted by now and needs no more].
- `:1005` block: no SAFETY. [cstr].
- `:1038` block: no SAFETY. [cstr] (`options` is the NUL-terminated data tmpfs takes).
- `:1100` block: no SAFETY. [cstr].
- `:1108` block: no SAFETY. [cstr].
- `:1166` block: no SAFETY. [cstr] (`options` is the NUL-terminated data overlayfs takes).
- `:1254` block: no SAFETY. [cstr].
- `:1272` block: no SAFETY. [cstr].
- `:1324` block: no SAFETY. [zstat].
- `:1325` block: no SAFETY. [fstat] (`source` is an `OwnedFd`, open for the call).
- `:1405` block: no SAFETY. [cstr].
- `:1441` block: no SAFETY. [cstr].
- `:1445` block: no SAFETY. [cstr] (`here` is both the new and the old root).
- `:1448` block: no SAFETY. [cstr].
- `:1452` block: no SAFETY. [cstr].
- `:1467` block: no SAFETY. [zstat].
- `:1468` block: no SAFETY. [fstat].
- `:1474` block: no SAFETY. `// SAFETY: \`close\` takes the descriptor by value; this is the program's process, which only runs \`execve\` or \`_exit\` after, so no owner of \`fd\` (an \`OwnedFd\` copied by the fork) closes or uses it again.`
- `:1481` block: no SAFETY. [int `socket`].
- `:1488` block: 2 ops (two `ioctl`s). Split, same order: `// SAFETY: \`request\` is a live #[repr(C)] \`struct ifreq\` naming \`lo\`, which SIOCGIFFLAGS fills; \`socket\` is open.` `let got = unsafe { libc::ioctl(socket, libc::SIOCGIFFLAGS as _, &mut request) } >= 0;` `let up = got && { request.flags |= ..; // SAFETY: \`request\` is the \`ifreq\` just filled, which SIOCSIFFLAGS only reads.` `unsafe { libc::ioctl(socket, libc::SIOCSIFFLAGS as _, &request) } >= 0 };`
- `:1495` block: no SAFETY. [close `socket`].
- `:1509` block: no SAFETY. [cstr].
- `:1520` block: no SAFETY. [cstr].
- `:1524` block: no SAFETY. [cstr].
- `:1530` block: no SAFETY. [fork] (the PID namespace's first process, forked from the one-thread first process).
- `:1537` block: no SAFETY. [exit].
- `:1539` block: no SAFETY. [close `made.socket`; the program's process has its own copy].
- `:1542` block: no SAFETY. [wait].
- `:1566` block: no SAFETY. [close `listener`; it was sent, and the supervisor holds its own copy].
- `:1568` block: no SAFETY. [close `made.socket`, which is not used after].
- `:1573` block: no SAFETY. `// SAFETY: \`argv\` and \`envp\` are null-terminated arrays of pointers into \`made.argv\` and \`made.env\`, CStrings that outlive the call; \`argv[0]\` is the program's path, or the null that makes execve fail with EFAULT.`
- `:1685` block: no SAFETY. `// SAFETY: \`prog\` is a live \`sock_fprog\` whose \`filter\` points at the \`program.len()\` instructions of \`program\`, borrowed for the call, which the kernel copies; the process may install it (it holds its namespace's capabilities, or set no_new_privs).`
- `:1710` block: no SAFETY. [zmsg].
- `:1714` block: no SAFETY. `// SAFETY: \`CMSG_SPACE\` only computes a length from its argument and reads no memory.`
- `:1716` block: 7 ops (`CMSG_FIRSTHDR`, 3 field writes, `CMSG_LEN`, `CMSG_DATA`, `write_unaligned`), alignment unsound (MAY BE UNSOUND 1). Split, same order: `// SAFETY: \`msg.msg_control\` is \`space\` and \`msg_controllen\` is CMSG_SPACE(4) = 24, room for a \`cmsghdr\`, so the first header is \`space\`'s start, not null.` `let cmsg = unsafe { libc::CMSG_FIRSTHDR(&msg) };` / `// SAFETY: \`cmsg\` is the start of \`space\`, aligned for \`cmsghdr\` (once \`space\` is), writable and borrowed by nothing else.` `unsafe { (*cmsg).cmsg_level = libc::SOL_SOCKET };` / same comment, `unsafe { (*cmsg).cmsg_type = libc::SCM_RIGHTS };` / `// SAFETY: \`CMSG_LEN\` only computes a length from its argument and reads no memory.` `let len = unsafe { libc::CMSG_LEN(size_of::<libc::c_int>() as u32) };` / same comment as the field writes, `unsafe { (*cmsg).cmsg_len = len as _ };` / `// SAFETY: \`cmsg\` is a header in \`msg\`'s control buffer; CMSG_DATA only offsets it by the aligned header size, inside \`space\`.` `let data = unsafe { libc::CMSG_DATA(cmsg) };` / `// SAFETY: \`data\` is 16 bytes into the 24 bytes of CMSG_SPACE(4), so a \`c_int\` fits; \`write_unaligned\` needs no alignment.` `unsafe { std::ptr::write_unaligned(data.cast::<libc::c_int>(), fd) };`
- `:1723` block: no SAFETY. `// SAFETY: \`msg\` points at \`iov\` (one byte of \`byte\`) and the control buffer \`space\`, all live on this frame for the call, which the kernel only reads.`
- `:1734` block: no SAFETY. [zmsg].
- `:1740` block: no SAFETY. `// SAFETY: \`msg\`'s iov is one writable byte of \`byte\` and its control buffer the 64 writable bytes of \`space\`, live on this frame; the kernel writes no more than their lengths.`
- `:1750` block: 4 ops (`CMSG_FIRSTHDR`, deref, `CMSG_DATA`, `read_unaligned`), alignment unsound (MAY BE UNSOUND 1). Split, same order: `// SAFETY: \`msg\` is the header recvmsg filled; CMSG_FIRSTHDR reads its control length and gives null when no header fits.` `let cmsg = unsafe { libc::CMSG_FIRSTHDR(&msg) };` / `// SAFETY: \`cmsg\` is not null (the \`||\` stops first) and is a header the kernel wrote at the start of \`space\`, aligned for \`cmsghdr\` (once \`space\` is).` `if cmsg.is_null() || unsafe { (*cmsg).cmsg_type } != libc::SCM_RIGHTS { return None; }` / `// SAFETY: \`cmsg\` is a header in \`space\`; CMSG_DATA offsets it by the aligned header size, inside \`space\`.` `let data = unsafe { libc::CMSG_DATA(cmsg) };` / `// SAFETY: an SCM_RIGHTS message the kernel wrote holds at least one descriptor at \`data\`; \`read_unaligned\` needs no alignment.` `Some(unsafe { std::ptr::read_unaligned(data.cast::<libc::c_int>()) })`
- `:1768` block: its comment is above `let flags`, not the block. Move it down, as: `// SAFETY: SECCOMP_IOCTL_NOTIF_SET_FLAGS takes the flags by value and reads no memory; a kernel before 6.6 fails it with EINVAL, which changes nothing.`
- `:1770` block: no SAFETY. [int `pidfd_open(first, 0)`] (a pid that ended gives -1, checked below).
- `:1777` block: no SAFETY. `// SAFETY: \`fds\` is a live Vec of \`fds.len()\` \`pollfd\`s for the kernel to write \`revents\` into, borrowed by nothing else during the call.`
- `:1795` block: no SAFETY. [close `pidfd`].
- `:2224` block: no SAFETY. `// SAFETY: \`local\` is the \`len\` writable bytes of \`buffer\`, live for the call; \`remote\` is only read, in \`pid\`'s address space, where the kernel checks it; it writes at most \`len\` bytes.`
- `:2255` block: no SAFETY. `// SAFETY: process_vm_readv wrote \`read\` bytes, 0 < read <= MOST <= capacity, at the start of \`into\`'s buffer, so they are initialized.`
- `:2279` block: its comment is above `let room`. Move it down, as: `// SAFETY: \`name\` ends with a NUL (written with \`\\0\`, checked above), and \`into\` has \`room\` bytes of capacity, which readlink writes at most.`
- `:2284` block: no SAFETY. `// SAFETY: readlink wrote \`read\` bytes, 0 <= read < capacity, at the start of \`into\`'s buffer, so they are initialized.`
- `:2347` block (test): no SAFETY. [int `sysconf(_SC_PAGESIZE)`].
- `:2363` block (test, `put`): 2 ops (`add`, `copy_nonoverlapping`). Split: `// SAFETY: \`at + bytes.len()\` is inside the two mapped pages for every call below.` `let to = unsafe { pages.cast::<u8>().add(at) };` / `// SAFETY: \`to\`..\`to + bytes.len()\` is mapped, writable and not \`bytes\`.` `unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), to, bytes.len()) }`
- `:2377` block (test): 2 ops (`add`, `munmap`). Split: `// SAFETY: \`page\` bytes in is the second page of the mapping.` `let second = unsafe { pages.cast::<u8>().add(page) };` / `// SAFETY: \`second\` is the start of a page this test mapped, and nothing points into it after.` `assert_eq!(unsafe { libc::munmap(second.cast(), page) }, 0);`
- `:2382` block (test): no SAFETY. `// SAFETY: the first page of the mapping is this test's and nothing points into it after; the second was unmapped above.`
- `:2773` block (test): 2 ops, no SAFETY. [ids].

### crates/blaze-cargo/src/sandbox/macos.rs

- `:66` `unsafe extern "C"`: not checked by the lint; optional: `// SAFETY: libsystem_sandbox's \`int sandbox_init_with_parameters(const char *profile, uint64_t flags, const char *const parameters[], char **errorbuf)\`, as \`sandbox-exec\` calls it.`
- `:214` block: no SAFETY. `// SAFETY: \`buffer\` is 1024 writable bytes of this frame and its length is passed, so confstr writes at most that many; the length it needed is checked below before the slice.`
- `:472` block: no SAFETY. `// SAFETY: \`setsid\` takes no arguments and reads no memory; in this forked child it only makes it a session leader.`
- `:486` block: no SAFETY. [exit].
- `:489` block: no SAFETY. [exit].
- `:491` block: no SAFETY. [cstr].
- `:494` block: no SAFETY. [exit].
- `:496` block: no SAFETY. `// SAFETY: \`argv_ptrs\` and \`env_ptrs\` are null-terminated arrays of pointers into \`argv\` and \`env\`, CStrings that outlive the call; \`argv_ptrs[0]\` is the program's path (a run with a program has one).`
- `:499` block: no SAFETY. [exit].
- `:505` block: no SAFETY. `// SAFETY: \`siginfo_t\` is integers and a union of integers and pointers, for which all-zero bytes are a valid value.`
- `:506` block: no SAFETY. `// SAFETY: \`info\` is a live \`siginfo_t\` of this frame for the kernel to fill; \`child\` is this process's own child.`
- `:514` block: no SAFETY. `// SAFETY: \`kill\` takes integers only and reads no memory; \`child\` has ended but is not reaped (WNOWAIT above), so its group's id is no other's.`
- `:516` block: no SAFETY. [wait].
- `:584` block: no SAFETY. `// SAFETY: \`kill\` with signal 0 takes integers only, reads no memory and sends nothing.`
- `:597` block: no SAFETY. Same text as `:584`.

### crates/blaze-cargo/src/sandbox/v2.rs

- `:126` block: no SAFETY. [zstat].
- `:132` block: no SAFETY. [int `fcntl(fd, F_GETFL)`] (a closed descriptor is an error return).

`windows.rs` (44 blocks, 1 `unsafe fn` with `# Safety`): every block has a SAFETY comment and one op. Nothing to do.

### crates/blaze-usercode

- `src/trace.rs:400`: MAY BE UNSOUND 3 above. It is the crate's only compiled unsafe (non-unix), so no `forbid`.
- `src/test/fork_server.rs` is text (`include_str!`'d into the test harness blaze writes), not under the lints: its 26 blocks each have a SAFETY comment and one op; its 3 flagged "fns" are extern declarations. Optional: `:151` and `:457` say only "a plain call". Better: `// SAFETY: \`kqueue\` takes no arguments and reads no memory.` and `// SAFETY: \`setpgid\` takes integers only and reads no memory; \`pid\` is this server's child, not reaped yet.`

### crates/blaze-macro-wasm

- `src/lib.rs:151` `unsafe impl Sync for Db`: no SAFETY (the comment above `:150` covers only `Send`). Proposed: `// SAFETY: as for \`Send\`: the pointer is dereferenced only on the thread that expands, while \`Session::run\` holds the database, which it clears before returning or resuming an unwind.`
- `src/lib.rs:748` block: no SAFETY (the comment at `:739` is above the `let beside`, and the `match` stops the lint). Proposed, on its own line above `.and_then(|it| unsafe {..})`: `// SAFETY: the store gave what it keeps under \`key\`, checked against its checksum as it was read, which \`serialize\` wrote for this key; Wasmtime refuses code of another version, configuration or CPU; other bytes would run as machine code.`
- `src/lib.rs:969` block: the comment at `:966` is above the assignment, and the block is inside a closure in `db.map(..)`. Put it right above `Db(unsafe {` to be sure: `// SAFETY: only the lifetime changes; \`run\` sets \`db\` to \`None\` before it returns or resumes an unwind (\`:1013\`), so the pointer is never read after the borrow it came from ends.`
- `src/lib.rs:742`: MAY BE UNSOUND 4 above.
- `guest/proc_macro.rs` (text, `include_str!`'d as `GUEST`, built by blaze as the wasm `proc_macro`; not under the lints): every block lacks a SAFETY comment. Optional, by kind:
  - `:79` `unsafe impl GlobalAlloc for Counted`: `// SAFETY: every method passes its arguments to \`System\` unchanged and returns what it returns; the counter is only a statistic.` Blocks `:81`, `:89`, `:97`, `:102`: `// SAFETY: the caller upholds \`GlobalAlloc\`'s contract for this call, which is \`System\`'s.`
  - `:117`: `// SAFETY: \`bytes\` has \`len\` bytes, the length the host answered for what is in its buffer, which \`take\` copies whole.`
  - `:338`, `:342`, `:346`, `:354`, `:358`, `:368`, `:372`, `:376`, `:380`, `:392`, `:396`, `:400`, `:884`, `:1376`, `:384`, `:388`, `:408`, `:438`: `// SAFETY: the import takes and gives integers only and reads no memory of the module.`
  - `:363`: `// SAFETY: \`out\` is two writable \`u32\`s of this frame, which the host writes.`
  - `:177`, `:929`, `:1112`, `:1128`, `:1134`, `:1356`, `:1363`, `:1405`, `:1410`: `// SAFETY: the pointer and length are those of a live \`&str\`/\`&[u8]\`/\`Vec\` of this frame, which the host only reads during the call.`
  - `:1344`: MAY BE UNSOUND 5 above.

### crates/blaze-expand

- `src/native/host.rs:66`, `:138`: SAFETY comments present, one op each. The crate root keeps unsafe, so no `forbid`.
- `src/native/proc_macro/lib.rs` (crate `blaze-native-proc-macro`, a workspace member through blaze-expand's dependency, with no `[lints]`, so the clippy lints don't reach it; edition 2024 still warns `unsafe_op_in_unsafe_fn`, and none is bare):
  - `:100` block: 3 ops (`write`, `add`, `write`). Split: `// SAFETY: the caller gives \`out\` room for two \`u32\`s (# Safety).` `unsafe { out.write(start) };` / `// SAFETY: the second of those two is inside the same room.` `let second = unsafe { out.add(1) };` / `// SAFETY: \`second\` is the second \`u32\` of the caller's room.` `unsafe { second.write(end) };`
  - `# Safety` missing on the 25 `pub unsafe fn`s of `sys` (`:49`, `:53`..`:127`, `:135`, `:144`, `:156`, `:162`, `:164`, `:166`). The 5 that take pointers say "SAFETY:" in prose: make each a `/// # Safety` heading with the same text. The 20 that take integers: `/// # Safety\n/// None: \`unsafe\` only to have the signature of the wasm guest's imports, which this \`proc_macro\` calls the same way.`
  - Blocks with no SAFETY: `:50`: `// SAFETY: the caller gives the \`ptr\` and \`len\` of a \`&str\` it holds (# Safety).` `:136`, `:145`: `// SAFETY: \`ptr\` and \`len\` are the caller's \`&str\` (this fn's # Safety), as \`read\` needs.` `:158`: `// SAFETY: \`to\` has room for \`buffer.len()\` bytes (# Safety: what the call that filled the buffer said), and a fresh \`Vec\` doesn't overlap it.` `:172`: as the guest's `:117`. `:393`, `:397`, `:401`, `:409`, `:413`, `:423`, `:427`, `:431`, `:435`, `:439`, `:443`, `:447`, `:451`, `:455`, `:463`, `:493`, `:939`: `// SAFETY: this \`sys\` function takes integers only and has no requirement (its # Safety).` `:418`: `// SAFETY: \`out\` is two writable \`u32\`s of this frame.` `:232`, `:984`, `:1167`, `:1183`, `:1189`: `// SAFETY: the pointer and length are those of a live \`&str\` of this frame.`
  - `:1444`, `:1459`: SAFETY comments present, one op each.

### crates/proc-macro-api

- `src/legacy_protocol/msg/flat.rs:84` block: its comment says nothing true ("If someone mess up the communication UB can happen"); `from_u32`'s # Safety says memory safety doesn't rest on the value. Replace: `// SAFETY: \`e\` is a context the server echoes back from the spans this client sent it (\`into_u32\` of this database's contexts); another value gives a panic or another context's hygiene, never UB (\`SyntaxContext::from_u32\`'s # Safety).`
- `src/legacy_protocol/msg.rs:208` (test): fine.

### crates/proc-macro-srv

- `src/lib.rs:359` block: 2 ops (`set_var`, `remove_var`, one per match arm). Split into the arms: `match self.snap.vars.get::<std::ffi::OsStr>(name.as_ref()) {` `// SAFETY: \`ENV_LOCK\` is held (\`_guard\`) until this drop ends, and every expansion, the only code here that changes the environment, runs under it.` `Some(prev_val) => unsafe { env::set_var(name, prev_val) },` `// SAFETY: as for the arm above.` `None => unsafe { env::remove_var(name) },` `}`
- `src/lib.rs:343`, `src/scope.rs:32`, `src/tests/utils.rs:16`: fine (`scope.rs:18` is an extern block, not an `unsafe fn`).

> applied by usercode: `assemble<'l, I: Inputs>(lines: impl IntoIterator<Item = &'l Line>, ..)`.
> Also applied: `ViewInputs::names` takes `Cx::dir`'s borrowed slice (request 1).

## 7. safety (safety-fuzz, `fuzz/`): fuzz targets for usercode's parsers of untrusted bytes

What: one target each, against these public APIs. Each target checks that the parser returns
`Ok` or `Err` without panicking, and that what it accepts round-trips where an encoder exists.
- `blaze_usercode::trace::parse(&str)`: the live record. A sandboxed program can write it on
  macOS. Invariant: the returned `used` is at most `text.len()`, and it ends at a line end.
- `blaze_usercode::trace::decode(&[u8])`: the launcher's report. Round-trips with
  `trace::encode`.
- `blaze_usercode::test::wire::Frames::push` then `pop` in a loop, and `Frame::decode`: frames
  from a sandboxed test's fork server. Invariant: no frame body over the 64 MiB limit is
  buffered; round-trips with `Frame::encode`.
- `blaze_usercode::index::Shape::decode` and `blaze_usercode::run::Value::decode`: store values,
  which may come from the remote. Each round-trips with its `encode`.
- proc-macro-api's warm-server message reader (`crates/proc-macro-api/src/bidirectional_protocol*`):
  bytes from a sandboxed proc-macro server.

Why: unsafe.md, "Done when": every parser of untrusted bytes has a fuzz target.

> forwarded to safety: `fuzz/` is lane safety's.

## 8. link (`crates/blaze-live/`, the live runtime): what a test executed, per test process

What: two weak C symbols the live runtime defines and the fork server
(`crates/blaze-usercode/src/test/fork_server.rs`, mine) calls when they resolve:
- `__blaze_live_fork_child()`, called in the child right after `fork`: it zeroes the per-process
  execution counters (the child's copy of the parent's, copy-on-write).
- `__blaze_live_executed(out: *mut [u8; 32], cap: usize) -> usize`, called before the child
  exits: the code hashes of the functions whose counters moved, written to `out`; the return
  value is how many there are.

Separately, the runtime gives the daemon the set the fork server executed before its first
fork (static initializers, the harness), which counts for every test of that binary.
Why: decision 10-29 (compile-time.md, change 3) selects tests by what they *executed*.
With lazy compilation over one shared code image, a function resolved once stays resolved
for every later process, so stub misses undercount what a test ran; only per-process counters,
zeroed at fork, are exact. Code that ran before the fork reaches every test. With these hooks,
usercode adds the executed set to the fork server's `Ended` frame and feeds `Reached` from it.
Until then, `Reached` is whatever the daemon gives today, which is sound when it is a superset.

> forwarded to link: `crates/blaze-live/` is lane link's.

## From safety-fuzz-macrowasm (2026-10-29): bugs in `blaze-macro-wasm` found by reading, and three public functions to fuzz

> applied (usercode-macros): items 1–9 in `crates/blaze-macro-wasm`, not compiled yet.
> (1) `read_bytes`/`read_text` copy from a slice of memory checked first (`guest_slice`).
> (2) `random_get` checks its buffer and fills it in place (`wasi::fill_random`).
> (3) `ts_from_str` gives `-1 - give(..)` with no call site, `literal_from_str` gives `-1`.
> (4) `fd_pread`/`fd_read` clamp the offset and saturate the end.
> (5) `lex::literal` is a `Result`, refusing unterminated quoted and raw literals.
> (6) `lex::SUFFIX_MOST` (255) is refused in `lex` and in `wire::Reader`.
> (7) `Reader::stream` reserves at most the bytes left over 10.
> (8) `wire::DEPTH_MOST` (4096 groups) is refused at decode, which bounds `flatten`, `placed`
> and the drop too. The native bridge's one-pass reader in `blaze-expand/src/native/wire.rs`
> now keeps its open groups on an explicit stack and has no bound.
> (9) checked `u32` arithmetic in `fd_write`, `environ_get` and the read total, `EINVAL` on overflow.
> `pub fn carried_list`, `pub fn decode(bytes, sites)` and `pub fn check_module(bytes)` exist,
> with `check_module` and `Wasm::compile` sharing `fn linked`. Tests are beside them in `lib.rs`.
> `native/lex.rs` stays the same text as `lex.rs`.

Targets: `fuzz/targets/macro_wasm_{validate,host,wire,lex}.rs` (shared module
`macro_wasm_guest.rs`). Each input below is kept in `fuzz/corpus/<target>/` and fails that
target's seeds test until fixed. The host-target inputs are programs for the fuzz guest:
phase byte, record count, 37-byte records (an op byte, then nine `i32`s), then data. Its
input buffer is at `0x8000`, so the data after one record is at `0x8027`.

1. **`read_bytes` (`lib.rs:324`) allocates `len` before checking it against memory.**
   `output`, `panicked`, `ts_from_str` and `literal_from_str` with `len = 0xffffffff` make the
   host allocate and zero 4 GiB, then fail. Fix: check `ptr + len <= memory.data_size()`
   first, or copy from `memory.data(&caller).get(ptr..ptr + len)`.
   Input: `macro_wasm_host/output-believes-its-length.hex`.
2. **`random_get` (`wasi.rs:400`) makes `len` random bytes before checking the buffer.**
   That is up to 4 GiB and 512M hash rounds per call. Fix: take the checked slice first,
   then fill it in place. Input: `macro_wasm_host/random-get-believes-its-length.hex`.
3. **`ts_from_str` and `literal_from_str` (`lib.rs:579`, `:600`) panic outside an expansion.**
   They `expect` the call site to be numbered. During a `Session`'s `_initialize` and during
   `Wasm::list`, `sites` is `[0; 3]`. A module that parses a `TokenStream` in a static
   initializer, or in its list export, therefore panics the host. `load` and `prepare` run the
   list. Fix: return the error (`-1 - give(msg)`), or give a call site there.
   Input: `macro_wasm_host/ts-from-str-in-initialize.hex` (`01 01 15 27800000 01000000 ...` with data `78`).
4. **`fd_pread` with a negative offset panics (`wasi.rs:449`).** `from = u64::MAX as usize`,
   so `from + len` overflows: a debug panic. In release the wrapped end is below the start,
   giving `bytes[file_len..4]`, which also panics. Fix: `from = usize::try_from(at).unwrap_or(usize::MAX).min(bytes.len())`
   and `saturating_add`. Input: `macro_wasm_host/fd-pread-negative-offset.hex`
   (`fd_pread(4, iov, 1, -1, 0x300)`).
5. **`lex::literal` (`lex.rs:90`) panics on an unterminated raw string at the end of the text.**
   The inputs are `r"`, `r#`, `br"`, `br#`, `cr"` and `cr#`. The lexer gives
   `n_hashes: None` with the token only its prefix, so `lit[2..1]` (or `[3..2]`) panics. Any
   module reaches it through `"r\"".parse::<TokenStream>()`, where rustc gives a `LexError`, and
   so does the public `tokens`. Fix: an `Err` when `n_hashes` is `None` or a quoted literal is
   unterminated. Inputs: `macro_wasm_lex/raw-string-unterminated.hex` (`7222`),
   `raw-string-hash-at-end.hex` (`7223`), `raw-byte-string-unterminated.hex` (`627222`),
   `macro_wasm_host/ts-from-str-unterminated-raw-string.hex`.
6. **A literal suffix over 255 bytes panics `tt::Literal::new`.** The panic is
   `suffix_len: u8 = suffix.len().try_into().unwrap()` (`crates/tt/src/lib.rs:408`, `:467`,
   format's). It is reached from `lex::literal` (text `1` then 256 letters) and from
   `wire::Reader` (a literal the module wrote with a long suffix). Fix here: refuse it (`Err`)
   in both until tt holds longer suffixes; format is told. Inputs:
   `macro_wasm_lex/suffix-256.hex`, `macro_wasm_wire/literal-suffix-256.hex`,
   `macro_wasm_host/literal-from-str-long-suffix.hex`.
7. **`wire::Reader::stream` (`wire.rs:253`) believes a stream's count.** It reserves
   `count.min(1 << 16)` trees on a 4-byte count, about 5.7 MB at roughly 88 bytes a `Tree`.
   Each nested group's stream does the same, so 18 bytes a level hold 5.7 MB a level live: 100
   levels (1.8 KB of output) is 570 MB. Fix: cap the reservation at `(bytes.len() - at) / 10`,
   the fewest bytes a tree takes. Input: `macro_wasm_wire/count-believed.hex` (`ffff0000`).
8. **Unbounded recursion in the wire decoder.** `Reader::tree`/`stream`, `wire::flatten`,
   `placed` and the drop of `Vec<Tree>` recurse once per nested group, at 18 bytes a level.
   A module output of a few MB overflows the expanding thread's stack, which aborts the
   process rather than panicking. Fix: decode with an explicit stack, or stop with an error
   at a depth bound. The input is too big for the corpus: `01000000`, then
   `00 00 01000000 01000000 01000000 01000000` repeated 500000 times, then `00000000`.
9. **Overflow-checked arithmetic on guest numbers (panics in debug builds only).**
   - `fd_write` (`wasi.rs:425`): `total += len as u32` overflows when iovecs sum past 4 GiB
     (16385 iovecs over the whole memory). Input: `macro_wasm_host/fd-write-total-overflows-u32.hex`.
   - `environ_get` (`wasi.rs:363`, `:370`): `environ + 4 * index as i32` and
     `at += entry.len() as i32` overflow `i32` for pointers near 2 GiB in a memory over 2 GiB.

   Fix: wrapping or checked `u32` arithmetic, giving `EINVAL` where it would overflow.
10. **Note, not a request:** the engine has no epoch or fuel. A module that loops in
    `_initialize` or its list hangs `load`/`prepare`, as a native proc macro hangs rustc. For
    that reason `macro_wasm_validate` never runs the module it is given.

Public functions the targets need, to feed bytes without a wasm instance (each is a thin
wrapper of what is private now). The targets go through `Wasm::at`, `Session::expand` and
`tokens` until these exist:
- `pub fn carried_list(bytes: &[u8]) -> Option<String>` (the private one at `lib.rs` `fn carried_list`).
  `prepare_soon` runs it on every module before compiling it. Today it can only be reached
  through `prepare_soon`, which starts an unjoined thread, or `prepare`, which runs the module.
- `pub fn decode(bytes: &[u8], sites: [Span; 3]) -> Result<tt::TopSubtree, String>`:
  `Reader::new(bytes, &spans).stream()` over `Spans` of the three sites (numbers 1, 2, 3), then
  `wire::top_subtree(trees, sites[1])`. This is the wire decoder without an instance per input.
- `pub fn check_module(bytes: &[u8]) -> Result<(), String>`: `Module::new(engine(), bytes)`
  then the linker of `Wasm::compile` (`wasi::add`, `add_host`, `wasi::rest`) and
  `instantiate_pre`. Unlike `Wasm::at`, it neither caches in `COMPILED`, which keeps every
  module a fuzz run loads, nor writes machine code beside a file.

> forwarded to usercode: `crates/blaze-macro-wasm/` is lane usercode's.

## From safety-fuzz-usercode (2026-10-29): `elide` allocates the square of a script's size

What: `crates/blaze-build-script/src/elide.rs`, `format`. A placeholder `{N}` may name one
argument any number of times, so a root file of n bytes can elide to about n²/12 bytes, all
built in the daemon (outside any sandbox) before `directives_only` judges it. A 9 KB script
(`println!("cargo:warning={0}{0}…")` with 2000 `{0}` and a 3000-byte argument) makes a 6 MB
`String`; a 1 MB `build.rs` in a dependency would ask for about 80 GB and abort the daemon.
Running such a script is fine (its output is bounded by the sandbox); reading it is not.

Fix: in `format` (and so `printed`), give `None` (the script is run, as for any form not read)
once `out.len()` plus what is about to be pushed passes a bound linear in the source, checked
before each `push_str`: `16 * source.len()` keeps every allocation elision makes under
`blaze_fuzz::allowed(len)` (1 MiB + 64 bytes per input byte), with room for `String`'s
doubling. No real script comes near it: every one in `elide/tests.rs`'s census prints less
than its source.

Input (the root file, as hex: `fuzz/corpus/usercode_script_elide/a-placeholder-reused-past-the-bound.hex`,
9046 bytes): `fn main() { println!("cargo:warning=` + `{0}` × 2000 + `", "` + `a` × 3000 + `"); }\n`.
Target `usercode_script_elide` checks `largest_allocation(elide) <= allowed(len)`, so its seeds
test fails on that file until this is fixed.

Why: docs/v2/plan/design/unsafe.md "Done when" (every parser of untrusted bytes is bounded);
the daemon must not be taken down by a dependency's source.

> forwarded to usercode: its own crate.

## 9. fastcompile: `Inputs::env_all` lends the environment (`crates/blaze-testsel/src/select/tests.rs`)

What: `blaze_usercode::trace::Inputs::env_all` is now `fn env_all(&mut self) -> &[(OsString, OsString)]`,
sorted by name, each name once. `World`'s impl becomes:

```rust
fn env_all(&mut self) -> &[(OsString, OsString)] {
    self.env.sort();
    &self.env
}
```

Why: a cache check of a run that read the whole environment (every traced run today, request
6) copied it per check. `index::find` now hashes a shape's inputs straight into the result key
and allocates nothing on a hit. Also `index::Found::Hit` is `{ key, value }` (no `traced`) and
`RunKey::DOMAIN` is "blaze v2 run traced 2", if blaze-testsel's tests name either.

> forwarded to fastcompile: `crates/blaze-testsel/` is its (told by tmux).

## 10. db (`crates/blaze-query/`): a view's tree hash as one query

What: `Cx::tree(&mut self, dir: &str, package: bool) -> Result<Option<[u8; 32]>, Cancelled>`,
a query keyed by `(dir, package)`. Its value is `blaze_usercode::trace::walk`'s hash of the
view below `dir`: each path in order, a directory as `part(rel).part(b"d")`, a file as
`part(rel).part(blake3)`, and with `package` nothing under a name starting with `.` or the top
`target`. It is computed bottom-up from each directory's own query, so an edit recomputes only
the directories above it. Also, `Cx::dir`'s entries sorted by name (one sort when the listing
is made), so `ViewInputs::names` and `dir` stop sorting per check.

Why: a build script that prints no `rerun-if-changed` is keyed by its whole package (cargo's
rule). Every check of it walks the package today, one `names` and one file hash per entry,
while red-green revalidation of the same tree is O(1) when nothing changed. I'll call it from
`ViewInputs` for a `PACKAGE`/`TREE` path inside the view once it exists.

> forwarded to db: `crates/blaze-query/` is its (told by tmux).

## 11. vfs: what changed outside every view, for `inputs::Files`

What: the watcher also covers the directories outside views that user code reads (the
toolchain, the C compiler, `/usr/include`, each traced path's parent), and gives
`fn unchanged_since(path: &Path, generation: u64) -> bool` and `fn generation() -> u64`
(an `AtomicU64` read, no lock). `Files` stores the generation with each entry and skips the
`stat` when nothing below changed.

Why: `Files::file` makes one `stat` per traced path outside the view per check. A C build
script reads hundreds of headers, so that is hundreds of syscalls per cache hit. Watchman
and Bazel's `--watchfs` answer the same question from the watcher with no syscall.

> forwarded to requests-outside (child of `requests`; vfs and core are DONE): the table in blaze-vfs, its sans-io watch driver, and `Effect::Flush` in blaze-core so a request can drain queued events before trusting a generation (events arrive after the change). usercode's `Files` and cargo's per-request flush come back as its requests.
