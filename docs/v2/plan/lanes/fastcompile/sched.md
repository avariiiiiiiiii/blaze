---
status: plan
summary: blaze v2 lane fastcompile, ideas 11 and 12 (scheduling from history; blaze built with PGO and BOLT): crates/blaze-history, its fixed-size per-key records in a seqlock table that readers never lock, persisted in the daemon image; the `Predict` hook the plane asks; tools/pgo, which trains, builds, BOLTs, gates and publishes blaze through blaze; and the state of the art each matches or beats.
date: 2026-10-29
---
# fastcompile: scheduling from history and blaze built with PGO and BOLT

Ideas 11 and 12 of [compile-time.md](../../design/compile-time.md). Owned here:
`crates/blaze-history/`, `tools/pgo/` and this file. The hooks in other lanes are requests in
`v2/requests/fastcompile.md` (`fastcompile-sched 1` to `5`).

## 11. Scheduling from history (`crates/blaze-history`)

The plane orders ready nodes by priority, a node's cost plus the largest priority among its
dependents (the critical path), and admits a node only when its memory fits the budget. Today
every cost is 1 and memory is a per-kind running estimate (`blaze_graph::admission::Estimates`).
With history, both come from what the same task did before.

### What is kept

- **Per key**: a `Record` of one (task kind, `QueryKey::task_key`) pair, 64 bytes: the kind, the
  first 128 bits of the key's BLAKE3 hash, how often it was seen, when it was last seen (a clock
  the writer advances per observation), its peak bytes and its last four run times in
  microseconds. The predicted duration is the median of the runs kept (one outlier, a cold
  page cache or a descheduled thread, does not move it); the predicted peak rises at once to a
  higher observation and halves towards a lower one, so admission errs towards fitting.
- **Per kind**: `Times`, an integer exponential mean (weight 1/8) of every run of the kind, and
  the existing `Estimates` for its peak. A key never seen is predicted from its kind; a kind
  never seen from the configured unseen values (1 ms, 64 MiB).

The kind estimate for an unseen key is **not a fallback** in the sense of AGENTS.md rule 3: a
prediction only orders and admits work, it never changes what is built or what it outputs, and
a key that has no history has no better input. A wrong guess costs time, never correctness, and
the first run of the key replaces it.

### The table

