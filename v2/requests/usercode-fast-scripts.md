# Requests from lane usercode-fast-scripts

## 1. blaze-cargo test: `Env::vars` is a sorted vector (required)

- What: replace
  `assert_eq!(env.vars.get(&OsString::from("DEP_Z_INCLUDE")), Some(&OsString::from(format!("{out_dir}/include"))));`
  with
  `assert_eq!(env.get("DEP_Z_INCLUDE"), Some(OsStr::new(&format!("{out_dir}/include"))));`
  (and `use std::ffi::OsStr;` if the file lacks it).
- Where: `crates/blaze-cargo/src/serve/unit/tests.rs:202`.
- Why: `blaze_build_script::Env::vars` is now `Vec<(OsString, OsString)>`, sorted by name and
  each name once (it was a `BTreeMap`), so the environment is one presized vector sorted
  once. `Env::get` is the binary search. Every other caller compiles unchanged:
  `.vars.iter()`, `.vars.into_iter().collect()` and `Env::apply` read the same order.

> forwarded to cargo (told by tmux).

## 2. blaze-cargo: judge a run by borrowing (optional, speed)

- What: in `judge` (around line 289), replace building a `blaze_build_script::run::Ran` from
  `value.stdout.clone()` and `value.stderr.clone()` and calling `ran.finish(unit, &cx)` with
  `blaze_build_script::run::judge(unit, &cx, status, &value.stdout, &value.stderr)`, and write
  cargo's files of the run (`run::files(unit, stdout, stderr)`: `output`/`stdout` dated when
  the run began, `stderr`, `root-output`) through `Effect::Write` once `judge` has passed the
  status and logged-error checks, as `finish` does.
- Where: `crates/blaze-cargo/src/serve/unit/script.rs`, `judge`.
- Why: `finish` copies nothing but both clones are whole outputs, and it writes the files with
  `std::fs` on the daemon's thread, which blocks it.

> forwarded to cargo (told by tmux).

## 3. blaze-cargo: begin an elided run without cloning its output (optional, speed)

- What: in `ScriptRun::start` (around line 608), call `blaze_build_script::run::begin(&start.unit)`
  for the time the run began, rather than cloning the inert run's stdout into
  `run::elided(&start.unit, stdout)` and keeping only its `.began`;
  better, make `OUT_DIR` and the run directory and write `run::invoked(unit)` (a path and its
  bytes) through effects, and take the time from the `Stat` of that file.
- Where: `crates/blaze-cargo/src/serve/unit/script.rs`, `ScriptRun::start`.
- Why: `elided` only wraps `begin` and the stdout given; the clone is thrown away. `begin`
  calls `create_dir_all` and `fs::write` on the daemon's thread.

> forwarded to cargo (told by tmux).

## 4. blaze-usercode: read what reruns a script without parsing everything (optional, speed)

- What: in `cargo_rule`, replace `match output::parse(&exit.stdout, &cx)` with
  `match blaze_build_script::watched_in(&exit.stdout, &cx, dir)`, its `Ok` arm becoming
  `Ok(watched) => CargoRule::of(watched)`. It returns the same `Watched`, or the same
  `Error` as `parse`.
- Where: `crates/blaze-usercode/src/script.rs`, `cargo_rule` (around line 111).
- Why: `watched_in` reads the output in the same pass and keeps only the `rerun-if-*` values,
  borrowed; `parse` copies every directive into an `Output` that is then dropped.

> forwarded to usercode (told by tmux).

## 5. docs/README.md: index the new plan doc (required)

- What: run `python3 tools/docs.py write` so the index lists
  `docs/v2/plan/lanes/usercode/scripts.md`.
- Where: `docs/README.md` (generated).
- Why: the doc is new, and `tools/docs.py check` fails on a stale index.

> forwarded to the coordinator: `docs/` is its; listed in requests' report (run `python3 tools/docs.py write`).
