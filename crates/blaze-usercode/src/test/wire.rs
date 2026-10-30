//! The fork server's protocol: what blaze ([`crate::test::queue`]) and a test binary's fork
//! server (the harness side, [`crate::test::FORK_SERVER`]) say on the Unix socket blaze listens
//! on. This file is compiled twice: as this module of blaze, and as text inside the harness of
//! every test binary blaze writes, in whatever edition the tested crate has. So it names
//! everything from `::std` and `::core`, uses nothing of blaze and nothing an edition changes,
//! and holds no tests (they are in `crate::test`'s).
//!
//! Every message is a frame: its body's length as a little-endian `u32`, then the body, at
//! most [`LONGEST`] bytes. A body is a kind byte and its fields: integers little-endian, a
//! string or bytes a `u32` length and then them, a list a `u32` count and then its items.
//!
//! - The server, once connected and before any test runs: [`Frame::Hello`] (kind 1) with
//!   [`VERSION`], its process id and the name of every test of the binary in order; a test is
//!   named by its index in this list from then on.
//! - blaze: [`Frame::Run`] (kind 129), the test at an index under an id of blaze's choosing.
//!   The server forks a child that marks its scope (`access(MARK + name)`), runs the one test
//!   as libtest would, with its output (standard output and error, together) going to the
//!   server, and exits; the server answers [`Frame::Ended`] (kind 2) with the id, how it ended
//!   and what it wrote. Many tests run at once.
//! - blaze: [`Frame::Kill`] (kind 130): the test of the id is killed, with every process it
//!   started; its [`Frame::Ended`] still comes. A kill of an id that ended is nothing.
//! - blaze: [`Frame::Quit`] (kind 131): the server exits once every test it runs has ended
//!   and been answered. A server whose socket ends kills what it runs and exits.

/// The variable of a test binary's environment that makes its `main` a fork server: the
/// absolute path of the socket to connect to. The server takes it out of its environment
/// before any test runs.
pub const SOCKET: &str = "__BLAZE_FORK_SERVER";

/// What a child marks its scope with in the sandbox's record, the test's name after it
/// (`blaze-cargo`'s `sandbox::MARK`).
pub const MARK: &str = "/.blaze-test/";

/// The protocol's version, said by [`Frame::Hello`].
pub const VERSION: u32 = 1;

/// The most bytes a frame's body may have.
pub const LONGEST: u32 = 64 << 20;

/// The most bytes of output a test's [`Frame::Ended`] carries; past it the output is cut and
/// says so.
pub const LONGEST_OUTPUT: usize = 16 << 20;

const HELLO: u8 = 1;
const ENDED: u8 = 2;
const RUN: u8 = 129;
const KILL: u8 = 130;
const QUIT: u8 = 131;

/// How a test's child ended.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum End {
    /// Its exit code: 0 is a pass, as libtest would judge it; anything else a failure, whose
    /// output says why.
    Exited(i32),
    /// The signal that ended it: a kill, an abort, a crash.
    Signaled(i32),
    /// No child was made, and why: the fork failed, or there is no test at the index.
    NotStarted(String),
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Frame {
    Hello { version: u32, pid: u32, tests: Vec<String> },
    Ended { id: u64, end: End, output: Vec<u8> },
    Run { id: u64, index: u32 },
    Kill { id: u64 },
    Quit,
}

fn put_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn put_u64(out: &mut Vec<u8>, value: u64) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn put_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    put_u32(out, bytes.len() as u32);
    out.extend_from_slice(bytes);
}

/// [`Frame::Ended`] of these parts, with its length, added to `out`: the bytes
/// [`Frame::encode`] adds for it, from a borrowed output, so a server encodes each test's
/// output once, straight from the buffer it was read into.
pub fn encode_ended(out: &mut Vec<u8>, id: u64, end: &End, output: &[u8]) {
    let why = match end {
        End::NotStarted(why) => why.len() + 4,
        End::Exited(_) | End::Signaled(_) => 4,
    };
    out.reserve(4 + 1 + 8 + 1 + why + 4 + output.len());
    let start = out.len();
    put_u32(out, 0);
    out.push(ENDED);
    put_u64(out, id);
    match end {
        End::Exited(code) => {
            out.push(0);
            out.extend_from_slice(&code.to_le_bytes());
        }
        End::Signaled(signal) => {
            out.push(1);
            out.extend_from_slice(&signal.to_le_bytes());
        }
        End::NotStarted(why) => {
            out.push(2);
            put_bytes(out, why.as_bytes());
        }
    }
    put_bytes(out, output);
    let len = (out.len() - start - 4) as u32;
    out[start..start + 4].copy_from_slice(&len.to_le_bytes());
}

impl Frame {
    /// The frame, with its length, added to `out`.
    pub fn encode(&self, out: &mut Vec<u8>) {
        if let Frame::Ended { id, end, output } = self {
            return encode_ended(out, *id, end, output);
        }
        let start = out.len();
        put_u32(out, 0);
        match self {
            Frame::Ended { .. } => {}
            Frame::Hello { version, pid, tests } => {
                out.push(HELLO);
                put_u32(out, *version);
                put_u32(out, *pid);
                put_u32(out, tests.len() as u32);
                for name in tests {
                    put_bytes(out, name.as_bytes());
                }
            }
            Frame::Run { id, index } => {
                out.push(RUN);
                put_u64(out, *id);
                put_u32(out, *index);
            }
            Frame::Kill { id } => {
                out.push(KILL);
                put_u64(out, *id);
            }
            Frame::Quit => out.push(QUIT),
        }
        let len = (out.len() - start - 4) as u32;
        out[start..start + 4].copy_from_slice(&len.to_le_bytes());
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        self.encode(&mut out);
        out
    }

