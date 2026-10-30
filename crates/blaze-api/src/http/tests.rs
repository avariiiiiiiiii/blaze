use blaze_core::sim::Rng;

use super::*;

fn parse(bytes: &[u8]) -> Result<Option<HttpRequest>, HttpError> {
    let mut parser = RequestParser::new(1 << 20);
    parser.push(bytes);
    parser.next()
}

fn status_of(bytes: &[u8]) -> u16 {
    parse(bytes).unwrap_err().status
}

#[test]
fn a_request_with_a_length_is_read_whole() {
    let bytes = request("POST", "daemon", "/api", &[("Content-Type", "application/cbor")], b"body");
    let request = parse(&bytes).unwrap().unwrap();
    assert_eq!((request.method.as_str(), request.path.as_str()), ("POST", "/api"));
    assert_eq!(request.header("content-type"), Some("application/cbor"));
    assert_eq!(request.header("HOST"), Some("daemon"));
    assert_eq!(request.header("Content-Length"), Some("4"));
    assert_eq!(request.body, b"body");
}

#[test]
fn a_request_without_a_body_has_none() {
    let request = parse(b"GET /status HTTP/1.1\r\nHost: x\r\n\r\n").unwrap().unwrap();
    assert_eq!(request.method, "GET");
    assert!(request.body.is_empty());
}

#[test]
fn a_chunked_body_is_decoded_with_extensions_and_trailers_dropped() {
    let bytes = b"POST /hook HTTP/1.1\r\nTransfer-Encoding: Chunked\r\n\r\n\
        4;name=value\r\nWiki\r\n5\r\npedia\r\nE \r\n in\r\n\r\nchunks.\r\n0\r\nX-Trailer: 1\r\n\r\n";
    let request = parse(bytes).unwrap().unwrap();
    assert_eq!(request.body, b"Wikipedia in\r\n\r\nchunks.");
}

#[test]
fn a_request_comes_out_the_same_whatever_the_cuts() {
    let mut stream = request("POST", "h", "/a", &[("X-One", "1")], b"first body");
    stream.extend_from_slice(
        b"POST /b HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n2\r\nde\r\n0\r\n\r\n",
    );
    stream.extend(request("GET", "h", "/c", &[], b""));
    let whole = {
        let mut parser = RequestParser::new(1 << 20);
        parser.push(&stream);
        let mut out = Vec::new();
        while let Some(request) = parser.next().unwrap() {
            out.push(request);
        }
        out
    };
    assert_eq!(whole.len(), 3);
    assert_eq!(whole[1].body, b"abcde");
    for seed in 0..300 {
        let mut rng = Rng::new(seed);
        let mut parser = RequestParser::new(1 << 20);
        let mut out = Vec::new();
        let mut at = 0;
        while at < stream.len() {
            let len = (rng.below(12) as usize).min(stream.len() - at);
            parser.push(&stream[at..at + len]);
            at += len;
            while let Some(request) = parser.next().unwrap() {
                out.push(request);
            }
        }
        assert_eq!(out, whole, "seed {seed}");
        assert_eq!(parser.held(), 0, "seed {seed}");
    }
}

#[test]
fn blank_lines_before_a_request_are_skipped() {
    let request = parse(b"\r\n\r\nGET / HTTP/1.1\r\n\r\n").unwrap().unwrap();
    assert_eq!(request.path, "/");
}

#[test]
fn a_head_line_over_8_kib_is_refused_before_it_ends() {
    let mut bytes = b"GET / HTTP/1.1\r\nX-Long: ".to_vec();
    bytes.extend(std::iter::repeat_n(b'a', 8 << 10));
    let error = parse(&bytes).unwrap_err();
    assert_eq!(error.status, 431);
    assert!(error.what.contains("8 KiB"), "{error}");
}

#[test]
fn a_head_line_of_8_kib_is_read() {
    let mut bytes = b"GET / HTTP/1.1\r\n".to_vec();
    let mut line = b"X-Long: ".to_vec();
    line.resize(8 << 10, b'a');
    bytes.extend(line);
    bytes.extend(b"\r\n\r\n");
    assert!(parse(&bytes).unwrap().is_some());
}

