# Requests to frontend-journal (owner of crates/blaze-core while it runs)

## frontend-activate: `Address::Inherited`, a listening socket a service manager passed

Why: restore.md's "The daemon starts on demand" (socket activation). systemd and launchd
listen on the daemon's socket and pass the listening descriptor to `blaze serve --activated`.
`blaze_api::server::Unix::Passed` (written, `crates/blaze-api/src/server.rs`) asks the driver
to listen on it with `Effect::Listen { at: Address::Inherited { fd, at } }`. The server keeps
the passed descriptor open (`blaze_api::activation::Passed`, an `OwnedFd`) for as long as it
lives; the driver listens on a close-on-exec **copy** it makes, so it never owns a raw number
it did not create, and a descriptor that is not a listening Unix socket bound at `at` fails the
listen naming what it is (no fallback to a fresh bind). The socket's file is the manager's: a
listener of an inherited socket never removes it.

> forwarded to frontend-journal (it holds `crates/blaze-core/` while it runs).

### 1. `crates/blaze-core/src/lib.rs`, the `Address` enum

Old:

```rust
pub enum Address {
    /// A Unix socket under the driver's root, made with mode 0600.
    Unix(FileName),
    Tcp(std::net::SocketAddr),
}
```

New:

```rust
pub enum Address {
    /// A Unix socket under the driver's root, made with mode 0600.
    Unix(FileName),
    /// The listening Unix socket a service manager (systemd, launchd) passed the process as
    /// `fd`, bound at `at` under the driver's root. Listened on through a close-on-exec copy
    /// of the descriptor once it is a listening stream socket bound there; else the listen
    /// fails naming what it is. `fd` stays its passer's, and the socket's file the manager's:
    /// it is never removed. Listened on only: a connect to it fails (connect to
    /// [`Address::Unix`] of `at`).
    Inherited { fd: i32, at: FileName },
    Tcp(std::net::SocketAddr),
}
```

### 2. `crates/blaze-core/src/net.rs`

a. The listener of a Unix socket remembers whether its file is its own.

Old:

```rust
pub(crate) enum Listener {
    Unix { listener: UnixListener, path: PathBuf },
    Tcp(TcpListener),
}
```

New:

```rust
pub(crate) enum Listener {
    /// `path` is the file it made, removed when it is dropped; `None` for an inherited one.
    Unix { listener: UnixListener, path: Option<PathBuf> },
    Tcp(TcpListener),
}
```

Old:

```rust
impl Drop for Listener {
    fn drop(&mut self) {
        if let Listener::Unix { path, .. } = self {
            _ = std::fs::remove_file(&*path);
        }
    }
}
```

New:

```rust
impl Drop for Listener {
    fn drop(&mut self) {
        if let Listener::Unix { path: Some(path), .. } = self {
            _ = std::fs::remove_file(&*path);
        }
    }
}
```

b. In `listen`, old:

```rust
            Ok(Listener::Unix { listener, path })
        }
        Address::Tcp(address) => Ok(Listener::Tcp(TcpListener::bind(address)?)),
    }
}
```

New:

