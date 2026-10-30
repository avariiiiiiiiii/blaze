//! HTTP/1.1, sans-IO: requests and responses cut from bytes as they arrive, and written as
//! bytes. For the API's HTTP side ([`crate::server`], [`crate::client`]), CI's webhook and its
//! GitHub client.
//!
//! Strict: a head line over 8 KiB, more than 100 headers, a body over the caller's bound
//! (413), both a length and chunks, another transfer coding, a bad chunk size or a line not
//! ended by CRLF is refused, naming what. Nothing is allocated on the peer's word before its
//! bytes are here.

use std::{fmt, io};

/// The longest line of a head or of a chunk's size, without its CRLF.
const LONGEST_LINE: usize = 8 << 10;
/// The most header lines of a head, and of a chunked body's trailers.
const MOST_HEADERS: usize = 100;

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct HttpRequest {
    pub method: String,
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl HttpRequest {
    /// The first header named `name`, in any case.
    pub fn header(&self, name: &str) -> Option<&str> {
        header(&self.headers, name)
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct HttpResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl HttpResponse {
    /// The first header named `name`, in any case.
    pub fn header(&self, name: &str) -> Option<&str> {
        header(&self.headers, name)
    }
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers.iter().find(|(it, _)| it.eq_ignore_ascii_case(name)).map(|(_, value)| value.as_str())
}

/// Bytes refused: the status a server answers them with, and what was wrong.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct HttpError {
    pub status: u16,
    pub what: String,
}

impl HttpError {
    fn new(status: u16, what: impl Into<String>) -> HttpError {
        HttpError { status, what: what.into() }
    }
}

impl fmt::Display for HttpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "HTTP {} {}: {}", self.status, reason(self.status), self.what)
    }
}

impl std::error::Error for HttpError {}

/// Requests cut from a connection's bytes, one after another (pipelined requests wait in it).
#[derive(Debug)]
pub struct RequestParser {
    most_body: usize,
    bytes: Vec<u8>,
    state: RequestState,
    broken: Option<HttpError>,
}

#[derive(Debug)]
enum RequestState {
    Head(HeadScan),
    Body { method: String, path: String, headers: Vec<(String, String)>, body: BodyRead },
}

#[derive(Debug)]
enum BodyRead {
    /// This many bytes, starting at the start of the parser's bytes.
    Length(usize),
    Chunked { chunks: Chunks, body: Vec<u8> },
}

impl RequestParser {
    pub fn new(most_body: usize) -> RequestParser {
        RequestParser {
            most_body,
            bytes: Vec::new(),
            state: RequestState::Head(HeadScan::default()),
            broken: None,
        }
    }

    pub fn push(&mut self, bytes: &[u8]) {
        if self.broken.is_none() {
            self.bytes.extend_from_slice(bytes);
        }
    }

    /// [`RequestParser::push`] of a buffer that is the caller's no more: kept as it is,
    /// uncopied, when nothing is held before it.
    pub(crate) fn push_vec(&mut self, bytes: Vec<u8>) {
        match self.bytes.is_empty() && self.broken.is_none() {
            true => self.bytes = bytes,
            false => self.push(&bytes),
        }
    }

    /// The next whole request (Content-Length or chunked body), `Ok(None)` until one is here.
    /// After an error every call gives it again: the connection is not in step any more.
    pub fn next(&mut self) -> Result<Option<HttpRequest>, HttpError> {
        if let Some(error) = &self.broken {
            return Err(error.clone());
        }
        self.step().inspect_err(|error| {
            self.broken = Some(error.clone());
            self.bytes = Vec::new();
        })
    }

    /// How many bytes it holds: of the request it is reading and of any after it.
    pub(crate) fn held(&self) -> usize {
        self.bytes.len()
    }

