//! Which item: every [`ItemKey`], and the way back from one to the item it names.
//!
//! A key is like rustc's `DefPathHash`: the key of what the item is in (its module, impl,
//! trait, enum or body), its kind, its name and where it is written, as the AST id of its
//! declaration in the file that declares it. That AST id is the kind of syntax, a hash of the
//! name and an index among the declarations of that file with both equal (`span::ast_id`), so
//! it does not move when another item is added, removed or edited; an item a macro wrote is in
//! that macro's expansion, named by the key of the call. A key never says what the item is:
//! an edit of its signature or body keeps it, a rename or a move gives a new one.
//!
//! A crate is named by its content, never by its `Crate` id: its name, version, origin, kind
//! of target and the cfg it is built with. Features are in the cfg: one package is built
//! twice in one build, for the host and for the target, with different features and, when not
//! cross compiling, the same target cfg. Turning on a feature names its crate's items anew;
//! their hashes stay what they were, so it costs queries, not code. Two crates of one build
//! with equal keys are an error [`resolve`] stops at.
//!
//! [`resolve`] goes back from a key to the item of the current database, by indexes over every
//! crate's items: one of the items of its modules, impls and traits, one per body of the items
//! of its blocks, its closures and its opaque types. A key it does not find names nothing that
//! is still there.

use base_db::{Crate, CrateOrigin, LangCrateOrigin, TargetKind};
use blaze_ids::{ContentId, Hasher};
use hir_def::{
    AdtId, BuiltinDeriveImplId, DefWithBodyId, EnumId, EnumVariantId, ExpressionStoreOwnerId,
    GenericDefId, HasModule, ImplId, ItemContainerId, Lookup, MacroId, ModuleDefId, ModuleId,
    ModuleIdLt, TraitId, VariantId,
    expr_store::ExpressionStore,
    hir::{ClosureKind, Expr, ExprId},
    nameres::DefMap,
    signatures::{
        ConstSignature, EnumSignature, FunctionSignature, StaticSignature, StructSignature,
        TraitSignature, TypeAliasSignature, UnionSignature,
    },
};
use hir_expand::{HirFileId, MacroCallKind, name::Name};
use hir_ty::{
    ImplTraitId, InferBodyId, SelfAndAssocBounds,
    db::{
        AnonConstId, ClosureKey, HirDatabase, InternedClosureId, InternedCoroutineClosureId,
        InternedCoroutineId, InternedOpaqueTyId,
    },
    next_solver::SolverDefId,
};
use rustc_hash::FxHashMap;
use span::ErasedFileAstId;
use syntax::ast::HasName;

use crate::{ItemId, ItemKey, NAME_DOMAIN, NameHash, SimplifiedType};

pub const KEY_DOMAIN: &str = "blaze v2 item key 1";
pub const CRATE_DOMAIN: &str = "blaze v2 crate key 1";
pub const SIMPLIFIED_DOMAIN: &str = "blaze v2 simplified type 1";

/// What a key names, in the current database.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Resolved {
    Item(ItemId),
    Module(ModuleId),
    Variant(EnumVariantId),
    /// A closure, coroutine or coroutine closure: the `Expr::Closure` at `expr` of the
    /// expression store of `store`. A coroutine closure has two keys, one as the closure
    /// whose type it has and one as itself; both resolve here.
    Closure { store: ExpressionStoreOwnerId, expr: ExprId, kind: ClosureKind },
    /// The `index`th `impl Trait` of the return type of a function or of a type alias.
    Opaque { owner: ItemId, index: u32 },
    BuiltinDerive(BuiltinDeriveImplId),
}

/// The key of a crate: its content, never its id (see the module doc).
#[salsa::tracked]
pub fn crate_key(db: &dyn HirDatabase, krate: Crate) -> ContentId {
    let data = krate.data(db);
    let extra = krate.extra_data(db);
    let mut h = Hasher::new(CRATE_DOMAIN);
    match &extra.display_name {
        Some(name) => h.update(&[1]).part(name.canonical_name().as_str().as_bytes()),
        None => h.update(&[0]),
    };
    match &extra.version {
        Some(version) => h.update(&[1]).part(version.as_bytes()),
        None => h.update(&[0]),
    };
    match &data.origin {
        CrateOrigin::Rustc { name } => h.update(&[0]).part(name.as_str().as_bytes()),
        CrateOrigin::Local { repo, name } => {
            h.update(&[1]);
            opt(&mut h, repo.as_deref());
            opt(&mut h, name.as_ref().map(|it| it.as_str()))
        }
        CrateOrigin::Library { repo, name } => {
            h.update(&[2]);
            opt(&mut h, repo.as_deref());
            h.part(name.as_str().as_bytes())
        }
        CrateOrigin::Lang(lang) => h.update(&[3, lang_tag(*lang)]),
    };
    match data.target_kind {
        TargetKind::Bin => h.update(&[0]),
        TargetKind::Lib { is_proc_macro } => h.update(&[1, is_proc_macro as u8]),
        TargetKind::Example => h.update(&[2]),
        TargetKind::Test => h.update(&[3]),
        TargetKind::Bench => h.update(&[4]),
        TargetKind::BuildScript => h.update(&[5]),
        TargetKind::Other => h.update(&[6]),
    };
    let mut atoms: Vec<Vec<u8>> = Vec::new();
    for atom in krate.cfg_options(db) {
        let mut bytes = Vec::new();
        match atom {
            cfg::CfgAtom::Flag(name) => {
                bytes.push(0);
                frame(&mut bytes, name.as_str());
            }
            cfg::CfgAtom::KeyValue { key, value } => {
                bytes.push(1);
                frame(&mut bytes, key.as_str());
                frame(&mut bytes, value.as_str());
            }
        }
        atoms.push(bytes);
    }
    atoms.sort_unstable();
    h.update(&(atoms.len() as u64).to_le_bytes());
    for atom in &atoms {
        h.part(atom);
    }
    h.finish()
}

