//! library/coretests panic/location.rs: `Location::caller()` in a constant is where the
//! constant calls it. blaze evaluated it to where `caller` asks, inside `core`, under a file
//! name rustc does not use, and then refused it: the evaluator did not know what rustc names
//! the file. It is the name the build gives it, as for `file!()`.

use std::panic::Location;

#[inline(never)]
fn asked() -> (u32, u32, bool) {
    const CALLER: &Location<'static> = Location::caller();
    const LINE: u32 = CALLER.line();
    const COLUMN: u32 = CALLER.column();
    (LINE, COLUMN, CALLER.file() == file!())
}

pub fn run() -> (u32, u32, bool) {
    asked()
}
