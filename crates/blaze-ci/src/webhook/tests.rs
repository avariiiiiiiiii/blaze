use blaze_api::http::HttpRequest;
use ring::hmac;

use super::*;

const SECRET: &[u8] = b"a test secret";
const HEAD: &str = "0123456789abcdef0123456789abcdef01234567";
const BASE: &str = "89abcdef0123456789abcdef0123456789abcdef";

fn sign(secret: &[u8], body: &[u8]) -> String {
    let tag = hmac::sign(&hmac::Key::new(hmac::HMAC_SHA256, secret), body);
    let hex: String = tag.as_ref().iter().map(|byte| format!("{byte:02x}")).collect();
    format!("sha256={hex}")
}

fn request(kind: &str, body: &str) -> HttpRequest {
    HttpRequest {
        method: "POST".to_owned(),
        path: "/webhook".to_owned(),
        headers: vec![
            ("x-github-event".to_owned(), kind.to_owned()),
            ("X-GitHub-Delivery".to_owned(), "72d3162e-cc78-11e3-81ab-4c9367dc0958".to_owned()),
            ("Content-Type".to_owned(), "application/json".to_owned()),
            ("X-Hub-Signature-256".to_owned(), sign(SECRET, body.as_bytes())),
        ],
        body: body.as_bytes().to_vec(),
    }
}

fn push(reference: &str, deleted: bool) -> String {
    format!(
        r#"{{"ref":"{reference}","before":"{BASE}","after":"{}","deleted":{deleted},
            "repository":{{"full_name":"indexable-inc/blaze","private":true}},
            "pusher":{{"name":"someone"}},"commits":[]}}"#,
        if deleted { "0000000000000000000000000000000000000000" } else { HEAD },
    )
}

fn pull_request(action: &str, head_repo: &str) -> String {
    format!(
        r#"{{"action":"{action}","number":42,
            "pull_request":{{"title":"a change","draft":false,
                "head":{{"ref":"lane","sha":"{HEAD}","repo":{head_repo}}},
                "base":{{"ref":"main","sha":"{BASE}","repo":{{"full_name":"indexable-inc/blaze"}}}}}},
            "repository":{{"full_name":"indexable-inc/blaze"}}}}"#
    )
}

const SAME_REPO: &str = r#"{"full_name":"indexable-inc/blaze"}"#;

#[test]
fn githubs_documented_example_verifies() {
    let signature = "sha256=757107ea0eb2509fc211221cce984b8a37570b6d7586c22c46f4379c8b043e17";
    assert_eq!(verify(b"It's a Secret to Everybody", b"Hello, World!", Some(signature)), Ok(()));
}

#[test]
fn a_wrong_signature_is_401() {
    let good = "sha256=757107ea0eb2509fc211221cce984b8a37570b6d7586c22c46f4379c8b043e17";
    let error = verify(b"another secret", b"Hello, World!", Some(good)).unwrap_err();
    assert_eq!(error.status, 401);
    let error = verify(b"It's a Secret to Everybody", b"Hello, World?", Some(good)).unwrap_err();
    assert_eq!(error.status, 401);
    let bad = "sha256=757107ea0eb2509fc211221cce984b8a37570b6d7586c22c46f4379c8b043e1";
    assert_eq!(verify(b"It's a Secret to Everybody", b"Hello, World!", Some(bad)).unwrap_err().status, 401);
    let sha1 = "sha1=01dc10d0c83e72ed246219cdd91669667fe2ca59";
    assert_eq!(verify(b"It's a Secret to Everybody", b"Hello, World!", Some(sha1)).unwrap_err().status, 401);
}

#[test]
fn a_missing_signature_is_400_and_an_empty_secret_checks_nothing() {
    let error = verify(SECRET, b"{}", None).unwrap_err();
    assert_eq!(error.status, 400);
    assert!(error.what.contains("X-Hub-Signature-256"), "{}", error.what);
    let body = b"{}";
    assert_eq!(verify(b"", body, Some(&sign(b"", body))).unwrap_err().status, 500);
}

#[test]
fn a_push_to_a_branch() {
    let delivery = receive(SECRET, &request("push", &push("refs/heads/main", false))).unwrap();
    assert_eq!(delivery.id, "72d3162e-cc78-11e3-81ab-4c9367dc0958");
    assert_eq!(delivery.repo, "indexable-inc/blaze");
    assert_eq!(delivery.event, Some(Event::Push { branch: "main".to_owned(), commit: HEAD.to_owned() }));
}

#[test]
fn a_branch_with_slashes_keeps_them() {
    let delivery = parse("push", "1", push("refs/heads/lanes/ci", false).as_bytes()).unwrap();
    assert_eq!(delivery.event, Some(Event::Push { branch: "lanes/ci".to_owned(), commit: HEAD.to_owned() }));
}

#[test]
fn a_pushed_tag() {
    let delivery = parse("push", "1", push("refs/tags/v2.0.0", false).as_bytes()).unwrap();
    assert_eq!(delivery.event, Some(Event::Tag { name: "v2.0.0".to_owned(), commit: HEAD.to_owned() }));
}

