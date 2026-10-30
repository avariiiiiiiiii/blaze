//! Protocol functions for json.
use std::io::{self, BufRead, Write};

use serde::{Serialize, de::DeserializeOwned};

use super::{LONGEST, invalid, read_frame};

/// The next line that is a message, without its newline; `None` at the end of the stream or at
/// an empty line. A last line with no newline is an error: it may be a message cut short.
pub(crate) fn read<'a, R: BufRead + ?Sized>(
    inp: &mut R,
    buf: &'a mut String,
) -> io::Result<Option<&'a mut String>> {
    let mut bytes = std::mem::take(buf).into_bytes();
    loop {
        bytes.clear();
        let ended = read_frame(inp, b'\n', LONGEST, &mut bytes)?;
        if !ended && !bytes.is_empty() {
            // alloc: error path, naming the length.
            return Err(invalid(format!("a last line of {} bytes with no newline", bytes.len())));
        }
        bytes.pop();
        // alloc: none, the reused buffer's bytes become the `String`.
        *buf = String::from_utf8(bytes)
            // alloc: error path.
            .map_err(|e| invalid(format!("a line that is not UTF-8: {}", e.utf8_error())))?;

        if buf.is_empty() {
            return Ok(None);
        }

        // Some ill behaved macro try to use stdout for debugging
        // We ignore it here
        if !buf.starts_with('{') {
            tracing::error!("proc-macro tried to print : {}", buf);
            bytes = std::mem::take(buf).into_bytes();
            continue;
        }

        return Ok(Some(buf));
    }
}

pub(crate) fn write<W: Write + ?Sized>(out: &mut W, buf: &String) -> io::Result<()> {
    tracing::debug!("> {}", buf);
    out.write_all(buf.as_bytes())?;
    out.write_all(b"\n")?;
    out.flush()
}

pub(crate) fn encode<T: Serialize>(msg: &T) -> io::Result<String> {
    Ok(serde_json::to_string(msg)?)
}

/// The one message of the line `buf`: anything but whitespace after it is refused. The messages
/// nest a few levels deep, so serde_json's depth limit stands and a line nesting deeper is
/// refused, not followed down the stack.
pub(crate) fn decode<T: DeserializeOwned>(buf: &mut str) -> io::Result<T> {
    let mut deserializer = serde_json::Deserializer::from_str(buf);
    let msg = T::deserialize(&mut deserializer)?;
    deserializer.end()?;
    Ok(msg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_last_line_with_no_newline_is_refused() {
        let mut inp: &[u8] = b"{\"ApiVersionCheck\":{}}";
        let error = read(&mut inp, &mut String::new()).unwrap_err().to_string();
        assert!(error.contains("no newline"), "{error}");
    }

    #[test]
    fn printed_lines_are_skipped_and_a_message_is_read_whole() {
        let mut inp: &[u8] = b"hello\n{\"a\":1}\n";
        let mut buf = String::new();
        assert_eq!(read(&mut inp, &mut buf).unwrap().map(|it| it.clone()), Some("{\"a\":1}".into()));
        assert!(read(&mut inp, &mut buf).unwrap().is_none());
    }

    #[test]
    fn bytes_after_the_message_are_refused() {
        assert!(decode::<serde_json::Value>(&mut String::from("{} ")).is_ok());
        assert!(decode::<serde_json::Value>(&mut String::from("{} {}")).is_err());
        assert!(decode::<serde_json::Value>(&mut "[".repeat(10_000)).is_err());
    }
}
