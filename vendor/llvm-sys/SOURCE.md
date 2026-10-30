# llvm-sys

- Upstream: https://gitlab.com/taricorp/llvm-sys.rs, crates.io `llvm-sys` 231.0.1
  (sha256 21cdb31b2047b57121a31a911af86e4bacb93d165843d27cea1e14e071ac2b4c, git d17ede3188).
- Licence: MIT (LICENSE).
- Left out: examples, scripts, CI files, Cargo.lock.

Changes, each marked `blaze:` in build.rs:

1. Static LLVM archives are linked `static:-bundle`: named to the final link, not copied into the
   rlib.
2. A static LLVM on Linux links libstdc++ statically, from the C++ compiler's own `libstdc++.a`
   (`$CXX -print-file-name`), so a release binary needs no C++ runtime where it runs.
3. `wrappers/passes.cpp` (built with `llvm-config --cxxflags`, declared in
   `src/transforms/pass_builder.rs` as `LLVMBlaze*Passes`): the pass builder, analysis managers,
   standard instrumentation and parsed pipelines `LLVMRunPasses` makes on every call, kept per
   target machine instead. blaze runs passes over one small module per function.
   `cargo:rerun-if-changed` is printed for both wrappers.

Why here and not in hir-ty-codegen's build.rs: rustc names a crate's native libraries right after
that crate's rlib, and GNU ld takes from an archive only what earlier inputs asked for. Libraries
named by hir-ty-codegen come before inkwell's rlib, which calls LLVM, so ld.bfd left inkwell's
calls undefined. llvm-sys sits below inkwell, so its libraries come after every caller.
