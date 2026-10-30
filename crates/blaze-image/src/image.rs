//! A mapped image, read in place: [`Image::open`] reads only the spine, and each group is checked
//! by its CRC32C the first time a read reaches it.

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

use blaze_format::{Record, SchemaHash};
use blaze_ids::ContentId;

use crate::{
    Cause, Corrupt, OpenError, Refused, Section, SectionId,
    crc::crc32c,
    raw,
    spine::{self, Spine},
};

/// Bytes at one address for as long as the value lives: an mmap of the image, or the bytes of a
/// test. [`Image`] reads them through a pointer taken once at open.
///
/// # Safety
///
/// Every call of [`Stable::bytes`] on a value returns the same slice (address and length), and
/// its bytes are never written while the value lives.
pub unsafe trait Stable: Send + Sync + 'static {
    fn bytes(&self) -> &[u8];
}

// SAFETY: a `Vec` owned by the image is never pushed to or written, so its buffer stays put.
unsafe impl Stable for Vec<u8> {
    fn bytes(&self) -> &[u8] {
        self
    }
}

// SAFETY: a boxed slice never moves or changes length; it is not written through `&self`.
unsafe impl Stable for Box<[u8]> {
    fn bytes(&self) -> &[u8] {
        self
    }
}

// SAFETY: a `'static` shared slice is one slice forever, and shared, so never written.
unsafe impl Stable for &'static [u8] {
    fn bytes(&self) -> &[u8] {
        self
    }
}

// SAFETY: `Aligned` owns its blocks in a `Vec` it never changes after it is made.
unsafe impl Stable for blaze_format::Aligned {
    fn bytes(&self) -> &[u8] {
        self
    }
}

/// An image, its spine checked. `Sync`: every read takes `&self`, and the only writes are each
/// group's bits, set once.
pub struct Image<S: Stable> {
    owner: S,
    spine: Spine,
    /// Bit `g` of the words: data group `g` matched its CRC. Then bit `groups + t`: CRC table
    /// group `t` matched its top CRC.
    checked: Box<[AtomicU64]>,
    /// The same bits for a group that did not match: read again, it fails again, with no CRC.
    bad: Box<[AtomicU64]>,
    pub(crate) touched: Touched,
}

/// What reads of an image's bytes reached, for the tests that count them; nothing outside tests.
#[derive(Default)]
pub(crate) struct Touched {
    #[cfg(test)]
    pub(crate) bytes: AtomicU64,
    #[cfg(test)]
    pub(crate) end: AtomicU64,
}

impl Touched {
    #[inline(always)]
    fn note(&self, at: u64, len: u64) {
        #[cfg(test)]
        {
            self.bytes.fetch_add(len, Relaxed);
            self.end.fetch_max(at + len, Relaxed);
        }
        let _ = (at, len);
    }
}

fn words(bits: u64) -> Box<[AtomicU64]> {
    (0..bits.div_ceil(64)).map(|_| AtomicU64::new(0)).collect()
}

/// The word and bit of bit `index`.
fn bit(index: u64) -> (usize, u64) {
    ((index / 64) as usize, 1 << (index % 64))
}

impl<S: Stable> Image<S> {
    /// The image held by `owner`, after reading its spine and nothing else: the header, the
    /// section table and the CRC table's CRCs, checked against the spine's hash.
    pub fn open(owner: S) -> Result<Image<S>, OpenError> {
        let touched = Touched::default();
        let bytes = owner.bytes();
        let spine = spine::parse(bytes.len() as u64, |prefix| {
            touched.note(0, prefix as u64);
            &bytes[..prefix]
        })?;
        let bits = u64::from(spine.groups) + spine.tops.len() as u64;
        Ok(Image { owner, spine, checked: words(bits), bad: words(bits), touched })
    }

    /// The image's hash: the BLAKE3 of its spine, which holds every group's CRC.
    pub fn hash(&self) -> ContentId {
        self.spine.hash
    }

    pub fn len(&self) -> u64 {
        self.spine.len
    }

    pub fn group_len(&self) -> u64 {
        self.spine.group_len()
    }

    pub fn sections(&self) -> &[Section] {
        &self.spine.sections
    }

    /// Section `id`, if the image has it and it is of `schema`.
    pub fn section(&self, id: SectionId, schema: SchemaHash) -> Result<Section, Refused> {
        let section = self.spine.sections.iter().find(|it| it.id == id).ok_or(Refused::Missing { id })?;
        match section.schema == schema {
            true => Ok(*section),
            false => Err(Refused::Schema { id, found: section.schema, wanted: schema }),
        }
    }

    /// `len` bytes of the file at `at`, which the spine says are in it.
    fn span(&self, at: u64, len: u64) -> &[u8] {
        #[cfg(test)]
        self.touched.note(at, len);
        &self.owner.bytes()[at as usize..(at + len) as usize]
    }

    /// Group `group` of `section`, checked: its bytes up to the section's end.
    pub fn group(&self, section: &Section, group: u32) -> Result<&[u8], Corrupt> {
        if group >= section.groups {
            let at = u64::from(group) * self.group_len();
            return Err(Corrupt { section: section.id, group, cause: Cause::Bounds { at, len: 1 } });
        }
        self.ensure(section, group)?;
        let at = u64::from(group) * self.group_len();
        let len = (section.len - at).min(self.group_len());
        Ok(self.span(section.at + at, len))
    }