#[test]
fn more_than_100_headers_are_refused() {
    let mut head = String::from("GET / HTTP/1.1\r\n");
    for n in 0..100 {
        head.push_str(&format!("X-{n}: v\r\n"));
    }
    let mut ok = head.clone();
    ok.push_str("\r\n");
    assert!(parse(ok.as_bytes()).unwrap().is_some());
    head.push_str("X-100: v\r\n\r\n");
    let error = parse(head.as_bytes()).unwrap_err();
    assert_eq!((error.status, error.what.as_str()), (431, "more than 100 headers"));
}

#[test]
fn a_body_over_the_bound_is_413_before_its_bytes_arrive() {
    let mut parser = RequestParser::new(10);
    parser.push(b"POST / HTTP/1.1\r\nContent-Length: 11\r\n\r\n");
    assert_eq!(parser.next().unwrap_err().status, 413);
    let mut parser = RequestParser::new(10);
    parser.push(b"POST / HTTP/1.1\r\nContent-Length: 10\r\n\r\n0123456789");
    assert_eq!(parser.next().unwrap().unwrap().body, b"0123456789");
}

#[test]
fn a_chunked_body_over_the_bound_is_413() {
    let mut parser = RequestParser::new(10);
    parser.push(b"POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n6\r\nabcdef\r\n5\r\n");
    assert_eq!(parser.next().unwrap_err().status, 413);
    let mut parser = RequestParser::new(10);
    parser.push(b"POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\nffffffffffffffff\r\n");
    assert_eq!(parser.next().unwrap_err().status, 413);
}

#[test]
fn both_a_length_and_chunks_are_refused() {
    let bytes = b"POST / HTTP/1.1\r\nContent-Length: 3\r\nTransfer-Encoding: chunked\r\n\r\n";
    let error = parse(bytes).unwrap_err();
    assert_eq!(error.status, 400);
    assert!(error.what.contains("both"), "{error}");
}

#[test]
fn bad_lengths_and_codings_are_refused_naming_them() {
    assert_eq!(status_of(b"POST / HTTP/1.1\r\nContent-Length: +3\r\n\r\nabc"), 400);
    assert_eq!(status_of(b"POST / HTTP/1.1\r\nContent-Length: 3\r\nContent-Length: 3\r\n\r\n"), 400);
    assert_eq!(status_of(b"POST / HTTP/1.1\r\nTransfer-Encoding: gzip, chunked\r\n\r\n"), 501);
}

#[test]
fn bad_chunk_sizes_are_refused() {
    for size in ["", "x", "-1", "11111111111111111", " 5"] {
        let bytes = format!("POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n{size}\r\n");
        let error = parse(bytes.as_bytes()).unwrap_err();
        assert_eq!((error.status, error.what.as_str()), (400, "a bad chunk size"), "{size:?}");
    }
    let bytes = b"POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n2\r\nabc\r\n";
    assert_eq!(parse(bytes).unwrap_err().what, "a chunk not ended by CRLF");
}

#[test]
fn malformed_heads_are_refused() {
    assert_eq!(status_of(b"GET / HTTP/1.1\nHost: x\r\n\r\n"), 400);
    assert_eq!(status_of(b"GET / HTTP/1.1\r\n folded\r\n\r\n"), 400);
    assert_eq!(status_of(b"GET / HTTP/1.1\r\nNo colon\r\n\r\n"), 400);
    assert_eq!(status_of(b"GET / HTTP/1.1\r\nBad name: x\r\n\r\n"), 400);
    assert_eq!(status_of(b"GET /  HTTP/1.1\r\n\r\n"), 400);
    assert_eq!(status_of(b"GET / HTTP/2.0\r\n\r\n"), 505);
    assert_eq!(status_of(b"GET / FTP/1.1\r\n\r\n"), 400);
}

#[test]
fn a_parser_that_refused_keeps_refusing() {
    let mut parser = RequestParser::new(100);
    parser.push(b"GET / HTTP/2.0\r\n\r\n");
    let error = parser.next().unwrap_err();
    parser.push(b"GET / HTTP/1.1\r\n\r\n");
    assert_eq!(parser.next().unwrap_err(), error);
}

