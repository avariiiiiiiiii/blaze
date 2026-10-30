//! A test binary's functions as selection sees them (docs/v2/plan/lanes/fastcompile/select.md,
//! "Keys"), made from what codegen says of the binary.
//!
//! Each function is a [`Version`]: its identity ([`FnId`], the same across edits of its body)
//! and its bound code ([`Bound`]: its code hash and what each of its refs binds to). A code
//! hash alone names each callee by its interface, so a caller edited to call another function
//! of the same signature keeps its code hash; its bound code does not. Functions called
//! through a jump-table slot are logged when they run; every other function (one called only
//! directly, the premapped dependencies) is in one hash, [`Image::unlogged`], which every set
//! carries: a change to any of them reruns every test.
//!
//! A [`Set`] is what a pass is kept under: the versions of the logged functions a test
//! executed (or reaches), sorted by identity, with the unlogged hash. It is stored by content;
//! [`SetView`] reads it in place.

use std::fmt;

use blaze_cas::Key;
use blaze_ids::{ContentId, Hasher};

use crate::record::Record;

/// A function by what it is across versions: its item and, for an instance, its type
/// arguments by identity (docs/v2/plan/design/compile-time.md, idea 15). A rename or a move
/// makes another.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct FnId(pub ContentId);

/// A function's code as it runs: its code hash, each function or method it refers to by its
/// [`FnId`], each constant and static it refers to by the deep hash of its value, and for an
/// instance the facts of its type arguments its code reads.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct Bound(pub ContentId);

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct Version {
    pub id: FnId,
    pub bound: Bound,
}

const _: () = assert!(size_of::<Version>() == 64);

/// What [`Image::new`] refuses.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum TableError {
    /// A slot names a function the table does not have.
    NoFunction { slot: u32, function: u32 },
    /// Two slots name one function.
    TwoSlots { function: u32 },
    /// Two functions of the table have one identity.
    Twice(FnId),
    /// A test reaches a function the table does not have.
    NotInTable(u32),
    /// A record of another binary: its slot count is not the table's.
    OtherBinary { record: u32, table: u32 },
}

impl fmt::Display for TableError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TableError::NoFunction { slot, function } => {
                write!(f, "slot {slot} names function {function}, which the binary's table does not have")
            }
            TableError::TwoSlots { function } => write!(f, "function {function} of the binary's table has two slots"),
            TableError::Twice(id) => write!(f, "two functions of the binary's table are function {}", id.0),
            TableError::NotInTable(at) => write!(f, "a test reaches function {at}, which the binary's table does not have"),
            TableError::OtherBinary { record, table } => {
                write!(f, "the execution record has {record} slots and the binary {table}: it is of another binary")
            }
        }
    }
}

/// No function: an empty place of [`Lookup`], a slot of no function.
const NONE: u32 = u32::MAX;

/// Identity to table index, open-addressed by the identity's first eight bytes, which are a
/// BLAKE3 output's and so uniform: one probe, nothing hashed again.
struct Lookup {
    mask: usize,
    places: Vec<u32>,
}

impl Lookup {
    fn home(id: &FnId) -> usize {
        let b = &id.0.0;
        u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]) as usize
    }

    fn new(fns: &[Version]) -> Result<Lookup, TableError> {
        let size = (fns.len() * 2).next_power_of_two().max(8);
        let mut lookup = Lookup { mask: size - 1, places: vec![NONE; size] };
        for (at, version) in fns.iter().enumerate() {
            let mut place = Lookup::home(&version.id) & lookup.mask;
            loop {
                match lookup.places[place] {
                    NONE => {
                        lookup.places[place] = at as u32;
                        break;
                    }
                    other if fns[other as usize].id == version.id => return Err(TableError::Twice(version.id)),
                    _ => place = (place + 1) & lookup.mask,
                }
            }
        }
        Ok(lookup)
    }

    fn get(&self, fns: &[Version], id: &FnId) -> Option<u32> {
        let mut place = Lookup::home(id) & self.mask;
        loop {
            match self.places[place] {
                NONE => return None,
                at if fns[at as usize].id == *id => return Some(at),
                _ => place = (place + 1) & self.mask,
            }
        }
    }
}

/// A test binary's functions.
pub struct Image {
    /// By the table's index, codegen's order.
    fns: Vec<Version>,
    /// Each slot's function: slot `n` is the jump table's `n`.
    slot_fn: Vec<u32>,
    /// Whether each function has a slot, by table index.
    logged: Vec<bool>,
    unlogged: ContentId,
    lookup: Lookup,
}

pub const UNLOGGED_DOMAIN: &str = "blaze v2 testsel unlogged 1";

impl Image {
    /// The table of a binary: its functions, each slot's function, and `beside`, the content
    /// of the code it runs that is in no table (the premapped dependency image; zero for none).
    pub fn new(fns: Vec<Version>, slot_fn: Vec<u32>, beside: ContentId) -> Result<Image, TableError> {
        let mut logged = vec![false; fns.len()];
        for (slot, &function) in slot_fn.iter().enumerate() {
            let Some(it) = logged.get_mut(function as usize) else {
                return Err(TableError::NoFunction { slot: slot as u32, function });
            };
            if *it {
                return Err(TableError::TwoSlots { function });
            }
            *it = true;
        }
        let lookup = Lookup::new(&fns)?;
        let mut outside: Vec<&Version> = fns.iter().zip(&logged).filter(|it| !*it.1).map(|it| it.0).collect();
        outside.sort_unstable();
        let mut hasher = Hasher::new(UNLOGGED_DOMAIN);
        hasher.id(&beside).update(&(outside.len() as u64).to_le_bytes());
        for version in outside {
            hasher.id(&version.id.0).id(&version.bound.0);
        }
        let unlogged = hasher.finish();
        Ok(Image { fns, slot_fn, logged, unlogged, lookup })
    }

