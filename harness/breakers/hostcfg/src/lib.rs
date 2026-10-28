//! What proc macros make when their crates are told the target they run on.

pub use macros::Fields;

pub trait Fields {
    fn fields() -> usize;
}

#[derive(Fields)]
pub struct Pair {
    pub left: u8,
    pub right: u8,
}

#[derive(Fields)]
pub enum Shape {
    Point,
    Line(u8, u8),
    Plane { width: u8, height: u8, depth: u8 },
}

/// `(target_os, target_family)` as a proc macro is told them.
pub fn host() -> (&'static str, &'static str) {
    macros::host!()
}
