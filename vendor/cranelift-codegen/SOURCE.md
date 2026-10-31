# cranelift-codegen

- Upstream: https://github.com/bytecodealliance/wasmtime (`cranelift/codegen`), crates.io
  `cranelift-codegen` 0.137.0 (sha256 a53dbec33ff86909e44be6f0ec2d17841aa38b931013707c9fc7279f1f0e5760).
- Licence: Apache-2.0 WITH LLVM-exception (LICENSE).
- Left out: Cargo.toml.orig, Cargo.lock.

The copy is the crate unchanged except for one visibility, the whole diff against it:

```diff
--- cranelift-codegen-0.137.0/src/machinst/buffer.rs
+++ vendor/cranelift-codegen/src/machinst/buffer.rs
@@ -432 +432 @@
-    pub(crate) unwind_info: SmallVec<[(CodeOffset, UnwindInst); 8]>,
+    pub unwind_info: SmallVec<[(CodeOffset, UnwindInst); 8]>,
```

`MachBufferInner::unwind_info` was `pub` through 0.136; 0.137 made it `pub(crate)` with no
accessor, and the processed `isa::unwind::systemv::UnwindInfo` and the gimli FDE it makes keep
their rows private. hir-ty-codegen builds each frame's `.eh_frame` rows from these instructions
(`SystemV::rows` in `crates/hir-ty-codegen/unwind.rs`), and blaze-stencil's x64 tests read them.

Drop when upstream exposes unwind_info: then delete this directory and its `[patch]` line.

Every crate that depends on `cranelift-codegen` (frontend, module, object, interpreter, reader) is
pinned in the root to this version, so `[patch]` covers each and the build holds one copy.
