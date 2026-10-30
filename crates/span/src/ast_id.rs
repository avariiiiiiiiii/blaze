//! `AstIdMap` allows to create stable IDs for "large" syntax nodes like items
//! and macro calls.
//!
//! Specifically, it enumerates all items in a file and uses the position of an
//! item as an ID. That way, IDs don't change unless the set of items itself
//! changes.
//!
//! These IDs are tricky. If one of them invalidates, its interned ID invalidates,
//! and this can cause *a lot* to be recomputed. For example, if you invalidate the ID
//! of a struct, and that struct has an impl (any impl!) this will cause the `Self`
//! type of the impl to invalidate, which will cause the all impls queries to be
//! invalidated, which will cause every trait solve query in this crate *and* all
//! transitive reverse dependencies to be invalidated, which is pretty much the worst
//! thing that can happen incrementality wise.
//!
//! So we want these IDs to stay as stable as possible. For top-level items, we store
//! their kind and name, which should be unique, but since they can still not be, we
//! also store an index disambiguator. For nested items, we also store the ID of their
//! parent. For macro calls, we store the macro name and an index. There aren't usually
//! a lot of macro calls in item position, and invalidation in bodies is not much of
//! a problem, so this should be enough.
//!
//! What is in a fn's body (blocks with items, items in blocks, macro calls, `asm!`) is
//! numbered apart from the rest of the file, by a walk of that body alone, when the body is
//! first asked about: a dependency's file is parsed as an outline whose bodies are parsed
//! only when needed (#1294), and its map must not parse them. A body's ids are in a space of
//! their own (the body bit of [`ErasedFileAstId`]): the hash bits name the body, by slots
//! the outline gives each body from its owner's id, and the index is the node's number among
//! the nodes of its kind in the body. So a body's ids depend on its owner's id and slots and
//! on its own text only, and an outline's map and a full parse's map give the same ids.

use std::{
    any::type_name,
    fmt,
    hash::{BuildHasher, Hash, Hasher},
    marker::PhantomData,
    sync::{Arc, OnceLock},
};

use la_arena::{Arena, Idx, RawIdx};
use rustc_hash::{FxBuildHasher, FxHashMap};
use syntax::{
    AstNode, AstPtr, FlatTree, SyntaxKind, SyntaxNode, SyntaxNodePtr, TextRange,
    ast::{self, HasName},
    match_ast,
};

// The first index is always the root node's AstId
/// The root ast id always points to the encompassing file, using this in spans is discouraged as
/// any range relative to it will be effectively absolute, ruining the entire point of anchored
/// relative text ranges.
pub const ROOT_ERASED_FILE_AST_ID: ErasedFileAstId =
    ErasedFileAstId(pack(0, 0, false, ErasedFileAstIdKind::Root as u32));

/// ErasedFileAstId used as the span for syntax node fixups. Any Span containing this file id is to be
/// considered fake.
/// Do not modify this, it is used by the proc-macro server.
pub const FIXUP_ERASED_FILE_AST_ID_MARKER: ErasedFileAstId =
    ErasedFileAstId(pack(0, 0, false, ErasedFileAstIdKind::Fixup as u32));

/// [`ErasedFileAstId`] used as the span for syntax nodes that should not be mapped down to
/// macro expansion. Any `Span` containing this file id is to be considered fake.
pub const NO_DOWNMAP_ERASED_FILE_AST_ID_MARKER: ErasedFileAstId =
    ErasedFileAstId(pack(0, 0, false, ErasedFileAstIdKind::NoDownmap as u32));

/// This is a type erased FileAstId.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct ErasedFileAstId(u32);

impl fmt::Debug for ErasedFileAstId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kind =
            self.kind_typed().map_or_else(|| "Unknown".to_owned(), |kind| format!("{kind:?}"));
        if f.alternate() {
            match self.in_body() {
                false => write!(f, "{kind}[{:04X}, {}]", self.hash_value(), self.index()),
                true => write!(f, "{kind}[{:04X}, {}, body]", self.hash_value(), self.index()),
            }
        } else {
            let mut f = f.debug_struct("ErasedFileAstId");
            f.field("kind", &kind)
                .field("index", &self.index())
                .field("hash", &format_args!("{:04X}", self.hash_value()));
            if self.in_body() {
                f.field("in_body", &true);
            }
            f.finish()
        }
    }
}

#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq)]
#[repr(u8)]
enum ErasedFileAstIdKind {
    /// This needs to not change because it's depended upon by the proc macro server.
    Fixup = 0,
    // The following are associated with `ErasedHasNameFileAstId`.
    Enum,
    Struct,
    Union,
    ExternCrate,
    MacroDef,
    MacroRules,
    Module,
    Static,
    Trait,
    // Until here associated with `ErasedHasNameFileAstId`.
    // The following are associated with `ErasedAssocItemFileAstId`.
    Variant,
    Const,
    Fn,
    MacroCall,
    TypeAlias,
    // Until here associated with `ErasedAssocItemFileAstId`.
    // Extern blocks don't really have any identifying property unfortunately.
    ExternBlock,
    // FIXME: If we store the final `UseTree` instead of the top-level `Use`, we can store its name,
    // and be way more granular for incrementality, at the expense of increased memory usage.
    // Use IDs aren't used a lot. The main thing that stores them is the def map. So everything that
    // uses the def map will be invalidated. That includes infers, and so is pretty bad, but our
    // def map incrementality story is pretty bad anyway and needs to be improved (see
    // https://rust-lang.zulipchat.com/#narrow/channel/185405-t-compiler.2Frust-analyzer/topic/.60infer.60.20queries.20and.20splitting.20.60DefMap.60).
    // So I left this as-is for now, as the def map improvement should also mitigate this.
    Use,
    /// Associated with [`ImplFileAstId`].
    Impl,
    /// Associated with [`BlockExprFileAstId`].
    BlockExpr,
    // `global_asm!()` is an item, so we need to give it an `AstId`. So we give to all inline asm
    // because incrementality is not a problem, they will always be the only item in the macro file,
    // and memory usage also not because they're rare.
    AsmExpr,
    /// Represents a fake [`ErasedFileAstId`] that should not be mapped down to macro expansion
    /// result.
    NoDownmap,
    /// Keep this last.
    Root,
}

impl ErasedFileAstIdKind {
    fn syntax_kind(self) -> Option<SyntaxKind> {
        match self {
            ErasedFileAstIdKind::Enum => Some(SyntaxKind::ENUM),
            ErasedFileAstIdKind::Struct => Some(SyntaxKind::STRUCT),
            ErasedFileAstIdKind::Union => Some(SyntaxKind::UNION),
            ErasedFileAstIdKind::ExternCrate => Some(SyntaxKind::EXTERN_CRATE),
            ErasedFileAstIdKind::MacroDef => Some(SyntaxKind::MACRO_DEF),
            ErasedFileAstIdKind::MacroRules => Some(SyntaxKind::MACRO_RULES),
            ErasedFileAstIdKind::Module => Some(SyntaxKind::MODULE),
            ErasedFileAstIdKind::Static => Some(SyntaxKind::STATIC),
            ErasedFileAstIdKind::Trait => Some(SyntaxKind::TRAIT),
            ErasedFileAstIdKind::Variant => Some(SyntaxKind::VARIANT),
            ErasedFileAstIdKind::Const => Some(SyntaxKind::CONST),
            ErasedFileAstIdKind::Fn => Some(SyntaxKind::FN),
            ErasedFileAstIdKind::MacroCall => Some(SyntaxKind::MACRO_CALL),
            ErasedFileAstIdKind::TypeAlias => Some(SyntaxKind::TYPE_ALIAS),
            ErasedFileAstIdKind::ExternBlock => Some(SyntaxKind::EXTERN_BLOCK),
            ErasedFileAstIdKind::Use => Some(SyntaxKind::USE),
            ErasedFileAstIdKind::Impl => Some(SyntaxKind::IMPL),
            ErasedFileAstIdKind::BlockExpr => Some(SyntaxKind::BLOCK_EXPR),
            ErasedFileAstIdKind::AsmExpr => Some(SyntaxKind::ASM_EXPR),
            ErasedFileAstIdKind::Fixup
            | ErasedFileAstIdKind::NoDownmap
            | ErasedFileAstIdKind::Root => None,
        }
    }
}

