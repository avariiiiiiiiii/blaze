//! Postcard encode and decode implementations.

use std::io::{self, BufRead, Write};

use serde::{Deserialize, Serialize, de::DeserializeOwned};

use super::{LONGEST, invalid, read_frame};

/// The next frame, up to and including its terminating zero; `None` once the stream ends between
/// frames. A stream ending inside a frame is an error.
pub(crate) fn read<'a, R: BufRead + ?Sized>(
    inp: &mut R,
    buf: &'a mut Vec<u8>,
) -> io::Result<Option<&'a mut Vec<u8>>> {
    buf.clear();
    let ended = read_frame(inp, 0, LONGEST, buf)?;
    if buf.is_empty() {
        return Ok(None);
    }
    if !ended {
        // alloc: error path, naming the length.
        return Err(invalid(format!("a frame of {} bytes with no terminating zero", buf.len())));
    }
    Ok(Some(buf))
}

pub(crate) fn write<W: Write + ?Sized>(out: &mut W, buf: &[u8]) -> io::Result<()> {
    out.write_all(buf)?;
    out.flush()
}

/// `msg` as one frame of its own: what [`Message::write`](crate::legacy_protocol::msg::Message)
/// sends. A conversation's frames go through [`encode_into`] instead.
pub(crate) fn encode<T: Serialize>(msg: &T) -> io::Result<Vec<u8>> {
    // alloc: a message written whole, once per handshake or by a test.
    let mut frame = Vec::new();
    encode_into(msg, &mut frame)?;
    Ok(frame)
}

/// `msg` as one COBS frame, its terminating zero last, in `frame`, which is cleared first and
/// keeps its capacity: a connection encodes every frame into the one buffer.
pub(crate) fn encode_into<T: Serialize + ?Sized>(msg: &T, frame: &mut Vec<u8>) -> io::Result<()> {
    frame.clear();
    let cobs = postcard::ser_flavors::Cobs::try_new(Filling(frame))
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    postcard::serialize_with_flavor(msg, cobs).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// A postcard flavor writing into a borrowed buffer, from its start: COBS goes back to fill in
/// each block's code byte, so it indexes what it wrote.
struct Filling<'a>(&'a mut Vec<u8>);

impl postcard::ser_flavors::Flavor for Filling<'_> {
    type Output = ();

    #[inline]
    fn try_extend(&mut self, data: &[u8]) -> postcard::Result<()> {
        self.0.extend_from_slice(data);
        Ok(())
    }

    #[inline]
    fn try_push(&mut self, data: u8) -> postcard::Result<()> {
        self.0.push(data);
        Ok(())
    }

    fn finalize(self) -> postcard::Result<()> {
        Ok(())
    }
}

impl std::ops::Index<usize> for Filling<'_> {
    type Output = u8;

    #[inline]
    fn index(&self, at: usize) -> &u8 {
        &self.0[at]
    }
}

impl std::ops::IndexMut<usize> for Filling<'_> {
    #[inline]
    fn index_mut(&mut self, at: usize) -> &mut u8 {
        &mut self.0[at]
    }
}

/// The one message of the frame `buf` ([`read`]'s, its zero last), decoded in place. A frame
/// with bytes after its message is refused.
pub(crate) fn decode<T: DeserializeOwned>(buf: &mut [u8]) -> io::Result<T> {
    let len = unframe(buf)?;
    parse(&buf[..len])
}

/// Undoes the COBS encoding of the frame `buf` ([`read`]'s, its zero last) in place, returning
/// the length of the message's bytes at its start, which [`parse`] reads, as often as it likes.
pub(crate) fn unframe(buf: &mut [u8]) -> io::Result<usize> {
    let Some((0, frame)) = buf.split_last_mut() else {
        // alloc: error path.
        return Err(invalid("a frame not ending in a zero".to_owned()));
    };
    unstuff(frame)
}

/// The one message of `bytes`, an [`unframe`]d frame's, borrowing from them what it can: a
/// frame with bytes after its message is refused.
pub(crate) fn parse<'a, T: Deserialize<'a>>(bytes: &'a [u8]) -> io::Result<T> {
    let (msg, rest) = postcard::take_from_bytes::<T>(bytes)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    if !rest.is_empty() {
        // alloc: error path, naming the length.
        return Err(invalid(format!("{} bytes after the message in its frame", rest.len())));
    }
    Ok(msg)
}

/// Undoes the COBS encoding of `frame` (its terminating zero left off) in place, returning the
/// decoded length. Each block is a code byte `c` and `c - 1` bytes, then a zero unless `c` is
/// `0xFF` or the block is the last.
fn unstuff(frame: &mut [u8]) -> io::Result<usize> {
    let (mut read, mut written) = (0, 0);
    while read < frame.len() {
        let code = usize::from(frame[read]);
        if code == 0 {
            // alloc: error path, naming the offset.
            return Err(invalid(format!("a zero inside a frame, at {read}")));
        }
        let end = read + code;
        if end > frame.len() {
            // alloc: error path, naming the offset.
            return Err(invalid(format!(
                "a block of {} bytes at {read} past the frame's end at {}",
                code - 1,
                frame.len()
            )));
        }
        frame.copy_within(read + 1..end, written);
        written += code - 1;
        read = end;
        if code != 0xFF && read < frame.len() {
            frame[written] = 0;
            written += 1;
        }
    }
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unstuffing_undoes_postcard_cobs() {
        let messages: [Vec<u8>; 4] =
            [vec![], vec![0; 3], (0..=255).collect(), (0..600).map(|it| (it % 7) as u8).collect()];
        for message in messages {
            let mut frame = postcard::to_allocvec_cobs(&message).unwrap();
            assert_eq!(decode::<Vec<u8>>(&mut frame).unwrap(), message);
        }
    }

    #[test]
    fn a_frame_encoded_into_a_reused_buffer_is_postcards_own() {
        let mut frame = vec![9; 3];
        for message in [vec![], vec![0; 3], (0..=255).collect::<Vec<u8>>(), vec![7; 600]] {
            encode_into(&message, &mut frame).unwrap();
            assert_eq!(frame, postcard::to_allocvec_cobs(&message).unwrap());
            assert_eq!(decode::<Vec<u8>>(&mut frame.clone()).unwrap(), message);
        }
    }

    #[test]
    fn a_frame_with_bytes_after_its_message_is_refused() {
        let mut bytes = postcard::to_allocvec(&7u32).unwrap();
        bytes.push(1);
        let mut frame = cobs_of(&bytes);
        assert!(decode::<u32>(&mut frame).unwrap_err().to_string().contains("after the message"));
    }

    #[test]
    fn a_stream_ending_inside_a_frame_is_refused() {
        let mut inp: &[u8] = &[3, 1, 2];
        let mut buf = Vec::new();
        assert!(read(&mut inp, &mut buf).unwrap_err().to_string().contains("no terminating zero"));
        let mut inp: &[u8] = &[];
        assert!(read(&mut inp, &mut buf).unwrap().is_none());
    }

    /// The COBS encoding of `bytes`, with its terminating zero.
    fn cobs_of(bytes: &[u8]) -> Vec<u8> {
        let mut out = vec![0];
        let mut code_at = 0;
        for &byte in bytes {
            if byte == 0 {
                out[code_at] = (out.len() - code_at) as u8;
                code_at = out.len();
                out.push(0);
                continue;
            }
            out.push(byte);
            if out.len() - code_at == 0xFF {
                out[code_at] = 0xFF;
                code_at = out.len();
                out.push(0);
            }
        }
        out[code_at] = (out.len() - code_at) as u8;
        out.push(0);
        out
    }
}
