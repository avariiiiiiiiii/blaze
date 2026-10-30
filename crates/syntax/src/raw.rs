//! The crate's one unsafe module (docs/v2/plan/design/unsafe.md): a text built straight into
//! its shared allocation. The safe way, a `String` and then `Arc::<str>::from`, copies the
//! whole file twice per edit and allocates twice; this copies it once.

use std::sync::Arc;

/// `parts` one after the other, written once into the shared text's allocation.
pub(crate) fn joined(parts: &[&str]) -> Arc<str> {
    let len = parts.iter().map(|it| it.len()).sum();
    let mut bytes = Arc::<[u8]>::new_uninit_slice(len);
    let out = Arc::get_mut(&mut bytes).expect("a new Arc has no other owner");
    let mut at = 0;
    for part in parts {
        let to = &mut out[at..at + part.len()];
        // SAFETY: `to` is `part.len()` writable bytes (the bounds-checked slice above) and
        // `part` is a borrowed `&str`, not this new allocation, so they do not overlap. A
        // shorter `to` would write past `out`.
        unsafe {
            std::ptr::copy_nonoverlapping(part.as_ptr(), to.as_mut_ptr().cast::<u8>(), to.len())
        };
        at += part.len();
    }
    assert_eq!(at, len, "every byte of the text is written");
    // SAFETY: the loop wrote `out[0..len]`, all of it (the assert above); reading an
    // unwritten byte would be undefined behavior.
    let bytes = unsafe { bytes.assume_init() };
    // SAFETY: the bytes are whole `&str`s one after the other, so valid UTF-8, and `str` has
    // the layout of `[u8]`, so the pointer from `into_raw` is a valid `Arc<str>` allocation.
    // Bytes that were not UTF-8 would break every `str` method on the result.
    unsafe { Arc::from_raw(Arc::into_raw(bytes) as *const str) }
}

#[cfg(test)]
mod tests {
    /// The safe reference version (`concat`) gives the same text.
    #[test]
    fn joined_is_the_concatenation() {
        let long = "x".repeat(100_000);
        let cases: [&[&str]; 6] = [
            &[],
            &[""],
            &["a"],
            &["", "é", ""],
            &["fn f() {", " x ", "}\n", "名"],
            &[long.as_str(), "🦀", "y"],
        ];
        for parts in cases {
            assert_eq!(&*super::joined(parts), parts.concat());
        }
    }
}
