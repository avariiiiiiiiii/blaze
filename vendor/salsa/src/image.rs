//! Interned values in the daemon image, at the IDs they had (docs/v2/plan/design/restore.md, "IDs
//! are stable across restarts"; v2/requests/db.md "frontend-image" section 6): the rules of
//! `intern::Mapped`, for salsa's tables.
//!
//! An interned struct opts in with `#[salsa::interned(image = CODEC)]`, `CODEC` an
//! [`ImageCodec`] of its fields. [`write_interned`] keeps, of those structs' values, the ones the
//! other sections still name (the roots, marked by their [`Id::index`] in an `intern::Live`) and
//! every value a kept one names, transitively; it marks the symbols they name in the symbol
//! table's `Live`. [`restore_interned`] puts every kept value back at its `Id`, generation
//! included, before anything is interned. Nothing is renumbered:
//!
//! - A page of a kept struct is written whole, so each slot comes back where it was. A slot whose
//!   value was not kept is written *vacant*: its generation one higher, its fields the codec's
//!   placeholder, listed stale so that only a value made later reuses it, under a new `Id`. A
//!   slot at its last generation stays empty for good.
//! - Every other page (inputs, tracked structs, structs not kept) comes back as a placeholder
//!   that nothing allocates in, so no `Id` of the old process is given to another value.
//!
//! Restoring decodes every value before it changes anything, so on any error the database is as
//! it was: empty, and so must be every section whose values name these IDs. A restored value is
//! decoded once, at restore, into its slot: salsa's slots own their fields.

use std::any::Any;
use std::fmt;

use blaze_format::{FormatError, SchemaHash};
use blaze_ids::bits::BitSet;
use blaze_image::{Builder, Corrupt, Image, Refused, SectionId, Stable};
use intern::Live;

use crate::durability::Durability;
use crate::table::{PageIndex, split_id};
use crate::zalsa::{IngredientIndex, Zalsa, ZalsaDatabase};
use crate::{Database, Id};

/// How the image keeps the fields `F` of an interned struct.
pub struct ImageCodec<F: 'static> {
    /// The schema of the bytes [`ImageCodec::write`] makes: a change is a refused image.
    pub schema: SchemaHash,
    /// Appends the fields' bytes. Symbols are written as their IDs, salsa IDs as their bits.
    pub write: fn(&F, &mut Vec<u8>),
    /// The fields of bytes `write` made, in an image whose symbol table this process restored.
    pub read: fn(&[u8]) -> Result<F, FormatError>,
    /// Pushes every salsa `Id` and every symbol ID the fields name, for the image's GC.
    pub names: fn(&F, &mut Vec<Id>, &mut Vec<u32>),
    /// Fields for a vacant slot: never read as a value, only replaced when the slot is reused.
    pub vacant: fn() -> F,
}

/// Where the image keeps salsa's interned values.
pub const SECTION: SectionId = SectionId::SALSA_INTERNED;

/// The section's layout, all little-endian:
const LAYOUT: SchemaHash = blaze_format::schema(
    "blaze v2 salsa interned 1: \
     header { magic u64, pages u32, ingredients u32, values u32, pad u32, blob u64 }; \
     pages [u32; pages] (the owning ingredient, or u32::MAX for a placeholder); \
     ingredients [{ index u32, first u32, values u32, name u32, schema [u8; 32], name_at u64 }]; \
     values [{ id u64, at u64, len u32, durability u8, kind u8, pad u16 }] grouped by \
     ingredient, each ingredient's in page and slot order; blob (names, then fields)",
);

const MAGIC: u64 = u64::from_le_bytes(*b"salsaint");
const HEADER: usize = 32;
const PAGE: usize = 4;
const INGREDIENT: usize = 56;
const VALUE: usize = 24;
const NO_OWNER: u32 = u32::MAX;

/// What a slot of a kept page holds.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub(crate) enum SlotKind {
    /// A kept value, at its `Id`.
    Live = 0,
    /// A freed value's slot, at the next generation, for a value made later.
    Vacant = 1,
    /// A freed value's slot at its last generation: never used again.
    Spent = 2,
}

