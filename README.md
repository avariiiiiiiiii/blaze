<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="docs/brand/fire/header-dark.webp">
    <img alt="blaze" src="docs/brand/fire/header-light.webp" width="344" height="184">
  </picture>
</p>

<h3 align="center">Rebuild Rust in milliseconds.</h3>

<p align="center">The same commands as cargo, with <code>blaze</code> in front.<br>Blazingly fast Rust, literally.</p>

<p align="center">
  <a href="#install">Install</a> ·
  <a href="#try-it">Try it</a> ·
  <a href="#how-fast">How fast</a> ·
  <a href="#how-it-works">How it works</a> ·
  <a href="#learn-more">Docs</a>
</p>

## Install

<!-- Once https://ix.dev/blaze/install.sh serves install.sh, it is the line to show here. -->
```sh
curl -fsSL https://raw.githubusercontent.com/indexable-inc/blaze/main/install.sh | sh
```

- **macOS on Apple silicon and Linux**, with or without Rust installed.
- blaze downloads the Rust it builds with once (**about 100 to 135 MB**), or uses rustup's.
- crates.io or Nix: [other ways to install](docs/install.md).

## Try it

In any Rust project:

```text
blaze run     # like cargo run
blaze test    # like cargo test
blaze build   # like cargo build
```