    fn step(&mut self) -> Result<Option<HttpRequest>, HttpError> {
        loop {
            match &mut self.state {
                RequestState::Head(scan) => {
                    let Some(end) = scan.scan(&self.bytes)? else { return Ok(None) };
                    let start = scan.start;
                    let (method, path, headers) = request_head(&self.bytes[start..end])?;
                    let body = match framing(&headers, self.most_body)? {
                        Framing::Length(len) => BodyRead::Length(len),
                        Framing::Chunked => {
                            BodyRead::Chunked { chunks: Chunks::Size, body: Vec::new() }
                        }
                        Framing::None => BodyRead::Length(0),
                    };
                    self.bytes.drain(..end);
                    self.state = RequestState::Body { method, path, headers, body };
                }
                RequestState::Body { body: BodyRead::Length(len), .. } => {
                    let len = *len;
                    if self.bytes.len() < len {
                        return Ok(None);
                    }
                    let rest = self.bytes.split_off(len);
                    let body = std::mem::replace(&mut self.bytes, rest);
                    return Ok(Some(self.finish(body)));
                }
                RequestState::Body { body: BodyRead::Chunked { chunks, body }, .. } => {
                    let mut at = 0;
                    let mut decoded = body.len();
                    let done = chunks.decode(&self.bytes, &mut at, body, &mut decoded, self.most_body);
                    self.bytes.drain(..at);
                    if !done? {
                        return Ok(None);
                    }
                    let body = std::mem::take(body);
                    return Ok(Some(self.finish(body)));
                }
            }
        }
    }

    fn finish(&mut self, body: Vec<u8>) -> HttpRequest {
        let state = std::mem::replace(&mut self.state, RequestState::Head(HeadScan::default()));
        let RequestState::Body { method, path, headers, .. } = state else {
            unreachable!("a request is finished only once its head is read")
        };
        HttpRequest { method, path, headers, body }
    }
}

/// A response cut from a connection's bytes as they arrive: its head, then its body as it
/// comes (chunks decoded). A push of no bytes is the end of the stream, which ends a body
/// that has neither a length nor chunks. Interim responses (1xx but 101) are skipped. A
/// response to `HEAD` is not told apart: it is read as having the body its head names.
#[derive(Debug)]
pub struct ResponseParser {
    most_body: usize,
    bytes: Vec<u8>,
    scan: HeadScan,
    head: Option<(u16, Vec<(String, String)>)>,
    body: ResponseBody,
    out: Vec<u8>,
    decoded: usize,
    ended: bool,
    broken: Option<HttpError>,
}

#[derive(Debug)]
enum ResponseBody {
    Length(usize),
    Chunked(Chunks),
    UntilEnd,
    Done,
}

impl ResponseParser {
    pub fn new(most_body: usize) -> ResponseParser {
        ResponseParser {
            most_body,
            bytes: Vec::new(),
            scan: HeadScan::default(),
            head: None,
            body: ResponseBody::Done,
            out: Vec::new(),
            decoded: 0,
            ended: false,
            broken: None,
        }
    }

    pub fn push(&mut self, bytes: &[u8]) -> Result<(), HttpError> {
        if let Some(error) = &self.broken {
            return Err(error.clone());
        }
        match bytes.is_empty() {
            true => self.ended = true,
            false => self.bytes.extend_from_slice(bytes),
        }
        self.step().inspect_err(|error| {
            self.broken = Some(error.clone());
            self.bytes = Vec::new();
        })
    }

    pub fn head(&self) -> Option<(u16, &[(String, String)])> {
        self.head.as_ref().map(|(status, headers)| (*status, headers.as_slice()))
    }

