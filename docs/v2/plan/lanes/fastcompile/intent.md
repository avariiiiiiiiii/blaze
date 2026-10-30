---
status: plan
summary: blaze v2 lane fastcompile, ideas 9 and 10 (semantic edits, declared intent): crates/blaze-intent's edits by meaning applied to the flat tree in place, the byte-equality argument against the text edit, rename through the front end's resolution, what each edit reports changed, the agent API's Edit, WillTest and WillSave, the session's take-over and cancel rules, the plane's idle class requested from core, the hooks requested from ci, core, db and cargo, the tests, and the state of the art.
date: 2026-10-29
---
# fastcompile: semantic edits and declared intent (`crates/blaze-intent`)

Ideas 9 and 10 of [compile-time.md](../../design/compile-time.md). An agent says what it changes
by meaning (`Edit`), and what it is about to do (`WillTest`, `WillSave`), so the daemon
reparses only what changed and builds ahead what the next request needs. The code is in
`crates/blaze-intent`. The API's half is in `crates/blaze-api` (`intent.rs`,
`session/intent.rs`). What other crates must do is requested in `v2/requests/fastcompile.md`
under `fastcompile-intent <n>`.

## Edits by meaning

`Request::Edit { base, head, edits }` names items by path within a file: `Shape::area`,
`<Circle as Shape>::area`, `fn area@1`, a tuple field by its index (`blaze_intent::path`). The
edit kinds are:

- `ReplaceBody`, `ReplaceSignature` and `ReplaceItem`;
- `AddField` and `RemoveField`;
- `AddItem` and `RemoveItem` (an item, a field or a variant);
- `AddUse`;
- `Rename`.

The daemon's job (`Work::Edit`) calls `blaze_intent::apply` on the view's lowered front end.

- Each edit finds its node in the file's flat tree and writes one splice
  (`file::Working::splice`). The tree is then reparsed only where the splice falls
  (`Parse::reparse`).
- The output is the VFS's `Op::Edit`s, one per splice, in file order, plus a renamed module's
  file moves. With them comes each touched item, with its `How`, its path, and its `ItemKey`
  in the view before the edit.
- The session commits the ops to `head` as a `Commit` would, after `Response::Edited`.

**Same bytes as the text edit.** A splice is a byte range and its replacement text, so the
bytes committed are exactly those of the text edit that replaces that range. The tests build
the equivalent text edit by hand for every kind and compare the VFS roots. Where an edit has
a choice to make (where to insert, how to indent, a trailing comma, a blank line), it takes
the file's own style and a fixed rule. The rule is the one a person would write, and the
tests hold it. The reparsed tree must equal a fresh parse of the new text: tree shape and
sorted errors are checked for every edit. Only error-free trees are handed to the front end
as seeds, since error order can differ.

**Changed items.** Each edit reports what it touched at item level.

- A body edit is `Body`. A signature edit is `Signature`. A field list edit is `Fields`.
- Replacing an item is `Whole`. `Added`, `Removed` and `Renamed` are what they say.
- A container whose member list changed (module, impl, trait, file) is `Scope`.

The test oracle diffs every item's own text, with its members cut out, before and after the
equivalent text edit. The set it finds equals the reported set for every kind. Keys come from
the base view, so invalidation can start from the named items (`blaze_items::resolve`), not
from a text diff.

**Rename resolves, never searches text.**

- The files searched are those of the definition's crate, plus every crate that depends on it
  if the definition is public. A `memchr` finder over the base text only prefilters them.
- Each identifier token with the old name is classified through `Semantics`: definitions,
  paths per namespace, method calls, fields, record shorthands, associated type bindings,
  `offset_of!`, `extern crate`.
- A shorthand (`Point { x }`, pattern or expression) is written out as `x: new` or `new: x`, so
  the binding keeps its name.
- An out-of-line module's file moves, `m.rs` and `m/` or `m/mod.rs`. A `#[path]` module's
  file stays where it is.
- **No-fallback.** When the old name occurs where the front end can't say what it means, the
  rename stops with an error naming `path:line`, and nothing is skipped. That covers a macro
  token tree, an unresolved path, a token a macro uses both for the item and for something
  else, and renaming an item that implements a trait method ("rename it in the trait").
  Renaming the trait method renames every impl of it and every call.

## Declared intent

`Request::WillTest { view, filter, targets, wait_ms }` and
`Request::WillSave { ops, head, test, targets, wait_ms }` are answered at once with
`Response::Announced { root }`. They start a job of no request through
`Builder::speculate`, which runs in the plane's idle class.

- **WillTest** (`Work::Ahead`) runs no test. It builds the test units holding a test the
  filter selects, using libtest's rule: the name contains a filter, or every test when there
  is no filter. The functions those tests reached when they last ran are compiled first, the
  ones more of them reached ahead of the rest (`blaze_intent::ahead::plan`).
- **WillSave** checks, or with `test` tests, the what-if of `ops` on `head`'s current root.

The session (`session/intent.rs`) keeps each intent's view open until its job finishes.

