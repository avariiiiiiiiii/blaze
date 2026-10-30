---
status: plan
summary: The speed of light of blaze-lints in v2 (a warm re-lint, an edit, a cold crate, blaze's reachability) and how the per-body design reaches it.
date: 2026-10-29
---
# blaze-lints: speed of light

Machine: one warm VM, 4 GHz, about 20 GB/s of memory bandwidth per core and 100 GB/s or more
across the socket. A large crate: 10,000 bodies, 2 million syntax elements, 40 MB of text.

## A re-lint with nothing changed

The work is one memo hit per body and per file, plus the findings' levels and the output.
A hit is about 15 loads with a dependent chain of 5 to 6, 6 to 8 ns
([frontend/db.md](../frontend/db.md)), so 10,000 bodies come to 60 to 80 µs on one core, or
under 10 µs over 16 cores. Levels are decided only for findings (one ancestor walk each), so a
clean crate pays nothing for them. **Bound: under 100 µs.** Reached by: every body is a
query (`body_lints`), its `LintSet` is interned (a `u32` compare), and the driver walks no tree
unless something was found.

## An edit of one body

The work is the edited body's tree walked once, each node dispatched on its kind, and the
typed lints' lookups for the few nodes their syntax filter passes. A body of 500 elements
walked through views is 500 steps of column arithmetic (2 to 3 ns each with the kind
dispatch, a `u16` tag `match`), about 1.5 µs; the type lookups hit `infer`, which the
check already ran for the same body (a memo hit). **Bound: a few µs, on top of the check's
own re-inference of that body.** Reached by: the query reads `body_syntax`, anchor-relative,
so no other body of the file runs again (rustc's lint passes run over the whole crate on every
build, the state of the art this beats: its late lints are not incremental); views, so the
walk counts and allocates nothing; types only asked for after a syntactic filter.

## A cold crate

The work is every element of the crate walked once: 2 million elements at about 2.5 ns is
5 ms of compute on one core, and the kind column (2 bytes an element) is 4 MB to read,
0.2 ms of bandwidth, so it is compute-bound. Bodies are independent queries and spread over
the cores (`Db::run_all`): **bound: 5 ms divided by the cores, about 0.4 ms on 16**, plus
inference, which the check has done. Which lints may be on is one scan of the text for
`clippy::`, `blaze::` and level attributes (memchr, about 10 GB/s: 4 ms for 40 MB, once per
file, not once per lint), each name found looked up by the registry's perfect hash.

## `redundant_clone`

The one lint that walks more than its node: for a `.clone()` that passes the syntactic and
type filters (a local, by autoref, not `Copy`), it walks the body's tree once for the local's
other mentions, a compare of a node kind each and a resolution (a memo hit) for each path.
A body of 2,000 elements with 4 such clones is 8,000 elements, about 20 µs at 2.5 ns, and
clones that pass the filters are a few per body. **Bound: one extra walk of the body per
candidate clone**, reached because every cheaper test runs first; a body with none pays
nothing past the filters.

## blaze's `reaches_alloc`

One query per function, memoized for the daemon's life: a function's answer is its calls'
answers ORed, a call one load of a memo. The cold cost is inferring the bodies of what the hot
code calls, std's among them: a few thousand bodies, which a build infers anyway for codegen.
**Bound: one memo hit per call site once warm.** Cycles start `false` and settle in one pass.

## What stands in the way

- Until `hir::BodyAnalysis` exists (requested of lane `db`), a typed lookup has no way to
  ask the relative tree; the lints are written against it.
- Macro calls in a body are expanded through salsa's `parse_macro_expansion`, a memo hit
  each; the expansion trees are views as well.
