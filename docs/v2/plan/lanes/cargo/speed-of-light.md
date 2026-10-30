---
status: plan
summary: lane cargo's speed of light: the bound each part of the workspace load, std's graph, the daemon and the CLI has from its work and the hardware, the arithmetic, what the code costs as written, and what closes each gap.
date: 2026-10-29
---
# Lane cargo: speed of light

The bar is "Within 10% of the theoretical maximum, everywhere" in
[principles.md](../../goals/principles.md), and the whole-build targets in
[targets.md](../../goals/targets.md): a warm no-op `blaze build` under 5 ms, and an edit of one
function to a result in 10 to 30 ms. Nothing here was measured: the bounds are arithmetic from
the work and the hardware, and the costs as written come from reading the code. `proof` measures
them once things compile (the last section).

Hardware figures: one core of the target machines (Graviton 5, EPYC Turin) at about 3.5 GHz,
branchy code at about 2 instructions per cycle, a hash probe that hits L2 about 30 ns, memory
copied at about 10 GB/s on one core, BLAKE3 at about 3 GB/s on one core with SIMD. Linux system
calls on cached data: `stat` about 0.5 to 1 µs, open + read + close of a small file 2 to 4 µs, a
Unix socket round trip 10 to 20 µs, an inotify event delivered to a waiting reader in tens of µs,
`exec` of a small static binary to `main` about 0.3 to 0.5 ms. macOS takes 2 to 3 times as long
for each of those, and FSEvents delivers in milliseconds (below).

Sizes used in the examples: a workspace of about 200 packages (U about 250 units for a dev
build, M about 200 mounts, one per registry package), ripgrep (about 80 units), and std's
`library/` as the pinned toolchain ships it: 3,724 `.rs` files, 56 MB of Rust, 73 MB with every
file.

## The workspace load: `blaze-units` (cargo-sol)

The load turns a checkout's files and the request's config into `Units`: every unit's crate,
dependencies, cfgs, env, flags and outputs (`load.rs`, `load/`). It is a set of queries on
blaze-query's database (`Invocation`, `Asking`, `Mounts`, `LoadFile`, `LoadDir`, `ConfigChain`,
`ManifestQuery`, `WorkspaceQuery`, `Plan`), so its warm cost is the database's: a memo hit in the
same view is one probe, and in a new view each memo is verified down to its inputs' fingerprints.

### Files and mounts (`load/files.rs`)

Work: a path the load reads names a mount (the workspace, a registry package's directory, std's
`library/`, a path dependency outside the checkout) or no mount; the file is then read through
the view's tree.

- Bound: a binary search over the M mounts by path, about log2(200) = 8 comparisons of a short
  prefix, under 100 ns, with no allocation; then the tree lookup (one probe per path part).
- Before: the mount table was a CBOR value decoded whole on every read: M strings allocated
  and freed per file read, about 20 to 40 µs per read at M = 200, and a long-lived daemon reaches
  2,000+ mounts.
- Now (first cargo-sol agent): `MountIndex`, fixed-width little-endian records (head 12 bytes,
  a mount 56, an outside directory 16, a known file 8) and one text area, kept by `Persist`
  as-is and searched in place by `MountRef`. A lookup allocates nothing (test
  `a_lookup_allocates_nothing`). This is the known fastest form for a sorted read-only
  table, the one rustc's and the VFS's own interned tables use. Within the bound.

### Manifests and config (`load/manifest.rs`, `load/config.rs`)

Work: each `Cargo.toml` and `.cargo/config.toml` parsed once into blaze-manifest's form;
`ManifestQuery` keeps a manifest as canonical TOML, so a change of whitespace or comments is
seen as no change (an early cutoff for everything downstream).

- Bound: one parse of each file's bytes. A TOML parser of the simd-json kind does well over
  100 MB/s; toml_edit, which blaze-manifest uses for cargo's exact errors, about 50 MB/s. 200
  manifests of about 3 KB are 600 KB: 12 ms with toml_edit, about 4 ms at 150 MB/s. This is the
  cold load only: warm, a manifest is a memo, and an edit of one `Cargo.toml` parses that one.
- As written: canonical TOML was parse, write, parse again and compare (three passes). Now
  (cargo-sol) a file that is canonical already, the usual case for a manifest written by
  `cargo new` or `cargo add`, stops after writing (two passes). A fresh manifest still costs
  parse + write + parse in `ManifestQuery`, then blaze-manifest parses the canonical text again:
  about 3 passes where one would do, so a cold load of 200 manifests takes about 35 ms where it
  should take 12.