/// One slot of a kept page, as the section holds it.
#[derive(Copy, Clone, Debug)]
pub(crate) struct SlotRecord {
    pub(crate) id: Id,
    pub(crate) durability: Durability,
    pub(crate) kind: SlotKind,
    /// The fields' bytes, in the blob; empty unless `Live`.
    pub(crate) bytes: (usize, usize),
}

/// A kept ingredient's slots, as [`crate::ingredient::Ingredient::write_image_slots`] appends them.
pub(crate) struct SlotsOut<'a> {
    pub(crate) slots: &'a mut Vec<SlotRecord>,
    pub(crate) blob: &'a mut Vec<u8>,
}

impl SlotsOut<'_> {
    /// Appends a kept value with the bytes `write` appends.
    pub(crate) fn live(&mut self, id: Id, durability: Durability, write: impl FnOnce(&mut Vec<u8>)) {
        let at = self.blob.len();
        write(self.blob);
        self.slots.push(SlotRecord {
            id,
            durability,
            kind: SlotKind::Live,
            bytes: (at, self.blob.len() - at),
        });
    }

    /// Appends the slot of a value the GC freed.
    pub(crate) fn freed(&mut self, id: Id) {
        let (id, kind) = match id.next_generation() {
            Some(next) => (next, SlotKind::Vacant),
            None => (id, SlotKind::Spent),
        };
        self.slots.push(SlotRecord {
            id,
            durability: Durability::LOW,
            kind,
            bytes: (0, 0),
        });
    }
}

/// Why the interned values were not written or restored.
#[derive(Debug)]
pub enum ImageError {
    Refused(Refused),
    Corrupt(Corrupt),
    /// The section's bytes are not a table of this layout.
    Invalid(&'static str),
    /// A kept struct's bytes did not read back as its fields.
    Fields {
        ingredient: &'static str,
        error: FormatError,
    },
    /// The image keeps a struct this process does not have at that index, or keeps under
    /// another schema: another build of the front end wrote it.
    Mismatch { index: u32, name: String },
    /// Values were interned before the restore, under IDs the image may hold.
    Started,
    /// A kept value names a salsa value that is not kept (an input, a tracked struct, or an
    /// interned struct with no codec): the next process could not read it.
    Unkept {
        value: &'static str,
        names: &'static str,
    },
    /// A kept value names an `Id` whose slot now holds another value.
    Stale { value: &'static str, id: Id },
}

impl fmt::Display for ImageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ImageError::Refused(refused) => write!(f, "salsa's interned values: {refused}"),
            ImageError::Corrupt(corrupt) => write!(f, "salsa's interned values: {corrupt}"),
            ImageError::Invalid(what) => write!(f, "salsa's interned values: {what}"),
            ImageError::Fields { ingredient, error } => {
                write!(f, "a kept `{ingredient}` does not read back: {error}")
            }
            ImageError::Mismatch { index, name } => write!(
                f,
                "the image keeps `{name}` as ingredient {index}, which this build has not"
            ),
            ImageError::Started => {
                write!(f, "values were interned before salsa's interned values were restored")
            }
            ImageError::Unkept { value, names } => write!(
                f,
                "a kept `{value}` names a `{names}`, which the image does not keep"
            ),
            ImageError::Stale { value, id } => {
                write!(f, "a kept `{value}` names {id:?}, whose slot was reused")
            }
        }
    }
}

impl std::error::Error for ImageError {}

impl From<Corrupt> for ImageError {
    fn from(corrupt: Corrupt) -> ImageError {
        ImageError::Corrupt(corrupt)
    }
}

impl From<Refused> for ImageError {
    fn from(refused: Refused) -> ImageError {
        ImageError::Refused(refused)
    }
}

/// Marks `id` a root of [`write_interned`]: a value another section names.
#[inline]
pub fn mark_root(roots: &mut Live, id: Id) {
    roots.mark_id(id.index());
}

