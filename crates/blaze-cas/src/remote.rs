//! The remote tier (docs/v2/plan/lanes/store.md section 8, "Remote"): a machine outside the pool (a laptop)
//! reads values from the store of a project's VM daemon, read-through. It is never on the VM's
//! hot path. [`Server`] runs in the VM's daemon and answers from its [`Store`]; [`Client`] runs
//! on the laptop as the store's [`Remote`]. Both are sans-IO: they push [`Effect`]s and take
//! [`Completion`]s, and keep no thread, clock or socket of their own.
//!
//! The wire, fixed-width and little-endian:
//!
//! - An **ask** is `b"BLZASK"`, the version as a `u16` (1), and the 32 bytes of the [`Key`].
//!   A client sends asks back to back on one connection without waiting (pipelined).
//! - An **answer frame** is its length as a `u64`, then [`Answer::encode`]'s bytes: `b"BLZANS"`,
//!   the version as a `u16` (1), a byte saying which answer (0 missing, 1 value), the key, and
//!   for a value its [`Kind::code`], its content hash, its length as a `u64` and its bytes. The
//!   server answers asks in the order they came.
//! - Bounds: a value is at most [`LONGEST_VALUE`] bytes, and a client refuses a frame longer
//!   than that and its header before reading it. A server hangs up a connection whose stream is
//!   not whole asks, and stops reading a connection while its unsent answers pass a bound.
//!
//! **Trust**: the store checks an answer's bytes against its `content` before it keeps the
//! value, so a value damaged on the way is caught. It cannot check that the key really maps to
//! that value: a server that lies about what a key's value is would be believed. So the channel
//! is what is trusted, and it is the user's own VM over a Unix socket (mode 0600) or a loopback
//! port forwarded over SSH. There is no TLS or authentication here, which is why
//! [`Server::new`] refuses a TCP address that is not loopback.
//!
//! No fallback: a fetch that can't be answered (no connection, the server hung up, a malformed
//! or too-long frame) completes [`Done::Failed`] naming what went wrong, for the store to see.

use std::collections::{BTreeMap, VecDeque};

use blaze_core::{Address, Completion, Conn, Done, Effect, IoError, Tag};
use blaze_ids::ContentId;

use crate::{Corrupt, Key, Kind, Remote, Store};

/// The most bytes of one value an answer may carry; a longer one is refused before it is read.
pub const LONGEST_VALUE: u64 = 1 << 30;

const VERSION: u16 = 1;
const ASK_MAGIC: &[u8; 6] = b"BLZASK";
const ANSWER_MAGIC: &[u8; 6] = b"BLZANS";
const MISSING: u8 = 0;
const VALUE: u8 = 1;

/// Magic, version and key.
const ASK_LEN: usize = 8 + 32;
/// Magic, version, which answer, key, kind, content, length.
const VALUE_HEADER: usize = 8 + 1 + 32 + 1 + 32 + 8;
/// The longest answer frame a client takes, not counting its length prefix.
const LONGEST_FRAME: u64 = LONGEST_VALUE + VALUE_HEADER as u64;
/// The fewest bytes a client asks of one receive.
const CLIENT_RECV: usize = 1 << 20;
/// The most bytes a client asks of one receive, while a long frame is owed.
const CLIENT_RECV_MOST: usize = 1 << 26;
/// The most bytes a server asks of one receive: a batch of asks.
const SERVER_RECV: usize = ASK_LEN * 1024;
/// A server reads no more asks from a connection while more than this of its answers are unsent.
const QUEUED_MOST: usize = 1 << 26;

/// What the server says about one key.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Answer {
    /// The server's store has no value of `key`.
    Missing { key: Key },
    /// `content` is `Entry::content` of the server's verified get.
    Value { key: Key, kind: Kind, content: ContentId, bytes: Vec<u8> },
}

impl Answer {
    /// The server's answer for `key` from `store`: its value through `Store::get` (verified), or
    /// `Missing`. A corrupt value is the `Err`, never sent.
    pub fn of(store: &Store, key: &Key) -> Result<Answer, Corrupt> {
        Ok(match store.get(key)? {
            Some(entry) => Answer::Value {
                key: *key,
                kind: entry.kind,
                content: entry.content,
                bytes: entry.bytes.to_vec(),
            },
            None => Answer::Missing { key: *key },
        })
    }

    /// The answer's bytes on the wire, without the frame's length prefix.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        match self {
            Answer::Missing { key } => write_missing(&mut out, key),
            Answer::Value { key, kind, content, bytes } => {
                write_value(&mut out, key, *kind, content, bytes)
            }
        }
        out
    }

    /// Refuses anything malformed, truncated, with trailing bytes, an unknown kind code, or a
    /// value longer than `LONGEST_VALUE`, saying why. It does not check `content` against
    /// `bytes`: the store does that before it keeps the value.
    pub fn decode(bytes: &[u8]) -> Result<Answer, String> {
        Ok(match parse(bytes)? {
            Parsed::Missing { key } => Answer::Missing { key },
            Parsed::Value { key, kind, content, bytes } => {
                Answer::Value { key, kind, content, bytes: bytes.to_vec() }
            }
        })
    }
}

/// An answer read in place.
enum Parsed<'a> {
    Missing { key: Key },
    Value { key: Key, kind: Kind, content: ContentId, bytes: &'a [u8] },
}

impl Parsed<'_> {
    fn key(&self) -> Key {
        match self {
            Parsed::Missing { key } | Parsed::Value { key, .. } => *key,
        }
    }
}

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, len: usize, what: &str) -> Result<&'a [u8], String> {
        let left = self.bytes.len() - self.at;
        if left < len {
            return Err(format!(
                "truncated: {what} needs {len} bytes at offset {}, {left} are left",
                self.at
            ));
        }
        let taken = &self.bytes[self.at..self.at + len];
        self.at += len;
        Ok(taken)
    }

    fn id(&mut self, what: &str) -> Result<ContentId, String> {
        let mut id = [0; 32];
        id.copy_from_slice(self.take(32, what)?);
        Ok(ContentId(id))
    }

    fn u64(&mut self, what: &str) -> Result<u64, String> {
        let mut word = [0; 8];
        word.copy_from_slice(self.take(8, what)?);
        Ok(u64::from_le_bytes(word))
    }
}