- What closes it: `ManifestQuery` keeps blaze-manifest's parsed `TomlManifest` (its own
  `Persist`) as the value with the canonical bytes as the fingerprint, and the workspace reads
  that value instead of parsing again. That needs blaze-manifest's types to be persistable,
  which is request 1 (the manifest crates are not lane cargo's). The config chain is a few
  files up the directory tree, about 5 `stat`s and one parse each, at the bound already.

### Resolve (`load/resolve.rs`)

Work: with a lock file, check that every locked package still matches its manifest and read
its manifest; without one, cargo's resolver over the index.

- Bound with a lock: O(P + dependency edges). For P = 200 and about 10 edges each, about 2,000
  hash probes plus each package's manifest read once (above): under 0.5 ms beyond the parses.
- Before: the load's step ran once per missing input. The step (`Workspace::new`, resolve,
  units, plans) runs inside `run_over`, which gives blaze-manifest a snapshot of the files read
  so far (its `Files` is an `Rc<dyn Files>` with no lifetime, so it can't read through the
  database). A file outside the snapshot stops the step, the file is read, and the step runs
  again. `Cargo.lock` was missed in the first run, so the step resolved with no lock. Then
  `locate_all` stopped at the first registry manifest not read yet, once per package: 2 + P
  runs, each doing all the work up to that package. That is O(P²) work: at P = 200, about 200
  runs of a workspace load and partial resolve, a second or more on a cold load.
- Now (cargo-sol): `run_over` hands its step a `Reads` handle that reads a file through the
  database into the snapshot during the step. `resolved` reads `Cargo.lock` before parsing it,
  and `locate_all` reads every located package's manifest before parsing any. A locked workspace
  resolves in one run (test `a_locked_workspace_resolves_in_one_run`). A path dependency outside
  the workspace still costs one run per level of path dependencies, which is rare and bounded by
  their depth.
- What removes the reruns entirely: blaze-manifest's `Files` taking a lifetime
  (`Rc<dyn Files + 'a>`) so the step reads through the database on demand, and `Files::read`
  returning a borrowed slice instead of a new `Vec` (each read copies the file today). Both
  are requests to request 1's owner.
- The fastest known technique is cargo's with a lock: the lock is taken as the answer and only
  checked against the manifests. The design matches it.

### The unit graph and plans (`graph.rs`, `load/units.rs`, `load/plan.rs`)

Work: from the resolve, one unit per (package, target, profile, platform) with its
dependencies, then each unit's plan: env (about 25 variables), cfgs, flags, outputs.

- Bound: O(U) with small constants: a unit's env and flags are about 40 short strings, about
  2 to 5 µs each with their allocations, so 0.5 to 1.3 ms at U = 250; the `Plan` value encoded
  once, about 3 KB per unit (750 KB at U = 250, about 1.5 ms at 500 MB/s).
- As written: at the bound. `plan.rs` keeps the roots in an `FxHashSet` and walks the graph
  once. The cost that matters is the reader's: the plan is a `Cbor<Planned>` decoded whole by
  each job that reads it (the daemon, below), about 2 to 3 ms at U = 250 with an allocation
  per string. The bound for a reader that changed nothing is no decode at all.

### `blaze tree` (`blaze-tree`)

Work: cargo tree's graph over the resolve, and its text.

- Bound: O(packages + edges + output bytes); a repeat is one memo hit, since the printed text is
  the query's value.
- As written: a port of cargo's `tree` graph (node clones, `BTreeSet`s), O(P + E) with
  allocation per node, well under 1 ms at P = 200. Within 10% of anything a user can notice. It
  is not on a target path and was left as it is.

## The standard library's graph and target table (`stdlib.rs`, `stdlib/`)

Work: std's own workspace (`library/`) loaded as a foreign workspace (`Foreign`), its units
(`StdUnits`) for the target, and the target table: a target's cfgs, as rustc's
`--print cfg` gives them.

- Bound: std's units depend on the toolchain, the target and the profile, not the checkout: a
  memo keyed by those, verified by the library root's content id. Warm, that is one probe.
  The target table is a lookup, about 1 µs per `target_cfg`.
