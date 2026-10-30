//! `dead_code` and `dead_code_pub_in_binary`: rustc's dead-code pass. The definitions of the
//! crate (`defs`), what is reachable from outside it (`reach`), where liveness starts (`seed`),
//! what is live from there (`mark`, with `res` for what a name resolves to), and what is
//! reported dead (`report`), each at the level of `dead_code` where it is (`level`).
//!
//! Where the front end can't say what rustc's pass would (a `global_asm!` naming a symbol, a
//! `pub macro` whose expansion reaches private items), nothing is linted.
//!
//! Follows `compiler/rustc_passes/src/dead.rs` (`live_symbols_and_ignored_derived_traits`,
//! `check_mod_deathness`) (MIT or Apache-2.0).

mod defs;
mod level;
mod mark;
mod reach;
mod report;
mod res;
mod seed;

use crate::{cx::Cx, lower::Found};

pub(super) fn lints(cx: &Cx<'_, '_>) -> Vec<Found> {
    let defs = defs::Defs::of(cx);
    if defs.unported {
        return Vec::new();
    }
    let levels = level::Levels::new();
    let reach = reach::Reach::of(cx, &defs);
    let k = seed::Crate { cx, defs: &defs, levels: &levels, reach: &reach, executable: seed::executable(cx) };
    let seeds = seed::seeds(&k);
    let (liveness, mut found) = mark::live(&k, seeds);
    found.extend(report::report(&k, &liveness));
    found
}