// First hash, then index, then whether the node is in a fn body, then kind. The kind stays
// at the same bits as before the body bit, so the markers above keep their values.
const HASH_BITS: u32 = 16;
const INDEX_BITS: u32 = 10;
const BODY_BITS: u32 = 1;
const KIND_BITS: u32 = 5;
const KIND_SHIFT: u32 = HASH_BITS + INDEX_BITS + BODY_BITS;
const INDEX_MASK: u32 = (1 << INDEX_BITS) - 1;
/// How many hash values there are to name bodies by.
const SLOTS: u32 = 1 << HASH_BITS;
const _: () = assert!(ErasedFileAstIdKind::Root as u32 <= ((1 << KIND_BITS) - 1));
const _: () = assert!(HASH_BITS + INDEX_BITS + BODY_BITS + KIND_BITS == u32::BITS);
const _: () = assert!(KIND_SHIFT == 27);

#[inline]
const fn u16_hash(hash: u64) -> u16 {
    // We do basically the same as `FxHasher`. We don't use rustc-hash and truncate because the
    // higher bits have more entropy, but unlike rustc-hash we don't rotate because it rotates
    // for hashmaps that just use the low bits, but we compare all bits.
    const K: u16 = 0xecc5;
    let (part1, part2, part3, part4) =
        (hash as u16, (hash >> 16) as u16, (hash >> 32) as u16, (hash >> 48) as u16);
    part1
        .wrapping_add(part2)
        .wrapping_mul(K)
        .wrapping_add(part3)
        .wrapping_mul(K)
        .wrapping_add(part4)
        .wrapping_mul(K)
}

#[inline]
const fn pack(hash: u16, index: u32, in_body: bool, kind: u32) -> u32 {
    (hash as u32)
        | (index << HASH_BITS)
        | ((in_body as u32) << (HASH_BITS + INDEX_BITS))
        | (kind << KIND_SHIFT)
}

impl ErasedFileAstId {
    #[inline]
    fn hash_value(self) -> u16 {
        self.0 as u16
    }

    #[inline]
    fn index(self) -> u32 {
        (self.0 >> HASH_BITS) & INDEX_MASK
    }

    /// Whether the node is in a fn body, numbered by a walk of that body alone.
    #[inline]
    fn in_body(self) -> bool {
        (self.0 >> (HASH_BITS + INDEX_BITS)) & 1 != 0
    }

    #[inline]
    fn kind(self) -> u32 {
        self.0 >> KIND_SHIFT
    }

    fn kind_typed(self) -> Option<ErasedFileAstIdKind> {
        let kind = self.kind();
        macro_rules! kind {
            ($($kind:ident),* $(,)?) => {
                if false {
                    // Ensure we covered all variants.
                    match ErasedFileAstIdKind::Root {
                        $( ErasedFileAstIdKind::$kind => {} )*
                    }
                    unreachable!()
                }
                $( else if kind == ErasedFileAstIdKind::$kind as u32 {
                    Some(ErasedFileAstIdKind::$kind)
                } )*
                else {
                    None
                }
            };
        }
        kind!(
            Root,
            Enum,
            Struct,
            Union,
            ExternCrate,
            MacroDef,
            MacroRules,
            Module,
            Static,
            Trait,
            Variant,
            Const,
            Fn,
            MacroCall,
            TypeAlias,
            ExternBlock,
            Use,
            Impl,
            BlockExpr,
            AsmExpr,
            Fixup,
            NoDownmap,
        )
    }

    #[inline]
    pub fn is_root(self) -> bool {
        self.kind() == ErasedFileAstIdKind::Root as u32
    }

    fn ast_id_for(
        node: &SyntaxNode,
        index_map: &mut ErasedAstIdNextIndexMap,
        parent: Option<&ErasedFileAstId>,
    ) -> Option<ErasedFileAstId> {
        // Blocks are deliberately not here - we only want to allocate a block if it contains items.
        has_name_ast_id(node, index_map)
            .or_else(|| assoc_item_ast_id(node, index_map, parent))
            .or_else(|| extern_block_ast_id(node, index_map))
            .or_else(|| use_ast_id(node, index_map))
            .or_else(|| impl_ast_id(node, index_map))
            .or_else(|| asm_expr_ast_id(node, index_map))
    }

    fn should_alloc(kind: SyntaxKind) -> Option<ErasedFileAstIdKind> {
        should_alloc_has_name(kind)
            .or_else(|| should_alloc_assoc_item(kind))
            .or_else(|| {
                ast::ExternBlock::can_cast(kind).then_some(ErasedFileAstIdKind::ExternBlock)
            })
            .or_else(|| ast::Use::can_cast(kind).then_some(ErasedFileAstIdKind::Use))
            .or_else(|| ast::Impl::can_cast(kind).then_some(ErasedFileAstIdKind::Impl))
            .or_else(|| ast::AsmExpr::can_cast(kind).then_some(ErasedFileAstIdKind::AsmExpr))
    }

    #[inline]
    pub fn into_raw(self) -> u32 {
        self.0
    }

    #[inline]
    pub const fn from_raw(v: u32) -> Self {
        Self(v)
    }
}

pub trait AstIdNode: AstNode {}

/// `AstId` points to an AST node in a specific file.
pub struct FileAstId<N> {
    raw: ErasedFileAstId,
    _marker: PhantomData<fn() -> N>,
}

/// Traits are manually implemented because `derive` adds redundant bounds.
impl<N> Clone for FileAstId<N> {
    #[inline]
    fn clone(&self) -> FileAstId<N> {
        *self
    }
}
impl<N> Copy for FileAstId<N> {}

impl<N> PartialEq for FileAstId<N> {
    fn eq(&self, other: &Self) -> bool {
        self.raw == other.raw
    }
}
impl<N> Eq for FileAstId<N> {}
impl<N> Hash for FileAstId<N> {
    fn hash<H: Hasher>(&self, hasher: &mut H) {
        self.raw.hash(hasher);
    }
}

impl<N> fmt::Debug for FileAstId<N> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "FileAstId::<{}>({:?})", type_name::<N>(), self.raw)
    }
}

impl<N> FileAstId<N> {
    /// The id `raw` is, read back as an id of an `N`: `raw` must have been one.
    #[inline]
    pub fn from_erased_unchecked(raw: ErasedFileAstId) -> Self {
        FileAstId { raw, _marker: PhantomData }
    }

    // Can't make this a From implementation because of coherence
    #[inline]
    pub fn upcast<M: AstIdNode>(self) -> FileAstId<M>
    where
        N: Into<M>,
    {
        FileAstId { raw: self.raw, _marker: PhantomData }
    }

    #[inline]
    pub fn downcast_unchecked<M: AstIdNode + Into<N>>(self) -> FileAstId<M> {
        debug_assert!(
            self.erase()
                .kind_typed()
                .and_then(|kind| kind.syntax_kind())
                .is_some_and(|kind| M::can_cast(kind)),
            "`FileAstId::<{}>::downcast_unchecked::<{}>()` is invalid",
            std::any::type_name::<N>(),
            std::any::type_name::<M>(),
        );
        FileAstId { raw: self.raw, _marker: PhantomData }
    }

    #[inline]
    pub fn erase(self) -> ErasedFileAstId {
        self.raw
    }
}

#[derive(Hash)]
struct ErasedHasNameFileAstId<'a> {
    name: &'a str,
}

/// This holds the ast ID for variants too (they're a kind of assoc item).
#[derive(Hash)]
struct ErasedAssocItemFileAstId<'a> {
    /// Subtle: items in `extern` blocks **do not** store the ID of the extern block here.
    /// Instead this is left empty. The reason is that `ExternBlockFileAstId` is pretty unstable
    /// (it contains only an index), and extern blocks don't introduce a new scope, so storing
    /// the extern block ID will do more harm to incrementality than help.
    parent: Option<ErasedFileAstId>,
    properties: ErasedHasNameFileAstId<'a>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ImplFileAstId<'a> {
    /// This can be `None` if the `Self` type is not a named type, or if it is inside a macro call.
    self_ty_name: Option<&'a str>,
    /// This can be `None` if this is an inherent impl, or if the trait name is inside a macro call.
    trait_name: Option<&'a str>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct BlockExprFileAstId {
    parent: Option<ErasedFileAstId>,
}

