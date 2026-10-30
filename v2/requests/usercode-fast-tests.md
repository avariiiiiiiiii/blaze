# Requests from usercode-fast-tests

## usercode-fast-tests 1: usercode: `index::search` for any store (`crates/blaze-usercode/src/index.rs`)

`test::select::find` checks a test's kept pass the slow way: it decodes each shape into a
`Shape` and builds a whole `Traced` of its values (`Shape::values`) to hash its result key,
for every test, skipped or not. `index::search` does the same check in place (`keyed`: the
shape read borrowed, each input hashed into the key as it is asked, no `Traced`), but it is
private, and `find`/`find_held` take a `Reader` or a `Store`, where selection has a
`&dyn Passes`.

What: make `search` `pub(crate)`, as it is:

```rust
pub(crate) fn search<'s, I: Inputs>(
    get: impl Fn(&Key) -> Result<Option<&'s [u8]>, Corrupt>,
    run: &RunKey,
    inputs: &mut I,
) -> Result<Found<'s>, FindError<I::Error>>
```

Then `test/select.rs`'s `find` becomes `index::search(|key| store.get(key), run, inputs)`,
with `Found::Hit { value, .. }` a pass when `value == PASS` (otherwise `NotOurs`),
`FindError::NotAShape` as `NotOurs`, and one difference to note: where `Shape::values`
hashes a directory that is no longer one, `keyed` says no result can match (`None`), which
`search` reads as a miss of that shape. A miss either way: the test runs.

Why: the speed pass (`docs/v2/plan/lanes/usercode/tests.md`, Selection): a skipped test
should cost its gets and its hashes, not a `Traced` built and dropped.

> applied (usercode): `index::search` is `pub(crate)`; `test::select::find` calls it as written above.

> forwarded to usercode (told by tmux).

## usercode-fast-tests 2: usercode (optional): `trace::assemble` over borrowed lines (`crates/blaze-usercode/src/trace.rs`)

A fork server's test now comes with `Trace::Lines(test::Lines)`: each line's `Seen` and what
it names, in one buffer, with no scope (every line is the test's or of none).
`Trace::traced` turns them into `Vec<Line>` (a `String` per line, and an empty scope) only
to call `assemble`. A `pub(crate) fn assemble_seen<'a, I: Inputs>(lines: impl IntoIterator<Item = (Seen, &'a str)>, inputs: &mut I) -> Result<Traced, I::Error>`
(what `assemble(lines, None, inputs)` does, of lines of no scope) would drop that copy.

Also optional: `trace::encode_traced(&Traced) -> Vec<u8>`, the bytes of
`encode(&Report::Ran(Some(traced.clone())))` without the clone; `select::record` clones a
borrowed record once to encode it into the history.

> applied (usercode): `trace::assemble_seen(lines, inputs)` over `(Seen, &str)`, each name owned once; `assemble` filters by scope and calls it; `Trace::traced` calls it on `Lines::iter()`. `encode_traced` not done: the one clone is on keeping a pass, after a test process ran.

> forwarded to usercode (told by tmux).

## usercode-fast-tests 3: codegen: the harness appends `fork_server(record)` (`crates/blaze-engine/src/test_harness.rs`)

This replaces step 2 of the `usercode-tests` request to codegen ("start the fork server from
the test harness's `main`"): `blaze_usercode::test::FORK_SERVER` is gone. Where that request
says `text.push_str(blaze_usercode::test::FORK_SERVER);`, push
`blaze_usercode::test::fork_server(record)`, where `record` is whether the binary has
jump-table slots (`fastcompile-ident-select 5` and 9): with `true` the text declares
`extern "C" { fn __blaze_exec_slots() -> u32; fn __blaze_exec_arm(region: *mut u8, len: usize) -> i32; }`
and the binary must link the runtime that exports them; with `false` it names neither. Both
texts start with `#[allow(dead_code)]` before `pub mod fork_server {`; a test that compares
the text after `MARKED` compares it with `fork_server(record)`. The wire version is 2.

> forwarded to codegen (window `codegen-next1`, told by tmux).

## usercode-fast-tests 4: cargo: what a test executed comes with its result (`crates/blaze-cargo/src/serve/unit/test.rs`)

`blaze_usercode::test::Finished` has a new field `executed: Executed`
(`blaze_usercode::test::Executed`): `Hits { slots, hits }` (strictly increasing, each below
`slots`) or `Not(why)`. Nothing breaks: cargo reads `Finished` by field. Once
`fastcompile-ident-select 7` moves cargo onto `blaze-testsel`, `Hits` is
`Record::of_hits(slots, hits)` and `Not(why)` is `Record::incomplete(why)`.
`Trace::Lines` now holds `test::Lines` (not `Vec<Line>`); `Trace::traced` is unchanged.

No new Cargo dependencies.

> forwarded to cargo (told by tmux).

## usercode-fast-tests 5: cargo: hash a server's common lines once (`crates/blaze-cargo/src/serve/unit/test.rs`)

From the speed-of-light pass (`docs/v2/plan/lanes/usercode/speed-of-light-tests.md`, the
queue). `test::Lines` now shares the lines of every scope (shared libraries, the runtime's
reads) between the results of one server: `Lines::common()` is how many of the first lines
are those, and `a.common_is(&b)` says two results share the same buffer of them. Today
`Trace::traced` hashes each line again for every test, about 20 µs a test for 2,000 common
lines, which puts a test's result 30% over its bound. Keep the `Traced` of the common part
per buffer (keyed by `common_is`, or the first result's `Arc` address while it lives) and
assemble only each test's own lines into a copy of it. Nothing breaks without it; it is the
speed.

> forwarded to cargo (told by tmux).

## usercode-fast-tests 6: parent (selection's callers): compute a test's identity once

`Test::identity` is about 90 µs for a test reaching 10,000 functions (sorting and hashing
its reached set), and `select` and `record` each compute it, so a run that selects and then
records pays it twice. Either compute it once and pass it to both (a `select::Test` built
once per test per request, with `identity` computed beside it), or have codegen give
`Reached::reached` already sorted and deduped, which removes the sort as well (identity is
then 64 µs: BLAKE3 over the bytes). Say which, and I change `select` and `record` to take it.

> forwarded to usercode (told by tmux).

## usercode-fast-tests 7: cargo and the parent's sandbox: replicas of a fork server

Speed of light, the fork server: one server forks serially, about 18,000 tests a second
per binary, which makes the fork (not the cores) the wall for any test shorter than
`cores × 55 µs` (3.5 ms on 64 cores). The removal is `K` replicas per binary: after the
hello the server forks `K - 1` copies, each connecting on a socket of its own. I change
the server and the queue (several connections per binary). What I need: cargo's listener
to accept `K` connections per binary on the same socket path (each sends its own `Hello`),
and the sandbox to allow it (it already allows the one). Reply with whether the sandbox
needs a change, and I send the wire change.

> forwarded to cargo and usercode (told by tmux).