    /// The hash of every function called not through a slot, and of the code in no table.
    pub fn unlogged(&self) -> ContentId {
        self.unlogged
    }

    pub fn slots(&self) -> u32 {
        self.slot_fn.len() as u32
    }

    /// The bound code of the function `id` in this binary; `None` when it has none.
    pub fn bound_of(&self, id: &FnId) -> Option<Bound> {
        self.lookup.get(&self.fns, id).map(|at| self.fns[at as usize].bound)
    }

    /// The function of table index `at`.
    pub fn function(&self, at: u32) -> Option<&Version> {
        self.fns.get(at as usize)
    }

    /// The set a whole record says the test executed.
    pub fn executed(&self, slots: u32, hits: &[u32]) -> Result<Set, TableError> {
        if slots != self.slots() {
            return Err(TableError::OtherBinary { record: slots, table: self.slots() });
        }
        let mut fns = Vec::with_capacity(hits.len());
        for &hit in hits {
            let Some(&function) = self.slot_fn.get(hit as usize) else {
                return Err(TableError::OtherBinary { record: slots, table: self.slots() });
            };
            fns.push(self.fns[function as usize]);
        }
        Ok(Set::of(self.unlogged, fns))
    }

    /// The set of what codegen's walk says a test reaches (`reached`, by table index): its
    /// logged functions; the rest are in [`Image::unlogged`].
    pub fn reached(&self, reached: &[u32]) -> Result<Set, TableError> {
        let mut fns = Vec::with_capacity(reached.len());
        for &at in reached {
            match self.logged.get(at as usize) {
                None => return Err(TableError::NotInTable(at)),
                Some(true) => fns.push(self.fns[at as usize]),
                Some(false) => {}
            }
        }
        Ok(Set::of(self.unlogged, fns))
    }

    /// The set a record keys a pass by: what it executed when whole.
    pub fn of_record(&self, record: &Record) -> Option<Result<Set, TableError>> {
        match record {
            Record::Complete { slots, hits } => Some(self.executed(*slots, hits)),
            Record::Incomplete { .. } => None,
        }
    }

    /// Whether every function of `set` is in this binary as it was: `None` when so, or what
    /// is not.
    pub fn changed(&self, set: &SetView<'_>) -> Option<Change> {
        if set.unlogged() != self.unlogged {
            return Some(Change::Unlogged);
        }
        for version in set.versions() {
            match self.bound_of(&version.id) {
                None => return Some(Change::Gone(version.id)),
                Some(bound) if bound != version.bound => return Some(Change::Changed(version.id)),
                Some(_) => {}
            }
        }
        None
    }
}

/// What of a set is not as it was.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Change {
    /// This function's bound code changed.
    Changed(FnId),
    /// This function is no longer in the binary.
    Gone(FnId),
    /// Code called not through a slot changed.
    Unlogged,
}

/// The versions a pass is kept under, sorted by identity, each once, and the unlogged hash.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Set {
    pub unlogged: ContentId,
    pub fns: Vec<Version>,
}

const SET_MAGIC: &[u8; 16] = b"blaze v2 fnset 1";
const SET_HEAD: usize = 16 + 32 + 8;
pub const SET_DOMAIN: &str = "blaze v2 testsel set 1";

impl Set {
    pub fn of(unlogged: ContentId, mut fns: Vec<Version>) -> Set {
        fns.sort_unstable();
        fns.dedup();
        Set { unlogged, fns }
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(SET_HEAD + 64 * self.fns.len());
        out.extend_from_slice(SET_MAGIC);
        out.extend_from_slice(&self.unlogged.0);
        out.extend_from_slice(&(self.fns.len() as u64).to_le_bytes());
        for version in &self.fns {
            out.extend_from_slice(&version.id.0.0);
            out.extend_from_slice(&version.bound.0.0);
        }
        out
    }

    /// The store's key of an encoded set: its content.
    pub fn key_of(encoded: &[u8]) -> Key {
        Key(ContentId::of(SET_DOMAIN, encoded))
    }
}

/// An encoded [`Set`], read in place.
#[derive(Clone, Copy, Debug)]
pub struct SetView<'a> {
    bytes: &'a [u8],
}

impl<'a> SetView<'a> {
    pub fn decode(bytes: &'a [u8]) -> Result<SetView<'a>, String> {
        if bytes.len() < SET_HEAD || bytes[..16] != *SET_MAGIC {
            return Err("not a set of functions".to_owned());
        }
        let count = u64::from_le_bytes(bytes[48..56].try_into().expect("8 bytes"));
        if (bytes.len() - SET_HEAD) as u64 != count.saturating_mul(64) {
            return Err("a set of functions holds another number of them than it says".to_owned());
        }
        Ok(SetView { bytes })
    }

    pub fn unlogged(&self) -> ContentId {
        ContentId(self.bytes[16..48].try_into().expect("32 bytes"))
    }

    pub fn len(&self) -> usize {
        (self.bytes.len() - SET_HEAD) / 64
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn versions(&self) -> impl Iterator<Item = Version> + 'a {
        let bytes: &'a [u8] = self.bytes;
        bytes[SET_HEAD..].chunks_exact(64).map(|it| Version {
            id: FnId(ContentId(it[..32].try_into().expect("32 bytes"))),
            bound: Bound(ContentId(it[32..].try_into().expect("32 bytes"))),
        })
    }

    pub fn to_set(&self) -> Set {
        Set { unlogged: self.unlogged(), fns: self.versions().collect() }
    }
}

#[cfg(test)]
pub(crate) mod tests;
