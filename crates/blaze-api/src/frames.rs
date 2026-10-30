//! Frames in and out of a byte stream, in [`blaze_protocol`]'s layout (a four-byte big-endian
//! length, then one item in its deterministic CBOR), cut as the bytes arrive. An item is an
//! [`Envelope`] of [`SCHEMA`]: another schema is refused by name, and a body that does not
//! decode is refused under its request's id.

use std::io::{self, Read};

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
        if self.broken.is_none() {
            self.compact();
            self.bytes.extend_from_slice(bytes);
        }
    }

    /// The next whole item; `Ok(None)` until one is here. An error for a length over
    /// [`LONGEST_REQUEST`], after which the stream is unusable (the peer is hung up on).
    pub fn next(&mut self) -> Result<Option<Vec<u8>>, ApiError> {
        Ok(self.next_ref()?.map(<[u8]>::to_vec))
    }

    /// [`Frames::next`] in place: the item stays in the buffer until the next push.
    pub(crate) fn next_ref(&mut self) -> Result<Option<&[u8]>, ApiError> {
        let Some(len) = self.declared()? else { return Ok(None) };
        let start = self.at + 4;
        let end = start + len;
        if self.bytes.len() < end {
            return Ok(None);
        }
        self.at = end;
        Ok(Some(&self.bytes[start..end]))
    }

    /// How many bytes are held that are not yet a whole item.
    pub(crate) fn held(&self) -> usize {
        self.bytes.len() - self.at
    }

    /// How many bytes to read next: what the frame under way still lacks, but no more than
    /// have arrived of it, so a buffer grows on the peer's word only as fast as its bytes come;
    /// and never fewer than `least`.
    pub(crate) fn wanted(&self, least: usize) -> usize {
        let held = self.held();
        let Some(&[a, b, c, d]) = self.bytes.get(self.at..self.at + 4) else { return least };
        let len = u32::from_be_bytes([a, b, c, d]);
        if len > LONGEST_REQUEST {
            return least;
        }
        let lacking = (4 + len as usize).saturating_sub(held);
        lacking.min(held).max(least)
    }

    /// One read of at most `most` bytes from `from`, straight into the buffer; 0 once the peer
    /// has hung up.
    pub(crate) fn fill(&mut self, from: &mut impl Read, most: usize) -> io::Result<usize> {
        self.compact();
        let len = self.bytes.len();
        self.bytes.resize(len + most, 0);
        let read = loop {
            match from.read(&mut self.bytes[len..]) {
                Ok(read) => break Ok(read),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => break Err(error),
            }
        };
        let got = match &read {
            Ok(got) => *got,
            Err(_) => 0,
        };
        self.bytes.truncate(len + got);
        read
    }

    /// The length the next frame declares, once its head is here.
    fn declared(&mut self) -> Result<Option<usize>, ApiError> {
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
        Ok(Some(len as usize))
    }

    /// Drops the items handed out, once they are at least half the buffer.
    fn compact(&mut self) {
        if self.at > 0 && self.at >= self.bytes.len() / 2 {
            self.bytes.drain(..self.at);
            self.at = 0;
        }
    }
}

/// Why an item was refused, with the envelope's id when it could be read.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Refused {
    pub id: Option<u64>,
    pub error: ApiError,
}

/// An item as an envelope of [`SCHEMA`]: another schema refused by name ("schema 2, this
/// daemon speaks 4"), whether or not its body is one of this schema's; non-deterministic CBOR
/// refused naming the offence. An item that decodes is read in one pass; only a refused one
/// has its head read apart.
pub fn decode<T: DeserializeOwned>(item: &[u8]) -> Result<Envelope<T>, Refused> {
    let error = match cbor::from_slice::<Envelope<T>>(item) {
        Ok(envelope) if envelope.schema == SCHEMA => return Ok(envelope),
        Ok(envelope) => return Err(wrong_schema(envelope.schema, envelope.id)),
        Err(error) => error,
    };
    match head(item)? {
        (schema, id) if schema != SCHEMA => Err(wrong_schema(schema, id)),
        (_, id) => Err(Refused { id: Some(id), error: undecodable(&error) }),
    }
}

/// The schema and id of the envelope `item` is, its body unread.
pub(crate) fn head(item: &[u8]) -> Result<(u32, u64), Refused> {
    let envelope: Envelope<IgnoredAny> =
        cbor::from_slice(item).map_err(|error| Refused { id: None, error: undecodable(&error) })?;
    Ok((envelope.schema, envelope.id))
}

fn wrong_schema(schema: u32, id: u64) -> Refused {
    let error = ApiError::new(format!("schema {schema}, this daemon speaks {SCHEMA}"), None);
    Refused { id: Some(id), error }
}

fn undecodable(error: &cbor::Error) -> ApiError {
    ApiError::new(format!("an envelope that does not decode: {error}"), None)
}

/// `envelope` in deterministic CBOR, framed (4-byte big-endian length, then the item).
pub fn encode<T: Serialize>(envelope: &Envelope<T>) -> Result<Vec<u8>, ApiError> {
    let item = cbor::to_vec(envelope)
        .map_err(|error| ApiError::new(format!("an envelope with no encoding: {error}"), None))?;
    let len = u32::try_from(item.len()).ok().filter(|&len| len <= blaze_protocol::LONGEST_FRAME);
    let Some(len) = len else {
        let said =
            format!("a frame of {} bytes, longer than {}", item.len(), blaze_protocol::LONGEST_FRAME);
        return Err(ApiError::new(said, None));
    };
    let mut frame = Vec::with_capacity(4 + item.len());
    frame.extend(len.to_be_bytes());
    frame.extend(item);
    Ok(frame)
}

#[cfg(test)]
mod tests;
