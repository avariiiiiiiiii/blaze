# Requests of requests-outside (child of `requests`): usercode.md section 11

What blaze-vfs now gives (`crates/blaze-vfs/src/outside.rs`, notes in
`docs/v2/plan/lanes/vfs/outside.md`), re-exported from the crate root:

- `blaze_vfs::generation() -> u64`: one acquire load.
- `blaze_vfs::unchanged_since(path: &Path, generation: u64) -> bool`: no lock, no allocation;
  true only when a watched directory at or above `path` was confirmed before `generation` was
  read, heard no change after it, and the daemon's last flush round is in. False means "stat it".
- `blaze_vfs::want(dir: &Path)`: watch `dir` from the daemon's next round (once per directory;
  repeat calls are a lock-free lookup).
- `blaze_vfs::Outside` (the sans-IO driver): `Outside::new(blaze_vfs::outside::dirs(), "/", tags)`,
  `owns(tag)`, `complete(completion, &mut effects)`, `flush(&mut effects)`, `flushed()`;
  `blaze_vfs::outside::TAGS` (`1 << 32`) tags from `tags`.
- `blaze_core::Effect::Flush { watch, tag }` / `Done::Flushed` in every driver.

## 1. usercode: `inputs::Files::file` asks the table before it stats

Where: `crates/blaze-usercode/src/inputs.rs` (`Files`, `Files::file`), and
`crates/blaze-usercode/Cargo.toml` (`blaze-vfs.workspace = true`; already a workspace
dependency).

What:
- Keep beside the stamp-keyed table a path-keyed answer: `(generation, answer)`, where
  `generation` is `blaze_vfs::generation()` read before the `stat` that made the answer
  (`None`, `DIRECTORY`, `SPECIAL` or the hash). Append-only, like `Sharded` today; a new answer
  is a new entry under a new generation, never a change to an old one.
- `Files::file(path)`: if the path's last answer has `blaze_vfs::unchanged_since(path, g)`,
  return it with no syscall. Otherwise do what it does now, reading the generation first, and
  record the answer under that generation.
- Record an answer as one the table may vouch for only when `path` is canonical (no symlink in
  it, no `.` or `..`): `std::fs::canonicalize(path)? == path`, checked on the miss (cold) path
  only. A watch hears nothing of a symlink's target outside it, nor of a symlink above it being
  repointed. A path that isn't canonical is stat'ed at every check, as now.
- On each miss, `blaze_vfs::want(parent)` for the canonical path's parent (the brief's "each
  traced path's parent"), and `want` the toolchain's sysroot, the C compiler's directory and
  `/usr/include` once where usercode knows them.
- The same for `Now::names` / `dir` if wanted: the directory's own path, the same rule.

Why: usercode.md section 11. A C build script's hundreds of header stats on a cache hit
become two atomic loads per ancestor each.

## 2. cargo: the daemon drives `Outside` and runs one flush round per request

Where: `crates/blaze-cargo/src/serve/daemon.rs` (its tag ranges, its loop's routing of
completions, and where a request waits for its checkout's cookie in `serve/heads.rs`).

What:
- Own one `blaze_vfs::Outside::new(blaze_vfs::outside::dirs(), "/", OUTSIDE)` for the daemon's
  life, with a free `2^32` range for `OUTSIDE` (e.g. `7 << 32`, after `JOBS_END`, if nothing
  uses it). The driver is rooted at `/` already (`serve.rs`), so every directory is nameable.
- Route completions with `outside.owns(tag)` to `outside.complete(completion, &mut effects)`.
- For each request that will run checks of user code (build, check, test, run), call
  `outside.flush(&mut effects)` beside writing the checkout's cookie, and hold the request
  until both the cookie is heard and `outside.flushed()`. Until then `unchanged_since` answers
  false, which is sound but stats everything, so the hold is for speed, not correctness.
  `flush` also starts watches for directories wanted since the last round.
- Never drop the `Outside` while checks run: dropping it makes every directory unconfirmed.

Why: events arrive after the change that made them, so without a round per request a check
could trust a generation while the request's own edit outside a view is still queued.