impl AstIdNode for ast::ExternBlock {}

fn extern_block_ast_id(
    node: &SyntaxNode,
    index_map: &mut ErasedAstIdNextIndexMap,
) -> Option<ErasedFileAstId> {
    if ast::ExternBlock::can_cast(node.kind()) {
        Some(index_map.new_id(ErasedFileAstIdKind::ExternBlock, ()))
    } else {
        None
    }
}

impl AstIdNode for ast::Use {}

fn use_ast_id(
    node: &SyntaxNode,
    index_map: &mut ErasedAstIdNextIndexMap,
) -> Option<ErasedFileAstId> {
    if ast::Use::can_cast(node.kind()) {
        Some(index_map.new_id(ErasedFileAstIdKind::Use, ()))
    } else {
        None
    }
}

impl AstIdNode for ast::AsmExpr {}

fn asm_expr_ast_id(
    node: &SyntaxNode,
    index_map: &mut ErasedAstIdNextIndexMap,
) -> Option<ErasedFileAstId> {
    if ast::AsmExpr::can_cast(node.kind()) {
        Some(index_map.new_id(ErasedFileAstIdKind::AsmExpr, ()))
    } else {
        None
    }
}

impl AstIdNode for ast::Impl {}

fn impl_ast_id(
    node: &SyntaxNode,
    index_map: &mut ErasedAstIdNextIndexMap,
) -> Option<ErasedFileAstId> {
    if let Some(node) = ast::Impl::cast(node.clone()) {
        let type_as_name = |ty: Option<ast::Type>| match ty? {
            ast::Type::PathType(it) => Some(it.path()?.segment()?.name_ref()?),
            _ => None,
        };
        let self_ty_name = type_as_name(node.self_ty());
        let trait_name = type_as_name(node.trait_());
        let data = ImplFileAstId {
            self_ty_name: self_ty_name.as_ref().map(|it| it.text()),
            trait_name: trait_name.as_ref().map(|it| it.text()),
        };
        Some(index_map.new_id(ErasedFileAstIdKind::Impl, data))
    } else {
        None
    }
}

// Blocks aren't `AstIdNode`s deliberately, because unlike other nodes, not all blocks get their own
// ast id, only if they have items. To account for that we have a different, fallible, API for blocks.
// impl !AstIdNode for ast::BlockExpr {}

fn block_expr_ast_id(
    kind: SyntaxKind,
    index_map: &mut ErasedAstIdNextIndexMap,
    parent: Option<&ErasedFileAstId>,
) -> Option<ErasedFileAstId> {
    if ast::BlockExpr::can_cast(kind) {
        Some(
            index_map.new_id(
                ErasedFileAstIdKind::BlockExpr,
                BlockExprFileAstId { parent: parent.copied() },
            ),
        )
    } else {
        None
    }
}

#[derive(Default)]
struct ErasedAstIdNextIndexMap(FxHashMap<(ErasedFileAstIdKind, u16), u32>);

impl ErasedAstIdNextIndexMap {
    #[inline]
    fn new_id(&mut self, kind: ErasedFileAstIdKind, data: impl Hash) -> ErasedFileAstId {
        let hash = FxBuildHasher.hash_one(&data);
        let initial_hash = u16_hash(hash);
        // Even though 2^INDEX_BITS=1024 items with the same hash seems like a lot,
        // it could happen with macro calls or `use`s in macro-generated files. So we want
        // to handle it gracefully. We just increment the hash.
        let mut hash = initial_hash;
        let index = loop {
            match self.0.entry((kind, hash)) {
                std::collections::hash_map::Entry::Occupied(mut entry) => {
                    let i = entry.get_mut();
                    if *i < ((1 << INDEX_BITS) - 1) {
                        *i += 1;
                        break *i;
                    }
                }
                std::collections::hash_map::Entry::Vacant(entry) => {
                    entry.insert(0);
                    break 0;
                }
            }
            hash = hash.wrapping_add(1);
            if hash == initial_hash {
                // That's 2^26=67,108,864 items!
                panic!("you have way too many items in the same file!");
            }
        };
        let kind = kind as u32;
        ErasedFileAstId(pack(hash, index, false, kind))
    }
}

macro_rules! register_enum_ast_id {
    (impl $AstIdNode:ident for $($ident:ident),+ ) => {
        $(
            impl $AstIdNode for ast::$ident {}
        )+
    };
}
register_enum_ast_id! {
    impl AstIdNode for
    Item, AnyHasGenericParams, Adt, Macro,
    AssocItem
}

macro_rules! register_has_name_ast_id {
    (impl $AstIdNode:ident for $($ident:ident = $name_method:ident),+ ) => {
        $(
            impl $AstIdNode for ast::$ident {}
        )+

        fn has_name_ast_id(node: &SyntaxNode, index_map: &mut ErasedAstIdNextIndexMap) -> Option<ErasedFileAstId> {
            match_ast! {
                match node {
                    $(
                        ast::$ident(node) => {
                            let name = node.$name_method();
                            let name = name.as_ref().map_or("", |it| it.text());
                            let result = ErasedHasNameFileAstId {
                                name,
                            };
                            Some(index_map.new_id(ErasedFileAstIdKind::$ident, result))
                        },
                    )*
                    _ => None,
                }
            }
        }

        fn should_alloc_has_name(kind: SyntaxKind) -> Option<ErasedFileAstIdKind> {
            $( if ast::$ident::can_cast(kind) { Some(ErasedFileAstIdKind::$ident) } else )* { None }
        }
    };
}
register_has_name_ast_id! {
    impl AstIdNode for
        Enum = name,
        Struct = name,
        Union = name,
        ExternCrate = name_ref,
        MacroDef = name,
        MacroRules = name,
        Module = name,
        Static = name,
        Trait = name
}

macro_rules! register_assoc_item_ast_id {
    (impl $AstIdNode:ident for $($ident:ident = $name_callback:expr),+ ) => {
        $(
            impl $AstIdNode for ast::$ident {}
        )+

        fn assoc_item_ast_id(
            node: &SyntaxNode,
            index_map: &mut ErasedAstIdNextIndexMap,
            parent: Option<&ErasedFileAstId>,
        ) -> Option<ErasedFileAstId> {
            match_ast! {
                match node {
                    $(
                        ast::$ident(node) => {
                            let name = $name_callback(node);
                            let name = name.as_ref().map_or("", |it| it.text());
                            let properties = ErasedHasNameFileAstId {
                                name,
                            };
                            let result = ErasedAssocItemFileAstId {
                                parent: parent.copied(),
                                properties,
                            };
                            Some(index_map.new_id(ErasedFileAstIdKind::$ident, result))
                        },
                    )*
                    _ => None,
                }
            }
        }

        fn should_alloc_assoc_item(kind: SyntaxKind) -> Option<ErasedFileAstIdKind> {
            $( if ast::$ident::can_cast(kind) { Some(ErasedFileAstIdKind::$ident) } else )* { None }
        }
    };
}
register_assoc_item_ast_id! {
    impl AstIdNode for
    Variant = |it: ast::Variant| it.name(),
    Const = |it: ast::Const| it.name(),
    Fn = |it: ast::Fn| it.name(),
    MacroCall = |it: ast::MacroCall| it.path().and_then(|path| path.segment()?.name_ref()),
    TypeAlias = |it: ast::TypeAlias| it.name()
}

