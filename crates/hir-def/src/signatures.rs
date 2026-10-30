//! Item signature IR definitions

use std::cell::LazyCell;

use base_db::{Db, EditionedFileId};
use bitflags::bitflags;
use blaze_ids::bits::Bits;
use cfg::{CfgExpr, CfgOptions};
use hir_expand::{
    AstId, HirFileId, InFile, Intern, Lookup,
    name::{AsName, Name},
    span_map::SpanMap,
};
use intern::{Symbol, sym};
use la_arena::{Arena, Idx};
use rustc_abi::{ExternAbi, IntegerType, ReprOptions};
use smallvec::SmallVec;
use span::{AstIdMap, AstIdNode, ROOT_ERASED_FILE_AST_ID, RealSpanMap};
use syntax::{
    AstNode, FlatTree, SyntaxKind, SyntaxNode, SyntaxNodePtr, T, TextRange,
    TextSize,
    ast::{self, HasGenericParams, HasName, HasVisibility, IsString},
};
use thin_vec::ThinVec;
use triomphe::Arc;

use crate::{
    AdtId, ConstId, EnumId, EnumVariantId, EnumVariantLoc, ExternBlockId, FunctionId, FxIndexMap,
    GenericDefId, HasModule, ImplId, ItemContainerId, LoweringMode, ModuleId, StaticId, StructId,
    TraitId, TypeAliasId, UnionId, VariantId,
    attrs::AttrFlags,
    expr_store::{
        ExpressionStore, ExpressionStoreSourceMap, Move,
        lower::{
            ExprCollector, lower_function, lower_generic_params_within, lower_impl, lower_trait,
            lower_type_alias, lower_type_ref,
        },
    },
    hir::{ExprId, PatId, generics::GenericParams},
    item_tree::{FieldsShape, RawVisibility, visibility_from_ast},
    src::HasSource,
    type_ref::{ConstRef, TraitRef, TypeBound, TypeRefId},
};

#[inline]
fn as_name_opt(name: Option<ast::Name>) -> Name {
    name.map_or_else(Name::missing, |it| it.as_name())
}

/// The flags of `$Flags` that the attributes `$attrs` set: each a shift and a mask of its one
/// `AttrFlags` bit, OR-ed together, with no branch per flag.
macro_rules! flags_from_attrs {
    ($attrs:expr, $Flags:ident { $($attr:ident => $flag:ident),+ $(,)? }) => {{
        type Bits = <$Flags as bitflags::Flags>::Bits;
        let attrs: AttrFlags = $attrs;
        let attrs = attrs.bits();
        $Flags::from_bits_retain(0 $(| {
            const FROM: u32 = single_bit(AttrFlags::$attr.bits());
            const TO: u32 = single_bit($Flags::$flag.bits() as u128);
            (((attrs >> FROM) & 1) as Bits) << TO
        })+)
    }};
}

/// The position of the one bit of `bits`.
const fn single_bit(bits: u128) -> u32 {
    assert!(bits.count_ones() == 1, "a flag read off an attribute is one bit");
    bits.trailing_zeros()
}

/// Which of `kinds` are among the tokens directly under `node`, read in one pass over its
/// children in the flat tree's kind column: each `*_token()` accessor scans them once more.
fn child_tokens<const N: usize>(node: &SyntaxNode, kinds: [SyntaxKind; N]) -> [bool; N] {
    let Some((tree, at)) = node.flat() else { unreachable!("a syntax node outside a flat tree") };
    let kinds = kinds.map(u16::from);
    let mut found = [false; N];
    for child in tree.children_with_tokens(at) {
        if tree.is_token(child) {
            let kind = tree.kind(child).0;
            for (found, want) in found.iter_mut().zip(kinds) {
                *found |= kind == want;
            }
        }
    }
    found
}

/// The maps a signature is lowered with when it is lowered from a [`SignatureSyntax`], `None`
/// when it is lowered from its file.
type Maps<'a> = Option<(&'a RealSpanMap, &'a AstIdMap)>;

/// What a signature is lowered from: its item written in a file, as a tree of its own whose
/// text starts at the item, with what no signature reads replaced by an empty node of its kind
/// (the body of a fn, const or static, the items of an impl or trait, and for
/// [`signature_syntax`] the variants of an enum), and the span and ast id maps of that tree.
/// It is compared by that text, so it stays the same while an edit elsewhere in the file moves
/// the item, or one inside what is left out: a signature is lowered again only when it
/// changes, as a body is from its `BodySyntax`. It is read off the file's [`FileOutline`], so
/// a body edit runs no query of it at all.
///
/// An item whose spans are counted from an item around it ([`RealSpanMap::anchor_at`]: an item
/// in a body, or in an `impl`, trait or module that has attributes) is lowered from that whole
/// item instead, less the bodies the outline leaves out of it, so any other edit inside that
/// one lowers it again. One in a body the outline leaves out is read off the file.
pub struct SignatureSyntax {
    pub file_id: HirFileId,
    /// Compared by its content, not a hash of it: salsa compares a new value with the old one
    /// only when the query runs again, and a compare of the columns reads each byte once where
    /// hashing them costs more and can collide.
    tree: std::sync::Arc<FlatTree>,
    /// The item in the tree.
    item: SyntaxNodePtr,
    /// The empty node in place of what is left out, when something is.
    elided: Option<SyntaxNodePtr>,
    ast_id_map: AstIdMap,
    span_map: RealSpanMap,
}

impl PartialEq for SignatureSyntax {
    fn eq(&self, other: &Self) -> bool {
        self.file_id == other.file_id
            && self.item == other.item
            && self.elided == other.elided
            && self.span_map == other.span_map
            && self.ast_id_map == other.ast_id_map
            && *self.tree == *other.tree
    }
}

impl Eq for SignatureSyntax {}

impl std::fmt::Debug for SignatureSyntax {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SignatureSyntax")
            .field("file_id", &self.file_id)
            .field("item", &self.item)
            .field("elided", &self.elided)
            .finish_non_exhaustive()
    }
}

impl SignatureSyntax {
    /// The syntax of `item`, a node of `source`'s tree, with `elide`'s node of it left out when
    /// the item is its own anchor. An item anchored at one around it is that whole one, less
    /// the bodies `source` already left out of it.
    fn new(source: Source<'_>, item: &SyntaxNode, elide: Elide) -> Option<SignatureSyntax> {
        let Source { file_id, span_map, ast_id_map, cuts } = source;
        let item_range = item.text_range();
        let (_, anchor) = span_map.anchor_at(item_range.start());
        let anchor = match anchor == ROOT_ERASED_FILE_AST_ID {
            true => item.ancestors().last()?,
            false => {
                let ptr = ast_id_map.get_erased(anchor);
                item.ancestors().find(|it| SyntaxNodePtr::new(it) == ptr)?
            }
        };
        let anchor_range = anchor.text_range();
        let own = anchor_range == item_range && anchor.kind() == item.kind();
        // The ast id map walks an empty fn body it is not told is left out as a body it has,
        // so each of the outline's empty bodies in an anchor around the item is left out too.
        // They hold nothing and all come after the item: none moves what the item has.
        let mut elided = SmallVec::<[SyntaxNode; 1]>::new();
        match own {
            true => elided.extend(elide(item)),
            false => bodies_under(&anchor, |body| {
                let ptr = SyntaxNodePtr::new(&body);
                let start = ptr.text_range().start();
                let at = cuts.partition_point(|it| it.text_range().start() < start);
                if cuts.get(at) == Some(&ptr) {
                    elided.push(body);
                }
            }),
        }
        let base = anchor_range.start();
        let copy = anchor.clone_subtree_without(&elided);
        // clone: an `Arc` count of the item's tree
        let tree = copy.flat().expect("a cloned subtree is flat").0.clone();
        let (placeholder, removed, spans_end) = match (own, elided.first()) {
            (true, Some(node)) => {
                let range = node.text_range();
                let placeholder =
                    SyntaxNodePtr::from_parts(node.kind(), TextRange::empty(range.start() - base));
                (Some(placeholder), range.len(), range.start())
            }
            _ => (None, TextSize::new(0), anchor_range.end()),
        };
        let item_ptr = SyntaxNodePtr::from_parts(
            item.kind(),
            TextRange::new(item_range.start() - base, item_range.end() - base - removed),
        );
        // alloc: inline for the one node an item leaves out
        let ptrs = elided.iter().map(SyntaxNodePtr::new).collect::<SmallVec<[_; 1]>>();
        Some(SignatureSyntax {
            file_id,
            tree,
            item: item_ptr,
            elided: placeholder,
            ast_id_map: ast_id_map.relative_without(&anchor, &copy, &ptrs),
            span_map: span_map.within(TextRange::new(base, spans_end)),
        })
    }

    fn root(&self) -> SyntaxNode {
        SyntaxNode::new_flat_root(self.tree.clone()) // clone: an `Arc` count of the outline tree
    }

    fn maps(&self) -> (&RealSpanMap, &AstIdMap) {
        (&self.span_map, &self.ast_id_map)
    }

    /// The item, in the tree.
    fn node<N: AstNode>(&self) -> InFile<N> {
        let node = self.item.to_node(&self.root());
        InFile::new(self.file_id, N::cast(node).expect("the item is a node of its kind"))
    }

    /// `source_map`, of a signature lowered from this tree, moved to where the item is in its
    /// file, at `file` there: what follows the empty node moves on by what it left out.
    fn moved(
        &self,
        file: TextRange,
        source_map: &ExpressionStoreSourceMap,
    ) -> ExpressionStoreSourceMap {
        let relative = self.item.text_range();
        let base = file.start() - relative.start();
        let removed = file.len() - relative.len();
        // Something is left out only of an item that is the tree's root, so the item is the
        // one node around the empty node.
        let to = |ptr: SyntaxNodePtr| {
            let range = match self.elided {
                _ if ptr == self.item => return SyntaxNodePtr::from_parts(ptr.kind(), file),
                Some(elided) if ptr == elided => {
                    TextRange::at(elided.text_range().start(), removed)
                }
                Some(elided) if ptr.text_range().start() >= elided.text_range().start() => {
                    ptr.text_range() + removed
                }
                _ => ptr.text_range(),
            };
            SyntaxNodePtr::from_parts(ptr.kind(), range + base)
        };
        source_map.moved(&Move { file: self.file_id, to: &to })
    }
}

/// What [`SignatureSyntax::new`] leaves out of an item: the child of it that no signature reads.
type Elide = fn(&SyntaxNode) -> Option<SyntaxNode>;

/// What a [`SignatureSyntax`] is read from: its file's [`FileOutline`], or the file itself for
/// an item in a body the outline leaves out. The maps are of the tree the item is a node of.
struct Source<'a> {
    file_id: HirFileId,
    span_map: &'a RealSpanMap,
    ast_id_map: &'a AstIdMap,
    /// The empty nodes the outline has in place of the bodies it leaves out, in order; none in
    /// a file.
    cuts: &'a [SyntaxNodePtr],
}

/// The body a [`FileOutline`] leaves out of `item` when the item is its own anchor: a fn's
/// block, a const's or static's expression. Each is the child the item's accessor returns
/// (`ast::Fn::body`, `ast::Const::body`): the first child of that kind.
fn left_out(item: &SyntaxNode) -> Option<SyntaxNode> {
    match item.kind() {
        SyntaxKind::FN => item.child_where(|kind| kind == SyntaxKind::BLOCK_EXPR),
        SyntaxKind::CONST | SyntaxKind::STATIC => item.child_where(ast::Expr::can_cast),
        _ => None,
    }
}

/// The [`left_out`] bodies of the items that can be span anchors under `node` (a file or an
/// item of one), in order: the file's items, and those of each impl, trait and module among
/// them, where `real_span_map` finds anchors. Only a fn's body is ever lazy, and none of these
/// is in one, so nothing here parses a body.
fn bodies_under(node: &SyntaxNode, mut body: impl FnMut(SyntaxNode)) {
    let mut item = |item: &SyntaxNode| {
        let list = match item.kind() {
            SyntaxKind::IMPL | SyntaxKind::TRAIT => SyntaxKind::ASSOC_ITEM_LIST,
            SyntaxKind::MODULE => SyntaxKind::ITEM_LIST,
            _ => {
                if let Some(it) = left_out(item) {
                    body(it);
                }
                return;
            }
        };
        if let Some(list) = item.child_where(|kind| kind == list) {
            list.children().filter_map(|it| left_out(&it)).for_each(&mut body);
        }
    };
    match node.kind() {
        SyntaxKind::SOURCE_FILE => node.children().for_each(|it| item(&it)),
        _ => item(node),
    }
}

/// A file's tree with the body of every fn, const and static that is its own span anchor left
/// out, each an empty node of its kind, and that tree's ast id and span maps: what every
/// signature of the file is read from. A body edit leaves it the same (nothing after a body
/// moves, as the body has no length here), so it backdates and no signature query of the file
/// runs again; rustc's per-owner HIR hash, in one query per file.
///
/// A body of an item anchored at one around it (an item of an impl, trait or module with
/// attributes, or of a module in a module) stays: cutting it would move its neighbours' spans,
/// which count from that anchor. An edit in it runs the file's signature queries again.
pub struct FileOutline {
    tree: std::sync::Arc<FlatTree>,
    ast_id_map: AstIdMap,
    span_map: RealSpanMap,
    /// The empty node in place of each body left out, in order.
    cuts: Box<[SyntaxNodePtr]>,
}

impl PartialEq for FileOutline {
    fn eq(&self, other: &Self) -> bool {
        self.cuts == other.cuts
            && self.span_map == other.span_map
            && self.ast_id_map == other.ast_id_map
            && *self.tree == *other.tree
    }
}

impl Eq for FileOutline {}

impl std::fmt::Debug for FileOutline {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileOutline").field("cuts", &self.cuts).finish_non_exhaustive()
    }
}

