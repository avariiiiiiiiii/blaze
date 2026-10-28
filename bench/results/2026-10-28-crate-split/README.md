# Splitting blaze-engine: what a cold build and a one-line edit cost (lane onews, step 4)

ix VM, x86_64, 64 vCPU, the flake's nightly. Every number is the wall time of
`cargo build --release --locked -p blaze-engine` in seconds, `--timings` kept. An edit is
one line appended to the file as a comment, built, then taken out again; each tree is
built warm before its edits. Trees take turns, so a slow minute of the machine falls on
both.

## (a) the engine is a library and a thin bin (#853)

`crates/blaze-engine/src/main.rs` became `lib.rs`; `main.rs` only calls
`blaze_engine::main()`. Base f9a83eaf47.

| | main.rs | lib.rs + thin bin |
|---|---:|---:|
| cold | 89.1 | 79.3 |
| edit `blaze-engine/src/lib.rs` (main.rs before) | 33.1, 32.5 | 33.3, 31.5 |
| edit `hir-ty-codegen/mod.rs` | 64.8, 50.4 | 40.4, 44.1 |
| edit `blaze-engine/src/cargo/mod.rs` | 37.7, 31.5 | 32.0, 33.2 |

Cold is 10 s shorter because a bin waits for every dependency's rlib and a library starts
on their metadata: in `--timings`, the engine starts at 48.2 s instead of 57.5 s. An edit
in the engine is the same, as it should be: the same crate compiles again.

## (b) the cargo drop-in is a crate of its own, `blaze-cargo` (#858)

`cargo/`, `plan`, `remote`, `deps`, `renamed`, `answers` and the leaves under them
(`dep_info`, `errors`, `memory`, `process`, `sandbox`, `socket`, `stub`, `trace`, `tree`,
`unstable`, `weaken`) moved to `crates/blaze-cargo`, which the engine depends on.
Before is (a) at 1451d7d9ef; after is (b) on it. Every cold build starts from an empty
target; the last two of each are ABAB.

| | (a) | (b) |
|---|---:|---:|
| cold | 96.5, 85.5, 86.2 | 77.1, 80.3, 82.5 |
| edit `blaze-engine/src/lib.rs` | 32.4, 31.8 | 18.8, 18.5 |
| edit `hir-ty-codegen/mod.rs` | 41.2, 41.6 | 37.6, 37.0 |
| edit `cargo/mod.rs` | 33.4, 31.5 | 27.5, 27.9 |
| edit `cargo/daemon.rs` | 33.5, 31.4 | 28.2, 27.3 |

An edit in the engine is 13 s shorter: 45k of its 78k lines are now `blaze-cargo`, which
does not compile again. An edit in the cargo drop-in compiles `blaze-cargo` and then the
engine, which is still shorter than the one crate they were. Cold is 4 to 6 s shorter
(medians 86.2 and 80.3). In `--timings` of the first cold build, (a)'s engine runs from
49.5 s to 85.1 s; in (b), `blaze-cargo` runs from 53.0 s to 71.1 s and the engine, which
starts on its metadata, from 62.4 s to 80.0 s, so the two overlap by 9 s.

`blaze-cargo` still depends on `hir-ty-codegen` (for `Target`, `ObjectFormat`, `host_cpu`,
`BUILTINS`, `STAND_IN_SAYS`), on `ide-db` (in `memory`, `tree`, `process`), on
`blaze-lints` (`cargo/own/clippy.rs`) and on `blaze-macro-wasm` (`compile_time`,
`own_macro`), so it starts after the front end's metadata. With those four edges cut, it
could compile alongside `hir-ty`.