- **Taken over (promoted, once).**
  - A request that builds the intent's root incrementally takes it over: check, build,
    test, run, codegen inputs, trace, or a best-of candidate.
  - So does a commit to its head that makes its root, whether a commit request, an edit's
    commit, or the daemon's own watcher commit (`Session::head_moved`).
  - From then on the job runs at normal priority, and the request joins its in-flight nodes
    through the plane's single flight. The work is reused, not repeated.
- **Cancelled.** An intent not taken over is cancelled by:
  - a newer intent of the same connection;
  - a commit to its head that makes another root;
  - its connection's end;
  - `wait_ms` passing on the daemon's clock (`Session::tick`, `Session::deadline`).

  Cancelling a job frees its memory as any cancel does. A taken-over intent is never
  cancelled by these.
- **A request without intent behaves exactly as before.** Every new path starts from an
  intent in the table. With an empty table, no `speculate`, `promote` or extra `cancel` is
  ever called. The session's existing 200-seed simulation asserts this.

**Never delaying real work** is the plane's job (request 2). An idle request's task starts
only when no foreground task is ready and fits. When a foreground task is blocked only by
running idle tasks, the plane yields the newest of them. Without the yield, a running idle
task could hold a thread for up to one task's length.

## Hooks

| # | Owner | What | State |
|---|---|---|---|
| 1 | ci | API types, variants, `SCHEMA` 9, `Work::{Edit, Ahead}`, `Builder::{speculate, promote}`, session handling and tests, blaze-ui lines | written by fastcompile-intent (ci DONE) |
| 2 | core | idle class, `Promote`, `Yield` in the plane | requested |
| 3 | db | a parse seed taken by `EditionedFileId::parse`, keyed by `FileText` content and edition | requested |
| 4 | cargo | `Jobs`' `speculate`/`promote`, `Work::Edit` and `Work::Ahead`, `tick`/`deadline`/`head_moved`, `heads.rs`, the dependency | requested |

## Tests (beside the code)

- `blaze-intent/src/tests/edits.rs`: every edit kind against its hand-made text edit (same
  bytes and VFS root, same changed items by the oracle), a run of several edits in a row,
  fresh-parse equality, and the error cases.
- `blaze-intent/src/tests/rename.rs`: rename over a two-crate module graph. It covers a
  function across crates, a field with shorthands written out, a module with its file moved,
  an associated function followed by a body edit, a trait method reaching its impls and
  calls, and the refusals (a macro body, an impl of a trait method). Keys are checked
  through `blaze_items::resolve`.
- `path.rs`, `file.rs` and `ahead.rs` have their own unit tests.
- `blaze-api/src/session/tests.rs`:
  - an edit commits its reported ops once durable, and a failed, cancelled, silent or gone
    edit commits nothing;
  - intent take-over by request and by commit;
  - each cancel path (a newer intent, another root, the connection's end, the wait);
  - a 200-seed simulation of intents, commits, clock ticks, finishes and connections ending:
    each intent is taken over or cancelled at most once, never both, never after its end,
    and every view is freed;
  - the request-without-intent assertion.
- `blaze-api/src/frames/identity.rs` holds every new variant to the reference codec.

## State of the art

- **rust-analyzer.**
  - Its rename (`ide-db` `rename` and `search`) also classifies text matches through
    `Semantics`. It edits with `TextEdit`s through the client, and it falls back to text in
    macros. Here a macro occurrence it can't resolve is an error.
  - Its incremental reparse (`syntax::parsing::reparsing`) relexes a token or reparses a
    `{}` block. blaze-intent uses the same `Parse::reparse`, and hands the result to the
    front end instead of discarding it.
- **Roslyn.** `Renamer` and `Solution.WithDocumentSyntaxRoot` apply semantic edits to an
  immutable syntax tree. `GetSpeculativeSemanticModel` answers what-ifs without committing.
  blaze's what-if is a VFS root, so a speculative build and a real one share their results
  by content.
- **Unison.** It stores definitions by content hash, so a rename touches only names. blaze
  keeps Rust's text as the source of truth but reports changes by `ItemKey`, so invalidation
  also starts from definitions.
- **LSP `textDocument/willSave`.** The editor-side precedent for `WillSave`. It carries no
  content and nothing builds on it; `WillSave` carries the ops, so the daemon can build the
  exact root before it exists.
- **Speculative builds.** IDE background compilers and watch modes (rust-analyzer flycheck,
  dune watch) rebuild after a save, and CI merge queues build ahead of merge. Here the build
  starts before the save, in a strict idle class, and is promoted, not restarted, when the
  save lands.

## Known limits

- An occurrence in code the lowered graph doesn't analyse is an error rather than skipped.
  An example is `cfg(test)` code when the test crate isn't lowered. Lowering more of the
  graph removes these errors; skipping them would be a quiet wrong rename.
- Without request 3, the first query after an edit parses the edited file again. The bytes
  are still correct; only the cost idea 9 removes is paid.
- Without request 2, a speculative job runs as an ordinary weighted request. Until then the
  daemon must not implement `speculate` as `start`: it stops with an error naming the
  missing idle class.