fn lang_tag(lang: LangCrateOrigin) -> u8 {
    match lang {
        LangCrateOrigin::Alloc => 0,
        LangCrateOrigin::Core => 1,
        LangCrateOrigin::ProcMacro => 2,
        LangCrateOrigin::Std => 3,
        LangCrateOrigin::Test => 4,
        LangCrateOrigin::Dependency => 5,
        LangCrateOrigin::Other => 6,
    }
}

fn frame(bytes: &mut Vec<u8>, part: &str) {
    bytes.extend_from_slice(&(part.len() as u64).to_le_bytes());
    bytes.extend_from_slice(part.as_bytes());
}

fn opt<'h>(h: &'h mut Hasher, part: Option<&str>) -> &'h mut Hasher {
    match part {
        None => h.update(&[0]),
        Some(part) => h.update(&[1]).part(part.as_bytes()),
    }
}

/// A key's preimage: the bytes a [`Hasher`] would be given (`part` is length-prefixed, `id`
/// the 32 bytes), kept so that many keys hash in one batch and come out as one at a time.
#[derive(Default)]
struct Bytes(Vec<u8>);

impl Bytes {
    fn update(&mut self, bytes: &[u8]) -> &mut Bytes {
        self.0.extend_from_slice(bytes);
        self
    }

    fn part(&mut self, bytes: &[u8]) -> &mut Bytes {
        self.update(&(bytes.len() as u64).to_le_bytes()).update(bytes)
    }

    fn id(&mut self, id: &ContentId) -> &mut Bytes {
        self.update(&id.0)
    }

    fn opt(&mut self, part: Option<&str>) -> &mut Bytes {
        match part {
            None => self.update(&[0]),
            Some(part) => self.update(&[1]).part(part.as_bytes()),
        }
    }
}

/// Many preimages hashed in one batch: BLAKE3 over 16 inputs at a time with AVX-512, 4 with
/// NEON, each exactly `ContentId::of(KEY_DOMAIN, it)`.
fn hash_all<T>(entries: Vec<(T, Bytes)>) -> impl Iterator<Item = (T, ItemKey)> {
    let inputs: Vec<&[u8]> = entries.iter().map(|(_, it)| &it.0[..]).collect();
    let keys = ContentId::of_many(KEY_DOMAIN, &inputs);
    drop(inputs);
    entries.into_iter().zip(keys).map(|((it, _), key)| (it, ItemKey(key)))
}

/// The key of a module: of its crate for a crate's root, of its block for a block's, else of
/// its parent and its name.
pub fn module_key(db: &dyn HirDatabase, module: ModuleId) -> ItemKey {
    module_key_in(db, module)
}

#[salsa::tracked]
fn module_key_in<'db>(db: &'db dyn HirDatabase, module: ModuleIdLt<'db>) -> ItemKey {
    let mut b = Bytes::default();
    match module.containing_module_inside_def_map(db) {
        Some(parent) => {
            let name = module.name(db);
            b.id(&module_key_in(db, parent).0).update(b"mod").opt(name.as_ref().map(|it| it.as_str()));
        }
        None => match module.block(db) {
            Some(block) => {
                let ast = block.ast_id(db);
                b.id(&module_key_in(db, block.module(db)).0).update(b"block");
                written(db, &mut b, ast.file_id, ast.value.erase());
            }
            None => {
                b.update(b"crate").id(&crate_key(db, module.krate(db)));
            }
        },
    }
    ItemKey(ContentId::of(KEY_DOMAIN, &b.0))
}

/// Where a declaration is written: the file (an expansion named by its call) and the raw AST
/// id in it.
fn written(db: &dyn HirDatabase, b: &mut Bytes, file: HirFileId, ast: ErasedFileAstId) {
    file_key(db, b, file);
    b.update(&ast.into_raw().to_le_bytes());
}

fn file_key(db: &dyn HirDatabase, b: &mut Bytes, file: HirFileId) {
    match file {
        HirFileId::FileId(_) => {
            b.update(&[0]);
        }
        HirFileId::MacroFile(call) => {
            let loc = call.loc(db);
            b.update(&[1]);
            written(db, b, loc.kind.file_id(), loc.kind.erased_ast_id());
            match &loc.kind {
                MacroCallKind::FnLike { .. } => b.update(&[0]),
                MacroCallKind::Derive { derive_attr_index, derive_index, .. } => b
                    .update(&[1])
                    .update(&derive_attr_index.item_tree_index().to_le_bytes())
                    .update(&derive_index.to_le_bytes()),
                MacroCallKind::Attr { censored_attr_ids, .. } => b
                    .update(&[2])
                    .update(&censored_attr_ids.invoc_attr().item_tree_index().to_le_bytes()),
            };
        }
    }
}

fn container_key(db: &dyn HirDatabase, container: ItemContainerId) -> (ItemKey, &'static [u8]) {
    match container {
        ItemContainerId::ModuleId(it) => (module_key(db, it), b""),
        ItemContainerId::ImplId(it) => (item_key(db, ItemId::Impl(it)), b""),
        ItemContainerId::TraitId(it) => (item_key(db, ItemId::Trait(it)), b""),
        // An extern block names nothing: its items are in its module's namespaces.
        ItemContainerId::ExternBlockId(it) => (module_key(db, it.lookup(db).container), b"extern "),
    }
}

