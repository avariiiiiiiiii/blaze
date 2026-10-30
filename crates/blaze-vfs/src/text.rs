//! A file's bytes as text, read in place (docs/v2/plan/design/frontend.md item 6,
//! docs/v2/plan/lanes/frontend/text.md): the front end's `Arc<str>` of a file is the tree's
//! own allocation, checked to be UTF-8 and cast, never copied. Holding it keeps the bytes
//! alive as any reader's [`Arc`] does.
//!
//! The one unsafe line is the cast of `Arc<[u8]>` to `Arc<str>`; the safe version,
//! `Arc::<str>::from`, copies the file, which every file the front end reads would pay once
//! per content.

use std::{str::Utf8Error, sync::Arc};

use blaze_ids::ContentId;

use crate::{Mode, Reader, ViewId, Vfs};

/// `bytes` as text: the same allocation, its count moved, nothing copied; or where they stop
/// being UTF-8.
pub fn text_of(bytes: Arc<[u8]>) -> Result<Arc<str>, Utf8Error> {
    check(&bytes)?;
    let raw: *const [u8] = Arc::into_raw(bytes);
    // SAFETY: `raw` is from `Arc::<[u8]>::into_raw`, and `str` has `[u8]`'s layout (size,
    // alignment and length metadata), which is what `Arc::from_raw` asks of a pointer of
    // another type. The bytes were checked to be UTF-8 just above, and while two `Arc`s share
    // them neither can get them mutably, so they stay UTF-8 for as long as this one lives.
    Ok(unsafe { Arc::from_raw(raw as *const str) })
}

/// [`std::str::from_utf8`]'s answer for `bytes`: the ASCII they start with skipped 64 bytes at a
/// time (nearly all source is ASCII, and an OR of eight words vectorizes to a load and a test
/// per 32 or 64 bytes, at memory bandwidth), then std's check of the rest. The prefix ends
/// where a char starts, so the rest is UTF-8 exactly when the whole is; a refusal is asked of
/// the whole again, for where it stops.
fn check(bytes: &[u8]) -> Result<(), Utf8Error> {
    match std::str::from_utf8(&bytes[ascii_prefix(bytes)..]) {
        Ok(_) => Ok(()),
        Err(_) => std::str::from_utf8(bytes).map(drop),
    }
}

/// The length of the whole 64-byte chunks of ASCII `bytes` start with.
fn ascii_prefix(bytes: &[u8]) -> usize {
    const HIGH: u64 = 0x8080_8080_8080_8080;
    let (chunks, _) = bytes.as_chunks::<64>();
    let ascii = chunks
        .iter()
        .take_while(|chunk| {
            chunk.as_chunks::<8>().0.iter().fold(0, |any, word| any | u64::from_ne_bytes(*word))
                & HIGH
                == 0
        })
        .count();
    ascii * 64
}

impl Reader {
    /// The file at `path` under `root` as text ([`text_of`]): its content ID and mode, and its
    /// text or why it has none. `None` as for [`Reader::file`].
    pub fn text(
        &self,
        root: ContentId,
        path: &str,
    ) -> Option<(ContentId, Mode, Result<Arc<str>, Utf8Error>)> {
        let (id, mode, bytes) = self.file(root, path)?;
        Some((id, mode, text_of(bytes)))
    }
}