1. The first build runs cargo, then blaze loads your project (a few seconds).
2. Change a function and build again: **back in milliseconds**. The `Finished` line says `[ra-jit]` (blaze's engine).

- Every other cargo command passes straight through to cargo.
- Where cargo is installed, `cargo blaze run` is the same as `blaze run`.

## How fast

Edits rebuild <!-- benchheadline:start -->**10× faster** on macOS arm64, **24× faster** on Linux aarch64 and **11× faster** on Linux x86-64<!-- benchheadline:end --> than with `cargo build`, on average over real projects.

<!-- benchchart:start -->
<picture><source media="(prefers-color-scheme: dark)" srcset="docs/benchmarks/charts/edits-dark.svg"><img alt="How much faster an edit builds under blaze than under cargo, by platform: All projects (geometric mean): macOS arm64 10× faster, Linux aarch64 24× faster, Linux x86-64 11× faster; ripgrep 15.1.0: macOS arm64 8.1× faster, Linux aarch64 23× faster, Linux x86-64 12× faster; fd 10.3.0: macOS arm64 6.0× faster, Linux aarch64 22× faster, Linux x86-64 9.3× faster; bat 0.26.0: Linux aarch64 33× faster, Linux x86-64 14× faster; hyperfine 1.19.0: Linux aarch64 14× faster, Linux x86-64 8.0× faster; just 1.43.0: Linux aarch64 22× faster, Linux x86-64 14× faster; axum 0.8.6: Linux aarch64 16× faster, Linux x86-64 10× faster; nushell 0.108.0: macOS arm64 10× faster, Linux aarch64 34× faster, Linux x86-64 20× faster; helix 25.07.1: Linux aarch64 48× faster, Linux x86-64 19× faster; bevy 0.20.0: Linux aarch64 26× faster, Linux x86-64 3.7× faster; biome @biomejs/biome@2.6.3: Linux x86-64 5.6× faster; bun 1.4.3: macOS arm64 21× faster, Linux aarch64 22× faster, Linux x86-64 22× faster" src="docs/benchmarks/charts/edits-light.svg" width="760"></picture>

<sub>Bar: the median over the kinds of edit. Thin line: slowest kind to fastest. Log scale. One machine per colour, so compare bars of one colour: macOS arm64 on MacBook Pro, 2026-10-21; Linux aarch64 on AWS c8g.16xlarge, 2026-10-23; Linux x86-64 on AWS c8a.metal-24xl, 2026-10-25. bun on machines of its own.</sub>
<!-- benchchart:end -->

With [live edits](docs/usage.md#change-a-running-program): **about 20 ms** from saving a file to the running program using the new code.

<details><summary>In milliseconds</summary>

<!-- benchpick:start -->
| Edit one function, then build (Linux x86-64) | `cargo build` | `cargo blaze build` |
|---|---:|---:|
| ripgrep | 508 ms | **56 ms** (9.1× faster) |
| nushell | 1.25 s | **236 ms** (5.3× faster) |
| helix | 2.15 s | **116 ms** (19× faster) |
| bun (1.25M lines of Rust) | 9.11 s | **451 ms** (20× faster) |
<!-- benchpick:end -->

</details>

Every project and platform, memory and disk: [benchmarks](docs/benchmarks.md).

## How it works

- blaze runs **beside cargo** and keeps your project compiled in memory.
- After an edit it compiles **only the functions you changed**.
- It writes them into the program cargo already built, **instead of linking again** from scratch.
- A program that opts in can take the new code **while it keeps running**.

[More](docs/architecture.md).

## Good to know

- **macOS on Apple silicon and Linux.** On Windows every build is cargo's.
- **One Rust: the latest nightly.** blaze builds with the dated nightly it pins (`nightly-2026-10-25` today), whatever toolchain your project pins.
  Stable projects build as they are; `#![feature(...)]` code builds too.
- **It trades memory for speed:** a few GB for a small project.
- **It's young.** Keep `cargo check` or your CI as the final word.
  [Tell us](https://github.com/indexable-inc/blaze/issues) when something differs from cargo.

All the details: [what works and what doesn't yet](docs/usage.md#what-works-and-what-doesnt-yet).

## Learn more

- [Install](docs/install.md): requirements, crates.io, Nix
- [Using blaze](docs/usage.md): day to day, rebuild on save, making plain `cargo build` use blaze
- [Benchmarks](docs/benchmarks.md): every number, how it's measured, and on what machines
- [How it works](docs/architecture.md): a map of the code
- [Cargo compatibility](docs/cargo.md): every command, flag and setting
- [Hot reload](docs/live.md): changing a running program
- [CI runner](docs/ci-runner.md) and [roadmap](docs/roadmap.md): where blaze is going
- [Contributing](docs/contributing.md)

## Demo: a Game of Life you can edit

```sh
nix run github:indexable-inc/blaze
```

- Opens a **Game of Life** window.
- On macOS on Apple silicon you can change its rules, colours and speed **while it runs**; elsewhere it runs without live edits.
- Needs Nix; builds blaze from source the first time.

![Editing the running Game of Life: rule, colours and speed change live](docs/demo.avif)

## Updates

<p align="center">
  <a href="https://ra-jit.ix.dev/updates?src=readme">
    <picture>
      <source media="(prefers-color-scheme: dark)" srcset="docs/brand/signup-on-dark.svg">
      <img alt="Tell us you use blaze" src="docs/brand/signup-on-light.svg" height="36">
    </picture>
  </a>
</p>

<p align="center">For release news: Watch → Custom → Releases on GitHub.</p>

## Contributing

- Build from a checkout, run the tests, send a change: [docs/contributing.md](docs/contributing.md).
- Report a security problem: [SECURITY.md](SECURITY.md).

## Credits

blaze stands on the shoulders of giants. Thank you to the people who built:

- [rust-analyzer](https://github.com/rust-lang/rust-analyzer), whose analysis and MIR blaze compiles from
- [Cranelift](https://cranelift.dev) (Bytecode Alliance), the fast code generator
- [LLVM](https://llvm.org) and [inkwell](https://github.com/TheDan64/inkwell), for the optimised tiers
- [salsa](https://github.com/salsa-rs/salsa), the incremental engine underneath
- [object](https://github.com/gimli-rs/object), for reading and writing executables
- [Rust](https://github.com/rust-lang/rust) and its compiler crates
- [rustd](https://github.com/DioxusLabs/rustd) (DioxusLabs), whose code blaze includes

None of these projects endorses blaze.

- blaze was called ra-jit until 2026-10-24; `cargo ra-jit` still works.
- Made at [ix](https://ix.dev) for den's native app.
- **MIT or Apache-2.0.**