- As written: the target table is a 423 KB JSON parsed on first use in each process, about
  1 ms once in the daemon's life (well inside its 1 s start). A table built into the binary
  would make it zero and is not worth the build step until `proof` says start matters at the
  ms level. `StdUnits` decodes `Foreign`'s CBOR and encodes its own: one extra copy of about
  25 units, tens of µs, once per toolchain and target. Both are within the bound for their
  paths.

## The daemon: `blaze-cargo/src/serve*` and `load-cargo` (cargo-sync's; gaps for cargo-sync)

The daemon is a sans-IO loop (`serve/daemon.rs`, `serve/job.rs`) with plane tasks for the
work. This section gives the bound and what stands in the way. The code changes are cargo-sync's
(requests in `v2/requests/cargo.md`, from cargo-sol).

### Heads and cookies (`serve/heads.rs`)

Work: a request must see every file change made before it was sent. The daemon writes a cookie
file and waits until its watcher reports it (watchman's way), so all earlier events are in.

- Bound: one file create and remove (about 10 µs on Linux) and one event round through the
  watcher: on Linux with inotify, tens of µs, so about 50 µs. The fastest known technique is the
  cookie; a check of every file's mtime is O(files) and much slower.
- On macOS, FSEvents delivers through `fseventsd` in milliseconds even with latency 0 and
  `NoDefer`. The cookie has to come through the same stream as the other events to prove they
  arrived, so a kqueue watch on the cookie alone would prove nothing. The 5 ms target is
  measured on a Linux VM. On macOS the floor is FSEvents' own delivery time, which `proof`
  should measure and report as its own line. If that is over a few ms, the fix is lane vfs's:
  keep a per-volume FSEvents stream already running so a cookie waits for one delivery, never
  for a stream to start.

### A job's path from arrival to `Built` (`serve/job.rs`, `serve/unit.rs`, `load-cargo/src/v2.rs`)

Work on a warm no-op: decode the request, wait for the cookie, see that the view's root and the
config are those of the last job, see that every unit is fresh and every output is still in
place, and answer.

- Bound: the request's decode and the answer's encode (a few KB, µs), the cookie (50 µs), one
  compare of (view root, config fingerprint) against the last job's, and one check per placed
  output that it was not touched. The watcher already sees `target/` (the cookies are written
  there), so an untouched output needs no system call at all. So about 0.1 ms in the
  daemon, and O(changed paths) when something changed.
- As written, each job does all of this:
  - D1, a new query view: `open_with` makes a new `ViewState` with no results, so `Plan` is
    verified in depth down to every input's fingerprint (about 2,000 edges at U = 250, each a
    probe and for inputs a tree path lookup and a BLAKE3 of id and mode, about 0.5 ms). Then the
    `Units` value is decoded whole (2 to 3 ms at U = 250, above).
  - D2, a new front end: `lower` builds a new `base_db::Db` (`Db::new()`) and puts every file of
    every package into it: std's `library/` alone is 3,724 files and 73 MB. Copying 73 MB takes
    about 7 ms, checking it is UTF-8 2 to 4 ms more, and making 4,000+ file inputs a few ms:
    10 to 20 ms per job, against a 5 ms budget for the whole no-op. The front end's memos are
    lost with the old `Db`, so the first query of every crate after it starts cold. For an
    edit of one function that is the whole cost: rebuilding std's def maps and item trees is
    hundreds of ms, against 10 to 30 ms for the edit.
  - D3, every output placed again: `store.place` runs for every output of every unit on every
    job, with no record of what was placed. On APFS a clone is about 30 to 60 µs, so U = 250
    with about 2 outputs each is 15 to 30 ms. On ext4 with no reflink, each placement writes the
    whole file: a debug `rg` is tens of MB, so 10 ms and more for one binary.
  - The parts at the bound: identities (`identity_of` hashes each crate's content id, its
    source root's content id and its dependencies' identities: a few µs per crate), the plane
    nodes (below) and the fresh checks (one store probe and the native-input checks of each
    record's header, a few µs per unit).
  - Total as written, ripgrep: roughly 1 + 12 + 5 ms in the daemon for a no-op (verify and
    decode, lower, placements), 4 times the 5 ms target before the CLI's own start (below).
- What closes it (requests to cargo-sync):
  - D1: keep, per head, the last job's `(view root, config fingerprint) -> (ViewId, Arc<Units>)`.
    The same pair reuses the `ViewId`, so `Plan` is one probe in that view's results, and the
    `Units` already decoded. A changed root opens a new view; with lane db's carry-forward
    (`open_after`, below) that view keeps every result whose inputs did not change.
  - D2: keep `Lowered` across jobs, keyed by `Units`' fingerprint, and apply a new view's
    change with `Db::set_view_root` (`crates/base-db/src/db.rs`): O(changed files), so an
    edit of one function sets one file's text. `Lowered` is rebuilt only when `Units` changes
    (a manifest, lock or config edit), which is the cold path.
  - D3: a placement record per output path (the store key placed, and the file's inode, size
    and mtime after placing), kept by the daemon. When a unit's key equals the record and no
    watcher event under `target/` touched the path, skip the placement. A changed output is
    placed as now. The store's `place` should take the same skip for a path it placed and that is
    unchanged. A removed or edited output is seen by its watcher event, so this never answers
    with a stale file.
  - For lane db: `Database::open_after(previous, view)` carrying the previous view's results
    forward through the tree diff of the two roots, so a new view costs O(changed paths)
    instead of a deep verify.

### Fetch (`serve/fetch.rs`, `serve/fetch/`)

Work on a cold build: download each `.crate` (and index files for a lock-less resolve), check
its sha256, unpack it in memory with cargo's checks, and put it into the VFS.

- Bound: the network: bytes over bandwidth plus round trips. CPU per MB is small next to it:
  sha256 with SHA-NI about 2 GB/s, gzip inflate 300 MB/s to 1 GB/s (zlib-ng or libdeflate),
  BLAKE3 for the VFS 3 GB/s. A cold fetch of 200 crates (about 30 MB) is about 1 s of network at
  250 Mb/s and about 150 ms of CPU spread over IO threads.
- As written: all fetches of a load round go out at once, one plane task each (`Planned::Needs`
  in `job.rs`), so the round trips overlap. With the prefill above, a locked workspace asks for
  every registry package in its first round, so a cold fetch is one round of downloads, not one
  per package. blaze-registry's client is HTTP/1.1 only (`hyper` with `http1`), so each
  concurrent download opens its own TLS connection (2 round trips more per connection). Cargo
  multiplexes over one HTTP/2 connection, which is the fastest known technique for many small
  files from one host. HTTP/2 in `blaze-registry` is a request to its owner. It matters only
  for cold builds, which have no target in targets.md, so the gap is noted and not chased.
  flate2's inflate (miniz_oxide) at about 300 MB/s is 0.1 s of CPU for 30 MB, under the
  network's time.

### The unit plane nodes (`serve/plane.rs`, `serve/unit.rs`)

Work: one node per unit in the plane's graph, posted as its dependencies finish.

- Bound: O(U) with a node add and an edge count each, about 0.3 µs a unit: under 0.1 ms at
  U = 250.
- As written: at the bound (`graph::Graph::new` over `Units` and one node per unit). On a no-op
  with D1 and D2 in place these nodes are the largest part of the daemon's work, and still
  well under the budget.

### The image at start (`serve/daemon.rs`, `serve/unit.rs`)

Work: open the database's image (mapped, not read), recover the VFS journal, open the store,
and have std's `library/` in the VFS.

- Bound: mapping is O(1) and the journal replay is O(entries since the last checkpoint). The
  library is content-addressed: if the journal records the library's root id, start reads no
  library file, and a check of the toolchain directory's fingerprint is enough.
- As written: `Steps::library` reads `library/` from disk once per daemon process: 3,724+
  files, about 7,500 system calls (15 to 30 ms) and 73 MB hashed with BLAKE3 (about 25 ms), so
  about 50 ms. That is inside the 1 s start target and paid once, so it is noted for cargo-sync
  (keep the library's root id in the image and read the files only when the toolchain's
  directory changed) and not a gap against any target.

## The CLI (`blaze-cargo/src/cli.rs`, `cli/`) (cargo-sol)

The CLI is a client of the daemon's API: it parses cargo's arguments, finds the workspace,
sends one request, prints the answer as cargo prints it, and runs tests.

### Argument parsing and the request (`cli/args/`, `cli/place.rs`, `cli/daemon.rs`)

Work: cargo's arguments into the request; the workspace root found by walking up for
`Cargo.toml`; the process's environment and config passed through.

- Bound: parsing a few arguments is µs. Finding the root is about 6 to 8 `stat`s (under 10 µs),
  reading the environment and sorting it a few µs, encoding the request (a few KB of CBOR)
  µs, and one connect plus one round trip on a Unix socket (20 to 40 µs). About 50 µs.
- As written: at the bound for the CLI's own work.
- What is not at the bound is the process: the CLI runs as the same `blaze` executable as the
  daemon. That executable links the whole front end, the codegen backends, and LLVM statically
  (`inkwell` with `llvm23-1-prefer-static`). Starting it costs the dynamic loader's relocations
  of a binary of 100+ MB, LLVM's static constructors (each `cl::opt` registers itself before
  `main`) and the page faults of their data: likely several ms on Linux, more on macOS,
  against a 5 ms budget for the whole no-op. A small static client binary (`blaze` as about a
  1 MB client of the API, the daemon a separate executable it starts) starts in 0.3 to 0.5 ms.
  This is how watchman, Bazel and buck2 do it. Which executable is `blaze` is the entry's
  (coordinator's), so it is a request to the coordinator: build `blaze-cargo`'s `cli` into a
  client binary with none of the front end, codegen or LLVM linked, and have it start the daemon
  binary.

### Printing (`cli/print/`, `cli/say/`, `cli/json/`)

Work: the answer's lines (cargo's `Compiling`, `Finished`, diagnostics, or JSON messages)
written to stdout and stderr.