    /// Body bytes decoded since the last call.
    pub fn take_body(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.out)
    }

    pub fn finished(&self) -> bool {
        self.head.is_some() && matches!(self.body, ResponseBody::Done)
    }

    /// The bytes after the end of the response, once it is finished: the next response's.
    pub(crate) fn rest(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.bytes)
    }

    fn step(&mut self) -> Result<(), HttpError> {
        while self.head.is_none() {
            let Some(end) = self.scan.scan(&self.bytes)? else {
                if self.ended {
                    return Err(HttpError::new(502, "the stream ended inside a response's head"));
                }
                return Ok(());
            };
            let (status, headers) = response_head(&self.bytes[self.scan.start..end])?;
            self.bytes.drain(..end);
            self.scan = HeadScan::default();
            if (100..200).contains(&status) && status != 101 {
                continue;
            }
            self.body = match status {
                101 | 204 | 304 => ResponseBody::Done,
                _ => match framing(&headers, self.most_body)? {
                    Framing::Length(0) => ResponseBody::Done,
                    Framing::Length(len) => ResponseBody::Length(len),
                    Framing::Chunked => ResponseBody::Chunked(Chunks::Size),
                    Framing::None => ResponseBody::UntilEnd,
                },
            };
            self.head = Some((status, headers));
        }
        let cut_short = || HttpError::new(502, "the stream ended inside a response's body");
        match &mut self.body {
            ResponseBody::Length(left) => {
                let here = self.bytes.len().min(*left);
                match here == self.bytes.len() && self.out.is_empty() {
                    true => std::mem::swap(&mut self.out, &mut self.bytes),
                    false => self.out.extend(self.bytes.drain(..here)),
                }
                *left -= here;
                if *left == 0 {
                    self.body = ResponseBody::Done;
                } else if self.ended {
                    return Err(cut_short());
                }
            }
            ResponseBody::Chunked(chunks) => {
                let mut at = 0;
                let done = chunks.decode(
                    &self.bytes,
                    &mut at,
                    &mut self.out,
                    &mut self.decoded,
                    self.most_body,
                );
                self.bytes.drain(..at);
                if done? {
                    self.body = ResponseBody::Done;
                } else if self.ended {
                    return Err(cut_short());
                }
            }
            ResponseBody::UntilEnd => {
                self.decoded += self.bytes.len();
                if self.decoded > self.most_body {
                    let said = format!("a body of more than {} bytes", self.most_body);
                    return Err(HttpError::new(413, said));
                }
                match self.out.is_empty() {
                    true => std::mem::swap(&mut self.out, &mut self.bytes),
                    false => self.out.append(&mut self.bytes),
                }
                if self.ended {
                    self.body = ResponseBody::Done;
                }
            }
            ResponseBody::Done => {}
        }
        Ok(())
    }
}

/// Finds where a head ends as its bytes arrive, holding its lines to the limits. Blank lines
/// before the start line are skipped (RFC 9112 section 2.2).
#[derive(Default, Debug)]
struct HeadScan {
    /// How far the bytes have been looked at.
    at: usize,
    line_start: usize,
    /// Where the start line begins.
    start: usize,
    lines: usize,
}

impl HeadScan {
    /// Where the head ends in `bytes` (past its blank line), once it is whole.
    fn scan(&mut self, bytes: &[u8]) -> Result<Option<usize>, HttpError> {
        let too_long = || HttpError::new(431, "a head line longer than 8 KiB");
        loop {
            let Some(found) = memchr::memchr(b'\n', &bytes[self.at..]) else {
                self.at = bytes.len();
                if self.at - self.line_start > LONGEST_LINE + 1 {
                    return Err(too_long());
                }
                return Ok(None);
            };
            let end = self.at + found;
            if end - self.line_start > LONGEST_LINE + 1 {
                return Err(too_long());
            }
            self.at = end + 1;
            let line = &bytes[self.line_start..end];
            let Some(line) = line.strip_suffix(b"\r") else {
                return Err(HttpError::new(400, "a head line ended by LF without CR"));
            };
            let empty = line.is_empty();
            self.line_start = self.at;
            match (empty, self.lines) {
                (true, 0) => self.start = self.at,
                (true, _) => return Ok(Some(self.at)),
                (false, _) => {
                    self.lines += 1;
                    if self.lines > MOST_HEADERS + 1 {
                        return Err(HttpError::new(431, "more than 100 headers"));
                    }
                }
            }
        }
    }
}

/// The lines of a whole head, its blank line left off.
fn lines(head: &[u8]) -> Result<Vec<&str>, HttpError> {
    let head = head.strip_suffix(b"\r\n\r\n").unwrap_or(head);
    let head =
        std::str::from_utf8(head).map_err(|_| HttpError::new(400, "a head that is not UTF-8"))?;
    let lines: Vec<&str> = head.split("\r\n").collect();
    if lines.iter().any(|line| line.contains('\r')) {
        return Err(HttpError::new(400, "a CR inside a head line"));
    }
    Ok(lines)
}

