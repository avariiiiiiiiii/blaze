//! The sockets behind [`crate::Effect::Listen`] and [`crate::Effect::Connect`], for every
//! driver on Unix: made here, then read and written as each driver does (blocking on a thread,
//! through the ring, through a kqueue).

use std::{
    ffi::OsString,
    io::{self, Read as _, Write as _},
    net::{SocketAddr, TcpListener, TcpStream},
    os::{
        fd::{AsRawFd as _, FromRawFd as _, IntoRawFd as _, OwnedFd, RawFd},
        unix::{
            ffi::OsStrExt as _,
            fs::PermissionsExt as _,
            net::{UnixListener, UnixStream},
        },
    },
    path::{Path, PathBuf},
};

use crate::Address;

pub(crate) enum Listener {
    Unix { listener: UnixListener, path: PathBuf },
    Tcp(TcpListener),
}

impl Listener {
    pub(crate) fn fd(&self) -> RawFd {
        match self {
            Listener::Unix { listener, .. } => listener.as_raw_fd(),
            Listener::Tcp(listener) => listener.as_raw_fd(),
        }
    }

    pub(crate) fn is_unix(&self) -> bool {
        matches!(self, Listener::Unix { .. })
    }

    pub(crate) fn set_nonblocking(&self, on: bool) -> io::Result<()> {
        match self {
            Listener::Unix { listener, .. } => listener.set_nonblocking(on),
            Listener::Tcp(listener) => listener.set_nonblocking(on),
        }
    }

    /// The address a TCP listen was given (its port, where it asked for port 0).
    pub(crate) fn local_addr(&self) -> Option<std::net::SocketAddr> {
        match self {
            Listener::Unix { .. } => None,
            Listener::Tcp(listener) => listener.local_addr().ok(),
        }
    }

    pub(crate) fn accept(&self) -> io::Result<Socket> {
        match self {
            Listener::Unix { listener, .. } => Ok(Socket::Unix(listener.accept()?.0)),
            Listener::Tcp(listener) => {
                let (stream, _) = listener.accept()?;
                stream.set_nodelay(true)?;
                Ok(Socket::Tcp(stream))
            }
        }
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        if let Listener::Unix { path, .. } = self {
            _ = std::fs::remove_file(&*path);
        }
    }
}

/// A socket listening at `at`. A Unix socket is made under a temporary name, given mode 0600
/// and renamed into place, so no other user can connect in between; a name left by a daemon
/// that is gone is replaced, one another process listens on is refused.
pub(crate) fn listen(root: &Path, at: &Address) -> io::Result<Listener> {
    match at {
        Address::Unix(name) => {
            let path = root.join(name.as_str());
            crate::fs::make_parent(&path)?;
            if path.exists() {
                if UnixStream::connect(&path).is_ok() {
                    return Err(io::Error::new(
                        io::ErrorKind::AddrInUse,
                        format!("{}: another process listens on it", path.display()),
                    ));
                }
                std::fs::remove_file(&path)?;
            }
            let temporary = crate::fs::temporary_next_to(&path);
            let listener = UnixListener::bind(&temporary)?;
            let placed = std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o600))
                .and_then(|()| std::fs::rename(&temporary, &path));
            if let Err(error) = placed {
                _ = std::fs::remove_file(&temporary);
                return Err(error);
            }
            Ok(Listener::Unix { listener, path })
        }
        Address::Tcp(address) => Ok(Listener::Tcp(TcpListener::bind(address)?)),
    }
}

pub(crate) enum Socket {
    Unix(UnixStream),
    Tcp(TcpStream),
}

impl Socket {
    /// A connected socket the driver got as a bare descriptor (an accept of the ring's).
    pub(crate) fn from_fd(fd: OwnedFd, unix: bool) -> Socket {
        let raw = fd.into_raw_fd();
        // SAFETY: the descriptor is a connected socket of the kind named, owned from here.
        unsafe {
            match unix {
                true => Socket::Unix(UnixStream::from_raw_fd(raw)),
                false => Socket::Tcp(TcpStream::from_raw_fd(raw)),
            }
        }
    }

    pub(crate) fn fd(&self) -> RawFd {
        match self {
            Socket::Unix(stream) => stream.as_raw_fd(),
            Socket::Tcp(stream) => stream.as_raw_fd(),
        }
    }

    pub(crate) fn set_nonblocking(&self, on: bool) -> io::Result<()> {
        match self {
            Socket::Unix(stream) => stream.set_nonblocking(on),
            Socket::Tcp(stream) => stream.set_nonblocking(on),
        }
    }

    pub(crate) fn read(&self, bytes: &mut [u8]) -> io::Result<usize> {
        loop {
            let read = match self {
                Socket::Unix(stream) => (&*stream).read(bytes),
                Socket::Tcp(stream) => (&*stream).read(bytes),
            };
            match read {
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                // The peer reset it: as good as hung up, for a reader.
                Err(error) if error.kind() == io::ErrorKind::ConnectionReset => return Ok(0),
                other => return other,
            }
        }
    }

