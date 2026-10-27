# blaze's own toolchain

blaze is its own Rust toolchain. It builds every project with one toolchain that blaze
controls, names and ships, whatever the project pins: `blaze-2026-10-25` today. It is not the
project's toolchain and it does not try to be. The project's own toolchain, and its CI, stay
the final word on whether the code builds.

Andrew, 2026-10-26, answering "blaze uses its pinned nightly even when the project pins stable":
"you should use YOUR OWN NIGHTLY that you control since you are trying to replace rustc".

## Why one toolchain, and why blaze's

blaze compiles your code itself (Cranelift or LLVM, from rust-analyzer's MIR) and links it with
code rustc compiled. The two must agree on how types are laid out, how functions are called,
what a `TypeId` hashes, the names of symbols, libtest's entry point, the proc-macro bridge and
what rustc answers about targets. blaze knows those facts for one toolchain, checked by the
corpus difftest, and that toolchain is the base blaze replaces piece by piece
([no-rustc.md](no-rustc.md): cargo, then rustc, then the standard library's build). Following
each project's pin would mean knowing every release's facts and checking every release; the
goal is the reverse, no rustc at all. So the toolchain is part of blaze, versioned with it, like
the standard library is part of a compiler.

## What it is today

- **Name:** `blaze-<date>` (`blaze-2026-10-25`), after the date of the Rust nightly it is built
  from (`harness/toolchain`, `nightly-2026-10-25`). `blaze toolchain` prints it and where it
  is; `blaze toolchain --path` prints the directory alone.
- **Bytes:** the upstream nightly's `rustc`, `cargo`, `rust-std` and `rust-src`, unchanged, from
  static.rust-lang.org, each checked against the sha256 compiled into blaze
  (`crates/ra-jit/src/cargo/toolchain/pins.txt`, `harness/toolchain_pins.py`). So `rustc -V`
  still says `1.101.0-nightly (12433c307 2026-10-24)`.
- **Where:** `~/.blaze/toolchains/nightly-2026-10-25-<host>`, or rustup's `nightly-2026-10-25`
  when rustup already has it with `rust-src` (the same bytes). The installer downloads it once;
  `blaze toolchain --path` prints its directory.
- **Who uses it:** cargo, rustc and the daemon that blaze starts (`CARGO`, `RUSTC`, `RUSTDOC`,
  `PATH` for its own processes). The user's `cargo`, `rustc` and pins are untouched.
- **Bumps:** on blaze's schedule, deliberately, with `harness/toolchain_bump.py`: it moves the
  pin, re-vendors `rustc_abi` and the proc-macro server, regenerates the pins and answers, and
  the corpus difftest (`harness/difftest.py corpus`) and the edit harnesses are the gate.

## What it means for a project

- A project that pins another toolchain (a stable release, an older nightly) builds with
  blaze's. blaze says so once per project and pin, as a note, not an error, and switches
  nothing:
  `note: blaze builds with its own toolchain, blaze-2026-10-25, not rustc 1.99.0 (...), which
  the project's rust-toolchain file names. ...`
- Rust keeps building old code, so stable code builds; `#![feature(...)]` code builds too.
- Code blaze's toolchain cannot build (a nightly feature it removed, a library item newer than
  it, `rust-version` above 1.101) fails with rustc's error and a note that blaze built it with
  its own toolchain, not the project's.
- Keep `cargo build`/`cargo test` with the project's toolchain in CI. blaze is for the edit
  loop; the project's toolchain decides.

## Where it goes: no rustc

blaze is moving off rustc ([no-rustc.md](no-rustc.md)); the rule is in [AGENTS.md](../AGENTS.md).
blaze's nightly is the definition of the language and the standard library blaze matches while
rustc is still in the tree: its layouts, ABI, `TypeId`s and answers are what blaze agrees with.
It is not a rustc blaze maintains. As the no-rustc steps land its pieces go: cargo, then rustc
with rust-lld and the proc-macro server, then `rust-std`; what is left is `rust-src`, shipped as
part of blaze, and the toolchain is blaze alone.

- [#402](https://github.com/indexable-inc/blaze/issues/402): ship the standard library's
  source (`rust-src` of the pinned nightly) with blaze's release, so blaze's toolchain needs no
  upstream download once rustc is gone.
- [#403](https://github.com/indexable-inc/blaze/issues/403): shrink the download as each
  no-rustc step removes a piece (`cargo`, `rustc`, `rust-std`), down to blaze alone.
- [#404](https://github.com/indexable-inc/blaze/issues/404): the bump, on blaze's schedule:
  `harness/toolchain_bump.py` with the corpus difftest and edit harnesses as the gate, the
  `blaze-<date>` name moving with it.
