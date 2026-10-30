//! The CLI's and CI's side of the API, blocking: requests out, each response back by its
//! request's id, over the daemon's Unix socket or HTTP ([`crate::server`] says how each is
//! spoken). Responses of other requests that come while one is waited for are kept for
//! [`Client::receive`].

use std::{
    collections::VecDeque,
    fmt,
    io::{self, Read},
    net::{SocketAddr, TcpStream},
    path::Path,
};

use blaze_protocol::cbor;

use crate::{
    ApiError, Envelope, LONGEST_REQUEST, Request, Response, SCHEMA,
    frames::{self, Frames},
    http::{self, ResponseParser},
};

/// The most bytes one read takes.
const RECEIVE: usize = 64 << 10;

#[derive(Debug)]
pub enum ClientError {
    Io(io::Error),
    /// The daemon's bytes were refused, or it answered over HTTP with an error.
    Refused(ApiError),
    /// The daemon speaks schema `theirs`.
    Schema { theirs: u32 },
}

impl fmt::Display for ClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ClientError::Io(error) => write!(f, "the daemon's connection: {error}"),
            ClientError::Refused(ApiError { what, at: Some(at) }) => write!(f, "{what} (at {at})"),
            ClientError::Refused(ApiError { what, at: None }) => f.write_str(what),
            ClientError::Schema { theirs } => {
                write!(f, "the daemon speaks schema {theirs}, this client {SCHEMA}")
            }
        }
    }
}

impl std::error::Error for ClientError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ClientError::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<io::Error> for ClientError {
    fn from(error: io::Error) -> ClientError {
        ClientError::Io(error)
    }
}

pub struct Client {
    link: Link,
    /// The daemon's bytes, as frames.
    frames: Frames,
    next_id: u64,
    kept: VecDeque<Envelope<Response>>,
    /// Every request is encoded with it, into `out`.
    writer: cbor::Writer,
    /// The request being sent: its frame, or over HTTP its item.
    out: Vec<u8>,
    /// An HTTP request's head, written before `out` in one write.
    head: Vec<u8>,
}

impl fmt::Debug for Client {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let over = match &self.link {
            #[cfg(unix)]
            Link::Unix(_) => "unix",
            Link::Http(_) => "http",
        };
        f.debug_struct("Client").field("over", &over).field("next_id", &self.next_id).finish()
    }
}

enum Link {
    #[cfg(unix)]
    Unix(std::os::unix::net::UnixStream),
    Http(HttpLink),
}

struct HttpLink {
    stream: TcpStream,
    host: String,
    /// The value of the Authorization header, the token in it.
    authorization: String,
    /// The response being read, of the one request outstanding.
    response: Option<ResponseParser>,
    /// Whether the response's head has been looked at.
    checked: bool,
    /// The request whose responses are being read, until its Done.
    waiting: Option<u64>,
    /// Bytes read past the end of the last response.
    rest: Vec<u8>,
    /// What each read reads into.
    buffer: Vec<u8>,
}

impl Client {
    #[cfg(unix)]
    pub fn unix(path: &Path) -> Result<Client, ClientError> {
        let stream = std::os::unix::net::UnixStream::connect(path)
            .map_err(|error| io::Error::new(error.kind(), format!("{}: {error}", path.display())))?;
        Ok(Client::over(Link::Unix(stream)))
    }

    pub fn http(at: SocketAddr, token: String) -> Result<Client, ClientError> {
        let stream = TcpStream::connect(at)
            .map_err(|error| io::Error::new(error.kind(), format!("{at}: {error}")))?;
        stream.set_nodelay(true)?;
        Ok(Client::over(Link::Http(HttpLink {
            stream,
            host: at.to_string(),
            authorization: format!("Bearer {token}"),
            response: None,
            checked: false,
            waiting: None,
            rest: Vec::new(),
            buffer: vec![0; RECEIVE],
        })))
    }

    fn over(link: Link) -> Client {
        Client {
            link,
            frames: Frames::new(),
            next_id: 1,
            kept: VecDeque::new(),
            writer: cbor::Writer::new(),
            out: Vec::new(),
            head: Vec::new(),
        }
    }

    /// Sends `request`; returns its id (chosen by the client, increasing).
    pub fn send(&mut self, request: Request) -> Result<u64, ClientError> {
        if matches!(self.link, Link::Http(_)) {
            self.drain()?;
        }
        let id = self.next_id;
        self.next_id += 1;
        let envelope = Envelope::new(id, request);
        self.out.clear();
        match &mut self.link {
            #[cfg(unix)]
            Link::Unix(stream) => {
                frames::encode_into(&mut self.writer, &mut self.out, &envelope)
                    .map_err(ClientError::Refused)?;
                io::Write::write_all(stream, &self.out)?;
            }
            Link::Http(link) => {
                self.writer.append(&mut self.out, &envelope).map_err(|error| {
                    ClientError::Refused(ApiError::new(format!("a request with no encoding: {error}"), None))
                })?;
                let item = self.out.as_slice();
                if item.len() > LONGEST_REQUEST as usize {
                    let said = format!("a request of {} bytes, longer than {LONGEST_REQUEST}", item.len());
                    return Err(ClientError::Refused(ApiError::new(said, None)));
                }
                let headers = [
                    ("Authorization", link.authorization.as_str()),
                    ("Content-Type", "application/cbor"),
                    ("Accept", "application/cbor"),
                ];
                let head = &mut self.head;
                head.clear();
                http::request_head_into(head, "POST", &link.host, "/api", &headers, item.len());
                blaze_protocol::write_both(&mut link.stream, head, item)?;
                let mut response = ResponseParser::new(usize::MAX);
                let rest = std::mem::take(&mut link.rest);
                if !rest.is_empty() {
                    response.push(&rest).map_err(invalid)?;
                }
                link.response = Some(response);
                link.checked = false;
                link.waiting = Some(id);
            }
        }
        Ok(id)
    }

