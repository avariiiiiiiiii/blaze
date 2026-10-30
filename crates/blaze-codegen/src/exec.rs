//! A test binary's execution table (fastcompile-ident-select 5,
//! docs/v2/plan/lanes/fastcompile/select.md, "Keys"): every function of the binary as test
//! selection keys it, the function of each jump-table slot, the code the binary runs that is
//! in no table, and the functions each test reaches. `blaze_testsel::Image::new` takes its
//! first three as they are, and each test's reached set by position.
//!
//! It is made beside the binary's link, which numbers the slots, and kept with the binary's
//! record (it is plain data), so a binary replayed rather than linked again selects by the
//! table it was linked with.

use std::fmt;

use blaze_ids::{ContentId, Hasher};
use blaze_items::ItemKey;
use serde::{Deserialize, Serialize};

use crate::{
    InstanceKey, Settings, Target,
    driver::{Built, reach::Walk},
    plan::Mode,
};

pub const FN_ID_DOMAIN: &str = "blaze v2 exec fn id 1";

/// What an instance is across versions, test selection's `FnId`: its identity
/// ([`crate::backend::Described::identity`]) with the settings it is compiled with, as one
/// instance compiled two ways (asked for by a member and by a dependency) is two functions of
/// a binary. An edit of a body keeps both.
pub fn fn_id(identity: ContentId, settings: Settings) -> ContentId {
    let mut hasher = Hasher::new(FN_ID_DOMAIN);
    hasher.id(&identity).id(&settings.0);
    hasher.finish()
}

pub const BOUND_DOMAIN: &str = "blaze v2 exec bound 1";

/// An instance's code as it runs, test selection's `Bound`: what the front end says of it
/// ([`crate::backend::Described::bound`]: its code, its item's bound hash, its type arguments)
/// with what each of its references binds to, in order: a function by its item (`item_of`,
/// the build's), so that a callee's body edit is the callee's change and which item it calls
/// (a `Drop` impl added under drop glue, a method resolved by its type arguments) the
/// instance's; a static by its item and instance, data by its content and a symbol by its
/// name. The positions, coverage, counters and PGO counters a function names are its own.
pub(crate) fn bound(
    described: &Result<ContentId, String>,
    refs: &[Target],
    item_of: impl Fn(&InstanceKey) -> Option<ItemKey>,
) -> Result<ContentId, String> {
    // clone: an error, only where an item has no bound hash
    let described = described.clone()?;
    let mut hasher = Hasher::new(BOUND_DOMAIN);
    hasher.id(&described).update(&(refs.len() as u64).to_le_bytes());
    for target in refs {
        match target {
            Target::Function(key) | Target::Direct(key) => {
                let Some(item) = item_of(key) else {
                    // alloc: an error, of a walk that missed a callee
                    return Err(format!("refers to {key:?}, which the build has no instance of"));
                };
                hasher.update(&[0]).id(&item.0)
            }
            Target::Static { item, instance } => hasher.update(&[1]).id(&item.0).id(instance),
            Target::Data(id) => hasher.update(&[2]).id(id),
            Target::Extern(name) => hasher.update(&[3]).part(name.as_bytes()),
            Target::Weak(name) => hasher.update(&[4]).part(name.as_bytes()),
            Target::Position(_) | Target::Coverage(_) | Target::Counter(_) | Target::ProfileCounters => {
                hasher.update(&[5])
            }
        };
    }
    Ok(hasher.finish())
}

/// Whether a binary built in `mode` has jump-table slots, so that its fork server records
/// what each test ran (`blaze_usercode::test::fork_server(true)`) and its link starts on the
/// execution log (`blaze_link::Output::exec_log`): a live build reaches its members through
/// slots and an in-process tiered run every function through the lazy table, where a file's
/// calls are direct.
pub fn records(mode: Mode) -> bool {
    matches!(mode, Mode::Live | Mode::Tiered)
}

/// A test binary's table.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug, Default)]
pub struct ExecTable {
    /// Every function of the binary, those dedupe merged away included, as `(FnId, Bound)`
    /// in `FnId` order: the order `blaze_testsel::Image::new` keys by, which a table already
    /// in checks with a compare per function (fastcompile-sol-select 2).
    pub fns: Vec<(ContentId, ContentId)>,
    /// Slot `n`'s function, an index into `fns`: one per slot of the binary's jump table.
    /// [`ExecTable::EMPTY`] for a slot an incremental link emptied, which no code names.
    pub slot_fn: Vec<u32>,
    /// The content ID of the code the binary runs that is in no table: the premapped
    /// dependency image; zero for none.
    pub beside: ContentId,
    /// Each test in the harness's order, with the functions it reaches as indices into `fns`,
    /// sorted.
    pub tests: Vec<(String, Vec<u32>)>,
}

/// Why a binary has no table.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ExecError {
    /// A function whose item has no bound hash, and why.
    Unbound { function: InstanceKey, why: String },
    /// Two functions of the binary are one function across versions.
    Twice { function: InstanceKey, other: InstanceKey },
    /// The link put a function in a slot past its jump table's.
    PastTable { function: InstanceKey, slot: u32, slots: u32 },
    /// The link named one slot for two functions.
    TwoInSlot { slot: u32, function: InstanceKey, other: InstanceKey },
    /// A test whose root is not one of the build's.
    NoRoot { test: String, root: usize },
}