#[test]
fn a_response_round_trips_through_the_parser() {
    let bytes = response(404, &[("Content-Type", "text/plain")], b"no such path");
    assert!(bytes.starts_with(b"HTTP/1.1 404 Not Found\r\n"));
    let response = read_response(&mut &bytes[..], 1 << 20).unwrap();
    assert_eq!(response.status, 404);
    assert_eq!(response.header("content-type"), Some("text/plain"));
    assert_eq!(response.body, b"no such path");
}

#[test]
fn a_chunked_response_hands_its_body_over_as_it_comes() {
    let mut bytes = chunked(200, &[("Content-Type", "application/cbor")]);
    bytes.extend(chunk(b"one"));
    bytes.extend(chunk(b""));
    bytes.extend(chunk(b"two"));
    bytes.extend(last_chunk());
    for seed in 0..200 {
        let mut rng = Rng::new(seed);
        let mut parser = ResponseParser::new(1 << 20);
        let mut body = Vec::new();
        let mut at = 0;
        while at < bytes.len() {
            // No empty pushes: one is the end of the stream.
            let len = (1 + rng.below(7) as usize).min(bytes.len() - at);
            parser.push(&bytes[at..at + len]).unwrap();
            at += len;
            body.extend(parser.take_body());
        }
        assert!(parser.finished(), "seed {seed}");
        assert_eq!(parser.head().unwrap().0, 200);
        assert_eq!(body, b"onetwo", "seed {seed}");
    }
}

#[test]
fn an_empty_chunk_is_nothing_and_never_the_last() {
    assert!(chunk(b"").is_empty());
    assert_eq!(chunk(b"abc"), b"3\r\nabc\r\n");
    assert_eq!(chunk(&[0; 26])[..4], *b"1a\r\n");
    assert_eq!(last_chunk(), b"0\r\n\r\n");
}

#[test]
fn bytes_after_a_response_are_the_next_ones() {
    let mut bytes = response(200, &[], b"a");
    bytes.extend(response(201, &[], b"b"));
    let mut parser = ResponseParser::new(100);
    parser.push(&bytes).unwrap();
    assert!(parser.finished());
    assert_eq!(parser.take_body(), b"a");
    let rest = parser.rest();
    let mut next = ResponseParser::new(100);
    next.push(&rest).unwrap();
    assert_eq!((next.head().unwrap().0, next.take_body()), (201, b"b".to_vec()));
}

#[test]
fn an_interim_response_is_skipped() {
    let mut bytes = b"HTTP/1.1 100 Continue\r\n\r\n".to_vec();
    bytes.extend(response(200, &[], b"done"));
    let response = read_response(&mut &bytes[..], 100).unwrap();
    assert_eq!((response.status, response.body), (200, b"done".to_vec()));
}

#[test]
fn a_response_without_a_length_ends_with_the_stream() {
    let bytes = b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\nall of it";
    let response = read_response(&mut &bytes[..], 100).unwrap();
    assert_eq!(response.body, b"all of it");
    let mut parser = ResponseParser::new(100);
    parser.push(bytes).unwrap();
    assert!(!parser.finished());
    parser.push(&[]).unwrap();
    assert!(parser.finished());
}

#[test]
fn a_response_cut_short_or_too_long_is_an_error() {
    let bytes = response(200, &[], b"twelve bytes");
    assert!(read_response(&mut &bytes[..bytes.len() - 1], 100).is_err());
    assert!(read_response(&mut &bytes[..10], 100).is_err());
    let error = read_response(&mut &bytes[..], 11).unwrap_err();
    assert!(error.to_string().contains("413"), "{error}");
}

#[test]
fn no_content_has_no_body() {
    let response = read_response(&mut &b"HTTP/1.1 204 No Content\r\n\r\n"[..], 100).unwrap();
    assert_eq!(response.status, 204);
    assert!(response.body.is_empty());
}

#[test]
fn a_status_line_without_a_reason_is_read() {
    let response = read_response(&mut &b"HTTP/1.1 299\r\nContent-Length: 0\r\n\r\n"[..], 1).unwrap();
    assert_eq!(response.status, 299);
}
