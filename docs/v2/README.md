---
status: plan
summary: blaze v2: the plan. One warm VM per project with branches as views, content-addressed items with fact-level invalidation, per-function work on all cores, a zero-copy store and daemon image, CI on the same VM. One file per lane.
date: 2026-10-29
---
# blaze v2

A plan, as of 2026-10-29. It lands as **one pull request** from an integration branch,
`blaze-v2`, built by parallel lanes against interfaces fixed first (section 12). It goes to main
when the whole gate, the rustc differential harnesses and the v2 performance budgets pass with
v2 as the only path (section 11). Nothing in it is a fallback to v1.


## Files

- [core.md](core.md): the sans-IO core, drivers, the scheduler and memory admission.
- [vfs.md](vfs.md): the Merkle VFS and views.
- [format.md](format.md): data formats: IDs, zero-copy records, flat syntax, side tables.
- [items.md](items.md): minimal invalidation: item hashes and facts.
- [usercode.md](usercode.md): build scripts, proc macros and tests.
- [db.md](db.md): our own query database.
- [store.md](store.md): the store.
- [codegen.md](codegen.md): per-function codegen.
- [link.md](link.md): the parallel in-place linker and live patching.
- [ci.md](ci.md): the agent API and the VM as CI.
- [proof.md](proof.md): proving it: oracle, differential corpus, determinism, budgets.
- [crates.md](crates.md): every v2 crate and which lane owns which paths.
- [plane.md](plane.md): notes on the scheduler as first written.

## 1. Targets

Measured on one warm VM (Graviton 5 or EPYC Turin, all cores), and on an M-series Mac for
macOS. "Edit to result" is the time from a save to a running program or a test result.

| | today (main) | v2 target |
|---|---|---|
| body edit, edit to result | ~0.29 s (nushell) | 10-30 ms |
| signature edit | ~0.31 s, 6,269 roots asked | proportional to the items whose facts changed |
| formatting or comment edit | recompiles the file's functions' places | compiles nothing |
| rename or move of an item | recompiles dependents | compiles nothing |
| no-op build | 11 ms (ripgrep) | under 5 ms |
| daemon start, warm | 35 s (nushell); up to 238 s in CI | under 1 s (mmap the image) |
| blaze's own CI, small PR | 30 min+ | under 2 min, the changed code and the tests it reaches |
| cold build of blaze | ~12 min of mostly serial codegen | bounded by the critical path, all cores busy |
| memory | runners OOM; cold ripgrep 2.0 GB live vs cargo's 0.56 GB (#806); nushell 12.5 GB (#734) | never OOM: admission against a budget; peak at most cargo's |
| body edit of a big top crate | ruff 75 s, ty 147 s; cargo 7 s; Astral's fork 0.3 s (#1450) | under 50 ms |
| dev-build code speed | ripgrep's dev build runs 5.8x slower than cargo's debug build (#1404) | at least as fast as cargo's debug build |

Non-goals: changing what Rust means (superset-of-rust holds); running rustc for anything
(no-rustc holds); a fallback when v2 can't do something (no-fallback holds: it stops and says
what and where); an IDE. The only interface is the agent API (CBOR over a socket or HTTP),
shaped for LLMs; the IDE crates (`ide-db`, `hir-ide`, `ide-diagnostics` and the rest) are not
a constraint, and what the compiler needs from them (diagnostics) moves into the compiler.

**Everything is ours.** Any dependency v2 needs to change is in the workspace and changed in
place: salsa (`vendor/salsa`), cranelift, llvm-sys, `rustc_type_ir` and the trait solver are
already there; anything else is vendored the moment it's in the way.

**Deterministic**: the same inputs give the same bytes, whatever the scheduling, the core count
or the machine. The invalidation oracle (section 11) depends on it.

**Profile first**: before the targets bind, the coordinator measures where today's 0.27 s body
edit and 35 s start go, and the targets are set from that.


## 2. The machine model

