//! A GitHub webhook delivery: its signature checked against the App's secret, then its payload
//! read into an [`Event`]. Nothing in a payload is trusted before [`verify`] passes.

use std::fmt;

use blaze_api::http::HttpRequest;
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

fn hex(text: &str) -> Option<Vec<u8>> {
    if text.len() != 64 {
        return None;
    }
    let digit = |byte: u8| char::from(byte).to_digit(16);
    text.as_bytes()
        .chunks_exact(2)
        .map(|pair| Some((digit(pair[0])? * 16 + digit(pair[1])?) as u8))
        .collect()
}

/// The payload of event kind `kind` (`X-GitHub-Event`), delivery `id` (`X-GitHub-Delivery`).
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
            let push: PushPayload = payload(kind, body)?;
            let repo = push.repository.full_name;
            if push.deleted {
                return Ok(Delivery { id, repo, event: None });
            }
            let commit = sha(kind, "after", push.after)?;
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
            let action: ActionPayload = payload(kind, body)?;
            if !matches!(&*action.action, "opened" | "synchronize" | "reopened" | "ready_for_review")
            {
                let repo = action.repository.map(|it| it.full_name).unwrap_or_default();
                return Ok(Delivery { id, repo, event: None });
            }
            let pr: PullRequestPayload = payload(kind, body)?;
            let base_repo = pr.pull_request.base.repo.ok_or_else(|| {
                WebhookError::new(400, "the pull_request payload has no pull_request.base.repo")
            })?;
            let from_fork =
                pr.pull_request.head.repo.is_none_or(|head| head.full_name != base_repo.full_name);
            let event = Event::PullRequest {
                number: pr.number,
                head: sha(kind, "pull_request.head.sha", pr.pull_request.head.sha)?,
                base: sha(kind, "pull_request.base.sha", pr.pull_request.base.sha)?,
                from_fork,
            };
            Ok(Delivery { id, repo: pr.repository.full_name, event: Some(event) })
        }
        _ => {
            let other: OtherPayload = payload(kind, body)?;
            let repo = other.repository.map(|it| it.full_name).unwrap_or_default();
            Ok(Delivery { id, repo, event: None })
        }
    }
}

/// [`verify`] then [`parse`], from the request's headers. Only `POST`.
pub fn receive(secret: &[u8], request: &HttpRequest) -> Result<Delivery, WebhookError> {
    if request.method != "POST" {
        return Err(WebhookError::new(
            405,
            format!("a webhook delivery is a POST, this was a {}", request.method),
        ));
    }
    verify(secret, &request.body, request.header("X-Hub-Signature-256"))?;
    if let Some(kind) = request.header("Content-Type") {
        let essence = kind.split(';').next().unwrap_or_default().trim();
        if !essence.eq_ignore_ascii_case("application/json") {
            return Err(WebhookError::new(
                415,
                format!("the webhook sends {kind}; set its content type to application/json"),
            ));
        }
    }
    let kind = request
        .header("X-GitHub-Event")
        .ok_or_else(|| WebhookError::new(400, "the delivery has no X-GitHub-Event"))?;
    let id = request
        .header("X-GitHub-Delivery")
        .ok_or_else(|| WebhookError::new(400, "the delivery has no X-GitHub-Delivery"))?;
    parse(kind, id, &request.body)
}

fn payload<'a, T: Deserialize<'a>>(kind: &str, body: &'a [u8]) -> Result<T, WebhookError> {
    serde_json::from_slice(body)
        .map_err(|error| WebhookError::new(400, format!("the {kind} payload is not one: {error}")))
}

/// A commit's full hex name, SHA-1 or SHA-256.
fn sha(kind: &str, field: &str, sha: String) -> Result<String, WebhookError> {
    let hex = sha.bytes().all(|it| matches!(it, b'0'..=b'9' | b'a'..=b'f'));
    if hex && matches!(sha.len(), 40 | 64) {
        Ok(sha)
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
struct Repository {
    full_name: String,
}

#[derive(Deserialize)]
struct PushPayload {
    #[serde(rename = "ref")]
    reference: String,
    after: String,
    #[serde(default)]
    deleted: bool,
    repository: Repository,
}

#[derive(Deserialize)]
struct ActionPayload {
    action: String,
    repository: Option<Repository>,
}

#[derive(Deserialize)]
struct PullRequestPayload {
    number: u64,
    pull_request: PullRequest,
    repository: Repository,
}

#[derive(Deserialize)]
struct PullRequest {
    head: Side,
    base: Side,
}

#[derive(Deserialize)]
struct Side {
    sha: String,
    /// Null when the fork the head is in was deleted.
    repo: Option<Repository>,
}

#[derive(Deserialize)]
struct OtherPayload {
    repository: Option<Repository>,
}

#[cfg(test)]
mod tests;
