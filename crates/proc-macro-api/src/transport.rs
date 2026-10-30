//! Contains construct for transport of messages.
pub(crate) mod json;
pub(crate) mod postcard;

use std::io::{self, BufRead};

/// The longest frame either transport reads: past it reading fails, so a server that never ends
/// a frame can't grow the client's buffer without bound. A frame is one message, a whole
/// expansion's answer at most, which no real macro makes near this long.
pub const LONGEST: usize = 1 << 30;

/// Appends to `buf` what `inp` has up to and including the first `end`, and says whether it found
/// one before the stream ended. Fails, naming the length, once the frame would pass `longest`
/// bytes ([`LONGEST`]).
pub(crate) fn read_frame<R: BufRead + ?Sized>(
    inp: &mut R,
    end: u8,
    longest: usize,
    buf: &mut Vec<u8>,
) -> io::Result<bool> {
    loop {
        let available = match inp.fill_buf() {
            Ok(it) => it,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        if available.is_empty() {
            return Ok(false);
        }
        let (taken, found) = match available.iter().position(|&it| it == end) {
            Some(at) => (at + 1, true),
            None => (available.len(), false),
        };
        if buf.len() + taken > longest {
            return Err(invalid(format!(
                "a frame of more than {longest} bytes: {} read and no end yet",
                buf.len() + taken
            )));
        }
        buf.extend_from_slice(&available[..taken]);
        inp.consume(taken);
        if found {
            return Ok(true);
        }
    }
}

pub(crate) fn invalid(why: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, why)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_frame_past_the_longest_is_refused_naming_its_length() {
        let mut inp = io::BufReader::with_capacity(4, &[1u8; 40][..]);
        let error = read_frame(&mut inp, 0, 16, &mut Vec::new()).unwrap_err().to_string();
        assert!(error.contains("more than 16 bytes: 20 read"), "{error}");
        let mut inp: &[u8] = &[1; 16];
        let mut buf = Vec::new();
        assert!(!read_frame(&mut inp, 0, 16, &mut buf).unwrap());
        assert_eq!(buf.len(), 16);
    }
}