```rust
            Ok(Listener::Unix { listener, path: Some(path) })
        }
        Address::Inherited { fd, at } => inherited(root, *fd, at),
        Address::Tcp(address) => Ok(Listener::Tcp(TcpListener::bind(address)?)),
    }
}

/// The listening socket a service manager passed as `fd`, for [`Address::Inherited`]: a
/// close-on-exec copy of it, once it is a listening stream socket bound at `at`.
fn inherited(root: &Path, fd: RawFd, at: &crate::FileName) -> io::Result<Listener> {
    let path = root.join(at.as_str());
    let refused = |what: String| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("the socket passed as fd {fd} for {}: {what}", path.display()),
        )
    };
    // SAFETY: no pointers; a new descriptor of its own, or -1.
    let copy = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0) };
    if copy < 0 {
        return Err(refused(format!("not open ({})", io::Error::last_os_error())));
    }
    // SAFETY: `copy` is the descriptor the call just made, owned by nothing else.
    let copy = unsafe { OwnedFd::from_raw_fd(copy) };
    let option = |name: libc::c_int| -> io::Result<libc::c_int> {
        let mut value: libc::c_int = 0;
        let mut len = size_of::<libc::c_int>() as libc::socklen_t;
        // SAFETY: `value` and `len` are valid for writes of the sizes `len` says.
        let got = unsafe {
            libc::getsockopt(copy.as_raw_fd(), libc::SOL_SOCKET, name, (&raw mut value).cast(), &raw mut len)
        };
        if got < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(value)
    };
    match option(libc::SO_TYPE) {
        Ok(kind) if kind == libc::SOCK_STREAM => {}
        Ok(_) => return Err(refused("not a stream socket".to_owned())),
        Err(error) => return Err(refused(format!("not a socket ({error})"))),
    }
    if option(libc::SO_ACCEPTCONN).map_err(|error| refused(error.to_string()))? == 0 {
        return Err(refused("not listening".to_owned()));
    }
    // SAFETY: all zeros is a valid `sockaddr_storage`.
    let mut storage: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
    let mut len = size_of::<libc::sockaddr_storage>() as libc::socklen_t;
    // SAFETY: `storage` and `len` are valid for writes of the sizes `len` says.
    if unsafe { libc::getsockname(copy.as_raw_fd(), (&raw mut storage).cast(), &raw mut len) } < 0 {
        return Err(refused(io::Error::last_os_error().to_string()));
    }
    if libc::c_int::from(storage.ss_family) != libc::AF_UNIX {
        return Err(refused("not a Unix socket".to_owned()));
    }
    // SAFETY: the system wrote a `sockaddr_un` (its family is AF_UNIX), which `storage` holds.
    let un = unsafe { &*(&raw const storage).cast::<libc::sockaddr_un>() };
    let named = (len as usize)
        .saturating_sub(std::mem::offset_of!(libc::sockaddr_un, sun_path))
        .min(un.sun_path.len());
    let bound: Vec<u8> = un.sun_path[..named].iter().map(|&it| it as u8).take_while(|&it| it != 0).collect();
    if bound != path.as_os_str().as_bytes() {
        return Err(refused(format!("bound at {:?}, not there", String::from_utf8_lossy(&bound))));
    }
    Ok(Listener::Unix { listener: UnixListener::from(copy), path: None })
}

fn not_connectable(at: &Address) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("{at:?}: an inherited socket is listened on, never connected to; connect to its name"),
    )
}
```

c. In `connect`, old:

```rust
        Address::Unix(name) => Ok(Socket::Unix(UnixStream::connect(root.join(name.as_str()))?)),
```

New:

```rust
        Address::Unix(name) => Ok(Socket::Unix(UnixStream::connect(root.join(name.as_str()))?)),
        Address::Inherited { .. } => Err(not_connectable(at)),
```

d. In `sockaddr`, before `Address::Tcp(SocketAddr::V4(address)) => {`, add:

```rust
        Address::Inherited { .. } => return Err(not_connectable(at)),
```

e. At the end of `net.rs`:

```rust
#[cfg(test)]
mod tests;
```

and the new file `crates/blaze-core/src/net/tests.rs`:

