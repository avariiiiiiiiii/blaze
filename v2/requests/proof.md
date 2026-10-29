# Requests from proof

## 1. blaze-api: what a request did (for the oracle's over-invalidation and the budgets)

- What: a `Response::Work(Work)` sent before `Response::Done` on every `Build`, `Test` and
  `Run`, with `Work { reran: u64, green: u64, compiled: u64, found: u64, link: LinkKind,
  critical_path_us: u64 }`: queries executed, queries verified green, functions compiled
  (code generated, not found in the store), functions found in the store, how the image was
  linked (`Patched` in place or `Placed` whole), and the plane's critical path of the request.
  The CLI prints it under `--message-format=json` as one line
  `{"reason": "blaze-work", "reran": N, "green": N, "compiled": N, "found": N, "link":
  "patched"|"placed", "critical_path_us": N}`.
- Where: `crates/blaze-api/src/lib.rs` (ci), the plane's request accounting (core), the CLI.
- Why: section 1's budgets ("a format or comment edit compiles nothing", "a signature edit
  compiles its callers") and docs/v2/proof.md's "over-invalidation is measured and budgeted"
  need the count per request. `harness/v2/engine.py`'s `V2.mark`/`work_since` and
  `critical_path_s` raise `engine.Missing` until this exists, which fails every v2 run of
  oracle.py and budgets.py naming it.

## 2. blaze-api: codegen inputs of a view

- What: `Request::CodegenInputs { view: ViewRef, targets: Targets }` answered with
  `Response::CodegenInputs(Vec<(InstanceKey, CodeHash)>)` (sorted by key), every instance the
  view's last build compiled or found, with the hash its code was generated from. CLI: `blaze
  codegen-inputs --format=json` printing `{"<instance key hex>": "<code hash hex>", ...}`.
- Where: `crates/blaze-api` (ci), answered by the codegen queries (codegen).
- Why: docs/v2/proof.md: "the incremental result must equal a from-scratch build, bit for bit
  for the codegen inputs". `V2.codegen_inputs` raises `engine.Missing` until then.

## 3. blaze-api: a view's trace

- What: `Request::Trace { view: ViewRef, targets: Targets }` answered with
  `Response::Trace(Vec<(QueryKey, Fingerprint, Vec<Edge>, bool)>)`: every result the view's
  last build ran or kept, its fingerprint, its edges in read order, and whether that build ran
  it. CLI: `blaze trace --out PATH` writing it as the deterministic CBOR of
  `blaze_v2_proof::Trace` (`crates/blaze-v2-proof`), which the `blaze-v2-proof` binary
  compares.
- Where: `crates/blaze-api` (ci), from `blaze_query::Database::memo` (db).
- Why: the oracle names the query that went stale and the read it missed (a fact missing from
  docs/v2/items.md section 6.2), not only the wrong program. `V2.trace` raises
  `engine.Missing` until then.

## 4. pyproject.toml: lint and type-check harness/v2

- What: add `"harness/v2"` to both `include` lists (ruff and pyright) and
  `"harness/v2/*.py" = ["E402"]` beside the other per-file ignores (the scripts put
  `harness/` on `sys.path` before importing `engine`, `report` and `blaze_cli`).
- Where: `pyproject.toml` (coordinator).
- Why: the scripts are written pyright-strict; nothing checks them until they are included.

## 5. Cargo.lock: blaze-v2-proof

- What: an entry for the path crate `blaze-v2-proof` (dependencies `blaze-ids`,
  `blaze-protocol`, `blaze-query`, `serde`, `serde_json`, all already workspace dependencies).
  It is a member through `crates/*`; no root `Cargo.toml` change.
- Where: `Cargo.lock` (coordinator).
- Why: its first build would add it anyway; written down so the lock is not a surprise.

## 6. docs/harness.md: the v2 proof

- What: a row for `harness/v2/proof.py` (one entry, one report: oracle, corpus, rejections,
  determinism, tight memory, budgets) and one per script, each run as `uv run --frozen python
  harness/v2/<script>.py '<json>'` with `"engine": {"kind": "v1"|"v2", "blaze": PATH}`;
  `docs/v2/proof.md` can link it.
- Where: `docs/harness.md`, `docs/v2/proof.md` (coordinator).
- Why: docs-say-what-they-are; the harnesses are otherwise undocumented.

---
Routed or applied by the coordinator, 16:54.
