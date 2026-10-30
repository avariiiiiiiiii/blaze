# Requests from lane usercode-sandbox (child of usercode)

## 1. `blaze-cargo` depends on `blaze-usercode` (applied: it is in `crates/blaze-cargo/Cargo.toml`)

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

> applied: `crates/blaze-cargo/Cargo.toml` has `blaze-usercode.workspace = true` (line 131).

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
  /// On Linux, the library every process of a traced sandbox loads through `LD_PRELOAD`
  /// (`src/sandbox/linux_interpose.c`): it interposes `getenv`, `secure_getenv`, the realtime
  /// clocks and the `exec`/`posix_spawn` family, and says it loaded, so the supervisor sees
  /// variable and clock reads a seccomp filter cannot, and which programs ran without it.
  /// Built with the system C compiler (`CC` when set), carried in the executable by
  /// `src/sandbox/linux.rs`.
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

> forwarded to cargo: `crates/blaze-cargo/build.rs` is lane cargo's and has no `linux_interpose` yet.

## 3. `windows-sys` features (from child `usercode-sandbox-windows`)

- The Windows launcher needs features beyond those already on the `windows-sys` line in
  `crates/blaze-cargo/Cargo.toml`. The child lane `usercode-sandbox-windows` appends the
  exact feature list to `v2/requests/usercode-sandbox-windows.md`; apply that too.

> forwarded to cargo: as `v2/requests/usercode-sandbox-windows.md` item 1.

## 4. Status (second agent of `usercode-sandbox`, 2026-10-29)

Requests 2 and 3 are still open: `crates/blaze-cargo/build.rs` has no `linux_interpose()` yet,
and the `windows-sys` line lacks the features in `v2/requests/usercode-sandbox-windows.md`.
Until 2 lands, `src/sandbox/linux.rs` does not build on Linux (it includes
`OUT_DIR/libblaze_sandbox_interpose.so` and reads `BLAZE_SANDBOX_INTERPOSE`); until 3 lands,
`src/sandbox/windows.rs` does not build on Windows. The macOS library
(`src/sandbox/macos_interpose.c`) needs no `build.rs` change: `sandbox_library()` builds the
same file, which now also calls only libSystem (`CCRandomGenerateBytes` is libcommonCrypto,
re-exported by libSystem).