/// What an item's key is hashed in a batch with: the items of its module, impl or trait.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Siblings {
    Module(ModuleId),
    Impl(ImplId),
    Trait(TraitId),
}

fn siblings_of_container(db: &dyn HirDatabase, container: ItemContainerId) -> Siblings {
    match container {
        ItemContainerId::ModuleId(it) => Siblings::Module(it),
        ItemContainerId::ImplId(it) => Siblings::Impl(it),
        ItemContainerId::TraitId(it) => Siblings::Trait(it),
        ItemContainerId::ExternBlockId(it) => Siblings::Module(it.lookup(db).container),
    }
}

fn siblings(db: &dyn HirDatabase, id: ItemId) -> Siblings {
    match id {
        ItemId::Function(it) => siblings_of_container(db, it.lookup(db).container),
        ItemId::Const(it) => siblings_of_container(db, it.lookup(db).container),
        ItemId::Static(it) => siblings_of_container(db, it.lookup(db).container),
        ItemId::Alias(it) => siblings_of_container(db, it.lookup(db).container),
        other => Siblings::Module(other.module(db)),
    }
}

/// The key of an item (see the module doc), hashed in one batch with its siblings'.
#[salsa::tracked]
pub fn item_key(db: &dyn HirDatabase, id: ItemId) -> ItemKey {
    let batch = match siblings(db, id) {
        Siblings::Module(it) => module_item_keys(db, it),
        Siblings::Impl(it) => impl_item_keys(db, it),
        Siblings::Trait(it) => trait_item_keys(db, it),
    };
    match batch.get(&id) {
        Some(&key) => key,
        // An item its module's scope does not list, as a `macro_rules!` that a later one of
        // its name shadows: the same bytes, hashed alone.
        None => item_key_alone(db, id),
    }
}

/// An item's key hashed by itself, which the batch gives bit for bit.
pub(crate) fn item_key_alone(db: &dyn HirDatabase, id: ItemId) -> ItemKey {
    ItemKey(ContentId::of(KEY_DOMAIN, &item_preimage(db, id).0))
}

/// The keys of the items a module's scope declares.
#[salsa::tracked(returns(ref))]
fn module_item_keys<'db>(
    db: &'db dyn HirDatabase,
    module: ModuleIdLt<'db>,
) -> FxHashMap<ItemId, ItemKey> {
    let scope = module.def_map(db).module(db, module).scope(db);
    let mut ids = Vec::new();
    for def in scope.declarations() {
        match def {
            ModuleDefId::FunctionId(it) => ids.push(ItemId::Function(it)),
            ModuleDefId::AdtId(it) => ids.push(ItemId::Adt(it)),
            ModuleDefId::ConstId(it) => ids.push(ItemId::Const(it)),
            ModuleDefId::StaticId(it) => ids.push(ItemId::Static(it)),
            ModuleDefId::TraitId(it) => ids.push(ItemId::Trait(it)),
            ModuleDefId::TypeAliasId(it) => ids.push(ItemId::Alias(it)),
            ModuleDefId::MacroId(it) => ids.push(ItemId::Macro(it)),
            ModuleDefId::EnumVariantId(_) | ModuleDefId::ModuleId(_) | ModuleDefId::BuiltinType(_) => {}
        }
    }
    ids.extend(scope.impls().map(ItemId::Impl));
    ids.extend(scope.unnamed_consts().map(ItemId::Const));
    for (_, macros) in scope.legacy_macros() {
        ids.extend(macros.iter().map(|&it| ItemId::Macro(it)));
    }
    // SAFETY: the static id is only compared with others inside this query, never kept.
    let here = Siblings::Module(unsafe { module.to_static() });
    ids.retain(|&id| siblings(db, id) == here);
    ids.sort_unstable();
    ids.dedup();
    let entries = ids.into_iter().map(|id| (id, item_preimage(db, id))).collect();
    hash_all(entries).collect()
}

#[salsa::tracked(returns(ref))]
fn impl_item_keys(db: &dyn HirDatabase, impl_: ImplId) -> FxHashMap<ItemId, ItemKey> {
    let entries = impl_
        .impl_items(db)
        .items
        .iter()
        .map(|&(_, it)| (ItemId::from(it), item_preimage(db, it.into())))
        .collect();
    hash_all(entries).collect()
}

#[salsa::tracked(returns(ref))]
fn trait_item_keys(db: &dyn HirDatabase, trait_: TraitId) -> FxHashMap<ItemId, ItemKey> {
    let entries = trait_
        .trait_items(db)
        .items
        .iter()
        .map(|&(_, it)| (ItemId::from(it), item_preimage(db, it.into())))
        .collect();
    hash_all(entries).collect()
}

