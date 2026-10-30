---
status: plan
summary: How blaze v2's native proc-macro expanders (#1297) are built, when they are used, and why bitflags has none.
date: 2026-10-29
---
# Native expanders (#1297)

Part of [usercode](plan/lanes/usercode.md). Code: `crates/blaze-expand/src/native/`; requests:
`v2/requests/usercode-macros-native.md`.

## What runs

The expander of a macro is the macro's own code, not a rewrite of it: the published sources
of serde_derive 1.0.229, thiserror-impl 1.0.69 and 2.0.21, tokio-macros 2.7.2, and of what they
are built with (syn 2.0.119 and 3.0.9, quote 1.0.47, proc-macro2 1.0.107, unicode-ident 1.0.27),
vendored under `vendor/native-macros/` and compiled into blaze as ordinary libraries. The only
source change is in the four macro crates' `src/lib.rs`: the lines of their
`#[proc_macro_derive]`, `#[proc_macro_attribute]` and `#[proc_macro]` attributes are deleted, so
the functions are plain `pub fn`s blaze calls. Each `Cargo.toml` is blaze's (renamed packages,
path dependencies, no proc-macro target); the published bytes of both files are kept under
`.published/`, and they are what the table of verified files holds.

The crates are built against `native/proc_macro/` (a package whose library is named
`proc_macro`), which is blaze's WebAssembly guest `proc_macro`
(`crates/blaze-macro-wasm/guest/proc_macro.rs`), the same text from `fn taken` to the end of its
`wire` module, with each wasm import turned into a call to a thread-local host
(`__native::Host`). `native/host.rs` answers those calls as the wasm host does
(`crates/blaze-macro-wasm/src/lib.rs`, `add_host`): spans are numbers the host gives out, the
def, call and mixed sites first, then the input's in the order they first appear; joins,
parents, sources, line and column, file and source text are asked of the database through
`load_cargo::answer`, as for a module. `native/wire.rs` and `native/lex.rs` are the wasm host's
`wire.rs` and `lex.rs`, text for text (blaze-macro-wasm depends on blaze-expand, so the two are
copies held equal by a test rather than one shared module).

### Why this and not hand-written expanders

Span exactness is the hard part (the #1297 comment's analysis): the output of serde_derive is a
function of how syn parses the input, of where quote's `quote_spanned!` puts call-site and
input spans, and of what proc-macro2 does with a join that fails. A hand-written expander must
reproduce every one of those choices for every input, forever, and drift silently with each
release. Running the published code against a `proc_macro` that is the one the wasm module
already uses leaves one place where the two can differ: the host's answers, which are the same
code paths. Supporting a new version is vendoring it and rerunning the tests, not porting it.

The cost is compile time of blaze (two syns, compiled once) and that blaze links code it
did not write. That code is what the user's build would run anyway, and none of the vendored
packages reads a file or an environment variable as it expands, so it needs no sandbox: what it
sees is its input and the host's answers.

## When it is used: an exact-match cache

`native::find(dylib, name, kind)` reads the library's closure from the hook registered with
`describe_with` (from the `rust-project.json` blaze wrote to build the library's module,
`closure_of_description`): for the macro's crate and every package it depends on but the
standard library, its name, version, the BLAKE3 of every file of its directory (but cargo's
`.cargo-ok` and `.cargo-checksum.json`), its cfg (features and what its build script set), and
its dependencies' names. It is the verified copy when, from the macro's crate down its
dependencies, every package has the same name and version, the same set of files with the same
hashes, the same features, the same dependency names pointing at the same verified packages,
and the same cfgs of those any source of it or its dependencies names. A cfg no compiled source
of the package or its dependencies names as a word (as `debug_assertions`) can't change the code
and is not compared. Target cfgs are not in a description; the native code runs on the same host
the real macro does.

Anything else (one byte, one more feature, another syn) is `Ok(None)`: the macro is built and
expanded as it would be with no native expander. That is a miss of a cache keyed by the whole
closure, not a fallback: the result is the same expansion either way. A failure to read or
describe the library is `Err`, which fails the expansion. `native::report()` counts hits and,
for each library of a verified crate's name and version that missed, its first difference.

A known limit of hit rate, not of correctness: cargo unifies features over the whole build, so
syn 3 is verified with the union serde_derive, thiserror-impl 2 and tokio-macros ask for
(clone-impls, default, derive, full, parsing, printing, proc-macro). A build whose other proc
macros ask syn for more (`extra-traits`, `visit-mut`) or that has none of tokio-macros' `full`
misses. Adding a configuration is vendoring the same sources again as a package built with other
features; which to add is a question for the corpus's miss report.

`Macro::identity` is a hash of the verified closure (every file, cfg and dependency), of the
native code (`native/*.rs`, `proc_macro/`), and of the macro's name and kind: what its
expansions are a function of besides their input and the database's answers.

## Proving it: verify mode

`BLAZE_NATIVE_MACROS=verify` (read by blaze-expand's `lib.rs`) expands every call with a native
expander both ways and fails the expansion, naming the macro, at `native::difference`'s first
difference: token by token, each delimiter kind, character, spacing, identifier text and
rawness, literal kind, text and suffix, and every span's range, anchor and syntax context.
Running the corpus with it set is the check that the expanders are exact; the log line
`native proc macros: N found; …` says how many were used and why the rest missed.

Tests beside the code (`native/tests.rs`) check: the vendored files are the table's and the
built `lib.rs` is the published one less its attribute lines; the verified closure matches
itself; a changed byte, a file more or less, a feature more or less, another syn, a dependency
less each miss with the message naming it; unnamed cfgs are not compared; proc-macro2's
build is what its code shows; `find` gives an expander only for the verified copy and errs
for an undeclared macro or an undescribed library; descriptions are read as closures;
`difference` is `None` for equal trees and names a changed text, rawness, spacing, range,
anchor, context or suffix, and a shorter tree; and serde's `Serialize` and `Deserialize`
(structs, enums, generics, `#[serde(rename, skip)]`, a union's error), thiserror 1 and 2
(`#[error("…")]`, `transparent`, `#[from]`, `#[source]`, a missing attribute's error),
tokio's `main` (default and `flavor = "current_thread"`, a non-async error), `test`, and its
`select!` helpers (including the panic they raise) expand to what they should.

## bitflags: no native expander

`bitflags!` 2.x is a `macro_rules!` macro. Its expansion is not a run of user code: it is
`mbe` matching and transcribing in process, already as native as it gets, and its cost is per
step. Its call expands to calls of the crate's internal macros (`__declare_public_bitflags!`,
`__impl_public_bitflags!`, `__impl_bitflags!` and the rest), each a separate expansion with its
own `MacroCallId`. The syntax context of every token is the chain of marks of the expansions
that produced it, with `$crate` resolved through those marks and each step's def-site tokens
anchored to the bitflags crate's own source.

Replacing the first step alone could be made exact (mbe's transcription rules for spans and
`SemiTransparent` marks, given the definition's token spans), but it would save nothing: mbe
does the same work. Replacing the whole recursion with one step is what would save time, and it
can't be exact: one expansion has one mark, so the output's syntax contexts would lack the
marks of every inner expansion, which changes hygiene (local names and `$crate` across steps)
and every span's expansion parent. So there is none, and no hook in `hir-expand` is requested.
Making bitflags cheaper belongs in mbe (cached matching per definition), which is the format
and db lanes'.
