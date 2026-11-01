//! A connection to the daemon's AF_UNIX socket on Windows, for [`crate::edit`].

#![allow(unsafe_code, reason = "Winsock's AF_UNIX calls, which std has no type for")]

use std::path::Path;

#[link(name = "ws2_32")]
unsafe extern "system" {
    fn WSAStartup(version: u16, data: *mut u8) -> i32;
    fn socket(family: i32, kind: i32, protocol: i32) -> usize;
    fn connect(socket: usize, address: *const u8, len: i32) -> i32;
    fn send(socket: usize, bytes: *const u8, len: i32, flags: i32) -> i32;
    fn recv(socket: usize, bytes: *mut u8, len: i32, flags: i32) -> i32;
    fn closesocket(socket: usize) -> i32;
    fn WSAGetLastError() -> i32;
}

/// A connection to the daemon's AF_UNIX socket, which the standard library has no type for on
/// Windows.
pub(crate) struct Socket(usize);

impl Socket {
    pub(crate) fn connect(path: &Path) -> std::io::Result<Socket> {
        const AF_UNIX: u16 = 1;
        const SOCK_STREAM: i32 = 1;
        const INVALID_SOCKET: usize = !0;
        let path = path.to_str().ok_or(std::io::ErrorKind::InvalidInput)?.as_bytes();
        // `sockaddr_un`: the family, then the path ending with a zero in 108 bytes.
        let mut address = [0u8; 2 + 108];
        if path.len() >= 108 {
            return Err(std::io::ErrorKind::InvalidInput.into());
        }
        address[..2].copy_from_slice(&AF_UNIX.to_le_bytes());
        address[2..2 + path.len()].copy_from_slice(path);
        // SAFETY: `WSADATA` is under 512 bytes; a second start only counts one more user.
        if unsafe { WSAStartup(0x202, [0u8; 512].as_mut_ptr()) } != 0 {
            return Err(Socket::error());
        }
        // SAFETY: it takes no pointer.
        let socket = unsafe { socket(i32::from(AF_UNIX), SOCK_STREAM, 0) };
        if socket == INVALID_SOCKET {
            return Err(Socket::error());
        }
        let socket = Socket(socket);
        // SAFETY: the address is a whole `sockaddr_un`.
        if unsafe { connect(socket.0, address.as_ptr(), address.len() as i32) } != 0 {
            return Err(Socket::error());
        }
        Ok(socket)
    }

    fn error() -> std::io::Error {
        // SAFETY: it takes nothing.
        std::io::Error::from_raw_os_error(unsafe { WSAGetLastError() })
    }
}

impl std::io::Read for Socket {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        let len = bytes.len().min(i32::MAX as usize) as i32;
        // SAFETY: `bytes` has room for `len`.
        let read = unsafe { recv(self.0, bytes.as_mut_ptr(), len, 0) };
        if read < 0 { Err(Socket::error()) } else { Ok(read as usize) }
    }
}

impl std::io::Write for Socket {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let len = bytes.len().min(i32::MAX as usize) as i32;
        // SAFETY: `bytes` has `len` of them.
        let sent = unsafe { send(self.0, bytes.as_ptr(), len, 0) };
        if sent < 0 { Err(Socket::error()) } else { Ok(sent as usize) }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Drop for Socket {
    fn drop(&mut self) {
        // SAFETY: the socket is this one's alone.
        unsafe { closesocket(self.0) };
    }
}
