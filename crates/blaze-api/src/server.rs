//! The daemon's listener (docs/v2/plan/lanes/ci.md; #277), a sans-IO state machine over [`blaze_core`]'s
//! sockets: it listens, cuts requests from what peers send, hands the session what
//! [`Arrived`], and sends each [`Reply`] where it goes.
//!
//! - **Unix socket**: frames both ways; many requests per connection, replies in any order,
//!   each naming its request's id.
//! - **HTTP** (remote clients, ix forks): `POST /api` with `Content-Type: application/cbor` and
//!   one unframed CBOR item of an [`Envelope`] of a [`Request`], carrying `Authorization:
//!   Bearer` with the token. The reply is 200, chunked: each response one frame in a chunk
//!   (`application/cbor`), or with `Accept: application/json` one line of [`json::response`]
//!   (`application/x-ndjson`), the body ending after [`Response::Done`]. One request at a time
//!   per connection, kept alive: a pipelined next request waits for the first's end.
//!
//! What a peer sends that is no request is answered here and never reaches the session: an
//! item that does not decode, or of another schema, gets [`Response::Error`] and then
//! [`Response::Done`] with `ok` false under its id (0 when unreadable), and the connection
//! carries on; a frame too long, or bytes that are no HTTP, are answered and then hung up on.
//!
//! One receive at a time per connection, of 64 KiB, or on the Unix socket up to what the frame
//! under way still lacks, at most as much as has come of it; and one send, later replies
//! queueing behind it. A peer that lets more than 64 MiB queue (it stopped reading) is hung up
//! on.

use std::{fmt, net::SocketAddr};

use blaze_core::{Address, Completion, Conn, Done, Effect, FileName, Tag};
use blaze_protocol::cbor;
use rustc_hash::FxHashMap;

use crate::{
    ApiError, Arrived, Envelope, LONGEST_REQUEST, Reply, Request, RequestRef, Response,
    frames::{self, Frames, Refused},
    http::{self, HttpRequest, RequestParser},
    json,
};

/// The most bytes one receive asks for.
const RECEIVE: usize = 64 << 10;
/// The most bytes a connection's replies may queue behind the send in flight.
const MOST_QUEUED: usize = 64 << 20;
/// The most bytes an HTTP connection may hold, pipelined behind the request being answered,
/// before the server stops reading it until that request is done.
const MOST_HELD: usize = LONGEST_REQUEST as usize + (1 << 20);
/// How many tags the server uses, from [`Config::tags`].
const TAGS: u64 = 1 << 32;
/// The most bytes [`Server::line`] keeps between chunks; a longer chunk's buffer is dropped.
const KEPT_LINE: usize = 1 << 20;

#[derive(Clone)]
pub struct Http {
    pub at: SocketAddr,
    /// Bearer token every HTTP request must carry; never logged or echoed.
    pub token: String,
}

impl fmt::Debug for Http {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Http").field("at", &self.at).field("token", &"(hidden)").finish()
    }
}

#[derive(Clone, Debug)]
pub struct Config {
    pub unix: Option<FileName>,
    pub http: Option<Http>,
    /// The tags it uses are `tags..tags + 2^32`.
    pub tags: u64,
    pub most_connections: usize,
}

pub struct Server {
    tags: u64,
    next_tag: u64,
    token: Option<String>,
    most_connections: usize,
    listeners: Vec<Listener>,
    pending: FxHashMap<Tag, Pending>,
    conns: FxHashMap<Conn, Connection>,
    failed: Option<String>,
    stopped: bool,
    /// What arrived, handed over at the end of the next [`Server::complete`].
    told: Vec<Arrived>,
    /// Every reply is encoded with it, onto its connection's queue.
    writer: cbor::Writer,
    /// An HTTP chunk's content, put together before its size is known.
    line: Vec<u8>,
}