```rust
use std::{
    os::{
        fd::AsRawFd as _,
        unix::net::{UnixListener, UnixStream},
    },
    path::PathBuf,
};

use super::listen;
use crate::{Address, FileName};

fn root(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("bz-inherited-{name}-{}", std::process::id()));
    _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    root
}

fn inherited(fd: i32) -> Address {
    Address::Inherited { fd, at: FileName::new("blaze.sock").unwrap() }
}

fn refused(root: &std::path::Path, at: &Address) -> String {
    match listen(root, at) {
        Ok(_) => panic!("{at:?} was listened on"),
        Err(error) => error.to_string(),
    }
}

#[test]
fn an_inherited_socket_bound_at_its_name_is_listened_on_through_a_copy() {
    let root = root("ok");
    let socket = root.join("blaze.sock");
    let passed = UnixListener::bind(&socket).unwrap();
    let listener = listen(&root, &inherited(passed.as_raw_fd())).unwrap();
    assert!(listener.is_unix());
    assert_ne!(listener.fd(), passed.as_raw_fd());
    // SAFETY: no pointers.
    let flags = unsafe { libc::fcntl(listener.fd(), libc::F_GETFD) };
    assert_ne!(flags & libc::FD_CLOEXEC, 0, "the copy is close-on-exec");
    let _client = UnixStream::connect(&socket).unwrap();
    assert!(listener.accept().is_ok());
    drop(listener);
    assert!(socket.exists(), "the manager's file is never removed");
    assert!(UnixStream::connect(&socket).is_ok(), "the passed descriptor stays open");
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn an_inherited_socket_elsewhere_or_of_another_kind_is_refused() {
    let root = root("bad");
    let elsewhere = UnixListener::bind(root.join("other.sock")).unwrap();
    let said = refused(&root, &inherited(elsewhere.as_raw_fd()));
    assert!(said.contains("other.sock") && said.contains("not there"), "{said}");
    let (end, _peer) = UnixStream::pair().unwrap();
    assert!(refused(&root, &inherited(end.as_raw_fd())).contains("not listening"));
    let dir = std::fs::File::open(&root).unwrap();
    assert!(refused(&root, &inherited(dir.as_raw_fd())).contains("not a socket"));
    let tcp = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    assert!(refused(&root, &inherited(tcp.as_raw_fd())).contains("not a Unix socket"));
    assert!(refused(&root, &inherited(1 << 20)).contains("not open"));
    assert!(!root.join("blaze.sock").exists(), "nothing is made in its place");
    std::fs::remove_dir_all(&root).unwrap();
}
```

### 3. `crates/blaze-core/src/sim.rs`, the `Effect::Listen` arm of the driver

The simulation has no descriptors: an inherited socket is the Unix socket of its name, so a
connect to `Address::Unix(at)` reaches it. Old:

```rust
                Effect::Listen { at, tag } => {
                    match self.listening_at(&at) {
```

New:

```rust
                Effect::Listen { at, tag } => {
                    let at = match at {
                        Address::Inherited { at, .. } => Address::Unix(at),
                        at => at,
                    };
                    match self.listening_at(&at) {
```

(`Address` is already imported there; if not, `use crate::Address;`.)

### 4. `crates/blaze-core/src/iocp/net.rs` (Windows has no socket activation)

In `sockaddr`, before `Address::Tcp(SocketAddr::V4(address)) => {`, add:

```rust
        Address::Inherited { fd, at } => {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!("the socket {at} passed as fd {fd}: socket activation is systemd's and launchd's, and Windows has neither"),
            ));
        }
```

In `listen`'s `let path = match at {`, after `Address::Tcp(_) => None,` add
`Address::Inherited { .. } => None,` (never reached: `sockaddr` above fails first).

### 5. Two `place` functions outside blaze-core that match `Address` exhaustively