fn parse(bytes: &[u8]) -> Result<Parsed<'_>, String> {
    let mut reader = Reader { bytes, at: 0 };
    let header = reader.take(8, "the header")?;
    if header[..6] != ANSWER_MAGIC[..] {
        return Err(format!("not an answer: its magic is {:?}", header[..6].escape_ascii().to_string()));
    }
    let version = u16::from_le_bytes([header[6], header[7]]);
    if version != VERSION {
        return Err(format!("an answer of version {version}; this blaze speaks version {VERSION}"));
    }
    let which = reader.take(1, "which answer")?[0];
    let key = Key(reader.id("the key")?);
    let parsed = match which {
        MISSING => Parsed::Missing { key },
        VALUE => {
            let code = reader.take(1, "the kind")?[0];
            let kind = Kind::from_code(code).ok_or_else(|| format!("an unknown kind code {code}"))?;
            let content = reader.id("the content hash")?;
            let len = reader.u64("the value's length")?;
            if len > LONGEST_VALUE {
                return Err(format!(
                    "a value of {len} bytes, longer than the {LONGEST_VALUE} an answer may carry"
                ));
            }
            let bytes = reader.take(len as usize, "the value")?;
            Parsed::Value { key, kind, content, bytes }
        }
        other => return Err(format!("an unknown answer {other} (0 is missing, 1 a value)")),
    };
    let trailing = bytes.len() - reader.at;
    if trailing != 0 {
        return Err(format!("{trailing} trailing bytes after the answer"));
    }
    Ok(parsed)
}

fn header(magic: &[u8; 6]) -> [u8; 8] {
    let mut header = [0; 8];
    header[..6].copy_from_slice(magic);
    header[6..].copy_from_slice(&VERSION.to_le_bytes());
    header
}

fn write_missing(out: &mut Vec<u8>, key: &Key) {
    out.reserve(8 + 1 + 32);
    out.extend_from_slice(&header(ANSWER_MAGIC));
    out.push(MISSING);
    out.extend_from_slice(&key.0.0);
}

fn write_value(out: &mut Vec<u8>, key: &Key, kind: Kind, content: &ContentId, bytes: &[u8]) {
    out.reserve(VALUE_HEADER + bytes.len());
    out.extend_from_slice(&header(ANSWER_MAGIC));
    out.push(VALUE);
    out.extend_from_slice(&key.0.0);
    out.push(kind.code());
    out.extend_from_slice(&content.0);
    out.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
    crate::copy::append(out, bytes);
}

fn write_ask(out: &mut Vec<u8>, key: &Key) {
    out.extend_from_slice(&header(ASK_MAGIC));
    out.extend_from_slice(&key.0.0);
}

/// An answer as one frame: its length, then its bytes.
fn framed(write: impl FnOnce(&mut Vec<u8>)) -> Vec<u8> {
    let mut out = vec![0; 8];
    write(&mut out);
    let len = (out.len() - 8) as u64;
    out[..8].copy_from_slice(&len.to_le_bytes());
    out
}

/// The answer frame for `key` from `store`, written from the value in place.
fn answer_frame(store: &Store, key: &Key) -> Result<Vec<u8>, Corrupt> {
    let entry = store.get(key)?;
    Ok(framed(|out| match entry {
        Some(entry) => write_value(out, key, entry.kind, &entry.content, entry.bytes),
        None => write_missing(out, key),
    }))
}

/// How many more bytes complete the frame `buffer` starts with: 0 until its length prefix is
/// whole, and for a prefix over [`LONGEST_FRAME`], which is refused, not read.
fn owed(buffer: &[u8]) -> usize {
    let Some(prefix) = buffer.first_chunk::<8>() else { return 0 };
    let len = u64::from_le_bytes(*prefix);
    if len > LONGEST_FRAME {
        return 0;
    }
    (8 + len as usize).saturating_sub(buffer.len())
}

fn place(at: &Address) -> String {
    match at {
        Address::Unix(name) => format!("unix:{name}"),
        Address::Tcp(addr) => format!("tcp:{addr}"),
    }
}

/// The tags of one state machine: `base..base + 2^32`.
struct Tags {
    base: u64,
    next: u64,
}

impl Tags {
    fn next(&mut self) -> Tag {
        let tag = Tag(self.base + (self.next & 0xffff_ffff));
        self.next = self.next.wrapping_add(1);
        tag
    }

    fn owns(&self, tag: Tag) -> bool {
        tag.0.wrapping_sub(self.base) < 1 << 32
    }
}

/// The laptop's side: one connection to `at`, asks pipelined on it, answers in order.
pub struct Client {
    at: Address,
    tags: Tags,
    link: Link,
    /// Asks not yet handed to a send.
    out: Vec<u8>,
    /// Fetches asked on this connection, oldest first: their answers come in this order.
    waiting: VecDeque<(Tag, Key)>,
    /// Connects given up while in flight: a connection one of them makes is hung up.
    abandoned: Vec<Tag>,
}

enum Link {
    Down,
    Connecting(Tag),
    Up(Up),
}

struct Up {
    conn: Conn,
    sending: Option<Tag>,
    recv: Option<Tag>,
    /// Received and not yet a whole frame.
    buffer: Vec<u8>,
}

enum Role {
    Connect,
    Send,
    Recv,
    Abandoned,
    /// Of a connection already given up: its hangup, or a send or receive that was in flight.
    Stale,
}

impl Client {
    /// Tags it asks with are `tags..tags + 2^32`.
    pub fn new(at: Address, tags: u64) -> Client {
        Client {
            at,
            tags: Tags { base: tags, next: 0 },
            link: Link::Down,
            out: Vec::new(),
            waiting: VecDeque::new(),
            abandoned: Vec::new(),
        }
    }

    /// Whether `tag` is one it asks with, so its completion goes to [`Client::complete`].
    pub fn owns(&self, tag: Tag) -> bool {
        self.tags.owns(tag)
    }

