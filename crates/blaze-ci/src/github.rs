//! The GitHub App: its JWT, its installation token and the check runs, over TLS verified
//! against the machine's roots.
//!
//! No token, JWT or key is ever in an error, a log or a `Debug`.

use std::{
    fmt,
    io::{self, Read, Write},
    net::{TcpStream, ToSocketAddrs},
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use base64::{
    Engine,
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
};
use blaze_api::http::{self, HttpResponse};
use ring::{
    rand::SystemRandom,
    signature::{RSA_PKCS1_SHA256, RsaKeyPair},
};
use rustls::{ClientConfig, ClientConnection, RootCertStore, StreamOwned, pki_types::ServerName};
use serde::Deserialize;
use serde_json::json;

use crate::{CheckRun, CheckStatus};

/// GitHub's cap on a check run's summary, in characters.
pub const MOST_SUMMARY: usize = 65535;

const CUT: &str = "\n\n(cut here: the summary was longer than GitHub's 65535 characters)";

/// How long a connect, a read or a write may take.
const TIMEOUT: Duration = Duration::from_secs(30);

/// The most bytes of a GitHub answer read.
const MOST_ANSWER: usize = 8 << 20;

/// A connection idle longer than this is not reused: the server may be closing it.
const KEEP_IDLE: Duration = Duration::from_secs(20);

/// A token is minted again this long before it expires.
const RENEW_BEFORE: u64 = 5 * 60;

/// The App's private key; Debug and Display never show it.
pub struct Key(RsaKeyPair);

impl Key {
    /// "RSA PRIVATE KEY" (PKCS#1, what GitHub issues) or "PRIVATE KEY" (PKCS#8).
    pub fn from_pem(pem: &str) -> Result<Key, GitHubError> {
        let mut lines = pem.lines().map(str::trim).skip_while(|line| line.is_empty());
        let begin = lines.next().unwrap_or_default();
        let label = begin
            .strip_prefix("-----BEGIN ")
            .and_then(|it| it.strip_suffix("-----"))
            .ok_or_else(|| GitHubError::new("the App's key is not PEM: no -----BEGIN line first"))?;
        let end = format!("-----END {label}-----");
        let mut base64 = String::new();
        let mut ended = false;
        for line in lines.by_ref() {
            if line == end {
                ended = true;
                break;
            }
            if line.contains(':') {
                return Err(GitHubError::new(
                    "the App's key PEM has headers, as an encrypted key does; give it unencrypted",
                ));
            }
            base64.push_str(line);
        }
        if !ended {
            return Err(GitHubError::new(format!("the App's key PEM has no {end} line")));
        }
        if lines.any(|line| !line.is_empty()) {
            return Err(GitHubError::new("the App's key PEM has more after its end line"));
        }
        let der = STANDARD
            .decode(base64.as_bytes())
            .map_err(|_| GitHubError::new(format!("the App's key PEM's {label} is not base64")))?;
        let pair = match label {
            "RSA PRIVATE KEY" => RsaKeyPair::from_der(&der),
            "PRIVATE KEY" => RsaKeyPair::from_pkcs8(&der),
            _ => {
                return Err(GitHubError::new(format!(
                    "the App's key PEM is a {label}, not an RSA PRIVATE KEY or a PRIVATE KEY"
                )));
            }
        };
        pair.map(Key)
            .map_err(|error| GitHubError::new(format!("the App's {label} was refused: {error}")))
    }

    /// The JWT the App authenticates as at `now` (Unix seconds): RS256, issued a minute back
    /// for clock drift, expiring in nine minutes (GitHub's limit is ten).
    fn jwt(&self, app: u64, now: u64) -> Result<String, GitHubError> {
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"RS256","typ":"JWT"}"#);
        let claims = format!(r#"{{"iat":{},"exp":{},"iss":{app}}}"#, now.saturating_sub(60), now + 540);
        let mut jwt = format!("{header}.{}", URL_SAFE_NO_PAD.encode(claims));
        let mut signature = vec![0; self.0.public().modulus_len()];
        self.0
            .sign(&RSA_PKCS1_SHA256, &SystemRandom::new(), jwt.as_bytes(), &mut signature)
            .map_err(|_| GitHubError::new("signing the App's JWT failed"))?;
        jwt.push('.');
        jwt.push_str(&URL_SAFE_NO_PAD.encode(signature));
        Ok(jwt)
    }
}

impl fmt::Debug for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Key(hidden)")
    }
}

impl fmt::Display for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the App's private key")
    }
}