- Bound: one write per stream per batch; for a no-op, about 2 lines and 1 to 2 writes, a few µs.
- As written: each line is formatted into one reused `String` and written with one `write_all`
  (stderr has no buffer, so one system call a line, about 1 µs). A no-op writes 1 or 2 lines; a
  build writes one `Compiling` line per unit, each far cheaper than the unit's own work. At the
  bound for every path that has a target.

### Running tests (`cli/test/`, `cli/run/`)

Work: run each test binary as cargo does, one after another, with cargo's arguments, and print
their output and the summary.

- Bound: one spawn per test binary (0.3 to 0.5 ms each on Linux for a small static binary), plus
  the tests' own time. Running the binaries at once would be faster, but it changes what a
  program sees (the terminal, shared ports and files), so cargo's order is the bound.
- As written: cargo's order, one spawn each, at the bound. Running tests in the daemon's fork
  server is lane usercode's and is outside this section.

## The whole: a warm no-op and an edit of one function

A warm no-op `blaze build` on Linux, the bound:

| part | bound |
| --- | --- |
| CLI start (small client) | 0.3 to 0.5 ms |
| arguments, root, request, socket | about 50 µs |
| cookie | about 50 µs |
| same view, same config, units fresh, outputs untouched | under 0.1 ms |
| answer printed | a few µs |

