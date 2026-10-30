//! Whose each instance and static is for a dependency image (fastcompile-premap 3,
//! docs/v2/plan/lanes/fastcompile/premap.md): a dependency's code goes in the image linked
//! once per set of dependencies, the workspace's in the link of every edit.
//!
//! The front end decides an instance's owner where it sees its type arguments
//! ([`crate::backend::Described::owner`]): an instance of a dependency's generic made for a
//! workspace type is the workspace's. A static is its crate's. A dependency's code never refers
//! to the workspace's, or the image would change with workspace code: the build checks every
//! reference of the dependency's code and refuses one to the workspace's, naming both.

use blaze_ids::ContentId;
use blaze_items::{CodeHash, ItemKey};
use serde::{Deserialize, Serialize};

use crate::{CodegenError, InstanceKey, Target, plan::Origin};

/// Whose code an instance or static is, for the dependency image.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum Owner {
    /// Linked once into the image of the build's dependencies: of a dependency's or the
    /// standard library's item, and no type argument names a workspace crate's type, closure
    /// or item. Ordered first: of two describers of one instance, the image's wins, since an
    /// instance is its key's code wherever it is linked.
    Dependency,
    /// Linked with every edit.
    Workspace,
}

impl Owner {
    /// The owner of code of a crate of `origin` that no type argument makes another's: a
    /// dependency's and the standard library's are the image's, a member's and build-time
    /// code's the workspace's.
    pub fn of(origin: &Origin) -> Owner {
        match origin {
            Origin::Dependency(_) | Origin::Std => Owner::Dependency,
            Origin::Member(_) | Origin::BuildTime(_) => Owner::Workspace,
        }
    }
}

/// A function of the build as [`owners`] takes it.
#[derive(Clone, Copy)]
pub(super) struct Function<'a> {
    pub(super) key: InstanceKey,
    pub(super) item: ItemKey,
    pub(super) owner: Owner,
    /// The code hash of the generic it is an instance of.
    pub(super) code: CodeHash,
    pub(super) refs: &'a [Target],
}

/// A static of the build as [`owners`] takes it.
#[derive(Clone, Copy)]
pub(super) struct StaticOf<'a> {
    pub(super) id: (ItemKey, ContentId),
    /// What its errors name it by: its value, as the build's other errors of a static do.
    pub(super) unit: InstanceKey,
    pub(super) owner: Owner,
    pub(super) refs: &'a [Target],
}

/// What [`owners`] gives: [`super::Built::owners`] and [`super::Built::static_owners`].
pub(super) struct Owned {
    pub(super) functions: Vec<(InstanceKey, Owner, CodeHash)>,
    pub(super) statics: Vec<(ItemKey, ContentId, Owner)>,
}

/// The owners of `functions` and `statics`, each sorted by key without duplicates, and into
/// `errors` one error for each of the dependency's functions and statics that refers to the
/// workspace's, naming the first such reference.
pub(super) fn owners<'a, F, S>(functions: F, statics: S, errors: &mut Vec<CodegenError>) -> Owned
where
    F: Iterator<Item = Function<'a>> + Clone,
    S: Iterator<Item = StaticOf<'a>> + Clone,
{
    let owned = Owned {
        // alloc: per build, what it gives the linker
        functions: functions.clone().map(|it| (it.key, it.owner, it.code)).collect(),
        // alloc: per build, what it gives the linker
        statics: statics.clone().map(|it| (it.id.0, it.id.1, it.owner)).collect(),
    };
    debug_assert!(owned.functions.is_sorted_by(|a, b| a.0 < b.0));
    debug_assert!(owned.statics.is_sorted_by(|a, b| (a.0, a.1) < (b.0, b.1)));
    let owner_of = |target: &Target| match target {
        Target::Function(key) | Target::Direct(key) => {
            let at = owned.functions.binary_search_by(|it| it.0.cmp(key)).ok()?;
            Some(owned.functions[at].1)
        }
        Target::Static { item, instance } => {
            let at = owned.statics.binary_search_by(|it| (it.0, it.1).cmp(&(*item, *instance))).ok()?;
            Some(owned.statics[at].2)
        }
        Target::Data(_)
        | Target::Extern(_)
        | Target::Weak(_)
        | Target::Position(_)
        | Target::Coverage(_)
        | Target::Counter(_)
        | Target::ProfileCounters => None,
    };
    let mut check = |unit: InstanceKey, item: ItemKey, refs: &[Target]| {
        let Some(target) = refs.iter().find(|it| owner_of(*it) == Some(Owner::Workspace)) else { return };
        errors.push(CodegenError {
            unit,
            item,
            // alloc: an error
            what: format!(
                "is a dependency's, so it goes in the dependency image, and it refers to \
                 {target:?}, which is the workspace's: the image would change with workspace code"
            ),
        });
    };
    for it in functions.filter(|it| it.owner == Owner::Dependency) {
        check(it.key, it.item, it.refs);
    }
    for it in statics.filter(|it| it.owner == Owner::Dependency) {
        check(it.unit, it.id.0, it.refs);
    }
    owned
}

#[cfg(test)]
mod tests;
