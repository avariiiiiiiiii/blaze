//! What the daemon and its clients say to each other, and how: frames of a four-byte
//! big-endian length and then one CBOR item in RFC 8949 section 4.2.1's core deterministic
//! encoding ([`cbor`]). One value has one encoding, so a recorded session can be hashed,
//! replayed into a renderer and compared byte for byte with another (docs/roadmap.md, "The
//! agent API").
//!
//! The item of a frame is a [`Message`]: the [`SCHEMA`] it is written in, the id of the
//! request it belongs to and its body. JSON is only a rendering of the same records for
//! people, never the wire.

pub mod cbor;
pub mod events;
pub mod follow;

#[cfg(test)]
mod tests;

use std::io::{self, Read, Write};

use serde::{Deserialize, Serialize, de::DeserializeOwned};

/// The version of the schema of every body: a reader of another refuses the message by
/// name rather than guess at its fields.
pub const SCHEMA: u32 = 1;

/// The longest item a frame carries: a build names every file it read.
pub const LONGEST_FRAME: u32 = 16 << 20;

/// One frame's item: a body of [`SCHEMA`], answering or asking request `id`. Replies and
/// events carry the id of the request they belong to, so a connection can carry several.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Message<T> {
    pub schema: u32,
    pub id: u64,
    pub body: T,
}

impl<T> Message<T> {
    pub fn new(id: u64, body: T) -> Message<T> {
        Message { schema: SCHEMA, id, body }
    }
}

/// Writes `item`, one encoded CBOR item, as one frame.
pub fn write_frame(to: &mut impl Write, item: &[u8]) -> io::Result<()> {
    let len = u32::try_from(item.len()).ok().filter(|&len| len <= LONGEST_FRAME);
    let Some(len) = len else {
        let said = format!("a frame of {} bytes, longer than {LONGEST_FRAME}", item.len());
        return Err(io::Error::new(io::ErrorKind::InvalidInput, said));
    };
    let mut frame = Vec::with_capacity(4 + item.len());
    frame.extend(len.to_be_bytes());
    frame.extend(item);
    to.write_all(&frame)
}

/// The item of the next frame, or `None` when the stream ends before one begins. A stream
/// that ends inside a frame is one whose writer did not live to finish it.
pub fn read_frame(from: &mut impl Read) -> io::Result<Option<Vec<u8>>> {
    let mut len = [0; 4];
    let mut read = 0;
    while read < len.len() {
        match from.read(&mut len[read..]) {
            Ok(0) if read == 0 => return Ok(None),
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(n) => read += n,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    let len = u32::from_be_bytes(len);
    if len > LONGEST_FRAME {
        let said = format!("a frame of {len} bytes, longer than {LONGEST_FRAME}");
        return Err(io::Error::new(io::ErrorKind::InvalidData, said));
    }
    // Room for at most a megabyte before the bytes are here, the length being the peer's
    // word: a frame of that much is read in one go.
    let mut item = Vec::with_capacity((len as usize).min(1 << 20));
    from.take(u64::from(len)).read_to_end(&mut item)?;
    if item.len() != len as usize {
        return Err(io::ErrorKind::UnexpectedEof.into());
    }
    Ok(Some(item))
}

/// Writes `value` as one frame.
pub fn send<T: Serialize + ?Sized>(to: &mut impl Write, value: &T) -> io::Result<()> {
    let item = cbor::to_vec(value).map_err(invalid)?;
    write_frame(to, &item)
}

/// The value of the next frame, or `None` when the stream ends before one begins. An item
/// not in the deterministic encoding is refused, naming the first offence.
pub fn receive<T: DeserializeOwned>(from: &mut impl Read) -> io::Result<Option<T>> {
    match read_frame(from)? {
        Some(item) => cbor::from_slice(&item).map(Some).map_err(invalid),
        None => Ok(None),
    }
}

fn invalid(error: cbor::Error) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error)
}
