//! Interned strings: symbols declared at compile time ([`symbols`]) and ones interned at
//! runtime, both never freed.
//!
//! A [`Symbol`] points at a [`Text`]: its `&'static str` and its order key. A declared symbol's
//! is a `static`, a runtime one's sits in the append-only arena of [`crate::arena`] beside its
//! bytes. The table is filled with the declared symbols first, so interning a declared symbol's
//! text gives the declared symbol.

use std::{
    alloc::Layout,
    cmp::Ordering,
    fmt,
    hash::BuildHasher,
    ptr::NonNull,
    sync::OnceLock,
};

use rustc_hash::FxBuildHasher;

use crate::arena::{Entry, Store};

pub mod symbols;

/// The order key of a name: its first 8 bytes big-endian, zero-padded. Two keys compare as the
/// names' first 8 bytes do, so `order_key(a) < order_key(b)` implies `a < b` and only equal
/// keys need the bytes compared.
#[inline]
pub const fn order_key(bytes: &[u8]) -> u64 {
    let mut key = [0u8; 8];
    let mut i = 0;
    while i < 8 && i < bytes.len() {
        key[i] = bytes[i];
        i += 1;
    }
    u64::from_be_bytes(key)
}

/// What a [`Symbol`] points at: 24 bytes, the order key read by [`Symbol::cmp_order`] beside
/// the text.
#[repr(C)]
pub(crate) struct Text {
    key: u64,
    str: &'static str,
}

const _: () = assert!(size_of::<Text>() == 8 + size_of::<&str>());

impl Text {
    pub(crate) const fn new(str: &'static str) -> Text {
        Text { key: order_key(str.as_bytes()), str }
    }
}

/// # Invariant
///
/// `repr` points at a [`Text`] that never moves, changes or is freed: a `static`, or one in
/// [`STORE`]'s arena, its `key` the [`order_key`] of its `str`. Two symbols are one string
/// exactly when they are one pointer.
#[derive(PartialEq, Eq, Hash, Clone, Copy)]
pub struct Symbol {
    repr: NonNull<Text>,
}

// SAFETY: A `Symbol` is a shared reference to a `Text` that is never written again.
unsafe impl Send for Symbol {}
// SAFETY: As for `Send`: a `Symbol` is a shared reference to a `Text` that is never written again.
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
    // SAFETY: Every value of the store is a `Text`, as `Symbol::repr` points at.
    let same = |at: NonNull<u8>| unsafe { at.cast::<Text>().as_ref().str == s };
    if let Entry::Vacant(vacant) = store.entry(hash(s), same) {
        vacant.publish(symbol.repr.cast());
    }
}

impl Symbol {
    /// The symbol of the `static` `r`.
    pub(crate) const fn from_static(r: &'static Text) -> Symbol {
        // SAFETY: A reference is not null.
        Symbol { repr: unsafe { NonNull::new_unchecked((r as *const Text).cast_mut()) } }
    }

