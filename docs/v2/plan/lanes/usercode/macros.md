---
status: plan
summary: How blaze v2 expands proc macros (warm sandboxed servers, wasm modules, native expanders), keys and keeps the expansions, and what it does to be the fastest.
date: 2026-10-29
---
# Proc macros

Part of [usercode](../usercode.md). Code: `crates/blaze-expand/`, `crates/blaze-macro-wasm/`,
`crates/proc-macro-api/` (warm servers, `src/warm.rs`), `crates/proc-macro-srv/` (scope
markers). Native expanders are in [macros-native.md](macros-native.md). Requests:
`v2/requests/usercode-macros.md`.

## What runs where

A macro runs one of three ways, chosen by `blaze_macro_wasm::rule`:

- **Natively in process.** This is a native expander for a verified crate, used on an exact
  byte match only.
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

- **Native expanders and wasm modules run in process.** There is no IPC, and a module is
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
