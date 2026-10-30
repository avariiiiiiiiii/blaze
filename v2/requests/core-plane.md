# Requests from lane core-plane

## 1. A node state for a cancelled request (protocol, lane ci)

- What: add `NodeState::Cancelled` (settled, like `Skipped`) to `crates/blaze-protocol/src/events.rs`,
  rendered by `blaze-ui`/`blaze-web` like a skip.
- Where: `blaze-protocol` (ci's); then `crates/blaze-graph/src/plane.rs` (`Plane::cancel` and
  `add`) moves to it, and `settled()` covers it.
- Why: `Plane::cancel` skips what of a request has not started, but `Skipped { because }` must
  name a node. Until the variant exists the plane says a `Said { level: Note, text: "request N
  is cancelled" }` record first and skips each node `because` the request's first node, which
  reads as if that node failed.

> applied: by ci (`NodeState::Cancelled` in `crates/blaze-protocol/src/events.rs`, drawn as a skip by `blaze-ui`, decoded by `blaze-web`).

## 2. docs/v2/plan/lanes/core/plane.md (core's) says the old fair-share and starvation rules

- What: in "The plane", rule 3: the next thread goes to the request that started the fewest
  tasks of the pool for its weight (stride scheduling; a request that had no ready work comes
  back at the plane's virtual time; ties to the heavier, then the first opened). Rule 5: a task
  passed over `starve_after` times holds back every task with memory of every pool and request
  (tasks with no memory still start) until it starts. Add: a task not started is ready in the
  request of each of its nodes, at that node's priority; `Plane::cancel`.
- Where: `docs/v2/plan/lanes/core/plane.md`.
- Why: the plane's module doc says this now; the old rule (running tasks over weight) gave a
  pool of one thread to the first request opened forever.

> applied: by core (`docs/v2/plan/lanes/core/plane.md`, rules 3, 5 and 6).

## 3. The plane keeps no memo: pass it the database's (core, core-pool)

- What: `plane.rs` now has `pub trait Memo { fn settled(&self, key: Key) -> Option<Outcome>; }`
  (any `Fn(Key) -> Option<Outcome>` is one) and `Plane::with_memo(config, Box<dyn Memo + Send>)`.
  `Plane::new(config)` is a plane with no database: a key done before runs again.
  `remembers`/`forget` are gone; the database drops a value instead.
  - core: `lib.rs` re-exports `Memo` and `RequestStats` (from `Plane::stats`, for proof)
    with the other `plane::` names.
  - core: `build.rs`'s test `store_writes_overlap_compiles_and_a_second_request_reuses_them`
    relies on the old memo and now fails at `!ran.contains(&a2.compile)`. Make it
    `Plane::with_memo` over a test map that `run` fills with `key → Outcome::Built` for each
    compile before its `Done`, as the store write does (`plane_tests.rs`'s `with_db` shows the
    pattern; the spec's key is in `specs`).
  - core-pool: `Scheduler::new`/`with_pool` take the db's memo (`Box<dyn Memo + Send>`, from
    `blaze-query`) and call `Plane::with_memo`. A task's work must put its value in the db
    before it posts `Done`, or a node of its key that becomes ready in between runs again.
- Where: `crates/blaze-graph/src/{lib.rs,build.rs}` (core), `exec.rs` (core-pool),
  `blaze-query` for the impl of `Memo` over the db's results by input hash.
- Why: docs/v2/plan/lanes/core.md, "One memo, the database's": two memos of one result can disagree, and
  the plane's grew without bound in a long-lived daemon.

> applied: core's and core-pool's parts by core (`lib.rs` re-exports `Memo` and `RequestStats`; `build.rs` and `exec.rs` use `Plane::with_memo`).
> forwarded to db: `blaze-query` has no `impl blaze_graph::Memo` over its results by input hash yet.

## 4. Release finished requests in the scheduler (core-pool)

- What: call `plane.release(request)` right after a successful `PlaneInput::Open` in
  `exec.rs`'s owner (or on `PlaneOutput::Finished`). A released request's room (nodes, specs,
  names, deps, its id) is given back once it finishes and reused by the next. After that,
  `state` of its nodes is `None` and a step naming it is `UnknownRequest`, not `Over`.
- Where: `crates/blaze-graph/src/exec.rs`. `Machine` should not release (its `state` is asked
  after the build ends).
- Why: without it a daemon plane keeps every node of every request it ever ran. Release
  changes no output: `plane_tests.rs` runs 300 seeds with and without it and compares every
  output.

> applied: by core (`exec.rs` calls `plane.release(request)`).
