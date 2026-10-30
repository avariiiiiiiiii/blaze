//! The published slices (design §7, §8): one salsa input per (crate, trait), per (crate, head)
//! and per (crate, name), each with its content and its `ContentId`, set only when the content
//! changed; the crate's [`SliceDirectory`] for keys a reader asks of before they have a slice;
//! and the singleton [`ImplSlices`] that finds a crate's directory.
//!
//! A slice, once made, is kept for the life of the database, emptied rather than dropped, so a
//! directory gains a key once and never loses it: it changes only when a key is first seen.
//!
//! The impls of body blocks stay in their module's pulled tables (`read::trait_bodies`,
//! `read::inherent_bodies`); a slice lists the modules whose bodies hold impls of its key, and
//! the directory the modules whose bodies hold any inherent impl, so no reader goes over the
//! crate's modules.

use std::sync::Arc;

use base_db::{Crate, Db};
use blaze_dbsp::{Pos, SliceHasher};
use blaze_ids::{ContentId, Hasher};
use hir_def::{BuiltinDeriveImplId, ImplId, ModuleId, TraitId, nameres::crate_def_map};
use hir_expand::{HirFileId, name::Name};
use rustc_hash::FxHashMap;
use rustc_type_ir::fast_reject::SimplifiedType as S;
use salsa::Setter;

use super::head::{Head, HeadDef};

/// The domain of an impl set's and of a candidate set's hash (design §7).
pub const IMPL_SET_DOMAIN: &str = "blaze v2 impl set 1";
pub const CANDIDATE_SET_DOMAIN: &str = "blaze v2 candidate set 1";
const IDENTITY_DOMAIN: &str = "blaze v2 impl identity 1";

/// One crate's impls of one trait, by the module that holds each: what the module's own scope
/// declares (`module_impls`), in position order, and the modules whose bodies' blocks hold
/// impls of it, whose tables readers add per module ([`super::read`]).
#[derive(Debug, Default, PartialEq, Eq)]
pub struct TraitRows {
    /// Each impl with its head, `None` for a blanket impl.
    pub impls: Box<[(ModuleId, ImplId, Option<Head>)]>,
    pub derives: Box<[(ModuleId, BuiltinDeriveImplId, Head)]>,
    /// Sorted by `ModuleId`; readers order them by module rank.
    pub bodies: Box<[ModuleId]>,
}

/// One crate's inherent impls for one head, as [`TraitRows`], each with its position: a whole
/// crate's table orders the heads one module holds by where their first impls are.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct InherentRows {
    pub impls: Box<[(ModuleId, ImplId, Pos)]>,
    /// The modules whose bodies hold inherent impls for the head, sorted by `ModuleId`.
    pub bodies: Box<[ModuleId]>,
}

/// The traits declared in one crate's modules with an associated item of one name, sorted by
/// `TraitId`.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct CandidateRows {
    pub traits: Box<[TraitId]>,
}

#[salsa::input(debug)]
pub struct TraitImplsSlice {
    #[returns(copy)]
    pub krate: Crate,
    #[returns(copy)]
    pub trait_: TraitId,
    #[returns(ref)]
    pub rows: TraitRows,
    #[returns(copy)]
    pub hash: ContentId,
}

#[salsa::input(debug)]
pub struct InherentSlice {
    #[returns(copy)]
    pub krate: Crate,
    #[returns(copy)]
    pub head: Head,
    #[returns(ref)]
    pub rows: InherentRows,
    #[returns(copy)]
    pub hash: ContentId,
}

#[salsa::input(debug)]
pub struct CandidateSlice {
    #[returns(copy)]
    pub krate: Crate,
    #[returns(copy)]
    pub name: Name,
    #[returns(ref)]
    pub rows: CandidateRows,
    #[returns(copy)]
    pub hash: ContentId,
}

/// Every key of one crate that has a slice, and the modules whose bodies hold any inherent
/// impl (sorted by `ModuleId`), what a whole crate's inherent table merges. Each field is read
/// on its own, so a new trait key leaves readers of heads and names as they were.
#[salsa::input(debug)]
pub struct SliceDirectory {
    #[returns(copy)]
    pub krate: Crate,
    #[returns(ref)]
    pub traits: Arc<FxHashMap<TraitId, TraitImplsSlice>>,
    #[returns(ref)]
    pub heads: Arc<FxHashMap<Head, InherentSlice>>,
    #[returns(ref)]
    pub names: Arc<FxHashMap<Name, CandidateSlice>>,
    #[returns(ref)]
    pub body_modules: Arc<[ModuleId]>,
}

