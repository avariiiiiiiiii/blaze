//! Which of a dev build's pieces are the image's and which the workspace link's.
//!
//! An instance is the dependencies' when its item is a dependency's (or std's) and none of
//! its type arguments names a workspace member's type, closure or item: codegen says which
//! ([`Owned`], from `Built::owners`). Instances of dependency generics made for workspace
//! types are the workspace's. A static is not generic: it is its crate's.
//!
//! - [`image_part`] takes the dependencies' pieces for a new image, and checks the image is
//!   closed: a dependency instance that names a workspace instance or static is an error
//!   naming both (it can't be: codegen's ownership says otherwise). Names outside blaze's
//!   code (`Target::Extern`, `Target::Weak`) stay open: natives define them, or the workspace
//!   does (the global allocator's `__rg_alloc`), and the run binds them.
//! - [`workspace_part`] takes, against an image's table, everything the image does not hold:
//!   the workspace's pieces, and the dependencies' instances the image was built without
//!   (`spilled`: a workspace edit that calls a dependency function the image's build did not
//!   reach). Each piece is in exactly one of the two, so every static has one address and
//!   one value, as in a whole link.
//! - Natives: the dependencies' objects, archives and libraries go in the image. The
//!   workspace link gets its own natives, every archive not taken whole, and every shared
//!   library: a workspace reference resolves against the image's table first, then pulls an
//!   archive member the image did not pull, so no member is in both.

use blaze_codegen::{CompiledFunction, InstanceKey, Static, Target, Unwind};
use blaze_ids::ContentId;
use blaze_items::{CodeHash, ItemKey};
use blaze_link::{Native, NativeKind};

use crate::PremapError;
use crate::key::{KeyedFunction, KeyedStatic, static_definition};
use crate::table::{SymbolId, TableView};

/// What codegen says is the dependencies', each list sorted.
#[derive(Clone, Copy, Debug)]
pub struct Owned<'a> {
    /// The dependencies' function instances, with their generics' code hashes, by key.
    pub functions: &'a [(InstanceKey, CodeHash)],
    pub statics: &'a [(ItemKey, ContentId)],
}

/// A dev build's output, as the linker takes it.
#[derive(Clone, Copy, Debug)]
pub struct Built<'a> {
    pub functions: &'a [CompiledFunction],
    pub statics: &'a [Static],
    /// The names Rust code defines, and what each is.
    pub names: &'a [(String, Target)],
    pub debug_names: &'a [(InstanceKey, String)],
}

/// A native of the link line and whose it is: a dependency unit's (its build script's
/// `rustc-link-lib`, its crate's `#[link]`) or a workspace member's. The C runtime's start
/// objects are the run process loader's, in neither.
#[derive(Clone, Copy, Debug)]
pub struct SidedNative<'a> {
    pub native: Native<'a>,
    pub dependency: bool,
}

/// The pieces of a new image.
pub struct ImagePart<'a> {
    pub functions: Vec<&'a CompiledFunction>,
    pub statics: Vec<&'a Static>,
    pub names: Vec<(String, Target)>,
    pub debug_names: Vec<(InstanceKey, String)>,
    pub natives: Vec<Native<'a>>,
    /// What the key holds of them.
    pub keyed_functions: Vec<KeyedFunction>,
    pub keyed_statics: Vec<KeyedStatic>,
}

/// The pieces the workspace link places, given an image.
pub struct WorkspacePart<'a> {
    pub functions: Vec<&'a CompiledFunction>,
    pub statics: Vec<&'a Static>,
    pub names: Vec<(String, Target)>,
    pub debug_names: Vec<(InstanceKey, String)>,
    pub natives: Vec<Native<'a>>,
    /// The dependencies' functions and statics the image does not hold: linked here this
    /// time, and the reason to build the next image of the graph.
    pub spilled: Vec<InstanceKey>,
    pub spilled_statics: Vec<(ItemKey, ContentId)>,
}

impl WorkspacePart<'_> {
    /// Whether the image held every dependency piece this build has.
    pub fn complete(&self) -> bool {
        self.spilled.is_empty() && self.spilled_statics.is_empty()
    }
}

