//! Interned strings: symbols declared at compile time ([`symbols`]) and ones interned at
//! runtime, both never freed.
//!
//! A [`Symbol`] points at a `&'static str`: a declared symbol's is a `static`, a runtime one's
//! sits in the append-only arena of [`crate::arena`] beside its bytes. The table is filled with
//! the declared symbols first, so interning a declared symbol's text gives the declared symbol.

use std::{
    alloc::Layout,
    fmt,
    hash::BuildHasher,
    ptr::NonNull,
    sync::OnceLock,
};

use rustc_hash::FxBuildHasher;

use crate::arena::{Entry, Store};

pub mod symbols;

/// # Invariant
///
/// `repr` points at a `&'static str` that never moves, changes or is freed: a `static`, or one
/// in [`STORE`]'s arena. Two symbols are one string exactly when they are one pointer.
#[derive(PartialEq, Eq, Hash, Clone, Copy)]
pub struct Symbol {
    repr: NonNull<&'static str>,
}

// SAFETY: A `Symbol` is a shared reference to a `&'static str` that is never written again.
unsafe impl Send for Symbol {}
unsafe impl Sync for Symbol {}

impl fmt::Debug for Symbol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.as_str().fmt(f)
    }
}

const _: () = assert!(size_of::<Symbol>() == size_of::<NonNull<()>>());
const _: () = assert!(align_of::<Symbol>() == align_of::<NonNull<()>>());
const _: () = assert!(size_of::<Option<Symbol>>() == size_of::<NonNull<()>>());

static STORE: OnceLock<Store> = OnceLock::new();

fn store() -> &'static Store {
    STORE.get_or_init(symbols::prefill)
}

fn hash(s: &str) -> u64 {
    FxBuildHasher.hash_one(s)
}

