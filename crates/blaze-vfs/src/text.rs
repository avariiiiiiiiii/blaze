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
    std::str::from_utf8(&bytes)?;
    let raw: *const [u8] = Arc::into_raw(bytes);
    // SAFETY: `raw` is from `Arc::<[u8]>::into_raw`, and `str` has `[u8]`'s layout (size,
    // alignment and length metadata), which is what `Arc::from_raw` asks of a pointer of
    // another type. The bytes were checked to be UTF-8 just above, and while two `Arc`s share
    // them neither can get them mutably, so they stay UTF-8 for as long as this one lives.
    Ok(unsafe { Arc::from_raw(raw as *const str) })
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

    use super::text_of;
    use crate::{FILE_DOMAIN, Mode, Op, View, Vfs};

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