fn request_head(head: &[u8]) -> Result<(String, String, Vec<(String, String)>), HttpError> {
    let lines = lines(head)?;
    let mut parts = lines[0].split(' ');
    let (Some(method), Some(path), Some(version), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(HttpError::new(400, "a request line that is not METHOD PATH VERSION"));
    };
    if method.is_empty() || !method.bytes().all(is_tchar) {
        return Err(HttpError::new(400, "a method that is not a token"));
    }
    if path.is_empty() {
        return Err(HttpError::new(400, "an empty request target"));
    }
    match version {
        "HTTP/1.1" | "HTTP/1.0" => {}
        _ if version.starts_with("HTTP/") => {
            let said = format!("{version}; this speaks HTTP/1.1");
            return Err(HttpError::new(505, said));
        }
        _ => return Err(HttpError::new(400, "a request line whose version is not HTTP/1.x")),
    }
    let headers: Vec<(String, String)> =
        lines[1..].iter().map(|line| header_line(line)).collect::<Result<_, _>>()?;
    Ok((method.to_owned(), path.to_owned(), headers))
}

fn response_head(head: &[u8]) -> Result<(u16, Vec<(String, String)>), HttpError> {
    let lines = lines(head)?;
    let bad = || HttpError::new(502, "a status line that is not HTTP/1.x STATUS REASON");
    let (version, rest) = lines[0].split_once(' ').ok_or_else(bad)?;
    if !matches!(version, "HTTP/1.1" | "HTTP/1.0") {
        return Err(bad());
    }
    let status = rest.split_once(' ').map_or(rest, |(status, _)| status);
    if status.len() != 3 || !status.bytes().all(|it| it.is_ascii_digit()) {
        return Err(bad());
    }
    let status = status.parse().map_err(|_| bad())?;
    let headers: Vec<(String, String)> =
        lines[1..].iter().map(|line| header_line(line)).collect::<Result<_, _>>()?;
    Ok((status, headers))
}

fn header_line(line: &str) -> Result<(String, String), HttpError> {
    if line.starts_with([' ', '\t']) {
        return Err(HttpError::new(400, "a folded header line"));
    }
    let Some((name, value)) = line.split_once(':') else {
        return Err(HttpError::new(400, "a header line without a colon"));
    };
    if name.is_empty() || !name.bytes().all(is_tchar) {
        return Err(HttpError::new(400, "a header name that is not a token"));
    }
    Ok((name.to_owned(), value.trim_matches([' ', '\t']).to_owned()))
}

fn is_tchar(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte)
}

enum Framing {
    Length(usize),
    Chunked,
    None,
}

fn framing(headers: &[(String, String)], most_body: usize) -> Result<Framing, HttpError> {
    let mut length = None;
    let mut chunked = false;
    for (name, value) in headers {
        if name.eq_ignore_ascii_case("content-length") {
            if length.is_some() {
                return Err(HttpError::new(400, "more than one Content-Length"));
            }
            let number = !value.is_empty() && value.bytes().all(|it| it.is_ascii_digit());
            let len = value.parse::<u64>().ok().filter(|_| number);
            let len = len.ok_or_else(|| HttpError::new(400, "a Content-Length that is not a number"))?;
            length = Some(len);
        } else if name.eq_ignore_ascii_case("transfer-encoding") {
            if chunked {
                return Err(HttpError::new(400, "more than one Transfer-Encoding"));
            }
            if !value.eq_ignore_ascii_case("chunked") {
                let said = format!("the transfer coding {value:?}; only chunked is read");
                return Err(HttpError::new(501, said));
            }
            chunked = true;
        }
    }
    match (length, chunked) {
        (Some(_), true) => {
            Err(HttpError::new(400, "both Content-Length and Transfer-Encoding: chunked"))
        }
        (Some(len), false) => match usize::try_from(len).ok().filter(|&len| len <= most_body) {
            Some(len) => Ok(Framing::Length(len)),
            None => {
                let said = format!("a body of {len} bytes, more than {most_body}");
                Err(HttpError::new(413, said))
            }
        },
        (None, true) => Ok(Framing::Chunked),
        (None, false) => Ok(Framing::None),
    }
}

/// Where a chunked body's decoding is.
#[derive(Debug)]
enum Chunks {
    Size,
    Data(usize),
    /// The CRLF after a chunk's data.
    DataEnd,
    /// Trailer lines read so far, after the last chunk.
    Trailers(usize),
    Done,
}

