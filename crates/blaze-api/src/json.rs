//! The rendering for people (`Accept: application/json` over HTTP, the CLI's `--json`): one
//! line of JSON per envelope. Never the wire, which is CBOR ([`crate::frames`]).

use serde::Serialize;

use crate::{Envelope, Request, Response};

/// `envelope` as one line of JSON, without its newline.
pub fn response(envelope: &Envelope<Response>) -> String {
    line(envelope)
}

/// `envelope` as one line of JSON, without its newline.
pub fn request(envelope: &Envelope<Request>) -> String {
    line(envelope)
}

fn line<T: Serialize>(envelope: &Envelope<T>) -> String {
    // The API's types key no map by anything but text, so every value has a JSON rendering.
    serde_json::to_string(envelope).expect("an envelope of the API has a JSON rendering")
}

#[cfg(test)]
mod tests;
