use serde_json::{Value, json};

use super::*;
use crate::{ApiError, SCHEMA};

#[test]
fn a_response_is_one_line_of_its_envelope() {
    let envelope = Envelope::new(
        7,
        Response::Error(ApiError::new("two\nlines", Some("src/lib.rs".to_owned()))),
    );
    let line = response(&envelope);
    assert!(!line.contains('\n'), "{line}");
    let value: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(
        value,
        json!({
            "schema": SCHEMA,
            "id": 7,
            "body": { "Error": { "what": "two\nlines", "at": "src/lib.rs" } },
        })
    );
}

#[test]
fn a_request_is_one_line_of_its_envelope() {
    let line = request(&Envelope::new(3, Request::Cancel { id: 2 }));
    let value: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(value, json!({ "schema": SCHEMA, "id": 3, "body": { "Cancel": { "id": 2 } } }));
    assert_eq!(request(&Envelope::new(1, Request::Status)), format!(
        r#"{{"schema":{SCHEMA},"id":1,"body":"Status"}}"#
    ));
}

#[test]
fn done_renders_its_outcome() {
    let line = response(&Envelope::new(9, Response::Done { ok: false }));
    let value: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(value["body"], json!({ "Done": { "ok": false } }));
}