/// Makes `symbol` the one `Symbol::intern` gives for its text, unless one is already.
fn prefill_one(store: &Store, symbol: Symbol) {
    let s = symbol.as_str();
    // SAFETY: Every value of the store is a `&'static str`, as `Symbol::repr` points at.
    let same = |at: NonNull<u8>| unsafe { *at.cast::<&'static str>().as_ref() == s };
    if let Entry::Vacant(vacant) = store.entry(hash(s), same) {
        vacant.publish(symbol.repr.cast());
    }
}

impl Symbol {
    /// The symbol of the `static` `r`.
    pub(crate) const fn from_static(r: &'static &'static str) -> Symbol {
        // SAFETY: A reference is not null.
        Symbol { repr: unsafe { NonNull::new_unchecked((r as *const &'static str).cast_mut()) } }
    }

    pub fn intern(s: &str) -> Self {
        let store = store();
        // SAFETY: Every value of the store is a `&'static str`.
        let same = |at: NonNull<u8>| unsafe { *at.cast::<&'static str>().as_ref() == s };
        let at = match store.entry(hash(s), same) {
            Entry::Found(at) => at,
            Entry::Vacant(vacant) => {
                let (layout, bytes_at) = Layout::new::<&'static str>()
                    .extend(Layout::array::<u8>(s.len()).expect("a symbol's layout"))
                    .expect("a symbol's layout");
                vacant.insert(layout, |to| {
                    // SAFETY: `to` is fresh memory of `layout`: a `&str` and, at `bytes_at`,
                    // `s.len()` bytes. Neither moves or is freed, so the `&str` written points
                    // at bytes that live for `'static`.
                    unsafe {
                        let bytes = to.as_ptr().add(bytes_at);
                        bytes.copy_from_nonoverlapping(s.as_ptr(), s.len());
                        let bytes = std::slice::from_raw_parts(bytes, s.len());
                        let text: &'static str = std::str::from_utf8_unchecked(bytes);
                        to.cast::<&'static str>().write(text);
                    }
                })
            }
        };
        Symbol { repr: at.cast() }
    }

    pub fn empty() -> Self {
        symbols::__empty
    }

    #[inline]
    pub fn as_str(&self) -> &str {
        // SAFETY: `repr`'s invariant.
        unsafe { *self.repr.as_ref() }
    }

    /// The `&'static str` the symbol points at.
    #[inline]
    pub(crate) fn as_static(&self) -> &'static &'static str {
        // SAFETY: `repr`'s invariant.
        unsafe { self.repr.as_ref() }
    }
}

impl fmt::Display for Symbol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.as_str().fmt(f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sym;

    #[test]
    fn smoke_test() {
        let hello = Symbol::intern("hello");
        let world = Symbol::intern("world");
        let bang = Symbol::intern("!");
        let q = Symbol::intern("?");
        assert_eq!(Symbol::intern("!"), bang);
        assert_eq!(Symbol::intern("hello"), hello);
        assert!(store().len() >= 4);
        assert_ne!(hello, world);
        assert_ne!(bang, q);
        let default = Symbol::intern("default");
        assert_eq!(
            "hello default world!",
            format!("{} {} {}{}", hello.as_str(), default.as_str(), world.as_str(), bang.as_str())
        );
        assert_eq!(format!("{hello:?} {world}"), "\"hello\" world");
    }

    #[test]
    fn round_trips() {
        for s in ["self", "Self", "", "fn", "0", "255", "256", "a new symbol", "ünïcödé", "x"] {
            assert_eq!(Symbol::intern(s).as_str(), s);
            assert_eq!(Symbol::intern(s), Symbol::intern(&s.to_owned()));
        }
        let long = "y".repeat(1 << 20);
        assert_eq!(Symbol::intern(&long).as_str(), long);
    }

    #[test]
    fn declared_symbols_are_interned() {
        assert_eq!(Symbol::intern("self"), sym::self_);
        assert_eq!(Symbol::intern("Self"), sym::Self_);
        assert_eq!(Symbol::intern("..."), sym::dotdotdot);
        assert_eq!(Symbol::intern(""), Symbol::empty());
        assert_eq!(Symbol::empty(), sym::__empty);
        assert_eq!(sym::self_.as_str(), "self");
        assert_ne!(sym::self_, sym::Self_);
        const DECLARED: Symbol = sym::self_;
        assert_eq!(DECLARED, Symbol::intern("self"));
    }

    #[test]
    fn integers() {
        for n in [0usize, 1, 9, 10, 255, 256, 1_000, 123_456_789] {
            let declared = sym::Integer::get(n);
            assert_eq!(declared.as_str(), n.to_string());
            assert_eq!(declared, Symbol::intern(&n.to_string()));
            assert_eq!(sym::Integer::as_uint(&declared), Some(n));
            assert_eq!(sym::Integer::as_uint(&Symbol::intern(&n.to_string())), Some(n));
        }
        assert_eq!(sym::Integer::as_uint(&sym::self_), None);
        assert_eq!(sym::Integer::as_uint(&Symbol::intern("12a")), None);
        assert_eq!(sym::RaGeneratedName::get(3).as_str(), "<ra@gennew>3");
        assert_eq!(sym::RaGeneratedName::get(300).as_str(), "<ra@gennew>300");
        assert_eq!(sym::RaGeneratedName::get(300), Symbol::intern("<ra@gennew>300"));
    }

    #[test]
    fn symbols_from_many_threads() {
        let seen: Vec<Vec<Symbol>> = std::thread::scope(|scope| {
            let workers: Vec<_> = (0..16u32)
                .map(|worker| {
                    scope.spawn(move || {
                        (0..10_000u32)
                            .map(|n| (n + worker * 613) % 10_000)
                            .map(|n| Symbol::intern(&format!("racing {n}")))
                            .collect::<Vec<_>>()
                    })
                })
                .collect();
            workers.into_iter().map(|it| it.join().unwrap()).collect()
        });
        for (worker, symbols) in seen.iter().enumerate() {
            for (i, &symbol) in symbols.iter().enumerate() {
                let n = (i as u32 + worker as u32 * 613) % 10_000;
                assert_eq!(symbol.as_str(), format!("racing {n}"));
                assert_eq!(symbol, seen[0][n as usize]);
            }
        }
    }
}