#[derive(Debug)]
pub struct App {
    pub id: u64,
    pub installation: u64,
    pub key: Key,
    /// The API's host, "api.github.com".
    pub api: String,
}

/// An installation token; Debug never shows it.
#[derive(Clone)]
pub struct Token(String);

impl Token {
    pub fn new(token: String) -> Token {
        Token(token)
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Token(hidden)")
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct GitHubError {
    pub what: String,
    /// GitHub's HTTP status, when it answered.
    pub status: Option<u16>,
}

impl GitHubError {
    fn new(what: impl Into<String>) -> GitHubError {
        GitHubError { what: what.into(), status: None }
    }
}

impl fmt::Display for GitHubError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.status {
            Some(status) => write!(f, "{} (GitHub answered {status})", self.what),
            None => f.write_str(&self.what),
        }
    }
}

impl std::error::Error for GitHubError {}

/// What the service needs of GitHub, so it can be tested with a fake.
pub trait Checks {
    /// Creates the check run on its commit; returns its id.
    fn create(&mut self, run: &CheckRun) -> Result<u64, GitHubError>;
    fn update(&mut self, id: u64, run: &CheckRun) -> Result<(), GitHubError>;
}

/// The App, installed on one repository.
pub struct GitHub {
    app: App,
    repo: String,
    tls: Arc<ClientConfig>,
    /// The token and when it expires, in Unix seconds.
    token: Option<(Token, u64)>,
    kept: Option<Kept>,
}

/// The connection of the last answer, kept for the next request.
struct Kept {
    stream: StreamOwned<ClientConnection, TcpStream>,
    idle_since: Instant,
}

/// Why an exchange failed. `stale`: nothing came back, as when the server closed the
/// connection while it was idle.
struct Failed {
    what: String,
    stale: bool,
}

fn exchange(
    stream: &mut StreamOwned<ClientConnection, TcpStream>,
    request: &[u8],
) -> Result<HttpResponse, Failed> {
    if let Err(error) = stream.write_all(request).and_then(|()| stream.flush()) {
        return Err(Failed { what: format!("sending: {error}"), stale: true });
    }
    let mut counted = Counted { from: stream, read: 0 };
    http::read_response(&mut counted, MOST_ANSWER).map_err(|error| Failed {
        what: format!("reading the answer: {error}"),
        stale: counted.read == 0,
    })
}

/// A reader that counts what it gave.
struct Counted<'a, R> {
    from: &'a mut R,
    read: usize,
}

impl<R: Read> Read for Counted<'_, R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let read = self.from.read(buffer)?;
        self.read += read;
        Ok(read)
    }
}

impl GitHub {
    /// `repo` is "owner/name". The machine's roots are loaded here: none is an error.
    pub fn new(app: App, repo: String) -> Result<GitHub, GitHubError> {
        let part = |it: &str| {
            !it.is_empty()
                && it != "."
                && it != ".."
                && it.bytes().all(|byte| byte.is_ascii_alphanumeric() || b"-_.".contains(&byte))
        };
        if !matches!(repo.split_once('/'), Some((owner, name)) if part(owner) && part(name)) {
            return Err(GitHubError::new(format!("{repo:?} is not a repository's owner/name")));
        }
        if app.api.is_empty()
            || !app.api.bytes().all(|byte| byte.is_ascii_alphanumeric() || b"-.".contains(&byte))
        {
            return Err(GitHubError::new(format!("{:?} is not a host name for GitHub's API", app.api)));
        }
        Ok(GitHub { app, repo, tls: tls()?, token: None, kept: None })
    }

    /// The installation token, minted (and kept until five minutes before it expires).
    pub fn token(&mut self) -> Result<Token, GitHubError> {
        let now = now()?;
        if let Some((token, expires)) = &self.token
            && now + RENEW_BEFORE < *expires
        {
            return Ok(token.clone());
        }
        self.token = None;
        let jwt = self.app.key.jwt(self.app.id, now)?;
        let path = format!("/app/installations/{}/access_tokens", self.app.installation);
        let answer = self.call("POST", &path, &format!("Bearer {jwt}"), b"", true)?;
        let minted: Minted = serde_json::from_slice(&answer.body).map_err(|_| {
            GitHubError::new("GitHub's answer to minting a token is not a token and its expires_at")
        })?;
        let expires = unix_seconds(&minted.expires_at)?;
        let token = Token(minted.token);
        self.token = Some((token.clone(), expires));
        Ok(token)
    }