impl Chunks {
    /// Decodes what of `bytes[*at..]` it can into `out`, moving `at` past it; true once the
    /// last chunk and its trailers are read. `decoded` counts the body's bytes, which may be
    /// at most `most`. Trailers are read and dropped.
    fn decode(
        &mut self,
        bytes: &[u8],
        at: &mut usize,
        out: &mut Vec<u8>,
        decoded: &mut usize,
        most: usize,
    ) -> Result<bool, HttpError> {
        loop {
            match *self {
                Chunks::Size => {
                    let Some((line, next)) = line_at(bytes, *at)? else { return Ok(false) };
                    let size = chunk_size(line)?;
                    *at = next;
                    *self = match size {
                        0 => Chunks::Trailers(0),
                        _ => match usize::try_from(size)
                            .ok()
                            .filter(|&size| size <= most.saturating_sub(*decoded))
                        {
                            Some(size) => Chunks::Data(size),
                            None => {
                                let said = format!("a chunked body of more than {most} bytes");
                                return Err(HttpError::new(413, said));
                            }
                        },
                    };
                }
                Chunks::Data(left) => {
                    let here = (bytes.len() - *at).min(left);
                    if here == 0 {
                        return Ok(false);
                    }
                    out.extend_from_slice(&bytes[*at..*at + here]);
                    *at += here;
                    *decoded += here;
                    *self = match left - here {
                        0 => Chunks::DataEnd,
                        left => Chunks::Data(left),
                    };
                }
                Chunks::DataEnd => match bytes.get(*at..*at + 2) {
                    None => return Ok(false),
                    Some(b"\r\n") => {
                        *at += 2;
                        *self = Chunks::Size;
                    }
                    Some(_) => return Err(HttpError::new(400, "a chunk not ended by CRLF")),
                },
                Chunks::Trailers(count) => {
                    let Some((line, next)) = line_at(bytes, *at)? else { return Ok(false) };
                    *at = next;
                    if line.is_empty() {
                        *self = Chunks::Done;
                        return Ok(true);
                    }
                    if count == MOST_HEADERS {
                        return Err(HttpError::new(431, "more than 100 trailers"));
                    }
                    *self = Chunks::Trailers(count + 1);
                }
                Chunks::Done => return Ok(true),
            }
        }
    }
}

/// The line starting at `from`, without its CRLF, and where the next starts; `None` until
/// its end is here.
fn line_at(bytes: &[u8], from: usize) -> Result<Option<(&[u8], usize)>, HttpError> {
    let rest = &bytes[from..];
    match rest.iter().take(LONGEST_LINE + 2).position(|&it| it == b'\n') {
        Some(end) => match rest[..end].strip_suffix(b"\r") {
            Some(line) => Ok(Some((line, from + end + 1))),
            None => Err(HttpError::new(400, "a chunk line ended by LF without CR")),
        },
        None if rest.len() > LONGEST_LINE + 1 => {
            Err(HttpError::new(431, "a chunk line longer than 8 KiB"))
        }
        None => Ok(None),
    }
}

/// A chunk's size line: hexadecimal digits, then any extensions, which are dropped.
fn chunk_size(line: &[u8]) -> Result<u64, HttpError> {
    let digits = line.split(|&it| it == b';').next().unwrap_or_default();
    let digits = digits.trim_ascii_end();
    if digits.is_empty() || digits.len() > 16 || !digits.iter().all(u8::is_ascii_hexdigit) {
        return Err(HttpError::new(400, "a bad chunk size"));
    }
    let digit = |it: u8| u64::from((it as char).to_digit(16).unwrap_or_default());
    Ok(digits.iter().fold(0, |size, &it| (size << 4) | digit(it)))
}

fn reason(status: u16) -> &'static str {
    match status {
        100 => "Continue",
        101 => "Switching Protocols",
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        204 => "No Content",
        304 => "Not Modified",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        411 => "Length Required",
        413 => "Content Too Large",
        415 => "Unsupported Media Type",
        422 => "Unprocessable Content",
        429 => "Too Many Requests",
        431 => "Request Header Fields Too Large",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        505 => "HTTP Version Not Supported",
        _ => "",
    }
}