#[test]
fn a_deleted_branch_or_tag_is_ignored() {
    for reference in ["refs/heads/main", "refs/tags/v1"] {
        let delivery = receive(SECRET, &request("push", &push(reference, true))).unwrap();
        assert_eq!(delivery.repo, "indexable-inc/blaze");
        assert_eq!(delivery.event, None);
    }
}

#[test]
fn a_push_to_another_kind_of_ref_is_ignored() {
    let delivery = parse("push", "1", push("refs/notes/commits", false).as_bytes()).unwrap();
    assert_eq!(delivery.event, None);
}

#[test]
fn a_pull_request_from_the_same_repository() {
    for action in ["opened", "synchronize", "reopened", "ready_for_review"] {
        let delivery = receive(SECRET, &request("pull_request", &pull_request(action, SAME_REPO))).unwrap();
        assert_eq!(delivery.repo, "indexable-inc/blaze");
        assert_eq!(
            delivery.event,
            Some(Event::PullRequest {
                number: 42,
                head: HEAD.to_owned(),
                base: BASE.to_owned(),
                from_fork: false,
            }),
            "{action}",
        );
    }
}

#[test]
fn a_pull_request_from_a_fork() {
    let body = pull_request("opened", r#"{"full_name":"someone/blaze"}"#);
    let delivery = parse("pull_request", "1", body.as_bytes()).unwrap();
    assert!(matches!(delivery.event, Some(Event::PullRequest { from_fork: true, .. })));
}

#[test]
fn a_pull_request_whose_fork_was_deleted_is_from_a_fork() {
    let delivery = parse("pull_request", "1", pull_request("synchronize", "null").as_bytes()).unwrap();
    assert!(matches!(delivery.event, Some(Event::PullRequest { from_fork: true, .. })));
}

#[test]
fn other_pull_request_actions_are_ignored() {
    for action in ["closed", "labeled", "edited", "converted_to_draft"] {
        let delivery = parse("pull_request", "1", pull_request(action, SAME_REPO).as_bytes()).unwrap();
        assert_eq!(delivery.event, None, "{action}");
        assert_eq!(delivery.repo, "indexable-inc/blaze");
    }
}

#[test]
fn a_ping_is_ignored() {
    let body = r#"{"zen":"Keep it logically awesome.","hook_id":1,"hook":{"type":"App"}}"#;
    let delivery = receive(SECRET, &request("ping", body)).unwrap();
    assert_eq!(delivery, Delivery { id: "72d3162e-cc78-11e3-81ab-4c9367dc0958".to_owned(), repo: String::new(), event: None });
}

#[test]
fn a_body_that_is_not_the_event_is_400() {
    let error = parse("push", "1", br#"{"ref":"refs/heads/main"}"#).unwrap_err();
    assert_eq!(error.status, 400);
    assert!(error.what.contains("push"), "{}", error.what);
    let error = parse("pull_request", "1", b"not json").unwrap_err();
    assert_eq!(error.status, 400);
    let bad_sha = push("refs/heads/main", false).replace(HEAD, "HEAD");
    let error = parse("push", "1", bad_sha.as_bytes()).unwrap_err();
    assert_eq!(error.status, 400);
    assert!(error.what.contains("after"), "{}", error.what);
}

#[test]
fn missing_headers_are_400_and_only_post_is_taken() {
    let body = push("refs/heads/main", false);
    let mut get = request("push", &body);
    get.method = "GET".to_owned();
    assert_eq!(receive(SECRET, &get).unwrap_err().status, 405);

    let mut no_event = request("push", &body);
    no_event.headers.retain(|(name, _)| !name.eq_ignore_ascii_case("X-GitHub-Event"));
    let error = receive(SECRET, &no_event).unwrap_err();
    assert_eq!((error.status, error.what.contains("X-GitHub-Event")), (400, true), "{}", error.what);

    let mut no_id = request("push", &body);
    no_id.headers.retain(|(name, _)| name != "X-GitHub-Delivery");
    assert_eq!(receive(SECRET, &no_id).unwrap_err().status, 400);

    let mut unsigned = request("push", &body);
    unsigned.headers.retain(|(name, _)| name != "X-Hub-Signature-256");
    assert_eq!(receive(SECRET, &unsigned).unwrap_err().status, 400);
}

#[test]
fn the_signature_is_checked_before_the_payload_is_read() {
    let mut request = request("push", "not json");
    request.headers.retain(|(name, _)| name != "X-Hub-Signature-256");
    request.headers.push(("X-Hub-Signature-256".to_owned(), sign(b"not the secret", b"not json")));
    assert_eq!(receive(SECRET, &request).unwrap_err().status, 401);
}

#[test]
fn a_form_encoded_delivery_is_refused_by_name() {
    let mut request = request("push", &push("refs/heads/main", false));
    request.headers.retain(|(name, _)| name != "Content-Type");
    request.headers.push(("Content-Type".to_owned(), "application/x-www-form-urlencoded".to_owned()));
    let error = receive(SECRET, &request).unwrap_err();
    assert_eq!(error.status, 415);
}

#[test]
fn a_payload_over_githubs_cap_is_refused() {
    let body = vec![b' '; MOST_PAYLOAD + 1];
    assert_eq!(parse("push", "1", &body).unwrap_err().status, 413);
}