impl fmt::Debug for Server {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Server")
            .field("connections", &self.conns.len())
            .field("failed", &self.failed)
            .field("stopped", &self.stopped)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Side {
    Unix,
    Http,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Listening {
    Yes,
    /// An unlisten is asked; its end is coming.
    Ending,
    Ended,
}

struct Listener {
    side: Side,
    at: Address,
    tag: Tag,
    listening: Listening,
}

/// What a tag of the server's was asked for.
enum Pending {
    Listen(usize),
    Recv(Conn),
    Send(Conn),
    Hangup,
}

struct Connection {
    reader: Reader,
    receiving: Option<Tag>,
    sending: Option<Tag>,
    /// Bytes waiting for the send in flight; empty while none is in flight, so a reply is
    /// encoded where it is sent from.
    queue: Vec<u8>,
    /// Hang up once everything queued is sent; nothing more is read.
    closing: bool,
}

enum Reader {
    Unix(Frames),
    Http { parser: RequestParser, answering: Option<Answering> },
}

/// The HTTP request whose responses a connection is sending.
#[derive(Clone, Copy, Debug)]
struct Answering {
    id: u64,
    json: bool,
    /// The client asked for the connection to close after it.
    close: bool,
}

impl Server {
    /// Asks to listen on each address.
    pub fn new(config: Config, effects: &mut Vec<Effect>) -> Server {
        let mut server = Server {
            tags: config.tags,
            next_tag: 0,
            token: None,
            most_connections: config.most_connections,
            listeners: Vec::new(),
            pending: FxHashMap::default(),
            conns: FxHashMap::default(),
            failed: None,
            stopped: false,
            told: Vec::new(),
            writer: cbor::Writer::new(),
            line: Vec::new(),
        };
        if config.unix.is_none() && config.http.is_none() {
            server.fail("the API server was given no address to listen on".to_owned());
        }
        if let Some(name) = config.unix {
            server.listen(Side::Unix, Address::Unix(name), effects);
        }
        if let Some(Http { at, token }) = config.http {
            match token.is_empty() {
                true => server.fail(format!("the API's HTTP listener at {at} has an empty token")),
                false => {
                    server.token = Some(token);
                    server.listen(Side::Http, Address::Tcp(at), effects);
                }
            }
        }
        server
    }

    pub fn owns(&self, tag: Tag) -> bool {
        tag.0.wrapping_sub(self.tags) < TAGS
    }

    /// Why a listener failed, naming its address: the daemon must stop or say so, as a
    /// listener that failed is never listened on again.
    pub fn failed(&self) -> Option<&str> {
        self.failed.as_deref()
    }

    /// Takes a completion of its own; what peers asked goes in `arrived`.
    pub fn complete(
        &mut self,
        completion: Completion,
        effects: &mut Vec<Effect>,
        arrived: &mut Vec<Arrived>,
    ) {
        let Completion { tag, done } = completion;
        match self.pending.remove(&tag) {
            Some(Pending::Listen(at)) => self.listened(at, tag, done, effects),
            Some(Pending::Recv(conn)) => self.received(conn, tag, done, effects),
            Some(Pending::Send(conn)) => self.sent(conn, tag, done, effects),
            Some(Pending::Hangup) | None => {}
        }
        arrived.append(&mut self.told);
    }

    /// Sends a reply; one to a connection that is gone is dropped.
    pub fn reply(&mut self, reply: Reply, effects: &mut Vec<Effect>) {
        let RequestRef { conn, id } = reply.to;
        let Some(connection) = self.conns.get_mut(&conn) else { return };
        let Connection { reader, queue, closing, .. } = connection;
        if *closing {
            return;
        }
        match reader {
            Reader::Unix(_) => frame(&mut self.writer, queue, id, &reply.response),
            Reader::Http { answering, .. } => {
                let Some(now) = *answering else { return };
                if now.id != id {
                    return;
                }
                rendered(&mut self.writer, &mut self.line, queue, id, &reply.response, now.json);
                if matches!(reply.response, Response::Done { .. }) {
                    queue.extend_from_slice(http::LAST_CHUNK);
                    *answering = None;
                    *closing = now.close;
                }
            }
        }
        self.flush(conn, effects);
    }

    /// Unlistens and hangs up every connection.
    pub fn stop(&mut self, effects: &mut Vec<Effect>) {
        self.stopped = true;
        for listener in &mut self.listeners {
            if listener.listening == Listening::Yes {
                listener.listening = Listening::Ending;
                effects.push(Effect::Unlisten { tag: listener.tag });
            }
        }
        let mut conns: Vec<Conn> = self.conns.keys().copied().collect();
        conns.sort_unstable();
        for conn in conns {
            self.gone(conn, effects);
        }
    }

    fn tag(&mut self) -> Tag {
        let tag = Tag(self.tags.wrapping_add(self.next_tag % TAGS));
        self.next_tag += 1;
        tag
    }

    fn fail(&mut self, what: String) {
        match &mut self.failed {
            Some(failed) => {
                failed.push_str("; ");
                failed.push_str(&what);
            }
            None => self.failed = Some(what),
        }
    }

    fn listen(&mut self, side: Side, at: Address, effects: &mut Vec<Effect>) {
        let tag = self.tag();
        self.pending.insert(tag, Pending::Listen(self.listeners.len()));
        effects.push(Effect::Listen { at: at.clone(), tag });
        self.listeners.push(Listener { side, at, tag, listening: Listening::Yes });
    }

    fn listened(&mut self, at: usize, tag: Tag, done: Done, effects: &mut Vec<Effect>) {
        match done {
            Done::Accepted { conn } => {
                self.pending.insert(tag, Pending::Listen(at));
                self.accept(self.listeners[at].side, conn, effects);
            }
            Done::Unlistened => self.listeners[at].listening = Listening::Ended,
            Done::Failed(error) => {
                self.listeners[at].listening = Listening::Ended;
                let address = shown(&self.listeners[at].at);
                self.fail(format!("listening on {address}: {error}"));
            }
            other => {
                self.listeners[at].listening = Listening::Ended;
                let address = shown(&self.listeners[at].at);
                self.fail(format!("listening on {address}: the driver answered {other:?}"));
            }
        }
    }

    fn accept(&mut self, side: Side, conn: Conn, effects: &mut Vec<Effect>) {
        if self.stopped || self.conns.len() >= self.most_connections {
            self.hang_up(conn, effects);
            return;
        }
        let reader = match side {
            Side::Unix => Reader::Unix(Frames::new()),
            Side::Http => Reader::Http {
                parser: RequestParser::new(LONGEST_REQUEST as usize),
                answering: None,
            },
        };
        let connection =
            Connection { reader, receiving: None, sending: None, queue: Vec::new(), closing: false };
        self.conns.insert(conn, connection);
        self.receive(conn, effects);
    }

    /// Asks for the connection's next bytes, unless a receive is in flight or it should not
    /// read now.
    fn receive(&mut self, conn: Conn, effects: &mut Vec<Effect>) {
        let Some(connection) = self.conns.get(&conn) else { return };
        let (most, held) = match &connection.reader {
            Reader::Unix(frames) => (frames.wanted(RECEIVE), false),
            Reader::Http { parser, answering } => {
                (RECEIVE, answering.is_some() && parser.held() > MOST_HELD)
            }
        };
        if connection.receiving.is_some() || connection.closing || held {
            return;
        }
        let tag = self.tag();
        self.pending.insert(tag, Pending::Recv(conn));
        effects.push(Effect::Recv { conn, most, tag });
        if let Some(connection) = self.conns.get_mut(&conn) {
            connection.receiving = Some(tag);
        }
    }

    fn received(&mut self, conn: Conn, tag: Tag, done: Done, effects: &mut Vec<Effect>) {
        let Some(connection) = self.conns.get_mut(&conn) else { return };
        if connection.receiving != Some(tag) {
            return;
        }
        connection.receiving = None;
        let bytes = match done {
            Done::Read(bytes) if !bytes.is_empty() => bytes,
            _ => return self.gone(conn, effects),
        };
        if connection.closing {
            return;
        }
        match &mut connection.reader {
            Reader::Unix(frames) => {
                frames.push_vec(bytes);
                self.unix_items(conn, effects);
            }
            Reader::Http { parser, .. } => {
                parser.push_vec(bytes);
                self.http_requests(conn, effects);
            }
        }
        self.receive(conn, effects);
    }

    fn sent(&mut self, conn: Conn, tag: Tag, done: Done, effects: &mut Vec<Effect>) {
        let Some(connection) = self.conns.get_mut(&conn) else { return };
        if connection.sending != Some(tag) {
            return;
        }
        connection.sending = None;
        if done != Done::Sent {
            return self.gone(conn, effects);
        }
        if !connection.queue.is_empty() {
            let bytes = std::mem::take(&mut connection.queue);
            return self.send(conn, bytes, effects);
        }
        if connection.closing {
            return self.gone(conn, effects);
        }
        self.http_requests(conn, effects);
        self.receive(conn, effects);
    }

    /// The whole items a Unix connection has sent, handed over or answered.
    fn unix_items(&mut self, conn: Conn, effects: &mut Vec<Effect>) {
        loop {
            let Some(Connection { reader: Reader::Unix(frames), closing: false, .. }) =
                self.conns.get_mut(&conn)
            else {
                return;
            };
            let decoded = match frames.next_ref() {
                Ok(None) => return,
                Ok(Some(item)) => Ok(frames::decode::<Request>(item)),
                Err(error) => Err(error),
            };
            match decoded {
                Ok(Ok(envelope)) => self.told.push(Arrived::Request {
                    from: RequestRef { conn, id: envelope.id },
                    request: envelope.body,
                }),
                Ok(Err(refused)) => self.refuse(conn, refused, effects),
                Err(error) => {
                    self.refuse(conn, Refused { id: None, error }, effects);
                    self.close(conn);
                    return;
                }
            }
        }
    }

    /// Answers an item that is no request of this schema, as its request would end.
    fn refuse(&mut self, conn: Conn, refused: Refused, effects: &mut Vec<Effect>) {
        let id = refused.id.unwrap_or(0);
        let Some(connection) = self.conns.get_mut(&conn) else { return };
        for response in [Response::Error(refused.error), Response::Done { ok: false }] {
            frame(&mut self.writer, &mut connection.queue, id, &response);
        }
        self.flush(conn, effects);
    }

    /// The whole requests an HTTP connection has sent, while none is being answered.
    fn http_requests(&mut self, conn: Conn, effects: &mut Vec<Effect>) {
        loop {
            let Some(Connection {
                reader: Reader::Http { parser, answering: None },
                closing: false,
                ..
            }) = self.conns.get_mut(&conn)
            else {
                return;
            };
            let request = match parser.next() {
                Ok(Some(request)) => request,
                Ok(None) => return,
                Err(error) => {
                    let bytes = plain(error.status, &[("Connection", "close")], &error.what);
                    self.queue(conn, &bytes, effects);
                    self.close(conn);
                    return;
                }
            };
            self.http_request(conn, request, effects);
        }
    }

    fn http_request(&mut self, conn: Conn, request: HttpRequest, effects: &mut Vec<Effect>) {
        let close = request.header("Connection").is_some_and(|it| {
            it.split(',').any(|option| option.trim().eq_ignore_ascii_case("close"))
        });
        let authorized = self.token.as_deref().is_some_and(|token| authorized(&request, token));
        let refusal = if !authorized {
            let challenge = [("WWW-Authenticate", "Bearer")];
            Some(plain(401, &challenge, "a missing or wrong Authorization: Bearer token"))
        } else if request.path != "/api" {
            Some(plain(404, &[], &format!("no such path as {}; the API is POST /api", request.path)))
        } else if request.method != "POST" {
            Some(plain(405, &[("Allow", "POST")], "the API is POST /api"))
        } else if !media_is(request.header("Content-Type"), &["application/cbor"]) {
            Some(plain(415, &[], "a body that is not application/cbor"))
        } else {
            None
        };
        if let Some(bytes) = refusal {
            self.queue(conn, &bytes, effects);
            if close {
                self.close(conn);
            }
            return;
        }
        let json = request
            .header("Accept")
            .is_some_and(|accept| media_is(Some(accept), &["application/json", "application/x-ndjson"]));
        let kind = if json { "application/x-ndjson" } else { "application/cbor" };
        let Some(Connection { reader: Reader::Http { answering, .. }, queue, .. }) =
            self.conns.get_mut(&conn)
        else {
            return;
        };
        http::chunked_into(queue, 200, &[("Content-Type", kind)]);
        match frames::decode::<Request>(&request.body) {
            Ok(envelope) => {
                *answering = Some(Answering { id: envelope.id, json, close });
                self.told.push(Arrived::Request {
                    from: RequestRef { conn, id: envelope.id },
                    request: envelope.body,
                });
                self.flush(conn, effects);
            }
            Err(refused) => {
                let id = refused.id.unwrap_or(0);
                for response in [Response::Error(refused.error), Response::Done { ok: false }] {
                    rendered(&mut self.writer, &mut self.line, queue, id, &response, json);
                }
                queue.extend_from_slice(http::LAST_CHUNK);
                self.flush(conn, effects);
                if close {
                    self.close(conn);
                }
            }
        }
    }

    /// Sends `bytes` now, or after the send in flight.
    fn queue(&mut self, conn: Conn, bytes: &[u8], effects: &mut Vec<Effect>) {
        let Some(connection) = self.conns.get_mut(&conn) else { return };
        connection.queue.extend_from_slice(bytes);
        self.flush(conn, effects);
    }

    /// Sends what is queued now when no send is in flight; else it waits for that send, and a
    /// peer with too much waiting is hung up on.
    fn flush(&mut self, conn: Conn, effects: &mut Vec<Effect>) {
        let Some(connection) = self.conns.get_mut(&conn) else { return };
        let waiting = connection.queue.len();
        match connection.sending {
            None if waiting > 0 => {
                let bytes = std::mem::take(&mut connection.queue);
                self.send(conn, bytes, effects);
            }
            Some(_) if waiting > MOST_QUEUED => self.gone(conn, effects),
            _ => {}
        }
    }

    fn send(&mut self, conn: Conn, bytes: Vec<u8>, effects: &mut Vec<Effect>) {
        let tag = self.tag();
        let Some(connection) = self.conns.get_mut(&conn) else { return };
        connection.sending = Some(tag);
        self.pending.insert(tag, Pending::Send(conn));
        effects.push(Effect::Send { conn, bytes, tag });
    }

    /// The connection is hung up on once what is queued for it is sent.
    fn close(&mut self, conn: Conn) {
        if let Some(connection) = self.conns.get_mut(&conn) {
            connection.closing = true;
        }
    }

    /// The connection is over: the session hears so, and it is hung up on.
    fn gone(&mut self, conn: Conn, effects: &mut Vec<Effect>) {
        if self.conns.remove(&conn).is_some() {
            self.told.push(Arrived::Gone { conn });
            self.hang_up(conn, effects);
        }
    }

    fn hang_up(&mut self, conn: Conn, effects: &mut Vec<Effect>) {
        let tag = self.tag();
        self.pending.insert(tag, Pending::Hangup);
        effects.push(Effect::Hangup { conn, tag });
    }
}

/// `response` to request `id` as one frame. One too long to frame is sent as the error that
/// says so.
fn frame(id: u64, response: Response) -> Vec<u8> {
    match frames::encode(&Envelope::new(id, response)) {
        Ok(bytes) => bytes,
        Err(error) => {
            let what = format!("a response that could not be sent: {}", error.what);
            let error = Response::Error(ApiError::new(what, error.at));
            frames::encode(&Envelope::new(id, error)).expect("an error's envelope has a frame")
        }
    }
}

/// `response` to request `id` as one chunk of an HTTP reply.
fn rendered(id: u64, response: Response, json: bool) -> Vec<u8> {
    match json {
        true => {
            let mut line = json::response(&Envelope::new(id, response));
            line.push('\n');
            http::chunk(line.as_bytes())
        }
        false => http::chunk(&frame(id, response)),
    }
}

/// A whole response of `status` saying `what`, for a request that reaches no session.
fn plain(status: u16, extra: &[(&str, &str)], what: &str) -> Vec<u8> {
    let mut headers = vec![("Content-Type", "text/plain; charset=utf-8")];
    headers.extend_from_slice(extra);
    http::response(status, &headers, what.as_bytes())
}

fn authorized(request: &HttpRequest, token: &str) -> bool {
    let Some(value) = request.header("Authorization") else { return false };
    let Some((scheme, credentials)) = value.split_once(' ') else { return false };
    scheme.eq_ignore_ascii_case("bearer") && same(credentials.trim().as_bytes(), token.as_bytes())
}

/// Whether `given` is `secret`, in a time that depends only on `given`'s length.
fn same(given: &[u8], secret: &[u8]) -> bool {
    let mut differ = u8::from(given.len() != secret.len());
    for (at, &byte) in given.iter().enumerate() {
        differ |= byte ^ secret.get(at).copied().unwrap_or(0);
    }
    std::hint::black_box(differ) == 0
}

/// Whether a Content-Type or Accept header names one of `kinds`, parameters aside.
fn media_is(header: Option<&str>, kinds: &[&str]) -> bool {
    header.is_some_and(|header| {
        header.split(',').any(|range| {
            let kind = range.split(';').next().unwrap_or_default().trim();
            kinds.iter().any(|it| kind.eq_ignore_ascii_case(it))
        })
    })
}

fn shown(at: &Address) -> String {
    match at {
        Address::Unix(name) => format!("the socket {name}"),
        Address::Tcp(at) => at.to_string(),
    }
}

#[cfg(test)]
mod tests;
