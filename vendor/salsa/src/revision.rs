use std::num::NonZeroU32;

use crate::sync::atomic::{AtomicU32, Ordering};

/// Value of the initial revision. We don't use 0 because we want to use a `NonZeroU32`.
const START: u32 = 1;

/// A unique identifier for the current version of the database.
///
/// Each time an input is changed, the revision number is incremented.
/// `Revision` is used internally to track which values may need to be
/// recomputed, but is not something you should have to interact with
/// directly as a user of salsa.
///
/// 4 bytes (docs/v2/plan/lanes/db/speed-of-light.md, "Memo at rest"): 2^32 - 1 revisions is a
/// new revision every millisecond for 49 days, and a daemon that gets there stops with an error
/// naming the limit ([`Revision::next`]) rather than wrapping.
#[derive(Copy, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(transparent)]
pub struct Revision {
    generation: NonZeroU32,
}

const _: () = assert!(size_of::<Revision>() == 4 && size_of::<Option<Revision>>() == 4);

impl Revision {
    #[inline]
    pub const fn max() -> Self {
        Self {
            generation: NonZeroU32::MAX,
        }
    }

    #[inline]
    pub(crate) const fn start() -> Self {
        Self {
            // SAFETY: `START` is non-zero.
            generation: unsafe { NonZeroU32::new_unchecked(START) },
        }
    }

    /// The revision numbered `g`, which is not 0 and fits in 32 bits.
    #[inline]
    pub(crate) const fn from(g: usize) -> Self {
        assert!(g <= u32::MAX as usize, "salsa: a revision past 2^32 - 1");
        Self {
            generation: NonZeroU32::new(g as u32).unwrap(),
        }
    }

    #[inline]
    pub(crate) const fn from_u32(g: u32) -> Option<Self> {
        match NonZeroU32::new(g) {
            Some(generation) => Some(Self { generation }),
            None => None,
        }
    }

    #[inline]
    pub(crate) fn from_opt(g: usize) -> Option<Self> {
        u32::try_from(g).ok().and_then(Self::from_u32)
    }

    #[inline]
    pub(crate) fn next(self) -> Revision {
        let next = self
            .generation
            .checked_add(1)
            .filter(|next| *next != NonZeroU32::MAX)
            .expect("salsa: the database made 2^32 - 2 revisions; a revision is 32 bits");
        Self { generation: next }
    }

    #[inline]
    pub(crate) const fn as_usize(self) -> usize {
        self.generation.get() as usize
    }

    #[inline]
    pub(crate) const fn as_u32(self) -> u32 {
        self.generation.get()
    }
}

#[cfg(feature = "persistence")]
impl serde::Serialize for Revision {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serde::Serialize::serialize(&self.generation, serializer)
    }
}

#[cfg(feature = "persistence")]
impl<'de> serde::Deserialize<'de> for Revision {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        serde::Deserialize::deserialize(deserializer).map(|generation| Self { generation })
    }
}

impl std::fmt::Debug for Revision {
    fn fmt(&self, fmt: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(fmt, "R{}", self.generation)
    }
}

#[derive(Debug)]
pub struct AtomicRevision {
    data: AtomicU32,
}

#[cfg(feature = "persistence")]
impl serde::Serialize for AtomicRevision {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serde::Serialize::serialize(&self.data.load(Ordering::Relaxed), serializer)
    }
}

#[cfg(feature = "persistence")]
impl<'de> serde::Deserialize<'de> for AtomicRevision {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let revision: Revision = serde::Deserialize::deserialize(deserializer)?;
        Ok(Self::new(revision))
    }
}

impl From<Revision> for AtomicRevision {
    fn from(value: Revision) -> Self {
        Self {
            data: AtomicU32::new(value.as_u32()),
        }
    }
}

impl AtomicRevision {
    pub const fn start() -> Self {
        Self {
            data: AtomicU32::new(START),
        }
    }

    pub const fn new(revision: Revision) -> Self {
        Self {
            data: AtomicU32::new(revision.as_u32()),
        }
    }

    #[inline]
    pub fn load(&self) -> Revision {
        Revision {
            // SAFETY: We know that the value is non-zero because we only ever store `START` which 1, or a
            // Revision which is guaranteed to be non-zero.
            generation: unsafe { NonZeroU32::new_unchecked(self.data.load(Ordering::Acquire)) },
        }
    }

