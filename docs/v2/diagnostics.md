---
status: plan
summary: blaze v2: diagnostics.
date: 2026-10-29
---
# blaze v2: diagnostics

See [plan/building.md](plan/building.md), section 18.

## Done when

- errors and warnings match rustc's text and JSON for rustc's UI tests
- every rustc rejection is a blaze rejection with the same code
- ide-diagnostics is no longer needed by any build
- nothing in the lane's paths matches `rg 'unimplemented!|todo!'`; tests for all of it are written; its report says `STATUS: DONE`
