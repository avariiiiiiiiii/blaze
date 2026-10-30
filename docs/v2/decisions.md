---
status: plan
summary: blaze v2: decisions made, so no lane reopens them. Newest last.
date: 2026-10-29
---
# Decisions

Each line is settled. A lane that thinks one is wrong says why in its request file; it does not
work around it.

| date | decision | why |
|---|---|---|
| 10-29 | One warm VM per project (or tenant); branches, PRs and CI jobs are views, not VMs or forks | one copy of every result, shared by every view; forks only for tenants, scale-out, resume |
| 10-29 | One big integration branch, one PR; testing all at once by the coordinator | Andrew's call; the oracle and corpus exist before integration so failures come attributed |
| 10-29 | Agents run nothing expensive; the coordinator type-checks at hourly checkpoints | keeps agents fast; catches errors before the all-at-once run |
| 10-29 | Finished code only: no `unimplemented!`, `todo!` or deferred parts | v2 lands complete or not at all |
| 10-29 | Everything is ours at every level: any crate, vendored dependency, format or interface changes when it makes blaze faster and keeps Rust's semantics | best performance possible |
| 10-29 | Our own query database, from `vendor/salsa`; no longer kept close to upstream | input-hash keys, fingerprints on edges, views, zero-copy image need it |
| 10-29 | The database is `Sync` and shared by reference; no per-thread handles; all parallel work is plane tasks | salsa's per-thread handles and threads spawned by queries bypass the scheduler |
| 10-29 | One memo of results, the database's; the plane schedules and joins equal keys but remembers nothing | two memos can disagree |
| 10-29 | One arena of views, `blaze_vfs::ViewId` | a view has one id everywhere |
| 10-29 | No IDE; the agent API is the only interface; the CLI is its client with cargo's behavior | LLMs are the users besides people at a terminal |
| 10-29 | Flat syntax everywhere, rowan removed | no IDE to keep |
| 10-29 | Data-oriented everywhere: tag plus word, CSR lists, hot and cold split, bitsets, size budgets | memory, cache misses, SIMD, compression |
| 10-29 | Hardware acceleration chosen at run time, every path bit-identical; no GPU | determinism; the work is irregular and each edit is tiny |
| 10-29 | Compression by tier: hot plain and mapped, warm bit-packed in place, cold per-column codecs plus zstd | zero-copy where it is read, small where it is not |
| 10-29 | Main frozen for the crates v2 replaces (#1540): bug fixes only, merged into `blaze-v2` at checkpoints | the branch can't rot the way blaze-next did |
| 10-29 | `rustc_type_ir` and the trait solver belong to db | types and the solver become `u32`-indexed tables |