/// Maps items' `SyntaxNode`s to `ErasedFileAstId`s and back.
///
/// The ids of what is outside fn bodies are made with the map. Each body's are made when
/// something in it is first asked about, from the tree the map keeps of it, so a map of an
/// outline parses only the bodies asked about. Which bodies were walked is not part of what
/// the map is: two maps are equal when their ids outside bodies are, and their bodies are at
/// the same places with the same text and slots, which make the same ids.
#[derive(Default)]
pub struct AstIdMap {
    /// An arena of the ptrs and their associated ID.
    arena: Arena<(SyntaxNodePtr, ErasedFileAstId)>,
    /// Map ptr to id.
    ptr_map: hashbrown::HashTable<ArenaId>,
    /// Map id to ptr.
    id_map: hashbrown::HashTable<ArenaId>,
    /// The fn bodies, in the order of their text.
    bodies: Box<[Body]>,
    /// The bodies' numbers in `bodies`, in the order of their first slot.
    by_slot: Box<[u32]>,
}

type ArenaId = Idx<(SyntaxNodePtr, ErasedFileAstId)>;

/// A fn body, whose ids are made by a walk of it alone, when it is first asked about.
struct Body {
    range: TextRange,
    /// The hash values its ids have: `slots` of them from `first_slot` on, `INDEX_MASK + 1`
    /// ids of each kind in each.
    first_slot: u32,
    slots: u32,
    text_hash: u64,
    /// What the body is walked from: a lazy node, walked through its part, or a parsed block.
    tree: Arc<FlatTree>,
    at: u32,
    entries: OnceLock<BodyEntries>,
}

struct BodyEntries {
    entries: Box<[(SyntaxNodePtr, ErasedFileAstId)]>,
    by_ptr: FxHashMap<SyntaxNodePtr, u32>,
    by_id: FxHashMap<ErasedFileAstId, u32>,
}

impl Body {
    fn entries(&self) -> &BodyEntries {
        self.entries.get_or_init(|| {
            let (tree, at) = expanded(&self.tree, self.at);
            BodyEntries::new(body_entries(tree, at, self.first_slot, self.slots))
        })
    }

    /// The same body, of the same text at `range` of `tree`, element `at`.
    fn moved(&self, range: TextRange, tree: Arc<FlatTree>, at: u32) -> Body {
        Body {
            range,
            first_slot: self.first_slot,
            slots: self.slots,
            text_hash: self.text_hash,
            tree,
            at,
            entries: OnceLock::new(),
        }
    }
}

impl BodyEntries {
    fn new(entries: Vec<(SyntaxNodePtr, ErasedFileAstId)>) -> BodyEntries {
        let mut by_ptr = FxHashMap::with_capacity_and_hasher(entries.len(), FxBuildHasher);
        let mut by_id = FxHashMap::with_capacity_and_hasher(entries.len(), FxBuildHasher);
        for (i, &(ptr, id)) in entries.iter().enumerate() {
            by_ptr.insert(ptr, i as u32);
            by_id.insert(id, i as u32);
        }
        BodyEntries { entries: entries.into_boxed_slice(), by_ptr, by_id }
    }

    fn id(&self, ptr: SyntaxNodePtr) -> Option<ErasedFileAstId> {
        self.by_ptr.get(&ptr).map(|&i| self.entries[i as usize].1)
    }

    fn ptr(&self, id: ErasedFileAstId) -> Option<SyntaxNodePtr> {
        self.by_id.get(&id).map(|&i| self.entries[i as usize].0)
    }
}

impl PartialEq for Body {
    fn eq(&self, other: &Body) -> bool {
        self.range == other.range
            && self.first_slot == other.first_slot
            && self.slots == other.slots
            && self.text_hash == other.text_hash
    }
}

impl Hash for Body {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.range.hash(state);
        self.first_slot.hash(state);
        self.slots.hash(state);
        self.text_hash.hash(state);
    }
}

impl fmt::Debug for Body {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Body")
            .field("range", &self.range)
            .field("first_slot", &self.first_slot)
            .field("slots", &self.slots)
            .finish()
    }
}

impl fmt::Debug for AstIdMap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AstIdMap")
            .field("arena", &self.arena)
            .field("bodies", &self.bodies)
            .finish()
    }
}

impl PartialEq for AstIdMap {
    fn eq(&self, other: &Self) -> bool {
        self.arena == other.arena && self.bodies == other.bodies
    }
}
impl Eq for AstIdMap {}