impl FileOutline {
    fn root(&self) -> SyntaxNode {
        SyntaxNode::new_flat_root(self.tree.clone()) // clone: an `Arc` count of the outline tree
    }
}

/// The [`FileOutline`] of `file_id`: one pass over the file's items, and one over what is left
/// of the file for the copy and each map.
#[salsa::tracked(returns(ref))]
pub fn file_outline(db: &Db, file_id: EditionedFileId) -> FileOutline {
    let file = HirFileId::from(file_id);
    let root = file.parse_or_expand(db);
    let ast_id_map = file.ast_id_map(db);
    let SpanMap::RealSpanMap(span_map) = file.span_map(db) else {
        unreachable!("the spans of a file, not a macro's, are a real span map")
    };
    // alloc: the bodies cut, once per file and edit
    let mut bodies = Vec::new();
    bodies_under(&root, |body| {
        let item = body.parent().expect("a body is under its item");
        let (start, anchor) = span_map.anchor_at(item.text_range().start());
        let own = start == item.text_range().start()
            && anchor != ROOT_ERASED_FILE_AST_ID
            && ast_id_map.get_erased(anchor) == SyntaxNodePtr::new(&item);
        if own {
            bodies.push(body);
        }
    });
    let outline = root.clone_subtree_without(&bodies);
    // alloc: one pointer per body cut, once per file and edit
    let ptrs = bodies.iter().map(SyntaxNodePtr::new).collect::<Vec<_>>();
    // alloc: one range per body cut, as `ptrs`
    let ranges = ptrs.iter().map(SyntaxNodePtr::text_range).collect::<Vec<_>>();
    let mut cut = TextSize::new(0);
    let cuts = ptrs
        .iter()
        .map(|ptr| {
            let at = ptr.text_range().start() - cut;
            cut += ptr.text_range().len();
            SyntaxNodePtr::from_parts(ptr.kind(), TextRange::empty(at))
        })
        // alloc: as `ptrs`
        .collect();
    FileOutline {
        ast_id_map: ast_id_map.relative_without(&root, &outline, &ptrs),
        span_map: span_map.without(&ranges),
        // clone: an `Arc` count of the outline tree
        tree: outline.flat().expect("a cloned subtree is flat").0.clone(),
        cuts,
    }
}

/// The [`SignatureSyntax`] of the item at `id`, `elide`'s node of it left out when it is its
/// own anchor: read off its file's [`FileOutline`], and so the same after any edit of a body
/// left out of it. An item in such a body is not in the outline and is read off its file, as
/// the body it is in is. `None` for an item a macro writes.
fn item_syntax<N: AstIdNode>(db: &Db, id: AstId<N>, elide: Elide) -> Option<SignatureSyntax> {
    let HirFileId::FileId(file) = id.file_id else { return None };
    let outline = file_outline(db, file);
    if let Some(ptr) = outline.ast_id_map.try_get_erased(id.value.erase()) {
        let source = Source {
            file_id: id.file_id,
            span_map: &outline.span_map,
            ast_id_map: &outline.ast_id_map,
            cuts: &outline.cuts,
        };
        return SignatureSyntax::new(source, &ptr.to_node(&outline.root()), elide);
    }
    let SpanMap::RealSpanMap(span_map) = id.file_id.span_map(db) else { return None };
    let ast_id_map = id.file_id.ast_id_map(db);
    let source = Source { file_id: id.file_id, span_map, ast_id_map, cuts: &[] };
    SignatureSyntax::new(source, id.to_node(db).syntax(), elide)
}

/// The [`SignatureSyntax`] of `def`'s signature, `None` for an item a macro writes.
#[salsa::tracked(returns(ref))]
pub fn signature_syntax(db: &Db, def: GenericDefId) -> Option<SignatureSyntax> {
    let nothing: Elide = |_| None;
    let items: Elide = |it| it.child_where(|kind| kind == SyntaxKind::ASSOC_ITEM_LIST);
    let variants: Elide = |it| it.child_where(|kind| kind == SyntaxKind::VARIANT_LIST);
    match def {
        GenericDefId::AdtId(AdtId::StructId(it)) => item_syntax(db, it.lookup(db).id, nothing),
        GenericDefId::AdtId(AdtId::UnionId(it)) => item_syntax(db, it.lookup(db).id, nothing),
        GenericDefId::AdtId(AdtId::EnumId(it)) => item_syntax(db, it.lookup(db).id, variants),
        GenericDefId::FunctionId(it) => item_syntax(db, it.lookup(db).id, left_out),
        GenericDefId::ConstId(it) => item_syntax(db, it.lookup(db).id, left_out),
        GenericDefId::StaticId(it) => item_syntax(db, it.lookup(db).id, left_out),
        GenericDefId::ImplId(it) => item_syntax(db, it.lookup(db).id, items),
        GenericDefId::TraitId(it) => item_syntax(db, it.lookup(db).id, items),
        GenericDefId::TypeAliasId(it) => item_syntax(db, it.lookup(db).id, nothing),
    }
}

/// The [`SignatureSyntax`] of an enum with its variants, nothing left out: what its
/// [`EnumVariants`] and the [`VariantFields`] of its variants are lowered from. `None` for an
/// enum a macro writes.
#[salsa::tracked(returns(ref))]
pub fn variants_syntax(db: &Db, def: EnumId) -> Option<SignatureSyntax> {
    item_syntax(db, def.lookup(db).id, |_| None)
}

/// `source_map`, of `def`'s signature lowered from its [`signature_syntax`], moved to `item`,
/// where `def` is in its file.
fn moved_to_file<N: AstIdNode>(
    db: &Db,
    def: GenericDefId,
    item: AstId<N>,
    source_map: &ExpressionStoreSourceMap,
) -> ExpressionStoreSourceMap {
    let syntax = signature_syntax(db, def).as_ref();
    syntax.expect("a signature lowered from no syntax").moved(file_range(db, item), source_map)
}

/// Where `item` is in its file, read off its ast id: no walk of the tree to its node.
fn file_range<N: AstIdNode>(db: &Db, item: AstId<N>) -> TextRange {
    item.file_id.ast_id_map(db).get(item.value).syntax_node_ptr().text_range()
}

#[derive(Debug, PartialEq, Eq)]
pub struct StructSignature {
    pub name: Name,
    pub generic_params: GenericParams,
    pub store: ExpressionStore,
    pub flags: StructFlags,
    pub shape: FieldsShape,
}

// Each signature is one `Arc` (8 bytes of count before it) holding its store and generic
// params (72 bytes each today) and its own fields, whose bytes are what these assert:
// 160 bytes for a struct, union, enum or trait, 184 for a function or type alias, 88 for a
// const or static (no generic params), 160 for an impl.
const _: () = assert!(
    size_of::<StructSignature>() == size_of::<ExpressionStore>() + size_of::<GenericParams>() + 16
);
const _: () = assert!(
    size_of::<UnionSignature>() == size_of::<ExpressionStore>() + size_of::<GenericParams>() + 16
);
const _: () = assert!(
    size_of::<EnumSignature>() == size_of::<ExpressionStore>() + size_of::<GenericParams>() + 16
);
const _: () = assert!(size_of::<ConstSignature>() == size_of::<ExpressionStore>() + 16);
const _: () = assert!(size_of::<StaticSignature>() == size_of::<ExpressionStore>() + 16);
const _: () = assert!(
    size_of::<ImplSignature>() == size_of::<ExpressionStore>() + size_of::<GenericParams>() + 16
);
const _: () = assert!(
    size_of::<TraitSignature>() == size_of::<ExpressionStore>() + size_of::<GenericParams>() + 16
);
const _: () = assert!(
    size_of::<FunctionSignature>()
        == size_of::<ExpressionStore>() + size_of::<GenericParams>() + 40
);
const _: () = assert!(
    size_of::<TypeAliasSignature>()
        == size_of::<ExpressionStore>() + size_of::<GenericParams>() + 40
);
// One word of flags per signature, tested with a mask.
const _: () = assert!(size_of::<StructFlags>() == 2);
const _: () = assert!(size_of::<EnumFlags>() == 1);
const _: () = assert!(size_of::<ConstFlags>() == 1);
const _: () = assert!(size_of::<StaticFlags>() == 1);
const _: () = assert!(size_of::<ImplFlags>() == 1);
const _: () = assert!(size_of::<TraitFlags>() == 2);
const _: () = assert!(size_of::<FnFlags>() == 2);
const _: () = assert!(size_of::<TypeAliasFlags>() == 1);

bitflags! {
    #[derive(Debug, Copy, Clone, PartialEq, Eq)]
    pub struct StructFlags: u16 {
        /// Indicates whether this struct has `#[repr]`.
        const HAS_REPR = 1 << 0;
        /// Indicates whether the struct has a `#[rustc_has_incoherent_inherent_impls]` attribute.
        const RUSTC_HAS_INCOHERENT_INHERENT_IMPLS = 1 << 1;
        /// Indicates whether the struct has a `#[fundamental]` attribute.
        const FUNDAMENTAL      = 1 << 2;
        /// Indicates whether the struct is `PhantomData`.
        const IS_PHANTOM_DATA  = 1 << 3;
        /// Indicates whether this struct is `Box`.
        const IS_BOX           = 1 << 4;
        /// Indicates whether this struct is `ManuallyDrop`.
        const IS_MANUALLY_DROP = 1 << 5;
        /// Indicates whether this struct is `UnsafeCell`.
        const IS_UNSAFE_CELL   = 1 << 6;
        /// Indicates whether this struct is `UnsafePinned`.
        const IS_UNSAFE_PINNED = 1 << 7;
        /// Indicates whether this struct is `CovariantUnsafeCell`.
        const IS_COVARIANT_UNSAFE_CELL = 1 << 8;
        /// Indicates whether this struct is `f16b`, laid out as the backend's bfloat scalar.
        const IS_F16B = 1 << 9;
        /// Indicates whether this struct is `ppcf128`, laid out as the backend's IBM
        /// double-double scalar.
        const IS_PPC_F128 = 1 << 10;
    }
}

/// The lang items a struct is laid out or checked as no other struct is, with the flag each
/// sets, found by `STRUCT_LANG_ITEM_NAMES` (member `i` is entry `i`): one load for a struct with
/// a `#[lang]`, none for any other.
const STRUCT_LANG_ITEMS: [(Symbol, StructFlags); 8] = [
    (sym::phantom_data, StructFlags::IS_PHANTOM_DATA),
    (sym::owned_box, StructFlags::IS_BOX),
    (sym::manually_drop, StructFlags::IS_MANUALLY_DROP),
    (sym::unsafe_cell, StructFlags::IS_UNSAFE_CELL),
    (sym::covariant_unsafe_cell, StructFlags::IS_COVARIANT_UNSAFE_CELL),
    (sym::unsafe_pinned, StructFlags::IS_UNSAFE_PINNED),
    (sym::f16b, StructFlags::IS_F16B),
    (sym::ppcf128, StructFlags::IS_PPC_F128),
];

static STRUCT_LANG_ITEM_NAMES: intern::KnownNames = intern::KnownNames::new(&[
    sym::Declared::phantom_data,
    sym::Declared::owned_box,
    sym::Declared::manually_drop,
    sym::Declared::unsafe_cell,
    sym::Declared::covariant_unsafe_cell,
    sym::Declared::unsafe_pinned,
    sym::Declared::f16b,
    sym::Declared::ppcf128,
]);

impl StructFlags {
    /// The flags of a struct or union that its attributes set.
    fn from_attrs(attrs: AttrFlags) -> StructFlags {
        flags_from_attrs!(attrs, StructFlags {
            HAS_REPR => HAS_REPR,
            RUSTC_HAS_INCOHERENT_INHERENT_IMPLS => RUSTC_HAS_INCOHERENT_INHERENT_IMPLS,
            FUNDAMENTAL => FUNDAMENTAL,
        })
    }

    /// The flag of a struct that is the lang item `lang`.
    fn from_lang_item(lang: &Symbol) -> StructFlags {
        STRUCT_LANG_ITEM_NAMES
            .get(lang)
            .map_or(StructFlags::empty(), |at| STRUCT_LANG_ITEMS[usize::from(at)].1)
    }
}

#[salsa::tracked]
impl StructSignature {
    #[salsa::tracked(returns(deref))]
    pub fn of(db: &Db, id: StructId) -> Arc<Self> {
        if let Some(it) = crate::deps::signatures::served_struct(db, id) {
            return it;
        }
        match Self::relative(db, id) {
            Some((it, _)) => it.clone(), // clone: an `Arc` count, once per memo
            // clone: an `Arc` count, for an item a macro writes
            None => Self::with_source_map(db, id).0.clone(),
        }
    }

    /// The signature with where each part of it is written in its file.
    #[salsa::tracked(returns(ref), lru = 1024)]
    pub fn with_source_map(
        db: &Db,
        id: StructId,
    ) -> (Arc<Self>, ExpressionStoreSourceMap) {
        match Self::relative(db, id) {
            // clone: an `Arc` count, once per memo
            Some((it, map)) => (it.clone(), moved_to_file(db, id.into(), id.lookup(db).id, map)),
            None => Self::lower(db, id, id.lookup(db).source(db), None),
        }
    }

    /// The signature lowered from its [`signature_syntax`], with the source map of that tree;
    /// `None` for a struct a macro writes.
    #[salsa::tracked(returns(ref))]
    pub fn relative(
        db: &Db,
        id: StructId,
    ) -> Option<(Arc<Self>, ExpressionStoreSourceMap)> {
        let syntax = signature_syntax(db, id.into()).as_ref()?;
        Some(Self::lower(db, id, syntax.node(), Some(syntax.maps())))
    }