/// The bytes an item's key is the hash of: its parent's key, its kind, its name and where it
/// is written.
fn item_preimage(db: &dyn HirDatabase, id: ItemId) -> Bytes {
    let (parent, prefix, tag, name, file, ast): (_, &[u8], &[u8], _, _, _) = match id {
        ItemId::Function(it) => {
            let loc = it.lookup(db);
            let (parent, prefix) = container_key(db, loc.container);
            let name = Some(FunctionSignature::of(db, it).name.as_str().to_owned());
            (parent, prefix, b"fn", name, loc.id.file_id, loc.id.value.erase())
        }
        ItemId::Adt(AdtId::StructId(it)) => {
            let loc = it.lookup(db);
            let name = Some(StructSignature::of(db, it).name.as_str().to_owned());
            (module_key(db, loc.container), b"", b"struct", name, loc.id.file_id, loc.id.value.erase())
        }
        ItemId::Adt(AdtId::EnumId(it)) => {
            let loc = it.lookup(db);
            let name = Some(EnumSignature::of(db, it).name.as_str().to_owned());
            (module_key(db, loc.container), b"", b"enum", name, loc.id.file_id, loc.id.value.erase())
        }
        ItemId::Adt(AdtId::UnionId(it)) => {
            let loc = it.lookup(db);
            let name = Some(UnionSignature::of(db, it).name.as_str().to_owned());
            (module_key(db, loc.container), b"", b"union", name, loc.id.file_id, loc.id.value.erase())
        }
        ItemId::Trait(it) => {
            let loc = it.lookup(db);
            let name = Some(TraitSignature::of(db, it).name.as_str().to_owned());
            (module_key(db, loc.container), b"", b"trait", name, loc.id.file_id, loc.id.value.erase())
        }
        ItemId::Impl(it) => {
            let loc = it.lookup(db);
            (module_key(db, loc.container), b"", b"impl", None, loc.id.file_id, loc.id.value.erase())
        }
        ItemId::Const(it) => {
            let loc = it.lookup(db);
            let (parent, prefix) = container_key(db, loc.container);
            let name = ConstSignature::of(db, it).name.as_ref().map(|it| it.as_str().to_owned());
            (parent, prefix, b"const", name, loc.id.file_id, loc.id.value.erase())
        }
        ItemId::Static(it) => {
            let loc = it.lookup(db);
            let (parent, prefix) = container_key(db, loc.container);
            let name = Some(StaticSignature::of(db, it).name.as_str().to_owned());
            (parent, prefix, b"static", name, loc.id.file_id, loc.id.value.erase())
        }
        ItemId::Alias(it) => {
            let loc = it.lookup(db);
            let (parent, prefix) = container_key(db, loc.container);
            let name = Some(TypeAliasSignature::of(db, it).name.as_str().to_owned());
            (parent, prefix, b"type", name, loc.id.file_id, loc.id.value.erase())
        }
        ItemId::Macro(MacroId::MacroRulesId(it)) => {
            let loc = it.lookup(db);
            let name = loc.id.to_node(db).name().map(|it| it.text().to_string());
            (module_key(db, loc.container), b"", b"macro_rules", name, loc.id.file_id, loc.id.value.erase())
        }
        ItemId::Macro(MacroId::Macro2Id(it)) => {
            let loc = it.lookup(db);
            let name = loc.id.to_node(db).name().map(|it| it.text().to_string());
            (module_key(db, loc.container), b"", b"macro", name, loc.id.file_id, loc.id.value.erase())
        }
        ItemId::Macro(MacroId::ProcMacroId(it)) => {
            let loc = it.lookup(db);
            let name = loc.id.to_node(db).name().map(|it| it.text().to_string());
            (module_key(db, loc.container), b"", b"proc_macro", name, loc.id.file_id, loc.id.value.erase())
        }
    };
    let mut b = Bytes::default();
    b.id(&parent.0).part(prefix).part(tag).opt(name.as_deref());
    written(db, &mut b, file, ast);
    b
}

/// The key of an enum variant: its enum's and its name.
pub fn variant_key(db: &dyn HirDatabase, variant: EnumVariantId) -> ItemKey {
    let loc = variant.lookup(db);
    match enum_variant_keys(db, loc.parent).get(&variant) {
        Some(&key) => key,
        None => ItemKey(ContentId::of(KEY_DOMAIN, &variant_preimage(db, loc.parent, &loc.name).0)),
    }
}

fn variant_preimage(db: &dyn HirDatabase, parent: EnumId, name: &Name) -> Bytes {
    let mut b = Bytes::default();
    b.id(&item_key(db, ItemId::Adt(parent.into())).0)
        .part(b"variant")
        .part(name.as_str().as_bytes());
    b
}

#[salsa::tracked(returns(ref))]
fn enum_variant_keys(db: &dyn HirDatabase, enum_: EnumId) -> FxHashMap<EnumVariantId, ItemKey> {
    let entries = enum_
        .enum_variants(db)
        .variants
        .iter()
        .map(|(name, &(variant, _))| (variant, variant_preimage(db, enum_, name)))
        .collect();
    hash_all(entries).collect()
}

fn variant_id_key(db: &dyn HirDatabase, variant: VariantId) -> ItemKey {
    match variant {
        VariantId::EnumVariantId(it) => variant_key(db, it),
        VariantId::StructId(it) => item_key(db, ItemId::Adt(it.into())),
        VariantId::UnionId(it) => item_key(db, ItemId::Adt(it.into())),
    }
}

pub(crate) fn generic_def_item(def: GenericDefId) -> ItemId {
    match def {
        GenericDefId::AdtId(it) => ItemId::Adt(it),
        GenericDefId::ConstId(it) => ItemId::Const(it),
        GenericDefId::FunctionId(it) => ItemId::Function(it),
        GenericDefId::ImplId(it) => ItemId::Impl(it),
        GenericDefId::StaticId(it) => ItemId::Static(it),
        GenericDefId::TraitId(it) => ItemId::Trait(it),
        GenericDefId::TypeAliasId(it) => ItemId::Alias(it),
    }
}