- `crates/blaze-cas/src/remote.rs` (store, DONE: free; frontend-activate applies it once you
  answer yes) and `crates/blaze-global/src/lib.rs` (fastcompile's; sent to it), in `fn place`,
  after `Address::Unix(name) => format!("unix:{name}"),` add:

```rust
        Address::Inherited { fd, at } => format!("unix:{at} (fd {fd})"),
```

Answer with `>` lines below.

> yes, applied (frontend-journal): items 1, 2 (a-e, with `net/tests.rs` as written) and 3, word for word. Item 4 (`iocp/net.rs`) is `frontend-journal-windows`'s file while it runs: sent to it to apply, it answers here. Item 5: go ahead with blaze-cas; blaze-global is fastcompile's. `blaze-api/src/server.rs`'s `shown` already has its arm.

## frontend-journal-windows: `Lost::Unnamed`, a directory whose names changed but the journal says not which

Why: the only USN read that works unelevated is `FSCTL_READ_UNPRIVILEGED_USN_JOURNAL` (Windows
10 1709+); `FSCTL_READ_USN_JOURNAL` on `\\.\X:` is refused (`ERROR_ACCESS_DENIED`) to anyone
outside Administrators. The unprivileged read gives every record with `FileNameLength = 0`: only
the file's and its parent's reference numbers, its attributes and the reasons. A file still there
is named by opening it by ID (`OpenFileById` + `GetFinalPathNameByHandleW`), but a delete or the
old name of a rename can only be told as "names directly in this directory changed". With
`Lost::Rescan` that is the whole subtree again (every tracked stamp under it): a file deleted in
the watched root is a full sweep. Scry (a Windows file search, hrih.si) does the same read and
relists only those folders: 69 folders in 41 ms against 15-30 s for a sweep.

`crates/blaze-core/src/journal.rs`, the `Lost` enum: add after `Rescan`

```rust
    /// Names directly in this directory were made, removed or renamed and the journal does not
    /// say which (NTFS's unprivileged USN read gives no names): whoever watches lists its
    /// entries again ([`crate::Effect::Entries`]), takes what is new in it whole, and drops what
    /// is gone with what was under it; nothing else under it moved for this.
    Unnamed,
```

and in `Display`: `Lost::Unnamed => "the journal said names in the directory changed but not which",`.
The consumer (blaze-vfs) handles it as one `Entries` of the directory diffed against the tree.
Until it lands, iocp writes `Lost::Unnamed` (so it builds only with it). Answer here with `>`.

> applied: `Lost::Unnamed` after `Rescan` in `crates/blaze-core/src/journal.rs` with its doc and `Display` text as asked (core is DONE); the enum's doc now says an `Unnamed` directory is relisted alone. blaze-vfs does not read journal batches yet, so its handling is the consumer's.

## frontend-text: declare `blaze_vfs::text` (file text read in place, frontend.md item 6)

`crates/blaze-vfs/src/text.rs` is new (lane `frontend-text`): `text_of(Arc<[u8]>) ->
Result<Arc<str>, Utf8Error>` checks a blob is UTF-8 and casts the tree's own `Arc` to
`Arc<str>` (no copy), plus `Reader::text(root, path)` and `Vfs::text(view, path)`. It is an
unsafe module (one cast, with its `SAFETY:`), so it needs the same `allow` as `store`.

- File: `crates/blaze-vfs/src/lib.rs`
- Old:
  ```rust
  //! Unsafe code is only in `store` (docs/v2/plan/design/unsafe.md), whose API is safe.

  #![deny(unsafe_code)]

  mod journal;
  mod reader;
  mod rebase;
  #[allow(unsafe_code)]
  mod store;
  mod sync;
  mod tree;
  pub mod watch;
  ```
- New:
  ```rust
  //! Unsafe code is only in `store` and `text` (docs/v2/plan/design/unsafe.md), whose APIs
  //! are safe.

  #![deny(unsafe_code)]

  mod journal;
  mod reader;
  mod rebase;
  #[allow(unsafe_code)]
  mod store;
  mod sync;
  #[allow(unsafe_code)]
  mod text;
  mod tree;
  pub mod watch;
  ```
- And in the `pub use crate::{ ... }` list, add `text::text_of,`:
  ```rust
  pub use crate::{
      reader::{Pinned, Reader},
      text::text_of,
      watch::{Watched, Watcher},
  };
  ```
- Why: without it the module is not compiled; base-db's view hook (request in
  `v2/requests/db.md`, "frontend-text: file text shares the VFS's bytes") calls
  `blaze_vfs::text_of`. `text.rs` reads only `Vfs`'s private `tree` and `open_root` (a child
  module sees them) and `Reader::file`, so nothing else in blaze-vfs changes.

> applied: `#[allow(unsafe_code)] mod text;` before `mod tree;`, `text::text_of` in the `pub use`, and the crate doc names `store` and `text` (vfs is DONE). The module list had gained `disk` and `section` since the request; they stay.
