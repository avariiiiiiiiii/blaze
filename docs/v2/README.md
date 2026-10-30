---
status: plan
summary: blaze v2: the plan, as an index. Start here; each line links to the detail.
date: 2026-10-29
---
# blaze v2

blaze remade for speed: one warm VM per project where every branch, PR and CI job is a view;
results keyed by content and invalidated by the facts they read; per-function work on every
core; one zero-copy store; CI on the same VM. It lands as one pull request from `blaze-v2`
when everything passes with v2 as the only path.

## Read in this order

1. **[plan/principles.md](plan/principles.md)**: the rules (everything is ours at every level;
   deterministic; finished code only; what agents may run).
2. **[plan/targets.md](plan/targets.md)**: the numbers v2 must hit.
3. **[plan/model.md](plan/model.md)**: one warm VM, branches as views.
4. **Your lane's file** (below), then **[crates.md](crates.md)**: which crate holds which types
   and who owns which paths.
5. As your work needs them: [plan/data.md](plan/data.md) (data-oriented layout, compression),
   [plan/hardware.md](plan/hardware.md) (SIMD, huge pages, io_uring, no GPU),
   [plan/building.md](plan/building.md) (lanes, integration, the freeze),
   [plan/risks.md](plan/risks.md), [decisions.md](decisions.md), [glossary.md](glossary.md),
   [status.md](status.md).

## Lanes

| lane | what |
|---|---|
| [core](core.md) | sans-IO core, drivers (io_uring, kqueue, IOCP, simulation), scheduler, memory admission |
| [vfs](vfs.md) | Merkle VFS, views, file watching |
| [format](format.md) | IDs, zero-copy columns, flat syntax, HIR layout, span tables |
| [items](items.md) | item hashes and facts: minimal invalidation |
| [db](db.md) | our own query database, types, trait solver, borrow check |
| [store](store.md) | the content-addressed store and its tiers |
| [codegen](codegen.md) | per-function codegen, instances, MIR |
| [link](link.md) | parallel in-place linker, live patching |
| [usercode](usercode.md) | build scripts, proc macros, tests, sandboxes |
| [ci](ci.md) | the agent API and the VM as CI |
| [proof](proof.md) | the oracle, the rustc differential corpus, budgets |
| [cargo](cargo.md) | workspace load, unit graph, CLI, std (plan/building.md) |
| [diagnostics](diagnostics.md) | rustc-exact errors and warnings (plan/building.md) |

Lane design notes live beside the lane files (`<lane>-notes.md`); `plane.md` holds the
scheduler's first notes.

## Where the old section numbers went

| section | now |
|---|---|
| 1 targets and principles | plan/targets.md, plan/principles.md |
| 2 machine model | plan/model.md |
| 3-11 | the lane files (3, 4 core; 5 format; 6 items, 6.5 usercode; 7 db; 8 store; 9 codegen and link; 10 ci; 11 proof) |
| 12, 18, 19 lanes, added lanes, integration | plan/building.md |
| 13 issues | each lane file's "Issues this lane takes in" |
| 14 risks, left out | plan/risks.md |
| 15 what agents may run | plan/principles.md |
| 16 hardware | plan/hardware.md |
| 17 data-oriented | plan/data.md |