/// The key of an expression store: of a signature, a body or the fields of a variant.
pub fn store_key(db: &dyn HirDatabase, store: ExpressionStoreOwnerId) -> ItemKey {
    let mut h = Hasher::new(KEY_DOMAIN);
    match store {
        ExpressionStoreOwnerId::Signature(def) => {
            h.id(&item_key(db, generic_def_item(def)).0).part(b"signature")
        }
        ExpressionStoreOwnerId::Body(DefWithBodyId::VariantId(it)) => {
            h.id(&variant_key(db, it).0).part(b"discriminant")
        }
        ExpressionStoreOwnerId::Body(body) => {
            h.id(&item_key(db, ItemId::of_body(db, body)).0).part(b"body")
        }
        ExpressionStoreOwnerId::VariantFields(it) => {
            h.id(&variant_id_key(db, it).0).part(b"fields")
        }
    };
    ItemKey(h.finish())
}

/// The key of an anonymous constant (an array length, a const argument): its store's and its
/// root expression's index there.
pub fn anon_const_key<'db>(db: &'db dyn HirDatabase, anon: AnonConstId<'db>) -> ItemKey {
    let loc = anon.loc(db);
    let mut h = Hasher::new(KEY_DOMAIN);
    h.id(&store_key(db, loc.owner).0)
        .part(b"anon const")
        .update(&loc.expr.into_raw().into_u32().to_le_bytes());
    ItemKey(h.finish())
}

fn closure_key_of(
    db: &dyn HirDatabase,
    store: ExpressionStoreOwnerId,
    tag: &'static [u8],
    nth: u32,
) -> ItemKey {
    let mut h = Hasher::new(KEY_DOMAIN);
    h.id(&store_key(db, store).0).part(tag).update(&nth.to_le_bytes());
    ItemKey(h.finish())
}

fn store_of<'db>(db: &'db dyn HirDatabase, owner: InferBodyId<'db>) -> ExpressionStoreOwnerId {
    owner.expression_store_owner(db)
}

/// The key of a closure: its store's and its place among the store's closures (an edit before
/// it in the body keeps it, as [`ClosureKey`] does).
pub fn closure_key<'db>(db: &'db dyn HirDatabase, closure: InternedClosureId<'db>) -> ItemKey {
    let loc = closure.loc(db);
    closure_key_of(db, store_of(db, loc.owner), b"closure", ClosureKey::nth(db, loc))
}

pub fn coroutine_key<'db>(db: &'db dyn HirDatabase, coroutine: InternedCoroutineId<'db>) -> ItemKey {
    let loc = coroutine.loc(db);
    closure_key_of(db, store_of(db, loc.owner), b"coroutine", ClosureKey::nth(db, loc))
}

pub fn coroutine_closure_key<'db>(
    db: &'db dyn HirDatabase,
    closure: InternedCoroutineClosureId<'db>,
) -> ItemKey {
    let loc = closure.loc(db);
    closure_key_of(db, store_of(db, loc.owner), b"coroutine closure", ClosureKey::nth(db, loc))
}

/// The key of the `index`th opaque type of `owner`: a function's return type or a type
/// alias's type.
pub fn opaque_key(db: &dyn HirDatabase, owner: ItemId, index: u32) -> ItemKey {
    let mut h = Hasher::new(KEY_DOMAIN);
    h.id(&item_key(db, owner).0).part(b"opaque").update(&index.to_le_bytes());
    ItemKey(h.finish())
}

fn opaque_of<'db>(db: &'db dyn HirDatabase, opaque: InternedOpaqueTyId<'db>) -> (ItemId, u32) {
    match opaque.loc(db) {
        ImplTraitId::ReturnTypeImplTrait(f, index) => {
            (ItemId::Function(f), index.into_raw().into_u32())
        }
        ImplTraitId::TypeAliasImplTrait(a, index) => {
            (ItemId::Alias(a), index.into_raw().into_u32())
        }
    }
}

pub fn opaque_ty_key<'db>(db: &'db dyn HirDatabase, opaque: InternedOpaqueTyId<'db>) -> ItemKey {
    let (owner, index) = opaque_of(db, opaque);
    opaque_key(db, owner, index)
}

/// The key of an impl a built-in derive writes: its type's, the trait and which derive.
pub fn builtin_derive_key(db: &dyn HirDatabase, id: BuiltinDeriveImplId) -> ItemKey {
    let loc = id.loc(db);
    let mut h = Hasher::new(KEY_DOMAIN);
    h.id(&item_key(db, ItemId::Adt(loc.adt)).0)
        .part(b"builtin derive")
        .part(loc.trait_.name().as_str().as_bytes())
        .update(&loc.derive_attr_id.item_tree_index().to_le_bytes())
        .update(&loc.derive_index.to_le_bytes());
    ItemKey(h.finish())
}