/// A head onto the end of `out`: its start line, `first`, `headers`, `last`, the blank line.
fn head_into(
    out: &mut Vec<u8>,
    start: &[&str],
    first: Option<(&str, &str)>,
    headers: &[(&str, &str)],
    last: (&str, &str),
) {
    for part in start {
        out.extend_from_slice(part.as_bytes());
    }
    out.extend_from_slice(b"\r\n");
    for (name, value) in first.iter().chain(headers).chain([&last]) {
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(b": ");
        out.extend_from_slice(value.as_bytes());
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(b"\r\n");
}

/// `n` in decimal, as the digits it ends `digits` with.
fn decimal(n: usize, digits: &mut [u8; 20]) -> &str {
    let mut at = digits.len();
    let mut n = n;
    loop {
        at -= 1;
        digits[at] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    std::str::from_utf8(&digits[at..]).expect("decimal digits are UTF-8")
}

/// A response's start line, `HTTP/1.1 200 OK`, as the parts [`head_into`] writes.
fn status_line(status: u16, digits: &mut [u8; 20]) -> [&str; 4] {
    ["HTTP/1.1 ", decimal(usize::from(status), digits), " ", reason(status)]
}

/// A whole response, with its Content-Length.
pub fn response(status: u16, headers: &[(&str, &str)], body: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let (mut digits, mut len) = ([0; 20], [0; 20]);
    let start = status_line(status, &mut digits);
    let length = ("Content-Length", decimal(body.len(), &mut len));
    head_into(&mut out, &start, None, headers, length);
    out.extend_from_slice(body);
    out
}

/// The head of a response whose body comes in [`chunk`]s, ended by [`last_chunk`].
pub fn chunked(status: u16, headers: &[(&str, &str)]) -> Vec<u8> {
    let mut out = Vec::new();
    chunked_into(&mut out, status, headers);
    out
}

/// [`chunked`] onto the end of `out`.
pub fn chunked_into(out: &mut Vec<u8>, status: u16, headers: &[(&str, &str)]) {
    let mut digits = [0; 20];
    let start = status_line(status, &mut digits);
    head_into(out, &start, None, headers, ("Transfer-Encoding", "chunked"));
}

/// `bytes` as one chunk; no bytes give no output, never the last chunk.
pub fn chunk(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    chunk_into(&mut out, bytes);
    out
}

/// [`chunk`] onto the end of `out`.
pub fn chunk_into(out: &mut Vec<u8>, bytes: &[u8]) {
    let len = bytes.len();
    if len == 0 {
        return;
    }
    let digits = (usize::BITS - len.leading_zeros()).div_ceil(4);
    out.reserve(digits as usize + len + 4);
    for digit in (0..digits).rev() {
        out.push(b"0123456789abcdef"[(len >> (digit * 4)) & 0xf]);
    }
    out.extend_from_slice(b"\r\n");
    out.extend_from_slice(bytes);
    out.extend_from_slice(b"\r\n");
}

/// The chunk that ends a chunked body, with no trailers.
pub const LAST_CHUNK: &[u8] = b"0\r\n\r\n";

pub fn last_chunk() -> Vec<u8> {
    LAST_CHUNK.to_vec()
}

/// A whole request to `host`, with its Content-Length.
pub fn request(
    method: &str,
    host: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> Vec<u8> {
    let mut out = Vec::new();
    request_head_into(&mut out, method, host, path, headers, body.len());
    out.extend_from_slice(body);
    out
}

/// The head of [`request`] of a body of `len` bytes onto the end of `out`, for the body to be
/// written after it from where it lies.
pub(crate) fn request_head_into(
    out: &mut Vec<u8>,
    method: &str,
    host: &str,
    path: &str,
    headers: &[(&str, &str)],
    len: usize,
) {
    let mut digits = [0; 20];
    let start = [method, " ", path, " HTTP/1.1"];
    let length = ("Content-Length", decimal(len, &mut digits));
    head_into(out, &start, Some(("Host", host)), headers, length);
}

/// Blocking: one whole response from `from`. What `from` gives past the response's end is
/// dropped, so the peer must send nothing more until asked (one request per connection).
pub fn read_response(from: &mut impl io::Read, most_body: usize) -> io::Result<HttpResponse> {
    let mut parser = ResponseParser::new(most_body);
    let mut body = Vec::new();
    let mut buffer = vec![0; 64 << 10];
    while !parser.finished() {
        let read = match from.read(&mut buffer) {
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        parser.push(&buffer[..read]).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        body.extend(parser.take_body());
        if read == 0 && !parser.finished() {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
    }
    let Some((status, headers)) = parser.head() else {
        return Err(io::ErrorKind::UnexpectedEof.into());
    };
    Ok(HttpResponse { status, headers: headers.to_vec(), body })
}

#[cfg(test)]
mod tests;