/// The directory of every crate the maintainer has gone over; set when a crate is first.
#[salsa::input(singleton, debug)]
pub struct ImplSlices {
    #[returns(ref)]
    pub crates: Arc<FxHashMap<Crate, SliceDirectory>>,
}

/// The directory of `krate`. A crate the impls maintainer never went over, and which no
/// frozen table serves, has none: that is a bug in the caller (no maintainer ran), reported
/// with the crate.
pub(crate) fn directory(db: &Db, krate: Crate) -> SliceDirectory {
    let found = ImplSlices::try_get(db).and_then(|it| it.crates(db).get(&krate).copied());
    found.unwrap_or_else(|| {
        panic!("impls: no maintained impl tables for crate {krate:?}: no maintainer ran")
    })
}

/// The slice of `trait_` in `krate`, if it has one: reads the directory's traits, and backdates
/// unless this key gains its slice.
#[salsa::tracked(returns(copy), mem_tag = Infer)]
pub(crate) fn trait_slice(db: &Db, krate: Crate, trait_: TraitId) -> Option<TraitImplsSlice> {
    directory(db, krate).traits(db).get(&trait_).copied()
}

#[salsa::tracked(returns(copy), mem_tag = Infer)]
pub(crate) fn inherent_slice(db: &Db, krate: Crate, head: Head) -> Option<InherentSlice> {
    directory(db, krate).heads(db).get(&head).copied()
}

#[salsa::tracked(returns(copy), mem_tag = Infer)]
pub(crate) fn candidate_slice(db: &Db, krate: Crate, name: Name) -> Option<CandidateSlice> {
    directory(db, krate).names(db).get(&name).copied()
}

/// The hash of the empty slice of a domain: what an absent key's hash is.
fn empty_hash(domain: &'static str) -> ContentId {
    let mut hasher = SliceHasher::new(domain);
    hasher.u64(0);
    hasher.finish()
}

/// The impl-set hash of `trait_` in `krate` (design §7, for lane item-hash): the slice's hash,
/// or the empty set's. A coherence or trait-solving result keys on the hashes of the slices it
/// read. The slice's hash covers its own rows; the impls of the body modules it lists are hashed
/// by [`super::read::body_impl_set_hash`], which this folds in.
pub fn impl_set_hash(db: &Db, krate: Crate, trait_: TraitId) -> ContentId {
    let own = match trait_slice(db, krate, trait_) {
        Some(slice) => slice.hash(db),
        None => empty_hash(IMPL_SET_DOMAIN),
    };
    let bodies = super::read::body_impl_set_hash(db, krate, trait_);
    let mut hasher = SliceHasher::new(IMPL_SET_DOMAIN);
    hasher.id(&own);
    hasher.id(&bodies);
    hasher.finish()
}

/// The candidate-set hash of `name` in `krate` (design §8): the hash of the traits declared in
/// `krate`'s modules with an associated item of that name.
pub fn candidate_set_hash(db: &Db, krate: Crate, name: Name) -> ContentId {
    match candidate_slice(db, krate, name) {
        Some(slice) => slice.hash(db),
        None => empty_hash(CANDIDATE_SET_DOMAIN),
    }
}

/// An impl's identity inside the hashes: its `ImplLoc` content, which is its crate's content,
/// the names of the modules from the crate root to its container, and its file AST ID with
/// the chain of macro calls its file comes from. Lane item-hash takes this over with its stable
/// item identity, here and nowhere else.
pub fn impl_identity(db: &Db, imp: ImplId) -> ContentId {
    let loc = imp.loc(db);
    let mut hasher = Hasher::new(IDENTITY_DOMAIN);
    hasher.part(b"impl");
    item_identity(db, &mut hasher, loc.container, loc.id.file_id, loc.id.value.erase().into_raw());
    hasher.finish()
}