/// The key of anything the solver names by a `SolverDefId`.
pub fn def_key<'db>(db: &'db dyn HirDatabase, def: SolverDefId<'db>) -> ItemKey {
    match def {
        SolverDefId::AdtId(it) => item_key(db, ItemId::Adt(it)),
        SolverDefId::ConstId(it) => item_key(db, ItemId::Const(it)),
        SolverDefId::FunctionId(it) => item_key(db, ItemId::Function(it)),
        SolverDefId::ImplId(it) => item_key(db, ItemId::Impl(it)),
        SolverDefId::BuiltinDeriveImplId(it) => builtin_derive_key(db, it),
        SolverDefId::StaticId(it) => item_key(db, ItemId::Static(it)),
        SolverDefId::AnonConstId(it) => anon_const_key(db, it),
        SolverDefId::TraitId(it) => item_key(db, ItemId::Trait(it)),
        SolverDefId::TypeAliasId(it) => item_key(db, ItemId::Alias(it)),
        SolverDefId::InternedClosureId(it) => closure_key(db, it),
        SolverDefId::InternedCoroutineId(it) => coroutine_key(db, it),
        SolverDefId::InternedCoroutineClosureId(it) => coroutine_closure_key(db, it),
        SolverDefId::InternedOpaqueTyId(it) => opaque_ty_key(db, it),
        SolverDefId::EnumVariantId(it) => variant_key(db, it),
        SolverDefId::Ctor(ctor) => {
            let of = match ctor {
                hir_ty::next_solver::Ctor::Struct(it) => item_key(db, ItemId::Adt(it.into())),
                hir_ty::next_solver::Ctor::Enum(it) => variant_key(db, it),
            };
            let mut h = Hasher::new(KEY_DOMAIN);
            h.id(&of.0).part(b"ctor");
            ItemKey(h.finish())
        }
    }
}

pub fn name_hash(name: &str) -> NameHash {
    NameHash(ContentId::of(NAME_DOMAIN, name.as_bytes()))
}

/// A simplified self type ([`crate::ImplIndexKey`]), with its def named by key.
pub fn simplified<'db>(
    db: &'db dyn HirDatabase,
    ty: &hir_ty::next_solver::SimplifiedType<'db>,
) -> SimplifiedType {
    use rustc_type_ir::fast_reject::SimplifiedType as S;
    let mut h = Hasher::new(SIMPLIFIED_DOMAIN);
    match *ty {
        S::Bool => h.update(&[0]),
        S::Char => h.update(&[1]),
        S::Int(it) => h.update(&[2]).part(it.name_str().as_bytes()),
        S::Uint(it) => h.update(&[3]).part(it.name_str().as_bytes()),
        S::Float(it) => h.update(&[4]).part(it.name_str().as_bytes()),
        S::Adt(def) => h.update(&[5]).id(&def_key(db, def).0),
        S::Foreign(def) => h.update(&[6]).id(&def_key(db, def).0),
        S::Str => h.update(&[7]),
        S::Array => h.update(&[8]),
        S::Slice => h.update(&[9]),
        S::Ref(m) => h.update(&[10, m.is_mut() as u8]),
        S::Ptr(m) => h.update(&[11, m.is_mut() as u8]),
        S::Never => h.update(&[12]),
        S::Tuple(n) => h.update(&[13]).update(&(n as u64).to_le_bytes()),
        S::MarkerTraitObject => h.update(&[14]),
        S::Trait(def) => h.update(&[15]).id(&def_key(db, def).0),
        S::Closure(def) => h.update(&[16]).id(&def_key(db, def).0),
        S::Coroutine(def) => h.update(&[17]).id(&def_key(db, def).0),
        S::CoroutineWitness(def) => h.update(&[18]).id(&def_key(db, def).0),
        S::Function(n) => h.update(&[19]).update(&(n as u64).to_le_bytes()),
        S::UnsafeBinder => h.update(&[20]),
        S::Placeholder => h.update(&[21]),
        S::Error => h.update(&[22]),
    };
    SimplifiedType(h.finish())
}

/// The path of an item as the program sees it (`type_name`, `module_path!`, a symbol's
/// demangled name): its crate's name, its modules, a `{block}` for each block it is in, and
/// its name; an impl's item under `<impl>`.
pub fn path(db: &dyn HirDatabase, id: ItemId) -> String {
    let mut out = String::new();
    let (module, container_segment) = match id {
        ItemId::Function(it) => assoc_container(db, it.lookup(db).container),
        ItemId::Const(it) => assoc_container(db, it.lookup(db).container),
        ItemId::Static(it) => assoc_container(db, it.lookup(db).container),
        ItemId::Alias(it) => assoc_container(db, it.lookup(db).container),
        other => (other.module(db), None),
    };
    module_path(db, module, &mut out);
    if let Some(segment) = container_segment {
        out.push_str("::");
        out.push_str(&segment);
    }
    out.push_str("::");
    out.push_str(&item_name(db, id).unwrap_or_else(|| "_".to_owned()));
    out
}

fn assoc_container(db: &dyn HirDatabase, container: ItemContainerId) -> (ModuleId, Option<String>) {
    match container {
        ItemContainerId::ModuleId(it) => (it, None),
        ItemContainerId::ExternBlockId(it) => (it.lookup(db).container, None),
        ItemContainerId::TraitId(it) => {
            (it.lookup(db).container, Some(TraitSignature::of(db, it).name.as_str().to_owned()))
        }
        ItemContainerId::ImplId(it) => (it.lookup(db).container, Some("<impl>".to_owned())),
    }
}

fn module_path<'db>(db: &'db dyn HirDatabase, module: ModuleIdLt<'db>, out: &mut String) {
    match module.containing_module(db) {
        Some(parent) => {
            module_path(db, parent, out);
            out.push_str("::");
            match module.name(db) {
                Some(name) => out.push_str(name.as_str()),
                None => out.push_str("{block}"),
            }
        }
        None => match &module.krate(db).extra_data(db).display_name {
            Some(name) => out.push_str(name.crate_name().symbol().as_str()),
            None => out.push_str("{crate}"),
        },
    }
}