`table::Writer` owns a power-of-two array of 64-byte slots in buckets of four (a set-associative
cache, like a CPU's): a key goes to bucket `(key[0] ^ kind * φ) & mask` and to the way that
holds it, else a free way, else the way seen longest ago (LRU within the bucket, by the writer's
clock). The default 65,536 slots are 4 MiB; memory never grows, keys are evicted by age, kinds
are kept (their table has one entry per kind, a few hundred).

Readers (`table::Reader`, cloneable, `Send + Sync`) never lock: each slot is a seqlock over
eight `AtomicU64`s (the sequence and seven packed words), written by the one writer (the
`Writer` is `Send` and not `Sync`, so the type system keeps it single) and read with an
acquire load of the sequence, relaxed loads of the words, an acquire fence and a recheck,
retrying while a write is in flight. All of it is safe Rust: the crate is
`#![forbid(unsafe_code)]`. A read is one cache line plus at most three more for the bucket.

### Persistence

`History::encode` writes, in a fixed order, `blzhis01`, the slot count, clock and eviction
count, the encoded `Estimates`, the kinds sorted, and the occupied slots in slot order: the same
history makes the same bytes. `decode` checks every field (magic, lengths, sort orders, that
each record sits in its bucket, distinct stamps, sound records) and refuses bad bytes with a
`DecodeError` that names what is wrong; a different slot count re-places every record in stamp
order, so a smaller table keeps the newest of each bucket. The bytes take the place of the
daemon's `state/estimates` file beside its image (`state/history`), written when it writes
the image and at stop (request 2). Bytes of an older version give an empty history with a
note; any other bad history stops the daemon naming the file.

### Feeding the plane

The narrowest hook (request 1, written here since core is done): `blaze_graph::admission::
Prediction { cost: Option<u64>, peak: u64 }` with `apply(&mut Spec)`, and a trait `Predict:
Send` with `predict(kind, key)`, `observe(kind, key, ran_ns, peak)` and `encode()`.
`Estimates` implements it (cost `None`, today's behaviour), so a plane without history is
unchanged. `History` implements it with every cost in
nanoseconds, a seen key's from its record and an unseen key's from its kind, so costs on one
plane are of one unit. The cargo daemon's plane and the exec scheduler hold a `Box<dyn
Predict>`, apply each node's prediction to its `Spec` (memory is the larger of the spec's and
the prediction's, cost the prediction's when it has one), and observe each finished node's own
run time and peak (request 2).

### Tests (`crates/blaze-history/src/tests.rs`)

The median and peak rules; a model of per-bucket LRU against the table over 100 random
streams, the reader seeing what the writer sees; encode/decode roundtrips, resizing both ways,
every `DecodeError`; byte-for-byte determinism; three readers racing 200,000 writes and never
seeing a torn record; the plane on a known graph (a single 2,000 µs node runs before a chain of 100 µs then 900 µs,
1,000 µs in all, where flat costs run in insertion order, the chain's head first); 200 random
graphs whose run order on one core matches a reference critical-path simulation; admission
holding two 80-byte keys apart and letting two 30-byte ones run together under a 100-byte
budget.

### State of the art

- **Seqlock-read tables** (the Linux kernel's `seqcount`, Folly's and crossbeam's seqlocks): the
  same protocol, in safe Rust over atomics, with one writer by type.
- **Set-associative LRU**: CPU caches and Caffeine/Ristretto-style bounded maps; ours is a fixed
  64-byte record per way, so memory is known up front and a lookup touches one bucket.
- **Scheduling from history**: Bazel and Buck2 record action durations and report the critical
  path after a build; cargo orders units by their dependents in the unit graph, with no timing;
  rustc's `-Zself-profile` records query times but nothing schedules by them. blaze schedules
  every query node, not every crate, by the critical path of its predicted times, and admits it
  by its predicted peak memory.
- **Not beaten yet**: remote execution's scheduling across machines (Bazel, Buck2); blaze's
  plane is one machine's.

## 12. blaze built with PGO and BOLT (`tools/pgo`)

`tools/pgo/pgo.py` takes one typed input (AGENTS.md rule 17, frozen dataclasses through
`typed_input`, unknown fields refused) of kind `build` or `publish`. `build`, in one ix VM per
target in the release train:

1. fetches the pinned LLVM (`crates/hir-ty-codegen/llvm.json`) through
   `.github/scripts/llvm.py`, which must also ship `llvm-profdata`, `llvm-bolt`, `merge-fdata`
   and `perf2bolt` and the profile runtime (request 5);
2. builds blaze (`-p blaze-engine --profile dist --features llvm`) with blaze, instrumented
   with `-Cprofile-generate` through `CARGO_ENCODED_RUSTFLAGS` (request 2 makes blaze read the
   flag, requests 3 and 4 make the LLVM backend and the linker honour it);
3. runs that blaze's daemon over `tools/pgo/training.json`: blaze itself at HEAD, ripgrep 15.1.0
   and nushell 0.108.0 at the commits `harness/bench.py` pins, and every crate of
   `harness/v2/corpus.json` (each `.crate` checked against the sparse index's sha256), with cold
   builds, no-op builds, edits (`const _: u64 = n;` appended, which changes the crate's code and
   fires no lint) and tests; every turn must succeed;
4. stops the daemon, waits for every process of the instrumented binaries to exit (by `pgrep`
   on their directory, never a signal to another lane's process), merges the raw profiles and
   refuses one under 10^9 counts;
5. builds blaze again with `-Cprofile-use` (and `--emit-relocs` on ELF, which BOLT needs);
6. on ELF targets runs BOLT on `blaze-engine`: trained by BOLT's own instrumentation by default
   (an ix VM has no LBR), or by `perf record -j any,u` and `perf2bolt` where the CPU has branch
   stacks, then `llvm-bolt -reorder-blocks=ext-tsp -reorder-functions=cdsort -split-functions
   -split-all-cold -split-eh -icf=1`. BOLT rewrites ELF only: asking for BOLT on Mach-O or PE is
   refused, and so is an ELF target that asks for none, so no Linux release ships without it;
7. runs the gate (by default `harness/ci.py run '{"part": "fast"}'`) with `BLAZE_BIN` set to
   the optimised engine;
8. writes the profile, the BOLT profile and a deterministic `tar.xz` of the binaries into `out`,
   each named by its sha256, and `<target>.json`, its entry for `tools/pgo/profiles.json`.

`publish` checks each named file against its sha256, uploads them to the release the pins name
(`pgo-profiles`) and writes the entries; a new toolchain drops every other target's profile. An
entry of the old cargo-trained pipeline reads as `builder: "cargo"`; a `blaze` entry also names
the LLVM commit that made it (an LLVM reads its own profile format only), its BOLT profile and
its binaries.

Tests (`tools/pgo/test_pgo.py`, `uv run --frozen python -m unittest tools/pgo/test_pgo.py`):
input parsing (defaults, unknown, missing and mistyped fields, BOLT per object format, gates
and counts), the checked-in `training.json` and `profiles.json`, pins roundtrip and merge,
content-hash naming, outputs checked against their files, deterministic packing, the profile
count, the encoded flags, the stage flags, the binaries of a manifest, the corpus index and
checksum, and unpacking that refuses entries outside the crate.

### State of the art

- rustc's own dist builds (`src/tools/opt-dist`): PGO of rustc and LLVM, BOLT of LLVM on Linux,
  trained on rustc-perf's benchmarks. Clang's and Chromium's releases: PGO plus BOLT or
  Propeller. tools/pgo matches that pipeline (instrumented build, training on real workloads,
  merge, optimised build, BOLT, gate) and trains on the long-running daemon doing what users do:
  edits and no-op rebuilds as well as cold builds, which is where blaze's time is.
- **Beyond them**: every artifact is named by content and pinned with the LLVM that read it,
  so a release is reproducible from its pins, and the pipeline never builds blaze with cargo or
  rustc (AGENTS.md rules 1 and 2).
- **Not done**: Propeller (needs LBR or a newer LLVM's Propeller tooling on ARM), BOLT on
  macOS (no Mach-O BOLT) and Windows; context-sensitive PGO (`-fcs-profile-generate`) is a
  later step once plain PGO is measured.

## Requests (in `v2/requests/fastcompile.md`)

1. core: `Prediction`, `Predict` in `blaze_graph::admission`, `impl Predict for Estimates`
   (written here, with a test); left for core: exec's scheduler holds a `Box<dyn Predict>` and
   observes each job.
2. cargo: the daemon's plane takes a `Box<dyn Predict>`, observes each node's run, keeps node
   keys; the daemon persists `blaze_history::History` in `state/history`; `-Cprofile-generate`
   and `-Cprofile-use` read (an error until wired, never ignored) and their profile hashed into
   codegen settings.
3. codegen: `Spec.pgo` hashed into `Settings`; the LLVM backend's instrumentation and use passes;
   Cranelift with PGO an error.
4. link: the pinned LLVM's profile runtime linked into instrumented binaries; `--emit-relocs`
   honoured in ELF output.
5. coordinator: `llvm.py` ships the profile and BOLT tools; a release-train job per target runs
   `tools/pgo/pgo.py`; `flake.nix` and `build_release.py` take blaze-built pins; ruff, pyright
   and the scripts job cover `tools/pgo`.
