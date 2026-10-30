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