    /// The frame a body is, or why it is not one.
    pub fn decode(body: &[u8]) -> Result<Frame, String> {
        let mut at = Cursor { bytes: body, at: 0 };
        let frame = match at.u8()? {
            HELLO => {
                let version = at.u32()?;
                let pid = at.u32()?;
                let count = at.u32()?;
                // Each name takes at least its 4-byte length, so a lying count allocates no
                // more than the body could hold.
                let mut tests = Vec::with_capacity((count as usize).min(at.left() / 4));
                for _ in 0..count {
                    tests.push(at.string()?);
                }
                Frame::Hello { version, pid, tests }
            }
            ENDED => {
                let id = at.u64()?;
                let end = match at.u8()? {
                    0 => End::Exited(at.u32()? as i32),
                    1 => End::Signaled(at.u32()? as i32),
                    2 => End::NotStarted(at.string()?),
                    other => return Err(format!("no end is of kind {}", other)),
                };
                let output = at.bytes()?.to_vec();
                Frame::Ended { id, end, output }
            }
            RUN => {
                let id = at.u64()?;
                Frame::Run { id, index: at.u32()? }
            }
            KILL => Frame::Kill { id: at.u64()? },
            QUIT => Frame::Quit,
            other => return Err(format!("no frame is of kind {}", other)),
        };
        match at.at == body.len() {
            true => Ok(frame),
            false => Err(format!("a frame has {} bytes after its end", body.len() - at.at)),
        }
    }
}

struct Cursor<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Cursor<'a> {
    fn left(&self) -> usize {
        self.bytes.len() - self.at
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8], String> {
        if self.bytes.len() - self.at < len {
            return Err("a frame ends early".to_owned());
        }
        let taken = &self.bytes[self.at..self.at + len];
        self.at += len;
        Ok(taken)
    }

    fn u8(&mut self) -> Result<u8, String> {
        Ok(self.take(1)?[0])
    }

    fn u32(&mut self) -> Result<u32, String> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn u64(&mut self) -> Result<u64, String> {
        let b = self.take(8)?;
        Ok(u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]))
    }

    fn bytes(&mut self) -> Result<&'a [u8], String> {
        let len = self.u32()? as usize;
        self.take(len)
    }

    fn string(&mut self) -> Result<String, String> {
        let bytes = self.bytes()?;
        match ::std::str::from_utf8(bytes) {
            Ok(text) => Ok(text.to_owned()),
            Err(_) => Err("a string of a frame is not UTF-8".to_owned()),
        }
    }
}

/// Frames out of a byte stream that comes in pieces. The frames read are skipped over, not
/// moved out: what still waits moves to the front only where a push would otherwise grow
/// the buffer, so many frames in one piece cost one pass, not one move of the rest per frame.
#[derive(Default, Debug)]
pub struct Frames {
    buffer: Vec<u8>,
    /// Where the bytes not yet read as a frame start.
    start: usize,
}

impl Frames {
    pub fn new() -> Frames {
        Frames { buffer: Vec::new(), start: 0 }
    }

    pub fn push(&mut self, bytes: &[u8]) {
        if self.start == self.buffer.len() {
            self.buffer.clear();
            self.start = 0;
        } else if self.start > 0 && self.buffer.len() + bytes.len() > self.buffer.capacity() {
            self.buffer.drain(..self.start);
            self.start = 0;
        }
        self.buffer.extend_from_slice(bytes);
    }

    /// [`Frames::push`] of bytes the caller owns: taken as the buffer, with no copy, when
    /// nothing is waiting.
    pub fn push_owned(&mut self, bytes: Vec<u8>) {
        match self.waiting() {
            0 => {
                self.buffer = bytes;
                self.start = 0;
            }
            _ => self.push(&bytes),
        }
    }

    /// The next whole frame, `None` until one has come. An error is for good: the stream is
    /// not this protocol's.
    pub fn pop(&mut self) -> Result<Option<Frame>, String> {
        let rest = &self.buffer[self.start..];
        if rest.len() < 4 {
            return Ok(None);
        }
        let len = u32::from_le_bytes([rest[0], rest[1], rest[2], rest[3]]);
        if len > LONGEST {
            return Err(format!("a frame of {} bytes, past the most of {}", len, LONGEST));
        }
        let len = len as usize;
        if rest.len() < 4 + len {
            return Ok(None);
        }
        let frame = Frame::decode(&rest[4..4 + len]);
        self.start += 4 + len;
        if self.start == self.buffer.len() {
            self.buffer.clear();
            self.start = 0;
        }
        frame.map(Some)
    }

    /// Bytes of a frame not yet whole.
    pub fn waiting(&self) -> usize {
        self.buffer.len() - self.start
    }

    /// How many more bytes the frame being received needs before it is whole (its length's
    /// bytes first): what one read can ask for, so a long frame comes in one read rather than
    /// many small ones. Never more than a frame can be.
    pub fn needed(&self) -> usize {
        let rest = &self.buffer[self.start..];
        if rest.len() < 4 {
            return 4 - rest.len();
        }
        let len = u32::from_le_bytes([rest[0], rest[1], rest[2], rest[3]]).min(LONGEST) as usize;
        (4 + len).saturating_sub(rest.len())
    }
}
