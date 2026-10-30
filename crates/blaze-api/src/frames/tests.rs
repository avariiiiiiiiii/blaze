use blaze_core::sim::Rng;

use super::*;
use crate::{Request, Response};

fn item(id: u64, request: Request) -> Vec<u8> {
    blaze_protocol::cbor::to_vec(&Envelope::new(id, request)).unwrap()
}

#[test]
fn an_encoded_envelope_is_a_frame_of_its_item() {
    let envelope = Envelope::new(9, Request::Status);
    let frame = encode(&envelope).unwrap();
    let item = item(9, Request::Status);
    assert_eq!(frame[..4], (item.len() as u32).to_be_bytes());
    assert_eq!(frame[4..], item);
    let back: Envelope<Request> = decode(&frame[4..]).unwrap();
    assert_eq!(back, envelope);
}

#[test]
fn items_come_out_whole_whatever_the_cuts() {
    let mut stream = Vec::new();
    let mut items = Vec::new();
    for id in 0..5 {
        let envelope = Envelope::new(id, Request::Cancel { id: id * 1000 });
        stream.extend(encode(&envelope).unwrap());
        items.push(item(id, Request::Cancel { id: id * 1000 }));
    }
    for seed in 0..300 {
        let mut rng = Rng::new(seed);
        let mut frames = Frames::new();
        let mut out = Vec::new();
        let mut at = 0;
        while at < stream.len() {
            let len = (rng.below(40) as usize).min(stream.len() - at);
            frames.push(&stream[at..at + len]);
            at += len;
            while let Some(item) = frames.next().unwrap() {
                out.push(item);
            }
        }
        assert_eq!(out, items, "seed {seed}");
        assert_eq!(frames.held(), 0, "seed {seed}");
    }
}

#[test]
fn nothing_comes_out_of_a_frame_cut_short() {
    let frame = encode(&Envelope::new(1, Request::Status)).unwrap();
    let mut frames = Frames::new();
    frames.push(&frame[..frame.len() - 1]);
    assert_eq!(frames.next().unwrap(), None);
    frames.push(&frame[frame.len() - 1..]);
    assert!(frames.next().unwrap().is_some());
    assert_eq!(frames.next().unwrap(), None);
}

#[test]
fn a_length_over_the_longest_breaks_the_stream_before_its_bytes_arrive() {
    let mut frames = Frames::new();
    frames.push(&(LONGEST_REQUEST + 1).to_be_bytes());
    let error = frames.next().unwrap_err();
    assert!(error.what.contains(&format!("{}", LONGEST_REQUEST + 1)), "{error:?}");
    frames.push(&encode(&Envelope::new(1, Request::Status)).unwrap());
    assert_eq!(frames.next().unwrap_err(), error);
}

#[test]
fn a_frame_of_the_longest_length_is_waited_for() {
    let mut frames = Frames::new();
    frames.push(&LONGEST_REQUEST.to_be_bytes());
    assert_eq!(frames.next().unwrap(), None);
}

#[test]
fn another_schema_is_refused_by_name_under_its_id() {
    let item =
        blaze_protocol::cbor::to_vec(&Envelope { schema: 2, id: 41, body: Request::Status }).unwrap();
    let refused = decode::<Request>(&item).unwrap_err();
    assert_eq!(refused.id, Some(41));
    assert_eq!(refused.error.what, format!("schema 2, this daemon speaks {SCHEMA}"));
}

#[test]
fn another_schema_is_refused_before_its_body_is_read() {
    #[derive(serde::Serialize)]
    enum Future {
        Teleport { to: String },
    }
    let item = blaze_protocol::cbor::to_vec(&Envelope {
        schema: SCHEMA + 1,
        id: 3,
        body: Future::Teleport { to: "mars".to_owned() },
    })
    .unwrap();
    let refused = decode::<Request>(&item).unwrap_err();
    assert!(refused.error.what.starts_with(&format!("schema {}", SCHEMA + 1)), "{refused:?}");
}

#[test]
fn non_deterministic_cbor_is_refused_naming_the_offence() {
    let mut item = item(5, Request::Status);
    // The id 5 as `18 05`, a longer head than it needs.
    let at = item.windows(3).position(|it| it == [0x62, b'i', b'd']).unwrap() + 3;
    assert_eq!(item[at], 0x05);
    item.splice(at..=at, [0x18, 0x05]);
    let refused = decode::<Request>(&item).unwrap_err();
    assert_eq!(refused.id, None);
    assert!(refused.error.what.contains("not deterministic CBOR"), "{refused:?}");
}

#[test]
fn a_body_that_is_not_a_request_is_refused_under_its_id() {
    let item = blaze_protocol::cbor::to_vec(&Envelope::new(12, Response::Done { ok: true })).unwrap();
    let refused = decode::<Request>(&item).unwrap_err();
    assert_eq!(refused.id, Some(12));
    assert!(refused.error.what.starts_with("an envelope that does not decode"), "{refused:?}");
}

#[test]
fn bytes_that_are_no_envelope_are_refused_without_an_id() {
    let refused = decode::<Request>(&[0xff]).unwrap_err();
    assert_eq!(refused.id, None);
    let refused = decode::<Request>(&blaze_protocol::cbor::to_vec(&7u32).unwrap()).unwrap_err();
    assert_eq!(refused.id, None);
}