    /// `len` bytes at `at` of `section`, each group they fall in checked.
    #[inline]
    pub fn range(&self, section: &Section, at: u64, len: u64) -> Result<&[u8], Corrupt> {
        let shift = self.spine.shift;
        let end = at.checked_add(len).filter(|&end| end <= section.len);
        let Some(end) = end else {
            let group = (at >> shift).min(u64::from(u32::MAX)) as u32;
            return Err(Corrupt { section: section.id, group, cause: Cause::Bounds { at, len } });
        };
        if len == 0 {
            return Ok(&[]);
        }
        let (first, last) = ((at >> shift) as u32, ((end - 1) >> shift) as u32);
        for group in first..=last {
            self.ensure(section, group)?;
        }
        Ok(self.span(section.at + at, len))
    }

    /// `count` `T`s at `at` of `section`, read in place: `T` a plain record, `at` aligned for it.
    #[inline]
    pub fn slice<T: Record>(&self, section: &Section, at: u64, count: u64) -> Result<&[T], Corrupt> {
        let bytes = count.checked_mul(size_of::<T>() as u64).unwrap_or(u64::MAX);
        let bytes = self.range(section, at, bytes)?;
        raw::cast::<T>(bytes).ok_or(Corrupt {
            section: section.id,
            group: (at >> self.spine.shift) as u32,
            cause: Cause::Misaligned { at, align: align_of::<T>() as u32 },
        })
    }

    /// The `T` at `at` of `section`.
    #[inline]
    pub fn value<T: Record>(&self, section: &Section, at: u64) -> Result<&T, Corrupt> {
        Ok(&self.slice::<T>(section, at, 1)?[0])
    }

    /// All of `section`, every group checked: for a section its owner reads whole at start.
    pub fn whole(&self, section: &Section) -> Result<&[u8], Corrupt> {
        self.range(section, 0, section.len)
    }

    /// Every group of every section checked now: each one that fails, in order.
    pub fn verify(&self) -> Vec<Corrupt> {
        let mut failed = Vec::new();
        for section in &self.spine.sections {
            for group in 0..section.groups {
                if let Err(corrupt) = self.ensure(section, group) {
                    failed.push(corrupt);
                }
            }
        }
        failed
    }

    /// Whether group `group` of `section` was read and matched its CRC.
    pub fn is_checked(&self, section: &Section, group: u32) -> bool {
        let (word, bit) = bit(u64::from(section.first + group));
        self.checked[word].load(Relaxed) & bit != 0
    }

    /// Group `group` of `section` checked, once.
    #[inline]
    fn ensure(&self, section: &Section, group: u32) -> Result<(), Corrupt> {
        // Relaxed: the image's bytes are never written after it is opened, so a set bit orders
        // nothing; a thread that misses one checks the group again and sets it again.
        let (word, bit) = bit(u64::from(section.first + group));
        if self.checked[word].load(Relaxed) & bit != 0 {
            return Ok(());
        }
        self.check(section, group)
    }

    #[cold]
    #[inline(never)]
    fn check(&self, section: &Section, group: u32) -> Result<(), Corrupt> {
        let global = section.first + group;
        let corrupt = |cause| Corrupt { section: section.id, group, cause };
        let wanted = self.table_entry(global).map_err(corrupt)?;
        let at = self.spine.group_len() * (1 + u64::from(global));
        let found = crc32c(self.span(at, self.spine.group_len()));
        let (word, bit) = bit(u64::from(global));
        if found != wanted {
            self.bad[word].fetch_or(bit, Relaxed);
            return Err(corrupt(Cause::Crc { found, wanted }));
        }
        self.checked[word].fetch_or(bit, Relaxed);
        Ok(())
    }

    /// The CRC the table holds for data group `global`, its table group checked first.
    fn table_entry(&self, global: u32) -> Result<u32, Cause> {
        let group = self.spine.group_len();
        let entry_at = u64::from(global) * 4;
        let table_group = (entry_at / group) as u32;
        let index = u64::from(self.spine.groups) + u64::from(table_group);
        let (word, bit) = bit(index);
        if self.checked[word].load(Relaxed) & bit == 0 {
            let wanted = self.spine.tops[table_group as usize];
            let found = crc32c(self.span(self.spine.crc_at + u64::from(table_group) * group, group));
            if found != wanted {
                self.bad[word].fetch_or(bit, Relaxed);
                return Err(Cause::CrcTable { table_group, found, wanted });
            }
            self.checked[word].fetch_or(bit, Relaxed);
        }
        let bytes = self.span(self.spine.crc_at + entry_at, 4);
        Ok(u32::from_le_bytes(bytes.try_into().expect("4 bytes")))
    }

    /// Whether a read of group `group` of `section` failed before.
    pub fn is_bad(&self, section: &Section, group: u32) -> bool {
        let (word, bit) = bit(u64::from(section.first + group));
        self.bad[word].load(Relaxed) & bit != 0
    }

    /// The bytes the image is held in, for an owner that hands them on (the database keeps
    /// its mapping for values it reads later).
    pub fn owner(&self) -> &S {
        &self.owner
    }
}