impl fmt::Display for ExecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ExecError::Unbound { function, why } => {
                write!(f, "{function:?} has no bound code, which test selection keys it by: {why}")
            }
            ExecError::Twice { function, other } => {
                write!(f, "{function:?} and {other:?} are one function across versions")
            }
            ExecError::PastTable { function, slot, slots } => {
                write!(f, "{function:?} is in slot {slot} of a jump table of {slots}")
            }
            ExecError::TwoInSlot { slot, function, other } => {
                write!(f, "slot {slot} is named for both {function:?} and {other:?}")
            }
            ExecError::NoRoot { test, root } => {
                write!(f, "the test `{test}` is at root {root}, which the build does not have")
            }
        }
    }
}

impl std::error::Error for ExecError {}

impl ExecTable {
    /// The function of a slot that runs none: one an incremental link emptied of a function
    /// gone from the binary, which nothing calls until another is put in it.
    pub const EMPTY: u32 = u32::MAX;

    /// The table of the test binary linked from `built`. `slots` is how many slots its jump
    /// table has and `slot_of` the slot the link named for a function, if any (an incremental
    /// image's `blaze_link::Image::slot`, a lazy run's table); `beside` is what
    /// [`ExecTable::beside`] says; `tests` are the harness's tests in its order, each with its
    /// function's place in [`Built::roots`].
    pub fn new(
        built: &Built,
        slots: u32,
        slot_of: impl Fn(&InstanceKey) -> Option<u32>,
        beside: ContentId,
        tests: impl IntoIterator<Item = (String, usize)>,
    ) -> Result<ExecTable, ExecError> {
        assert_eq!(built.versions.len(), built.inputs.len(), "a build gives a version per instance");
        let count = built.inputs.len();
        assert!(count < ExecTable::EMPTY as usize, "fewer functions than `EMPTY`");
        // alloc: once per test binary, the identity order
        let mut order: Vec<u32> = (0..count as u32).collect();
        // By place too, so that of two functions with one `FnId` the later is named.
        order.sort_unstable_by_key(|&at| (built.versions[at as usize].0, at));
        // alloc: once per test binary, the table it keeps
        let mut fns: Vec<(ContentId, ContentId)> = Vec::with_capacity(count);
        // alloc: once per test binary, each function's place in the table
        let mut rank = vec![0u32; count];
        for (place, &at) in order.iter().enumerate() {
            let key = built.inputs[at as usize].0;
            let (id, bound) = &built.versions[at as usize];
            let bound = match bound {
                Ok(it) => *it,
                // clone: an error
                Err(why) => return Err(ExecError::Unbound { function: key, why: why.clone() }),
            };
            if fns.last().is_some_and(|last| last.0 == *id) {
                let other = built.inputs[order[place - 1] as usize].0;
                return Err(ExecError::Twice { function: key, other });
            }
            rank[at as usize] = place as u32;
            fns.push((*id, bound));
        }

        // alloc: once per test binary, a word per slot
        let mut slot_fn = vec![ExecTable::EMPTY; slots as usize];
        for (at, (key, _)) in built.inputs.iter().enumerate() {
            let Some(slot) = slot_of(key) else { continue };
            let Some(place) = slot_fn.get_mut(slot as usize) else {
                return Err(ExecError::PastTable { function: *key, slot, slots });
            };
            if *place != ExecTable::EMPTY {
                let other = built.inputs[order[*place as usize] as usize].0;
                return Err(ExecError::TwoInSlot { slot, function: *key, other });
            }
            *place = rank[at];
        }

        let mut walk = Walk::default();
        // alloc: once per test binary, a bit per function, cleared by each test that sets it
        let mut bits = vec![0u64; count.div_ceil(64)];
        // alloc: once per test binary, its tests
        let mut reached = Vec::new();
        for (test, root) in tests {
            let functions = built.roots.get(root).and_then(|key| built.reach.functions_of(key, &mut walk));
            let Some(mut functions) = functions else {
                return Err(ExecError::NoRoot { test, root });
            };
            in_table_order(&mut functions, &rank, &mut bits);
            reached.push((test, functions));
        }
        Ok(ExecTable { fns, slot_fn, beside, tests: reached })
    }
}

/// `functions`, places in key order, as places in the table, sorted: by a sort when they are
/// few, else by `bits` (all clear, and left so), as the table's order is no function of the
/// keys'.
fn in_table_order(functions: &mut Vec<u32>, rank: &[u32], bits: &mut [u64]) {
    for it in functions.iter_mut() {
        *it = rank[*it as usize];
    }
    if functions.len() * 4 < bits.len() {
        functions.sort_unstable();
        return;
    }
    for &it in functions.iter() {
        bits[it as usize / 64] |= 1 << (it % 64);
    }
    functions.clear();
    for (word, bits) in bits.iter_mut().enumerate() {
        while *bits != 0 {
            functions.push(word as u32 * 64 + bits.trailing_zeros());
            *bits &= *bits - 1;
        }
    }
}

#[cfg(test)]
mod tests;
