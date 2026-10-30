# Requests from usercode-scripts

## 1. #804: compile build scripts in the daemon, not in a fresh blaze process

- What: a build script's compile today is `compile_time.rs`'s `Driver::script` →
  `Driver::compile` → `own_script::build_described` → `own_script::built` → `own_script::build`,
  which writes a `rust-project.json` and runs `Command::new(std::env::current_exe())` with
  `--crate build_script_build --out … --exe … --dep-info …` (`own_script.rs` near line 596).
  That child loads the sysroot again, about 2 s per script (#804). In v2 the compile is a
  codegen request of the daemon (the daemon already has std loaded), returning the script's
  executable as a file the store places; `Driver::script`'s `Ok(exe)` then names that file.
- Where: `crates/blaze-cargo/src/cargo/compile_time.rs` (`script`, `compile`, the `fresh`
  stamp beside the executable) and `crates/blaze-cargo/src/cargo/own_script.rs`
  (`build_described`, `built`, `build`). Coordinator's paths; codegen owns the compile itself.
- What `blaze-build-script` needs: nothing. Nothing in it assumes a script made by cargo or
  rustc: `Unit::script` is any executable path, `run::run` runs whatever command it is given,
  `run::run_dir` and `run::printed` derive from `OUT_DIR` (cargo's two layouts of the build
  directory), never from where the executable is, and no `-C metadata` hash or
  `build-script-build` file name is read anywhere outside the harness's stand-in mode in
  `main.rs`. The executable path appears only in the failure messages, as cargo's do.
- Also for the coordinator, where the compile happens: decide elision before compiling
  (`blaze_build_script::elide(root, files)`, then `elide::flags_allow` on the flags the
  script would be compiled with), so an elided script is never compiled at all; the run is
  then `run::elided(unit, stdout)` and `Ran::finish` as for any run.
- Why: #804 (cold load spends its time reloading the sysroot per script) and #1296 (scripts
  that need no compile).