    pub(crate) fn write(&self, bytes: &[u8]) -> io::Result<usize> {
        match self {
            Socket::Unix(stream) => (&*stream).write(bytes),
            Socket::Tcp(stream) => (&*stream).write(bytes),
        }
    }

    pub(crate) fn write_all(&self, bytes: &[u8]) -> io::Result<()> {
        match self {
            Socket::Unix(stream) => (&*stream).write_all(bytes),
            Socket::Tcp(stream) => (&*stream).write_all(bytes),
        }
    }

    /// Both directions shut: a read in flight on it ends with nothing.
    pub(crate) fn shutdown(&self) -> io::Result<()> {
        let shut = match self {
            Socket::Unix(stream) => stream.shutdown(std::net::Shutdown::Both),
            Socket::Tcp(stream) => stream.shutdown(std::net::Shutdown::Both),
        };
        match shut {
            Err(error) if error.kind() == io::ErrorKind::NotConnected => Ok(()),
            other => other,
        }
    }
}

/// A connection made to `at`, blocking until it is.
pub(crate) fn connect(root: &Path, at: &Address) -> io::Result<Socket> {
    match at {
        Address::Unix(name) => Ok(Socket::Unix(UnixStream::connect(root.join(name.as_str()))?)),
        Address::Tcp(address) => {
            let stream = TcpStream::connect(address)?;
            stream.set_nodelay(true)?;
            Ok(Socket::Tcp(stream))
        }
    }
}

/// `at` as the system names a socket's address, for a driver that connects by itself.
pub(crate) struct SockAddr {
    pub(crate) storage: libc::sockaddr_storage,
    pub(crate) len: libc::socklen_t,
    pub(crate) domain: libc::c_int,
}

pub(crate) fn sockaddr(root: &Path, at: &Address) -> io::Result<SockAddr> {
    // SAFETY: all zeros is a valid `sockaddr_storage`.
    let mut storage: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
    let (len, domain) = match at {
        Address::Unix(name) => {
            let path: OsString = root.join(name.as_str()).into_os_string();
            let bytes = path.as_bytes();
            // SAFETY: `sockaddr_storage` is large and aligned enough for any address.
            let un = unsafe { &mut *(&raw mut storage).cast::<libc::sockaddr_un>() };
            if bytes.len() >= un.sun_path.len() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("{}: longer than a socket's path may be", path.to_string_lossy()),
                ));
            }
            un.sun_family = libc::AF_UNIX as libc::sa_family_t;
            for (to, &from) in un.sun_path.iter_mut().zip(bytes) {
                *to = from as libc::c_char;
            }
            let len = std::mem::offset_of!(libc::sockaddr_un, sun_path) + bytes.len() + 1;
            (len, libc::AF_UNIX)
        }
        Address::Tcp(SocketAddr::V4(address)) => {
            // SAFETY: as above.
            let in4 = unsafe { &mut *(&raw mut storage).cast::<libc::sockaddr_in>() };
            in4.sin_family = libc::AF_INET as libc::sa_family_t;
            in4.sin_port = address.port().to_be();
            in4.sin_addr = libc::in_addr { s_addr: u32::from_ne_bytes(address.ip().octets()) };
            (size_of::<libc::sockaddr_in>(), libc::AF_INET)
        }
        Address::Tcp(SocketAddr::V6(address)) => {
            // SAFETY: as above.
            let in6 = unsafe { &mut *(&raw mut storage).cast::<libc::sockaddr_in6>() };
            in6.sin6_family = libc::AF_INET6 as libc::sa_family_t;
            in6.sin6_port = address.port().to_be();
            in6.sin6_flowinfo = address.flowinfo();
            in6.sin6_addr = libc::in6_addr { s6_addr: address.ip().octets() };
            in6.sin6_scope_id = address.scope_id();
            (size_of::<libc::sockaddr_in6>(), libc::AF_INET6)
        }
    };
    set_len(&mut storage, len);
    Ok(SockAddr { storage, len: len as libc::socklen_t, domain })
}

/// Every address starts with its length on Apple's systems.
#[cfg(target_os = "macos")]
fn set_len(storage: &mut libc::sockaddr_storage, len: usize) {
    storage.ss_len = len as u8;
}

#[cfg(not(target_os = "macos"))]
fn set_len(_: &mut libc::sockaddr_storage, _: usize) {}

/// A socket of `domain`, of the kind `connect` takes, not yet connected.
pub(crate) fn stream_socket(domain: libc::c_int) -> io::Result<OwnedFd> {
    // SAFETY: no pointers; a descriptor of its own or -1.
    let fd = unsafe { libc::socket(domain, libc::SOCK_STREAM, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the call returned a descriptor of its own.
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    set_cloexec(fd.as_raw_fd())?;
    Ok(fd)
}

pub(crate) fn set_cloexec(fd: RawFd) -> io::Result<()> {
    // SAFETY: an open descriptor.
    if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
