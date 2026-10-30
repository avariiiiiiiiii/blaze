//! Frames in and out of a byte stream, in [`blaze_protocol`]'s layout (a four-byte big-endian
//! length, then one item in its deterministic CBOR), cut as the bytes arrive. An item is an
//! [`Envelope`] of [`SCHEMA`]: its schema and id are read before its body, so another schema
//! is refused by name and a body that does not decode is refused under its request's id.

use blaze_protocol::cbor;
use serde::{
    Serialize,
    de::{DeserializeOwned, IgnoredAny},
};

use crate::{ApiError, Envelope, LONGEST_REQUEST, SCHEMA};

/// Bytes of a stream, cut into frames' items as they arrive.
#[derive(Default, Debug)]
pub struct Frames {
    bytes: Vec<u8>,
    /// Where the next frame starts in `bytes`.
    at: usize,
    broken: Option<ApiError>,
}

impl Frames {
    pub fn new() -> Frames {
        Frames::default()
    }

    pub fn push(&mut self, bytes: &[u8]) {
        if self.broken.is_some() {
            return;
        }
        if self.at > 0 && self.at >= self.bytes.len() / 2 {
            self.bytes.drain(..self.at);
            self.at = 0;
        }
        self.bytes.extend_from_slice(bytes);
    }

    /// The next whole item; `Ok(None)` until one is here. An error for a length over
    /// [`LONGEST_REQUEST`], after which the stream is unusable (the peer is hung up on).
    pub fn next(&mut self) -> Result<Option<Vec<u8>>, ApiError> {
        if let Some(error) = &self.broken {
            return Err(error.clone());
        }
        let Some(&[a, b, c, d]) = self.bytes.get(self.at..self.at + 4) else { return Ok(None) };
        let len = u32::from_be_bytes([a, b, c, d]);
        if len > LONGEST_REQUEST {
            let error =
                ApiError::new(format!("a frame of {len} bytes, longer than {LONGEST_REQUEST}"), None);
            self.broken = Some(error.clone());
            self.bytes = Vec::new();
            self.at = 0;
            return Err(error);
        }
        let start = self.at + 4;
        let end = start + len as usize;
        let Some(item) = self.bytes.get(start..end) else { return Ok(None) };
        let item = item.to_vec();
        self.at = end;
        if self.at == self.bytes.len() {
            self.bytes.clear();
            self.at = 0;
        }
        Ok(Some(item))
    }

    /// How many bytes are held that are not yet a whole item.
    pub(crate) fn held(&self) -> usize {
        self.bytes.len() - self.at
    }
}

/// Why an item was refused, with the envelope's id when it could be read.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Refused {
    pub id: Option<u64>,
    pub error: ApiError,
}

/// An item as an envelope of [`SCHEMA`]: another schema refused by name ("schema 2, this
/// daemon speaks 4") before the body is read, non-deterministic CBOR refused naming the
/// offence.
pub fn decode<T: DeserializeOwned>(item: &[u8]) -> Result<Envelope<T>, Refused> {
    let (schema, id) = head(item)?;
    if schema != SCHEMA {
        let error = ApiError::new(format!("schema {schema}, this daemon speaks {SCHEMA}"), None);
        return Err(Refused { id: Some(id), error });
    }
    cbor::from_slice(item).map_err(|error| Refused { id: Some(id), error: undecodable(&error) })
}

/// The schema and id of the envelope `item` is, its body unread.
pub(crate) fn head(item: &[u8]) -> Result<(u32, u64), Refused> {
    let envelope: Envelope<IgnoredAny> =
        cbor::from_slice(item).map_err(|error| Refused { id: None, error: undecodable(&error) })?;
    Ok((envelope.schema, envelope.id))
}

fn undecodable(error: &cbor::Error) -> ApiError {
    ApiError::new(format!("an envelope that does not decode: {error}"), None)
}

/// `envelope` in deterministic CBOR, framed (4-byte big-endian length, then the item).
pub fn encode<T: Serialize>(envelope: &Envelope<T>) -> Result<Vec<u8>, ApiError> {
    let item = cbor::to_vec(envelope)
        .map_err(|error| ApiError::new(format!("an envelope with no encoding: {error}"), None))?;
    let mut frame = Vec::new();
    blaze_protocol::write_frame(&mut frame, &item)
        .map_err(|error| ApiError::new(error.to_string(), None))?;
    Ok(frame)
}

#[cfg(test)]
mod tests;