/// An item's `ItemLoc` content into `hasher`: crate, module path, file and AST ID.
fn item_identity(db: &Db, hasher: &mut Hasher, container: ModuleId, file: HirFileId, ast: u32) {
    hasher.id(&container.krate(db).content(db));
    let mut module = Some(container);
    while let Some(at) = module {
        match at.name(db) {
            Some(name) => hasher.part(name.as_str().as_bytes()),
            None if at.block(db).is_some() => hasher.part(b"{block}"),
            None => hasher.part(b"{root}"),
        };
        module = at.containing_module(db);
    }
    hasher.update(&ast.to_le_bytes());
    let mut file = file;
    while let Some(call) = file.macro_file() {
        let kind = &call.loc(db).kind;
        hasher.part(b"macro").update(&kind.erased_ast_id().into_raw().to_le_bytes());
        file = kind.file_id();
    }
}

/// A built-in derive impl's identity inside the hashes: its derive and its index among its
/// ADT's derives (the ADT is its row's head).
pub(super) fn derive_identity(db: &Db, hasher: &mut Hasher, imp: BuiltinDeriveImplId) {
    let loc = imp.loc(db);
    hasher.part(loc.trait_.name().as_str().as_bytes());
    hasher.update(&loc.derive_index.to_le_bytes());
}

/// A head's identity inside the hashes: its variant, its numbers, and its def's `ItemLoc`.
pub(super) fn head_identity(db: &Db, hasher: &mut Hasher, head: Head) {
    let def = |hasher: &mut Hasher, def: HeadDef| {
        let (tag, container, file, ast) = match def {
            HeadDef::Adt(it) => {
                let (container, id) = match it {
                    hir_def::AdtId::StructId(it) => {
                        let loc = it.loc(db);
                        (loc.container, (loc.id.file_id, loc.id.value.erase().into_raw()))
                    }
                    hir_def::AdtId::UnionId(it) => {
                        let loc = it.loc(db);
                        (loc.container, (loc.id.file_id, loc.id.value.erase().into_raw()))
                    }
                    hir_def::AdtId::EnumId(it) => {
                        let loc = it.loc(db);
                        (loc.container, (loc.id.file_id, loc.id.value.erase().into_raw()))
                    }
                };
                (b"adt".as_slice(), container, id.0, id.1)
            }
            HeadDef::Trait(it) => {
                let loc = it.loc(db);
                let ast = loc.id.value.erase().into_raw();
                (b"trait".as_slice(), loc.container, loc.id.file_id, ast)
            }
            HeadDef::Alias(it) => {
                use hir_def::HasModule;
                let loc = it.loc(db);
                let ast = loc.id.value.erase().into_raw();
                (b"alias".as_slice(), it.module(db), loc.id.file_id, ast)
            }
        };
        hasher.part(tag);
        item_identity(db, hasher, container, file, ast);
    };
    let tag: u8 = match head {
        S::Bool => 0,
        S::Char => 1,
        S::Int(it) => {
            hasher.update(&[it as u8]);
            2
        }
        S::Uint(it) => {
            hasher.update(&[it as u8]);
            3
        }
        S::Float(it) => {
            hasher.update(&[it as u8]);
            4
        }
        S::Adt(it) => {
            def(hasher, it);
            5
        }
        S::Foreign(it) => {
            def(hasher, it);
            6
        }
        S::Str => 7,
        S::Array => 8,
        S::Slice => 9,
        S::Ref(it) => {
            hasher.update(&[it as u8]);
            10
        }
        S::Ptr(it) => {
            hasher.update(&[it as u8]);
            11
        }
        S::Never => 12,
        S::Tuple(n) => {
            hasher.update(&(n as u64).to_le_bytes());
            13
        }
        S::MarkerTraitObject => 14,
        S::Trait(it) => {
            def(hasher, it);
            15
        }
        S::Closure(it) | S::Coroutine(it) | S::CoroutineWitness(it) => {
            unreachable!("impls: a head names a closure or coroutine ({it:?})")
        }
        S::Function(n) => {
            hasher.update(&(n as u64).to_le_bytes());
            16
        }
        S::UnsafeBinder => 17,
        S::Placeholder => 18,
        S::Error => 19,
    };
    hasher.update(&[tag]);
}

fn trait_identity(db: &Db, hasher: &mut Hasher, trait_: TraitId) {
    head_identity(db, hasher, Head::Trait(HeadDef::Trait(trait_)));
}

