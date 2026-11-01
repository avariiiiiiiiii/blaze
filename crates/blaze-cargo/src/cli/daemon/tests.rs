use std::path::Path;

use blaze_api::{Response, Status};

use super::*;

/// Hands `on` each of `responses` as [`Client::call_ref`] does: encoded as the daemon sends it,
/// read where it lies, with whether another follows it; the last Done's `ok`. The fakes of
/// [`Api`] answer through it.
pub(crate) fn hand(responses: &[Response], mut on: impl FnMut(ResponseRef<'_>, bool)) -> bool {
    let mut ok = false;
    for (at, response) in responses.iter().enumerate() {
        if let Response::Done { ok: it } = response {
            ok = *it;
        }
        let bytes = blaze_protocol::cbor::to_vec(response).unwrap();
        on(blaze_protocol::cbor::from_slice::<ResponseRef<'_>>(&bytes).unwrap(), at + 1 < responses.len());
    }
    ok
}

#[test]
fn a_response_handed_in_place_is_the_one_sent() {
    let sent = [Response::Fetched { verb: blaze_api::FetchVerb::Updating, what: "crates.io index".to_owned() }, Response::Done {
        ok: true,
    }];
    let mut got = Vec::new();
    assert!(hand(&sent, |response, more| got.push((response.into_owned(), more))));
    assert_eq!(got, [(sent[0].clone(), true), (sent[1].clone(), false)]);
}

#[test]
fn a_start_that_never_listens_names_the_socket() {
    let said = not_listening(Path::new("/run/blaze/v2.sock"), Path::new("/c/blaze/v2/serve.log"), "connection refused");
    assert_eq!(
        said,
        "the daemon (`blaze serve`) was started and nothing listens at /run/blaze/v2.sock after 10s \
         (connection refused); its log is /c/blaze/v2/serve.log"
    );
}

#[test]
fn the_status_for_people() {
    let status = Status { pid: 41, requests: vec![3, 7], views: 2, history: None, memory_bytes: 3 << 29, budget_bytes: 8 << 30 };
    assert_eq!(status_lines(&status), "pid: 41\nrequests: 2 (3, 7)\nviews: 2\nmemory: 1.5 GiB of 8.0 GiB\n");
    let idle = Status { pid: 7, requests: Vec::new(), views: 0, history: None, memory_bytes: 512, budget_bytes: 1536 };
    assert_eq!(status_lines(&idle), "pid: 7\nrequests: 0\nviews: 0\nmemory: 512 B of 1.5 KiB\n");
    let failed = Status { history: Some("the profile history: writing /h/0-00000003.spans.parquet: disk full".to_owned()), ..idle };
    assert!(status_lines(&failed).ends_with("history: the profile history: writing /h/0-00000003.spans.parquet: disk full\n"));
}
