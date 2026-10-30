# Requests of diagnostics-lints-syntax-early

## The span of a statement a `$x:stmt` fragment transcribes (hir-expand's mbe)

What: rustc's UI test `lint/redundant-semicolon/suggest-remove-semi-in-macro-expansion-issue-142143.rs`
expects the empty statement `;` handed to `($stmt:stmt) => { #[allow(bad_style)] $stmt }` by
`m!(;)` to be spanned by the `$stmt` of the macro's definition (6:43, `^^^^^`, "in this macro
invocation" at the call), not by the `;` of the call: rustc parses a transcribed `stmt` fragment
from its invisible-delimited group, whose span is the metavariable's. Here the `;` of the
expansion maps back to the call's `;`.
Where: `crates/hir-expand` (the span map of an expansion), for fragments of kind `stmt` (and
whichever others rustc re-parses from their group).
Why: `redundant_semicolons` (`lints/syntax/early/builtin.rs`) gives the right lint and no
suggestion there, but at the wrong span; the test is left out of
`tests/lints/syntax/early/builtin.rs` until this is in.

> forwarded to db: `crates/hir-expand/` (the mbe transcription's spans) is lane db's.