    /// The next response of any request sent.
    pub fn receive(&mut self) -> Result<Envelope<Response>, ClientError> {
        match self.kept.pop_front() {
            Some(envelope) => Ok(envelope),
            None => self.read(),
        }
    }

    /// Sends `request` and hands each of its responses to `on` until its Done; returns Done's
    /// `ok`. Responses of other requests that arrive meanwhile are kept for
    /// [`Client::receive`].
    pub fn call(
        &mut self,
        request: Request,
        mut on: impl FnMut(&Response),
    ) -> Result<bool, ClientError> {
        let id = self.send(request)?;
        loop {
            let envelope = self.read()?;
            if envelope.id != id {
                self.kept.push_back(envelope);
                continue;
            }
            on(&envelope.body);
            if let Response::Done { ok } = envelope.body {
                return Ok(ok);
            }
        }
    }

    /// The next response off the connection.
    fn read(&mut self) -> Result<Envelope<Response>, ClientError> {
        loop {
            if let Some(envelope) = self.next_envelope()? {
                return Ok(envelope);
            }
            if !self.more()? {
                let said = "nothing more is coming: every request sent over HTTP has had its Done";
                return Err(ClientError::Io(io::Error::new(io::ErrorKind::InvalidInput, said)));
            }
        }
    }

    /// Reads the rest of the HTTP response outstanding, keeping its responses.
    fn drain(&mut self) -> Result<(), ClientError> {
        loop {
            while let Some(envelope) = self.next_envelope()? {
                self.kept.push_back(envelope);
            }
            if !self.more()? {
                return Ok(());
            }
        }
    }

    /// The next whole response in the frames, decoded where it lies.
    fn next_envelope(&mut self) -> Result<Option<Envelope<Response>>, ClientError> {
        let Some(item) = self.frames.next_ref().map_err(ClientError::Refused)? else {
            return Ok(None);
        };
        let envelope = frames::decode::<Response>(item).map_err(|refused| match frames::head(item) {
            Ok((theirs, _)) if theirs != SCHEMA => ClientError::Schema { theirs },
            _ => ClientError::Refused(refused.error),
        })?;
        if let Link::Http(link) = &mut self.link
            && link.waiting == Some(envelope.id)
            && matches!(envelope.body, Response::Done { .. })
        {
            link.waiting = None;
        }
        Ok(envelope)
    }

    /// Reads more of the daemon's bytes into the frames; false once an HTTP response has
    /// ended and nothing more is coming until the next request.
    fn more(&mut self) -> Result<bool, ClientError> {
        let link = match &mut self.link {
            #[cfg(unix)]
            Link::Unix(stream) => {
                let most = self.frames.wanted(RECEIVE);
                if self.frames.fill(stream, most)? == 0 {
                    let said = "the daemon hung up";
                    return Err(ClientError::Io(io::Error::new(io::ErrorKind::UnexpectedEof, said)));
                }
                return Ok(true);
            }
            Link::Http(link) => link,
        };
        let Some(response) = &mut link.response else { return Ok(false) };
        if response.finished() {
            let rest = response.rest();
            link.response = None;
            link.rest = rest;
            if let Some(id) = link.waiting.take() {
                let said = format!("the daemon's answer to request {id} ended before its Done");
                return Err(ClientError::Refused(ApiError::new(said, Some(url(&link.host)))));
            }
            if self.frames.held() != 0 {
                let said = "the daemon's answer ended inside a frame";
                return Err(ClientError::Refused(ApiError::new(said, Some(url(&link.host)))));
            }
            return Ok(false);
        }
        let bytes = read_some(&mut link.stream, &mut link.buffer)?;
        response.push(bytes).map_err(invalid)?;
        if !link.checked
            && let Some((status, headers)) = response.head()
        {
            link.checked = true;
            if status != 200 {
                let mut body = response.take_body();
                while !response.finished() {
                    let bytes = read_some(&mut link.stream, &mut link.buffer)?;
                    response.push(bytes).map_err(invalid)?;
                    body.extend(response.take_body());
                }
                link.rest = response.rest();
                link.response = None;
                link.waiting = None;
                let body = String::from_utf8_lossy(&body);
                let said = format!("HTTP {status}: {}", body.trim());
                return Err(ClientError::Refused(ApiError::new(said, Some(url(&link.host)))));
            }
            let kind = http_header(headers, "Content-Type").unwrap_or_default();
            if !kind.eq_ignore_ascii_case("application/cbor") {
                let said = format!("an answer of Content-Type {kind:?}, not application/cbor");
                return Err(ClientError::Refused(ApiError::new(said, Some(url(&link.host)))));
            }
        }
        self.frames.push_vec(response.take_body());
        Ok(true)
    }
}

fn http_header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers.iter().find(|(it, _)| it.eq_ignore_ascii_case(name)).map(|(_, value)| value.as_str())
}

fn url(host: &str) -> String {
    format!("http://{host}/api")
}

fn invalid(error: http::HttpError) -> ClientError {
    ClientError::Io(io::Error::new(io::ErrorKind::InvalidData, error))
}

/// What one read into `buffer` gives: empty once the peer has hung up.
fn read_some<'a>(from: &mut impl Read, buffer: &'a mut [u8]) -> io::Result<&'a [u8]> {
    loop {
        match from.read(buffer) {
            Ok(read) => return Ok(&buffer[..read]),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
}

#[cfg(all(test, unix))]
mod tests;
