//! blaze's loader: the process of a dev run or test, started by the daemon
//! (`blaze_inproc::load`).

#![forbid(unsafe_code)]

fn main() {
    blaze_inproc::load::main()
}
