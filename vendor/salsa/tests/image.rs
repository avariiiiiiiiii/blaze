#![cfg(all(feature = "inventory", not(feature = "shuttle")))]

//! Interned values in the daemon image (`src/image.rs`): a restored value keeps its `Id`, the
//! values no root reaches are freed and their slots go only to values made later, at a new
//! generation, and every image that cannot be restored as written is refused with nothing
//! restored.

#![forbid(unsafe_code)]

use blaze_format::FormatError;
use blaze_image::{Builder, Image};
use intern::Live;
use salsa::image::{ImageError, mark_root, restore_interned, write_interned};
use salsa::plumbing::{AsId, FromId, ImageCodec};
use salsa::{DatabaseImpl, Id};

#[salsa::interned(image = NAME)]
struct Name<'db> {
    #[returns(ref)]
    text: String,
}

/// A name and the symbol it is spelled with.
#[salsa::interned(image = PATH)]
struct Path<'db> {
    name: Name<'db>,
    symbol: u32,
}

/// A path, one step further from the roots.
#[salsa::interned(image = NESTED)]
struct Nested<'db> {
    path: Path<'db>,
}

/// No codec: the image keeps none of these.
#[salsa::interned]
struct Plain<'db> {
    key: u32,
}

#[salsa::interned(image = WRAPS)]
struct Wraps<'db> {
    plain: Plain<'db>,
}

fn invalid(what: &'static str) -> FormatError {
    FormatError::Invalid { at: 0, what }
}

fn read_id<T: FromId>(bytes: &[u8]) -> Result<T, FormatError> {
    let bits: [u8; 8] = bytes.try_into().map_err(|_| invalid("not an id"))?;
    Ok(T::from_id(Id::from_bits(u64::from_le_bytes(bits))))
}

const NAME: ImageCodec<(String,)> = ImageCodec {
    schema: blaze_format::schema("test Name 1: utf-8"),
    write: |fields, out| out.extend_from_slice(fields.0.as_bytes()),
    read: |bytes| {
        let text = std::str::from_utf8(bytes).map_err(|_| invalid("not utf-8"))?;
        // alloc: the field is a `String`; a test value, once
        Ok((text.to_owned(),))
    },
    names: |_, _, _| {},
    // alloc: an empty `String` allocates nothing
    vacant: || (String::new(),),
};

const PATH: ImageCodec<(Name<'static>, u32)> = ImageCodec {
    schema: blaze_format::schema("test Path 1: name id u64, symbol u32"),
    write: |fields, out| {
        out.extend_from_slice(&fields.0.as_id().as_bits().to_le_bytes());
        out.extend_from_slice(&fields.1.to_le_bytes());
    },
    read: |bytes| {
        let (name, symbol) = bytes.split_at_checked(8).ok_or(invalid("short"))?;
        let symbol: [u8; 4] = symbol.try_into().map_err(|_| invalid("not a symbol"))?;
        Ok((read_id(name)?, u32::from_le_bytes(symbol)))
    },
    names: |fields, ids, symbols| {
        ids.push(fields.0.as_id());
        symbols.push(fields.1);
    },
    vacant: || (Name::from_id(Id::from_bits(1)), 0),
};

const NESTED: ImageCodec<(Path<'static>,)> = ImageCodec {
    schema: blaze_format::schema("test Nested 1: path id u64"),
    write: |fields, out| out.extend_from_slice(&fields.0.as_id().as_bits().to_le_bytes()),
    read: |bytes| Ok((read_id(bytes)?,)),
    names: |fields, ids, _| ids.push(fields.0.as_id()),
    vacant: || (Path::from_id(Id::from_bits(1)),),
};

const WRAPS: ImageCodec<(Plain<'static>,)> = ImageCodec {
    schema: blaze_format::schema("test Wraps 1: plain id u64"),
    write: |fields, out| out.extend_from_slice(&fields.0.as_id().as_bits().to_le_bytes()),
    read: |bytes| Ok((read_id(bytes)?,)),
    names: |fields, ids, _| ids.push(fields.0.as_id()),
    vacant: || (Plain::from_id(Id::from_bits(1)),),
};

/// The image `db` writes with `roots` marked, and the symbols it marked.
fn write(db: &mut DatabaseImpl, roots: &[Id]) -> Result<(Vec<u8>, Live), ImageError> {
    let mut live = Live::new();
    for &root in roots {
        mark_root(&mut live, root);
    }
    let mut symbols = Live::new();
    let mut builder = Builder::new();
    write_interned(db, &live, &mut symbols, &mut builder)?;
    Ok((builder.finish().expect("a small image"), symbols))
}