impl Hash for AstIdMap {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.arena.len().hash(state);
        for (_, entry) in self.arena.iter() {
            entry.hash(state);
        }
        self.bodies.hash(state);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ContainsItems {
    Yes,
    No,
}

#[inline]
fn kind_of(tree: &FlatTree, at: u32) -> SyntaxKind {
    SyntaxKind::from(tree.kind(at).0)
}

/// Whether node `at` is a fn's body: a lazy node, or the block of a fn.
#[inline]
fn is_body(tree: &FlatTree, at: u32, kind: SyntaxKind) -> bool {
    tree.is_lazy(at)
        || (kind == SyntaxKind::BLOCK_EXPR
            && tree.parent(at).is_some_and(|it| kind_of(tree, it) == SyntaxKind::FN))
}

/// The node `at` stands for, as a node with its children: its part when it is lazy.
fn expanded(tree: &FlatTree, at: u32) -> (&FlatTree, u32) {
    match tree.is_lazy(at) {
        true => (&**tree.part(at), 0),
        false => (tree, at),
    }
}

/// The node of `kind` at `range`, not empty, at or under `at`, found by stepping down to the
/// child that holds `range`: no lazy node is entered on the way, as none holds another node.
fn find_node(tree: &FlatTree, mut at: u32, kind: SyntaxKind, range: TextRange) -> Option<u32> {
    loop {
        if tree.range(at) == range && kind_of(tree, at) == kind {
            return Some(at);
        }
        at = tree.children(at).find(|&it| tree.range(it).contains_range(range))?;
    }
}

/// What [`walk`] finds, in the order ids are made.
enum Step<P> {
    /// The block a found item is in, which holds no item found before it: its id is made
    /// before the item's.
    Block { at: u32, parent: Option<P> },
    Item { at: u32, kind: ErasedFileAstIdKind, parent: Option<P> },
    /// A fn's body, left to a walk of its own.
    Body { at: u32, parent: Option<P> },
}

/// Walks the nodes under `root` that may have ids, parents before children: by walking the
/// tree in breadth-first order parents get lower ids than children, so adding a new child
/// does not change its parent's id (adding a new function to a trait does not change the ids
/// of top-level items, which helps caching). `visit` is told of each and gives an item the
/// parent its children are told of. With `bodies_apart`, a fn's body is told of and not
/// walked into.
///
/// Blocks get an id only if they contain items. The walk keeps the stack of the blocks it is
/// under: when it enters a block it pushes `(block, No)`; items inside a block are attributed
/// to the block's container, not the block; for the first item found inside a block it makes
/// that `(block, Yes)` and tells of the block. When it leaves the block it pops it, whether
/// or not it told of it. It may seem that this tells of blocks that have no items directly but
/// have items inside other items inside them. This is true, but it doesn't matter, because
/// such blocks can't exist: the block will then contain the *outer* item.
///
/// Each layer is its elements in preorder, an item's subtree is stepped over by its end, and
/// a block is left once the walk is past its end. A lazy node has no children in the arrays,
/// so the walk never parses one.
fn walk<P: Copy>(
    tree: &FlatTree,
    root: u32,
    bodies_apart: bool,
    mut visit: impl FnMut(Step<P>) -> Option<P>,
) {
    let mut blocks: Vec<(u32, ContainsItems)> = Vec::new();
    let mut curr_layer: Vec<(u32, Option<P>)> = Vec::with_capacity(32);
    curr_layer.push((root, None));
    let mut next_layer = Vec::with_capacity(32);
    while !curr_layer.is_empty() {
        for (layer, parent) in curr_layer.drain(..) {
            let end = tree.end(layer);
            let mut it = layer;
            while it < end {
                while blocks.last().is_some_and(|&(block, _)| tree.end(block) <= it) {
                    blocks.pop();
                }
                if tree.is_token(it) {
                    it += 1;
                    continue;
                }
                let kind = kind_of(tree, it);
                if bodies_apart && is_body(tree, it, kind) {
                    visit(Step::Body { at: it, parent });
                    it = tree.end(it);
                    continue;
                }
                if ast::BlockExpr::can_cast(kind) {
                    blocks.push((it, ContainsItems::No));
                } else if let Some(id_kind) = ErasedFileAstId::should_alloc(kind) {
                    // We don't associate items with blocks, only with items, since block IDs can be quite unstable.
                    // FIXME: Is this the correct thing to do? Macro calls might actually be more incremental if
                    // associated with blocks (not sure). Either way it's not a big deal.
                    let is_item = matches!(
                        id_kind,
                        ErasedFileAstIdKind::Enum
                            | ErasedFileAstIdKind::Struct
                            | ErasedFileAstIdKind::Union
                            | ErasedFileAstIdKind::ExternCrate
                            | ErasedFileAstIdKind::MacroDef
                            | ErasedFileAstIdKind::MacroRules
                            | ErasedFileAstIdKind::Module
                            | ErasedFileAstIdKind::Static
                            | ErasedFileAstIdKind::Trait
                            | ErasedFileAstIdKind::Const
                            | ErasedFileAstIdKind::Fn
                            | ErasedFileAstIdKind::TypeAlias
                            | ErasedFileAstIdKind::ExternBlock
                            | ErasedFileAstIdKind::Use
                            | ErasedFileAstIdKind::Impl
                    );
                    if let Some((last_block, already_allocated @ ContainsItems::No)) =
                        blocks.last_mut()
                        && (is_item
                            || (id_kind == ErasedFileAstIdKind::MacroCall && {
                                let parent = tree.parent(it);
                                parent.is_some_and(|it| kind_of(tree, it) == SyntaxKind::MACRO_EXPR)
                                    && parent.and_then(|it| tree.parent(it)).is_some_and(|it| {
                                        matches!(
                                            kind_of(tree, it),
                                            SyntaxKind::EXPR_STMT | SyntaxKind::STMT_LIST
                                        )
                                    })
                            }))
                    {
                        visit(Step::Block { at: *last_block, parent });
                        *already_allocated = ContainsItems::Yes;
                    }
                    let idx = visit(Step::Item { at: it, kind: id_kind, parent });
                    next_layer.extend(tree.children(it).map(|child| (child, idx)));
                    it = tree.end(it);
                    continue;
                }
                it += 1;
            }
            while blocks.last().is_some_and(|&(block, _)| tree.end(block) <= end) {
                blocks.pop();
            }
        }
        std::mem::swap(&mut curr_layer, &mut next_layer);
        assert!(blocks.is_empty(), "didn't leave all BlockExprs");
    }
}

/// The ids of the body at `root` of `tree`, which has `slots` hash values from `first_slot`
/// on: each node's is its number among the body's nodes of its kind, in the walk's order.
fn body_entries(
    tree: &FlatTree,
    root: u32,
    first_slot: u32,
    slots: u32,
) -> Vec<(SyntaxNodePtr, ErasedFileAstId)> {
    let mut next = [0u32; ErasedFileAstIdKind::Root as usize + 1];
    let mut entries = Vec::new();
    walk::<()>(tree, root, false, |step| {
        let (at, kind) = match step {
            Step::Block { at, .. } => (at, ErasedFileAstIdKind::BlockExpr),
            Step::Item { at, kind, .. } => (at, kind),
            Step::Body { .. } => unreachable!("a body's own walk leaves no body apart"),
        };
        let n = next[kind as usize];
        next[kind as usize] += 1;
        let slot = n >> INDEX_BITS;
        // Each node of a kind has a token of its own, so there are fewer than the body has
        // bytes, which its slots were counted from.
        assert!(slot < slots, "more {kind:?} nodes in a fn body than it has bytes");
        let id = pack((first_slot + slot) as u16, n & INDEX_MASK, true, kind as u32);
        entries.push((
            SyntaxNodePtr::from_parts(kind_of(tree, at), tree.range(at)),
            ErasedFileAstId(id),
        ));
        Some(())
    });
    entries
}

/// The first of `slots` hash values in a row none of which is in `used`, looked for from
/// `start` on, marked used.
fn take_slots(used: &mut [u64], start: u16, slots: u32) -> u32 {
    assert!(slots <= SLOTS, "a fn body too long to number its nodes ({slots} slots)");
    let taken = |used: &[u64], it: u32| used[(it / 64) as usize] & (1 << (it % 64)) != 0;
    let mut first = u32::from(start);
    for _ in 0..2 * SLOTS {
        if first + slots > SLOTS {
            first = 0;
        }
        match (first..first + slots).rfind(|&it| taken(used, it)) {
            Some(it) => first = it + 1,
            None => {
                for it in first..first + slots {
                    used[(it / 64) as usize] |= 1 << (it % 64);
                }
                return first;
            }
        }
    }
    panic!("the fn bodies of one file need more than {SLOTS} slots to number their nodes")
}

/// `found`'s bodies of `tree`, each with the owner it was found in, given their slots in the
/// order they were found.
fn place_bodies(tree: &Arc<FlatTree>, found: Vec<(u32, ErasedFileAstId)>) -> Vec<Body> {
    let mut used = vec![0u64; (SLOTS / 64) as usize];
    let source = tree.source();
    found
        .into_iter()
        .map(|(at, owner)| {
            let range = tree.range(at);
            let slots = u32::from(range.len()) / (INDEX_MASK + 1) + 1;
            let first_slot = take_slots(&mut used, u16_hash(FxBuildHasher.hash_one(owner)), slots);
            Body {
                range,
                first_slot,
                slots,
                text_hash: FxBuildHasher.hash_one(&source[range]),
                tree: tree.clone(),
                at,
                entries: OnceLock::new(),
            }
        })
        .collect()
}

impl AstIdMap {
    pub fn from_source(node: &SyntaxNode) -> AstIdMap {
        assert!(node.parent().is_none());
        let mut res = AstIdMap::default();
        let mut index_map = ErasedAstIdNextIndexMap::default();

        // Ensure we allocate the root.
        res.arena.alloc((SyntaxNodePtr::new(node), ROOT_ERASED_FILE_AST_ID));

        // A cursor is made only of an item, to name it; a fn's body is not walked into, but
        // kept to be walked when asked about.
        let root = node.to_flat();
        let (tree, at) = root.flat().expect("a flat tree");
        let mut found = Vec::new();
        walk::<ArenaId>(tree, at, true, |step| match step {
            Step::Block { at, parent } => {
                let parent = parent_of(parent, &res.arena).copied();
                let kind = kind_of(tree, at);
                let id = block_expr_ast_id(kind, &mut index_map, parent.as_ref())
                    .expect("not a BlockExpr");
                res.arena.alloc((SyntaxNodePtr::from_parts(kind, tree.range(at)), id));
                None
            }
            Step::Item { at, kind: _, parent } => {
                let parent = parent_of(parent, &res.arena).copied();
                let item = root.flat_element(at).into_node().expect("a node");
                let id = ErasedFileAstId::ast_id_for(&item, &mut index_map, parent.as_ref())
                    .expect("this node should have an ast id");
                let ptr = SyntaxNodePtr::from_parts(kind_of(tree, at), tree.range(at));
                Some(res.arena.alloc((ptr, id)))
            }
            Step::Body { at, parent } => {
                found.push((at, parent.map_or(ROOT_ERASED_FILE_AST_ID, |it| res.arena[it].1)));
                None
            }
        });
        res.set_bodies(place_bodies(tree, found));
        res.index();
        return res;

        fn parent_of(
            parent_idx: Option<ArenaId>,
            arena: &Arena<(SyntaxNodePtr, ErasedFileAstId)>,
        ) -> Option<&ErasedFileAstId> {
            let mut parent = parent_idx.map(|parent_idx| &arena[parent_idx].1);
            if parent.is_some_and(|parent| parent.kind() == ErasedFileAstIdKind::ExternBlock as u32)
            {
                // See the comment on `ErasedAssocItemFileAstId` for why is this.
                // FIXME: Technically there could be an extern block inside another item, e.g.:
                // ```
                // fn foo() {
                //     extern "C" {
                //         fn bar();
                //     }
                // }
                // ```
                // Here we want to make `foo()` the parent of `bar()`, but we make it `None`.
                // Shouldn't be a big deal though.
                parent = None;
            }
            parent
        }
    }

