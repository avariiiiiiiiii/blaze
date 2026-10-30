# Requests from lane core-linux

## 1. `tests.rs`: run the confined and shared scripts on `Uring` too

- What: in `crates/blaze-core/src/tests.rs`, the confined/unconfined test (around line 335,
  `Threads::new(&root, 1).confined(Arc::new(Shell))`) also on
  `crate::uring::Uring::new(&root).unwrap().confined(Arc::new(Shell))` under
  `#[cfg(target_os = "linux")]`, and any new shared script of processes or sockets likewise.
- Where: `crates/blaze-core/src/tests.rs` (core's).
- Why: `Uring` now has every effect and `pub fn confined(self, Arc<dyn Confine>) -> Uring`
  with the same meaning as `Threads::confined`; the shared scripts are where the drivers are
  held to one meaning.

## 2. Optional: `Threads::local_addr`, for a TCP port-0 comparison

- What: `pub fn local_addr(&self, listen: Tag) -> Option<std::net::SocketAddr>` on `Threads`,
  as `Uring::local_addr` is (the address a TCP listen of port 0 was given; `None` for a Unix
  socket or a listen not in flight).
- Where: `crates/blaze-core/src/threads.rs`.
- Why: `uring.rs`'s TCP round trip runs on the ring alone because nothing reads the port back
  off a `Threads` listen; the Unix-socket round trip is compared on both. With it, the TCP one
  can be compared too (and a state machine that listens on port 0 can say where it listens).

## 3. docs/v2/plane.md's watch row: what makes fanotify refuse

- What: after "fanotify, inotify where fanotify needs a privilege the process lacks", add: "A
  file system that gives no file handles (fanotify reports changes by them) is an error naming
  it, not a quiet switch to inotify."
- Where: `docs/v2/plane.md`, the IO core table.
- Why: that is what `crate::watch::Tree::new` does (no-fallback): only `EPERM` (no
  `CAP_SYS_ADMIN` or `CAP_DAC_READ_SEARCH`) means inotify. A daemon running as root on a file
  system without handles (an overlayfs before 6.6, some FUSE or 9p mounts) will fail its
  watches with that error, which whoever runs the VMs should know.