fn restore(bytes: Vec<u8>) -> Result<DatabaseImpl, ImageError> {
    let image = Image::open(bytes).expect("the image opens");
    let mut db = DatabaseImpl::new();
    restore_interned(&mut db, &image)?;
    Ok(db)
}

#[test]
fn a_restored_value_keeps_its_id_and_its_fields() {
    let mut db = DatabaseImpl::new();
    let (a, b) = (Name::new(&db, "a"), Name::new(&db, "b"));
    let path = Path::new(&db, b, 7);
    let nested = Nested::new(&db, path);
    let ids = (a.as_id(), b.as_id(), path.as_id(), nested.as_id());
    let (bytes, symbols) = write(&mut db, &[ids.0, ids.3]).unwrap();
    assert!(symbols.contains(7) && symbols.count() == 1, "the kept path's symbol is marked");

    let db = restore(bytes).unwrap();
    let nested = Nested::from_id(ids.3);
    let path = nested.path(&db);
    assert_eq!(path.as_id(), ids.2, "a value a root names is kept at its id");
    assert_eq!(path.symbol(&db), 7);
    assert_eq!(path.name(&db).as_id(), ids.1);
    assert_eq!(path.name(&db).text(&db), "b");
    assert_eq!(Name::new(&db, "a").as_id(), ids.0, "interning a kept value finds it");
    assert_eq!(Nested::new(&db, path).as_id(), ids.3);
}

#[test]
fn a_value_no_root_reaches_is_freed_and_its_slot_goes_to_a_later_generation() {
    let mut db = DatabaseImpl::new();
    let kept = Name::new(&db, "kept").as_id();
    let freed = Name::new(&db, "freed").as_id();
    let (bytes, _) = write(&mut db, &[kept]).unwrap();

    let db = restore(bytes).unwrap();
    let again = Name::new(&db, "freed").as_id();
    assert_ne!(again.index(), freed.index(), "a freed value comes back in a new slot");
    assert_ne!(again.index(), kept.index());
    let vacant = Name::new(&db, "").as_id();
    assert_eq!(vacant.index(), freed.index(), "the freed slot is listed for reuse");
    assert_eq!(vacant.generation(), freed.generation() + 1, "at the next generation");
}

#[test]
fn a_kept_value_naming_one_the_image_does_not_keep_is_refused() {
    let mut db = DatabaseImpl::new();
    let wraps = Wraps::new(&db, Plain::new(&db, 3)).as_id();
    let error = write(&mut db, &[wraps]).err();
    assert!(matches!(error, Some(ImageError::Unkept { .. })), "{error:?}");
}

#[test]
fn a_restore_after_interning_is_refused_and_restores_nothing() {
    let mut db = DatabaseImpl::new();
    let name = Name::new(&db, "a").as_id();
    let (bytes, _) = write(&mut db, &[name]).unwrap();

    let image = Image::open(bytes).unwrap();
    let mut started = DatabaseImpl::new();
    let other = Name::new(&started, "other").as_id();
    let error = restore_interned(&mut started, &image).err();
    assert!(matches!(error, Some(ImageError::Started)), "{error:?}");
    assert_eq!(Name::new(&started, "other").as_id(), other);
}

#[test]
fn an_image_whose_fields_do_not_read_back_is_refused_with_nothing_restored() {
    let mut db = DatabaseImpl::new();
    let name = Name::new(&db, "\u{e9}").as_id();
    let (mut bytes, _) = write(&mut db, &[name]).unwrap();
    // The image's own checksums would refuse a flipped byte; this one is rewritten with them.
    // clone: the test rebuilds the image from these bytes below
    let image = Image::open(bytes.clone()).unwrap();
    let section = image.section(salsa::image::SECTION, salsa::image::LAYOUT).unwrap();
    let whole = image.whole(&section).unwrap();
    let at = whole.windows(2).position(|pair| pair == "\u{e9}".as_bytes()).unwrap();
    // alloc: a test's copy of the section, once
    let mut changed = whole.to_vec();
    changed[at] = 0xff;
    let mut builder = Builder::new();
    builder.add(salsa::image::SECTION, salsa::image::LAYOUT, &changed);
    bytes = builder.finish().unwrap();

    let image = Image::open(bytes).unwrap();
    let mut fresh = DatabaseImpl::new();
    let error = restore_interned(&mut fresh, &image).err();
    assert!(matches!(error, Some(ImageError::Fields { .. })), "{error:?}");
    let again = Name::new(&fresh, "\u{e9}").as_id();
    assert_eq!(again.index(), 0, "nothing was restored, so the table starts at slot 0");
}