    /// Takes a completion of its own. Returns the answers to fetches: each a `Completion` whose
    /// tag is the tag given to `Remote::fetch`, with `Done::Read(answer.encode() bytes as received)`,
    /// or `Done::Failed(..)` naming what went wrong (could not connect, the server hung up, a
    /// malformed or too-long frame). The caller hands each to `Store::complete`.
    pub fn complete(&mut self, completion: Completion, effects: &mut Vec<Effect>) -> Vec<Completion> {
        let mut answers = Vec::new();
        let Completion { tag, done } = completion;
        match self.role(tag) {
            Role::Abandoned => {
                self.abandoned.retain(|it| *it != tag);
                if let Done::Connected { conn } = done {
                    effects.push(Effect::Hangup { conn, tag: self.tags.next() });
                }
            }
            Role::Connect => {
                self.link = Link::Down;
                match done {
                    Done::Connected { conn } => {
                        self.link =
                            Link::Up(Up { conn, sending: None, recv: None, buffer: Vec::new() });
                        self.pump(effects);
                    }
                    Done::Failed(error) => {
                        self.fail(&format!("could not connect: {error}"), &mut answers, effects)
                    }
                    other => {
                        self.fail(&format!("a connect completed as {other:?}"), &mut answers, effects)
                    }
                }
            }
            Role::Send => match done {
                Done::Sent => {
                    if let Link::Up(up) = &mut self.link {
                        up.sending = None;
                    }
                    self.pump(effects);
                }
                Done::Failed(error) => {
                    self.fail(&format!("sending asks failed: {error}"), &mut answers, effects)
                }
                other => self.fail(&format!("a send completed as {other:?}"), &mut answers, effects),
            },
            Role::Recv => {
                if let Link::Up(up) = &mut self.link {
                    up.recv = None;
                }
                match done {
                    Done::Read(bytes) if bytes.is_empty() => {
                        let why =
                            format!("the server hung up with {} answers owed", self.waiting.len());
                        self.fail(&why, &mut answers, effects);
                    }
                    Done::Read(bytes) => {
                        if let Link::Up(up) = &mut self.link {
                            up.buffer.extend_from_slice(&bytes);
                        }
                        match self.take_answers(&mut answers) {
                            Ok(()) => self.pump(effects),
                            Err(why) => self.fail(&why, &mut answers, effects),
                        }
                    }
                    Done::Failed(error) => {
                        self.fail(&format!("receiving answers failed: {error}"), &mut answers, effects)
                    }
                    other => {
                        self.fail(&format!("a receive completed as {other:?}"), &mut answers, effects)
                    }
                }
            }
            Role::Stale => {}
        }
        answers
    }

    /// Hangs up; every fetch still waiting is answered `Done::Failed`, returned here.
    pub fn close(&mut self, effects: &mut Vec<Effect>) -> Vec<Completion> {
        let mut answers = Vec::new();
        self.fail("the client closed", &mut answers, effects);
        answers
    }

    fn role(&self, tag: Tag) -> Role {
        if self.abandoned.contains(&tag) {
            return Role::Abandoned;
        }
        match &self.link {
            Link::Connecting(connect) if *connect == tag => Role::Connect,
            Link::Up(up) if up.sending == Some(tag) => Role::Send,
            Link::Up(up) if up.recv == Some(tag) => Role::Recv,
            _ => Role::Stale,
        }
    }

    /// Sends what is unsent and receives while answers are owed, one of each in flight.
    fn pump(&mut self, effects: &mut Vec<Effect>) {
        let Link::Up(up) = &mut self.link else { return };
        if up.sending.is_none() && !self.out.is_empty() {
            let tag = self.tags.next();
            up.sending = Some(tag);
            effects.push(Effect::Send { conn: up.conn, bytes: std::mem::take(&mut self.out), tag });
        }
        if up.recv.is_none() && !self.waiting.is_empty() {
            let tag = self.tags.next();
            up.recv = Some(tag);
            let most = owed(&up.buffer).clamp(CLIENT_RECV, CLIENT_RECV_MOST);
            effects.push(Effect::Recv { conn: up.conn, most, tag });
        }
    }

    /// Answers the waiting fetches whose frames have arrived whole.
    fn take_answers(&mut self, answers: &mut Vec<Completion>) -> Result<(), String> {
        let Link::Up(up) = &mut self.link else { return Ok(()) };
        let mut start = 0;
        let result = loop {
            let rest = &up.buffer[start..];
            if rest.len() < 8 {
                break Ok(());
            }
            let mut prefix = [0; 8];
            prefix.copy_from_slice(&rest[..8]);
            let len = u64::from_le_bytes(prefix);
            if len > LONGEST_FRAME {
                break Err(format!(
                    "an answer frame of {len} bytes, more than the {LONGEST_FRAME} allowed"
                ));
            }
            let len = len as usize;
            if rest.len() - 8 < len {
                break Ok(());
            }
            let body = &rest[8..8 + len];
            let Some((tag, key)) = self.waiting.pop_front() else {
                break Err("an answer to nothing asked".to_owned());
            };
            match parse(body) {
                Err(why) => {
                    self.waiting.push_front((tag, key));
                    break Err(format!("a malformed answer: {why}"));
                }
                Ok(parsed) if parsed.key() != key => {
                    self.waiting.push_front((tag, key));
                    break Err(format!("an answer for {:x} in place of one for it", parsed.key().0));
                }
                Ok(_) => answers.push(Completion { tag, done: Done::Read(body.to_vec()) }),
            }
            start += 8 + len;
        };
        up.buffer.drain(..start);
        up.buffer.reserve(owed(&up.buffer));
        result
    }

    /// Fails every waiting fetch with `why` and gives up the connection; the next fetch makes
    /// another.
    fn fail(&mut self, why: &str, answers: &mut Vec<Completion>, effects: &mut Vec<Effect>) {
        let at = place(&self.at);
        answers.extend(self.waiting.drain(..).map(|(tag, key)| Completion {
            tag,
            done: Done::Failed(IoError(format!(
                "the remote store at {at}: {why}; asked for {:x}",
                key.0
            ))),
        }));
        match std::mem::replace(&mut self.link, Link::Down) {
            Link::Down => {}
            Link::Connecting(tag) => self.abandoned.push(tag),
            Link::Up(up) => effects.push(Effect::Hangup { conn: up.conn, tag: self.tags.next() }),
        }
        self.out.clear();
    }
}

impl Remote for Client {
    /// Connects first if it has no connection, then sends the ask.
    fn fetch(&mut self, key: &Key, tag: Tag, effects: &mut Vec<Effect>) {
        if let Link::Down = self.link {
            let connect = self.tags.next();
            effects.push(Effect::Connect { at: self.at.clone(), tag: connect });
            self.link = Link::Connecting(connect);
        }
        write_ask(&mut self.out, key);
        self.waiting.push_back((tag, *key));
        self.pump(effects);
    }
}

/// The VM daemon's side: listens at `at`, answers asks from `store`.
pub struct Server {
    at: Address,
    tags: Tags,
    listen: Option<Tag>,
    /// Listens unlistened and not yet ended: a connection one of them accepts is hung up.
    closing: Vec<Tag>,
    failed: Option<IoError>,
    peers: BTreeMap<Conn, Peer>,
    /// The connection of each send and receive in flight.
    ops: BTreeMap<Tag, Conn>,
}