    /// The ids of the nodes of `node`, a node of the tree this map is of, as nodes of
    /// `relative`: the same node, the root of a tree of its own. What a body is lowered from
    /// when it is lowered without the rest of its file, so that the map stays the same while
    /// the text around `node` moves it. The first entry is `node` itself.
    ///
    /// Either tree may be an outline or a full parse. A fn body in `node` is not walked: the
    /// map made keeps it, with its slots, to be walked in `relative` when asked about, so
    /// lowering one body of an item parses no other.
    pub fn relative_to(&self, node: &SyntaxNode, relative: &SyntaxNode) -> AstIdMap {
        let (node, relative) = (node.to_flat(), relative.to_flat());
        let (tree, at) = node.flat().expect("a flat tree");
        let (tree, at) = expanded(tree, at);
        let (relative_tree, relative_at) = relative.flat().expect("a flat tree");
        let (start, relative_range) = (tree.range(at).start(), relative_tree.range(relative_at));
        debug_assert!(
            tree.kind(at) == relative_tree.kind(relative_at)
                && tree.range(at).len() == relative_range.len(),
            "not the same node"
        );
        let moved = |range: TextRange| range - start + relative_range.start();
        // Under a node in a body, everything is that body's.
        let in_body = self.body_holding(tree.range(at)).is_some();
        let mut res = AstIdMap::default();
        let mut bodies = Vec::new();
        let end = tree.end(at);
        let mut it = at;
        while it < end {
            if tree.is_token(it) {
                it += 1;
                continue;
            }
            let kind = kind_of(tree, it);
            let range = tree.range(it);
            if !in_body && is_body(tree, it, kind) {
                let body = self
                    .body_holding(range)
                    .filter(|body| body.range == range)
                    .expect("a fn body of the node is one of the map's");
                let range = moved(range);
                let relative_at = find_node(relative_tree, relative_at, kind, range)
                    .expect("the relative node has the body");
                bodies.push(body.moved(range, relative_tree.clone(), relative_at));
                it = tree.end(it);
                continue;
            }
            let may_have_id =
                ast::BlockExpr::can_cast(kind) || ErasedFileAstId::should_alloc(kind).is_some();
            if may_have_id
                && let Some(id) = self.try_erased_ast_id(SyntaxNodePtr::from_parts(kind, range))
            {
                res.arena.alloc((SyntaxNodePtr::from_parts(kind, moved(range)), id));
            }
            it += 1;
        }
        res.set_bodies(bodies);
        res.index();
        res
    }

    fn set_bodies(&mut self, mut bodies: Vec<Body>) {
        bodies.sort_by_key(|it| it.range.start());
        let mut by_slot: Vec<u32> = (0..bodies.len() as u32).collect();
        by_slot.sort_by_key(|&it| bodies[it as usize].first_slot);
        self.bodies = bodies.into_boxed_slice();
        self.by_slot = by_slot.into_boxed_slice();
    }

    /// The fn body `range` is in.
    fn body_holding(&self, range: TextRange) -> Option<&Body> {
        let n = self.bodies.partition_point(|it| it.range.start() <= range.start());
        self.bodies[..n].last().filter(|it| it.range.contains_range(range))
    }

    /// The fn body `id`, an id of a node in a body, is of: the one with its hash value.
    fn body_of(&self, id: ErasedFileAstId) -> Option<&Body> {
        let slot = u32::from(id.hash_value());
        let n = self.by_slot.partition_point(|&it| self.bodies[it as usize].first_slot <= slot);
        let body = &self.bodies[*self.by_slot[..n].last()? as usize];
        (slot < body.first_slot + body.slots).then_some(body)
    }

    fn index(&mut self) {
        let res = self;
        res.ptr_map = hashbrown::HashTable::with_capacity(res.arena.len());
        res.id_map = hashbrown::HashTable::with_capacity(res.arena.len());
        for (idx, (ptr, ast_id)) in res.arena.iter() {
            let ptr_hash = hash_ptr(ptr);
            let ast_id_hash = hash_ast_id(ast_id);
            match res.ptr_map.entry(
                ptr_hash,
                |idx2| *idx2 == idx,
                |&idx| hash_ptr(&res.arena[idx].0),
            ) {
                hashbrown::hash_table::Entry::Occupied(_) => unreachable!(),
                hashbrown::hash_table::Entry::Vacant(entry) => {
                    entry.insert(idx);
                }
            }
            match res.id_map.entry(
                ast_id_hash,
                |idx2| *idx2 == idx,
                |&idx| hash_ast_id(&res.arena[idx].1),
            ) {
                hashbrown::hash_table::Entry::Occupied(_) => unreachable!(),
                hashbrown::hash_table::Entry::Vacant(entry) => {
                    entry.insert(idx);
                }
            }
        }
        res.arena.shrink_to_fit();
    }

    /// The root node.
    pub fn root(&self) -> SyntaxNodePtr {
        self.arena[Idx::from_raw(RawIdx::from_u32(0))].0
    }

    pub fn ast_id<N: AstIdNode>(&self, item: &N) -> FileAstId<N> {
        self.ast_id_for_ptr(AstPtr::new(item))
    }

    /// Blocks may not be allocated (if they have no items), so they have a different API.
    pub fn ast_id_for_block(&self, block: &ast::BlockExpr) -> Option<FileAstId<ast::BlockExpr>> {
        self.ast_id_for_ptr_for_block(AstPtr::new(block))
    }

    pub fn ast_id_for_ptr<N: AstIdNode>(&self, ptr: AstPtr<N>) -> FileAstId<N> {
        let ptr = ptr.syntax_node_ptr();
        FileAstId { raw: self.erased_ast_id(ptr), _marker: PhantomData }
    }

    /// Blocks may not be allocated (if they have no items), so they have a different API.
    pub fn ast_id_for_ptr_for_block(
        &self,
        ptr: AstPtr<ast::BlockExpr>,
    ) -> Option<FileAstId<ast::BlockExpr>> {
        let ptr = ptr.syntax_node_ptr();
        self.try_erased_ast_id(ptr).map(|raw| FileAstId { raw, _marker: PhantomData })
    }

    fn erased_ast_id(&self, ptr: SyntaxNodePtr) -> ErasedFileAstId {
        self.try_erased_ast_id(ptr).unwrap_or_else(|| {
            panic!(
                "Can't find SyntaxNodePtr {:?} in AstIdMap:\n{:?}",
                ptr,
                self.arena.iter().map(|(_id, i)| i).collect::<Vec<_>>(),
            )
        })
    }

    /// The id of the node at `ptr`; of a node in a fn body, after walking that body if no one
    /// asked about it before.
    fn try_erased_ast_id(&self, ptr: SyntaxNodePtr) -> Option<ErasedFileAstId> {
        let hash = hash_ptr(&ptr);
        if let Some(&idx) = self.ptr_map.find(hash, |&idx| self.arena[idx].0 == ptr) {
            return Some(self.arena[idx].1);
        }
        self.body_holding(ptr.text_range())?.entries().id(ptr)
    }

    // Don't bound on `AstIdNode` here, because `BlockExpr`s are also valid here (`ast::BlockExpr`
    // doesn't always have a matching `FileAstId`, but a `FileAstId<ast::BlockExpr>` always has
    // a matching node).
    pub fn get<N: AstNode>(&self, id: FileAstId<N>) -> AstPtr<N> {
        let ptr = self.get_erased(id.raw);
        AstPtr::try_from_raw(ptr)
            .unwrap_or_else(|| panic!("AstIdMap node mismatch with node `{ptr:?}`"))
    }

    /// The nodes this map has an id of: every item of the file, and the blocks with items,
    /// those in fn bodies too (which walks every body no one asked about before).
    pub fn ptrs(&self) -> impl Iterator<Item = SyntaxNodePtr> + '_ {
        let bodies =
            self.bodies.iter().flat_map(|body| body.entries().entries.iter().map(|(ptr, _)| *ptr));
        self.arena.iter().map(|(_, (ptr, _))| *ptr).chain(bodies)
    }

