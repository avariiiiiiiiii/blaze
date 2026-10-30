---
status: plan
summary: blaze v2: the rules every part keeps.
date: 2026-10-29
---
# Principles

Non-goals: changing what Rust means (superset-of-rust holds); running rustc for anything
(no-rustc holds); a fallback when v2 can't do something (no-fallback holds: it stops and says
what and where); an IDE. The only interface is the agent API (CBOR over a socket or HTTP),
shaped for LLMs; the IDE crates (`ide-db`, `hir-ide`, `ide-diagnostics` and the rest) are not
a constraint, and what the compiler needs from them (diagnostics) moves into the compiler.

**Everything is ours, at every level, for the best performance possible.** Nothing is fixed:
not a crate, an interface, a data structure, a vendored dependency (salsa, cranelift, LLVM's
bindings, the trait solver, std's build), the toolchain, the kernel interfaces we use, the
file formats, the CI or the machines. If changing it makes blaze faster and keeps Rust's
semantics, change it. Any dependency v2 needs to change is in the workspace and changed in
place: salsa (`vendor/salsa`), cranelift, llvm-sys, `rustc_type_ir` and the trait solver are
already there; anything else is vendored the moment it's in the way.

**Deterministic**: the same inputs give the same bytes, whatever the scheduling, the core count
or the machine. The invalidation oracle (section 11) depends on it.

**Profile first**: before the targets bind, the coordinator measures where today's 0.27 s body
edit and 35 s start go, and the targets are set from that.

Lane agents write code and tests and read; they don't run anything expensive. Allowed: reading
files, `rg`, `ls`, `wc`, read-only git (`diff`, `log`, `show`, `status`, `grep`, `ls-tree`),
`gh issue view` and `gh pr view`, and starting child agents. Not allowed: any build, check or test
(`blaze`, `cargo`, `rustc`, clippy, `nix build`, `nix develop`), harnesses, benchmarks, ix VMs,
downloads, long loops, and git commands that change state. The coordinator alone builds and tests,
all at once, in an ix VM.

**Finished code only.** Every lane writes its part end to end: every body, wired into the path a
real build takes, with no `unimplemented!`, `todo!`, stub or deferred piece. A lane isn't done
while any remain in its paths. v2 lands complete or not at all.