fn item_name(db: &dyn HirDatabase, id: ItemId) -> Option<String> {
    Some(match id {
        ItemId::Function(it) => FunctionSignature::of(db, it).name.as_str().to_owned(),
        ItemId::Adt(AdtId::StructId(it)) => StructSignature::of(db, it).name.as_str().to_owned(),
        ItemId::Adt(AdtId::EnumId(it)) => EnumSignature::of(db, it).name.as_str().to_owned(),
        ItemId::Adt(AdtId::UnionId(it)) => UnionSignature::of(db, it).name.as_str().to_owned(),
        ItemId::Trait(it) => TraitSignature::of(db, it).name.as_str().to_owned(),
        ItemId::Impl(_) => "<impl>".to_owned(),
        ItemId::Const(it) => ConstSignature::of(db, it).name.as_ref()?.as_str().to_owned(),
        ItemId::Static(it) => StaticSignature::of(db, it).name.as_str().to_owned(),
        ItemId::Alias(it) => TypeAliasSignature::of(db, it).name.as_str().to_owned(),
        ItemId::Macro(MacroId::MacroRulesId(it)) => {
            it.lookup(db).id.to_node(db).name()?.text().to_string()
        }
        ItemId::Macro(MacroId::Macro2Id(it)) => it.lookup(db).id.to_node(db).name()?.text().to_string(),
        ItemId::Macro(MacroId::ProcMacroId(it)) => {
            it.lookup(db).id.to_node(db).name()?.text().to_string()
        }
    })
}

impl ItemId {
    pub fn module(self, db: &dyn HirDatabase) -> ModuleId {
        match self {
            ItemId::Function(it) => it.module(db),
            ItemId::Adt(it) => it.module(db),
            ItemId::Trait(it) => it.module(db),
            ItemId::Impl(it) => it.module(db),
            ItemId::Const(it) => it.module(db),
            ItemId::Static(it) => it.module(db),
            ItemId::Alias(it) => it.module(db),
            ItemId::Macro(it) => it.module(db),
        }
    }
}

/// The item of the current database a key names, if one still has it (see the module doc).
pub fn resolve(db: &dyn HirDatabase, key: ItemKey) -> Option<Resolved> {
    crate_keys_distinct(db);
    let krates = base_db::all_crates(db);
    for &krate in krates.iter() {
        if let Some(found) = crate_items(db, krate).index.get(&key) {
            return Some(*found);
        }
    }
    for &krate in krates.iter() {
        for &inner in &crate_items(db, krate).inner {
            if let Some(found) = inner_items(db, inner, ()).index.get(&key) {
                return Some(*found);
            }
        }
    }
    None
}

type Index = std::collections::HashMap<ItemKey, Resolved, KeyHashing>;

/// Hashing for maps keyed by a key, which is a BLAKE3 hash already: its words are folded into
/// the map's hash, which was computed once with the key and is carried, not hashed again.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub(crate) struct KeyHashing;

impl std::hash::BuildHasher for KeyHashing {
    type Hasher = KeyHasher;

    fn build_hasher(&self) -> KeyHasher {
        KeyHasher(0)
    }
}

pub(crate) struct KeyHasher(u64);

impl std::hash::Hasher for KeyHasher {
    fn finish(&self) -> u64 {
        self.0
    }

    // A `[u8; 32]` is hashed as its length and then its bytes: the length is the same for
    // every key, so only the bytes count.
    fn write_usize(&mut self, _: usize) {}

    fn write(&mut self, bytes: &[u8]) {
        for chunk in bytes.chunks(8) {
            let mut word = [0; 8];
            word[..chunk.len()].copy_from_slice(chunk);
            self.0 = self.0.rotate_left(29) ^ u64::from_le_bytes(word);
        }
    }
}

/// Stops if two crates of the build have one key: an item of one would be taken for the
/// other's.
#[salsa::tracked]
fn crate_keys_distinct(db: &dyn HirDatabase) {
    let mut seen: FxHashMap<ContentId, Crate> = FxHashMap::default();
    for &krate in base_db::all_crates(db).iter() {
        if let Some(other) = seen.insert(crate_key(db, krate), krate) {
            panic!(
                "blaze-items: crates {krate:?} and {other:?} have one key: the same name, \
                 version, origin, kind of target and cfg"
            );
        }
    }
}

/// Every item of `krate` with an [`ItemId`], those of its bodies' blocks included.
pub(crate) fn crate_item_ids(db: &dyn HirDatabase, krate: Crate) -> Vec<ItemId> {
    let items = crate_items(db, krate);
    let mut ids: Vec<ItemId> = Vec::new();
    let mut take = |items: &Items| {
        ids.extend(items.index.values().filter_map(|it| match *it {
            Resolved::Item(id) => Some(id),
            _ => None,
        }))
    };
    take(items);
    for &inner in &items.inner {
        take(inner_items(db, inner, ()));
    }
    ids
}

/// Where else an item has things with keys: a store, with its closures and the items of its
/// blocks, or the opaque types of an item's type, which are counted by lowering it.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Inner {
    Store(ExpressionStoreOwnerId),
    Opaques(ItemId),
}

#[derive(PartialEq, Eq, Debug, Default)]
struct Items {
    index: Index,
    inner: Vec<Inner>,
}

impl Items {
    fn add(&mut self, key: ItemKey, resolved: Resolved) {
        if let Some(old) = self.index.insert(key, resolved) {
            assert_eq!(
                old, resolved,
                "blaze-items: two items with one key {key:?}: {old:?} and {resolved:?}"
            );
        }
    }

    fn merge(&mut self, other: &Items) {
        for (&key, &resolved) in &other.index {
            self.add(key, resolved);
        }
    }
}

