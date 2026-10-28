//! docs/support.md listed `type_id` of a type with lifetimes in a constant as a gap: a
//! `TypeId` does not tell lifetimes apart, so the one a constant holds is the one code makes
//! of the same type, whatever its lifetimes are named.

use std::any::TypeId;

struct Borrowed<'a>(#[allow(dead_code)] &'a str);

const STR: TypeId = TypeId::of::<&'static str>();
const BORROWED: TypeId = TypeId::of::<Borrowed<'static>>();
const HIGHER: TypeId = TypeId::of::<for<'a> fn(&'a str) -> Borrowed<'a>>();
const PAIR: TypeId = TypeId::of::<(&'static [u8], fn(&u32) -> &u32)>();

#[inline(never)]
fn of<T: ?Sized + 'static>() -> TypeId {
    TypeId::of::<T>()
}

pub fn run() -> (bool, bool, bool, bool, bool) {
    (
        STR == of::<&str>(),
        BORROWED == of::<Borrowed<'static>>(),
        HIGHER == of::<fn(&str) -> Borrowed<'_>>(),
        PAIR == of::<(&[u8], for<'b> fn(&'b u32) -> &'b u32)>(),
        STR != BORROWED,
    )
}