    /// One request, on the kept connection when it is fresh; a non-2xx answer is an error with
    /// GitHub's `message`. A kept connection the server closed while idle is replaced and the
    /// request sent again only when `replayable`: sending it twice must change nothing.
    fn call(
        &mut self,
        method: &str,
        path: &str,
        authorization: &str,
        body: &[u8],
        replayable: bool,
    ) -> Result<HttpResponse, GitHubError> {
        let what = format!("{method} https://{}{path}", self.app.api);
        let mut headers = vec![
            ("Authorization", authorization),
            ("Accept", "application/vnd.github+json"),
            ("X-GitHub-Api-Version", "2022-11-28"),
            ("User-Agent", "blaze-ci"),
        ];
        if !body.is_empty() {
            headers.push(("Content-Type", "application/json"));
        }
        let request = http::request(method, &self.app.api, path, &headers, body);
        let connect = |github: &GitHub| {
            github.connect().map_err(|error| GitHubError::new(format!("{what}: {error}")))
        };
        let kept = self.kept.take().filter(|it| it.idle_since.elapsed() < KEEP_IDLE);
        let reused = kept.is_some();
        let mut stream = match kept {
            Some(kept) => kept.stream,
            None => connect(&*self)?,
        };
        let mut exchanged = exchange(&mut stream, &request);
        if reused && replayable && matches!(exchanged, Err(Failed { stale: true, .. })) {
            stream = connect(&*self)?;
            exchanged = exchange(&mut stream, &request);
        }
        let answer = exchanged.map_err(|failed| GitHubError::new(format!("{what}: {}", failed.what)))?;
        let closes = answer
            .header("Connection")
            .is_some_and(|it| it.split(',').any(|token| token.trim().eq_ignore_ascii_case("close")));
        if !closes {
            self.kept = Some(Kept { stream, idle_since: Instant::now() });
        }
        if (200..300).contains(&answer.status) {
            return Ok(answer);
        }
        let message = serde_json::from_slice::<Message>(&answer.body)
            .map_or_else(|_| "no message".to_owned(), |it| it.message);
        Err(GitHubError { what: format!("{what}: {message}"), status: Some(answer.status) })
    }

    fn connect(&self) -> Result<StreamOwned<ClientConnection, TcpStream>, String> {
        let addresses = (self.app.api.as_str(), 443)
            .to_socket_addrs()
            .map_err(|error| format!("resolving {}: {error}", self.app.api))?;
        let mut last = format!("{} resolved to no address", self.app.api);
        let mut connected = None;
        for address in addresses {
            match TcpStream::connect_timeout(&address, TIMEOUT) {
                Ok(tcp) => {
                    connected = Some(tcp);
                    break;
                }
                Err(error) => last = format!("connecting to {address}: {error}"),
            }
        }
        let tcp = connected.ok_or(last)?;
        tcp.set_read_timeout(Some(TIMEOUT))
            .and_then(|()| tcp.set_write_timeout(Some(TIMEOUT)))
            .map_err(|error| format!("setting timeouts: {error}"))?;
        let name = ServerName::try_from(self.app.api.clone())
            .map_err(|error| format!("{:?} as a TLS server name: {error}", self.app.api))?;
        let tls = ClientConnection::new(Arc::clone(&self.tls), name)
            .map_err(|error| format!("starting TLS: {error}"))?;
        Ok(StreamOwned::new(tls, tcp))
    }
}

impl Checks for GitHub {
    fn create(&mut self, run: &CheckRun) -> Result<u64, GitHubError> {
        let token = self.token()?;
        let body = check_run(run, true).to_string();
        let path = format!("/repos/{}/check-runs", self.repo);
        let answer =
            self.call("POST", &path, &format!("token {}", token.expose()), body.as_bytes(), false)?;
        let created: Created = serde_json::from_slice(&answer.body).map_err(|error| {
            GitHubError::new(format!("GitHub's answer to creating check run {}: {error}", run.name))
        })?;
        Ok(created.id)
    }

    fn update(&mut self, id: u64, run: &CheckRun) -> Result<(), GitHubError> {
        let token = self.token()?;
        let body = check_run(run, false).to_string();
        let path = format!("/repos/{}/check-runs/{id}", self.repo);
        self.call("PATCH", &path, &format!("token {}", token.expose()), body.as_bytes(), true)?;
        Ok(())
    }
}

/// A check run's JSON. `head_sha` is only taken when one is created.
fn check_run(run: &CheckRun, create: bool) -> serde_json::Value {
    let (status, conclusion, title) = match run.status {
        CheckStatus::Queued => ("queued", None, "Queued"),
        CheckStatus::InProgress => ("in_progress", None, "Running"),
        CheckStatus::Success => ("completed", Some("success"), "Passed"),
        CheckStatus::Failure => ("completed", Some("failure"), "Failed"),
        CheckStatus::Cancelled => ("completed", Some("cancelled"), "Cancelled"),
    };
    let mut body = json!({
        "name": run.name,
        "status": status,
        "output": { "title": title, "summary": summary(&run.summary) },
    });
    if create {
        body["head_sha"] = json!(run.commit);
    }
    if let Some(conclusion) = conclusion {
        body["conclusion"] = json!(conclusion);
    }
    body
}