#[derive(Default)]
struct Peer {
    recv: Option<Tag>,
    /// The send in flight and its length.
    sending: Option<(Tag, usize)>,
    out: VecDeque<Vec<u8>>,
    /// Bytes of answers not yet sent, the one in flight with them.
    queued: usize,
    /// Received and not yet a whole ask.
    buffer: Vec<u8>,
}

impl Server {
    /// Refuses (Err, saying why) a TCP address that is not loopback: there is no TLS or
    /// authentication here, so it is reached through a Unix socket (mode 0600, made by the
    /// driver) or a loopback port forwarded over SSH. Tags are `tags..tags + 2^32`.
    pub fn new(at: Address, tags: u64) -> Result<Server, String> {
        if let Address::Tcp(addr) = &at
            && !addr.ip().to_canonical().is_loopback()
        {
            return Err(format!(
                "the remote store would listen on {addr}, which is not loopback: it has no TLS or \
                 authentication, so it is served on a Unix socket or a loopback port forwarded over SSH"
            ));
        }
        Ok(Server {
            at,
            tags: Tags { base: tags, next: 0 },
            listen: None,
            closing: Vec::new(),
            failed: None,
            peers: BTreeMap::new(),
            ops: BTreeMap::new(),
        })
    }

    /// Asks to listen.
    pub fn start(&mut self, effects: &mut Vec<Effect>) {
        if self.listen.is_some() {
            return;
        }
        let tag = self.tags.next();
        self.listen = Some(tag);
        self.failed = None;
        effects.push(Effect::Listen { at: self.at.clone(), tag });
    }

    /// Whether `tag` is one it asks with, so its completion goes to [`Server::complete`].
    pub fn owns(&self, tag: Tag) -> bool {
        self.tags.owns(tag)
    }

    /// Why it stopped listening, when the listen ended by itself rather than by
    /// [`Server::stop`]: the daemon is no longer serving the remote tier and says so.
    pub fn failed(&self) -> Option<&IoError> {
        self.failed.as_ref()
    }

    /// Takes a completion of its own; answers every whole ask it has from `store`. Returns the
    /// corrupt values it found (and did not send; it answered `Missing` for them).
    pub fn complete(
        &mut self,
        completion: Completion,
        store: &Store,
        effects: &mut Vec<Effect>,
    ) -> Vec<Corrupt> {
        let mut corrupt = Vec::new();
        let Completion { tag, done } = completion;
        if self.listen == Some(tag) {
            match done {
                Done::Accepted { conn } => {
                    self.peers.insert(conn, Peer::default());
                    self.pump(conn, effects);
                }
                Done::Failed(error) => {
                    self.listen = None;
                    self.failed = Some(IoError(format!(
                        "listening at {} for the remote store: {error}",
                        place(&self.at)
                    )));
                }
                other => {
                    self.listen = None;
                    self.failed = Some(IoError(format!(
                        "listening at {} for the remote store completed as {other:?}",
                        place(&self.at)
                    )));
                }
            }
            return corrupt;
        }
        if self.closing.contains(&tag) {
            match done {
                Done::Accepted { conn } => {
                    effects.push(Effect::Hangup { conn, tag: self.tags.next() });
                }
                _ => self.closing.retain(|it| *it != tag),
            }
            return corrupt;
        }
        let Some(conn) = self.ops.remove(&tag) else { return corrupt };
        let Some(peer) = self.peers.get_mut(&conn) else { return corrupt };
        if peer.sending.is_some_and(|(sending, _)| sending == tag) {
            match done {
                Done::Sent => {
                    if let Some((_, len)) = peer.sending.take() {
                        peer.queued -= len;
                    }
                    self.pump(conn, effects);
                }
                _ => self.drop_peer(conn, effects),
            }
        } else {
            peer.recv = None;
            match done {
                Done::Read(bytes) if !bytes.is_empty() => {
                    peer.buffer.extend_from_slice(&bytes);
                    self.serve(conn, store, &mut corrupt, effects);
                }
                _ => self.drop_peer(conn, effects),
            }
        }
        corrupt
    }

    /// Unlistens and hangs up every connection.
    pub fn stop(&mut self, effects: &mut Vec<Effect>) {
        if let Some(tag) = self.listen.take() {
            effects.push(Effect::Unlisten { tag });
            self.closing.push(tag);
        }
        for conn in std::mem::take(&mut self.peers).into_keys() {
            effects.push(Effect::Hangup { conn, tag: self.tags.next() });
        }
        self.ops.clear();
    }

    /// Answers the whole asks `conn` has sent; hangs it up at one that is not an ask.
    fn serve(
        &mut self,
        conn: Conn,
        store: &Store,
        corrupt: &mut Vec<Corrupt>,
        effects: &mut Vec<Effect>,
    ) {
        let Some(peer) = self.peers.get_mut(&conn) else { return };
        let ask_header = header(ASK_MAGIC);
        let mut start = 0;
        let mut broken = false;
        while peer.buffer.len() - start >= ASK_LEN {
            let ask = &peer.buffer[start..start + ASK_LEN];
            if ask[..8] != ask_header {
                broken = true;
                break;
            }
            let mut id = [0; 32];
            id.copy_from_slice(&ask[8..]);
            let key = Key(ContentId(id));
            let frame = answer_frame(store, &key).unwrap_or_else(|found| {
                corrupt.push(found);
                framed(|out| write_missing(out, &key))
            });
            peer.queued += frame.len();
            peer.out.push_back(frame);
            start += ASK_LEN;
        }
        peer.buffer.drain(..start);
        if broken {
            self.drop_peer(conn, effects);
        } else {
            self.pump(conn, effects);
        }
    }

    /// Sends the next answer and receives more asks while few enough answers are unsent, one of
    /// each in flight.
    fn pump(&mut self, conn: Conn, effects: &mut Vec<Effect>) {
        let Some(peer) = self.peers.get_mut(&conn) else { return };
        if peer.sending.is_none()
            && let Some(bytes) = peer.out.pop_front()
        {
            let tag = self.tags.next();
            peer.sending = Some((tag, bytes.len()));
            self.ops.insert(tag, conn);
            effects.push(Effect::Send { conn, bytes, tag });
        }
        if peer.recv.is_none() && peer.queued <= QUEUED_MOST {
            let tag = self.tags.next();
            peer.recv = Some(tag);
            self.ops.insert(tag, conn);
            effects.push(Effect::Recv { conn, most: SERVER_RECV, tag });
        }
    }

