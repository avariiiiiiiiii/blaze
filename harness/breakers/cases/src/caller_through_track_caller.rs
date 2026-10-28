//! `Location::caller()` in a constant through `#[track_caller]` const fns: each passes on
//! the place it is called from, and the first function that does not track its caller gives
//! the place of its own call, as at run time.

use std::panic::Location;

#[track_caller]
const fn here() -> &'static Location<'static> {
    Location::caller()
}

#[track_caller]
const fn passed_on() -> &'static Location<'static> {
    here()
}

const fn stops() -> &'static Location<'static> {
    passed_on()
}

const DIRECT: &Location<'static> = here();
const THROUGH: &Location<'static> = passed_on();
const STOPPED: &Location<'static> = stops();

#[inline(never)]
fn place(at: &Location<'_>) -> (u32, u32, bool) {
    (at.line(), at.column(), at.file() == file!())
}

pub fn run() -> [(u32, u32, bool); 4] {
    [place(DIRECT), place(THROUGH), place(STOPPED), place(stops())]
}