- **One warm VM per project** (or per tenant's projects), all its cores, effectively unlimited
  local disk. One blaze daemon holds one query database and one store.
- **A branch, a PR or a CI job is a view**: a root of the Merkle VFS (a map of every file to its
  content hash). Opening a view is constant time; building it computes only what its inputs
  reach that no other view already computed.
- **Results are keyed by the hashes of their inputs, never by view or revision**, so every view
  shares every equal result. Identical work in flight runs once (single-flight).
- **Forks of the VM only for**: another tenant (isolation), a build bigger than one box (helpers
  for a slice of codegen), and snapshots to resume after a crash or a stop.
- **macOS and Windows** each get their own warm machine running the same daemon.


## 12. How it is built: lanes on one integration branch

Each lane is a cursor-agent (`--yolo`, Opus 5.5 medium) in its own worktree, on a branch off
`blaze-v2`, merging into `blaze-v2` with `--no-ff` when its code builds. The coordinator fixes
the interfaces first (step 0 below) and owns them afterwards: a lane that needs an interface
changed asks the coordinator, who changes it on `blaze-v2` for every lane at once.

**Main is frozen** for the crates v2 rewrites while v2 is built: bug fixes only, each one
merged into `blaze-v2` by the coordinator the same day. Other crates keep landing on main as
usual and are merged in daily.

| lane | owns | depends on |
|---|---|---|
| `v2` (coordinator) | the profile of today, the interfaces, the branch, main merges, the full suite, the final PR | — |
| `v2-core` | sans-IO core, drivers (io_uring, kqueue, simulation), scheduler, admission | interfaces |
| `v2-vfs` | Merkle VFS, views, journal, file watching | core |
| `v2-format` | the zero-copy format and schemas, both kinds of IDs, sharded interners, flat syntax (replacing rowan), SoA HIR, span side tables | interfaces |
| `v2-db` | our own query database from `vendor/salsa`: input-hash keys, fingerprints, views, signature/body split, GC, daemon image, parallel expansion, the rerun explainer | format |
| `v2-items` | item hashes (interface, layout, code, deep), facts, negative-fact indexes, rename and move | db |
| `v2-store` | packs, index, compression, chunking, verification, remote tier | format, core |
| `v2-codegen` | per-function units, instance keys, code dedupe, release inlining edges | items, store |
| `v2-link` | the parallel in-place linker and commit, debug side table, live patching | codegen |
| `v2-usercode` | traced-input sandboxes for build scripts and proc macros, wasm proc macros, warm process pool, test selection | core, store |
| `v2-ci` | the VM as CI: webhook, views per PR, check runs, the train | vfs, db |
| `v2-proof` | the invalidation oracle and fuzzer, the crate corpus differential, performance budgets | all, from day one against v1 |

Order:

0. **Interfaces** (coordinator, first): the effect and completion types, the task and admission
   API, the ID and arena types, the query key and fingerprint types, the item-hash and fact
   types, the store's get/put API. Written as crates with types and doc comments and no logic,
   merged into `blaze-v2` before any lane starts.
1. All lanes in parallel against the interfaces, writing code and its tests, keeping the
   branch building, running no suites.
2. `v2-proof` builds the oracle, the fuzzer, the corpus and the budgets in parallel, and sets
   the bar by running them against v1.
3. Integration, all at once: everything is run together on `blaze-v2`; failures go back to
   the owning lane; v1's paths are deleted and the IDE crates v2 doesn't need are removed; the
   branch is green on the whole gate with no v1 path left.
4. One PR to main through `land.py`; the docs this supersedes (`driver.md`, `INCREMENTAL.md`'s
   plan parts, `remote-cache.md`, `linker.md`) marked or rewritten in the same PR.


**Left out of v2 for now**: #1448 effect-generic I/O, #1444 tests as WASI modules, #868
purity facts, #859-#866 the hermetic build graph for build scripts, #104 mutation testing, #226
the Bazel remote execution API.

## 14. Risks

- **One huge PR**: review and merge conflicts with main. Mitigation: main merged into
  `blaze-v2` daily by the coordinator; the PR's review is the test suite plus the per-lane design
  notes, not a line-by-line read.
- **Facts too fine**: tracking costs more than it saves. Mitigation: the oracle measures both
  under- and over-invalidation; any fact kind can be coarsened without changing correctness.
- **ix VMs that don't place or stay up**: the `sansio` lane couldn't measure on 16 cores. A lane
  without a VM says so in its HANDOFF and stops on measurement, never runs on the Mac or AWS.
- **Front-end churn**: `hir-*` changes (IDs, SoA, fingerprints) touch the most code. They keep
  rustc's semantics; the differential corpus is the check.
- **Testing all at once**: the first full run will fail in many places together, and failures
  that cross lanes take longest. Mitigation: the oracle and corpus exist before integration
  (step 2), so failures come with the fact or crate that broke, and the explainer says why.
- **A chain of lanes** (format, then db, then items, then codegen, then link): downstream lanes
  build against the interfaces and their own stubs, not against finished upstream work.
- **Toolchain bumps get cheaper, not costlier**: with item hashes, a new nightly's std
  invalidates only the std items that changed.

## 15. Agents: what they may run

Lane agents write code and tests and read; they don't run anything expensive. Allowed: reading
files, `rg`, `ls`, `wc`, read-only git (`diff`, `log`, `show`, `status`, `grep`, `ls-tree`),
`gh issue view` and `gh pr view`, and starting child agents. Not allowed: any build, check or test
(`blaze`, `cargo`, `rustc`, clippy, `nix build`, `nix develop`), harnesses, benchmarks, ix VMs,
downloads, long loops, and git commands that change state. The coordinator alone builds and tests,
all at once, in an ix VM.

**Finished code only.** Every lane writes its part end to end: every body, wired into the path a
real build takes, with no `unimplemented!`, `todo!`, stub or deferred piece. A lane isn't done
while any remain in its paths. v2 lands complete or not at all.

## 16. Hardware: use all of it

Every hot loop uses the machine's acceleration, chosen at run time by CPU feature detection.
Every path gives **bit-identical results** (determinism), so the choice is dispatch, not a
fallback. Targets: AMD EPYC Turin (Zen 5: AVX-512 with a full 512-bit datapath, VAES,
VPCLMULQDQ, SHA-NI, 12 CCDs), AWS Graviton 5 (Neoverse V3: SVE2, NEON, SHA3, crypto
extensions, one die), Apple M-series (NEON, crypto extensions, P and E cores).

| work | acceleration | lane |
|---|---|---|
| BLAKE3 of sources, items and blobs | AVX-512 and NEON (BLAKE3's SIMD, many chunks at once); one batched call per many small inputs, not one call each | format, items, store |
| lexing and outline parse | SIMD classification of bytes (simdjson-style: whitespace, identifiers, punctuation, string and comment bounds found 64 bytes at a time) | format |
| interner and memo lookups | SwissTable-style probing with SIMD tag matching; prefetch of the next probe; hashes computed once and carried | format, db |
| content-defined chunking | a SIMD gear hash (FastCDC) | store |
| compression | zstd with long-distance matching and threads for cold packs; dictionaries per kind; LZ4 where decompression speed matters | store |
| checksums of headers and indexes | CRC32C instructions (SSE4.2, ARMv8 CRC) | store |
| relocations and blob copies | parallel per function; large copies with non-temporal stores; reflink or clonefile instead of copies on disk | link, store |
| byte comparison (oracle, dedupe, backend memo) | SIMD compares, hash first | proof, codegen |
| memory | 2 MB huge pages (THP, `MAP_HUGETLB`) for arenas, interners and mapped packs; memory allocated on the NUMA node of the worker that uses it; per-worker arenas | core, format |
| IO | io_uring with SQPOLL, registered buffers and files, linked submissions, `O_DIRECT` for pack writes, multishot accept and recv; NVMe queue depth kept full | core, store |
| threads | pinned per core; work stealing within a CCD or cluster first; P cores for the critical path on Apple | core |
| blaze itself | built with PGO and BOLT per target CPU (`znver5`, `neoverse-v3`, `apple-m4`); hot paths checked for vectorization in the emitted code | coordinator |
| the code blaze emits | target features of the machine it runs on for dev builds (`-C target-cpu=native` semantics when the program runs where it was built), and the profile's choice for release | codegen |

**No GPU on the hot path.** A compiler front end is pointer-chasing and branchy (name
resolution, trait solving, inference, macro expansion), which GPUs run badly, and the
Graviton and EPYC machines have no GPU. GPU compilers exist only for small languages
(Pareas, co-dfns). The only parts that would map well are hashing, chunking, compression and
byte comparison of very large data, and on these machines the CPU's SIMD already runs them
near memory bandwidth. A GPU is reconsidered only if a measurement shows one of those
saturating the CPU.

## 17. Data-oriented everywhere

Struct of arrays is not only for syntax and HIR: every stage's long-lived data is columns of
small integers over `u32` indices, laid out the same in memory and on disk (section 5).

**Encoding rules** (every lane):
- **Tag plus one word**: a node is a `u8` kind in one column and a `u32` data word in another;
  variants that need more put it in a shared `extra: [u32]` array and store its index (Zig's
  encoding). No Rust `enum` with payloads in a hot array.
- **Lists as CSR**: children, arguments, edges and projections are `(start, len)` ranges into
  one flat array per kind, never `Vec<Vec<_>>` or a `Vec` per node.
- **Hot and cold split**: what a pass reads every time sits in the main columns; what it reads
  rarely (spans, docs, attributes, diagnostics data) sits in side tables keyed by the index.
- **Flags as bitsets**: booleans and small sets (type flags such as "has inference variables",
  "has params"; live locals; reached functions) are bit columns, tested 512 at a time with
  AVX-512 or 128 with NEON.
- **Names as symbol IDs**, never `String`; no `Box`, `Arc` or `Option<Box<_>>` in long-lived data.
- **Size budgets**, checked by `const` asserts beside each type: a token 4 bytes, a syntax node
  8, a HIR expression 12, a MIR statement 16, a memo 24 plus 4 per edge, a relocation 16.
- **Batch by kind**: a phase runs over every item of one kind at once (hash every changed
  body in one BLAKE3 batch, lex every file, collect every signature of a crate), and tiny
  per-function tasks are grouped so the scheduler's cost per task stays below the task's work.

**By stage:**

| data | layout | lane |
|---|---|---|
| tokens, syntax, item trees, HIR bodies (exprs, pats, stmts) | tag and data columns, CSR children, spans in a side table | format |
| types | one interned table: kind column, CSR generic arguments, a flags bitset column; a type is a `u32` | db (`vendor/rustc_type_ir`) |
| inference | union-find in flat `parent` and `rank` arrays in the task's arena; obligations in a column queue | db |
| trait solving | goals and results interned as `u32`s; the solver's cache an open-addressed table on them | db (`vendor/rustc_next_trait_solver`) |
| MIR | locals as columns (type, mutability); blocks as ranges into one statement array; statements as tag and data; places as a base plus a CSR projection list | codegen (`hir-ty-mir`) |
| borrow check | facts as columnar relations (Polonius-style); liveness and initialization as dense bitset dataflow per block | db (`hir-ty-borrowck`) |
| query database | memos as columns (key hash, fingerprint, verified-at, value offset); edges as one CSR array per memo table (today salsa's edges are 0.8 GB of nushell's 12.5 GB, #734) | db |
| item hashes and facts | columns per hash kind; facts as `(kind, subject, object)` triples sorted for range scans; negative-fact indexes as sorted arrays | items |
| compiled functions and relocations | code bytes in one buffer per unit; relocations as columns (offset, kind, target), sorted by target so a moved function's callers are one range | codegen, link |
| linker symbols | columns (name, section, offset, size); placement by prefix sums over sizes | link |
| store index | the open-addressed table of section 8, keys and places in separate arrays | store |

`proof-perf` reports bytes per token, node, expression, statement and memo, memory per
thousand lines, and cache misses per edit (`perf stat`), against these budgets.
