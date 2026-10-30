---
status: plan
summary: How blaze v2 expands proc macros (warm sandboxed servers and wasm modules, until change 20's evaluator replaces both), keys and keeps the expansions, and what it does to be the fastest.
date: 2026-10-29
---
# Proc macros

Part of [usercode](../usercode.md). Code: `crates/blaze-expand/`, `crates/blaze-macro-wasm/`,
`crates/proc-macro-api/` (warm servers, `src/warm.rs`), `crates/proc-macro-srv/` (scope
markers). Requests:
`v2/requests/usercode-macros.md`.

## What runs where

Until proc macros run on blaze's own compiler ([compile-time.md](../../design/compile-time.md),
change 20, lane fastcompile), a macro runs one of two ways, chosen by `blaze_macro_wasm::rule`.
The native expanders of #1297 are superseded by change 20 and removed; what they had of the
bridge is handed to fastcompile ([below](#for-fastcompile-the-proc-macro-bridge)).

- **As a wasm module in process.** This is the default, and the module may read only its
  package.
- **In a sandboxed warm server.** This is for packages listed in `native-proc-macros`, or for
  every macro under `BLAZE_PROC_MACRO_WASM=0`.

A wasm module that can't be built or run is an error that names the macro. It is never run
natively in its place.

## Keys

An expansion is a run of user code: `RunKey { code, given }`, found and kept through
`blaze_usercode::index` in `blaze_cas::Store`.

- `code` is the content ID of the library (a server) or of the module's bytes (wasm).
- `given` holds the name, the kind, the proc-macro ABI, the toolchain (a server), the input and
  attribute tokens with spans relative to the call, the directory, and the crate's variables.
- The result is keyed by what the run was traced reading:
  - **A warm server:** the lines of the expansion's scope in the sandbox's live record. The
    server marks each scope with `access("/.blaze-test/expand-<n>")`.
  - **A wasm module:** the reads the WASI host recorded, plus each question the module asked
    of a span, with its answer.

Clock and random reads make a run untraceable, and it is not kept. A run is also not kept when
it asked the database what its key doesn't say, or when its output has spans its input
doesn't. `BLAZE_PROC_MACRO_CACHE=check` expands anyway and turns any difference from what was
kept into an error.

## Speed

The bar is rustc's in-process proc-macro bridge: one run of the macro for each call. A hit
here costs no run. It is one BLAKE3 of the call, one lookup in the content store, a stat of
each traced input (its hash is cached by stat, and BLAKE3 runs only when the stat changed),
and a decode of the expansion in place from the store's bytes. The other two references are
rust-analyzer's proc-macro server, which has a process boundary on every call and no cache,
and dtolnay's watt, which runs wasm proc macros in process with no cache. A blaze hit does
less than all three: it neither runs the macro nor crosses a process.

- **Wasm modules run in process.** There is no IPC, and a module is
  compiled once to machine code, which the store keeps by digest.
- **Warm servers** start once per library and sandbox, keyed without `PATH` or the variables
  passed through (#805). Those variables are sent with each expansion instead. The client
  asks the pool for its socket I/O as effects, through the sans-IO `Bridge`.
- **No lock or shared write on a read.**
  - The passed environment is interned and published through one atomic.
  - The input check uses a hash table with no lock, shared by the whole process
    (`blaze_usercode::inputs`).
  - The store's read view is requested from the store lane.
- **No copy on a hit.** The expansion is decoded from the stored bytes. The `check` mode
  alone copies them, because it needs them to compare against.
- **Still to do** (requests, section 6):
  - a store read view without a lock;
  - the trace record streamed over the server's socket instead of read from a file;
  - a reused thread pool in the vendored server;
  - a `Given` that hashes as it is built. The tree encoding already goes into one reused
    buffer per thread, but `Given` still copies every part and variable before hashing them.

## For fastcompile: the proc-macro bridge

Change 20 runs a macro's own code on the MIR evaluator and copy-and-patch. Everything below
already exists, and is exact against rustc's proc-macro server, so a port can reuse it.

- **The `proc_macro` a macro is compiled against.** `crates/blaze-macro-wasm/guest/proc_macro.rs`
  is blaze's own `proc_macro`: the full public API, with token streams, spans and literals as
  host handles. `crates/blaze-expand/src/native/proc_macro/lib.rs` is the same file, held equal
  to it by a test. Its host calls go to a thread-local `__native::Host` in process, instead of
  to wasm imports. This is the crate to build macros against: its `sys` functions become the
  evaluator's shims, and they are the only way out.
- **The shims.** These are the host functions, one per `sys` import, in `add_host` in
  `crates/blaze-macro-wasm/src/lib.rs` and in `crates/blaze-expand/src/native/host.rs`:
  - sites: `def_site`, `call_site`, `mixed_site`;
  - span operations: `span_join`, `span_parent`, `span_source`, `span_start`, `span_end`,
    `span_byte_range`, `span_subspan`, `span_located_at`, `span_resolved_at`, `span_line`,
    `span_column`, `span_debug`;
  - questions of the database: `span_file`, `span_local_file`, `span_source_text`;
  - lexing: `ts_from_str`, `literal_from_str`;
  - handing data across: `take`, `output`;
  - outcomes: `panicked`, `diagnostic`;
  - no-ops: `track_path`, `track_env_var`;
  - `retained`, which reports that an expansion left allocations live: a stateful macro.

  The wasm host also answers WASI preview 1 (`crates/blaze-macro-wasm/src/wasi.rs`). It gives
  the package read-only at `/blaze/package`, an empty `OUT_DIR`, cargo's `CARGO_*` variables
  only, and the real clock and randomness, marked untraceable.
- **Span numbering.** The def, call and mixed sites are numbers 1, 2 and 3, the same spans or
  not. The input's spans follow in the order they first appear in the encoded stream (`Spans`,
  `wire.rs`). Keys carry spans relative to the call (`Shapes`: anchors and contexts as indices),
  so one expansion is found again for the same tokens in another file or item.
- **The wire format.** `crates/blaze-expand/src/native/wire.rs` and the wasm host's `wire.rs`
  are byte-identical but for one `use`. They hold the token stream encoding and a one-pass
  reader straight into rust-analyzer's flat tree, with no intermediate allocation.
  `lex.rs` lexes literals and `ts_from_str` exactly as proc-macro-srv does, with
  `rustc_lexer`.
- **Questions are inputs.** A question asked of a span (its source text, file, line and column,
  or byte range) is answered by `load_cargo::answer`. It is recorded with its answer as an
  input of the run (`Value::read` and `Value::said`, as NUL-named entries of `Traced::env`), so
  the run is found again only while the database answers the same. A question the key can't
  say makes the run unkeepable (`State::asked`).
- **Keys and keeping.** A run is `RunKey { code, given }` through `blaze_usercode::index`:
  `code` is the macro's code ID, and `given` is the host version, name, kind, input and
  attribute bytes, span shapes and cargo variables. Untraceable reads, an unkeepable question,
  retained state, or output spans the call doesn't have all mean the run is not kept.
  `BLAZE_PROC_MACRO_CACHE=check` re-runs a kept run and fails on any difference.
  `blaze_expand::native::difference` names the first token or span that differs, which is the
  comparison for the evaluator against the wasm module while change 20 lands.
- **Stateful macros.** A module whose expansion left allocations live gets a verdict in the
  store: stateful or stateless, by module digest. A stateful one runs in a session per calling
  crate and database revision, and nothing it makes is kept (`Session`, `Verdict`). The
  evaluator needs the same detection, because statics in a macro survive between calls under
  rustc.
- **What change 20 makes obsolete here:** the warm servers (`proc-macro-api/src/warm.rs`), the
  scope markers (`proc-macro-srv/src/scope.rs`), the wasm build of macros, and `rule.rs`.
  Remove them once the evaluator runs every macro, with no fallback to them.
