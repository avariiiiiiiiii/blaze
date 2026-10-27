# nocargo lane handoff (2026-10-27)

Brief: /tmp/blaze-lanes/nocargo.txt. Goal: `cargo blaze build`/`test` of harness/norustc/fixture
and ripgrep exec no cargo (beyond the typed `cargo blaze` itself).

## State
- Worktree /Volumes/Projects/blaze-nocargo-1027, branch nocargo-1027 off origin/main 47793534d2.
- VM `nocargo` (arm64, 32 cpu, 32G committed, ttl 12h). Tree at /root/nc/src, release build
  `nix develop -c cargo build -p ra-jit --release` (unit nc-build, log /root/nc/build.log).
- Trace scripts: /tmp/nocargo-scripts/{trace.py,classify.py} on the Mac, copied to /root/nc/.

## Step 1: measurement (done, 2026-10-27, main 47793534d2, VM nocargo arm64)

`strace -f --seccomp-bpf -e trace=execve,clone,clone3,fork,vfork` of `cargo blaze build` and
`cargo blaze test` from a clean `target/`, with and without RA_JIT_WHOLE_BUILD=1, daemon stopped
before each run and after it went quiet (scripts: /tmp/nocargo-scripts/trace.py, classify.py;
JSON in /tmp/nocargo-scripts/r1). Counted: execs of the toolchain's real cargo (the rustup proxy
in front of it is not counted twice). All 8 runs exit 0.

| caller | asks | from | fixture-build-whole | fixture-build | fixture-test-whole | fixture-test | ripgrep-build-whole | ripgrep-build | ripgrep-test-whole | ripgrep-test |
|---|---|---|---|---|---|---|---|---|---|---|
| `vendor/rust-analyzer/crates/project-model/src/sysroot.rs load_library_via_cargo` | metadata of the sysroot's library/ | unit load | 2 | 4 | 2 | 2 | 14 | 14 | 18 | 20 |
| `vendor/rust-analyzer/crates/project-model/src/sysroot.rs load_library_via_cargo` | metadata of the sysroot's library/ | workspace load | 2 | 2 | 2 | 2 | 2 | 2 | 4 | 4 |
| `vendor/rust-analyzer/crates/project-model/src/sysroot.rs load_library_via_cargo` | metadata of the sysroot's library/ | ? | 0 | 0 | 0 | 0 | 2 | 2 | 4 | 4 |
| `vendor/rust-analyzer/crates/project-model/src/cargo_config_file.rs CargoConfigFile::load` | config get --format toml --show-origin | workspace load | 1 | 1 | 1 | 1 | 1 | 1 | 2 | 2 |
| `vendor/rust-analyzer/crates/project-model/src/toolchain_info/version.rs get` | --version | workspace load | 1 | 1 | 1 | 1 | 1 | 1 | 2 | 2 |
| `vendor/rust-analyzer/crates/project-model/src/toolchain_info/rustc_cfg.rs rustc_print_cfg` | rustc --print cfg | workspace load | 1 | 1 | 1 | 1 | 1 | 1 | 2 | 2 |
| `vendor/rust-analyzer/crates/project-model/src/toolchain_info/target_data.rs get` | rustc --print target-spec-json | workspace load | 1 | 1 | 1 | 1 | 1 | 1 | 2 | 2 |
| `crates/ra-jit/src/plan.rs UnitGraph::of` | build --unit-graph | workspace load | 1 | 1 | 1 | 1 | 1 | 1 | 2 | 2 |
| `crates/ra-jit/src/plan.rs Flags::of` | config get --format json | workspace load | 1 | 1 | 1 | 1 | 1 | 1 | 2 | 2 |
| `vendor/rust-analyzer/crates/project-model/src/build_dependencies.rs build_command (main.rs workspace_at)` | check --compile-time-deps | workspace load | 1 | 1 | 1 | 1 | 1 | 1 | 2 | 2 |
| `(typed) cargo blaze` | the user's command | - | 1 | 1 | 1 | 1 | 1 | 1 | 1 | 1 |
| **cargo total** | | | 12 | 14 | 12 | 12 | 26 | 26 | 41 | 43 |

Reading it:
- RA_JIT_WHOLE_BUILD no longer exists in the code: build/run/test always take `whole.rs`. Both
  columns are the same path; differences are run-to-run (how many unit loads ran).
- "unit load" = ra-jit loading a `rust-project.json` it wrote to compile one build script or proc
  macro (own_script.rs/own_macro.rs, under `--compile-time-deps`). Each load asks cargo for the
  sysroot's `library/` metadata twice (`--no-deps`, then full). "?" = the same from a sandboxed
  probe whose parent strace could not see.
- Biggest caller by far: sysroot.rs `load_library_via_cargo` (2 per load, every load): 28 of
  ripgrep test's 43.
- No `cargo fetch`/`generate-lockfile`: blaze-registry fetches already. No workspace
  `cargo metadata`: blaze-metadata answers (OwnMetadata) for every load measured.
- rustc execs in the raw trace are `target/.../ra-jit-rustc/rustc` stand-ins for build-script
  probes (anyhow, proc-macro2, libc, serde: `--version` and probe compiles), not the toolchain's.

Per workspace load: config get toml, --version, rustc --print cfg, rustc --print
target-spec-json, sysroot metadata x2, build --unit-graph, config get json, check
--compile-time-deps = 9, plus 2 per unit load.

## Step 2: removal, in parallel sub-lanes (started 2026-10-27 ~02:15 PDT)
Shared brief: /tmp/blaze-lanes/nocargo-sub-common.md. Each has worktree
/Volumes/Projects/blaze-nocargo-<sub>-1027, branch nocargo-<sub>, VM nc-<sub>, own HANDOFF.md.
- `sysroot`: sysroot.rs load_library_via_cargo (2 per load, biggest) -> blaze-metadata.
- `toolq`: config get toml/json, --version, rustc --print cfg / target-spec-json; workspace.rs
  "so cargo does" fallbacks -> errors.
- `unitgraph`: plan.rs UnitGraph::of -> pure unit graph in cargo's schema (#458-shaped).
- `ctdeps`: --compile-time-deps -> blaze's own build-script/proc-macro driver over the unit graph
  (depends on unitgraph to land the switch).
Parent: wait for them, re-trace after merges (r2), update docs/no-rustc.md, write
/tmp/blaze-lanes/nocargo.result.md. Parent VM `nocargo` still up for re-traces.
