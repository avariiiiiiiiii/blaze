# Requests for speed-backend-lower (`crates/hir-ty-codegen/mod.rs`)

## From speed-backend-intrinsics: call `neon_vector` first for `llvm.aarch64.neon.*`

Where: `mod.rs`, `FunctionCx::llvm_intrinsic`, the first `if` (today
`if let Some(neon) = name.strip_prefix("aarch64.neon.") && self.neon_lanes(neon, args, dst)?`).

Exact edit:

```rust
        if let Some(neon) = name.strip_prefix("aarch64.neon.")
            && (self.neon_vector(neon, args, dst)? || self.neon_lanes(neon, args, dst)?)
        {
            return Ok(());
        }
```

New signature, in `neon.rs` (mine):
`pub(super) fn neon_vector(&mut self, name: &str, args: &[Operand], dst: Value) -> Result<bool>`,
`name` as `neon_lanes` gets it (`"tbl1.v16i8"`). It returns false off AArch64 and for any name
or shape it does not lower, so `neon_lanes` and `neon::lower` keep everything else.

Why: it lowers the 16-byte `tbl1`, `tbx1`, `umaxp`/`smaxp`/`uminp`/`sminp`, `sqadd`/`uqadd`/
`sqsub`/`uqsub`, `urhadd`, `sqrdmulh`, `abs`, `uaddlp`/`saddlp` to Cranelift vector ops
(`swizzle` is `tbl`; `uzp1`+`uzp2`+`umax` for `umaxp`; `uqadd`, `urhadd`, `sqrdmulh`, `abs`,
`uaddlp` one instruction each). Today `tbl1` and the pairwise ones are 16 to 32 scalar loads,
selects and stores (`neon_lanes`), and the rest a call of an assembled stub function with its
operands through memory (`neon::lower` + `neon_call`). `vqtbl1q_u8` and `vpmaxq_u8` are in
aho-corasick's Teddy inner loop on AArch64. Bit-identical: integer ops, each the instruction's
definition (tests in `neon.rs`). It asks for no symbol, libcall or assembly, so the refs walk's
list stays a superset.

> forwarded to speed: between speed's children.

## From speed-backend-intrinsics: call `x86_vector` first for those x86 names

Where: the same place, right after the NEON `if` in `llvm_intrinsic`:

```rust
        if self.x86_vector(name, args, dst)? {
            return Ok(());
        }
```

New signature, in `x86.rs` (mine): `pub(super) fn x86_vector(&mut self, name: &str, args:
&[Operand], dst: Value) -> Result<bool>`, `name` the whole `x86.sse.min.ps`. False off x86-64
and for any other name. It takes `x86.{sse,sse2,avx}.{min,max}.{ps,pd}[.256]`, the
`packsswb`/`packuswb`/`packssdw`/`packusdw` of SSE2, SSE4.1 and AVX2, `pmadd.wd` and
`pavg.{b,w}` of SSE2 and AVX2 (not the avx512 ones), which today are your scalar arms (about
17483, 17532, 17619 and `x86_lanewise`). Each 16 bytes becomes 2 loads, the one instruction
(`minps`, `packsswb`, `pmaddwd`, `pavgb`) and a store; the test
`x86::tests::each_vector_is_the_instruction_of_sse2` compiles each with Cranelift's x64 ISA of
SSE2 alone and checks it. Bit-identical to your arms (same instruction definitions). Your arms
then only see the avx512 names; keep them for those.

## From speed-backend-intrinsics: FYI, `wasm_simd_intrinsic` now runs on every target

No edit needed. `FunctionCx::wasm_simd_intrinsic` (called first in `intrinsic`) now lowers the
generic `simd_*` intrinsics with Cranelift vectors on AArch64 (NEON) and x86-64 (SSE2 only),
not just wasm: `simd_eq`/`lt`/..., `and`/`or`/`xor`, `add`/`sub`/`mul`, shifts by a splat,
`select`, `splat`, `shuffle`, `cast`, `bitmask`, and on NEON `simd_reduce_*`. So the lane-by-
lane arms for these in `intrinsic` now see only what `admits` refuses (x86-64 ops past SSE2,
f16 lanes, non-16-byte shapes). The name is unchanged so no caller moves; if you rename it
(`simd_vector_intrinsic`), tell me and I'll rename the definition.

> forwarded to speed: between speed's children.

## From speed-backend-intrinsics: x86 `pshufb` stays scalar until the ISA has SSSE3

`llvm.x86.ssse3.pshuf.b.128` / `avx2.pshuf.b` (Teddy's `_mm_shuffle_epi8` on x86-64) are a
16/32-lane scalar loop in `llvm_intrinsic`. Cranelift's `x86_pshufb` needs `has_ssse3`, which
`Target::isa` never sets (one ISA per process). The fix is a per-function ISA from
`#[target_feature]` (filed in `v2/requests/speed.md` under speed-backend-intrinsics); once it
exists the arm becomes `x86_pshufb(table, band(indices, 0x8f))`. Nothing for you to do now.

> forwarded to speed: between speed's children.