    fn drop_peer(&mut self, conn: Conn, effects: &mut Vec<Effect>) {
        let Some(peer) = self.peers.remove(&conn) else { return };
        if let Some(tag) = peer.recv {
            self.ops.remove(&tag);
        }
        if let Some((tag, _)) = peer.sending {
            self.ops.remove(&tag);
        }
        effects.push(Effect::Hangup { conn, tag: self.tags.next() });
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use blaze_core::{
        Address, Completion, Conn, Done, Effect, FileName, IoError, Tag, run_until_idle,
        sim::{Faults, Sim},
    };
    use blaze_ids::ContentId;

    use super::*;
    use crate::{Config, Corrupt, Key, Kind, Remote, Store};

    const CLIENT: u64 = 1 << 32;
    const SERVER: u64 = 2 << 32;

    fn key(name: &str) -> Key {
        Key(ContentId::of("blaze v2 remote test key 1", name.as_bytes()))
    }

    fn socket() -> Address {
        Address::Unix(FileName::new("remote.sock").unwrap())
    }

    fn ask(name: &str) -> Vec<u8> {
        let mut out = Vec::new();
        write_ask(&mut out, &key(name));
        out
    }

    fn unframe(frame: &[u8]) -> Answer {
        let len = u64::from_le_bytes(frame[..8].try_into().unwrap()) as usize;
        assert_eq!(frame.len(), 8 + len);
        Answer::decode(&frame[8..]).unwrap()
    }

    /// An open store holding `values`.
    fn stored(values: &[(&str, Kind, &[u8])]) -> Store {
        let mut sim = Sim::new(7, Faults::default());
        let mut effects = Vec::new();
        let config =
            Config { dir: FileName::new("store").unwrap(), layers: Vec::new(), tags: 0 };
        let mut store = Store::open(config, &mut effects);
        run_until_idle(&mut sim, &mut effects, |completion, effects| {
            assert!(store.owns(completion.tag));
            store.complete(completion, effects);
        });
        assert!(store.ready());
        for (name, kind, bytes) in values {
            store.put(key(name), *kind, bytes.to_vec()).unwrap();
        }
        store
    }

    fn failure(completion: &Completion) -> &str {
        match &completion.done {
            Done::Failed(IoError(why)) => why,
            other => panic!("expected a failure, got {other:?}"),
        }
    }

    fn answer(completion: &Completion) -> Answer {
        match &completion.done {
            Done::Read(bytes) => Answer::decode(bytes).unwrap(),
            other => panic!("expected an answer, got {other:?}"),
        }
    }

    const C: usize = 0;
    const S: usize = 1;

    /// One end of a connection in the test's network.
    struct End {
        conn: Conn,
        open: bool,
        recv: Option<(Tag, usize)>,
        incoming: Vec<u8>,
    }

    /// A network between one client and one server, run by hand: each side's effects become
    /// completions, and bytes sent reach the peer's receives at most `chunk` at a time.
    struct Net {
        client: Client,
        server: Server,
        effects: [Vec<Effect>; 2],
        inbox: [VecDeque<Completion>; 2],
        listen: Option<Tag>,
        links: Vec<[End; 2]>,
        chunk: usize,
        /// A side whose receives wait.
        hold: [bool; 2],
        answers: Vec<Completion>,
        corrupt: Vec<Corrupt>,
    }

    impl Net {
        fn new(chunk: usize) -> Net {
            let mut server = Server::new(socket(), SERVER).unwrap();
            let mut server_effects = Vec::new();
            server.start(&mut server_effects);
            Net {
                client: Client::new(socket(), CLIENT),
                server,
                effects: [Vec::new(), server_effects],
                inbox: [VecDeque::new(), VecDeque::new()],
                listen: None,
                links: Vec::new(),
                chunk,
                hold: [false; 2],
                answers: Vec::new(),
                corrupt: Vec::new(),
            }
        }

        fn fetch(&mut self, name: &str, tag: u64) {
            self.client.fetch(&key(name), Tag(tag), &mut self.effects[C]);
        }

        fn run(&mut self, store: &Store) {
            loop {
                let mut moved = false;
                for side in [C, S] {
                    for effect in std::mem::take(&mut self.effects[side]) {
                        moved = true;
                        self.effect(side, effect);
                    }
                }
                moved |= self.deliver();
                while let Some(completion) = self.inbox[C].pop_front() {
                    moved = true;
                    assert!(self.client.owns(completion.tag));
                    let answers = self.client.complete(completion, &mut self.effects[C]);
                    self.answers.extend(answers);
                }
                while let Some(completion) = self.inbox[S].pop_front() {
                    moved = true;
                    assert!(self.server.owns(completion.tag));
                    let corrupt = self.server.complete(completion, store, &mut self.effects[S]);
                    self.corrupt.extend(corrupt);
                }
                if !moved {
                    return;
                }
            }
        }

        fn end(&mut self, side: usize, conn: Conn) -> &mut [End; 2] {
            self.links.iter_mut().find(|link| link[side].conn == conn).unwrap()
        }

        fn effect(&mut self, side: usize, effect: Effect) {
            let done = |tag, done| Completion { tag, done };
            match effect {
                Effect::Listen { at, tag } if side == S => {
                    assert_eq!(at, socket());
                    self.listen = Some(tag);
                }
                Effect::Unlisten { tag } if side == S => {
                    assert_eq!(self.listen.take(), Some(tag));
                    self.inbox[S].push_back(done(tag, Done::Unlistened));
                }
                Effect::Connect { at, tag } if side == C => {
                    assert_eq!(at, socket());
                    let Some(listen) = self.listen else {
                        let failed = Done::Failed(IoError("nothing listens there".into()));
                        self.inbox[C].push_back(done(tag, failed));
                        return;
                    };
                    let n = self.links.len() as u64;
                    let end = |conn| End { conn: Conn(conn), open: true, recv: None, incoming: Vec::new() };
                    self.links.push([end(2 * n), end(2 * n + 1)]);
                    self.inbox[C].push_back(done(tag, Done::Connected { conn: Conn(2 * n) }));
                    self.inbox[S].push_back(done(listen, Done::Accepted { conn: Conn(2 * n + 1) }));
                }
                Effect::Send { conn, bytes, tag } => {
                    let link = self.end(side, conn);
                    let sent = if link[side].open && link[1 - side].open {
                        link[1 - side].incoming.extend_from_slice(&bytes);
                        Done::Sent
                    } else {
                        Done::Failed(IoError("the peer hung up".into()))
                    };
                    self.inbox[side].push_back(done(tag, sent));
                }
                Effect::Recv { conn, most, tag } => {
                    let link = self.end(side, conn);
                    assert!(link[side].open, "a receive on a connection hung up");
                    assert!(link[side].recv.is_none(), "two receives in flight on one connection");
                    link[side].recv = Some((tag, most));
                }
                Effect::Hangup { conn, tag } => {
                    let link = self.end(side, conn);
                    link[side].open = false;
                    let recv = link[side].recv.take();
                    if let Some((recv, _)) = recv {
                        self.inbox[side].push_back(done(recv, Done::Read(Vec::new())));
                    }
                    self.inbox[side].push_back(done(tag, Done::HungUp));
                }
                other => panic!("side {side} asked for {other:?}"),
            }
        }

        fn deliver(&mut self) -> bool {
            let mut moved = false;
            for link in &mut self.links {
                for side in [C, S] {
                    if self.hold[side] {
                        continue;
                    }
                    let Some((tag, most)) = link[side].recv else { continue };
                    let read = if !link[side].incoming.is_empty() {
                        let n = most.min(self.chunk).min(link[side].incoming.len());
                        link[side].incoming.drain(..n).collect()
                    } else if !link[1 - side].open {
                        Vec::new()
                    } else {
                        continue;
                    };
                    link[side].recv = None;
                    self.inbox[side].push_back(Completion { tag, done: Done::Read(read) });
                    moved = true;
                }
            }
            moved
        }
    }

    fn effect_on(effects: &[Effect], conn: Conn, recv: bool) -> Option<Tag> {
        effects.iter().find_map(|effect| match effect {
            Effect::Recv { conn: on, tag, .. } if recv && *on == conn => Some(*tag),
            Effect::Send { conn: on, tag, .. } if !recv && *on == conn => Some(*tag),
            _ => None,
        })
    }

    fn sent_on(effects: &[Effect], conn: Conn) -> Option<&[u8]> {
        effects.iter().find_map(|effect| match effect {
            Effect::Send { conn: on, bytes, .. } if *on == conn => Some(&bytes[..]),
            _ => None,
        })
    }

    fn hung_up(effects: &[Effect], conn: Conn) -> bool {
        effects.iter().any(|effect| matches!(effect, Effect::Hangup { conn: on, .. } if *on == conn))
    }

    fn samples() -> Vec<Answer> {
        let mut answers = vec![Answer::Missing { key: key("gone") }];
        for (n, kind) in Kind::ALL.into_iter().enumerate() {
            let bytes: Vec<u8> = (0..n * 37).map(|it| it as u8).collect();
            answers.push(Answer::Value {
                key: key(kind.name()),
                kind,
                content: ContentId::of("blaze v2 remote test content 1", &bytes),
                bytes,
            });
        }
        answers
    }

    #[test]
    fn answers_round_trip() {
        for answer in samples() {
            assert_eq!(Answer::decode(&answer.encode()), Ok(answer));
        }
    }

    #[test]
    fn decode_refuses_what_is_not_an_answer() {
        let value = Answer::Value {
            key: key("a"),
            kind: Kind::Code,
            content: ContentId::of("blaze v2 remote test content 1", b"abc"),
            bytes: b"abc".to_vec(),
        };
        let good = value.encode();
        let refused = |bytes: &[u8], why: &str| {
            let error = Answer::decode(bytes).unwrap_err();
            assert!(error.contains(why), "{error:?} does not say {why:?}");
        };

        let mut magic = good.clone();
        magic[0] = b'X';
        refused(&magic, "magic");
        let mut ask_magic = good.clone();
        ask_magic[..6].copy_from_slice(ASK_MAGIC);
        refused(&ask_magic, "magic");

        let mut version = good.clone();
        version[6] = 2;
        refused(&version, "version 2");

        let mut which = good.clone();
        which[8] = 7;
        refused(&which, "unknown answer 7");

        let mut kind = good.clone();
        kind[8 + 1 + 32] = 200;
        refused(&kind, "unknown kind code 200");

        for answer in samples() {
            let bytes = answer.encode();
            for len in 0..bytes.len() {
                refused(&bytes[..len], "truncated");
            }
            let mut trailing = bytes.clone();
            trailing.push(0);
            refused(&trailing, "1 trailing bytes");
        }

        let mut long = good[..VALUE_HEADER].to_vec();
        long[VALUE_HEADER - 8..].copy_from_slice(&(LONGEST_VALUE + 1).to_le_bytes());
        refused(&long, "longer than");
        let mut longest = long.clone();
        longest[VALUE_HEADER - 8..].copy_from_slice(&LONGEST_VALUE.to_le_bytes());
        refused(&longest, "truncated");
    }

    #[test]
    fn a_fetch_is_answered_with_a_value_and_one_with_missing() {
        let store = stored(&[("a", Kind::Code, b"the code of a".as_slice())]);
        let mut net = Net::new(usize::MAX);
        net.fetch("a", 1);
        net.fetch("b", 2);
        net.run(&store);

        assert_eq!(net.answers.len(), 2);
        assert_eq!(net.answers[0].tag, Tag(1));
        let content = store.get(&key("a")).unwrap().unwrap().content;
        assert_eq!(
            answer(&net.answers[0]),
            Answer::Value { key: key("a"), kind: Kind::Code, content, bytes: b"the code of a".to_vec() }
        );
        assert_eq!(answer(&net.answers[0]), Answer::of(&store, &key("a")).unwrap());
        assert_eq!(net.answers[1].tag, Tag(2));
        assert_eq!(answer(&net.answers[1]), Answer::Missing { key: key("b") });
        assert!(net.corrupt.is_empty());
        assert_eq!(net.links.len(), 1);
    }

    /// Kinds the store keeps whole, not as chunks.
    const PLAIN: [Kind; 4] = [Kind::Query, Kind::Code, Kind::Debug, Kind::Run];

    fn many() -> (Store, Vec<String>) {
        let names: Vec<String> = (0..24).map(|n| format!("value {n}")).collect();
        let bodies: Vec<Vec<u8>> =
            (0..24).map(|n| (0..n * 131).map(|it| (it * 7 + n) as u8).collect()).collect();
        let kept: Vec<(&str, Kind, &[u8])> = names
            .iter()
            .zip(&bodies)
            .enumerate()
            .filter(|(n, _)| n % 3 != 1)
            .map(|(n, (name, bytes))| (name.as_str(), PLAIN[n % PLAIN.len()], &bytes[..]))
            .collect();
        (stored(&kept), names)
    }

    fn pipelined(chunk: usize) {
        let (store, names) = many();
        let mut net = Net::new(chunk);
        for (n, name) in names.iter().enumerate() {
            net.fetch(name, 100 + n as u64);
        }
        net.run(&store);

        assert_eq!(net.answers.len(), names.len());
        for (n, (completion, name)) in net.answers.iter().zip(&names).enumerate() {
            assert_eq!(completion.tag, Tag(100 + n as u64));
            let expected = Answer::of(&store, &key(name)).unwrap();
            assert_eq!(matches!(expected, Answer::Missing { .. }), n % 3 == 1);
            assert_eq!(answer(completion), expected);
        }
        assert_eq!(net.links.len(), 1);
    }

    #[test]
    fn pipelined_asks_are_answered_in_order() {
        pipelined(usize::MAX);
    }

    #[test]
    fn answers_reassemble_from_any_split() {
        for chunk in [1, 2, 7, 39, 40, 41, 83, 4096] {
            pipelined(chunk);
        }
    }

    #[test]
    fn asks_between_answers_reuse_the_connection() {
        let (store, names) = many();
        let mut net = Net::new(5);
        for (n, name) in names.iter().enumerate() {
            net.fetch(name, n as u64);
            net.run(&store);
            assert_eq!(net.answers.len(), n + 1);
            assert_eq!(answer(&net.answers[n]), Answer::of(&store, &key(name)).unwrap());
        }
        assert_eq!(net.links.len(), 1);
    }

    #[test]
    fn a_server_hang_up_fails_the_waiting_fetches_and_the_next_reconnects() {
        let store = stored(&[("a", Kind::Run, b"a run".as_slice())]);
        let mut net = Net::new(3);
        net.hold[S] = true;
        net.fetch("a", 1);
        net.fetch("b", 2);
        net.run(&store);
        assert!(net.answers.is_empty());

        net.server.stop(&mut net.effects[S]);
        net.run(&store);
        assert_eq!(net.answers.len(), 2);
        for (completion, tag) in net.answers.iter().zip([1, 2]) {
            assert_eq!(completion.tag, Tag(tag));
            assert!(failure(completion).contains("hung up"), "{completion:?}");
            assert!(failure(completion).contains("unix:remote.sock"), "{completion:?}");
        }
        assert!(net.server.failed().is_none());

        net.answers.clear();
        net.hold[S] = false;
        net.server.start(&mut net.effects[S]);
        net.run(&store);
        net.fetch("a", 3);
        net.run(&store);
        assert_eq!(net.links.len(), 2);
        assert_eq!(net.answers.len(), 1);
        assert_eq!(net.answers[0].tag, Tag(3));
        assert_eq!(answer(&net.answers[0]), Answer::of(&store, &key("a")).unwrap());
    }

    #[test]
    fn a_fetch_with_nothing_listening_fails_and_the_next_tries_again() {
        let store = stored(&[("a", Kind::Query, b"a query".as_slice())]);
        let mut net = Net::new(usize::MAX);
        net.server.stop(&mut net.effects[S]);
        net.run(&store);
        net.fetch("a", 1);
        net.run(&store);
        assert_eq!(net.answers.len(), 1);
        assert!(failure(&net.answers[0]).contains("could not connect"));

        net.answers.clear();
        net.server.start(&mut net.effects[S]);
        net.run(&store);
        net.fetch("a", 2);
        net.run(&store);
        assert_eq!(net.answers.len(), 1);
        assert_eq!(answer(&net.answers[0]), Answer::of(&store, &key("a")).unwrap());
    }

    /// A client with fetches of `names` asked and connected as `conn`, its send done.
    fn connected(names: &[&str], conn: Conn) -> (Client, Vec<Effect>, Tag) {
        let mut client = Client::new(socket(), CLIENT);
        let mut effects = Vec::new();
        for (n, name) in names.iter().enumerate() {
            client.fetch(&key(name), Tag(n as u64), &mut effects);
        }
        let [Effect::Connect { tag: connect, .. }] = effects[..] else {
            panic!("expected one connect, got {effects:?}")
        };
        effects.clear();
        let none = client.complete(Completion { tag: connect, done: Done::Connected { conn } }, &mut effects);
        assert!(none.is_empty());
        let send = effect_on(&effects, conn, false).unwrap();
        let recv = effect_on(&effects, conn, true).unwrap();
        let asked: Vec<u8> = names.iter().flat_map(|name| ask(name)).collect();
        assert_eq!(sent_on(&effects, conn), Some(&asked[..]));
        effects.clear();
        assert!(client.complete(Completion { tag: send, done: Done::Sent }, &mut effects).is_empty());
        (client, effects, recv)
    }

    #[test]
    fn a_client_refuses_a_frame_too_long_before_reading_it() {
        let conn = Conn(5);
        let (mut client, mut effects, recv) = connected(&["a", "b"], conn);
        let prefix = (LONGEST_FRAME + 1).to_le_bytes().to_vec();
        let answers = client.complete(Completion { tag: recv, done: Done::Read(prefix) }, &mut effects);
        assert_eq!(answers.len(), 2);
        for answer in &answers {
            assert!(failure(answer).contains("more than"), "{answer:?}");
        }
        assert!(hung_up(&effects, conn));

        effects.clear();
        client.fetch(&key("a"), Tag(9), &mut effects);
        assert!(matches!(effects[..], [Effect::Connect { .. }]), "{effects:?}");
    }

    #[test]
    fn a_long_answer_is_received_in_reads_of_what_it_still_needs() {
        let conn = Conn(4);
        let (mut client, mut effects, recv) = connected(&["a"], conn);
        let bytes = vec![7; 10 << 20];
        let content = ContentId::of("blaze v2 remote test content 1", &bytes);
        let frame = framed(|out| write_value(out, &key("a"), Kind::Code, &content, &bytes));
        let first = Completion { tag: recv, done: Done::Read(frame[..100].to_vec()) };
        assert!(client.complete(first, &mut effects).is_empty());
        let [Effect::Recv { most, tag: rest, .. }] = effects[..] else { panic!("{effects:?}") };
        assert_eq!(most, frame.len() - 100);

        effects.clear();
        let last = Completion { tag: rest, done: Done::Read(frame[100..].to_vec()) };
        let answers = client.complete(last, &mut effects);
        assert_eq!(answers, vec![Completion { tag: Tag(0), done: Done::Read(frame[8..].to_vec()) }]);
        assert!(effects.is_empty(), "nothing is owed, so nothing is received");

        assert_eq!(owed(&(LONGEST_FRAME + 1).to_le_bytes()), 0);
        assert_eq!(owed(&[1, 0, 0]), 0);
    }

    #[test]
    fn a_client_refuses_a_malformed_or_misplaced_answer() {
        let garbage = framed(|out| out.extend_from_slice(b"not an answer"));
        let other_key = framed(|out| write_missing(out, &key("b")));
        let unasked = [framed(|out| write_missing(out, &key("a"))), other_key.clone()].concat();
        for (stream, why, failed) in [
            (garbage, "malformed", 1),
            (other_key, "in place of", 1),
            (unasked, "nothing asked", 0),
        ] {
            let conn = Conn(6);
            let (mut client, mut effects, recv) = connected(&["a"], conn);
            let answers = client.complete(Completion { tag: recv, done: Done::Read(stream) }, &mut effects);
            let failures: Vec<&Completion> =
                answers.iter().filter(|it| matches!(it.done, Done::Failed(_))).collect();
            assert_eq!(failures.len(), failed, "{answers:?}");
            if let Some(first) = failures.first() {
                assert!(failure(first).contains(why), "{first:?}");
            }
            assert!(hung_up(&effects, conn), "{why}: {effects:?}");
        }
    }

    #[test]
    fn a_client_closing_fails_what_waits() {
        let conn = Conn(7);
        let (mut client, mut effects, _) = connected(&["a"], conn);
        let answers = client.close(&mut effects);
        assert_eq!(answers.len(), 1);
        assert!(failure(&answers[0]).contains("closed"));
        assert!(hung_up(&effects, conn));

        let mut client = Client::new(socket(), CLIENT);
        let mut effects = Vec::new();
        client.fetch(&key("a"), Tag(1), &mut effects);
        let [Effect::Connect { tag: connect, .. }] = effects[..] else { panic!("{effects:?}") };
        effects.clear();
        assert_eq!(client.close(&mut effects).len(), 1);
        let late = Completion { tag: connect, done: Done::Connected { conn: Conn(8) } };
        assert!(client.complete(late, &mut effects).is_empty());
        assert!(hung_up(&effects, Conn(8)), "a connection made after closing is hung up");
    }

    /// A started server that has accepted `conns`, and the tag of each one's receive.
    fn serving(conns: &[Conn]) -> (Server, Vec<Effect>, Vec<Tag>) {
        let mut server = Server::new(socket(), SERVER).unwrap();
        let mut effects = Vec::new();
        server.start(&mut effects);
        let [Effect::Listen { tag: listen, .. }] = effects[..] else { panic!("{effects:?}") };
        let store = stored(&[]);
        let mut recvs = Vec::new();
        for conn in conns {
            effects.clear();
            let accepted = Completion { tag: listen, done: Done::Accepted { conn: *conn } };
            assert!(server.complete(accepted, &store, &mut effects).is_empty());
            recvs.push(effect_on(&effects, *conn, true).unwrap());
        }
        effects.clear();
        (server, effects, recvs)
    }

    #[test]
    fn a_server_answers_each_connection_on_its_own() {
        let store = stored(&[
            ("a", Kind::Debug, b"debug of a".as_slice()),
            ("b", Kind::Query, b"a query".as_slice()),
        ]);
        let (one, two) = (Conn(1), Conn(2));
        let (mut server, mut effects, recvs) = serving(&[one, two]);
        for (conn, recv, name) in [(one, recvs[0], "a"), (two, recvs[1], "b")] {
            effects.clear();
            let read = Completion { tag: recv, done: Done::Read(ask(name)) };
            assert!(server.complete(read, &store, &mut effects).is_empty());
            let frame = sent_on(&effects, conn).unwrap();
            assert_eq!(unframe(frame), Answer::of(&store, &key(name)).unwrap());
            assert!(effect_on(&effects, conn, true).is_some(), "it reads on for more asks");
        }
    }

    #[test]
    fn a_server_hangs_up_a_stream_that_is_not_whole_asks() {
        let store = stored(&[("a", Kind::Code, b"code".as_slice())]);
        let (bad, partial) = (Conn(3), Conn(4));
        let (mut server, mut effects, recvs) = serving(&[bad, partial]);

        let mut not_an_ask = ask("a");
        not_an_ask[3] = b'!';
        let read = Completion { tag: recvs[0], done: Done::Read(not_an_ask) };
        server.complete(read, &store, &mut effects);
        assert!(hung_up(&effects, bad));
        assert!(sent_on(&effects, bad).is_none());

        effects.clear();
        let half = ask("a")[..ASK_LEN / 2].to_vec();
        server.complete(Completion { tag: recvs[1], done: Done::Read(half) }, &store, &mut effects);
        assert!(!hung_up(&effects, partial));
        let recv = effect_on(&effects, partial, true).unwrap();
        effects.clear();
        server.complete(Completion { tag: recv, done: Done::Read(Vec::new()) }, &store, &mut effects);
        assert!(hung_up(&effects, partial));
        assert!(sent_on(&effects, partial).is_none());
    }

    #[test]
    fn a_server_refuses_a_tcp_address_that_is_not_loopback() {
        for refused in ["192.0.2.1:9000", "0.0.0.0:9000", "[::]:9000", "[2001:db8::1]:9000"] {
            let at = Address::Tcp(refused.parse().unwrap());
            let error = Server::new(at, SERVER).err().unwrap();
            assert!(error.contains("not loopback"), "{error}");
        }
        for allowed in ["127.0.0.1:9000", "[::1]:9000", "[::ffff:127.0.0.1]:9000"] {
            assert!(Server::new(Address::Tcp(allowed.parse().unwrap()), SERVER).is_ok());
        }
        assert!(Server::new(socket(), SERVER).is_ok());
    }

    #[test]
    fn tags_stay_in_their_range() {
        let mut tags = Tags { base: CLIENT, next: u64::from(u32::MAX) };
        assert_eq!(tags.next(), Tag(CLIENT + u64::from(u32::MAX)));
        assert_eq!(tags.next(), Tag(CLIENT));
        assert!(tags.owns(Tag(CLIENT)));
        assert!(!tags.owns(Tag(CLIENT - 1)));
        assert!(!tags.owns(Tag(SERVER)));
    }
}