/// The hash of a trait slice: each row's impl identity, head (or blanket) and trait, in order.
/// The body modules are left out: `ModuleId`s are not stable across databases, and what they
/// hold is [`super::read::body_impl_set_hash`]'s.
pub(crate) fn trait_rows_hash(db: &Db, trait_: TraitId, rows: &TraitRows) -> ContentId {
    let mut hasher = Hasher::new(IMPL_SET_DOMAIN);
    hasher.part(b"trait");
    trait_identity(db, &mut hasher, trait_);
    hasher.update(&(rows.impls.len() as u64).to_le_bytes());
    for &(_, imp, head) in rows.impls.iter() {
        hasher.id(&impl_identity(db, imp));
        match head {
            Some(head) => head_identity(db, &mut hasher, head),
            None => _ = hasher.part(b"blanket"),
        }
    }
    hasher.update(&(rows.derives.len() as u64).to_le_bytes());
    for &(_, imp, head) in rows.derives.iter() {
        derive_identity(db, &mut hasher, imp);
        head_identity(db, &mut hasher, head);
    }
    hasher.finish()
}

pub(crate) fn inherent_rows_hash(db: &Db, head: Head, rows: &InherentRows) -> ContentId {
    let mut hasher = Hasher::new(IMPL_SET_DOMAIN);
    hasher.part(b"inherent");
    head_identity(db, &mut hasher, head);
    hasher.update(&(rows.impls.len() as u64).to_le_bytes());
    for &(_, imp, _) in rows.impls.iter() {
        hasher.id(&impl_identity(db, imp));
    }
    hasher.finish()
}

pub(crate) fn candidate_rows_hash(db: &Db, name: Name, rows: &CandidateRows) -> ContentId {
    let mut hasher = Hasher::new(CANDIDATE_SET_DOMAIN);
    hasher.part(name.as_str().as_bytes());
    hasher.update(&(rows.traits.len() as u64).to_le_bytes());
    for &trait_ in rows.traits.iter() {
        trait_identity(db, &mut hasher, trait_);
    }
    hasher.finish()
}

/// The inputs of one crate as the maintainer holds them: each slice made, by key, and the
/// directory. What readers see is only what these inputs hold.
#[derive(Default)]
pub(crate) struct Published {
    directory: Option<SliceDirectory>,
    traits: FxHashMap<TraitId, TraitImplsSlice>,
    heads: FxHashMap<Head, InherentSlice>,
    names: FxHashMap<Name, CandidateSlice>,
    body_modules: Arc<[ModuleId]>,
    /// Keys first seen in this write, which the directory gains at [`Published::finish`].
    new_traits: bool,
    new_heads: bool,
    new_names: bool,
    new_body_modules: bool,
}

impl Published {
    /// Sets the slice of `trait_` to `rows` unless it holds them already; whether it was set.
    pub(crate) fn set_trait(
        &mut self,
        db: &mut Db,
        krate: Crate,
        trait_: TraitId,
        rows: TraitRows,
    ) -> bool {
        match self.traits.get(&trait_) {
            Some(&slice) => {
                if *slice.rows(&*db) == rows {
                    return false;
                }
                let hash = trait_rows_hash(&*db, trait_, &rows);
                slice.set_rows(db).to(rows);
                slice.set_hash(db).to(hash);
            }
            None => {
                let hash = trait_rows_hash(&*db, trait_, &rows);
                let slice = TraitImplsSlice::new(&*db, krate, trait_, rows, hash);
                self.traits.insert(trait_, slice);
                self.new_traits = true;
            }
        }
        true
    }

    pub(crate) fn set_head(
        &mut self,
        db: &mut Db,
        krate: Crate,
        head: Head,
        rows: InherentRows,
    ) -> bool {
        match self.heads.get(&head) {
            Some(&slice) => {
                if *slice.rows(&*db) == rows {
                    return false;
                }
                let hash = inherent_rows_hash(&*db, head, &rows);
                slice.set_rows(db).to(rows);
                slice.set_hash(db).to(hash);
            }
            None => {
                let hash = inherent_rows_hash(&*db, head, &rows);
                let slice = InherentSlice::new(&*db, krate, head, rows, hash);
                self.heads.insert(head, slice);
                self.new_heads = true;
            }
        }
        true
    }