    /// The node of `id`, when this map has it: a map [`AstIdMap::relative_to`] a node has only
    /// the ids of what is in that node. The id of a node in a fn body walks that body if no
    /// one asked about it before.
    pub fn try_get_erased(&self, id: ErasedFileAstId) -> Option<SyntaxNodePtr> {
        if id.in_body() {
            return self.body_of(id)?.entries().ptr(id);
        }
        let hash = hash_ast_id(&id);
        self.id_map.find(hash, |&idx| self.arena[idx].1 == id).map(|&idx| self.arena[idx].0)
    }

    pub fn get_erased(&self, id: ErasedFileAstId) -> SyntaxNodePtr {
        self.try_get_erased(id).unwrap_or_else(|| {
            panic!(
                "Can't find ast id {:?} in AstIdMap:\n{:?}\n{:?}",
                id,
                self.arena.iter().map(|(_id, i)| i).collect::<Vec<_>>(),
                self.bodies,
            )
        })
    }
}

#[cfg(not(no_salsa_async_drops))]
type Dropped = (
    Arena<(SyntaxNodePtr, ErasedFileAstId)>,
    hashbrown::HashTable<ArenaId>,
    hashbrown::HashTable<ArenaId>,
    Box<[Body]>,
);

#[cfg(not(no_salsa_async_drops))]
impl Drop for AstIdMap {
    fn drop(&mut self) {
        let arena = std::mem::take(&mut self.arena);
        let ptr_map = std::mem::take(&mut self.ptr_map);
        let id_map = std::mem::take(&mut self.id_map);
        let bodies = std::mem::take(&mut self.bodies);
        static AST_ID_MAP_DROP_THREAD: std::sync::OnceLock<std::sync::mpsc::Sender<Dropped>> =
            std::sync::OnceLock::new();
        AST_ID_MAP_DROP_THREAD
            .get_or_init(|| {
                let (sender, receiver) = std::sync::mpsc::channel::<Dropped>();
                std::thread::Builder::new()
                    .name("AstIdMapDropper".to_owned())
                    .spawn(move || {
                        loop {
                            // block on a receive
                            _ = receiver.recv();
                            // then drain the entire channel
                            while receiver.try_recv().is_ok() {}
                            // and sleep for a bit
                            std::thread::sleep(std::time::Duration::from_millis(100));
                        }
                        // why do this over just a `receiver.iter().for_each(drop)`? To reduce contention on the channel lock.
                        // otherwise this thread will constantly wake up and sleep again.
                    })
                    .unwrap();
                sender
            })
            .send((arena, ptr_map, id_map, bodies))
            .unwrap();
    }
}

#[inline]
fn hash_ptr(ptr: &SyntaxNodePtr) -> u64 {
    FxBuildHasher.hash_one(ptr)
}

#[inline]
fn hash_ast_id(ptr: &ErasedFileAstId) -> u64 {
    FxBuildHasher.hash_one(ptr)
}

#[cfg(test)]
mod tests {
    use std::{
        hash::BuildHasher,
        path::{Path, PathBuf},
        sync::Arc,
    };

    use rustc_hash::{FxBuildHasher, FxHashMap, FxHashSet};
    use syntax::{
        AstNode, AstPtr, Edition, SourceFile, SyntaxKind, SyntaxNode, SyntaxNodePtr, WalkEvent,
        ast::{self, HasName},
    };

    use super::ErasedFileAstId;
    use crate::AstIdMap;

    #[test]
    fn check_all_nodes() {
        let syntax = SourceFile::parse(
            r#"
extern crate foo;
fn foo() {
    union U {}
}
struct S;
macro_rules! m {}
macro m2() {}
trait Trait {}
impl Trait for S {}
impl S {}
impl m!() {}
impl m2!() for m!() {}
type T = i32;
enum E {
    V1(),
    V2 {},
    V3,
}
struct S; // duplicate
extern "C" {
    static S: i32;
}
static mut S: i32 = 0;
const FOO: i32 = 0;
        "#,
            Edition::CURRENT,
        )
        .syntax_node();
        let ast_id_map = AstIdMap::from_source(&syntax);
        for node in syntax.preorder() {
            let WalkEvent::Enter(node) = node else { continue };
            if !matches!(
                node.kind(),
                SyntaxKind::EXTERN_CRATE
                    | SyntaxKind::FN
                    | SyntaxKind::UNION
                    | SyntaxKind::STRUCT
                    | SyntaxKind::MACRO_RULES
                    | SyntaxKind::MACRO_DEF
                    | SyntaxKind::MACRO_CALL
                    | SyntaxKind::TRAIT
                    | SyntaxKind::IMPL
                    | SyntaxKind::TYPE_ALIAS
                    | SyntaxKind::ENUM
                    | SyntaxKind::VARIANT
                    | SyntaxKind::EXTERN_BLOCK
                    | SyntaxKind::STATIC
                    | SyntaxKind::CONST
            ) {
                continue;
            }
            let ptr = SyntaxNodePtr::new(&node);
            let ast_id = ast_id_map.erased_ast_id(ptr);
            let turn_back = ast_id_map.get_erased(ast_id);
            assert_eq!(ptr, turn_back);
        }
    }

    #[test]
    fn different_names_get_different_hashes() {
        let syntax = SourceFile::parse(
            r#"
fn foo() {}
fn bar() {}
        "#,
            Edition::CURRENT,
        )
        .syntax_node();
        let ast_id_map = AstIdMap::from_source(&syntax);
        let fns = syntax.descendants().filter_map(ast::Fn::cast).collect::<Vec<_>>();
        let [foo_fn, bar_fn] = fns.as_slice() else {
            panic!("not exactly 2 functions");
        };
        let foo_fn_id = ast_id_map.ast_id(foo_fn);
        let bar_fn_id = ast_id_map.ast_id(bar_fn);
        assert_ne!(foo_fn_id.raw.hash_value(), bar_fn_id.raw.hash_value(), "hashes are equal");
    }

    #[test]
    fn different_parents_get_different_hashes() {
        let syntax = SourceFile::parse(
            r#"
fn foo() {
    m!();
}
fn bar() {
    m!();
}
        "#,
            Edition::CURRENT,
        )
        .syntax_node();
        let ast_id_map = AstIdMap::from_source(&syntax);
        let macro_calls = syntax.descendants().filter_map(ast::MacroCall::cast).collect::<Vec<_>>();
        let [macro_call_foo, macro_call_bar] = macro_calls.as_slice() else {
            panic!("not exactly 2 macro calls");
        };
        let macro_call_foo_id = ast_id_map.ast_id(macro_call_foo);
        let macro_call_bar_id = ast_id_map.ast_id(macro_call_bar);
        assert_ne!(
            macro_call_foo_id.raw.hash_value(),
            macro_call_bar_id.raw.hash_value(),
            "hashes are equal"
        );
    }

    #[test]
    fn blocks_with_no_items_have_no_id() {
        let syntax = SourceFile::parse(
            r#"
fn foo() {
    let foo = 1;
    bar(foo);
}
        "#,
            Edition::CURRENT,
        )
        .syntax_node();
        let ast_id_map = AstIdMap::from_source(&syntax);
        let block = syntax.descendants().find_map(ast::BlockExpr::cast).expect("no block");
        assert!(ast_id_map.ast_id_for_block(&block).is_none());
    }

    fn outline_of(text: &Arc<str>) -> SyntaxNode {
        SourceFile::parse_outline(text.clone(), Edition::CURRENT).syntax_node()
    }

    fn full_of(text: &Arc<str>) -> SyntaxNode {
        SourceFile::parse_shared(text.clone(), Edition::CURRENT).syntax_node()
    }

    fn count(ids: &[(SyntaxNodePtr, ErasedFileAstId)], kind: SyntaxKind) -> usize {
        ids.iter().filter(|(ptr, _)| ptr.kind() == kind).count()
    }

    /// The lazy bodies of `root`'s tree that are parsed.
    fn parsed(root: &SyntaxNode) -> Vec<syntax::TextRange> {
        let (tree, _) = root.flat().expect("a flat tree");
        tree.lazies().filter(|&it| tree.is_parsed(it)).map(|it| tree.range(it)).collect()
    }

    fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
        let mut entries: Vec<_> = std::fs::read_dir(dir)
            .unwrap_or_else(|e| panic!("{}: {e}", dir.display()))
            .map(|it| it.expect("a directory entry").path())
            .collect();
        entries.sort();
        for path in entries {
            if path.is_dir() {
                rust_files(&path, out);
            } else if path.extension().is_some_and(|it| it == "rs") {
                out.push(path);
            }
        }
    }

    /// The parser's test data and the `syntax` crate's own sources.
    fn corpus() -> Vec<(PathBuf, Arc<str>)> {
        let crates = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let mut paths = Vec::new();
        for dir in ["parser/test_data/parser", "syntax/src"] {
            rust_files(&crates.join(dir), &mut paths);
        }
        assert!(paths.len() > 300, "the corpus is {} files", paths.len());
        paths
            .into_iter()
            .map(|path| {
                let text = std::fs::read_to_string(&path)
                    .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
                (path, Arc::from(text))
            })
            .collect()
    }

    /// Each node a map has an id of, with it.
    fn ids(map: &AstIdMap) -> Vec<(SyntaxNodePtr, ErasedFileAstId)> {
        map.ptrs().map(|ptr| (ptr, map.erased_ast_id(ptr))).collect()
    }

    #[test]
    fn an_outline_has_the_ids_of_a_full_parse() {
        for (path, text) in corpus() {
            let path = path.display();
            let full = full_of(&text);
            let full_map = AstIdMap::from_source(&full);
            let want = ids(&full_map);
            let distinct: FxHashSet<_> = want.iter().map(|&(_, id)| id).collect();
            assert_eq!(distinct.len(), want.len(), "{path}: two nodes have one id");
            let with_id: FxHashSet<_> = want.iter().map(|&(ptr, _)| ptr).collect();
            for node in full.descendants() {
                let ptr = SyntaxNodePtr::new(&node);
                if ErasedFileAstId::should_alloc(node.kind()).is_some() {
                    assert!(with_id.contains(&ptr), "{path}: {node:?} has no id");
                }
                if full_map.try_erased_ast_id(ptr).is_some() {
                    assert!(with_id.contains(&ptr), "{path}: `ptrs` misses {node:?}");
                }
            }

            for reverse in [false, true] {
                let outline = outline_of(&text);
                let map = AstIdMap::from_source(&outline);
                assert!(parsed(&outline).is_empty(), "{path}: making the map parsed a body");
                assert!(map == full_map, "{path}: the outline's map is not the full parse's");
                assert_eq!(FxBuildHasher.hash_one(&map), FxBuildHasher.hash_one(&full_map));
                let mut order: Vec<_> = want.iter().copied().collect();
                if reverse {
                    order.reverse();
                }
                for (i, (ptr, id)) in order.into_iter().enumerate() {
                    match i % 2 {
                        0 => assert_eq!(map.try_erased_ast_id(ptr), Some(id), "{path}: {ptr:?}"),
                        _ => assert_eq!(map.try_get_erased(id), Some(ptr), "{path}: {id:?}"),
                    }
                }
                assert!(map == full_map, "{path}: walking bodies changed the map");
            }

            let outline = outline_of(&text);
            let map = AstIdMap::from_source(&outline);
            std::thread::scope(|scope| {
                for thread in 0..4 {
                    let (map, want) = (&map, &want);
                    scope.spawn(move || {
                        let skip = thread * want.len() / 4;
                        for &(ptr, id) in want.iter().cycle().skip(skip).take(want.len()) {
                            assert_eq!(map.try_erased_ast_id(ptr), Some(id));
                            assert_eq!(map.try_get_erased(id), Some(ptr));
                        }
                    });
                }
            });
            let got: FxHashMap<_, _> = ids(&map).into_iter().collect();
            assert!(got == want.into_iter().collect::<FxHashMap<_, _>>(), "{path}: `ptrs` differ");
        }
    }

    #[test]
    fn a_body_is_walked_when_asked_about() {
        let text: Arc<str> =
            "fn a() { m!(); struct S; }\nfn b() { n!(); }\nimpl T { fn c() { fn d() { o!() } } }\n"
                .into();
        let full = full_of(&text);
        let call = full
            .descendants()
            .filter_map(ast::MacroCall::cast)
            .find(|it| it.path().is_some_and(|it| it.syntax().to_string() == "n"))
            .expect("`n!()`");
        let body = full
            .descendants()
            .filter_map(ast::Fn::cast)
            .find(|it| it.name().is_some_and(|it| it.syntax().to_string() == "b"))
            .and_then(|it| it.body())
            .expect("`b`'s body")
            .syntax()
            .text_range();

        let outline = outline_of(&text);
        let map = AstIdMap::from_source(&outline);
        let (tree, _) = outline.flat().expect("a flat tree");
        assert_eq!(tree.lazies().count(), 3, "the bodies of `a`, `b` and `c`");
        assert!(parsed(&outline).is_empty());
        let id = map.ast_id(&call);
        assert!(id.erase().in_body());
        assert_eq!(parsed(&outline), [body], "asking for `n!()` parses `b`'s body alone");
        assert_eq!(map.get(id), AstPtr::new(&call));

        // An id from another map of the same text finds its body by its slots.
        let again = outline_of(&text);
        let map = AstIdMap::from_source(&again);
        assert_eq!(map.get(id), AstPtr::new(&call));
        assert_eq!(parsed(&again), [body]);
    }

    #[test]
    fn many_bodies_with_the_same_macros_have_distinct_ids() {
        let text: String =
            (0..5000).map(|i| format!("fn f{i}() {{ a!(); a!(); {{ b!(); }} }}\n")).collect();
        let text: Arc<str> = text.into();
        let full_map = AstIdMap::from_source(&full_of(&text));
        let want = ids(&full_map);
        assert_eq!(count(&want, SyntaxKind::FN), 5000);
        assert_eq!(count(&want, SyntaxKind::MACRO_CALL), 3 * 5000);
        let distinct: FxHashSet<_> = want.iter().map(|&(_, id)| id).collect();
        assert_eq!(distinct.len(), want.len());
        let map = AstIdMap::from_source(&outline_of(&text));
        for &(ptr, id) in want.iter().rev() {
            assert_eq!(map.try_get_erased(id), Some(ptr));
        }
    }

    #[test]
    fn relative_to_an_outline_or_a_full_parse() {
        let text: Arc<str> =
            "struct A;\nimpl T for A {\n    fn c() { m!(); fn d() { n!() } }\n    fn e() {}\n}\n"
                .into();
        let full_map = AstIdMap::from_source(&full_of(&text));
        for file in [outline_of(&text), full_of(&text)] {
            let map = AstIdMap::from_source(&file);
            let anchor =
                file.children().find(|it| it.kind() == SyntaxKind::IMPL).expect("the impl");
            let (start, range) = (anchor.text_range().start(), anchor.text_range());
            let anchor_text: Arc<str> = Arc::from(&text[range]);
            for relative_root in [outline_of(&anchor_text), full_of(&anchor_text)] {
                let relative = relative_root
                    .children()
                    .find(|it| it.kind() == SyntaxKind::IMPL)
                    .expect("the impl");
                let relative_map = map.relative_to(&anchor, &relative);
                assert_eq!(relative_map.root().text_range(), relative.text_range());
                let inside = ids(&full_map)
                    .into_iter()
                    .filter(|(ptr, _)| range.contains_range(ptr.text_range()))
                    .collect::<Vec<_>>();
                assert_eq!(count(&inside, SyntaxKind::IMPL), 1);
                assert_eq!(count(&inside, SyntaxKind::FN), 3, "`c`, `d` and `e`");
                assert_eq!(count(&inside, SyntaxKind::MACRO_CALL), 2);
                for (ptr, id) in inside {
                    let moved = SyntaxNodePtr::from_parts(ptr.kind(), ptr.text_range() - start);
                    assert_eq!(relative_map.try_erased_ast_id(moved), Some(id));
                    assert_eq!(relative_map.try_get_erased(id), Some(moved));
                }
            }
            assert!(parsed(&file).is_empty(), "the file's bodies are walked in the relative trees");
        }
    }
}