/// The dependencies' pieces of `built`, closed under what they refer to.
pub fn image_part<'a>(
    built: &Built<'a>,
    owned: &Owned<'_>,
    natives: &[SidedNative<'a>],
) -> Result<ImagePart<'a>, PremapError> {
    let is_dep = |key: &InstanceKey| owned.functions.binary_search_by(|it| it.0.cmp(key)).is_ok();
    let is_dep_static =
        |item: &ItemKey, instance: &ContentId| owned.statics.binary_search(&(*item, *instance)).is_ok();
    sorted(owned)?;

    let mut functions = Vec::with_capacity(owned.functions.len());
    let mut keyed_functions = Vec::with_capacity(owned.functions.len());
    for function in built.functions {
        let Ok(at) = owned.functions.binary_search_by(|it| it.0.cmp(&function.key)) else { continue };
        let refers = |target: &Target| match target {
            Target::Function(key) | Target::Direct(key) if !is_dep(key) => Err(PremapError::new(format!(
                "dependency instance {:?} refers to {key:?}, which is the workspace's: an image can't \
                 call into the workspace but by name",
                function.key
            ))),
            Target::Static { item, instance } if !is_dep_static(item, instance) => Err(PremapError::new(format!(
                "dependency instance {:?} refers to the static {item:?} ({instance:?}), which is the workspace's",
                function.key
            ))),
            _ => Ok(()),
        };
        for reloc in function.relocs.iter().chain(function.data.iter().flat_map(|it| it.relocs.iter())) {
            refers(&reloc.target)?;
        }
        match &function.unwind {
            Some(Unwind::Dwarf { personality: Some(target), .. }) | Some(Unwind::Windows { handler: Some(target), .. }) => {
                refers(target)?
            }
            _ => {}
        }
        functions.push(function);
        keyed_functions.push(KeyedFunction { key: function.key, code: owned.functions[at].1 });
    }
    if functions.len() != owned.functions.len() {
        let missing = owned
            .functions
            .iter()
            .find(|(key, _)| !built.functions.iter().any(|it| it.key == *key))
            .map(|it| it.0);
        return Err(PremapError::new(format!(
            "codegen says {missing:?} is a dependency instance of this build, and the build has no code for it"
        )));
    }

    let mut statics = Vec::with_capacity(owned.statics.len());
    let mut keyed_statics = Vec::with_capacity(owned.statics.len());
    for value in built.statics {
        if !is_dep_static(&value.item, &value.instance) {
            continue;
        }
        let relocs = match &value.init {
            blaze_codegen::Init::Bytes { relocs, .. } => relocs.as_slice(),
            blaze_codegen::Init::Zeroed { .. } => &[],
        };
        for reloc in relocs.iter().chain(value.data.iter().flat_map(|it| it.relocs.iter())) {
            match &reloc.target {
                Target::Function(key) | Target::Direct(key) if !is_dep(key) => {
                    return Err(PremapError::new(format!(
                        "the dependency static {:?} points at {key:?}, which is the workspace's",
                        value.item
                    )));
                }
                Target::Static { item, instance } if !is_dep_static(item, instance) => {
                    return Err(PremapError::new(format!(
                        "the dependency static {:?} points at the static {item:?} ({instance:?}), which is the workspace's",
                        value.item
                    )));
                }
                _ => {}
            }
        }
        statics.push(value);
        keyed_statics.push(KeyedStatic {
            item: value.item,
            instance: value.instance,
            definition: static_definition(value),
        });
    }
    if statics.len() != owned.statics.len() {
        return Err(PremapError::new(format!(
            "codegen names {} dependency statics and the build defines {} of them",
            owned.statics.len(),
            statics.len()
        )));
    }

    let in_image = |target: &Target| match target {
        Target::Function(key) | Target::Direct(key) => is_dep(key),
        Target::Static { item, instance } => is_dep_static(item, instance),
        _ => false,
    };
    let names: Vec<(String, Target)> = built.names.iter().filter(|it| in_image(&it.1)).cloned().collect();
    let debug_names = built.debug_names.iter().filter(|it| is_dep(&it.0)).cloned().collect();
    let natives = natives.iter().filter(|it| it.dependency).map(|it| it.native).collect();
    Ok(ImagePart { functions, statics, names, debug_names, natives, keyed_functions, keyed_statics })
}

/// What of `built` the image whose table is `table` does not hold.
pub fn workspace_part<'a>(
    built: &Built<'a>,
    owned: &Owned<'_>,
    table: &TableView<'_>,
    natives: &[SidedNative<'a>],
) -> Result<WorkspacePart<'a>, PremapError> {
    sorted(owned)?;
    let has_function = |key: &InstanceKey| table.get(&SymbolId::function(key)).is_some();
    let has_static = |item: &ItemKey, instance: &ContentId| table.get(&SymbolId::of_static(item, instance)).is_some();

    let mut functions = Vec::with_capacity(built.functions.len().saturating_sub(owned.functions.len()));
    let mut spilled = Vec::new();
    for function in built.functions {
        if has_function(&function.key) {
            continue;
        }
        if owned.functions.binary_search_by(|it| it.0.cmp(&function.key)).is_ok() {
            spilled.push(function.key);
        }
        functions.push(function);
    }
    let mut statics = Vec::with_capacity(built.statics.len().saturating_sub(owned.statics.len()));
    let mut spilled_statics = Vec::new();
    for value in built.statics {
        if has_static(&value.item, &value.instance) {
            continue;
        }
        if owned.statics.binary_search(&(value.item, value.instance)).is_ok() {
            spilled_statics.push((value.item, value.instance));
        }
        statics.push(value);
    }
    let outside = |target: &Target| match target {
        Target::Function(key) | Target::Direct(key) => has_function(key),
        Target::Static { item, instance } => has_static(item, instance),
        _ => false,
    };
    let names = built.names.iter().filter(|it| !outside(&it.1)).cloned().collect();
    let debug_names = built.debug_names.iter().filter(|it| !has_function(&it.0)).cloned().collect();
    let natives = natives
        .iter()
        .filter(|it| match it.native.kind {
            _ if !it.dependency => true,
            NativeKind::Object | NativeKind::Archive { whole: true } => false,
            NativeKind::Archive { whole: false } | NativeKind::Shared { .. } => true,
        })
        .map(|it| it.native)
        .collect();
    Ok(WorkspacePart { functions, statics, names, debug_names, natives, spilled, spilled_statics })
}

fn sorted(owned: &Owned<'_>) -> Result<(), PremapError> {
    if !owned.functions.is_sorted_by(|a, b| a.0 < b.0) || !owned.statics.is_sorted_by(|a, b| a < b) {
        return Err(PremapError::new(
            "codegen's dependency instances are not sorted by key, each once".to_owned(),
        ));
    }
    Ok(())
}
