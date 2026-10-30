use std::path::Path;

use blaze_api::Status;

use super::*;

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
    let status = Status { pid: 41, requests: vec![3, 7], views: 2, memory_bytes: 3 << 29, budget_bytes: 8 << 30 };
    assert_eq!(status_lines(&status), "pid: 41\nrequests: 2 (3, 7)\nviews: 2\nmemory: 1.5 GiB of 8.0 GiB\n");
    let idle = Status { pid: 7, requests: Vec::new(), views: 0, memory_bytes: 512, budget_bytes: 1536 };
    assert_eq!(status_lines(&idle), "pid: 7\nrequests: 0\nviews: 0\nmemory: 512 B of 1.5 KiB\n");
}