impl Vfs {
    /// The file at `path` in `view` as text ([`text_of`]): its content ID, and its text or why
    /// it has none. `None` as for [`Vfs::read`].
    pub fn text(&self, view: ViewId, path: &str) -> Option<(ContentId, Result<Arc<str>, Utf8Error>)> {
        let (node, _) = self.tree.entry(self.open_root(view), path)?;
        let bytes = self.tree.bytes(node)?;
        Some((self.tree.id(node), text_of(Arc::clone(bytes))))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use blaze_core::FileName;
    use blaze_ids::ContentId;

    use super::{ascii_prefix, check, text_of};
    use crate::{FILE_DOMAIN, Mode, Op, View, Vfs};

    /// The check against std's on bytes weighted to where it could differ: long ASCII runs that
    /// cross 64-byte chunks, then a multibyte char, a bad byte, or a char cut by the end.
    #[test]
    fn the_check_is_stds() {
        const PIECES: [&[u8]; 9] =
            [b"a", b"fn x() {}\n", b"\xc3\xa9", b"\xe2\x82\xac", b"\xf0\x9f\xa6\x80", b"\x80", b"\xc3", b"\xff", b"\xed\xa0\x80"];
        let mut state = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = move |below: u64| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state % below
        };
        for _ in 0..4000 {
            let mut bytes = vec![b'x'; next(300) as usize];
            for _ in 0..next(4) {
                let piece = PIECES[next(PIECES.len() as u64) as usize];
                let at = next(bytes.len() as u64 + 1) as usize;
                bytes.splice(at..at, piece.iter().copied());
            }
            assert_eq!(check(&bytes), std::str::from_utf8(&bytes).map(drop), "{bytes:?}");
            let ascii = ascii_prefix(&bytes);
            assert!(ascii % 64 == 0 && bytes[..ascii].is_ascii());
        }
    }

    #[test]
    fn the_ascii_prefix_stops_at_the_chunk_with_a_high_byte() {
        let mut bytes = vec![b'a'; 200];
        assert_eq!(ascii_prefix(&bytes), 192);
        bytes[130] = 0xc3;
        assert_eq!(ascii_prefix(&bytes), 128);
        bytes[0] = 0x80;
        assert_eq!(ascii_prefix(&bytes), 0);
        assert_eq!(ascii_prefix(b""), 0);
    }

    #[test]
    fn text_shares_the_bytes() {
        let bytes: Arc<[u8]> = Arc::from(&b"fn main() {}\n"[..]);
        let text = text_of(Arc::clone(&bytes)).expect("UTF-8");
        assert_eq!(&*text, "fn main() {}\n");
        assert_eq!(text.as_ptr(), bytes.as_ptr());
        assert_eq!(Arc::strong_count(&bytes), 2);
        drop(text);
        assert_eq!(Arc::strong_count(&bytes), 1);
    }

    #[test]
    fn bytes_that_are_not_utf8_say_where() {
        let bytes: Arc<[u8]> = Arc::from(&b"ok\xff\xfe"[..]);
        let error = text_of(Arc::clone(&bytes)).unwrap_err();
        assert_eq!(error.valid_up_to(), 2);
        assert_eq!(Arc::strong_count(&bytes), 1, "a refused text drops its count");
    }

    #[test]
    fn a_tree_file_is_read_as_its_own_bytes() {
        let mut vfs = Vfs::recover(FileName::new("state/journal").unwrap(), 0, &[]).0;
        let ops = [
            Op::Write { mode: Mode::Regular, path: "src/lib.rs".into(), bytes: b"mod a;\n".to_vec() },
            Op::Write { mode: Mode::Regular, path: "data.bin".into(), bytes: vec![0xff] },
        ];
        let empty = vfs.empty();
        let root = vfs.commit("main", empty, &ops).unwrap().root;

        let reader = vfs.reader();
        let (id, mode, text) = reader.text(root, "src/lib.rs").expect("a file");
        let text = text.expect("UTF-8");
        assert_eq!((id, mode, &*text), (ContentId::of(FILE_DOMAIN, b"mod a;\n"), Mode::Regular, "mod a;\n"));
        let (_, _, bytes) = reader.file(root, "src/lib.rs").unwrap();
        assert_eq!(text.as_ptr(), bytes.as_ptr(), "the tree's allocation, not a copy");
        assert!(reader.text(root, "data.bin").unwrap().2.is_err());
        assert!(reader.text(root, "src").is_none());

        let view = vfs.open(View { root }).unwrap();
        let (id, text) = vfs.text(view, "src/lib.rs").expect("a file");
        assert_eq!(id, ContentId::of(FILE_DOMAIN, b"mod a;\n"));
        assert_eq!(text.unwrap().as_ptr(), bytes.as_ptr());
        assert!(vfs.text(view, "missing.rs").is_none());
    }
}