    pub fn intern(s: &str) -> Self {
        let store = store();
        // SAFETY: Every value of the store is a `Text`.
        let same = |at: NonNull<u8>| unsafe { at.cast::<Text>().as_ref().str == s };
        let at = match store.entry(hash(s), same) {
            Entry::Found(at) => at,
            Entry::Vacant(vacant) => {
                let (layout, bytes_at) = Layout::new::<Text>()
                    .extend(Layout::array::<u8>(s.len()).expect("a symbol's layout"))
                    .expect("a symbol's layout");
                let key = order_key(s.as_bytes());
                vacant.insert(layout, |to| {
                    // SAFETY: `to` is fresh memory of `layout`, and `bytes_at` is where
                    // `Layout::extend` put the bytes in it, so the pointer stays in the allocation.
                    let bytes = unsafe { to.as_ptr().add(bytes_at) };
                    // SAFETY: `bytes` has room for `s.len()` bytes (the array part of `layout`)
                    // and is fresh memory, so it cannot overlap `s`.
                    unsafe { bytes.copy_from_nonoverlapping(s.as_ptr(), s.len()) };
                    // SAFETY: The `s.len()` bytes at `bytes` were just written, and the arena
                    // never moves, writes or frees them, so they live for `'static`.
                    let bytes: &'static [u8] = unsafe { std::slice::from_raw_parts(bytes, s.len()) };
                    // SAFETY: `bytes` is a copy of `s`, a `&str`, so it is UTF-8.
                    let str: &'static str = unsafe { std::str::from_utf8_unchecked(bytes) };
                    // SAFETY: `layout` starts with a `Text` (`Layout::new`), so `to` is aligned
                    // for it and has room, and nothing was there to drop.
                    unsafe { to.cast::<Text>().write(Text { key, str }) };
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
        self.as_static().str
    }

    /// The symbol's [`order_key`], stored beside it when it was interned.
    #[inline]
    pub fn order_key(&self) -> u64 {
        self.as_static().key
    }

    /// The order of `self.as_str().cmp(other.as_str())`: the order keys first, the bytes only
    /// when they tie.
    #[inline]
    pub fn cmp_order(&self, other: &Symbol) -> Ordering {
        if self == other {
            return Ordering::Equal;
        }
        let (a, b) = (self.as_static(), other.as_static());
        match a.key.cmp(&b.key) {
            Ordering::Equal => a.str.as_bytes().cmp(b.str.as_bytes()),
            unequal => unequal,
        }
    }

    /// The symbol's index in [`symbols::Declared`] when it is a declared one, from one
    /// pointer subtraction and a bounds check; `None` for one interned at run time or of a list
    /// such as [`symbols::Integer`]. Interning a declared symbol's text gives the declared
    /// symbol, so a name read from source has its index too.
    #[inline]
    pub fn declared_index(&self) -> Option<u16> {
        symbols::declared_index(self.as_static()).map(|it| it as u16)
    }

    /// The `Text` the symbol points at.
    #[inline]
    pub(crate) fn as_static(&self) -> &'static Text {
        // SAFETY: `repr`'s invariant.
        unsafe { self.repr.as_ref() }
    }
}

impl fmt::Display for Symbol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.as_str().fmt(f)
    }
}

/// A fixed set of declared symbols (lang items, builtin attributes, target features) as the
/// `u16` index of each in the set: a table by [`Symbol::declared_index`], built by `const fn`,
/// so recognizing a member is a pointer subtraction and one load, with no hash and no string
/// compared. 2 bytes a declared symbol.
pub struct KnownNames {
    by_declared: [u16; symbols::DECLARED],
}

impl KnownNames {
    const NONE: u16 = u16::MAX;

    /// The set whose member `i` is `members[i]`. A symbol named twice is the first member
    /// that names it.
    pub const fn new(members: &[symbols::Declared]) -> KnownNames {
        assert!(members.len() < KnownNames::NONE as usize, "fewer than 65535 members");
        let mut by_declared = [KnownNames::NONE; symbols::DECLARED];
        let mut i = members.len();
        while i > 0 {
            i -= 1;
            by_declared[members[i] as usize] = i as u16;
        }
        KnownNames { by_declared }
    }

    /// The index in the set of `symbol`, if it is a member.
    #[inline]
    pub fn get(&self, symbol: &Symbol) -> Option<u16> {
        let at = *self.by_declared.get(usize::from(symbol.declared_index()?))?;
        (at != KnownNames::NONE).then_some(at)
    }