About 0.6 ms, 8 times under the 5 ms target. As written it is roughly 20 to 40 ms: the CLI's
start in the full binary (several ms), D1 (about 1 to 3 ms), D2 (10 to 20 ms) and D3 (15 to 30
ms on APFS, more on ext4 for large outputs). With D1 to D3 and the client binary, the no-op is
at the bound.

An edit of one function's body, the cargo lane's share:

- Bound: the cookie, a new view root whose diff is one file, the load's results carried forward
  (no load query reads a source file's bytes, so none changes), one `set_file_text` in the kept
  front end, the identities of that crate and its dependents rehashed (a few µs each), and the
  unit plane nodes of those units. Under 0.2 ms, which leaves the 10 to 30 ms to the front end
  and codegen.
- As written: a deep verify of the load in the new view and a decode of `Units` (1 to 3 ms), a
  new front end with every file copied (10 to 20 ms) and cold memos (hundreds of ms for std
  alone), and every output placed again. With D1 to D3 and lane db's carry-forward, it is at
  the bound.

## What `proof` should measure

- A warm no-op `blaze build` of ripgrep and of a 200-package workspace on a Linux VM, split
  into the CLI's start, the socket round trip, the cookie, and the daemon's job from arrival to
  `Built`. The job should be under 0.2 ms once D1 to D3 land; until then, the split shows which
  of D1, D2 and D3 is the most.
- The same on macOS, with FSEvents' cookie delivery as its own line.
- An edit of one function: the daemon's time before the front end's first query, which should
  be under 0.2 ms.
- A cold load of the 200-package workspace with a lock and every file in the VFS: the count of
  the load's step runs (1) and its time, against about 15 ms (parses at toml_edit's speed plus
  resolve and plans).
- The CLI's start alone (`blaze --version`), against 0.5 ms.
