---
status: plan
summary: blaze v2: diagnostics.
date: 2026-10-29
---
# blaze v2: diagnostics

See [plan/process/building.md](../process/building.md), section 18.

## Done when

- every program rustc rejects, blaze rejects, and every one rustc accepts, blaze accepts, with
  the same error codes and lint names, the same warnings, and the same primary spans, checked
  against rustc's UI tests
- the text is blaze's own, rendered for speed and for LLM readers; it need not match rustc's
  wording or layout. cargo's `--message-format=json` schema stays, because tools parse it
- ide-diagnostics is no longer needed by any build
- nothing in the lane's paths matches `rg 'unimplemented!|todo!'`; tests for all of it are written; its report says `STATUS: DONE`