    /// The index in the set of the declared symbol `declared`, if it is a member.
    pub const fn get_declared(&self, declared: symbols::Declared) -> Option<u16> {
        match self.by_declared[declared as usize] {
            KnownNames::NONE => None,
            at => Some(at),
        }
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
    fn declared_indices() {
        use crate::sym::Declared;
        assert_eq!(sym::self_.declared_index(), Some(Declared::self_ as u16));
        assert_eq!(Symbol::intern("self").declared_index(), Some(Declared::self_ as u16));
        assert_eq!(sym::Self_.declared_index(), Some(Declared::Self_ as u16));
        assert_eq!(Symbol::empty().declared_index(), Some(Declared::__empty as u16));
        assert_eq!(Symbol::intern("not a declared symbol").declared_index(), None);
        assert_eq!(sym::Integer::get(3).declared_index(), None);
        assert_eq!(sym::RaGeneratedName::get(3).declared_index(), None);
        assert!(sym::DECLARED <= u16::MAX as usize);
    }

    #[test]
    fn known_names() {
        use crate::sym::Declared;
        static SET: KnownNames = KnownNames::new(&[Declared::fn_, Declared::self_, Declared::fn_]);
        assert_eq!(SET.get(&sym::fn_), Some(0));
        assert_eq!(SET.get(&Symbol::intern("fn")), Some(0));
        assert_eq!(SET.get(&sym::self_), Some(1));
        assert_eq!(SET.get(&sym::Self_), None);
        assert_eq!(SET.get(&Symbol::intern("a runtime name")), None);
        assert_eq!(SET.get(&sym::Integer::get(0)), None);
        assert_eq!(SET.get_declared(Declared::self_), Some(1));
        assert_eq!(SET.get_declared(Declared::crate_), None);
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

    #[test]
    fn order_keys() {
        assert_eq!(order_key(b""), 0);
        assert_eq!(order_key(b"a"), 0x61 << 56);
        assert_eq!(order_key(b"abcdefgh"), u64::from_be_bytes(*b"abcdefgh"));
        assert_eq!(order_key(b"abcdefghij"), u64::from_be_bytes(*b"abcdefgh"));
        assert_eq!(order_key(b"a\0"), order_key(b"a"));
        for s in ["", "a", "self", "a much longer runtime symbol", "ünïcödé"] {
            assert_eq!(Symbol::intern(s).order_key(), order_key(s.as_bytes()));
        }
        assert_eq!(sym::self_.order_key(), order_key(b"self"));
        assert_eq!(sym::Integer::get(42).order_key(), order_key(b"42"));
        assert_eq!(sym::RaGeneratedName::get(7).order_key(), order_key(b"<ra@gennew>7"));
    }

    fn check_order(names: &[&str]) {
        let symbols: Vec<Symbol> = names.iter().map(|s| Symbol::intern(s)).collect();
        for a in &symbols {
            for b in &symbols {
                assert_eq!(a.cmp_order(b), a.as_str().cmp(b.as_str()), "{a:?} against {b:?}");
            }
        }
        let mut by_key = symbols.clone();
        by_key.sort_by(Symbol::cmp_order);
        let mut by_str = symbols;
        by_str.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        assert_eq!(by_key, by_str);
    }

    #[test]
    fn cmp_order_is_str_order() {
        check_order(&[
            "",
            "\0",
            "a",
            "a\0",
            "a\0b",
            "a\u{1}",
            "ab",
            "abcdefgh",
            "abcdefgh\0",
            "abcdefgha",
            "abcdefghb",
            "abcdefgi",
            "self",
            "Self",
            "_",
            "~",
            "\u{7f}",
            "ünïcödé",
            "ü",
            "0",
            "255",
            "256",
            "<ra@gennew>3",
            "<ra@gennew>30",
        ]);
    }

    #[test]
    fn cmp_order_matches_str_order_on_random_names() {
        // A splitmix64 stream: seeded, so a failure reproduces.
        let mut state = 0x5eed_0f0d_e5u64;
        let mut next = move || {
            state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut z = state;
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            z ^ (z >> 31)
        };
        // Few letters and lengths around 8 so that many names share an order key.
        let alphabet = ['\0', 'a', 'b', 'é', '_'];
        let names: Vec<String> = (0..400)
            .map(|_| {
                let len = (next() % 13) as usize;
                (0..len).map(|_| alphabet[(next() % alphabet.len() as u64) as usize]).collect()
            })
            .collect();
        let names: Vec<&str> = names.iter().map(String::as_str).collect();
        check_order(&names);
    }
}
