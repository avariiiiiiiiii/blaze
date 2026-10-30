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

## 2. docs/v2/plane.md (core's) says the old fair-share and starvation rules

- What: in "The plane", rule 3: the next thread goes to the request that started the fewest
  tasks of the pool for its weight (stride scheduling; a request that had no ready work comes
  back at the plane's virtual time; ties to the heavier, then the first opened). Rule 5: a task
  passed over `starve_after` times holds back every task with memory of every pool and request
  (tasks with no memory still start) until it starts. Add: a task not started is ready in the
  request of each of its nodes, at that node's priority; `Plane::cancel`.
- Where: `docs/v2/plane.md`.
- Why: the plane's module doc says this now; the old rule (running tasks over weight) gave a
  pool of one thread to the first request opened forever.
