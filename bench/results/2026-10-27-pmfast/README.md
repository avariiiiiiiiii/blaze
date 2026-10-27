# Proc macros as wasm: the machine code kept path-free, compiled off the load (lane pmfast)

ix VM, arm64, 16 vCPU, cranelift, the flake's nightly. The corpus is one binary using the
macros of serde (Serialize, Deserialize), clap (Parser), tokio (main), thiserror (Error),
async-trait, pin-project, derive_more (Display) and strum (EnumString), with serde_json:
8 proc-macro modules. Each run is one `ra-jit <ws> --own-target --crate pmcorpus --out ..
--exe ..` (the build `cargo blaze build`'s daemon makes) with `RA_JIT_CACHE` set to the
arm's own store and `RA_JIT_STATS=1`, timed whole. native is `RA_JIT_PROC_MACRO_WASM=0`
(blaze builds the macro crates as native libraries for its server).

States: **cold** an empty store and target; **again** the next run with nothing changed;
**edit** a changed string in `main.rs`'s body; then **again** after it.

## Before and after, wall seconds

base is main at 855f615913; patched is this branch. One run of each, in this order.

| arm | cold | again | edit | again |
|---|---|---|---|---|
| wasm, base | 80.05 | 49.94 | 5.18 | 2.90 |
| wasm, patched | 48.09 | 4.88 | 4.42 | 1.92 |
| native, base | 46.07 | 14.47 | 3.98 | 1.99 |
| native, patched | 43.12 | 15.19 | 3.76 | 2.03 |

## wasm against native, alternating (`abab.sh`)

Per round, each arm: cold, again, then three times edit and again; three rounds, the arms
taking turns so that neither has the machine's later state. Medians of the samples (cold 3
each, again-after-cold 3 each, edit and steady again 9 each).

| arm | cold | again | edit | again |
|---|---|---|---|---|
| wasm (all of this branch) | 46.08 (44.83, 47.25, 46.08) | 5.34 | 4.05 | 1.89 |
| native | 45.17 (45.17, 42.58, 46.16) | 13.38 | 3.99 | 2.02 |

## Where the cold wasm build's time went, base

- Cargo's step: 6.7 s, the same in both arms.
- The load compiled every module with Cranelift, one after another (load-cargo loads them in
  order), to read its list of macros: 4.4 to 10.9 s each, 21 to 50 s in all.
- The run after it ("again", 49.9 s) compiled them all again: the store's key of the machine
  code had the store's build identity in it, which changes with the first toolchain name the
  process sets (`compiles_against`), so the next process looked under another key.

## What the branch changes

- The machine code is kept under a key of no path and no build: the module's sha256, the
  architecture, and Wasmtime's `precompile_compatibility_hash` (its version, target, flags,
  CPU features and tunables), in the store's lasting namespace (`insert_lasting`), so it rides
  the sealed packs to R2 as the rest. A copy beside the module (`<module>.<key>.cwasm`) is
  mmapped with `Module::deserialize_file`. A mismatch compiles again; Wasmtime refuses code of
  another configuration.
- The module is compiled where it is built (`compile_time.rs`, `own_macro.rs`), beside the
  other units, with Cranelift's parallel compilation on (the `parallel-compilation` feature).
- The module carries its list of macros in a custom section (`blaze_proc_macros`), so the list
  is written beside it at once and the compile runs on a thread of its own; the first
  expansion of its macros waits for it.
- A list beside the module, stamped with the module's and its `.made-of`'s length, mtime and
  inode, lets the load read no module at all.