    pub fn store(&self, r: Revision) {
        self.data.store(r.as_u32(), Ordering::Release);
    }

    pub fn get_mut(&mut self) -> &mut Revision {
        // SAFETY: `Revision` is repr(transparent) over `NonZeroU32`, we only ever store `NonZeroU32` values, thus making this cast valid.
        unsafe { &mut *(self.data.get_mut() as *mut u32).cast::<Revision>() }
    }
}

impl Clone for AtomicRevision {
    fn clone(&self) -> Self {
        Self {
            data: AtomicU32::new(self.load().as_u32()),
        }
    }
}

#[derive(Debug)]
pub(crate) struct OptionalAtomicRevision {
    data: AtomicU32,
}

#[cfg(feature = "persistence")]
impl serde::Serialize for OptionalAtomicRevision {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serde::Serialize::serialize(&self.data.load(Ordering::Relaxed), serializer)
    }
}

#[cfg(feature = "persistence")]
impl<'de> serde::Deserialize<'de> for OptionalAtomicRevision {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        serde::Deserialize::deserialize(deserializer).map(|data| Self {
            data: AtomicU32::new(data),
        })
    }
}

impl From<Revision> for OptionalAtomicRevision {
    fn from(value: Revision) -> Self {
        Self {
            data: AtomicU32::new(value.as_u32()),
        }
    }
}

impl OptionalAtomicRevision {
    pub(crate) fn new(revision: Option<Revision>) -> Self {
        Self {
            data: AtomicU32::new(revision.map_or(0, |r| r.as_u32())),
        }
    }

    #[inline]
    pub(crate) fn load(&self) -> Option<Revision> {
        Revision::from_u32(self.data.load(Ordering::Acquire))
    }

    pub(crate) fn swap(&self, val: Option<Revision>) -> Option<Revision> {
        Revision::from_u32(
            self.data
                .swap(val.map_or(0, |r| r.as_u32()), Ordering::AcqRel),
        )
    }

    pub(crate) fn compare_exchange(
        &self,
        current: Option<Revision>,
        new: Option<Revision>,
    ) -> Result<Option<Revision>, Option<Revision>> {
        self.data
            .compare_exchange(
                current.map_or(0, |r| r.as_u32()),
                new.map_or(0, |r| r.as_u32()),
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .map(Revision::from_u32)
            .map_err(Revision::from_u32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "persistence")]
    #[test]
    fn atomic_revision_rejects_zero_when_deserializing() {
        assert!(serde_json::from_str::<AtomicRevision>("0").is_err());
    }

    #[test]
    fn optional_atomic_revision() {
        let val = OptionalAtomicRevision::new(Some(Revision::start()));
        assert_eq!(val.load(), Some(Revision::start()));
        assert_eq!(val.swap(None), Some(Revision::start()));
        assert_eq!(val.load(), None);
        assert_eq!(val.swap(Some(Revision::start())), None);
        assert_eq!(val.load(), Some(Revision::start()));
        assert_eq!(
            val.compare_exchange(Some(Revision::start()), None),
            Ok(Some(Revision::start()))
        );
        assert_eq!(
            val.compare_exchange(Some(Revision::start()), None),
            Err(None)
        );
    }

    #[test]
    fn revisions_are_four_bytes_and_order_by_number() {
        assert_eq!(size_of::<AtomicRevision>(), 4);
        assert_eq!(size_of::<OptionalAtomicRevision>(), 4);
        let a = Revision::from(7);
        assert_eq!(a.next(), Revision::from(8));
        assert!(a < a.next());
        assert_eq!(Revision::from(u32::MAX as usize - 1).as_u32(), u32::MAX - 1);
        assert_eq!(Revision::from_opt(0), None);
        assert_eq!(Revision::from_opt(1 << 33), None);
        assert_eq!(Revision::from_u32(9), Some(Revision::from(9)));
    }

    #[test]
    #[should_panic(expected = "a revision is 32 bits")]
    fn the_last_revision_has_no_next() {
        let _ = Revision::from(u32::MAX as usize - 1).next();
    }
}
