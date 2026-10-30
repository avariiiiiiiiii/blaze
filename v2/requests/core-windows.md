# Requests from lane core-windows

## 1. `windows-sys` for `blaze-core` (needed to build `iocp`)

- What: in the workspace `Cargo.toml`'s `[workspace.dependencies]`:

  ```toml
  windows-sys = { version = "0.61.2", features = [
      "Win32_Foundation",
      "Win32_Networking_WinSock",
      "Win32_Security",
      "Win32_Storage_FileSystem",
      "Win32_System_IO",
      "Win32_System_Ioctl",
      "Win32_System_JobObjects",
      "Win32_System_Memory",
      "Win32_System_Pipes",
      "Win32_System_SystemServices",
      "Win32_System_Threading",
  ] }
  ```

  (if another crate already declares `windows-sys` there, add these features to it), and in
  `crates/blaze-core/Cargo.toml`:

  ```toml
  [target.'cfg(windows)'.dependencies]
  windows-sys.workspace = true
  ```

- Where: `Cargo.toml`, `crates/blaze-core/Cargo.toml` (core's).
- Why: `crates/blaze-core/src/iocp.rs` and `iocp/` use only these modules; 0.61.2 is the
  version in `Cargo.lock`. `lib.rs` already has `#[cfg(windows)] pub mod iocp;`, so a Windows
  build of `blaze-core` fails until this lands.

## 2. `tests.rs`: run the shared scripts on `Iocp`

- What: beside the `Uring` and `Kqueue` runs in `crates/blaze-core/src/tests.rs` (`script`,
  `processes_and_sockets`, `a_sandbox_is_made_by_the_confine_or_refused`), a
  `#[cfg(windows)]` run on `crate::iocp::Iocp::new(&root, 2).unwrap()` (and
  `.confined(Arc::new(..))` for the confined one). The scripts' processes are `/bin/sh` today,
  so on Windows the processes script needs `%SystemRoot%\System32\cmd.exe /d /c ...` commands,
  or stays Unix-only (`iocp.rs`'s own tests cover processes on Windows).
- Where: `crates/blaze-core/src/tests.rs` (core's).
- Why: those scripts are where every driver is held to one meaning. `Iocp` has every effect,
  `pub fn confined(self, Arc<dyn Confine>) -> Iocp` and `pub fn local_addr(&self, Tag)`, with
  the same meaning as on `Threads`, `Uring` and `Kqueue`.

## 3. `fs.rs` on Windows: what `Threads` does should match what `Iocp` does

- What: give `fs.rs`'s Windows branches the calls `iocp/files.rs` makes, so `Threads` on
  Windows does what `Iocp` does:
  - `clone`: ReFS block cloning (`FSCTL_GET_INTEGRITY_INFORMATION` for the cluster size,
    `FSCTL_SET_INTEGRITY_INFORMATION` on the target, its end of file set, then
    `FSCTL_DUPLICATE_EXTENTS_TO_FILE` in cluster-rounded pieces under 4 GiB). Elsewhere, an
    error naming the file system that "does not copy". Today `fs::clone` on Windows always
    refuses.
  - `set_mode`: the read-only attribute as the owner's write bit; 0o755, 0o644, 0o555 and
    0o444 kept, anything else refused, naming the file. Today it is `Unsupported` for every
    mode.
  - `map`: `CreateFileMappingW` and `MapViewOfFile`. Today it is `Unsupported`.
  - `write_atomically` and `rename`: `MoveFileExW(MOVEFILE_REPLACE_EXISTING |
    MOVEFILE_WRITE_THROUGH)`, so the new name is durable when it returns, as the Unix
    directory sync makes it. Today `sync_dir_of` is a no-op on Windows and std's rename is not
    written through.

  The simplest way is to move `iocp/files.rs`'s `clone`, `set_mode`, `map`, `moved` and
  `write_atomically` into `fs.rs` under `#[cfg(windows)]`, and `iocp.rs` calls `fs::` like the
  others. Tell me and I drop my copies.
- Where: `crates/blaze-core/src/fs.rs` (core's); `crates/blaze-core/src/iocp/files.rs` (mine).
- Why: no-fallback and one meaning per effect. As things are, the same `Effect::Clone`, `Mode`
  or `Map` succeeds on `Iocp` and fails on `Threads`. `iocp.rs`'s file-script test compares the
  two and has to allow for the clone difference. Its `Mode` step uses 0o600, which both drivers
  refuse.

## 4. docs/v2/plan/lanes/core.md's Windows section: how a process's end is heard

- What: where it says the process's end is the job's `JOB_OBJECT_MSG_EXIT_PROCESS`, say
  instead: "a thread-pool wait on the process (`RegisterWaitForSingleObject`) posts its end to
  the port; the job's messages, which Windows does not promise to deliver, only say that the job
  went past its memory, and the driver then kills it". Also: "a memory-limited job's peak is at
  most its limit: Windows refuses any commit past it".
- Where: `docs/v2/plan/lanes/core.md` and `~/restart-2026-10-28/briefs/v2/core-windows.md` (if
  that brief is kept as the record).
- Why: the job's completion-port messages are documented as not guaranteed. A lost
  `EXIT_PROCESS` would leave a spawn in flight forever, while a wait on the process handle
  always fires. And with `JOB_OBJECT_LIMIT_JOB_MEMORY` the commit is refused at the limit, so
  "a peak over it" can't be observed; the test checks `0 < peak_memory <= limit` and an exit
  code other than 0.
