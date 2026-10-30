# Requests from lane usercode-macros

Proc-macro expansions as traced runs of user code (docs/v2/plan/lanes/usercode.md section 6.5, #1297,
#805). The code in `crates/blaze-expand/`, `crates/blaze-macro-wasm/`, `crates/proc-macro-api/`
and `crates/proc-macro-srv/` is written against exactly the items below. None of it has been
built: the lane may not build.

## 1. `crates/blaze-engine`: install blaze-expand with the content store

Where: `crates/blaze-engine/src/lib.rs`, the call `blaze_expand::install();` (about line 8890).

Why: expansions are kept in `blaze_cas::Store` through `blaze_usercode::index::{find, keep}`,
not in `blaze-store`; blaze-expand needs the store, the toolchain's id and what warm servers
pass through. `install` now returns an error for a bad `BLAZE_PROC_MACRO_CACHE` (only unset,
`keep` or `check`; `off`, `0` and the other old modes are gone) or `BLAZE_NATIVE_MACROS`
(`on`, `verify`, `off`).

```rust
let environment = proc_macro_api::warm::Environment::new();
blaze_expand::install(blaze_expand::Expansions {
    store: the_daemons_cas_store.clone(),       // Arc<RwLock<blaze_cas::Store>>
    toolchain: the_pinned_toolchains_content_id, // blaze_ids::ContentId
    environment: environment.clone(),
})?;
```

and, per request, before expanding: `environment.pass(the_requests_env)?` (a
`Vec<(OsString, OsString)>` of the variables passed through to proc macros; a non-UTF-8 one is
an error naming it).

`blaze_macro_wasm::flush()` now flushes only machine code and verdicts; expansions are in the
CAS store, which the engine flushes as it does any other.

> forwarded to codegen: `crates/blaze-engine/src/lib.rs` is codegen's.

## 2. `crates/blaze-engine` (or the daemon's pool owner): warm proc-macro servers

Where: wherever `ProcMacroClient::spawn` is called for a load.

Why: servers must run sandboxed and traced, warm, from the parent lane's `Pool`, over a Unix
socket (#805: a server is not keyed by `PATH`).

- For each server: `proc_macro_api::warm::Server { program, code, dir, read, write, memory,
  sandboxed: true }`; `server.command()` is the `Command` (sandbox `On { trace: true, network:
  false }`, only `warm::SET` in its environment) and `server.key()` its `WarmKey`.
- When the pool says the server is ready on connection `conn`: `let (bridge, link) =
  warm::Bridge::new(conn, tags, wake)`, where `tags` is a base no other effect uses and `wake`
  wakes the loop. The loop calls `bridge.effects(&mut effects)` each turn, hands it every
  completion whose tag `bridge.owns(tag)`, and `bridge.end(why)` when the server exits.
- `ProcMacroClient::warm(program, vec![warm::Warm { link, traced: Some((live_record,
  socket)) }, …], environment)`.

> forwarded to codegen: `ProcMacroClient::spawn`'s callers are in `crates/blaze-engine/` (codegen's); under v2 the daemon's pool owner is lane cargo's daemon, told with the same message.

## 3. `vendor/proc-macro-srv`: the warm protocol

Where: the vendored server's main and its expand loop.

Why: the client needs API version 7 (`proc_macro_api::SCOPED_EXPANSIONS`) from a warm server,
which marks each expansion's scope so its reads can be told apart in the live record.

- Connect to the socket named by `BLAZE_WARM_SOCKET` and speak the
  `bidirectional-postcard-prototype` format over it instead of stdin/stdout; unset the
  variable once connected.
- Count every `ExpandMacro` request, from 0, before anything can fail, and run each on its
  thread between `access("/.blaze-test/expand-<n>")` and `access("/.blaze-test/")`: copy
  `crates/proc-macro-srv/src/scope.rs`, which `crates/proc-macro-srv` already does.
- Report API version 7.
- Read the environment a macro is given by name only: `EnvSnapshot` walks `environ`, which the
  trace records as reading every variable, so no expansion would be keyed by fewer.

> forwarded to coordinator: `vendor/proc-macro-srv/` has no owner in crates.md (usercode owns `crates/proc-macro-srv/`), so it is the coordinator's to apply or give to usercode.

## 4. `crates/blaze-cargo/src/cargo/own_macro.rs`: the wasm or native rule

Where: `runs_as_wasm`, `wasm`.

Why: the rule is now `blaze_macro_wasm::rule::runs(name, &native, wasm)`, which also refuses a
`BLAZE_PROC_MACRO_WASM` that is neither `0`/`off` nor `1`/`on`; a module that can't be built
should fail with `blaze_macro_wasm::rule::refused(name, why)`. Use them in place of the copies
there.

> forwarded to cargo

## 5. Native expanders (child `usercode-macros-native`)

Apply `v2/requests/usercode-macros-native.md`: vendor the nine verified crates, add the six
`native-*` workspace dependencies (`crates/blaze-expand/Cargo.toml` already names them, so the
manifest lint reports them missing until then), and register `native::describe_with(…)` beside
`blaze_expand::install`. Without the registration every load fails while
`BLAZE_NATIVE_MACROS` is unset (its default, `on`), naming the macro; that is the intended
no-fallback behavior, not something to paper over.

## 6. Workspace

The crates' own manifests now name `blaze-core`, `blaze-ids`, `blaze-usercode`, `blaze-cas`,
`blaze-expand` and `blake3` with `workspace = true`; all are in the root `[workspace.dependencies]`
already. `blaze-expand` no longer uses `sha2` or `blaze-store`.

> applied: checked: `blaze-core`, `blaze-ids`, `blaze-usercode`, `blaze-cas`, `blaze-expand` and `blake3` are all in the root `[workspace.dependencies]`.
