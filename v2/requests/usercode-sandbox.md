# Requests from lane usercode-sandbox (child of usercode)

## 1. `blaze-cargo` depends on `blaze-usercode`

- What: add `blaze-usercode.workspace = true` to `crates/blaze-cargo/Cargo.toml`'s
  `[dependencies]`.
- Where: `crates/blaze-cargo/Cargo.toml`. `blaze-usercode` is already a workspace dependency
  (root `Cargo.toml`), and depends only on `blake3`, `blaze-build-script`, `blaze-cas`,
  `blaze-core`, `blaze-ids`, `blaze-items`, `blaze-query` — none of which depend on
  `blaze-cargo`, so this adds no cycle.
- Why: the v2 sandbox launcher (`blaze sandbox --v2 SPEC`) parses
  `blaze_usercode::confine::Spec`, appends `blaze_usercode::trace::Line`s to the live record,
  and writes `blaze_usercode::trace::encode(&Report)` with
  `blaze_usercode::trace::assemble` over `blaze_usercode::inputs::Disk`.

## 2. The Linux interposer's `build.rs`

- What: in `crates/blaze-cargo/build.rs`, after `sandbox_library()`, build
  `src/sandbox/linux_interpose.c` on Linux into `OUT_DIR/libblaze_sandbox_interpose.so` and
  export its path and content hash, mirroring how `sandbox_library()` builds
  `macos_interpose.c`. The exact addition:

  ```rust
  // in main(), after sandbox_library();
  linux_interpose();
  ```

  and the function:

  ```rust
  /// On Linux, the library every process of a sandbox loads through `LD_PRELOAD`
  /// (`src/sandbox/linux_interpose.c`): it interposes `getenv`, `secure_getenv`, the realtime
  /// clocks and `getrandom`, and says its nonce as it loads, so the supervisor can see
  /// environment and clock reads a seccomp filter cannot. Built with the system C compiler
  /// (`CC` when set), carried in the executable by `src/sandbox/linux.rs`.
  fn linux_interpose() {
      let source = "src/sandbox/linux_interpose.c";
      println!("cargo:rerun-if-changed={source}");
      println!("cargo:rerun-if-env-changed=CC");
      if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("linux") {
          return;
      }
      let out = PathBuf::from(std::env::var_os("OUT_DIR").unwrap())
          .join("libblaze_sandbox_interpose.so");
      let cc = std::env::var("CC").unwrap_or_else(|_| "cc".to_owned());
      let status = std::process::Command::new(&cc)
          .args(["-shared", "-fPIC", "-O2", "-Wall"])
          .arg("-o")
          .arg(&out)
          .arg(source)
          // After the source: under `--as-needed` a library named before the object it
          // serves is dropped.
          .arg("-ldl")
          .status()
          .unwrap_or_else(|error| panic!("{cc} did not run to build {source}: {error}"));
      assert!(status.success(), "{cc} could not build {source}");
      let mut hash = Sha256::new();
      hash.update(&std::fs::read(&out).unwrap());
      let hex: String =
          hash.finish().iter().take(12).map(|byte| format!("{byte:02x}")).collect();
      println!("cargo:rustc-env=BLAZE_SANDBOX_INTERPOSE={hex}");
  }
  ```

  `linux.rs` reads it with
  `include_bytes!(concat!(env!("OUT_DIR"), "/libblaze_sandbox_interpose.so"))` and
  `env!("BLAZE_SANDBOX_INTERPOSE")`, guarded by `#[cfg(target_os = "linux")]`.
- Why: the seccomp supervisor cannot see `clock_gettime` (vDSO) or `getenv` by name; the
  preloaded library reports them, which keeps a run's traced inputs sound (no untraceable
  clock read missed, no environment variable read unseen).

## 3. `windows-sys` features (from child `usercode-sandbox-windows`)

- The Windows launcher needs features beyond those already on the `windows-sys` line in
  `crates/blaze-cargo/Cargo.toml`. The child lane `usercode-sandbox-windows` appends the
  exact feature list to `v2/requests/usercode-sandbox-windows.md`; apply that too.
