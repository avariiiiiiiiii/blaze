//! A GitHub webhook delivery: its signature checked against the App's secret, then its payload
//! read into an [`Event`]. Nothing in a payload is trusted before [`verify`] passes.

use std::{borrow::Cow, fmt};

use blaze_api::http::{HttpRequest, HttpRequestRef};
use ring::hmac;
use serde::Deserialize;

use crate::Event;

/// GitHub's cap on a delivery's payload.
pub const MOST_PAYLOAD: usize = 25 << 20;

/// A delivery whose signature checked. `event` is `None` for one CI ignores (a ping, a closed
/// PR, a deleted branch).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Delivery {
    pub id: String,
    /// "owner/name"; empty for a delivery about no repository (an App's ping).
    pub repo: String,
    pub event: Option<Event>,
}

/// Why a delivery was refused, with the HTTP status to answer.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct WebhookError {
    pub status: u16,
    pub what: String,
}

impl WebhookError {
    fn new(status: u16, what: impl Into<String>) -> WebhookError {
        WebhookError { status, what: what.into() }
    }
}

impl fmt::Display for WebhookError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({})", self.what, self.status)
    }
}

impl std::error::Error for WebhookError {}

/// `X-Hub-Signature-256` ("sha256=<hex>") checked against HMAC-SHA256 of `body` under `secret`,
/// in constant time (ring::hmac::verify).
pub fn verify(secret: &[u8], body: &[u8], signature: Option<&str>) -> Result<(), WebhookError> {
    if secret.is_empty() {
        return Err(WebhookError::new(500, "the webhook secret is empty, so no signature can check"));
    }
    let signature =
        signature.ok_or_else(|| WebhookError::new(400, "the delivery has no X-Hub-Signature-256"))?;
    let tag = signature
        .strip_prefix("sha256=")
        .and_then(hex)
        .ok_or_else(|| WebhookError::new(401, "X-Hub-Signature-256 is not sha256=<64 hex digits>"))?;
    let key = hmac::Key::new(hmac::HMAC_SHA256, secret);
    hmac::verify(&key, body, &tag).map_err(|_| {
        WebhookError::new(401, "X-Hub-Signature-256 does not match the body under the webhook secret")
    })
}

/// The 32 bytes of 64 hex digits, on the stack.
fn hex(text: &str) -> Option<[u8; 32]> {
    let bytes = text.as_bytes();
    if bytes.len() != 64 {
        return None;
    }
    let digit = |byte: u8| char::from(byte).to_digit(16);
    let mut tag = [0; 32];
    for (out, pair) in tag.iter_mut().zip(bytes.chunks_exact(2)) {
        *out = (digit(pair[0])? * 16 + digit(pair[1])?) as u8;
    }
    Some(tag)
}

/// The payload of event kind `kind` (`X-GitHub-Event`), delivery `id` (`X-GitHub-Delivery`).
///
/// One pass over the body: serde reads only the fields below, skipping the rest of the payload
/// without building it, and borrows each string from the body unless it has escapes. The only
/// copies are the [`Delivery`]'s own strings.
pub fn parse(kind: &str, id: &str, body: &[u8]) -> Result<Delivery, WebhookError> {
    if body.len() > MOST_PAYLOAD {
        return Err(WebhookError::new(
            413,
            format!("the payload has {} bytes, over GitHub's cap of {MOST_PAYLOAD}", body.len()),
        ));
    }
    if id.is_empty() {
        return Err(WebhookError::new(400, "the delivery's X-GitHub-Delivery is empty"));
    }
    let id = id.to_owned();
    match kind {
        "push" => {
            let push: PushPayload<'_> = payload(kind, body)?;
            let repo = push.repository.full_name.into_owned();
            if push.deleted {
                return Ok(Delivery { id, repo, event: None });
            }
            let commit = sha(kind, "after", &push.after)?;
            let event = if let Some(branch) = push.reference.strip_prefix("refs/heads/") {
                Some(Event::Push { branch: name(kind, "ref", branch)?, commit })
            } else if let Some(tag) = push.reference.strip_prefix("refs/tags/") {
                Some(Event::Tag { name: name(kind, "ref", tag)?, commit })
            } else {
                None
            };
            Ok(Delivery { id, repo, event })
        }
        "pull_request" => {
            let pr: PullRequestPayload<'_> = payload(kind, body)?;
            if !matches!(&*pr.action, "opened" | "synchronize" | "reopened" | "ready_for_review") {
                let repo = pr.repository.map(|it| it.full_name.into_owned()).unwrap_or_default();
                return Ok(Delivery { id, repo, event: None });
            }
            let missing =
                |field: &str| WebhookError::new(400, format!("the {kind} payload has no {field}"));
            let repository = pr.repository.ok_or_else(|| missing("repository"))?;
            let number = pr.number.ok_or_else(|| missing("number"))?;
            let pull = pr.pull_request.ok_or_else(|| missing("pull_request"))?;
            let base_repo = pull.base.repo.ok_or_else(|| missing("pull_request.base.repo"))?;
            let from_fork = pull.head.repo.is_none_or(|head| head.full_name != base_repo.full_name);
            let event = Event::PullRequest {
                number,
                head: sha(kind, "pull_request.head.sha", &pull.head.sha)?,
                base: sha(kind, "pull_request.base.sha", &pull.base.sha)?,
                from_fork,
            };
            Ok(Delivery { id, repo: repository.full_name.into_owned(), event: Some(event) })
        }
        _ => {
            let other: OtherPayload<'_> = payload(kind, body)?;
            let repo = other.repository.map(|it| it.full_name.into_owned()).unwrap_or_default();
            Ok(Delivery { id, repo, event: None })
        }
    }
}

