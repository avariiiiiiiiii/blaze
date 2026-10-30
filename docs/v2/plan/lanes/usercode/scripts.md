---
status: plan
summary: How blaze v2 reads build scripts (what they print, their environment, the scripts elision reads instead of running, what reruns them) in one pass over borrowed bytes, and how each part compares with the fastest known way.
date: 2026-10-29
---
# Build scripts

Part of [usercode](../usercode.md). Code: `crates/blaze-build-script/`. Requests:
`v2/requests/usercode-fast-scripts.md`.

Every part below reads its input once, borrowed, and allocates only what it hands back. Each
rewrite keeps the code it replaced as a test-only reference (`reference.rs` beside it) and a
seeded test that the two agree on every input: what changed is speed, not what is read.
Nothing here is measured yet: the comparisons are of technique, and the benchmark to settle
them is under [What is not done](#what-is-not-done).

## What a script prints (`output.rs`)

cargo's `BuildOutput::parse` (`src/core/compiler/custom_build.rs`), the baseline, splits the
output with `lines()`, makes a `String` per line, and dispatches each key by a chain of
string comparisons, owning every value it keeps.

blaze reads it in one pass ([`output::read`]): line breaks are found by memchr (SIMD, 16 or
32 bytes a step), a line that does not start `cargo:` is passed over without being read as
text, and the key is matched by its length and then its bytes into a `u8` enum (`Key`), so no
key costs more than one comparison of its own length. Each directive goes to a `Sink` as a
`Directive<'_>` whose values borrow the output; a value is copied only when it names the
`OUT_DIR` the script ran with and that directory moved (found by a `memmem::Finder` built
once per output). `parse` is the sink that keeps an `Output`; `watch::watched_in` keeps the
`rerun-if-*` values alone and copies nothing else.

This matches the fastest known way for this input. Lines are short and few (tens to
thousands), so memchr's line splitting is the bound, and the dispatch is one jump on length
plus one compare. What is left is the UTF-8 check of each directive line, which std does in
SIMD already.

## The environment (`env.rs`)

cargo builds the environment in a `HashMap` of owned strings, a `format!` per derived
variable, then sorts for the process.

blaze presizes one vector for the fixed variables, the unit's and the inherited ones; builds
each value at its exact length without `format!` (`prefixed`, `decimal`, `concat`); takes the
fixed names as `&'static str`; borrows inherited variables until they are kept; and sorts once
at the end, by a big-endian `u64` of each name's first eight bytes, then the name, then the
order set, keeping the last word per name. `Env::vars` is the sorted vector; lookups
(`get`, `origin`) are binary searches.

This is the fastest known shape: one allocation per kept string, one sort of about a hundred
entries whose comparisons are mostly one integer compare. A reused buffer would only add a
copy, since every kept string is handed back owned; the one place that joins scratch paths
(`cargo_started_in`) reuses a `PathBuf`.

## Scripts read rather than run (`elide.rs`, `elide/lex.rs`)

Relax (`front/src/unitgraph_noop.rs` of makepad/makepad, #1296) decides a script prints only
constants by matching its source text against the forms it reads. simdjson's stage 1 is the
fastest known lexer for small grammars: classify 64 bytes at a time into bitmasks (quotes,
backslashes, structure, whitespace), then walk the set bits.

blaze lexes with a 256-entry byte-class table (built by a `const fn`) and jumps over the
inside of strings, block comments and raw strings with memchr2 and memchr, so the per-byte
work is one table load except inside literals, where it is SIMD. Tokens are made on demand
(`Tokens`), and elision asks for them only as its forms need them: most scripts start with
`use`, and are refused at their first token, having lexed nothing else. A string token keeps
its text and whether it has escapes; its value is decoded only when the script is printed
(`Token::push_value`), into a buffer reused for every call, so no token allocates.

Where this falls short of simdjson: stage 1's bitmasks would classify a whole file in a few
instructions per 64 bytes, where we take one load per byte outside literals. For a `build.rs`
of a few kilobytes that is a few microseconds, below the cost of reading the file, and the
early exit means most files are not lexed past their first line. Closing the gap would need
`std::simd` (nightly-only, portable) or `unsafe` intrinsics, which this crate forbids; it is
worth doing only if a profile shows lexing, not I/O, on the path.

Against Relax: both refuse at the first form not read; Relax matches text and so refuses
scripts a comment or a line break moves, where tokens read every spelling rustc reads.

## What reruns a script (`watch.rs`, `apply.rs`)

cargo joins each `rerun-if-changed` path to the package's directory with a fresh `PathBuf`
and dedups by a linear scan; `mtime_recursive` stats each entry by its full path.

blaze joins every path in one reused `PathBuf` and copies it out only when it was not met
before; dedup is a scan up to 16 entries and a hash of each value past that (a script naming
every file it reads lists hundreds). `Rerun::stale` reuses one buffer the same way. The
recursive walk reads each entry's own time from its directory (`DirEntry::metadata`, which
is `fstatat` on Unix) and makes its full path only for a link or a directory. `to_link`
(cargo's `build_map`) asks the graph for each dependency's node once per edge where it asked
twice, and does not follow a dependency that is neither for the host nor linked, which can
give nothing.

## Runs (`run.rs`)

`run`, `elided`, `begin` and `Ran::finish` make directories, write cargo's files and stat
them: they block, and are for the harness and v1. A caller on a thread that must not block
(blaze v2's daemon, which acts through effects) uses `run::judge`, which reads only the
bytes it is given, and makes the directories and files itself from `run::invoked` and
`run::files`. What a failed run shows (`logged`) is found by one `memmem` search for
`\ncargo:`, so lines that are not directives are never looked at.

## What is not done

- A benchmark: the seeded generators of the differential tests at sizes from real crates
  (the corpus of `harness/scripts.py`), old against new, per part. Until then the claims
  above are of technique.
- The callers' side, which is other lanes' (`v2/requests/usercode-fast-scripts.md`): blaze-
  cargo judging runs by borrowing (`run::judge`) rather than cloning both outputs, making
  `OUT_DIR`, the run directory and cargo's files through effects, and blaze-usercode reading
  what reruns a script with `watch::watched_in` rather than parsing the whole output.

[`output::read`]: ../../../../../crates/blaze-build-script/src/output.rs