/// Writes the interned values of every struct with an [`ImageCodec`] that `roots` reaches into
/// `builder`, marking the symbols they name in `symbols`. Takes the database's storage
/// exclusively, as a write does.
pub fn write_interned<Db: Database>(
    db: &mut Db,
    roots: &Live,
    symbols: &mut Live,
    builder: &mut Builder,
) -> Result<(), ImageError> {
    let zalsa: &Zalsa = db.zalsa_mut();
    let bytes = write_section(zalsa, roots, symbols)?;
    builder.add(SECTION, LAYOUT, &bytes);
    Ok(())
}

/// Puts the interned values `image` keeps back at their IDs, before anything is interned. On an
/// error nothing was restored.
pub fn restore_interned<Db: Database, S: Stable>(
    db: &mut Db,
    image: &Image<S>,
) -> Result<(), ImageError> {
    let section = image.section(SECTION, LAYOUT)?;
    let bytes = image.whole(&section)?;
    restore_section(db.zalsa_mut(), bytes)
}

fn write_section(zalsa: &Zalsa, roots: &Live, symbols: &mut Live) -> Result<Vec<u8>, ImageError> {
    let table = zalsa.table();
    let kept: Vec<Option<SchemaHash>> = zalsa
        .ingredients()
        .map(|ingredient| ingredient.image_schema())
        // alloc: the edge: once per image written
        .collect();
    let is_kept = |index: IngredientIndex| kept[index.as_u32() as usize].is_some();

    let slots = table.page_count() << crate::table::PAGE_LEN_BITS;
    let mut marked: BitSet<u32> = BitSet::new(slots);
    // alloc: the edge: once per image written
    let mut work: Vec<Id> = Vec::new();
    for (index, _) in kept.iter().enumerate().filter(|(_, schema)| schema.is_some()) {
        let ingredient = zalsa.lookup_ingredient(IngredientIndex::new(index as u32));
        ingredient.image_ids(zalsa, &mut |id| {
            if roots.contains(id.index()) && marked.insert(id.index()) {
                work.push(id);
            }
        });
    }
    // alloc: the edge: once per image written
    let (mut named, mut names) = (Vec::new(), Vec::new());
    while let Some(id) = work.pop() {
        let owner = zalsa.lookup_ingredient(table.ingredient_index(id));
        named.clear();
        names.clear();
        // SAFETY: `write_interned` holds the storage exclusively, so no slot changes.
        unsafe { owner.image_names(zalsa, id, &mut named, &mut names) };
        for &symbol in &names {
            symbols.mark_id(symbol);
        }
        for &other in &named {
            let index = table.ingredient_index(other);
            let target = zalsa.lookup_ingredient(index);
            if !is_kept(index) {
                return Err(ImageError::Unkept {
                    value: owner.debug_name(),
                    names: target.debug_name(),
                });
            }
            if !target.image_holds(zalsa, other) {
                return Err(ImageError::Stale {
                    value: owner.debug_name(),
                    id: other,
                });
            }
            if marked.insert(other.index()) {
                work.push(other);
            }
        }
    }

    let pages: Vec<u32> = (0..table.page_count())
        .map(|page| match table.page_owner(page) {
            Some(owner) if is_kept(owner) => owner.as_u32(),
            _ => NO_OWNER,
        })
        // alloc: the edge: once per image written
        .collect();
    // alloc: the edge: once per image written
    let (mut records, mut blob) = (Vec::new(), Vec::new());
    // alloc: the edge: once per image written
    let mut directory = Vec::new();
    for (index, schema) in kept.iter().enumerate() {
        let Some(schema) = schema else { continue };
        let ingredient = zalsa.lookup_ingredient(IngredientIndex::new(index as u32));
        let name = ingredient.debug_name();
        let name_at = blob.len();
        blob.extend_from_slice(name.as_bytes());
        let first = records.len();
        // SAFETY: `write_interned` holds the storage exclusively, so no slot changes.
        unsafe {
            ingredient.write_image_slots(
                zalsa,
                &marked,
                &mut SlotsOut {
                    slots: &mut records,
                    blob: &mut blob,
                },
            )
        };
        directory.push((index as u32, first, records.len() - first, name.len(), *schema, name_at));
    }

    let count = |n: usize, what: &'static str| u32::try_from(n).map_err(|_| ImageError::Invalid(what));
    let mut out = Vec::with_capacity(
        HEADER + pages.len() * PAGE + directory.len() * INGREDIENT + records.len() * VALUE + blob.len(),
    );
    out.extend_from_slice(&MAGIC.to_le_bytes());
    out.extend_from_slice(&count(pages.len(), "more than 2^32 pages")?.to_le_bytes());
    out.extend_from_slice(&count(directory.len(), "more than 2^32 ingredients")?.to_le_bytes());
    out.extend_from_slice(&count(records.len(), "more than 2^32 values")?.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&(blob.len() as u64).to_le_bytes());
    for owner in &pages {
        out.extend_from_slice(&owner.to_le_bytes());
    }
    for &(index, first, values, name_len, schema, name_at) in &directory {
        out.extend_from_slice(&index.to_le_bytes());
        out.extend_from_slice(&(first as u32).to_le_bytes());
        out.extend_from_slice(&(values as u32).to_le_bytes());
        out.extend_from_slice(&count(name_len, "a name past 4 GB")?.to_le_bytes());
        out.extend_from_slice(&schema.0.0);
        out.extend_from_slice(&(name_at as u64).to_le_bytes());
    }
    for record in &records {
        out.extend_from_slice(&record.id.as_bits().to_le_bytes());
        out.extend_from_slice(&(record.bytes.0 as u64).to_le_bytes());
        out.extend_from_slice(&count(record.bytes.1, "a value past 4 GB")?.to_le_bytes());
        out.push(record.durability.index() as u8);
        out.push(record.kind as u8);
        out.extend_from_slice(&[0; 2]);
    }
    out.extend_from_slice(&blob);
    Ok(out)
}