/// [`verify`] then [`parse`], from the request's headers. Only `POST`.
pub fn receive(secret: &[u8], request: &HttpRequest) -> Result<Delivery, WebhookError> {
    receive_parts(secret, &request.method, |name| request.header(name), &request.body)
}

/// [`receive`] of a request read in place ([`RequestParser::next_ref`]): its head and body
/// are neither copied nor allocated.
///
/// [`RequestParser::next_ref`]: blaze_api::http::RequestParser::next_ref
pub fn receive_ref(secret: &[u8], request: &HttpRequestRef<'_>) -> Result<Delivery, WebhookError> {
    receive_parts(secret, request.method, |name| request.header(name), request.body)
}

fn receive_parts<'a>(
    secret: &[u8],
    method: &str,
    header: impl Fn(&str) -> Option<&'a str>,
    body: &[u8],
) -> Result<Delivery, WebhookError> {
    if method != "POST" {
        return Err(WebhookError::new(405, format!("a webhook delivery is a POST, this was a {method}")));
    }
    verify(secret, body, header("X-Hub-Signature-256"))?;
    if let Some(kind) = header("Content-Type") {
        let essence = kind.split(';').next().unwrap_or_default().trim();
        if !essence.eq_ignore_ascii_case("application/json") {
            return Err(WebhookError::new(
                415,
                format!("the webhook sends {kind}; set its content type to application/json"),
            ));
        }
    }
    let kind = header("X-GitHub-Event")
        .ok_or_else(|| WebhookError::new(400, "the delivery has no X-GitHub-Event"))?;
    let id = header("X-GitHub-Delivery")
        .ok_or_else(|| WebhookError::new(400, "the delivery has no X-GitHub-Delivery"))?;
    parse(kind, id, body)
}

fn payload<'a, T: Deserialize<'a>>(kind: &str, body: &'a [u8]) -> Result<T, WebhookError> {
    serde_json::from_slice(body)
        .map_err(|error| WebhookError::new(400, format!("the {kind} payload is not one: {error}")))
}

/// A commit's full hex name, SHA-1 or SHA-256.
fn sha(kind: &str, field: &str, sha: &str) -> Result<String, WebhookError> {
    let hex = sha.bytes().all(|it| matches!(it, b'0'..=b'9' | b'a'..=b'f'));
    if hex && matches!(sha.len(), 40 | 64) {
        Ok(sha.to_owned())
    } else {
        Err(WebhookError::new(400, format!("the {kind} payload's {field} {sha:?} is not a commit")))
    }
}

fn name(kind: &str, field: &str, name: &str) -> Result<String, WebhookError> {
    if name.is_empty() || name.bytes().any(|it| it.is_ascii_control()) {
        Err(WebhookError::new(400, format!("the {kind} payload's {field} {name:?} names nothing")))
    } else {
        Ok(name.to_owned())
    }
}

#[derive(Deserialize)]
struct Repository<'a> {
    #[serde(borrow)]
    full_name: Cow<'a, str>,
}

#[derive(Deserialize)]
struct PushPayload<'a> {
    #[serde(rename = "ref", borrow)]
    reference: Cow<'a, str>,
    #[serde(borrow)]
    after: Cow<'a, str>,
    #[serde(default)]
    deleted: bool,
    #[serde(borrow)]
    repository: Repository<'a>,
}

/// Every action's payload: the fields past `action` are required only of the actions CI runs.
#[derive(Deserialize)]
struct PullRequestPayload<'a> {
    #[serde(borrow)]
    action: Cow<'a, str>,
    number: Option<u64>,
    #[serde(borrow)]
    pull_request: Option<PullRequest<'a>>,
    #[serde(borrow)]
    repository: Option<Repository<'a>>,
}

#[derive(Deserialize)]
struct PullRequest<'a> {
    #[serde(borrow)]
    head: Side<'a>,
    #[serde(borrow)]
    base: Side<'a>,
}

#[derive(Deserialize)]
struct Side<'a> {
    #[serde(borrow)]
    sha: Cow<'a, str>,
    /// Null when the fork the head is in was deleted.
    #[serde(borrow)]
    repo: Option<Repository<'a>>,
}

#[derive(Deserialize)]
struct OtherPayload<'a> {
    #[serde(borrow)]
    repository: Option<Repository<'a>>,
}

#[cfg(test)]
mod tests;