/// `text` cut to GitHub's limit at a char boundary, saying it was cut. Bytes are counted,
/// never fewer than characters, so the cut text is within the limit however GitHub counts.
fn summary(text: &str) -> String {
    if text.len() <= MOST_SUMMARY {
        return text.to_owned();
    }
    let mut at = MOST_SUMMARY - CUT.len();
    while !text.is_char_boundary(at) {
        at -= 1;
    }
    format!("{}{CUT}", &text[..at])
}

/// A client config with the machine's roots, the ring provider, TLS 1.2 and 1.3.
fn tls() -> Result<Arc<ClientConfig>, GitHubError> {
    let found = rustls_native_certs::load_native_certs();
    let mut roots = RootCertStore::empty();
    let (added, _) = roots.add_parsable_certificates(found.certs);
    if added == 0 {
        let errors: Vec<String> = found.errors.iter().map(ToString::to_string).collect();
        return Err(GitHubError::new(format!(
            "no root certificate to verify GitHub with was found on this machine ({})",
            if errors.is_empty() { "no errors loading them".to_owned() } else { errors.join("; ") },
        )));
    }
    let config = ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .map_err(|error| GitHubError::new(format!("TLS versions: {error}")))?
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(Arc::new(config))
}

fn now() -> Result<u64, GitHubError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|it| it.as_secs())
        .map_err(|_| GitHubError::new("the clock is before 1970"))
}

/// An RFC 3339 time in UTC (`2016-07-11T22:14:10Z`, fractions of a second allowed) as Unix
/// seconds. Another offset is refused.
fn unix_seconds(time: &str) -> Result<u64, GitHubError> {
    let refuse = |why: &str| GitHubError::new(format!("expires_at {time:?} {why}"));
    let bytes = time.as_bytes();
    if bytes.len() < 19 {
        return Err(refuse("is not an RFC 3339 time"));
    }
    let digits = |at: usize, len: usize| -> Result<u64, GitHubError> {
        let part = &bytes[at..at + len];
        if !part.iter().all(u8::is_ascii_digit) {
            return Err(refuse("is not an RFC 3339 time"));
        }
        Ok(part.iter().fold(0, |sum, digit| sum * 10 + u64::from(digit - b'0')))
    };
    for (at, separator) in [(4, b'-'), (7, b'-'), (10, b'T'), (13, b':'), (16, b':')] {
        if bytes[at] != separator && !(at == 10 && bytes[at] == b't') {
            return Err(refuse("is not an RFC 3339 time"));
        }
    }
    let (year, month, day) = (digits(0, 4)?, digits(5, 2)?, digits(8, 2)?);
    let (hour, minute, second) = (digits(11, 2)?, digits(14, 2)?, digits(17, 2)?);
    let mut rest = &bytes[19..];
    if let Some(fraction) = rest.strip_prefix(b".") {
        let len = fraction.iter().take_while(|it| it.is_ascii_digit()).count();
        if len == 0 {
            return Err(refuse("has a fraction of a second with no digits"));
        }
        rest = &fraction[len..];
    }
    match rest {
        b"Z" | b"z" => {}
        b"" => return Err(refuse("has no offset; GitHub's are UTC (Z)")),
        _ => return Err(refuse("is not in UTC (Z); no other offset is taken")),
    }
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let days_in_month = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => return Err(refuse("has no such month")),
    };
    if year < 1970 || day == 0 || day > days_in_month || hour > 23 || minute > 59 || second > 60 {
        return Err(refuse("is no such time"));
    }
    Ok(days_since_epoch(year, month, day) * 86400 + hour * 3600 + minute * 60 + second)
}

/// Days from 1970-01-01 to a date of the proleptic Gregorian calendar, from 1970 on.
fn days_since_epoch(year: u64, month: u64, day: u64) -> u64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year / 400;
    let year_of_era = year - era * 400;
    let day_of_year = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

#[derive(Deserialize)]
struct Minted {
    token: String,
    expires_at: String,
}

#[derive(Deserialize)]
struct Created {
    id: u64,
}

#[derive(Deserialize)]
struct Message {
    message: String,
}

#[cfg(test)]
mod tests;