/// Bounds-checked little-endian reads of the section.
struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn bytes(&self, at: usize, len: usize) -> Result<&'a [u8], ImageError> {
        at.checked_add(len)
            .and_then(|end| self.0.get(at..end))
            .ok_or(ImageError::Invalid("a record past the section's end"))
    }

    fn u32(&self, at: usize) -> Result<u32, ImageError> {
        Ok(u32::from_le_bytes(self.bytes(at, 4)?.try_into().unwrap_or_default()))
    }

    fn u64(&self, at: usize) -> Result<u64, ImageError> {
        Ok(u64::from_le_bytes(self.bytes(at, 8)?.try_into().unwrap_or_default()))
    }
}

fn restore_section(zalsa: &mut Zalsa, bytes: &[u8]) -> Result<(), ImageError> {
    if zalsa.table().page_count() != 0 {
        return Err(ImageError::Started);
    }
    let read = Reader(bytes);
    if read.u64(0)? != MAGIC {
        return Err(ImageError::Invalid("not salsa's interned values"));
    }
    let (pages, ingredients, values) = (
        read.u32(8)? as usize,
        read.u32(12)? as usize,
        read.u32(16)? as usize,
    );
    let blob_len = read.u64(24)? as usize;
    let pages_at = HEADER;
    let directory_at = pages_at + pages * PAGE;
    let values_at = directory_at + ingredients * INGREDIENT;
    let blob_at = values_at + values * VALUE;
    let blob = read.bytes(blob_at, blob_len)?;
    if blob_at + blob_len != bytes.len() {
        return Err(ImageError::Invalid("bytes past the blob"));
    }
    if pages > crate::table::MAX_PAGES {
        return Err(ImageError::Invalid("more pages than a table holds"));
    }

    let owners: Vec<u32> = (0..pages)
        .map(|page| read.u32(pages_at + page * PAGE))
        // alloc: the edge: once per restore
        .collect::<Result<_, _>>()?;
    // alloc: the edge: once per restore
    let mut decoded: Vec<(IngredientIndex, Box<dyn Any>)> = Vec::with_capacity(ingredients);
    let mut next = 0;
    let mut previous: Option<u32> = None;
    for entry in 0..ingredients {
        let at = directory_at + entry * INGREDIENT;
        let index = read.u32(at)?;
        let (first, count) = (read.u32(at + 4)? as usize, read.u32(at + 8)? as usize);
        let name_len = read.u32(at + 12)? as usize;
        let schema = SchemaHash(blaze_ids::ContentId(
            read.bytes(at + 16, 32)?.try_into().unwrap_or_default(),
        ));
        let name_at = read.u64(at + 48)? as usize;
        let name = std::str::from_utf8(
            blob.get(name_at..name_at.saturating_add(name_len))
                .ok_or(ImageError::Invalid("a name past the blob"))?,
        )
        .map_err(|_| ImageError::Invalid("a name that is not UTF-8"))?;
        if previous.is_some_and(|previous| previous >= index) || first != next {
            return Err(ImageError::Invalid("ingredients out of order"));
        }
        previous = Some(index);
        next = first + count;
        if next > values {
            return Err(ImageError::Invalid("an ingredient's values past the table"));
        }
        let mismatch = || ImageError::Mismatch {
            index,
            // alloc: the edge: an error, once
            name: name.to_owned(),
        };
        let ingredient_index = IngredientIndex::new(index);
        let Some(ingredient) = zalsa.ingredients().nth(index as usize) else {
            return Err(mismatch());
        };
        if ingredient.debug_name() != name || ingredient.image_schema() != Some(schema) {
            return Err(mismatch());
        }

        // alloc: the edge: once per restore
        let mut slots = Vec::with_capacity(count);
        let mut expected: Option<(usize, usize)> = None;
        for value in first..next {
            let at = values_at + value * VALUE;
            let bits = read.u64(at)?;
            if bits as u32 == 0 {
                return Err(ImageError::Invalid("an `Id` of index 0"));
            }
            let id = Id::from_bits(bits);
            let (page, slot) = split_id(id);
            let (page, slot) = (page.as_usize(), slot.as_usize());
            if owners.get(page) != Some(&index) {
                return Err(ImageError::Invalid("a value on a page its ingredient does not own"));
            }
            // Each kept page whole, from slot 0, pages ascending.
            let fits = match expected {
                None => slot == 0,
                Some((last_page, last_slot)) => {
                    (page == last_page && slot == last_slot + 1) || (page > last_page && slot == 0)
                }
            };
            if !fits {
                return Err(ImageError::Invalid("a kept page with a slot missing"));
            }
            expected = Some((page, slot));
            let durability = match bytes[at + 20] {
                level @ 0..=3 => Durability::from_index(level),
                _ => return Err(ImageError::Invalid("a durability past `NEVER_CHANGE`")),
            };
            let kind = match bytes[at + 21] {
                0 => SlotKind::Live,
                1 => SlotKind::Vacant,
                2 => SlotKind::Spent,
                _ => return Err(ImageError::Invalid("a slot of no known kind")),
            };
            let field_bytes = (read.u64(at + 8)? as usize, read.u32(at + 16)? as usize);
            blob.get(field_bytes.0..field_bytes.0.saturating_add(field_bytes.1))
                .ok_or(ImageError::Invalid("fields past the blob"))?;
            slots.push(SlotRecord {
                id,
                durability,
                kind,
                bytes: field_bytes,
            });
        }
        let values = ingredient.decode_image_slots(&slots, blob)?;
        decoded.push((ingredient_index, values));
    }
    if next != values {
        return Err(ImageError::Invalid("values no ingredient holds"));
    }
    if owners
        .iter()
        .any(|&owner| owner != NO_OWNER && !decoded.iter().any(|(index, _)| index.as_u32() == owner))
    {
        return Err(ImageError::Invalid("a page of an ingredient the image does not keep"));
    }

    // Nothing failed: from here the restore only applies what it decoded.
    for (page, &owner) in owners.iter().enumerate() {
        if owner == NO_OWNER {
            zalsa.table_mut().push_uninit_page();
        } else {
            let (ingredient, table) = zalsa.ingredient_and_table_mut(IngredientIndex::new(owner));
            ingredient.force_image_page(table, PageIndex::new(page));
        }
    }
    for (index, values) in decoded {
        let (ingredient, table) = zalsa.ingredient_and_table_mut(index);
        ingredient.apply_image_slots(table, values);
    }
    Ok(())
}
