# Requests from lane core-macos

## 1. Stop the footprint watch before the reap (`process.rs`)

- What: `Finish::exited` stops `Limit::Footprint(watch)` (`watch.stop()`), but every driver
  calls it after `process::reap(pid)`. Between the reap and that `stop`, the watch thread may
  still read the footprint and `SIGKILL` the pid, which by then can belong to another process.
  `footprint::Watch::stop` must come before the reap (its doc says so; the thread checks nothing
  once `stop` returns).
- Where: `crates/blaze-core/src/process.rs`. Proposed: `Finish` does the reap itself, so no
  driver can get the order wrong:

  ```rust
  /// Once `pid` has exited (not yet reaped): stops what holds its memory, reaps it, and says
  /// how it ended.
  pub(crate) fn exited(self, pid: libc::pid_t, stdout: Vec<u8>, stderr: Vec<u8>) -> io::Result<Exit>
  ```

  stopping the footprint watch (and reading the cgroup's peak) first, then `reap(pid)`, then the
  rest as now. Callers to change with it: `kqueue.rs` `finish_spawn` (replace
  `process::reap(pid)...and_then(|(status, rusage)| finish.exited(status, &rusage, out, err))`
  with `finish.exited(pid, out, err)`) and `abandon` (`_ = finish.exited(pid, Vec::new(),
  Vec::new())` after `process::kill(pid)`), and the threads and io_uring drivers. Tell me and I
  change `kqueue.rs` myself, or change those two lines with it.
- Why: the brief's rule "the kill must never hit a reaped pid". `footprint.rs` already returns
  without a kill when the read says `ESRCH`, which narrows the window but cannot close it.

## 2. For your review: `limit_self` and `EINVAL` (no change asked)

`footprint::limit_self` sets `RLIMIT_DATA` and `RLIMIT_AS` (both soft and hard, clamped to the
current hard limit). macOS refuses an address-space limit below what the process already reserves
(`ulimit -v` gives "Invalid argument"; every process reserves hundreds of GB of unbacked ranges),
so an `EINVAL` from `setrlimit` is taken as "macOS does not hold this limit" and the footprint
watch holds it alone. Any other error fails the spawn. If you'd rather an `EINVAL` fail the
spawn, it's one line in `limit_self`, but then every spawn with a memory limit would fail on
macOS. `footprint::tests::limits_set_before_exec_let_a_small_program_run` and
`kqueue::tests::processes_end_with_their_output_code_and_signal` (tag 5) check that a limited
spawn runs.
