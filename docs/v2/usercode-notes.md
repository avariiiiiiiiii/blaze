---
status: plan
summary: Design notes of blaze v2's usercode lane: the sandbox's record and report, how a run's result is keyed and found, cargo's rule for what can't be traced, the warm pool, and how the pieces meet.
date: 2026-10-29
---
# blaze v2 usercode: design notes

Notes of the `usercode` lane on [usercode.md](usercode.md), as of 2026-10-29. Code:
`crates/blaze-usercode/` (the lane's own), the sandbox in `crates/blaze-cargo/src/sandbox*`,
build scripts in `crates/blaze-build-script/`, proc macros in `crates/blaze-expand/`,
`crates/proc-macro-api/`, `crates/proc-macro-srv/`, `crates/blaze-macro-wasm/`,
`crates/blaze-wasi/`.

## The sandbox, from a driver's side

A driver runs a spawn whose `Sandbox` is on through a `blaze_core::Confine`;
`blaze_usercode::confine::Launcher` is the one blaze gives it. It writes a
`confine::Spec` (JSON, unknown fields refused) into the run's scratch directory and has the
driver start `blaze sandbox --v2 SPEC` in place of the program. The launcher makes the
platform's sandbox (namespaces, overlays and seccomp on Linux; Seatbelt and an interposed
library on macOS; an AppContainer in a job object on Windows), runs the program with exactly
the spec's environment, and leaves a report (`trace::Report`, `trace::encode`): that the
sandbox could not be made (nothing ran), or that the program ran and, when it traces, what it
read. `Confine::traced` reads the report back; a report saying the sandbox was not made fails
the spawn, never a run outside one.

## The record

As the program runs, the sandbox appends one `trace::Line` per thing it sees to the run's live
record, before the call it notes goes on: `R` read or run, `L` looked at, `D` listed, `E` a
variable read by name (`*`: the whole environment, for a process whose reads by name can't be
seen or that walked `environ`), `T` untraceable (`clock`, `random`, `network`, `descriptor`),
`W` and `N` refused (no inputs: the refusal is the same each run), `X` the record is not whole.
Each line has a scope, the name the sandbox's marker gave the thread or process (a test, an
expansion), empty for every scope's. A warm process (a proc-macro server, a fork server) is
read after each answer, by scope.

`trace::assemble` turns lines into a `blaze_core::Traced`: sorted, each once, each file hashed
as it is when assembled (BLAKE3 of its bytes; `trace::DIRECTORY` for a directory;
`inputs::SPECIAL` for a pipe, socket or device, which is never read), each listing as the hash
of its sorted names, each variable with its value. `X` is untraceable as `descriptor`. A run
that read the whole environment has every variable and one more entry, `trace::ALL_ENV_NAME`,
whose value is the hash of the names, so a variable added later is one it would have seen.

## Keys

A run's `RunKey` is its code (the content ID of the binary or module) and what it was given
(`Given`: kind, arguments, the variables blaze sets, stdin, named parts such as a macro's input
tokens). Its result is kept under `RunKey::traced(traced)`: the key and every input read with
its value.

To find a result before running, the store keeps the run key's **shapes** (`index::Shape`):
which paths, listings and variables a run read, without values, in a chain of slots under the
run key. Finding is, for each shape, the values of its inputs now (`Shape::values`) and one get
of the key they make. A run key has one shape per distinct set of reads, usually one, so finding
does not slow down as results pile up. The store never replaces a value, so the chain grows a
slot at a time; a slot another daemon filled with another shape is skipped.

What a run reads in its own writable `OUT_DIR` (given empty) it wrote itself and is no input.
What its given key has already (a variable blaze set) is not traced again. The jobserver's
variables (`script::RUN_ONLY`) are of one run alone, as in cargo's fingerprint.

Where inputs are hashed now: `inputs::ViewInputs` reads a view's files through the query
database's `Cx`, so the query that checks a run depends on exactly what the run read, and the
machine's files (toolchain, the C compiler and headers: #1480) through `inputs::Disk`, cached
by what `stat` says. A file that is there and can't be read is an error naming it.

## What can't be traced

A run that read the clock, randomness, the network or a descriptor it was given keeps cargo's
rule (`rule::widen`), added to what it was seen to read: its `rerun-if-changed` paths (a
directory meaning everything below it: a `trace::TREE` entry), or its whole package without
hidden directories and `target` (a `trace::PACKAGE` entry), and its `rerun-if-env-changed`
variables. It is never kept for longer than either rule would keep it.

## Runs

`run::Run` is a run that ends (a build script): from the store with its `OUT_DIR` written back,
or a spawn in a sandbox that traces, its `OUT_DIR` given empty and read back with every path
checked to stay inside (a link out of it is an error), kept when it succeeded. `run::Mode::Verify`
runs anyway and fails naming the run when the result differs from the one kept.

`script::prepare` makes a build script's run from `blaze_build_script::Unit`: a variable the
script sees with the value it had where the build started is passed through and traced, any
other is given (#805); either way the value it saw is in its key.

## The warm pool

`pool::Pool` keeps sandboxed processes that serve many requests, found by `pool::WarmKey` (code,
program, arguments, directory, sandbox, the variables blaze sets; never one passed through), each
talking over a Unix socket the pool listens on (`pool::SOCKET`), its live record in a directory
it can't write (`pool::Pool::live`), so what its sandbox says it read is the sandbox's alone.