/// The items of a crate's modules, impls and traits, and where to look for the rest.
#[salsa::tracked(returns(ref))]
fn crate_items(db: &dyn HirDatabase, krate: Crate) -> Items {
    let mut items = Items::default();
    def_map_items(db, hir_def::nameres::crate_def_map(db, krate), &mut items);
    items
}

/// What has a key inside one store or one item's type, transitively: the items of a block
/// have stores of their own.
// The unit makes the arguments something salsa interns: `Inner` is no salsa struct.
#[salsa::tracked(returns(ref))]
fn inner_items(db: &dyn HirDatabase, inner: Inner, _: ()) -> Items {
    let mut items = Items::default();
    match inner {
        Inner::Opaques(owner) => {
            let count = match owner {
                ItemId::Function(it) => SelfAndAssocBounds::return_type_impl_traits(db, it).len(),
                ItemId::Alias(it) => SelfAndAssocBounds::type_alias_opaque_count(db, it),
                _ => 0,
            };
            for index in 0..count as u32 {
                items.add(opaque_key(db, owner, index), Resolved::Opaque { owner, index });
            }
        }
        Inner::Store(store) => {
            let exprs = ExpressionStore::of(db, store);
            let mut nth = 0u32;
            for (expr, it) in exprs.exprs() {
                let Expr::Closure { closure_kind, .. } = it else { continue };
                let kind = *closure_kind;
                let resolved = Resolved::Closure { store, expr, kind };
                match kind {
                    ClosureKind::Closure => {
                        items.add(closure_key_of(db, store, b"closure", nth), resolved)
                    }
                    ClosureKind::CoroutineClosure(_) => {
                        items.add(closure_key_of(db, store, b"closure", nth), resolved);
                        items.add(closure_key_of(db, store, b"coroutine closure", nth), resolved);
                    }
                    ClosureKind::OldCoroutine(_) | ClosureKind::Coroutine { .. } => {
                        items.add(closure_key_of(db, store, b"coroutine", nth), resolved)
                    }
                }
                nth += 1;
            }
            let mut blocks = Items::default();
            for (_, block) in exprs.blocks(db) {
                def_map_items(db, block, &mut blocks);
            }
            for &inner in &blocks.inner {
                items.merge(inner_items(db, inner, ()));
            }
            items.merge(&blocks);
        }
    }
    items
}

/// The items of a def map's modules, and where to look inside them.
fn def_map_items(db: &dyn HirDatabase, def_map: &DefMap, items: &mut Items) {
    for (module, data) in def_map.modules(db) {
        items.add(module_key(db, module), Resolved::Module(module));
        let scope = data.scope(db);
        for def in scope.declarations() {
            match def {
                ModuleDefId::FunctionId(it) => item(db, ItemId::Function(it), items),
                ModuleDefId::AdtId(it) => item(db, ItemId::Adt(it), items),
                ModuleDefId::ConstId(it) => item(db, ItemId::Const(it), items),
                ModuleDefId::StaticId(it) => item(db, ItemId::Static(it), items),
                ModuleDefId::TraitId(it) => {
                    item(db, ItemId::Trait(it), items);
                    for &(_, assoc) in &it.trait_items(db).items {
                        item(db, assoc.into(), items);
                    }
                }
                ModuleDefId::TypeAliasId(it) => item(db, ItemId::Alias(it), items),
                ModuleDefId::MacroId(it) => item(db, ItemId::Macro(it), items),
                ModuleDefId::EnumVariantId(_)
                | ModuleDefId::ModuleId(_)
                | ModuleDefId::BuiltinType(_) => {}
            }
        }
        for it in scope.impls() {
            item(db, ItemId::Impl(it), items);
            for &(_, assoc) in &it.impl_items(db).items {
                item(db, assoc.into(), items);
            }
        }
        for it in scope.unnamed_consts() {
            item(db, ItemId::Const(it), items);
        }
        for (_, macros) in scope.legacy_macros() {
            for &it in macros {
                if it.module(db) == module {
                    item(db, ItemId::Macro(it), items);
                }
            }
        }
        for it in scope.builtin_derive_impls() {
            items.add(builtin_derive_key(db, it), Resolved::BuiltinDerive(it));
        }
    }
}

/// An item, its enum's variants, and where to look inside it.
fn item(db: &dyn HirDatabase, id: ItemId, items: &mut Items) {
    items.add(item_key(db, id), Resolved::Item(id));
    if let Some(def) = id.generic_def() {
        items.inner.push(Inner::Store(ExpressionStoreOwnerId::Signature(def)));
    }
    if let Some(body) = id.body() {
        items.inner.push(Inner::Store(ExpressionStoreOwnerId::Body(body)));
    }
    match id {
        ItemId::Function(_) | ItemId::Alias(_) => items.inner.push(Inner::Opaques(id)),
        ItemId::Adt(AdtId::EnumId(it)) => {
            for (_, &(variant, _)) in &it.enum_variants(db).variants {
                items.add(variant_key(db, variant), Resolved::Variant(variant));
                items.inner.push(Inner::Store(ExpressionStoreOwnerId::Body(variant.into())));
                items.inner.push(Inner::Store(ExpressionStoreOwnerId::VariantFields(variant.into())));
            }
        }
        ItemId::Adt(AdtId::StructId(it)) => {
            items.inner.push(Inner::Store(ExpressionStoreOwnerId::VariantFields(it.into())))
        }
        ItemId::Adt(AdtId::UnionId(it)) => {
            items.inner.push(Inner::Store(ExpressionStoreOwnerId::VariantFields(it.into())))
        }
        _ => {}
    }
}