    fn lower(
        db: &Db,
        id: StructId,
        InFile { file_id, value: source }: InFile<ast::Struct>,
        maps: Maps<'_>,
    ) -> (Arc<Self>, ExpressionStoreSourceMap) {
        let loc = id.lookup(db);
        let attrs = AttrFlags::query(db, id.into());

        let mut flags = StructFlags::from_attrs(attrs);
        if let Some(lang) = attrs.lang_item_with_attrs(db, id.into()) {
            flags |= StructFlags::from_lang_item(&lang);
        }
        let shape = adt_shape(source.kind());

        let (store, generic_params, source_map) = lower_generic_params_within(
            db,
            loc.container,
            id.into(),
            file_id,
            source.generic_param_list(),
            source.where_clause(),
            LoweringMode::Analysis,
            maps,
        );
        (
            // alloc: the memo's value, one per signature
            Arc::new(StructSignature {
                generic_params,
                store,
                flags,
                shape,
                name: as_name_opt(source.name()),
            }),
            source_map,
        )
    }

    #[inline]
    pub fn repr(&self, db: &Db, id: StructId) -> Option<ReprOptions> {
        if self.flags.contains(StructFlags::HAS_REPR) {
            AttrFlags::repr_assume_has(db, id.into())
        } else {
            None
        }
    }
}