    pub(crate) fn set_name(
        &mut self,
        db: &mut Db,
        krate: Crate,
        name: Name,
        rows: CandidateRows,
    ) -> bool {
        match self.names.get(&name) {
            Some(&slice) => {
                if *slice.rows(&*db) == rows {
                    return false;
                }
                let hash = candidate_rows_hash(&*db, name, &rows);
                slice.set_rows(db).to(rows);
                slice.set_hash(db).to(hash);
            }
            None => {
                let hash = candidate_rows_hash(&*db, name, &rows);
                let slice = CandidateSlice::new(&*db, krate, name, rows, hash);
                self.names.insert(name, slice);
                self.new_names = true;
            }
        }
        true
    }

    /// Sets the directory's body modules to `modules` at [`Published::finish`] unless it holds
    /// them already.
    pub(crate) fn set_body_modules(&mut self, modules: &[ModuleId]) {
        if *self.body_modules != *modules {
            // alloc: the list, once per write that changed it
            self.body_modules = modules.into();
            self.new_body_modules = true;
        }
    }

    /// Every slice made, emptied: the crate maintained from empty sets each anew.
    pub(crate) fn keys(&self) -> (Vec<TraitId>, Vec<Head>, Vec<Name>) {
        (
            self.traits.keys().copied().collect(), // alloc: a crate maintained from empty, once
            self.heads.keys().copied().collect(), // alloc: as above
            self.names.keys().copied().collect(), // alloc: as above
        )
    }

    /// Gives the directory the keys first seen in this write, making it (and registering it in
    /// [`ImplSlices`]) the first time. A directory field is set only when its keys grew.
    pub(crate) fn finish(&mut self, db: &mut Db, krate: Crate) -> u64 {
        let mut set = 0;
        match self.directory {
            Some(directory) => {
                if std::mem::take(&mut self.new_traits) {
                    // alloc: clone: the directory's map, once per write that adds a key
                    directory.set_traits(db).to(Arc::new(self.traits.clone()));
                    set += 1;
                }
                if std::mem::take(&mut self.new_heads) {
                    // alloc: clone: as above
                    directory.set_heads(db).to(Arc::new(self.heads.clone()));
                    set += 1;
                }
                if std::mem::take(&mut self.new_names) {
                    // alloc: clone: as above
                    directory.set_names(db).to(Arc::new(self.names.clone()));
                    set += 1;
                }
                if std::mem::take(&mut self.new_body_modules) {
                    // clone: a count
                    directory.set_body_modules(db).to(self.body_modules.clone());
                    set += 1;
                }
            }
            None => {
                self.new_traits = false;
                self.new_heads = false;
                self.new_names = false;
                self.new_body_modules = false;
                let directory = SliceDirectory::new(
                    &*db,
                    krate,
                    Arc::new(self.traits.clone()), // alloc: clone: the crate's first directory
                    Arc::new(self.heads.clone()), // alloc: clone: as above
                    Arc::new(self.names.clone()), // alloc: clone: as above
                    self.body_modules.clone(),    // clone: a count
                );
                self.directory = Some(directory);
                register(db, krate, directory);
                set += 1;
            }
        }
        set
    }
}

/// Adds `krate`'s directory to the singleton.
fn register(db: &mut Db, krate: Crate, directory: SliceDirectory) {
    match ImplSlices::try_get(&*db) {
        Some(all) => {
            // alloc: clone: the crate map, once per crate first maintained
            let mut crates = (**all.crates(&*db)).clone();
            crates.insert(krate, directory);
            // alloc: the singleton's map, once per crate first maintained
            all.set_crates(db).to(Arc::new(crates));
        }
        None => {
            let mut crates = FxHashMap::default();
            crates.insert(krate, directory);
            // alloc: the singleton's map, once per database
            _ = ImplSlices::new(&*db, Arc::new(crates));
        }
    }
}

/// The module rank of `module` in its crate's `DefMap::modules` order: what readers merge the
/// per-module parts of a slice by.
pub(crate) fn module_rank(db: &Db, module: ModuleId) -> u32 {
    crate_def_map(db, module.krate(db)).module_rank(db, module).unwrap_or_else(|| {
        panic!("impls: module {module:?} is not among its crate's modules")
    })
}