#[inline]
fn adt_shape(adt_kind: ast::StructKind) -> FieldsShape {
    match adt_kind {
        ast::StructKind::Record(_) => FieldsShape::Record,
        ast::StructKind::Tuple(_) => FieldsShape::Tuple,
        ast::StructKind::Unit => FieldsShape::Unit,
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct UnionSignature {
    pub name: Name,
    pub generic_params: GenericParams,
    pub store: ExpressionStore,
    pub flags: StructFlags,
}

#[salsa::tracked]
impl UnionSignature {
    #[salsa::tracked(returns(deref))]
    pub fn of(db: &Db, id: UnionId) -> Arc<Self> {
        if let Some(it) = crate::deps::signatures::served_union(db, id) {
            return it;
        }
        match Self::relative(db, id) {
            Some((it, _)) => it.clone(), // clone: an `Arc` count, once per memo
            // clone: an `Arc` count, for an item a macro writes
            None => Self::with_source_map(db, id).0.clone(),
        }
    }

    /// The signature with where each part of it is written in its file.
    #[salsa::tracked(returns(ref), lru = 1024)]
    pub fn with_source_map(
        db: &Db,
        id: UnionId,
    ) -> (Arc<Self>, ExpressionStoreSourceMap) {
        match Self::relative(db, id) {
            // clone: an `Arc` count, once per memo
            Some((it, map)) => (it.clone(), moved_to_file(db, id.into(), id.lookup(db).id, map)),
            None => Self::lower(db, id, id.lookup(db).source(db), None),
        }
    }

    /// The signature lowered from its [`signature_syntax`], with the source map of that tree;
    /// `None` for a union a macro writes.
    #[salsa::tracked(returns(ref))]
    pub fn relative(
        db: &Db,
        id: UnionId,
    ) -> Option<(Arc<Self>, ExpressionStoreSourceMap)> {
        let syntax = signature_syntax(db, id.into()).as_ref()?;
        Some(Self::lower(db, id, syntax.node(), Some(syntax.maps())))
    }

    fn lower(
        db: &Db,
        id: UnionId,
        InFile { file_id, value: source }: InFile<ast::Union>,
        maps: Maps<'_>,
    ) -> (Arc<Self>, ExpressionStoreSourceMap) {
        let loc = id.lookup(db);
        let flags = StructFlags::from_attrs(AttrFlags::query(db, id.into()));

        let (store, generic_params, source_map) = lower_generic_params_within(
            db,
            loc.container,
            id.into(),
            file_id,
            source.generic_param_list(),
            source.where_clause(),
            LoweringMode::Analysis,
            maps,
        );
        (
            // alloc: the memo's value, one per signature
            Arc::new(UnionSignature {
                generic_params,
                store,
                flags,
                name: as_name_opt(source.name()),
            }),
            source_map,
        )
    }

    #[inline]
    pub fn repr(&self, db: &Db, id: UnionId) -> Option<ReprOptions> {
        if self.flags.contains(StructFlags::HAS_REPR) {
            AttrFlags::repr_assume_has(db, id.into())
        } else {
            None
        }
    }
}

bitflags! {
    #[derive(Debug, Copy, Clone, PartialEq, Eq)]
    pub struct EnumFlags: u8 {
        /// Indicates whether this enum has `#[repr]`.
        const HAS_REPR = 1 << 0;
        /// Indicates whether the enum has a `#[rustc_has_incoherent_inherent_impls]` attribute.
        const RUSTC_HAS_INCOHERENT_INHERENT_IMPLS  = 1 << 1;
        /// Whether this enum has `#[fundamental]`.
        const FUNDAMENTAL = 1 << 2;
    }
}

impl EnumFlags {
    fn from_attrs(attrs: AttrFlags) -> EnumFlags {
        flags_from_attrs!(attrs, EnumFlags {
            HAS_REPR => HAS_REPR,
            RUSTC_HAS_INCOHERENT_INHERENT_IMPLS => RUSTC_HAS_INCOHERENT_INHERENT_IMPLS,
            FUNDAMENTAL => FUNDAMENTAL,
        })
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct EnumSignature {
    pub name: Name,
    pub generic_params: GenericParams,
    pub store: ExpressionStore,
    pub flags: EnumFlags,
}

#[salsa::tracked]
impl EnumSignature {
    #[salsa::tracked(returns(deref))]
    pub fn of(db: &Db, id: EnumId) -> Arc<Self> {
        if let Some(it) = crate::deps::signatures::served_enum(db, id) {
            return it;
        }
        match Self::relative(db, id) {
            Some((it, _)) => it.clone(), // clone: an `Arc` count, once per memo
            // clone: an `Arc` count, for an item a macro writes
            None => Self::with_source_map(db, id).0.clone(),
        }
    }

    /// The signature with where each part of it is written in its file.
    #[salsa::tracked(returns(ref), lru = 1024)]
    pub fn with_source_map(
        db: &Db,
        id: EnumId,
    ) -> (Arc<Self>, ExpressionStoreSourceMap) {
        match Self::relative(db, id) {
            // clone: an `Arc` count, once per memo
            Some((it, map)) => (it.clone(), moved_to_file(db, id.into(), id.lookup(db).id, map)),
            None => Self::lower(db, id, id.lookup(db).source(db), None),
        }
    }

    /// The signature lowered from its [`signature_syntax`] (its variants left out), with the
    /// source map of that tree; `None` for an enum a macro writes.
    #[salsa::tracked(returns(ref))]
    pub fn relative(
        db: &Db,
        id: EnumId,
    ) -> Option<(Arc<Self>, ExpressionStoreSourceMap)> {
        let syntax = signature_syntax(db, id.into()).as_ref()?;
        Some(Self::lower(db, id, syntax.node(), Some(syntax.maps())))
    }

    fn lower(
        db: &Db,
        id: EnumId,
        InFile { file_id, value: source }: InFile<ast::Enum>,
        maps: Maps<'_>,
    ) -> (Arc<Self>, ExpressionStoreSourceMap) {
        let loc = id.lookup(db);
        let flags = EnumFlags::from_attrs(AttrFlags::query(db, id.into()));

        let (store, generic_params, source_map) = lower_generic_params_within(
            db,
            loc.container,
            id.into(),
            file_id,
            source.generic_param_list(),
            source.where_clause(),
            LoweringMode::Analysis,
            maps,
        );

        (
            // alloc: the memo's value, one per signature
            Arc::new(EnumSignature {
                generic_params,
                store,
                flags,
                name: as_name_opt(source.name()),
            }),
            source_map,
        )
    }
}

impl EnumSignature {
    pub fn variant_body_type(db: &Db, id: EnumId) -> IntegerType {
        match AttrFlags::repr(db, id.into()) {
            Some(ReprOptions { int: Some(builtin), .. }) => builtin,
            _ => IntegerType::Pointer(true),
        }
    }

    #[inline]
    pub fn repr(&self, db: &Db, id: EnumId) -> Option<ReprOptions> {
        if self.flags.contains(EnumFlags::HAS_REPR) {
            AttrFlags::repr_assume_has(db, id.into())
        } else {
            None
        }
    }
}
bitflags::bitflags! {
    #[derive(Debug, Clone, Copy, Eq, PartialEq, Default)]
    pub struct ConstFlags: u8 {
        const HAS_BODY = 1 << 1;
        const RUSTC_ALLOW_INCOHERENT_IMPL = 1 << 7;
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct ConstSignature {
    pub name: Option<Name>,
    // generic_params: GenericParams,
    pub store: ExpressionStore,
    pub type_ref: TypeRefId,
    pub flags: ConstFlags,
}

#[salsa::tracked]
impl ConstSignature {
    #[salsa::tracked(returns(deref))]
    pub fn of(db: &Db, id: ConstId) -> Arc<Self> {
        if let Some(it) = crate::deps::signatures::served_const(db, id) {
            return it;
        }
        match Self::relative(db, id) {
            Some((it, _)) => it.clone(), // clone: an `Arc` count, once per memo
            // clone: an `Arc` count, for an item a macro writes
            None => Self::with_source_map(db, id).0.clone(),
        }
    }

    /// The signature with where each part of it is written in its file.
    #[salsa::tracked(returns(ref), lru = 1024)]
    pub fn with_source_map(
        db: &Db,
        id: ConstId,
    ) -> (Arc<Self>, ExpressionStoreSourceMap) {
        match Self::relative(db, id) {
            // clone: an `Arc` count, once per memo
            Some((it, map)) => (it.clone(), moved_to_file(db, id.into(), id.lookup(db).id, map)),
            None => Self::lower(db, id, id.lookup(db).source(db), None),
        }
    }

    /// The signature lowered from its [`signature_syntax`] (its body left out), with the
    /// source map of that tree; `None` for a const a macro writes.
    #[salsa::tracked(returns(ref))]
    pub fn relative(
        db: &Db,
        id: ConstId,
    ) -> Option<(Arc<Self>, ExpressionStoreSourceMap)> {
        let syntax = signature_syntax(db, id.into()).as_ref()?;
        Some(Self::lower(db, id, syntax.node(), Some(syntax.maps())))
    }

    fn lower(
        db: &Db,
        id: ConstId,
        source: InFile<ast::Const>,
        maps: Maps<'_>,
    ) -> (Arc<Self>, ExpressionStoreSourceMap) {
        let loc = id.lookup(db);

        let module = loc.container.module(db);
        let attrs = AttrFlags::query(db, id.into());
        let mut flags = flags_from_attrs!(attrs, ConstFlags {
            RUSTC_ALLOW_INCOHERENT_IMPL => RUSTC_ALLOW_INCOHERENT_IMPL,
        });
        flags.set(ConstFlags::HAS_BODY, source.value.body().is_some());

        let (store, source_map, type_ref) =
            lower_type_ref(db, module, source.as_ref().map(|it| it.ty()), maps);

        (
            // alloc: the memo's value, one per signature
            Arc::new(ConstSignature {
                store,
                type_ref,
                flags,
                name: source.value.name().map(|it| it.as_name()),
            }),
            source_map,
        )
    }
}

impl ConstSignature {
    pub fn has_body(&self) -> bool {
        self.flags.contains(ConstFlags::HAS_BODY)
    }
}

bitflags::bitflags! {
    #[derive(Debug, Clone, Copy, Eq, PartialEq, Default)]
    pub struct StaticFlags: u8 {
        const HAS_BODY = 1 << 1;
        const MUTABLE    = 1 << 3;
        const UNSAFE     = 1 << 4;
        const EXPLICIT_SAFE = 1 << 5;
        const EXTERN     = 1 << 6;
        const RUSTC_ALLOW_INCOHERENT_IMPL = 1 << 7;
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct StaticSignature {
    pub name: Name,

    // generic_params: GenericParams,
    pub store: ExpressionStore,
    pub type_ref: TypeRefId,
    pub flags: StaticFlags,
}

#[salsa::tracked]
impl StaticSignature {
    #[salsa::tracked(returns(deref))]
    pub fn of(db: &Db, id: StaticId) -> Arc<Self> {
        if let Some(it) = crate::deps::signatures::served_static(db, id) {
            return it;
        }
        match Self::relative(db, id) {
            Some((it, _)) => it.clone(), // clone: an `Arc` count, once per memo
            // clone: an `Arc` count, for an item a macro writes
            None => Self::with_source_map(db, id).0.clone(),
        }
    }

    /// The signature with where each part of it is written in its file.
    #[salsa::tracked(returns(ref), lru = 1024)]
    pub fn with_source_map(
        db: &Db,
        id: StaticId,
    ) -> (Arc<Self>, ExpressionStoreSourceMap) {
        match Self::relative(db, id) {
            // clone: an `Arc` count, once per memo
            Some((it, map)) => (it.clone(), moved_to_file(db, id.into(), id.lookup(db).id, map)),
            None => Self::lower(db, id, id.lookup(db).source(db), None),
        }
    }

    /// The signature lowered from its [`signature_syntax`] (its body left out), with the
    /// source map of that tree; `None` for a static a macro writes.
    #[salsa::tracked(returns(ref))]
    pub fn relative(
        db: &Db,
        id: StaticId,
    ) -> Option<(Arc<Self>, ExpressionStoreSourceMap)> {
        let syntax = signature_syntax(db, id.into()).as_ref()?;
        Some(Self::lower(db, id, syntax.node(), Some(syntax.maps())))
    }

    fn lower(
        db: &Db,
        id: StaticId,
        source: InFile<ast::Static>,
        maps: Maps<'_>,
    ) -> (Arc<Self>, ExpressionStoreSourceMap) {
        let loc = id.lookup(db);

        let module = loc.container.module(db);
        let attrs = AttrFlags::query(db, id.into());
        let mut flags = flags_from_attrs!(attrs, StaticFlags {
            RUSTC_ALLOW_INCOHERENT_IMPL => RUSTC_ALLOW_INCOHERENT_IMPL,
        });
        flags.set(StaticFlags::EXTERN, matches!(loc.container, ItemContainerId::ExternBlockId(_)));

        let [mut_, unsafe_, safe] =
            child_tokens(source.value.syntax(), [T![mut], T![unsafe], T![safe]]);
        flags.set(StaticFlags::HAS_BODY, source.value.body().is_some());
        flags.set(StaticFlags::MUTABLE, mut_);
        flags.set(StaticFlags::UNSAFE, unsafe_);
        flags.set(StaticFlags::EXPLICIT_SAFE, safe);

        let (store, source_map, type_ref) =
            lower_type_ref(db, module, source.as_ref().map(|it| it.ty()), maps);

        (
            // alloc: the memo's value, one per signature
            Arc::new(StaticSignature {
                store,
                type_ref,
                flags,
                name: as_name_opt(source.value.name()),
            }),
            source_map,
        )
    }
}

bitflags::bitflags! {
    #[derive(Debug, Clone, Copy, Eq, PartialEq, Default)]
    pub struct ImplFlags: u8 {
        const NEGATIVE = 1 << 1;
        const DEFAULT = 1 << 2;
        const UNSAFE = 1 << 3;
        /// `const impl Tr for T` (or the older `impl const Tr for T`).
        const CONST = 1 << 4;
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct ImplSignature {
    pub generic_params: GenericParams,
    pub store: ExpressionStore,
    pub self_ty: TypeRefId,
    pub target_trait: Option<TraitRef>,
    pub flags: ImplFlags,
}

#[salsa::tracked]
impl ImplSignature {
    #[salsa::tracked(returns(deref))]
    pub fn of(db: &Db, id: ImplId) -> Arc<Self> {
        if let Some(it) = crate::deps::signatures::served_impl(db, id) {
            return it;
        }
        match Self::relative(db, id) {
            Some((it, _)) => it.clone(), // clone: an `Arc` count, once per memo
            // clone: an `Arc` count, for an item a macro writes
            None => Self::with_source_map(db, id).0.clone(),
        }
    }

    /// The signature with where each part of it is written in its file.
    #[salsa::tracked(returns(ref), lru = 1024)]
    pub fn with_source_map(
        db: &Db,
        id: ImplId,
    ) -> (Arc<Self>, ExpressionStoreSourceMap) {
        match Self::relative(db, id) {
            // clone: an `Arc` count, once per memo
            Some((it, map)) => (it.clone(), moved_to_file(db, id.into(), id.lookup(db).id, map)),
            None => Self::lower(db, id, id.lookup(db).source(db), None),
        }
    }

    /// The signature lowered from its [`signature_syntax`] (its items left out), with the
    /// source map of that tree; `None` for an impl a macro writes.
    #[salsa::tracked(returns(ref))]
    pub fn relative(
        db: &Db,
        id: ImplId,
    ) -> Option<(Arc<Self>, ExpressionStoreSourceMap)> {
        let syntax = signature_syntax(db, id.into()).as_ref()?;
        Some(Self::lower(db, id, syntax.node(), Some(syntax.maps())))
    }

    fn lower(
        db: &Db,
        id: ImplId,
        src: InFile<ast::Impl>,
        maps: Maps<'_>,
    ) -> (Arc<Self>, ExpressionStoreSourceMap) {
        let loc = id.lookup(db);

        let [unsafe_, negative, default, const_] =
            child_tokens(src.value.syntax(), [T![unsafe], T![!], T![default], T![const]]);
        let mut flags = ImplFlags::empty();
        flags.set(ImplFlags::UNSAFE, unsafe_);
        flags.set(ImplFlags::NEGATIVE, negative);
        flags.set(ImplFlags::DEFAULT, default);
        flags.set(ImplFlags::CONST, const_);

        let (store, source_map, self_ty, target_trait, generic_params) =
            lower_impl(db, loc.container, src, id, maps);

        (
            // alloc: the memo's value, one per signature
            Arc::new(ImplSignature { store, generic_params, self_ty, target_trait, flags }),
            source_map,
        )
    }
}

impl ImplSignature {
    #[inline]
    pub fn is_negative(&self) -> bool {
        self.flags.contains(ImplFlags::NEGATIVE)
    }

    #[inline]
    pub fn is_default(&self) -> bool {
        self.flags.contains(ImplFlags::DEFAULT)
    }

    #[inline]
    pub fn is_const(&self) -> bool {
        self.flags.contains(ImplFlags::CONST)
    }
}

bitflags::bitflags! {
    #[derive(Debug, Clone, Copy, Eq, PartialEq, Default)]
    pub struct TraitFlags: u16 {
        const RUSTC_HAS_INCOHERENT_INHERENT_IMPLS = 1 << 1;
        const FUNDAMENTAL = 1 << 2;
        const UNSAFE = 1 << 3;
        const AUTO = 1 << 4;
        const SKIP_ARRAY_DURING_METHOD_DISPATCH = 1 << 5;
        const SKIP_BOXED_SLICE_DURING_METHOD_DISPATCH = 1 << 6;
        const RUSTC_PAREN_SUGAR = 1 << 7;
        const COINDUCTIVE = 1 << 8;
        const ALIAS = 1 << 9;
        /// `const trait Tr`.
        const CONST = 1 << 10;
    }
}

impl TraitFlags {
    fn from_attrs(attrs: AttrFlags) -> TraitFlags {
        flags_from_attrs!(attrs, TraitFlags {
            FUNDAMENTAL => FUNDAMENTAL,
            RUSTC_HAS_INCOHERENT_INHERENT_IMPLS => RUSTC_HAS_INCOHERENT_INHERENT_IMPLS,
            RUSTC_PAREN_SUGAR => RUSTC_PAREN_SUGAR,
            RUSTC_COINDUCTIVE => COINDUCTIVE,
            RUSTC_SKIP_ARRAY_DURING_METHOD_DISPATCH => SKIP_ARRAY_DURING_METHOD_DISPATCH,
            RUSTC_SKIP_BOXED_SLICE_DURING_METHOD_DISPATCH => SKIP_BOXED_SLICE_DURING_METHOD_DISPATCH,
        })
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct TraitSignature {
    pub name: Name,
    pub generic_params: GenericParams,
    pub store: ExpressionStore,
    pub flags: TraitFlags,
}

#[salsa::tracked]
impl TraitSignature {
    #[salsa::tracked(returns(deref))]
    pub fn of(db: &Db, id: TraitId) -> Arc<Self> {
        if let Some(it) = crate::deps::signatures::served_trait(db, id) {
            return it;
        }
        match Self::relative(db, id) {
            Some((it, _)) => it.clone(), // clone: an `Arc` count, once per memo
            // clone: an `Arc` count, for an item a macro writes
            None => Self::with_source_map(db, id).0.clone(),
        }
    }

    /// The signature with where each part of it is written in its file.
    #[salsa::tracked(returns(ref), lru = 1024)]
    pub fn with_source_map(
        db: &Db,
        id: TraitId,
    ) -> (Arc<Self>, ExpressionStoreSourceMap) {
        match Self::relative(db, id) {
            // clone: an `Arc` count, once per memo
            Some((it, map)) => (it.clone(), moved_to_file(db, id.into(), id.lookup(db).id, map)),
            None => Self::lower(db, id, id.lookup(db).source(db), None),
        }
    }

    /// The signature lowered from its [`signature_syntax`] (its items left out), with the
    /// source map of that tree; `None` for a trait a macro writes.
    #[salsa::tracked(returns(ref))]
    pub fn relative(
        db: &Db,
        id: TraitId,
    ) -> Option<(Arc<Self>, ExpressionStoreSourceMap)> {
        let syntax = signature_syntax(db, id.into()).as_ref()?;
        Some(Self::lower(db, id, syntax.node(), Some(syntax.maps())))
    }

    fn lower(
        db: &Db,
        id: TraitId,
        source: InFile<ast::Trait>,
        maps: Maps<'_>,
    ) -> (Arc<Self>, ExpressionStoreSourceMap) {
        let loc = id.lookup(db);

        let mut flags = TraitFlags::from_attrs(AttrFlags::query(db, id.into()));
        let [auto, unsafe_, alias, const_] = child_tokens(
            source.value.syntax(),
            [T![auto], T![unsafe], T![=], T![const]],
        );
        if auto {
            flags |= TraitFlags::AUTO | TraitFlags::COINDUCTIVE;
        }
        flags.set(TraitFlags::UNSAFE, unsafe_);
        flags.set(TraitFlags::ALIAS, alias);
        flags.set(TraitFlags::CONST, const_);

        let name = as_name_opt(source.value.name());
        let (store, source_map, generic_params) = lower_trait(db, loc.container, source, id, maps);

        // alloc: the memo's value, one per signature
        (Arc::new(TraitSignature { store, generic_params, flags, name }), source_map)
    }
}

bitflags! {
    #[derive(Debug, Clone, Copy, Eq, PartialEq, Default)]
    pub struct FnFlags: u16 {
        const HAS_BODY = 1 << 1;
        const DEFAULT = 1 << 2;
        const CONST = 1 << 3;
        const ASYNC = 1 << 4;
        const GEN = 1 << 5;
        const UNSAFE = 1 << 6;
        const HAS_VARARGS = 1 << 7;
        const RUSTC_ALLOW_INCOHERENT_IMPL = 1 << 8;
        const HAS_SELF_PARAM = 1 << 9;
        /// The `#[target_feature]` attribute is necessary to check safety (with RFC 2396),
        /// but keeping it for all functions will consume a lot of memory when there are
        /// only very few functions with it. So we only encode its existence here, and lookup
        /// it if needed.
        const HAS_TARGET_FEATURE = 1 << 10;
        /// `#[rustc_deprecated_safe_2024]` on an `unsafe fn`, which keeps `UNSAFE`: its body
        /// is an unsafe function's, and a call of it before edition 2024 needs no `unsafe`
        /// block, only the `deprecated_safe_2024` lint (rustc's `check_unsafety`).
        const DEPRECATED_SAFE_2024 = 1 << 11;
        const EXPLICIT_SAFE = 1 << 12;
        const HAS_LEGACY_CONST_GENERICS = 1 << 13;
        const RUSTC_INTRINSIC = 1 << 14;
    }
}

impl FnFlags {
    /// The flags of a function its attributes and its `unsafe` keyword set.
    fn from_attrs(attrs: AttrFlags, unsafe_: bool) -> FnFlags {
        let mut flags = flags_from_attrs!(attrs, FnFlags {
            RUSTC_ALLOW_INCOHERENT_IMPL => RUSTC_ALLOW_INCOHERENT_IMPL,
            HAS_TARGET_FEATURE => HAS_TARGET_FEATURE,
            RUSTC_INTRINSIC => RUSTC_INTRINSIC,
            HAS_LEGACY_CONST_GENERICS => HAS_LEGACY_CONST_GENERICS,
            RUSTC_DEPRECATED_SAFE_2024 => DEPRECATED_SAFE_2024,
        });
        if !unsafe_ {
            flags.remove(FnFlags::DEPRECATED_SAFE_2024);
        }
        flags.set(FnFlags::UNSAFE, unsafe_);
        flags
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct FunctionSignature {
    pub name: Name,
    pub generic_params: GenericParams,
    pub store: ExpressionStore,
    pub params: Box<[TypeRefId]>,
    pub ret_type: Option<TypeRefId>,
    pub abi: ExternAbi,
    pub flags: FnFlags,
}

#[salsa::tracked]
impl FunctionSignature {
    #[salsa::tracked(returns(deref))]
    pub fn of(db: &Db, id: FunctionId) -> Arc<Self> {
        if let Some(it) = crate::deps::signatures::served_function(db, id) {
            return it;
        }
        match Self::relative(db, id) {
            Some((it, _)) => it.clone(), // clone: an `Arc` count, once per memo
            // clone: an `Arc` count, for an item a macro writes
            None => Self::with_source_map(db, id).0.clone(),
        }
    }

    /// The signature with where each part of it is written in its file.
    #[salsa::tracked(returns(ref), lru = 1024)]
    pub fn with_source_map(
        db: &Db,
        id: FunctionId,
    ) -> (Arc<Self>, ExpressionStoreSourceMap) {
        match Self::relative(db, id) {
            // clone: an `Arc` count, once per memo
            Some((it, map)) => (it.clone(), moved_to_file(db, id.into(), id.lookup(db).id, map)),
            None => Self::lower(db, id, id.lookup(db).source(db), None),
        }
    }

    /// The signature lowered from its [`signature_syntax`] (its body left out), with the
    /// source map of that tree; `None` for a function a macro writes.
    #[salsa::tracked(returns(ref))]
    pub fn relative(
        db: &Db,
        id: FunctionId,
    ) -> Option<(Arc<Self>, ExpressionStoreSourceMap)> {
        let syntax = signature_syntax(db, id.into()).as_ref()?;
        Some(Self::lower(db, id, syntax.node(), Some(syntax.maps())))
    }

    fn lower(
        db: &Db,
        id: FunctionId,
        source: InFile<ast::Fn>,
        maps: Maps<'_>,
    ) -> (Arc<Self>, ExpressionStoreSourceMap) {
        let loc = id.lookup(db);
        let module = loc.container.module(db);

        let attrs = AttrFlags::query(db, id.into());
        let [unsafe_, async_, gen_, const_, default, safe] = child_tokens(
            source.value.syntax(),
            [T![unsafe], T![async], T![gen], T![const], T![default], T![safe]],
        );
        let mut flags = FnFlags::from_attrs(attrs, unsafe_);
        flags.set(FnFlags::ASYNC, async_);
        flags.set(FnFlags::GEN, gen_);
        flags.set(FnFlags::CONST, const_);
        flags.set(FnFlags::DEFAULT, default);
        flags.set(FnFlags::EXPLICIT_SAFE, safe);
        flags.set(FnFlags::HAS_BODY, source.value.body().is_some());

        let name = as_name_opt(source.value.name());
        let abi = source
            .value
            .abi()
            .map(|abi| {
                abi.abi_string()
                    .and_then(|abi| abi.text_without_quotes().parse().ok())
                    .unwrap_or(ExternAbi::FALLBACK)
            })
            .or_else(|| match loc.container {
                ItemContainerId::ExternBlockId(extern_block) => {
                    Some(extern_block_abi(db, extern_block))
                }
                ItemContainerId::ModuleId(_)
                | ItemContainerId::ImplId(_)
                | ItemContainerId::TraitId(_) => None,
            })
            .unwrap_or(ExternAbi::Rust);
        let (store, source_map, generic_params, params, ret_type, self_param, variadic) =
            lower_function(db, module, source, id, maps);
        flags.set(FnFlags::HAS_SELF_PARAM, self_param);
        flags.set(FnFlags::HAS_VARARGS, variadic);
        (
            // alloc: the memo's value, one per signature
            Arc::new(FunctionSignature {
                generic_params,
                store,
                params,
                ret_type,
                abi,
                flags,
                name,
            }),
            source_map,
        )
    }
}

impl FunctionSignature {
    pub fn has_body(&self) -> bool {
        self.flags.contains(FnFlags::HAS_BODY)
    }

    /// True if the first param is `self`. This is relevant to decide whether this
    /// can be called as a method.
    pub fn has_self_param(&self) -> bool {
        self.flags.contains(FnFlags::HAS_SELF_PARAM)
    }

    pub fn is_default(&self) -> bool {
        self.flags.contains(FnFlags::DEFAULT)
    }

    pub fn is_const(&self) -> bool {
        self.flags.contains(FnFlags::CONST)
    }

    pub fn is_async(&self) -> bool {
        self.flags.contains(FnFlags::ASYNC)
    }

    pub fn is_gen(&self) -> bool {
        self.flags.contains(FnFlags::GEN)
    }

    pub fn is_unsafe(&self) -> bool {
        self.flags.contains(FnFlags::UNSAFE)
    }

    pub fn is_deprecated_safe_2024(&self) -> bool {
        self.flags.contains(FnFlags::DEPRECATED_SAFE_2024)
    }

    pub fn is_safe(&self) -> bool {
        self.flags.contains(FnFlags::EXPLICIT_SAFE)
    }

    pub fn is_varargs(&self) -> bool {
        self.flags.contains(FnFlags::HAS_VARARGS)
    }

    pub fn has_target_feature(&self) -> bool {
        self.flags.contains(FnFlags::HAS_TARGET_FEATURE)
    }

    #[inline]
    pub fn legacy_const_generics_indices<'db>(
        &self,
        db: &'db Db,
        id: FunctionId,
    ) -> Option<&'db [u32]> {
        if !self.flags.contains(FnFlags::HAS_LEGACY_CONST_GENERICS) {
            return None;
        }

        AttrFlags::legacy_const_generic_indices(db, id).as_deref()
    }

    pub fn is_intrinsic(db: &Db, id: FunctionId) -> bool {
        let data = FunctionSignature::of(db, id);
        data.flags.contains(FnFlags::RUSTC_INTRINSIC)
    }
}

bitflags! {
    #[derive(Debug, Clone, Copy, Eq, PartialEq, Default)]
    pub struct TypeAliasFlags: u8 {
        const RUSTC_HAS_INCOHERENT_INHERENT_IMPL = 1 << 1;
        const IS_EXTERN = 1 << 6;
        const RUSTC_ALLOW_INCOHERENT_IMPL = 1 << 7;
    }
}

impl TypeAliasFlags {
    fn from_attrs(attrs: AttrFlags) -> TypeAliasFlags {
        flags_from_attrs!(attrs, TypeAliasFlags {
            RUSTC_HAS_INCOHERENT_INHERENT_IMPLS => RUSTC_HAS_INCOHERENT_INHERENT_IMPL,
            RUSTC_ALLOW_INCOHERENT_IMPL => RUSTC_ALLOW_INCOHERENT_IMPL,
        })
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct TypeAliasSignature {
    pub name: Name,
    pub generic_params: GenericParams,
    pub store: ExpressionStore,
    pub bounds: Box<[TypeBound]>,
    pub ty: Option<TypeRefId>,
    pub flags: TypeAliasFlags,
}

#[salsa::tracked]
impl TypeAliasSignature {
    #[salsa::tracked(returns(deref))]
    pub fn of(db: &Db, id: TypeAliasId) -> Arc<Self> {
        if let Some(it) = crate::deps::signatures::served_type_alias(db, id) {
            return it;
        }
        match Self::relative(db, id) {
            Some((it, _)) => it.clone(), // clone: an `Arc` count, once per memo
            // clone: an `Arc` count, for an item a macro writes
            None => Self::with_source_map(db, id).0.clone(),
        }
    }

    /// The signature with where each part of it is written in its file.
    #[salsa::tracked(returns(ref), lru = 1024)]
    pub fn with_source_map(
        db: &Db,
        id: TypeAliasId,
    ) -> (Arc<Self>, ExpressionStoreSourceMap) {
        match Self::relative(db, id) {
            // clone: an `Arc` count, once per memo
            Some((it, map)) => (it.clone(), moved_to_file(db, id.into(), id.lookup(db).id, map)),
            None => Self::lower(db, id, id.lookup(db).source(db), None),
        }
    }

    /// The signature lowered from its [`signature_syntax`], with the source map of that tree;
    /// `None` for a type alias a macro writes.
    #[salsa::tracked(returns(ref))]
    pub fn relative(
        db: &Db,
        id: TypeAliasId,
    ) -> Option<(Arc<Self>, ExpressionStoreSourceMap)> {
        let syntax = signature_syntax(db, id.into()).as_ref()?;
        Some(Self::lower(db, id, syntax.node(), Some(syntax.maps())))
    }

    fn lower(
        db: &Db,
        id: TypeAliasId,
        source: InFile<ast::TypeAlias>,
        maps: Maps<'_>,
    ) -> (Arc<Self>, ExpressionStoreSourceMap) {
        let loc = id.lookup(db);

        let mut flags = TypeAliasFlags::from_attrs(AttrFlags::query(db, id.into()));
        flags.set(
            TypeAliasFlags::IS_EXTERN,
            matches!(loc.container, ItemContainerId::ExternBlockId(_)),
        );
        let name = as_name_opt(source.value.name());
        let (store, source_map, generic_params, bounds, ty) =
            lower_type_alias(db, loc.container, source, id, maps);

        (
            // alloc: the memo's value, one per signature
            Arc::new(TypeAliasSignature { store, generic_params, flags, bounds, name, ty }),
            source_map,
        )
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct FunctionBody {
    pub store: ExpressionStore,
    pub parameters: Box<[PatId]>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct VariantFieldsBody {
    pub store: ExpressionStore,
    pub fields: Box<[Option<ExprId>]>,
}

/// A single field of an enum variant or struct
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldData {
    pub name: Name,
    pub type_ref: TypeRefId,
    pub visibility: RawVisibility,
    pub is_unsafe: bool,
    pub default_value: Option<ConstRef>,
}

// 40 bytes per field with today's 16-byte `RawVisibility`, in one array per variant.
const _: () = assert!(size_of::<FieldData>() == size_of::<RawVisibility>() + 24);

pub type LocalFieldId = Idx<FieldData>;

#[derive(Debug, PartialEq, Eq)]
pub struct VariantFields {
    fields: Arena<FieldData>,
    pub store: ExpressionStore,
    pub shape: FieldsShape,
}

// The field array (24 bytes), the shape and the store (72 bytes today): 104 bytes in one `Arc`.
const _: () = assert!(size_of::<VariantFields>() == size_of::<ExpressionStore>() + 32);

/// What the fields of `id` are lowered from: the [`variants_syntax`] of an enum variant's
/// enum, the [`signature_syntax`] of a struct or union. `None` for one a macro writes.
fn variant_syntax(db: &Db, id: VariantId) -> Option<&SignatureSyntax> {
    match id {
        VariantId::EnumVariantId(it) => variants_syntax(db, it.lookup(db).parent).as_ref(),
        VariantId::StructId(it) => signature_syntax(db, it.into()).as_ref(),
        VariantId::UnionId(it) => signature_syntax(db, it.into()).as_ref(),
    }
}

#[salsa::tracked]
impl VariantFields {
    /// The fields with where each part of them is written in their file.
    #[salsa::tracked(returns(ref), lru = 1024)]
    pub fn with_source_map(
        db: &Db,
        id: VariantId,
    ) -> (Arc<Self>, ExpressionStoreSourceMap) {
        let Some((it, map)) = Self::relative(db, id) else { return Self::lower(db, id, None) };
        let file = match id {
            VariantId::EnumVariantId(it) => file_range(db, it.lookup(db).parent.lookup(db).id),
            VariantId::StructId(it) => file_range(db, it.lookup(db).id),
            VariantId::UnionId(it) => file_range(db, it.lookup(db).id),
        };
        let syntax = variant_syntax(db, id).expect("fields lowered from no syntax");
        (it.clone(), syntax.moved(file, map)) // clone: an `Arc` count, once per memo
    }

    /// The fields lowered from their [`variant_syntax`], with the source map of that tree;
    /// `None` for a variant, struct or union a macro writes.
    #[salsa::tracked(returns(ref))]
    pub fn relative(
        db: &Db,
        id: VariantId,
    ) -> Option<(Arc<Self>, ExpressionStoreSourceMap)> {
        let syntax = variant_syntax(db, id)?;
        Some(Self::lower(db, id, Some(syntax)))
    }

    fn lower(
        db: &Db,
        id: VariantId,
        syntax: Option<&SignatureSyntax>,
    ) -> (Arc<Self>, ExpressionStoreSourceMap) {
        let maps = syntax.map(SignatureSyntax::maps);
        let (shape, result) = match id {
            VariantId::EnumVariantId(id) => {
                let loc = id.lookup(db);
                let parent = loc.parent.lookup(db);
                let source = match syntax {
                    Some(syntax) => {
                        let variant = syntax.ast_id_map.get(loc.id.value).to_node(&syntax.root());
                        InFile::new(syntax.file_id, variant)
                    }
                    None => loc.source(db),
                };
                let shape = adt_shape(source.value.kind());
                let enum_vis = Some(source.value.parent_enum().visibility());
                let fields = lower_field_list(
                    db,
                    parent.container,
                    source.map(|src| src.field_list()),
                    enum_vis,
                    maps,
                );
                (shape, fields)
            }
            VariantId::StructId(id) => {
                let loc = id.lookup(db);
                let source = syntax.map_or_else(|| loc.source(db), SignatureSyntax::node);
                let shape = adt_shape(source.value.kind());
                let fields = lower_field_list(
                    db,
                    loc.container,
                    source.map(|src| src.field_list()),
                    None,
                    maps,
                );
                (shape, fields)
            }
            VariantId::UnionId(id) => {
                let loc = id.lookup(db);
                let source = syntax.map_or_else(|| loc.source(db), SignatureSyntax::node);
                let fields = lower_field_list(
                    db,
                    loc.container,
                    source.map(|src| src.record_field_list().map(ast::FieldList::RecordFieldList)),
                    None,
                    maps,
                );
                (FieldsShape::Record, fields)
            }
        };
        match result {
            Some((fields, store, source_map)) => {
                // alloc: the memo's value, one per signature
                (Arc::new(VariantFields { fields, store, shape }), source_map)
            }
            None => (
                // alloc: the memo's value, one per signature
                Arc::new(VariantFields {
                    fields: Arena::default(),
                    // clone: of the empty store, which allocates nothing
                    store: ExpressionStore::empty().clone(),
                    shape,
                }),
                ExpressionStoreSourceMap::default(),
            ),
        }
    }

    #[salsa::tracked(returns(deref))]
    pub fn of(db: &Db, id: VariantId) -> Arc<Self> {
        if let Some(it) = crate::deps::signatures::served_variant_fields(db, id) {
            return it;
        }
        match Self::relative(db, id) {
            Some((it, _)) => it.clone(), // clone: an `Arc` count, once per memo
            // clone: an `Arc` count, for an item a macro writes
            None => Self::with_source_map(db, id).0.clone(),
        }
    }
}

impl VariantFields {
    /// Fields read back from a dependency image.
    pub(crate) fn from_parts(
        fields: Arena<FieldData>,
        store: ExpressionStore,
        shape: FieldsShape,
    ) -> VariantFields {
        VariantFields { fields, store, shape }
    }

    pub fn len(&self) -> usize {
        self.fields.len()
    }

    pub fn fields(&self) -> &Arena<FieldData> {
        &self.fields
    }

    pub fn field(&self, name: &Name) -> Option<LocalFieldId> {
        self.fields().iter().find_map(|(id, data)| if &data.name == name { Some(id) } else { None })
    }
}

fn lower_field_list(
    db: &Db,
    module: ModuleId,
    fields: InFile<Option<ast::FieldList>>,
    override_visibility: Option<Option<ast::Visibility>>,
    maps: Maps<'_>,
) -> Option<(Arena<FieldData>, ExpressionStore, ExpressionStoreSourceMap)> {
    let file_id = fields.file_id;
    match fields.value? {
        ast::FieldList::RecordFieldList(fields) => lower_fields(
            db,
            module,
            fields.fields().count(),
            InFile::new(file_id, fields.fields().map(|field| (field.ty(), field))),
            |_, field| as_name_opt(field.name()),
            |field| field.eq_token().and_then(|_| field.default_val()),
            override_visibility,
            maps,
        ),
        ast::FieldList::TupleFieldList(fields) => lower_fields(
            db,
            module,
            fields.fields().count(),
            InFile::new(file_id, fields.fields().map(|field| (field.ty(), field))),
            |idx, _| Name::new_tuple_field(idx),
            |_| None,
            override_visibility,
            maps,
        ),
    }
}

/// The `count` fields of a list, written with their types; `None` when it has none written,
/// before anything is set up to lower them.
fn lower_fields<Field: ast::HasAttrs + ast::HasVisibility>(
    db: &Db,
    module: ModuleId,
    count: usize,
    fields: InFile<impl Iterator<Item = (Option<ast::Type>, Field)>>,
    mut field_name: impl FnMut(usize, &Field) -> Name,
    mut default_value: impl FnMut(&Field) -> Option<ast::ConstArg>,
    override_visibility: Option<Option<ast::Visibility>>,
    maps: Maps<'_>,
) -> Option<(Arena<FieldData>, ExpressionStore, ExpressionStoreSourceMap)> {
    if count == 0 {
        return None;
    }
    let cfg_options = module.krate(db).cfg_options(db);
    let mut col =
        ExprCollector::with_maps(db, module, fields.file_id, crate::LoweringMode::Analysis, maps);
    let override_visibility = override_visibility.map(|vis| {
        LazyCell::new(|| {
            let span_map = match maps {
                Some((span_map, _)) => SpanMap::RealSpanMap(span_map),
                None => fields.file_id.span_map(db),
            };
            visibility_from_ast(db, vis, &mut |range| span_map.span_for_range(range).ctx)
        })
    });

    let mut arena = Arena::with_capacity(count);
    let mut idx = 0;
    for (ty, field) in fields.value {
        match AttrFlags::is_cfg_enabled_for(&field, cfg_options) {
            Ok(()) => {
                let type_ref =
                    col.lower_type_ref_opt(ty, &mut ExprCollector::impl_trait_error_allocator);
                let visibility = override_visibility.as_ref().map_or_else(
                    || {
                        visibility_from_ast(db, field.visibility(), &mut |range| {
                            col.span_map().span_for_range(range).ctx
                        })
                    },
                    |it| RawVisibility::clone(it),
                );
                let [is_unsafe] = child_tokens(field.syntax(), [T![unsafe]]);
                let name = field_name(idx, &field);
                let default_value = default_value(&field).map(|expr| col.lower_const_arg(expr));

                arena.alloc(FieldData { name, type_ref, visibility, is_unsafe, default_value });
                idx += 1;
            }
            Err(cfg) => {
                col.store.diagnostics.push(
                    crate::expr_store::ExpressionStoreDiagnostics::InactiveCode {
                        node: InFile::new(fields.file_id, SyntaxNodePtr::new(field.syntax())),
                        cfg,
                    },
                );
            }
        }
    }
    let (store, source_map) = col.store.finish();
    if arena.len() < count {
        arena.shrink_to_fit();
    }
    Some((arena, store, source_map))
}

#[derive(Debug, PartialEq, Eq)]
pub struct InactiveEnumVariantCode {
    pub cfg: CfgExpr,
    pub opts: CfgOptions,
    pub ast_id: span::FileAstId<ast::Variant>,
}

#[derive(Debug)]
pub struct EnumVariants {
    pub variants: FxIndexMap<Name, (EnumVariantId, FieldsShape)>,
    /// For each of [`EnumVariants::variants`], at the same index, whether it is written with
    /// an explicit discriminant (`= expr`): rustc's `VariantDiscr::Explicit`, read off the
    /// syntax, so that nothing a signature asks lowers the discriminant's body.
    explicit_discriminant: VariantBits,
}

/// One bit per variant, by index, kept inline up to 64 variants: no allocation for almost
/// every enum. Bits past the last variant are zero, so equal sets are equal words.
type VariantBits = Bits<SmallVec<[u64; 1]>>;

// The inline word and its length, or a heap pointer and its length, plus the capacity.
const _: () = assert!(size_of::<VariantBits>() == 24);

// The order of the variants is part of the enum: discriminants and layout follow it. Two
// `IndexMap`s with the same entries are equal whatever their order.
impl PartialEq for EnumVariants {
    fn eq(&self, other: &Self) -> bool {
        self.variants.len() == other.variants.len()
            && self.variants.iter().eq(other.variants.iter())
            && self.explicit_discriminant == other.explicit_discriminant
    }
}

impl Eq for EnumVariants {}

#[salsa::tracked]
impl EnumVariants {
    #[salsa::tracked(returns(ref))]
    pub(crate) fn of(
        db: &Db,
        e: EnumId,
    ) -> (EnumVariants, ThinVec<InactiveEnumVariantCode>) {
        if let Some(it) = crate::deps::signatures::served_enum_variants(db, e) {
            return it;
        }
        Self::lower(db, e)
    }
}

impl EnumVariants {
    /// The variants lowered from syntax: what [`EnumVariants::of`] gives when no image serves
    /// them, and what checking an image compares its record with.
    pub(crate) fn lower(db: &Db, e: EnumId) -> (EnumVariants, ThinVec<InactiveEnumVariantCode>) {
        let loc = e.lookup(db);
        // The relative tree's ast ids are the file's: what a variant is interned by.
        let (source, ast_id_map) = match variants_syntax(db, e) {
            Some(syntax) => (syntax.node::<ast::Enum>(), &syntax.ast_id_map),
            None => {
                let source = loc.source(db);
                let ast_id_map = source.file_id.ast_id_map(db);
                (source, ast_id_map)
            }
        };

        let mut diagnostics = ThinVec::new();
        let cfg_options = loc.container.krate(db).cfg_options(db);
        let Some(list) = source.value.variant_list() else {
            let variants = FxIndexMap::default();
            let explicit_discriminant = VariantBits::default();
            return (EnumVariants { variants, explicit_discriminant }, ThinVec::new());
        };
        let count = list.variants().count();
        let mut variants = FxIndexMap::with_capacity_and_hasher(count, Default::default());
        let mut explicit_discriminant = VariantBits::new(count); // alloc: inline up to 64 variants
        for variant in list.variants() {
            let ast_id = ast_id_map.ast_id(&variant);
            match AttrFlags::is_cfg_enabled_for(&variant, cfg_options) {
                Ok(()) => {
                    let name = as_name_opt(variant.name());
                    let id = source.with_value(ast_id);
                    // clone: a `Symbol` count: the loc and the map each keep the name
                    let enum_variant = EnumVariantLoc { id, parent: e, name: name.clone() }.intern(db);
                    let shape = adt_shape(variant.kind());
                    // The expression the variant's body is lowered from, when it has one.
                    let explicit = variant.const_arg().and_then(|it| it.expr()).is_some();
                    // A name written twice keeps its first place and the last variant's value.
                    let (index, _) = variants.insert_full(name, (enum_variant, shape));
                    explicit_discriminant.set(index, explicit);
                }
                Err(cfg) => {
                    diagnostics.push(InactiveEnumVariantCode {
                        ast_id,
                        cfg,
                        // clone: error path, a cfg'd-out variant's diagnostic
                        opts: cfg_options.clone(),
                    });
                }
            }
        }
        if variants.len() < count {
            variants.shrink_to_fit();
            explicit_discriminant.truncate(variants.len());
            explicit_discriminant.shrink_to_fit();
        }
        diagnostics.shrink_to_fit();

        (EnumVariants { variants, explicit_discriminant }, diagnostics)
    }

    /// Variants read back from a dependency image; `explicit[i]` is whether variant `i` is
    /// written with `= expr`.
    pub(crate) fn from_parts(
        variants: FxIndexMap<Name, (EnumVariantId, FieldsShape)>,
        explicit: &[bool],
    ) -> EnumVariants {
        let mut explicit_discriminant = VariantBits::new(variants.len()); // alloc: inline up to 64 variants
        for (index, &it) in explicit.iter().enumerate() {
            explicit_discriminant.set(index, it);
        }
        EnumVariants { variants, explicit_discriminant }
    }
}

impl EnumVariants {
    /// Whether `variant` is written with an explicit discriminant, `= expr`. Read off the
    /// syntax when the variants are lowered; its body is not lowered.
    pub fn has_explicit_discriminant(&self, variant: EnumVariantId) -> bool {
        self.variants
            .values()
            .position(|&(it, _)| it == variant)
            .is_some_and(|index| self.explicit_discriminant.contains(index))
    }

    /// Each variant with whether it is written with an explicit discriminant.
    pub fn with_explicit_discriminant(
        &self,
    ) -> impl Iterator<Item = (EnumVariantId, FieldsShape, bool)> + '_ {
        self.variants
            .values()
            .enumerate()
            .map(|(index, &(id, shape))| (id, shape, self.explicit_discriminant.contains(index)))
    }

    pub fn variant(&self, name: &Name) -> Option<EnumVariantId> {
        self.variants.get(name).map(|&(id, _)| id)
    }

    pub fn variant_name_by_id(&self, variant_id: EnumVariantId) -> Option<Name> {
        self.variants
            .iter()
            // clone: a `Symbol` count, the name the caller asked for
            .find_map(|(name, (id, _))| if *id == variant_id { Some(name.clone()) } else { None })
    }

    // [Adopted from rustc](https://github.com/rust-lang/rust/blob/bd53aa3bf7a24a70d763182303bd75e5fc51a9af/compiler/rustc_middle/src/ty/adt.rs#L446-L448)
    pub fn is_payload_free(&self, db: &Db) -> bool {
        self.with_explicit_discriminant().all(|(v, shape, explicit)| {
            // The condition check order is slightly modified from rustc
            // to improve performance by early returning with relatively fast checks

            // The outer if condition is whether this variant has const ctor or not
            if !matches!(shape, FieldsShape::Unit) {
                let variant = v.fields(db);
                if !variant.fields().is_empty() {
                    return false;
                }

                if explicit {
                    return false;
                }
            }
            true
        })
    }
}

#[salsa::tracked(returns(copy))]
pub(crate) fn extern_block_abi(db: &Db, extern_block: ExternBlockId) -> ExternAbi {
    let source = extern_block.lookup(db).source(db);
    source
        .value
        .abi()
        .and_then(|abi| abi.abi_string()?.text_without_quotes().parse().ok())
        .unwrap_or(ExternAbi::FALLBACK)
}

#[cfg(test)]
mod tests {
    use blaze_ids::rng::SplitMix64;
    use syntax::{AstNode, Edition, NodeOrToken, SourceFile, T, ast};

    use super::*;

    /// Every single bit, none, all, and seeded random words.
    fn attr_inputs() -> Vec<AttrFlags> {
        let mut rng = SplitMix64::new(0x5167_5f66_6c61_6773);
        (0..128)
            .map(|bit| 1u128 << bit)
            .chain([0, u128::MAX])
            .chain(
                (0..4096).map(|_| (u128::from(rng.next_u64()) << 64) | u128::from(rng.next_u64())),
            )
            .map(AttrFlags::from_bits_retain)
            .collect()
    }

    /// The flags as they were read before: one `if` per attribute.
    fn struct_reference(attrs: AttrFlags) -> StructFlags {
        let mut flags = StructFlags::empty();
        if attrs.contains(AttrFlags::RUSTC_HAS_INCOHERENT_INHERENT_IMPLS) {
            flags |= StructFlags::RUSTC_HAS_INCOHERENT_INHERENT_IMPLS;
        }
        if attrs.contains(AttrFlags::FUNDAMENTAL) {
            flags |= StructFlags::FUNDAMENTAL;
        }
        if attrs.contains(AttrFlags::HAS_REPR) {
            flags |= StructFlags::HAS_REPR;
        }
        flags
    }

    fn enum_reference(attrs: AttrFlags) -> EnumFlags {
        let mut flags = EnumFlags::empty();
        if attrs.contains(AttrFlags::RUSTC_HAS_INCOHERENT_INHERENT_IMPLS) {
            flags |= EnumFlags::RUSTC_HAS_INCOHERENT_INHERENT_IMPLS;
        }
        if attrs.contains(AttrFlags::HAS_REPR) {
            flags |= EnumFlags::HAS_REPR;
        }
        if attrs.contains(AttrFlags::FUNDAMENTAL) {
            flags |= EnumFlags::FUNDAMENTAL;
        }
        flags
    }

    fn trait_reference(attrs: AttrFlags) -> TraitFlags {
        let mut flags = TraitFlags::empty();
        if attrs.contains(AttrFlags::FUNDAMENTAL) {
            flags |= TraitFlags::FUNDAMENTAL;
        }
        if attrs.contains(AttrFlags::RUSTC_HAS_INCOHERENT_INHERENT_IMPLS) {
            flags |= TraitFlags::RUSTC_HAS_INCOHERENT_INHERENT_IMPLS;
        }
        if attrs.contains(AttrFlags::RUSTC_PAREN_SUGAR) {
            flags |= TraitFlags::RUSTC_PAREN_SUGAR;
        }
        if attrs.contains(AttrFlags::RUSTC_COINDUCTIVE) {
            flags |= TraitFlags::COINDUCTIVE;
        }
        if attrs.contains(AttrFlags::RUSTC_SKIP_ARRAY_DURING_METHOD_DISPATCH) {
            flags |= TraitFlags::SKIP_ARRAY_DURING_METHOD_DISPATCH;
        }
        if attrs.contains(AttrFlags::RUSTC_SKIP_BOXED_SLICE_DURING_METHOD_DISPATCH) {
            flags |= TraitFlags::SKIP_BOXED_SLICE_DURING_METHOD_DISPATCH;
        }
        flags
    }

    /// As before, except that an `unsafe fn` with `#[rustc_deprecated_safe_2024]` keeps
    /// `UNSAFE` (rustc's `fn_sig` of it is unsafe).
    fn fn_reference(attrs: AttrFlags, unsafe_: bool) -> FnFlags {
        let mut flags = FnFlags::empty();
        if attrs.contains(AttrFlags::RUSTC_ALLOW_INCOHERENT_IMPL) {
            flags.insert(FnFlags::RUSTC_ALLOW_INCOHERENT_IMPL);
        }
        if attrs.contains(AttrFlags::HAS_TARGET_FEATURE) {
            flags.insert(FnFlags::HAS_TARGET_FEATURE);
        }
        if attrs.contains(AttrFlags::RUSTC_INTRINSIC) {
            flags.insert(FnFlags::RUSTC_INTRINSIC);
        }
        if attrs.contains(AttrFlags::HAS_LEGACY_CONST_GENERICS) {
            flags.insert(FnFlags::HAS_LEGACY_CONST_GENERICS);
        }
        if unsafe_ {
            flags.insert(FnFlags::UNSAFE);
            if attrs.contains(AttrFlags::RUSTC_DEPRECATED_SAFE_2024) {
                flags.insert(FnFlags::DEPRECATED_SAFE_2024);
            }
        }
        flags
    }

    fn type_alias_reference(attrs: AttrFlags) -> TypeAliasFlags {
        let mut flags = TypeAliasFlags::empty();
        if attrs.contains(AttrFlags::RUSTC_HAS_INCOHERENT_INHERENT_IMPLS) {
            flags.insert(TypeAliasFlags::RUSTC_HAS_INCOHERENT_INHERENT_IMPL);
        }
        if attrs.contains(AttrFlags::RUSTC_ALLOW_INCOHERENT_IMPL) {
            flags.insert(TypeAliasFlags::RUSTC_ALLOW_INCOHERENT_IMPL);
        }
        flags
    }

    #[test]
    fn flags_from_attrs_as_one_if_per_flag() {
        for attrs in attr_inputs() {
            assert_eq!(StructFlags::from_attrs(attrs), struct_reference(attrs), "{attrs:?}");
            assert_eq!(EnumFlags::from_attrs(attrs), enum_reference(attrs), "{attrs:?}");
            assert_eq!(TraitFlags::from_attrs(attrs), trait_reference(attrs), "{attrs:?}");
            assert_eq!(TypeAliasFlags::from_attrs(attrs), type_alias_reference(attrs), "{attrs:?}");
            for unsafe_ in [false, true] {
                assert_eq!(
                    FnFlags::from_attrs(attrs, unsafe_),
                    fn_reference(attrs, unsafe_),
                    "{attrs:?} unsafe: {unsafe_}"
                );
            }
            let allow = attrs.contains(AttrFlags::RUSTC_ALLOW_INCOHERENT_IMPL);
            let const_flags = flags_from_attrs!(attrs, ConstFlags {
                RUSTC_ALLOW_INCOHERENT_IMPL => RUSTC_ALLOW_INCOHERENT_IMPL,
            });
            assert_eq!(const_flags.contains(ConstFlags::RUSTC_ALLOW_INCOHERENT_IMPL), allow);
            assert_eq!(const_flags - ConstFlags::RUSTC_ALLOW_INCOHERENT_IMPL, ConstFlags::empty());
            let static_flags = flags_from_attrs!(attrs, StaticFlags {
                RUSTC_ALLOW_INCOHERENT_IMPL => RUSTC_ALLOW_INCOHERENT_IMPL,
            });
            assert_eq!(static_flags.contains(StaticFlags::RUSTC_ALLOW_INCOHERENT_IMPL), allow);
            assert_eq!(
                static_flags - StaticFlags::RUSTC_ALLOW_INCOHERENT_IMPL,
                StaticFlags::empty()
            );
        }
    }

    #[test]
    fn struct_lang_items_as_a_match() {
        let reference = |lang: &Symbol| match lang {
            _ if *lang == sym::phantom_data => StructFlags::IS_PHANTOM_DATA,
            _ if *lang == sym::owned_box => StructFlags::IS_BOX,
            _ if *lang == sym::manually_drop => StructFlags::IS_MANUALLY_DROP,
            _ if *lang == sym::unsafe_cell => StructFlags::IS_UNSAFE_CELL,
            _ if *lang == sym::covariant_unsafe_cell => StructFlags::IS_COVARIANT_UNSAFE_CELL,
            _ if *lang == sym::unsafe_pinned => StructFlags::IS_UNSAFE_PINNED,
            _ if *lang == sym::f16b => StructFlags::IS_F16B,
            _ if *lang == sym::ppcf128 => StructFlags::IS_PPC_F128,
            _ => StructFlags::empty(),
        };
        let others = [sym::sized, sym::copy, sym::non_null, sym::va_list, sym::drop];
        let interned = ["phantom_data", "owned_box", "ppcf128", "not_a_lang_item"].map(Symbol::intern);
        for lang in STRUCT_LANG_ITEMS.iter().map(|(it, _)| it.clone()).chain(others).chain(interned) {
            assert_eq!(StructFlags::from_lang_item(&lang), reference(&lang), "{lang:?}");
        }
        for (i, (lang, _)) in STRUCT_LANG_ITEMS.iter().enumerate() {
            assert_eq!(STRUCT_LANG_ITEM_NAMES.get(lang), Some(i as u16), "{lang:?}");
        }
    }

    const KEYWORDS: &str = r#"
unsafe fn a() {}
const async unsafe extern "C" fn b() {}
impl Z { default fn c() {} }
pub(crate) const fn d() {}
fn e() -> i32 { 0 }
async gen fn f() {}
unsafe extern "C" {
    safe fn g();
    unsafe fn h();
    safe static S: u8;
    unsafe static mut T: u8;
    static U: u8;
}
static mut V: u8 = 0;
static W: u8 = 0;
unsafe impl Send for X {}
impl !Sync for X {}
default impl<T> Tr for T {}
impl const Tr for X {}
impl Y {}
auto trait Au {}
unsafe trait Un {}
trait Al = Clone;
const trait Co {}
trait Pl {}
struct R { unsafe a: u8, b: u8, pub c: u8 }
struct Q(pub u8, u8);
"#;

    #[test]
    fn child_tokens_as_the_accessors() {
        let file = SourceFile::parse(KEYWORDS, Edition::CURRENT).tree();
        let mut seen_fn = [false; 6];
        let mut seen_static = [false; 3];
        let mut seen_impl = [false; 4];
        let mut seen_trait = [false; 3];
        let mut seen_unsafe_field = false;
        fn or(seen: &mut [bool], found: &[bool]) {
            seen.iter_mut().zip(found).for_each(|(seen, found)| *seen |= *found)
        }
        for node in file.syntax().descendants() {
            if let Some(it) = ast::Fn::cast(node.clone()) {
                let found = child_tokens(
                    it.syntax(),
                    [T![unsafe], T![async], T![gen], T![const], T![default], T![safe]],
                );
                let accessors = [
                    it.unsafe_token().is_some(),
                    it.async_token().is_some(),
                    it.gen_token().is_some(),
                    it.const_token().is_some(),
                    it.default_token().is_some(),
                    it.safe_token().is_some(),
                ];
                assert_eq!(found, accessors, "{it}");
                or(&mut seen_fn, &found);
            } else if let Some(it) = ast::Static::cast(node.clone()) {
                let found = child_tokens(it.syntax(), [T![mut], T![unsafe], T![safe]]);
                let accessors = [
                    it.mut_token().is_some(),
                    it.unsafe_token().is_some(),
                    it.safe_token().is_some(),
                ];
                assert_eq!(found, accessors, "{it}");
                or(&mut seen_static, &found);
            } else if let Some(it) = ast::Impl::cast(node.clone()) {
                let found =
                    child_tokens(it.syntax(), [T![unsafe], T![!], T![default], T![const]]);
                let accessors = [
                    it.unsafe_token().is_some(),
                    it.excl_token().is_some(),
                    it.default_token().is_some(),
                    it.const_token().is_some(),
                ];
                assert_eq!(found, accessors, "{it}");
                or(&mut seen_impl, &found);
            } else if let Some(it) = ast::Trait::cast(node.clone()) {
                let found = child_tokens(it.syntax(), [T![auto], T![unsafe], T![=], T![const]]);
                let accessors = [
                    it.auto_token().is_some(),
                    it.unsafe_token().is_some(),
                    it.eq_token().is_some(),
                    it.const_token().is_some(),
                ];
                assert_eq!(found, accessors, "{it}");
                or(&mut seen_trait, &found[..3]);
            } else if let Some(it) = ast::RecordField::cast(node.clone()) {
                let [found] = child_tokens(it.syntax(), [T![unsafe]]);
                let scanned = it
                    .syntax()
                    .children_with_tokens()
                    .filter_map(NodeOrToken::into_token)
                    .any(|token| token.kind() == T![unsafe]);
                assert_eq!(found, scanned, "{it}");
                seen_unsafe_field |= found;
            } else if let Some(it) = ast::TupleField::cast(node) {
                let [found] = child_tokens(it.syntax(), [T![unsafe]]);
                assert!(!found, "{it}");
            }
        }
        // `gen` is left out: whether `async gen fn` parses is the parser's.
        assert_eq!([seen_fn[0], seen_fn[1], seen_fn[3], seen_fn[4], seen_fn[5]], [true; 5]);
        assert_eq!(seen_static, [true; 3]);
        assert_eq!(seen_impl, [true; 4]);
        assert_eq!(seen_trait, [true; 3]);
        assert!(seen_unsafe_field);
    }

    #[test]
    fn variant_bits_as_bools() {
        let mut rng = SplitMix64::new(0x7661_7269_616e_7473);
        for len in [0, 1, 2, 63, 64, 65, 127, 128, 129, 200, 1000] {
            // Only the first `kept` are ever set, as only the variants kept get a bit.
            let kept = if len == 0 { 0 } else { rng.index(len + 1) };
            let mut model = vec![false; kept];
            let mut bits = VariantBits::new(len);
            for _ in 0..3 * len {
                if kept == 0 {
                    break;
                }
                let index = rng.index(kept);
                let value = rng.coin();
                model[index] = value;
                bits.set(index, value);
                assert_eq!(bits.contains(index), value);
            }
            for (index, &value) in model.iter().enumerate() {
                assert_eq!(bits.contains(index), value, "len {len} index {index}");
            }
            for index in kept..len {
                assert!(!bits.contains(index), "len {len} index {index}");
            }

            bits.truncate(kept);
            let mut fresh = VariantBits::new(kept);
            for (index, &value) in model.iter().enumerate() {
                fresh.set(index, value);
            }
            assert_eq!(bits, fresh, "len {len} kept {kept}");
            for (index, &value) in model.iter().enumerate() {
                assert_eq!(bits.contains(index), value, "len {len} kept {kept} index {index}");
            }
        }
        assert!(!VariantBits::new(64).into_words().spilled());
        assert!(VariantBits::new(65).into_words().spilled());
    }
}

/// What an edit of a file lowers again of the signatures: each is lowered from its own
/// [`SignatureSyntax`], so an edit of a body, or of another item, lowers none of them.
#[cfg(test)]
mod relative_tests {
    use test_fixture::WithFixture;

    use super::*;
    use crate::{
        AssocItemId, ModuleDefId,
        nameres::{
            assoc::{ImplItems, TraitItems},
            crate_def_map,
        },
        test_db::{TestDB, TestDBExt},
    };

    const CODE: &str = r#"
pub struct S<T> { a: T, b: u32 }
pub union U { a: u32, b: f32 }
pub enum E<T> { A = 1, B(T), C { c: u8 } }
macro_rules! ty { () => { u32 } }
pub trait Tr { const C: u32; fn d(&self) -> u32 { 41 } }
impl Tr for S<u32> { const C: u32 = 2; fn d(&self) -> u32 { self.b + 5 } }
pub type A = S<ty!()>;
pub const K: u32 = 3;
pub static L: u32 = K + 1;
pub fn f(x: u32) -> ty!() { x + 11 }
pub fn g<T: Tr>(t: &T) -> u32 where T: Copy { t.d() }
#[cfg(all())]
impl S<u8> { fn h(&self) -> u8 { 0 } }
"#;

    /// Each body of `CODE` edited, the text around it moved.
    const BODY_EDITS: [(&str, &str); 7] = [
        ("{ 41 }", "{ 41 + 1 }"),
        ("= 2;", "= 2 * 2;"),
        ("{ self.b + 5 }", "{ self.b * 5 + 1 }"),
        ("= 3;", "= 3 + 4;"),
        ("= K + 1;", "= K + 100;"),
        ("{ x + 11 }", "{ (x + 11) * 3 }"),
        ("{ t.d() }", "{ t.d() + t.d() }"),
    ];

    fn edited(code: &str, edits: &[(&str, &str)]) -> String {
        edits.iter().fold(code.to_owned(), |code, (from, to)| {
            assert_eq!(code.matches(from).count(), 1, "`{from}` is not written once");
            code.replace(from, to)
        })
    }

    fn assoc(item: AssocItemId) -> GenericDefId {
        match item {
            AssocItemId::FunctionId(it) => it.into(),
            AssocItemId::ConstId(it) => it.into(),
            AssocItemId::TypeAliasId(it) => it.into(),
        }
    }

    /// Every signature of the test crate, items of traits and impls too, and every variant.
    fn defs(db: &TestDB) -> (Vec<GenericDefId>, Vec<VariantId>) {
        let (mut signatures, mut variants) = (Vec::new(), Vec::new());
        for (_, module) in crate_def_map(db, db.fetch_test_crate()).modules(db) {
            let scope = module.scope(db);
            for decl in scope.declarations() {
                match decl {
                    ModuleDefId::FunctionId(it) => signatures.push(it.into()),
                    ModuleDefId::ConstId(it) => signatures.push(it.into()),
                    ModuleDefId::StaticId(it) => signatures.push(it.into()),
                    ModuleDefId::TypeAliasId(it) => signatures.push(it.into()),
                    ModuleDefId::TraitId(it) => {
                        signatures.push(it.into());
                        let items = TraitItems::query(db, it).items.iter();
                        signatures.extend(items.map(|&(_, item)| assoc(item)));
                    }
                    ModuleDefId::AdtId(it) => {
                        signatures.push(it.into());
                        match it {
                            AdtId::StructId(it) => variants.push(it.into()),
                            AdtId::UnionId(it) => variants.push(it.into()),
                            AdtId::EnumId(it) => {
                                let of = EnumVariants::of(db, it).0.variants.values();
                                variants.extend(of.map(|&(v, _)| VariantId::from(v)));
                            }
                        }
                    }
                    _ => {}
                }
            }
            for imp in scope.impls() {
                signatures.push(imp.into());
                let items = ImplItems::of(db, imp).0.items.iter();
                signatures.extend(items.map(|&(_, item)| assoc(item)));
            }
        }
        (signatures, variants)
    }

    /// Where each signature, each variant's fields and each enum's variants are kept: what
    /// lowering them again makes anew.
    fn lowered(db: &TestDB) -> Vec<usize> {
        fn at<T>(it: &T) -> usize {
            std::ptr::from_ref(it) as usize
        }
        let (signatures, variants) = defs(db);
        let signatures = signatures.into_iter().map(|def| match def {
            GenericDefId::AdtId(AdtId::StructId(it)) => at(StructSignature::of(db, it)),
            GenericDefId::AdtId(AdtId::UnionId(it)) => at(UnionSignature::of(db, it)),
            GenericDefId::AdtId(AdtId::EnumId(it)) => {
                at(EnumSignature::of(db, it)) ^ at(EnumVariants::of(db, it))
            }
            GenericDefId::ConstId(it) => at(ConstSignature::of(db, it)),
            GenericDefId::StaticId(it) => at(StaticSignature::of(db, it)),
            GenericDefId::ImplId(it) => at(ImplSignature::of(db, it)),
            GenericDefId::TraitId(it) => at(TraitSignature::of(db, it)),
            GenericDefId::FunctionId(it) => at(FunctionSignature::of(db, it)),
            GenericDefId::TypeAliasId(it) => at(TypeAliasSignature::of(db, it)),
        });
        let variants = variants.into_iter().map(|it| at(VariantFields::of(db, it)));
        signatures.chain(variants).collect()
    }

    /// The signature lowerings in `log`.
    fn lowerings(log: &[String]) -> Vec<&str> {
        let lowering = |it: &&String| {
            it.contains("Signature::relative")
                || it.contains("VariantFields::relative")
                || it.contains("EnumVariants::of")
        };
        log.iter().filter(lowering).map(String::as_str).collect()
    }

    /// `edits` of `CODE`, with every signature asked for before and after: what they lower.
    fn lowered_by(edits: &[(&str, &str)]) -> (Vec<String>, Vec<usize>, Vec<usize>) {
        let (mut db, file) = TestDB::with_single_file(CODE);
        let before = lowered(&db);
        assert!(lowerings(&db.executed_in_read_order(|| _ = lowered(&db))).is_empty());
        db.set_file_text(file.file_id(&db), &edited(CODE, edits));
        let mut after = Vec::new();
        let log = db.executed_in_read_order(|| after = lowered(&db));
        (log, before, after)
    }

    #[test]
    fn a_body_edit_lowers_no_signature() {
        let (log, before, after) = lowered_by(&BODY_EDITS);
        assert_eq!(lowerings(&log), Vec::<&str>::new(), "{log:#?}");
        assert_eq!(before, after, "a signature was lowered anew");
    }

    #[test]
    fn an_edit_of_another_item_lowers_none_of_this_one() {
        // `S` gets a field: its signature and fields are lowered again, and every item after
        // it moves, but none of them is lowered again.
        let (log, ..) = lowered_by(&[("a: T, b: u32", "a: T, b: u32, c: u64")]);
        let mut lowerings = lowerings(&log);
        lowerings.sort_unstable();
        assert_eq!(lowerings.len(), 2, "{log:#?}");
        assert!(lowerings[0].contains("StructSignature::relative"), "{lowerings:?}");
        assert!(lowerings[1].contains("VariantFields::relative"), "{lowerings:?}");
    }

    #[test]
    fn a_discriminant_edit_lowers_the_variants_not_the_enum() {
        let (log, ..) = lowered_by(&[("A = 1", "A = 1 + 1")]);
        let lowerings = lowerings(&log);
        assert!(!lowerings.iter().any(|it| it.contains("EnumSignature")), "{log:#?}");
        assert!(lowerings.iter().any(|it| it.contains("EnumVariants::of")), "{log:#?}");
    }

    /// The bodies an outline of `CODE` leaves out: those of every fn, const and static that is
    /// its own anchor, which are what `BODY_EDITS` edits, and not `h`'s, whose impl has an
    /// attribute.
    const LEFT_OUT: [(&str, &str); 7] = [
        ("{ 41 }", ""),
        ("= 2;", "= ;"),
        ("{ self.b + 5 }", ""),
        ("= 3;", "= ;"),
        ("= K + 1;", "= ;"),
        ("{ x + 11 }", ""),
        ("{ t.d() }", ""),
    ];

    #[test]
    fn the_outline_leaves_out_each_body_of_its_own_anchor() {
        let (db, file) = TestDB::with_single_file(CODE);
        let outline = file_outline(&db, file);
        assert_eq!(outline.root().to_string(), edited(CODE, &LEFT_OUT));
        assert_eq!(outline.cuts.len(), BODY_EDITS.len(), "{outline:?}");
        let root = outline.root();
        for cut in &outline.cuts {
            assert!(cut.text_range().is_empty(), "{cut:?}");
            let item = root
                .descendants()
                .find(|it| left_out(it).is_some_and(|it| SyntaxNodePtr::new(&it) == *cut));
            assert!(item.is_some(), "{cut:?} is not the body of an item in the outline");
        }
    }

    #[test]
    fn a_body_edit_runs_no_signature_syntax() {
        let (mut db, file) = TestDB::with_single_file(CODE);
        _ = lowered(&db);
        db.set_file_text(file.file_id(&db), &edited(CODE, &BODY_EDITS));
        let log = db.executed_in_read_order(|| _ = lowered(&db));
        assert!(log.iter().any(|it| it.contains("file_outline")), "{log:#?}");
        let syntax =
            |it: &&String| it.contains("signature_syntax") || it.contains("variants_syntax");
        assert_eq!(log.iter().filter(syntax).count(), 0, "{log:#?}");
    }

    /// An item in a body the outline leaves out is read off the file, its anchor whole; an
    /// item in the signature of a fn whose body is left out is read off the outline, the
    /// anchor without that body. Both keep the file's ast id for the item.
    #[test]
    fn items_anchored_at_a_fn_are_read_around_its_body() {
        const NESTED: &str = r#"
pub fn outer() -> u32 { struct Inner<T>(T); 1 }
pub fn p(x: [u8; { struct InType; 1 }]) -> u8 { x[0] }
"#;
        let (db, file) = TestDB::with_single_file(NESTED);
        let file_id = HirFileId::from(file);
        let root = file_id.parse_or_expand(&db);
        let named = |name: &str| {
            root.descendants()
                .filter_map(ast::Struct::cast)
                .find(|it| it.name().is_some_and(|it| it.syntax().text() == name))
                .unwrap_or_else(|| panic!("no struct {name}"))
        };
        let nothing: Elide = |_| None;
        for (name, text) in [
            ("Inner", "pub fn outer() -> u32 { struct Inner<T>(T); 1 }"),
            ("InType", "pub fn p(x: [u8; { struct InType; 1 }]) -> u8 "),
        ] {
            let item = named(name);
            let id = InFile::new(file_id, file_id.ast_id_map(&db).ast_id(&item));
            let syntax = item_syntax(&db, id, nothing).unwrap_or_else(|| panic!("{name}"));
            assert_eq!(syntax.root().to_string(), text, "{name}");
            assert_eq!(syntax.node::<ast::Struct>().value.syntax().to_string(), item.to_string());
            assert_eq!(syntax.ast_id_map.get_erased(id.value.erase()), syntax.item, "{name}");
            assert_eq!(syntax.elided, None, "{name}");
        }
    }

    /// Every read of a lowered signature: the memo hits of its queries, and each field, variant
    /// and flag read off what they hold.
    fn read(db: &TestDB, def: GenericDefId) {
        use std::hint::black_box;
        black_box(signature_syntax(db, def));
        match def {
            GenericDefId::AdtId(AdtId::StructId(it)) => {
                black_box(StructSignature::of(db, it));
                black_box(StructSignature::with_source_map(db, it));
            }
            GenericDefId::AdtId(AdtId::UnionId(it)) => {
                black_box(UnionSignature::of(db, it));
                black_box(UnionSignature::with_source_map(db, it));
            }
            GenericDefId::AdtId(AdtId::EnumId(it)) => {
                black_box(EnumSignature::of(db, it));
                black_box(EnumSignature::with_source_map(db, it));
                black_box(variants_syntax(db, it));
                let (variants, _) = EnumVariants::of(db, it);
                for (name, &(variant, _)) in &variants.variants {
                    black_box(variants.variant(name));
                    black_box(variants.has_explicit_discriminant(variant));
                }
                black_box(variants.is_payload_free(db));
            }
            GenericDefId::ConstId(it) => {
                black_box(ConstSignature::of(db, it).has_body());
                black_box(ConstSignature::with_source_map(db, it));
            }
            GenericDefId::StaticId(it) => {
                black_box(StaticSignature::of(db, it));
                black_box(StaticSignature::with_source_map(db, it));
            }
            GenericDefId::ImplId(it) => {
                black_box(ImplSignature::of(db, it).is_negative());
                black_box(ImplSignature::with_source_map(db, it));
            }
            GenericDefId::TraitId(it) => {
                black_box(TraitSignature::of(db, it));
                black_box(TraitSignature::with_source_map(db, it));
            }
            GenericDefId::FunctionId(it) => {
                black_box(FunctionSignature::of(db, it));
                black_box(FunctionSignature::with_source_map(db, it));
            }
            GenericDefId::TypeAliasId(it) => {
                black_box(TypeAliasSignature::of(db, it));
                black_box(TypeAliasSignature::with_source_map(db, it));
            }
        }
    }

    fn read_fields(db: &TestDB, id: VariantId) {
        use std::hint::black_box;
        let fields = VariantFields::of(db, id);
        black_box(VariantFields::with_source_map(db, id));
        for (field, data) in fields.fields().iter() {
            black_box((field, data));
            black_box(fields.field(&data.name));
        }
    }

    /// Allocation budget: once lowered, reading a signature, its fields or its variants is a
    /// memo hit and reads of what it holds, and allocates nothing.
    #[test]
    fn reading_signatures_allocates_nothing() {
        let (db, _) = TestDB::with_single_file(CODE);
        let (signatures, variants) = defs(&db);
        assert!(signatures.len() > 15 && variants.len() > 4, "too small to measure");
        for &def in &signatures {
            read(&db, def);
        }
        for &id in &variants {
            read_fields(&db, id);
        }
        blaze_alloc_count::assert_at_most(0, "reading every signature, field and variant", || {
            for &def in &signatures {
                read(&db, def);
            }
            for &id in &variants {
                read_fields(&db, id);
            }
        });
    }

    /// What `with_source_map` gives, moved from the item's tree to the file, is what lowering
    /// from the whole file gives: the same signature, each part at the same place.
    #[test]
    fn moved_source_maps_are_the_files() {
        macro_rules! check {
            ($db:ident, $Signature:ident, $id:expr) => {{
                let id = $id;
                let (signature, map) = $Signature::with_source_map(&$db, id);
                let source = id.lookup(&$db).source(&$db);
                let (whole, whole_map) = $Signature::lower(&$db, id, source, None);
                assert_eq!(**signature, *whole, "{id:?}");
                assert!(*map == whole_map, "{id:?}: {map:#?}\nfrom the file: {whole_map:#?}");
            }};
        }
        let (mut db, file) = TestDB::with_single_file(CODE);
        for text in [CODE.to_owned(), edited(CODE, &BODY_EDITS)] {
            db.set_file_text(file.file_id(&db), &text);
            let (signatures, variants) = defs(&db);
            for def in signatures {
                assert!(signature_syntax(&db, def).is_some(), "{def:?} has no syntax of its own");
                match def {
                    GenericDefId::AdtId(AdtId::StructId(it)) => check!(db, StructSignature, it),
                    GenericDefId::AdtId(AdtId::UnionId(it)) => check!(db, UnionSignature, it),
                    GenericDefId::AdtId(AdtId::EnumId(it)) => check!(db, EnumSignature, it),
                    GenericDefId::ConstId(it) => check!(db, ConstSignature, it),
                    GenericDefId::StaticId(it) => check!(db, StaticSignature, it),
                    GenericDefId::ImplId(it) => check!(db, ImplSignature, it),
                    GenericDefId::TraitId(it) => check!(db, TraitSignature, it),
                    GenericDefId::FunctionId(it) => check!(db, FunctionSignature, it),
                    GenericDefId::TypeAliasId(it) => check!(db, TypeAliasSignature, it),
                }
            }
            for id in variants {
                let (fields, map) = VariantFields::with_source_map(&db, id);
                let (whole, whole_map) = VariantFields::lower(&db, id, None);
                assert_eq!(**fields, *whole, "{id:?}");
                assert!(*map == whole_map, "{id:?}: {map:#?}\nfrom the file: {whole_map:#?}");
            }
        }
    }
}
