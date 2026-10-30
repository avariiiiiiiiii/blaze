//! [`ExpressionStore`], [`Body`] and their source maps as [`blaze_format`] buffers
//! (docs/v2/plan/lanes/format.md section 5), in the encoding of [`crate::hir::persist`].
//!
//! A store's expressions and patterns are written as the columns they are in memory
//! ([`HirColumns`]): the kind bytes, the data words, `extra` and the list arrays copied as they
//! are, the statements, arms and record fields as records of words (a field's name a [`Str`]),
//! and the cold side tables (names, paths, literals, generic args, `offset_of!`, `asm!`) in the
//! words, as [`Code`] writes them. Type refs are written the same way, as the columns of
//! [`TypeColumns`]: kinds, data words, `extra` and the list array copied, the side tables
//! (bounds, paths, references, arrays, fn pointers, type params) in the words. Bindings, labels
//! and lifetimes are one [`Node`] column per arena, their fields in the same words. Its source
//! map is a buffer of its own, so that spans stay out of what is hashed: the maps back from an
//! id to its syntax as dense columns of [`Src`], the maps from syntax to an id as two columns
//! each, the keys as [`Src`]s in the [`SourceTable`]'s order and the ids beside them, and
//! everything else in words.
//!
//! What is read from a buffer is checked before it is used: each kind and each column's ids,
//! and the order of each source table's keys, when the buffer is read; each id in a record or a
//! side table, and the columns as a whole ([`HirColumns::check`], [`TypeColumns::check`]), when
//! it is decoded. An id past the arena it indexes is a [`FormatError::Invalid`], never a later
//! panic. The ids of a source map index its store, which is another buffer: they are not
//! checked.

use std::mem::{offset_of, size_of};

use blaze_format::{
    FormatError, Part, Persist, Record, SchemaHash, Slice, Str, Writer, open_as, schema_of,
};
use cfg::{CfgAtom, CfgOptions};
use either::Either;
use hir_expand::{
    HirFileId, InFile, MacroCallId, MacroCallStyles, MacroDefId, MacroDefKind,
    builtin::{BuiltinAttrExpander, BuiltinDeriveExpander, BuiltinFnLikeExpander, EagerExpander},
    name::Name,
    proc_macro::{CustomProcMacroExpander, ProcMacroKind},
};
use intern::{Symbol, sym};
use la_arena::{Arena, ArenaMap, Idx, RawIdx};
use rustc_abi::ExternAbi;
use rustc_hash::FxHashMap;
use salsa::plumbing::{AsId, FromId};
use smallvec::SmallVec;
use span::Edition;
use syntax::{AstNode, AstPtr, SyntaxKind, SyntaxNodePtr, TextRange, TextSize, ast};

use crate::{
    AdtId, BlockId, ConstId, EnumId, EnumVariantId, FunctionId, GenericDefId, ImplId,
    LifetimeParamId, Macro2Id, MacroId, MacroRulesId, ProcMacroId, StaticId, StructId, TraitId,
    TypeAliasId, TypeOrConstParamId, TypeParamId, UnionId,
    builtin_type::{BuiltinFloat, BuiltinInt, BuiltinUint},
    expr_store::{
        Body, BodySourceMap, ExprOrPatSource, ExprRoot, ExpressionOnlySourceMap,
        ExpressionOnlyStore, ExpressionStore, ExpressionStoreDiagnostics, ExpressionStoreSourceMap,
        ExprSource, FormatTemplate, HygieneId, IdTable, LabelSource, LifetimeSource, MacroCallPtr,
        MissingBodyItemKind, NO_OWNER, PatSource, SourceTable, TypeSource,
        body::Param,
        path::{
            AssociatedTypeBinding, GenericArg, GenericArgs, GenericArgsParentheses, NormalPath,
            Path,
        },
    },
    hir::{
        AsmOperand, AsmOptions, AsmPiece, Binding, BindingAnnotation, BindingId, BindingProblems,
        ExprId, ExprOrPatId, ExprOrPatIdPacked, FloatTypeWrapper, InlineAsm, InlineAsmKind,
        InlineAsmRegOrRegClass, Item, Label, LabelId, Literal, MatchArm, OffsetOf, PatId,
        RecordFieldPat, RecordLitField, Statement,
        columns::{self, Frozen, HirColumns},
        persist::{Code, Column, Node, Placed, Read, Words, WordsRec, syntax_context},
    },
    lang_item::LangItemTarget,
    type_ref::{
        self, ArrayType, ConstRef, FnType, LifetimeRef, LifetimeRefId, Mutability, PathId,
        RefType, TraitBoundModifier, TypeBound, TypeColumns, TypeRefId, UseArgRef,
    },
};

blaze_format::record! {
    /// Two ids, or an id and what a side table holds for it.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub(crate) struct IdPair: 1 {
        pub(crate) a: u32,
        pub(crate) b: u32,
    }
}

blaze_format::record! {
    /// An [`ExprRoot`]: its root, and where the expressions, patterns and bindings it holds end.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub(crate) struct ExprRootRec: 1 {
        pub(crate) root: u32,
        pub(crate) exprs_end: u32,
        pub(crate) pats_end: u32,
        pub(crate) bindings_end: u32,
    }
}

blaze_format::record! {
    /// A [`Statement`]: its kind ([`LET`] to [`MACRO_DEF`]), then its ids, each `Option` as the
    /// raw id plus one, 0 for `None`. `id` is the pattern of a `let`, the expression of an
    /// expression statement, or the word a macro definition is written at; a word the kind has
    /// no use for is 0.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub(crate) struct StatementRec: 1 {
        pub(crate) kind: u32,
        pub(crate) id: u32,
        pub(crate) type_ref: u32,
        pub(crate) initializer: u32,
        pub(crate) else_branch: u32,
    }
}

blaze_format::record! {
    /// A [`MatchArm`], its guard the raw id plus one, 0 for none.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub(crate) struct ArmRec: 1 {
        pub(crate) pat: u32,
        pub(crate) guard: u32,
        pub(crate) expr: u32,
    }
}

blaze_format::record! {
    /// A [`RecordLitField`] (`id` its expression) or a [`RecordFieldPat`] (`id` its pattern).
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub(crate) struct FieldRec: 1 {
        pub(crate) name: Str,
        pub(crate) id: u32,
    }
}

/// [`StatementRec::kind`]s.
const LET: u32 = 0;
const SUPER_LET: u32 = 1;
const EXPR: u32 = 2;
const EXPR_SEMI: u32 = 3;
const OTHER_ITEM: u32 = 4;
const MACRO_DEF: u32 = 5;

blaze_format::record! {
    /// The root of an [`ExpressionStore`]'s buffer: [`HirColumns`] and [`TypeColumns`] as they
    /// are in memory, each id a raw `u32`, the side tables in the words from `side` and
    /// `ty_side` on.
    #[derive(Clone, Copy)]
    pub(crate) struct StoreRoot: 3 {
        pub(crate) expr_kind: Slice<u8>,
        pub(crate) expr_data: Slice<u32>,
        pub(crate) pat_kind: Slice<u8>,
        pub(crate) pat_data: Slice<u32>,
        pub(crate) extra: Slice<u32>,
        pub(crate) expr_lists: Slice<u32>,
        pub(crate) pat_lists: Slice<u32>,
        pub(crate) statements: Slice<StatementRec>,
        pub(crate) arms: Slice<ArmRec>,
        pub(crate) record_fields: Slice<FieldRec>,
        pub(crate) field_pats: Slice<FieldRec>,
        /// Each `Option<TypeRefId>` as the raw id plus one, 0 for `None`.
        pub(crate) type_lists: Slice<u32>,
        pub(crate) bindings: Slice<Node>,
        pub(crate) labels: Slice<Node>,
        pub(crate) ty_kind: Slice<u8>,
        pub(crate) ty_data: Slice<u32>,
        pub(crate) ty_extra: Slice<u32>,
        /// The tuples' element ids, raw.
        pub(crate) ty_lists: Slice<u32>,
        pub(crate) lifetimes: Slice<Node>,
        /// The owner of each binding, or [`NO_OWNER`]; empty when the top level owns them all.
        pub(crate) binding_owners: Slice<u32>,
        /// Each block's salsa id, as its bits.
        pub(crate) block_scopes: Slice<u64>,
        /// The packed id and the syntax context, by packed id in increasing order.
        pub(crate) ident_hygiene: Slice<IdPair>,
        /// The block and its edition, by block in increasing order.
        pub(crate) block_editions: Slice<IdPair>,
        pub(crate) overflow_inheriting: Slice<u32>,
        pub(crate) expr_roots: Slice<ExprRootRec>,
        /// The type and the syntax context, by type in increasing order.
        pub(crate) type_hygiene: Slice<IdPair>,
        pub(crate) words: WordsRec,
        /// The word the side tables begin at: names, paths, literals, generic args,
        /// `offset_of!`s and `asm!`s, each a count and its values.
        pub(crate) side: u32,
        /// The word the type side tables begin at: bounds, paths, references, arrays, fn
        /// pointers and type params, each a count and its values.
        pub(crate) ty_side: u32,
        /// [`HAS_EXPRS`] and [`HAS_TYPE_HYGIENE`].
        pub(crate) flags: u32,
    }
}

blaze_format::record! {
    /// The root of a [`Body`]'s buffer.
    #[derive(Clone, Copy)]
    pub(crate) struct BodyRoot: 3 {
        pub(crate) store: StoreRoot,
        /// Each parameter's formal and user-written pattern.
        pub(crate) params: Slice<IdPair>,
        /// The formal and user-written binding of `self`, when there is one.
        pub(crate) self_param: Slice<IdPair>,
    }
}

blaze_format::record! {
    /// A syntax pointer in a file: absent when `file_kind` is 0, and then all zeros.
    #[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
    pub(crate) struct Src: 1 {
        /// The salsa id of the file or of the macro call, as its bits.
        pub(crate) file: u64,
        /// 0 for no pointer, [`FILE`] or [`MACRO_FILE`].
        pub(crate) file_kind: u32,
        pub(crate) kind: u32,
        pub(crate) start: u32,
        pub(crate) end: u32,
    }
}

blaze_format::record! {
    /// The root of an [`ExpressionStoreSourceMap`]'s buffer. The back maps are dense, by id.
    /// Each [`SourceTable`] is its keys, in strictly increasing order of [`Src::key`], and the
    /// value of each beside it: an id raw (a packed one for expressions and patterns), a macro
    /// call's salsa id as its bits.
    #[derive(Clone, Copy)]
    pub(crate) struct SourceMapRoot: 2 {
        pub(crate) expr_map_back: Slice<Src>,
        pub(crate) pat_map_back: Slice<Src>,
        pub(crate) label_map_back: Slice<Src>,
        pub(crate) types_map_back: Slice<Src>,
        pub(crate) lifetime_map_back: Slice<Src>,
        pub(crate) expr_map: Slice<Src>,
        pub(crate) expr_map_ids: Slice<u32>,
        pub(crate) pat_map: Slice<Src>,
        pub(crate) pat_map_ids: Slice<u32>,
        pub(crate) label_map: Slice<Src>,
        pub(crate) label_map_ids: Slice<u32>,
        pub(crate) types_map: Slice<Src>,
        pub(crate) types_map_ids: Slice<u32>,
        pub(crate) lifetime_map: Slice<Src>,
        pub(crate) lifetime_map_ids: Slice<u32>,
        pub(crate) expansions: Slice<Src>,
        pub(crate) expansion_calls: Slice<u64>,
        pub(crate) words: WordsRec,
        /// [`HAS_EXPRS`].
        pub(crate) flags: u32,
    }
}

blaze_format::record! {
    /// The root of a [`BodySourceMap`]'s buffer.
    #[derive(Clone, Copy)]
    pub(crate) struct BodySourceMapRoot: 2 {
        pub(crate) store: SourceMapRoot,
        /// The pointer to `self`, when there is one.
        pub(crate) self_param: Slice<Src>,
    }
}

/// The store has an [`ExpressionOnlyStore`], or the source map an [`ExpressionOnlySourceMap`].
const HAS_EXPRS: u32 = 1 << 0;
/// The store's `type_hygiene` is `Some`.
const HAS_TYPE_HYGIENE: u32 = 1 << 1;

/// A [`Src`] in a file.
const FILE: u32 = 1;
/// A [`Src`] in the expansion of a macro call.
const MACRO_FILE: u32 = 2;

macro_rules! kinds {
    ($($(#[$meta:meta])* $name:ident { $($variant:ident),* $(,)? })*) => {$(
        $(#[$meta])*
        #[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
        pub enum $name {
            $($variant),*
        }

        impl $name {
            const ALL: &[$name] = &[$($name::$variant),*];

            fn from_u32(kind: u32) -> Option<$name> {
                $name::ALL.get(kind as usize).copied()
            }
        }
    )*};
}

/// Asserts each variant of `ours` has the number of the same variant of `theirs`: a kind
/// read in place from a buffer is the byte of the column it was copied from.
macro_rules! same_numbers {
    ($($ours:ident = $theirs:path { $($variant:ident),* $(,)? })*) => {$(
        const _: () = {
            $(::core::assert!(
                $ours::$variant as u8 == <$theirs>::$variant as u8,
                ::core::concat!(
                    ::core::stringify!($ours), "::", ::core::stringify!($variant),
                    " is not the column's number",
                ),
            );)*
            ::core::assert!(
                $ours::ALL.len() == [$($ours::$variant),*].len(),
                ::core::concat!(::core::stringify!($ours), " is not listed whole"),
            );
        };
    )*};
}

kinds! {
    /// Which [`crate::hir::Expr`] variant an expression of a buffer is, read in place.
    ExprKind {
        Missing, Path, If, Let, Block, Const, Loop, Call, MethodCall, Match, Continue, Break,
        Return, Become, Yield, Yeet, RecordLit, Field, Await, Cast, Ref, UnaryOp, BinaryOp,
        Assignment, Index, Closure, Tuple, Array, Literal, Underscore, OffsetOf, InlineAsm,
        IncludeBytes,
    }

    /// Which [`crate::hir::Pat`] variant a pattern of a buffer is, read in place.
    PatKind {
        Missing, Rest, Wild, Tuple, Or, Record, Range, Slice, Path, Lit, Bind, TupleStruct, Ref,
        Box, Deref, NotNull, Expr,
    }

    /// Which [`type_ref::TypeRef`] variant a type of a buffer is, read in place.
    TypeRefKind {
        Never, Placeholder, Tuple, Path, RawPtr, Reference, Array, Slice, Fn, ImplTrait, DynTrait,
        TypeParam, PatternType, Error,
    }

    /// Which [`LifetimeRef`] variant a node of a buffer is, read in place.
    LifetimeRefKind {
        Named, Static, Placeholder, Param, Error,
    }
}

same_numbers! {
    ExprKind = columns::ExprKind {
        Missing, Path, If, Let, Block, Const, Loop, Call, MethodCall, Match, Continue, Break,
        Return, Become, Yield, Yeet, RecordLit, Field, Await, Cast, Ref, UnaryOp, BinaryOp,
        Assignment, Index, Closure, Tuple, Array, Literal, Underscore, OffsetOf, InlineAsm,
        IncludeBytes,
    }
    PatKind = columns::PatKind {
        Missing, Rest, Wild, Tuple, Or, Record, Range, Slice, Path, Lit, Bind, TupleStruct, Ref,
        Box, Deref, NotNull, Expr,
    }
    TypeRefKind = type_ref::TypeRefKind {
        Never, Placeholder, Tuple, Path, RawPtr, Reference, Array, Slice, Fn, ImplTrait, DynTrait,
        TypeParam, PatternType, Error,
    }
}

fn raw<T>(id: Idx<T>) -> u32 {
    id.into_raw().into_u32()
}

fn idx<T>(raw: u32) -> Idx<T> {
    Idx::from_raw(RawIdx::from_u32(raw))
}

fn bits(id: impl AsId) -> u64 {
    id.as_id().as_bits()
}

/// The salsa id of `bits`, refused when salsa could not have made it.
fn salsa_id(bits: u64) -> Option<salsa::Id> {
    (bits as u32 != 0).then(|| salsa::Id::from_bits(bits))
}

/// Where `value`, which is in `buffer`, is from the buffer's start.
fn at_of<T>(value: &T, buffer: &[u8]) -> u64 {
    (value as *const T as usize).wrapping_sub(buffer.as_ptr() as usize) as u64
}

fn ensure<T>(ok: bool, value: &T, buffer: &[u8], what: &'static str) -> Result<(), FormatError> {
    match ok {
        true => Ok(()),
        false => Err(FormatError::Invalid { at: at_of(value, buffer), what }),
    }
}

/// Checks that `column` is in strictly increasing order of `key`.
fn ascending<T>(
    column: &[T],
    key: impl Fn(&T) -> u32,
    buffer: &[u8],
    what: &'static str,
) -> Result<(), FormatError> {
    column.windows(2).try_for_each(|pair| ensure(key(&pair[0]) < key(&pair[1]), &pair[1], buffer, what))
}

fn check_nodes(
    nodes: &[Node],
    kinds: usize,
    words: u32,
    buffer: &[u8],
    what: &'static str,
) -> Result<(), FormatError> {
    nodes.iter().try_for_each(|node| {
        ensure((node.kind as usize) < kinds, node, buffer, what)?;
        ensure(node.at <= words, node, buffer, "a node whose fields begin past the words")
    })
}

fn check_kinds(
    kinds: &[u8],
    count: usize,
    buffer: &[u8],
    what: &'static str,
) -> Result<(), FormatError> {
    match kinds.iter().find(|&&kind| kind as usize >= count) {
        Some(kind) => Err(FormatError::Invalid { at: at_of(kind, buffer), what }),
        None => Ok(()),
    }
}

fn hygiene_bits(hygiene: HygieneId) -> u32 {
    hygiene.0.into_u32()
}

fn hygiene_of(bits: u32) -> HygieneId {
    HygieneId(syntax_context(bits).expect("a syntax context of u32::MAX, refused when the buffer was read"))
}

fn packed_bits(id: ExprOrPatIdPacked) -> u32 {
    const PAT_BIT: u32 = 1 << (u32::BITS - 1);
    match id.unpack() {
        ExprOrPatId::ExprId(expr) => raw(expr),
        ExprOrPatId::PatId(pat) => raw(pat) | PAT_BIT,
    }
}

fn packed_of(bits: u32) -> ExprOrPatIdPacked {
    const PAT_BIT: u32 = 1 << (u32::BITS - 1);
    match bits & PAT_BIT {
        0 => {
            let expr: ExprId = idx(bits);
            ExprOrPatIdPacked::from(expr)
        }
        _ => {
            let pat: PatId = idx(bits & !PAT_BIT);
            ExprOrPatIdPacked::from(pat)
        }
    }
}

/// An optional id as a word: the raw id plus one, 0 for `None`.
fn opt_raw<T>(id: Option<Idx<T>>) -> u32 {
    id.map_or(0, |id| raw(id) + 1)
}

fn opt_idx<T>(word: u32) -> Option<Idx<T>> {
    word.checked_sub(1).map(idx)
}

fn edition_of(bits: u32) -> Option<Edition> {
    (bits <= Edition::LATEST as u32).then(|| Edition::from_u32(bits))
}

fn syntax_kind(bits: u32) -> Option<SyntaxKind> {
    (bits < SyntaxKind::__LAST as u32).then(|| SyntaxKind::from(bits as u16))
}

/// The words of `bytes`, four to a word, after their length.
fn put_bytes(w: &mut Words, bytes: &[u8]) {
    w.len(bytes.len());
    for chunk in bytes.chunks(4) {
        let mut word = [0; 4];
        word[..chunk.len()].copy_from_slice(chunk);
        w.u32(u32::from_le_bytes(word));
    }
}

fn get_bytes(r: &mut Read<'_>) -> Result<Box<[u8]>, FormatError> {
    let len = r.u32()? as usize;
    let mut bytes = Vec::new();
    for _ in 0..len.div_ceil(4) {
        bytes.extend_from_slice(&r.u32()?.to_le_bytes());
    }
    if bytes[len..].iter().any(|&byte| byte != 0) {
        return Err(r.invalid("bytes padded with other than zeros"));
    }
    bytes.truncate(len);
    Ok(bytes.into_boxed_slice())
}

/// Writes `table` in its order, which is by id.
fn put_table<K: Code, V: Code>(w: &mut Words, table: &IdTable<K, V>) {
    w.len(table.entries().len());
    for (k, v) in table.entries() {
        k.put(w);
        v.put(w);
    }
}

fn get_table<K: Code + Ord + Copy, V: Code>(r: &mut Read<'_>) -> Result<IdTable<K, V>, FormatError> {
    let len = r.len()?;
    let entries = (0..len)
        .map(|_| Ok((K::get(r)?, V::get(r)?)))
        .collect::<Result<Box<[(K, V)]>, FormatError>>()?;
    IdTable::from_sorted(entries).ok_or_else(|| r.invalid("a table not sorted by id, or an id twice"))
}

/// Writes `map` densely: a slot for each id up to its last, each an `Option`.
fn put_arena_map<T, V: Code>(w: &mut Words, map: &ArenaMap<Idx<T>, V>) {
    let len = map.iter().next_back().map_or(0, |(id, _)| raw(id) as usize + 1);
    w.len(len);
    let mut entries = map.iter().peekable();
    for slot in 0..len {
        match entries.next_if(|&(id, _)| raw(id) as usize == slot) {
            Some((_, value)) => {
                w.u32(1);
                value.put(w);
            }
            None => w.u32(0),
        }
    }
}

fn get_arena_map<T, V: Code>(r: &mut Read<'_>) -> Result<ArenaMap<Idx<T>, V>, FormatError> {
    let len = r.len()?;
    let mut map = ArenaMap::default();
    for slot in 0..len {
        match r.get::<Option<V>>()? {
            Some(value) => {
                map.insert(idx(slot as u32), value);
            }
            None if slot + 1 == len => return Err(r.invalid("a map whose last slot is empty")),
            None => {}
        }
    }
    Ok(map)
}

fn file_bits(file: HirFileId) -> (u32, u64) {
    match file {
        HirFileId::FileId(file) => (FILE, bits(file)),
        HirFileId::MacroFile(call) => (MACRO_FILE, bits(call)),
    }
}

impl Src {
    /// The order of a [`SourceTable`]'s keys: the file (files before macro files, each by its
    /// salsa id), the range, the syntax kind.
    fn key(&self) -> (u32, u64, u32, u32, u32) {
        (self.file_kind, self.file, self.start, self.end, self.kind)
    }

    fn of<N: AstNode>(src: &InFile<AstPtr<N>>) -> Src {
        let (file_kind, file) = file_bits(src.file_id);
        let range = src.value.text_range();
        Src {
            file,
            file_kind,
            kind: u32::from(u16::from(src.value.kind())),
            start: range.start().into(),
            end: range.end().into(),
        }
    }

    /// The pointer this is, `None` for no pointer.
    fn get<N: AstNode>(&self) -> Result<Option<InFile<AstPtr<N>>>, &'static str> {
        let file = match self.file_kind {
            0 if *self == Src::default() => return Ok(None),
            0 => return Err("an empty slot that is not zeros"),
            FILE => HirFileId::FileId(FromId::from_id(salsa_id(self.file).ok_or("a salsa id of index 0")?)),
            MACRO_FILE => {
                HirFileId::MacroFile(FromId::from_id(salsa_id(self.file).ok_or("a salsa id of index 0")?))
            }
            _ => return Err("a file kind out of range"),
        };
        let kind = syntax_kind(self.kind).ok_or("a syntax kind out of range")?;
        if self.start > self.end {
            return Err("a range that ends before it starts");
        }
        let range = TextRange::new(TextSize::from(self.start), TextSize::from(self.end));
        let ptr = AstPtr::try_from_raw(SyntaxNodePtr::from_parts(kind, range))
            .ok_or("a syntax pointer of a kind its type does not cast")?;
        Ok(Some(InFile::new(file, ptr)))
    }
}

/// `map` as a dense column, up to its last id.
fn back_column<T, N: AstNode>(map: &ArenaMap<Idx<T>, InFile<AstPtr<N>>>) -> Vec<Src> {
    let len = map.iter().next_back().map_or(0, |(id, _)| raw(id) as usize + 1);
    let mut column = vec![Src::default(); len];
    for (id, src) in map.iter() {
        column[raw(id) as usize] = Src::of(src);
    }
    column
}

fn check_back_column<N: AstNode>(column: &[Src], buffer: &[u8]) -> Result<(), FormatError> {
    for src in column {
        src.get::<N>().map_err(|what| FormatError::Invalid { at: at_of(src, buffer), what })?;
    }
    match column.last() {
        Some(last) => ensure(last.file_kind != 0, last, buffer, "a map whose last slot is empty"),
        None => Ok(()),
    }
}

fn back_map<T, N: AstNode>(
    column: &[Src],
    buffer: &[u8],
) -> Result<ArenaMap<Idx<T>, InFile<AstPtr<N>>>, FormatError> {
    let mut map = ArenaMap::default();
    for (slot, src) in column.iter().enumerate() {
        let found =
            src.get::<N>().map_err(|what| FormatError::Invalid { at: at_of(src, buffer), what })?;
        if let Some(found) = found {
            map.insert(idx(slot as u32), found);
        }
    }
    Ok(map)
}

/// A [`SourceTable`]'s keys and the word of each value, in the table's order.
fn table_columns<N: AstNode, V, W>(
    table: &SourceTable<InFile<AstPtr<N>>, V>,
    word: impl Fn(&V) -> W,
) -> (Vec<Src>, Vec<W>) {
    let (keys, words): (Vec<Src>, Vec<W>) =
        table.entries().iter().map(|(src, value)| (Src::of(src), word(value))).unzip();
    debug_assert!(
        keys.windows(2).all(|pair| pair[0].key() < pair[1].key()),
        "a source table not in the order of `Src::key`, which no reader accepts"
    );
    (keys, words)
}

/// Checks a [`SourceTable`]'s columns in place: a value for each key, each key a pointer of its
/// type, the keys in strictly increasing order of [`Src::key`].
fn check_table<N: AstNode, W>(
    keys: &[Src],
    values: &[W],
    root: &SourceMapRoot,
    buffer: &[u8],
) -> Result<(), FormatError> {
    let paired = keys.len() == values.len();
    ensure(paired, root, buffer, "a source table that is not one value for each key")?;
    for src in keys {
        let found =
            src.get::<N>().map_err(|what| FormatError::Invalid { at: at_of(src, buffer), what })?;
        ensure(found.is_some(), src, buffer, "an empty key in a source table")?;
    }
    keys.windows(2).try_for_each(|pair| {
        let ordered = pair[0].key() < pair[1].key();
        ensure(ordered, &pair[1], buffer, "a source table out of order, or a key twice")
    })
}

/// The [`SourceTable`] of columns [`check_table`] accepted, in their order.
fn source_table<N: AstNode, W: Copy, V>(
    keys: &[Src],
    values: &[W],
    buffer: &[u8],
    value: impl Fn(W) -> V,
) -> Result<SourceTable<InFile<AstPtr<N>>, V>, FormatError> {
    let entries = (keys.iter().zip(values))
        .map(|(src, &word)| {
            let at = at_of(src, buffer);
            let found = src.get::<N>().map_err(|what| FormatError::Invalid { at, what })?;
            let found =
                found.ok_or(FormatError::Invalid { at, what: "an empty key in a source table" })?;
            Ok((found, value(word)))
        })
        .collect::<Result<Box<[_]>, FormatError>>()?;
    let at = keys.first().map_or(0, |src| at_of(src, buffer));
    SourceTable::from_sorted(entries)
        .ok_or(FormatError::Invalid { at, what: "a source table out of order, or a key twice" })
}

/// The word of `src`'s value in a [`SourceTable`]'s checked columns, by binary search.
fn table_get<N: AstNode, W: Copy>(keys: &[Src], values: &[W], src: &InFile<AstPtr<N>>) -> Option<W> {
    let at = keys.binary_search_by_key(&Src::of(src).key(), Src::key).ok()?;
    values.get(at).copied()
}

/// The ids a store's nodes may name: what each of its arenas holds.
struct Ids<'s> {
    exprs: u32,
    pats: u32,
    lifetimes: u32,
    types: &'s TypeColumns<Frozen>,
}

type Checked = Result<(), &'static str>;

fn below(id: u32, len: u32, what: &'static str) -> Checked {
    match id < len {
        true => Ok(()),
        false => Err(what),
    }
}

impl Ids<'_> {
    fn expr(&self, expr: ExprId) -> Checked {
        below(raw(expr), self.exprs, "an expression id past its arena")
    }

    fn expr_opt(&self, expr: Option<ExprId>) -> Checked {
        expr.map_or(Ok(()), |expr| self.expr(expr))
    }

    fn pat(&self, pat: PatId) -> Checked {
        below(raw(pat), self.pats, "a pattern id past its arena")
    }

    fn lifetime(&self, lifetime: LifetimeRefId) -> Checked {
        below(raw(lifetime), self.lifetimes, "a lifetime id past its arena")
    }

    fn ty(&self, ty: TypeRefId) -> Checked {
        below(raw(ty), self.types.kind.len() as u32, "a type id past its arena")
    }

    fn ty_opt(&self, ty: Option<TypeRefId>) -> Checked {
        ty.map_or(Ok(()), |ty| self.ty(ty))
    }

    fn path_id(&self, path: PathId) -> Checked {
        match self.types.kind.get(raw(path.type_ref()) as usize) {
            Some(&kind) if kind == TypeRefKind::Path as u8 => Ok(()),
            Some(_) => Err("a path id of a type that is not a path"),
            None => Err("a type id past its arena"),
        }
    }

    fn path(&self, path: &Path) -> Checked {
        match path {
            Path::BarePath(_) | Path::LangItem(..) => Ok(()),
            Path::Normal(path) => {
                let NormalPath { generic_args, type_anchor, mod_path: _ } = &**path;
                generic_args.iter().flatten().try_for_each(|args| self.generic_args(args))?;
                self.ty_opt(*type_anchor)
            }
        }
    }

    fn generic_args(&self, args: &GenericArgs) -> Checked {
        let GenericArgs { args, has_self_type: _, bindings, parenthesized: _ } = args;
        for arg in args {
            match *arg {
                GenericArg::Type(ty) => self.ty(ty)?,
                GenericArg::Lifetime(lifetime) => self.lifetime(lifetime)?,
                GenericArg::Const(ConstRef { expr }) => self.expr(expr)?,
            }
        }
        for AssociatedTypeBinding { name: _, args, type_ref, bounds } in bindings {
            args.iter().try_for_each(|args| self.generic_args(args))?;
            self.ty_opt(*type_ref)?;
            self.bounds(bounds)?;
        }
        Ok(())
    }

    fn bounds(&self, bounds: &[TypeBound]) -> Checked {
        bounds.iter().try_for_each(|bound| match bound {
            &TypeBound::Path(path, _) | &TypeBound::ForLifetime(_, path) => self.path_id(path),
            &TypeBound::Lifetime(lifetime) => self.lifetime(lifetime),
            TypeBound::Use(args) => args.iter().try_for_each(|arg| match *arg {
                UseArgRef::Lifetime(lifetime) => self.lifetime(lifetime),
                UseArgRef::Name(_) => Ok(()),
            }),
            TypeBound::Error => Ok(()),
        })
    }

    fn statement(&self, statement: &Statement) -> Checked {
        match *statement {
            Statement::Let { pat, type_ref, initializer, else_branch, is_super: _ } => {
                self.pat(pat)?;
                self.ty_opt(type_ref)?;
                self.expr_opt(initializer)?;
                self.expr_opt(else_branch)
            }
            Statement::Expr { expr, has_semi: _ } => self.expr(expr),
            Statement::Item(_) => Ok(()),
        }
    }

    fn asm(&self, asm: &InlineAsm) -> Checked {
        asm.operands.iter().try_for_each(|(_, operand)| match operand {
            &AsmOperand::In { reg: _, expr }
            | &AsmOperand::InOut { reg: _, expr, late: _ }
            | &AsmOperand::Label(expr)
            | &AsmOperand::Const(expr) => self.expr(expr),
            &AsmOperand::Out { reg: _, expr, late: _ } => self.expr_opt(expr),
            &AsmOperand::SplitInOut { reg: _, in_expr, out_expr, late: _ } => {
                self.expr(in_expr)?;
                self.expr_opt(out_expr)
            }
            AsmOperand::Sym(path) => self.path(path),
        })
    }

    /// The ids the side tables' values hold, which [`HirColumns::check`] does not look into.
    fn side_tables(&self, side: &SideTables) -> Checked {
        side.paths.iter().try_for_each(|path| self.path(path))?;
        side.generic_args.iter().try_for_each(|args| self.generic_args(args))?;
        side.offset_ofs.iter().try_for_each(|offset_of| self.ty(offset_of.container))?;
        side.asms.iter().try_for_each(|asm| self.asm(asm))
    }

    /// The ids the type side table of paths holds (type anchors, generic args), which
    /// [`TypeColumns::check`] leaves to the encoding.
    fn type_paths(&self) -> Checked {
        self.types.paths.iter().try_for_each(|path| self.path(path))
    }
}

/// Writes each value's fields and returns the column of their nodes.
fn column<'v, T: 'v>(
    w: &mut Words,
    values: impl Iterator<Item = &'v T>,
    put: fn(&T, &mut Words) -> u32,
) -> Vec<Node> {
    values
        .map(|value| {
            let at = w.at();
            let kind = put(value, w);
            Node { kind, at }
        })
        .collect()
}

/// Reads the arena of `nodes`, the fields of each from its word.
fn arena<T, K>(
    nodes: &[Node],
    words: &Read<'_>,
    buffer: &[u8],
    kind: fn(u32) -> Option<K>,
    get: fn(K, &mut Read<'_>) -> Result<T, FormatError>,
) -> Result<Arena<T>, FormatError> {
    let mut arena = Arena::with_capacity(nodes.len());
    for node in nodes {
        let kind = kind(node.kind).ok_or(FormatError::Invalid {
            at: at_of(node, buffer),
            what: "a node kind out of range",
        })?;
        arena.alloc(get(kind, &mut words.from(node.at))?);
    }
    Ok(arena)
}

fn only_kind(kind: u32) -> Option<()> {
    (kind == 0).then_some(())
}

/// A column of `hir`, empty for a store without expressions.
fn slice<'c, T>(
    hir: Option<&'c HirColumns<Frozen>>,
    column: impl FnOnce(&'c HirColumns<Frozen>) -> &'c [T],
) -> &'c [T] {
    hir.map_or(&[][..], column)
}

/// The cold side tables of [`HirColumns`], read back from the words.
struct SideTables {
    names: Vec<Name>,
    paths: Vec<Path>,
    literals: Vec<Literal>,
    generic_args: Vec<GenericArgs>,
    offset_ofs: Vec<OffsetOf>,
    asms: Vec<InlineAsm>,
}

impl SideTables {
    /// Writes the side tables of `hir`, each a count and its values, and returns the word they
    /// begin at.
    fn put(w: &mut Words, hir: Option<&HirColumns<Frozen>>) -> u32 {
        let at = w.at();
        w.seq(slice(hir, |c| &c.names[..]).iter());
        w.seq(slice(hir, |c| &c.paths[..]).iter());
        w.seq(slice(hir, |c| &c.literals[..]).iter());
        w.seq(slice(hir, |c| &c.generic_args[..]).iter());
        w.seq(slice(hir, |c| &c.offset_ofs[..]).iter());
        w.seq(slice(hir, |c| &c.asms[..]).iter());
        at
    }

    fn get(r: &mut Read<'_>) -> Result<SideTables, FormatError> {
        Ok(SideTables {
            names: r.seq::<Name, _>()?,
            paths: r.seq::<Path, _>()?,
            literals: r.seq::<Literal, _>()?,
            generic_args: r.seq::<GenericArgs, _>()?,
            offset_ofs: r.seq::<OffsetOf, _>()?,
            asms: r.seq::<InlineAsm, _>()?,
        })
    }

    fn is_empty(&self) -> bool {
        let SideTables { names, paths, literals, generic_args, offset_ofs, asms } = self;
        names.is_empty()
            && paths.is_empty()
            && literals.is_empty()
            && generic_args.is_empty()
            && offset_ofs.is_empty()
            && asms.is_empty()
    }
}

/// The side tables of [`TypeColumns`], read back from the words.
struct TypeSide {
    bounds: Vec<TypeBound>,
    paths: Vec<Path>,
    refs: Vec<RefType>,
    arrays: Vec<ArrayType>,
    fns: Vec<FnType>,
    type_params: Vec<TypeParamId>,
}

impl TypeSide {
    /// Writes the side tables of `types`, each a count and its values, and returns the word they
    /// begin at.
    fn put(w: &mut Words, types: &TypeColumns<Frozen>) -> u32 {
        let at = w.at();
        w.seq(types.bounds.iter());
        w.seq(types.paths.iter());
        w.seq(types.refs.iter());
        w.seq(types.arrays.iter());
        w.seq(types.fns.iter());
        w.seq(types.type_params.iter());
        at
    }

    fn get(r: &mut Read<'_>) -> Result<TypeSide, FormatError> {
        Ok(TypeSide {
            bounds: r.seq::<TypeBound, _>()?,
            paths: r.seq::<Path, _>()?,
            refs: r.seq::<RefType, _>()?,
            arrays: r.seq::<ArrayType, _>()?,
            fns: r.seq::<FnType, _>()?,
            type_params: r.seq::<TypeParamId, _>()?,
        })
    }
}

/// The names of record fields and field patterns, each text once, for [`FieldRec`]s to point
/// at.
#[derive(Default)]
struct FieldNames {
    text: String,
    /// Each name's start in `text` and its length.
    by_symbol: FxHashMap<Symbol, (u32, u32)>,
}

impl FieldNames {
    fn add(&mut self, name: &Name) {
        let symbol = name.symbol();
        if self.by_symbol.contains_key(symbol) {
            return;
        }
        let start =
            u32::try_from(self.text.len()).expect("more than 4 GiB of field names in one buffer");
        let len = u32::try_from(symbol.as_str().len()).expect("a name of 4 GiB or more");
        self.text.push_str(symbol.as_str());
        self.by_symbol.insert(symbol.clone(), (start, len));
    }

    /// Writes `fields`, each a name and an id, as a column of [`FieldRec`]s, their names in
    /// this text, which was written at `text_at`.
    fn column<'f>(
        &self,
        out: &mut Writer,
        text_at: u64,
        fields: impl Iterator<Item = (&'f Name, u32)>,
    ) -> Column {
        let at = out.next::<FieldRec>();
        let records: Vec<FieldRec> = (fields.enumerate())
            .map(|(index, (name, id))| {
                let (start, len) = self.by_symbol[name.symbol()];
                let field_at =
                    at + (index * size_of::<FieldRec>() + offset_of!(FieldRec, name)) as u64;
                let name = match len {
                    0 => Str::EMPTY,
                    len => Str::between(field_at, text_at + u64::from(start), len as usize),
                };
                FieldRec { name, id }
            })
            .collect();
        let column = Column::push(out, &records);
        debug_assert_eq!(column.at, at, "`Writer::next` is not where `push_slice` writes");
        column
    }
}

/// The name a [`FieldRec`] holds, which is in `buffer`.
fn field_name(field: &FieldRec, buffer: &[u8]) -> Result<Name, FormatError> {
    let text = field.name.as_str(buffer)?;
    ensure(!text.starts_with("r#"), field, buffer, "a name written with its `r#`")?;
    Ok(Name::new_symbol_root(Symbol::intern(text)))
}

/// `statement` as a record, a macro definition it holds written in `w`.
fn statement_rec(statement: &Statement, w: &mut Words) -> StatementRec {
    let rec = |kind, id| StatementRec { kind, id, type_ref: 0, initializer: 0, else_branch: 0 };
    match statement {
        &Statement::Let { pat, type_ref, initializer, else_branch, is_super } => StatementRec {
            kind: if is_super { SUPER_LET } else { LET },
            id: raw(pat),
            type_ref: opt_raw(type_ref),
            initializer: opt_raw(initializer),
            else_branch: opt_raw(else_branch),
        },
        &Statement::Expr { expr, has_semi } => {
            rec(if has_semi { EXPR_SEMI } else { EXPR }, raw(expr))
        }
        Statement::Item(Item::Other) => rec(OTHER_ITEM, 0),
        Statement::Item(Item::MacroDef(def)) => {
            let at = w.at();
            w.put(&**def);
            rec(MACRO_DEF, at)
        }
    }
}

/// The statement `rec` is, which is in `buffer`, a macro definition read from `words`.
fn statement_of(
    rec: &StatementRec,
    words: &Read<'_>,
    buffer: &[u8],
) -> Result<Statement, FormatError> {
    let StatementRec { kind, id, type_ref, initializer, else_branch } = *rec;
    let unused = |words: &[u32]| {
        let zeros = words.iter().all(|&word| word == 0);
        ensure(zeros, rec, buffer, "a statement word its kind has no use for that is not 0")
    };
    Ok(match kind {
        LET | SUPER_LET => Statement::Let {
            pat: idx(id),
            type_ref: opt_idx(type_ref),
            initializer: opt_idx(initializer),
            else_branch: opt_idx(else_branch),
            is_super: kind == SUPER_LET,
        },
        EXPR | EXPR_SEMI => {
            unused(&[type_ref, initializer, else_branch])?;
            Statement::Expr { expr: idx(id), has_semi: kind == EXPR_SEMI }
        }
        OTHER_ITEM => {
            unused(&[id, type_ref, initializer, else_branch])?;
            Statement::Item(Item::Other)
        }
        MACRO_DEF => {
            unused(&[type_ref, initializer, else_branch])?;
            Statement::Item(Item::MacroDef(Box::new(words.from(id).get()?)))
        }
        _ => {
            let what = "a statement kind out of range";
            return Err(FormatError::Invalid { at: at_of(rec, buffer), what });
        }
    })
}

/// Each value of `records`, which are in `buffer`, as `of` makes it and `check` accepts it.
fn decoded<R, T>(
    records: &[R],
    buffer: &[u8],
    of: impl Fn(&R) -> Result<T, FormatError>,
    check: impl Fn(&T) -> Checked,
) -> Result<Vec<T>, FormatError> {
    (records.iter())
        .map(|record| {
            let value = of(record)?;
            check(&value).map_err(|what| FormatError::Invalid { at: at_of(record, buffer), what })?;
            Ok(value)
        })
        .collect()
}

/// Where everything of a store but its root was written.
struct StoreColumns {
    words: Placed,
    side: u32,
    ty_side: u32,
    expr_kind: Column,
    expr_data: Column,
    pat_kind: Column,
    pat_data: Column,
    extra: Column,
    expr_lists: Column,
    pat_lists: Column,
    statements: Column,
    arms: Column,
    record_fields: Column,
    field_pats: Column,
    type_lists: Column,
    bindings: Column,
    labels: Column,
    ty_kind: Column,
    ty_data: Column,
    ty_extra: Column,
    ty_lists: Column,
    lifetimes: Column,
    binding_owners: Column,
    block_scopes: Column,
    ident_hygiene: Column,
    block_editions: Column,
    overflow_inheriting: Column,
    expr_roots: Column,
    type_hygiene: Column,
    flags: u32,
}

impl StoreColumns {
    fn write(store: &ExpressionStore, out: &mut Writer) -> StoreColumns {
        let ExpressionStore { expr_only, types, lifetimes, type_hygiene } = store;
        let expr_only = expr_only.as_deref();
        let hir = expr_only.map(|it| &it.columns);
        let mut w = Words::new();
        let side = SideTables::put(&mut w, hir);
        let ty_side = TypeSide::put(&mut w, types);
        let statements: Vec<StatementRec> = (slice(hir, |c| &c.statements[..]).iter())
            .map(|statement| statement_rec(statement, &mut w))
            .collect();
        let bindings =
            column(&mut w, expr_only.into_iter().flat_map(|it| it.bindings.values()), put_binding);
        let labels =
            column(&mut w, expr_only.into_iter().flat_map(|it| it.labels.values()), put_label);
        let lifetimes = column(&mut w, lifetimes.values(), put_lifetime);
        let words = w.place(out);

        let (record_fields, field_pats) =
            (slice(hir, |c| &c.record_fields[..]), slice(hir, |c| &c.field_pats[..]));
        let mut names = FieldNames::default();
        record_fields.iter().for_each(|field| names.add(&field.name));
        field_pats.iter().for_each(|field| names.add(&field.name));
        let text_at = out.push_str(&names.text);
        let record_fields = names.column(
            out,
            text_at,
            record_fields.iter().map(|field| (&field.name, raw(field.expr))),
        );
        let field_pats =
            names.column(out, text_at, field_pats.iter().map(|field| (&field.name, raw(field.pat))));
        let arms: Vec<ArmRec> = (slice(hir, |c| &c.arms[..]).iter())
            .map(|arm| ArmRec { pat: raw(arm.pat), guard: opt_raw(arm.guard), expr: raw(arm.expr) })
            .collect();
        let expr_lists: Vec<u32> =
            slice(hir, |c| &c.expr_lists[..]).iter().map(|&expr| raw(expr)).collect();
        let pat_lists: Vec<u32> =
            slice(hir, |c| &c.pat_lists[..]).iter().map(|&pat| raw(pat)).collect();
        let type_lists: Vec<u32> =
            slice(hir, |c| &c.type_lists[..]).iter().map(|&ty| opt_raw(ty)).collect();
        let ty_lists: Vec<u32> = types.lists.iter().map(|&ty| raw(ty)).collect();

        let (mut binding_owners, mut block_scopes, mut ident_hygiene) = (vec![], vec![], vec![]);
        let (mut block_editions, mut overflow_inheriting, mut expr_roots) = (vec![], vec![], vec![]);
        if let Some(it) = expr_only {
            let ExpressionOnlyStore {
                columns: _,
                bindings: _,
                labels: _,
                binding_owners: owners,
                block_scopes: blocks,
                ident_hygiene: hygiene,
                block_editions: editions,
                overflow_inheriting: inheriting,
                expr_roots: roots,
            } = it;
            binding_owners = owners.to_vec();
            block_scopes = blocks.iter().map(|&block| bits(block)).collect();
            ident_hygiene = (hygiene.iter())
                .map(|&(id, hygiene)| IdPair { a: packed_bits(id), b: hygiene_bits(hygiene) })
                .collect();
            block_editions = (editions.iter())
                .map(|&(block, edition)| IdPair { a: raw(block), b: edition as u32 })
                .collect();
            overflow_inheriting = inheriting.iter().map(|&closure| raw(closure)).collect();
            expr_roots = (roots.iter())
                .map(|root| ExprRootRec {
                    root: raw(root.root),
                    exprs_end: raw(root.exprs_end),
                    pats_end: raw(root.pats_end),
                    bindings_end: raw(root.bindings_end),
                })
                .collect();
        }
        let type_hygiene_pairs: Vec<IdPair> = (type_hygiene.iter().flat_map(|it| it.iter()))
            .map(|&(ty, hygiene)| IdPair { a: raw(ty), b: hygiene_bits(hygiene) })
            .collect();
        let flags = (if expr_only.is_some() { HAS_EXPRS } else { 0 })
            | (if type_hygiene.is_some() { HAS_TYPE_HYGIENE } else { 0 });
        StoreColumns {
            words,
            side,
            ty_side,
            expr_kind: Column::push(out, slice(hir, |c| &c.expr_kind[..])),
            expr_data: Column::push(out, slice(hir, |c| &c.expr_data[..])),
            pat_kind: Column::push(out, slice(hir, |c| &c.pat_kind[..])),
            pat_data: Column::push(out, slice(hir, |c| &c.pat_data[..])),
            extra: Column::push(out, slice(hir, |c| &c.extra[..])),
            expr_lists: Column::push(out, &expr_lists),
            pat_lists: Column::push(out, &pat_lists),
            statements: Column::push(out, &statements),
            arms: Column::push(out, &arms),
            record_fields,
            field_pats,
            type_lists: Column::push(out, &type_lists),
            bindings: Column::push(out, &bindings),
            labels: Column::push(out, &labels),
            ty_kind: Column::push(out, &types.kind[..]),
            ty_data: Column::push(out, &types.data[..]),
            ty_extra: Column::push(out, &types.extra[..]),
            ty_lists: Column::push(out, &ty_lists),
            lifetimes: Column::push(out, &lifetimes),
            binding_owners: Column::push(out, &binding_owners),
            block_scopes: Column::push(out, &block_scopes),
            ident_hygiene: Column::push(out, &ident_hygiene),
            block_editions: Column::push(out, &block_editions),
            overflow_inheriting: Column::push(out, &overflow_inheriting),
            expr_roots: Column::push(out, &expr_roots),
            type_hygiene: Column::push(out, &type_hygiene_pairs),
            flags,
        }
    }

    /// The root of these columns, for one written at `at`.
    fn rec(&self, at: u64) -> StoreRoot {
        let field = |offset: usize| at + offset as u64;
        StoreRoot {
            expr_kind: self.expr_kind.slice(field(offset_of!(StoreRoot, expr_kind))),
            expr_data: self.expr_data.slice(field(offset_of!(StoreRoot, expr_data))),
            pat_kind: self.pat_kind.slice(field(offset_of!(StoreRoot, pat_kind))),
            pat_data: self.pat_data.slice(field(offset_of!(StoreRoot, pat_data))),
            extra: self.extra.slice(field(offset_of!(StoreRoot, extra))),
            expr_lists: self.expr_lists.slice(field(offset_of!(StoreRoot, expr_lists))),
            pat_lists: self.pat_lists.slice(field(offset_of!(StoreRoot, pat_lists))),
            statements: self.statements.slice(field(offset_of!(StoreRoot, statements))),
            arms: self.arms.slice(field(offset_of!(StoreRoot, arms))),
            record_fields: self.record_fields.slice(field(offset_of!(StoreRoot, record_fields))),
            field_pats: self.field_pats.slice(field(offset_of!(StoreRoot, field_pats))),
            type_lists: self.type_lists.slice(field(offset_of!(StoreRoot, type_lists))),
            bindings: self.bindings.slice(field(offset_of!(StoreRoot, bindings))),
            labels: self.labels.slice(field(offset_of!(StoreRoot, labels))),
            ty_kind: self.ty_kind.slice(field(offset_of!(StoreRoot, ty_kind))),
            ty_data: self.ty_data.slice(field(offset_of!(StoreRoot, ty_data))),
            ty_extra: self.ty_extra.slice(field(offset_of!(StoreRoot, ty_extra))),
            ty_lists: self.ty_lists.slice(field(offset_of!(StoreRoot, ty_lists))),
            lifetimes: self.lifetimes.slice(field(offset_of!(StoreRoot, lifetimes))),
            binding_owners: self.binding_owners.slice(field(offset_of!(StoreRoot, binding_owners))),
            block_scopes: self.block_scopes.slice(field(offset_of!(StoreRoot, block_scopes))),
            ident_hygiene: self.ident_hygiene.slice(field(offset_of!(StoreRoot, ident_hygiene))),
            block_editions: self.block_editions.slice(field(offset_of!(StoreRoot, block_editions))),
            overflow_inheriting: (self.overflow_inheriting)
                .slice(field(offset_of!(StoreRoot, overflow_inheriting))),
            expr_roots: self.expr_roots.slice(field(offset_of!(StoreRoot, expr_roots))),
            type_hygiene: self.type_hygiene.slice(field(offset_of!(StoreRoot, type_hygiene))),
            words: self.words.rec(field(offset_of!(StoreRoot, words))),
            side: self.side,
            ty_side: self.ty_side,
            flags: self.flags,
        }
    }
}

/// An [`ExpressionStore`] read in place from its buffer.
#[derive(Clone, Copy)]
pub struct ExpressionStoreRef<'a> {
    buffer: &'a [u8],
    root: &'a StoreRoot,
    expr_kind: &'a [u8],
    expr_data: &'a [u32],
    pat_kind: &'a [u8],
    pat_data: &'a [u32],
    extra: &'a [u32],
    expr_lists: &'a [u32],
    pat_lists: &'a [u32],
    statements: &'a [StatementRec],
    arms: &'a [ArmRec],
    record_fields: &'a [FieldRec],
    field_pats: &'a [FieldRec],
    type_lists: &'a [u32],
    bindings: &'a [Node],
    labels: &'a [Node],
    ty_kind: &'a [u8],
    ty_data: &'a [u32],
    ty_extra: &'a [u32],
    ty_lists: &'a [u32],
    lifetimes: &'a [Node],
    binding_owners: &'a [u32],
    block_scopes: &'a [u64],
    ident_hygiene: &'a [IdPair],
    block_editions: &'a [IdPair],
    overflow_inheriting: &'a [u32],
    expr_roots: &'a [ExprRootRec],
    type_hygiene: &'a [IdPair],
}

impl<'a> ExpressionStoreRef<'a> {
    /// The store of `root`, in `buffer`, its columns checked.
    fn new(root: &'a StoreRoot, buffer: &'a [u8]) -> Result<ExpressionStoreRef<'a>, FormatError> {
        let store = ExpressionStoreRef {
            buffer,
            root,
            expr_kind: root.expr_kind.follow(buffer)?,
            expr_data: root.expr_data.follow(buffer)?,
            pat_kind: root.pat_kind.follow(buffer)?,
            pat_data: root.pat_data.follow(buffer)?,
            extra: root.extra.follow(buffer)?,
            expr_lists: root.expr_lists.follow(buffer)?,
            pat_lists: root.pat_lists.follow(buffer)?,
            statements: root.statements.follow(buffer)?,
            arms: root.arms.follow(buffer)?,
            record_fields: root.record_fields.follow(buffer)?,
            field_pats: root.field_pats.follow(buffer)?,
            type_lists: root.type_lists.follow(buffer)?,
            bindings: root.bindings.follow(buffer)?,
            labels: root.labels.follow(buffer)?,
            ty_kind: root.ty_kind.follow(buffer)?,
            ty_data: root.ty_data.follow(buffer)?,
            ty_extra: root.ty_extra.follow(buffer)?,
            ty_lists: root.ty_lists.follow(buffer)?,
            lifetimes: root.lifetimes.follow(buffer)?,
            binding_owners: root.binding_owners.follow(buffer)?,
            block_scopes: root.block_scopes.follow(buffer)?,
            ident_hygiene: root.ident_hygiene.follow(buffer)?,
            block_editions: root.block_editions.follow(buffer)?,
            overflow_inheriting: root.overflow_inheriting.follow(buffer)?,
            expr_roots: root.expr_roots.follow(buffer)?,
            type_hygiene: root.type_hygiene.follow(buffer)?,
        };
        store.check()?;
        Ok(store)
    }

    fn check(&self) -> Result<(), FormatError> {
        let (root, buffer) = (self.root, self.buffer);
        let flags = root.flags;
        ensure(flags & !(HAS_EXPRS | HAS_TYPE_HYGIENE) == 0, root, buffer, "store flags out of range")?;
        let words = root.words.words.len;
        let datas = self.expr_data.len() == self.expr_kind.len()
            && self.pat_data.len() == self.pat_kind.len()
            && self.ty_data.len() == self.ty_kind.len();
        ensure(datas, root, buffer, "a data column that is not one word for each node")?;
        check_kinds(self.expr_kind, ExprKind::ALL.len(), buffer, "an expression kind out of range")?;
        check_kinds(self.pat_kind, PatKind::ALL.len(), buffer, "a pattern kind out of range")?;
        check_kinds(self.ty_kind, TypeRefKind::ALL.len(), buffer, "a type kind out of range")?;
        check_nodes(self.bindings, 1, words, buffer, "a binding kind out of range")?;
        check_nodes(self.labels, 1, words, buffer, "a label kind out of range")?;
        check_nodes(
            self.lifetimes,
            LifetimeRefKind::ALL.len(),
            words,
            buffer,
            "a lifetime kind out of range",
        )?;
        ensure(root.side <= words, root, buffer, "side tables that begin past the words")?;
        ensure(root.ty_side <= words, root, buffer, "type side tables that begin past the words")?;
        if flags & HAS_EXPRS == 0 {
            let empty = self.expr_kind.is_empty()
                && self.pat_kind.is_empty()
                && self.extra.is_empty()
                && self.expr_lists.is_empty()
                && self.pat_lists.is_empty()
                && self.statements.is_empty()
                && self.arms.is_empty()
                && self.record_fields.is_empty()
                && self.field_pats.is_empty()
                && self.type_lists.is_empty()
                && self.bindings.is_empty()
                && self.labels.is_empty()
                && self.binding_owners.is_empty()
                && self.block_scopes.is_empty()
                && self.ident_hygiene.is_empty()
                && self.block_editions.is_empty()
                && self.overflow_inheriting.is_empty()
                && self.expr_roots.is_empty();
            ensure(empty, root, buffer, "expression columns in a store without expressions")?;
        }
        if flags & HAS_TYPE_HYGIENE == 0 {
            ensure(self.type_hygiene.is_empty(), root, buffer, "type hygiene the store has not")?;
        }
        let (exprs, pats, bindings) =
            (self.expr_kind.len() as u32, self.pat_kind.len() as u32, self.bindings.len() as u32);
        let owners = self.binding_owners;
        ensure(
            owners.is_empty() || owners.len() == self.bindings.len(),
            root,
            buffer,
            "binding owners that are not one for each binding",
        )?;
        for owner in owners {
            ensure(*owner == NO_OWNER || *owner < exprs, owner, buffer, "a binding owner past the expressions")?;
        }
        for block in self.block_scopes {
            ensure(salsa_id(*block).is_some(), block, buffer, "a block id of index 0")?;
        }
        ascending(self.ident_hygiene, |it| it.a, buffer, "ident hygiene out of order")?;
        for pair in self.ident_hygiene {
            let id = match packed_of(pair.a).unpack() {
                ExprOrPatId::ExprId(expr) => raw(expr) < exprs,
                ExprOrPatId::PatId(pat) => raw(pat) < pats,
            };
            ensure(id, pair, buffer, "ident hygiene of an id past its arena")?;
            ensure(pair.b != u32::MAX, pair, buffer, "a syntax context of u32::MAX")?;
        }
        ascending(self.block_editions, |it| it.a, buffer, "block editions out of order")?;
        for pair in self.block_editions {
            ensure(pair.a < exprs, pair, buffer, "the edition of a block past the expressions")?;
            ensure(edition_of(pair.b).is_some(), pair, buffer, "an edition out of range")?;
        }
        ascending(self.overflow_inheriting, |&it| it, buffer, "closures out of order")?;
        for closure in self.overflow_inheriting {
            ensure(*closure < exprs, closure, buffer, "a closure past the expressions")?;
        }
        for (at, root) in self.expr_roots.iter().enumerate() {
            let fits = root.root < exprs
                && root.exprs_end <= exprs
                && root.pats_end <= pats
                && root.bindings_end <= bindings;
            ensure(fits, root, buffer, "an expression root past its arenas")?;
            if let Some(previous) = at.checked_sub(1).map(|at| &self.expr_roots[at]) {
                let ordered = previous.exprs_end <= root.exprs_end
                    && previous.pats_end <= root.pats_end
                    && previous.bindings_end <= root.bindings_end;
                ensure(ordered, root, buffer, "expression roots out of order")?;
            }
        }
        ascending(self.type_hygiene, |it| it.a, buffer, "type hygiene out of order")?;
        for pair in self.type_hygiene {
            ensure(pair.a < self.ty_kind.len() as u32, pair, buffer, "the hygiene of a type past its arena")?;
            ensure(pair.b != u32::MAX, pair, buffer, "a syntax context of u32::MAX")?;
        }
        Ok(())
    }

    /// Whether the store holds expressions, patterns, bindings or labels.
    pub fn has_exprs(self) -> bool {
        self.root.flags & HAS_EXPRS != 0
    }

    pub fn exprs_len(self) -> usize {
        self.expr_kind.len()
    }

    pub fn pats_len(self) -> usize {
        self.pat_kind.len()
    }

    pub fn bindings_len(self) -> usize {
        self.bindings.len()
    }

    pub fn labels_len(self) -> usize {
        self.labels.len()
    }

    pub fn types_len(self) -> usize {
        self.ty_kind.len()
    }

    pub fn lifetimes_len(self) -> usize {
        self.lifetimes.len()
    }

    /// Which variant `expr` is, `None` past the arena.
    pub fn expr_kind(self, expr: ExprId) -> Option<ExprKind> {
        ExprKind::from_u32(u32::from(*self.expr_kind.get(raw(expr) as usize)?))
    }

    pub fn pat_kind(self, pat: PatId) -> Option<PatKind> {
        PatKind::from_u32(u32::from(*self.pat_kind.get(raw(pat) as usize)?))
    }

    pub fn type_kind(self, ty: TypeRefId) -> Option<TypeRefKind> {
        TypeRefKind::from_u32(u32::from(*self.ty_kind.get(raw(ty) as usize)?))
    }

    pub fn lifetime_kind(self, lifetime: LifetimeRefId) -> Option<LifetimeRefKind> {
        LifetimeRefKind::from_u32(self.lifetimes.get(raw(lifetime) as usize)?.kind)
    }

    /// As [`ExpressionStore::binding_owner`].
    pub fn binding_owner(self, binding: BindingId) -> Option<ExprId> {
        let owner = *self.binding_owners.get(raw(binding) as usize)?;
        (owner != NO_OWNER).then(|| idx(owner))
    }

    /// As [`ExpressionStore::expr_roots`].
    pub fn expr_roots(self) -> impl DoubleEndedIterator<Item = ExprId> + 'a {
        self.expr_roots.iter().map(|root| idx(root.root))
    }

    /// The blocks of the store that may hold items, as [`ExpressionStore::blocks`] walks them.
    pub fn block_scopes(self) -> impl Iterator<Item = BlockId> + 'a {
        (self.block_scopes.iter())
            .filter_map(|&block| salsa_id(block))
            .map(<BlockId as FromId>::from_id)
    }

    /// As [`ExpressionStore::inherits_overflow_checks`].
    pub fn inherits_overflow_checks(self, closure: ExprId) -> bool {
        self.overflow_inheriting.binary_search(&raw(closure)).is_ok()
    }

    /// As [`ExpressionStore::block_edition`].
    pub fn block_edition(self, block: ExprId) -> Option<Edition> {
        let index = self.block_editions.binary_search_by_key(&raw(block), |it| it.a).ok()?;
        edition_of(self.block_editions[index].b)
    }

    /// The store, back as the value that was written.
    pub fn decode(self) -> Result<ExpressionStore, FormatError> {
        let buffer = self.buffer;
        let words = Read::new(&self.root.words, buffer);
        let lifetimes = arena(self.lifetimes, &words, buffer, LifetimeRefKind::from_u32, get_lifetime)?;
        let bindings = arena(self.bindings, &words, buffer, only_kind, get_binding)?;
        let labels = arena(self.labels, &words, buffer, only_kind, get_label)?;
        let side = SideTables::get(&mut words.from(self.root.side))?;
        let TypeSide { bounds, paths: type_paths, refs, arrays, fns, type_params } =
            TypeSide::get(&mut words.from(self.root.ty_side))?;
        let types = TypeColumns::<Frozen> {
            kind: self.ty_kind.into(),
            data: self.ty_data.into(),
            extra: self.ty_extra.into(),
            lists: self.ty_lists.iter().map(|&ty| idx(ty)).collect(),
            bounds: bounds.into(),
            paths: type_paths.into(),
            refs: refs.into(),
            arrays: arrays.into(),
            fns: fns.into(),
            type_params: type_params.into(),
        };
        let (exprs, pats) = (self.expr_kind.len(), self.pat_kind.len());
        (types.check(exprs, pats, lifetimes.len()))
            .map_err(|what| FormatError::Invalid { at: at_of(self.root, buffer), what })?;
        let ids = Ids {
            exprs: exprs as u32,
            pats: pats as u32,
            lifetimes: lifetimes.len() as u32,
            types: &types,
        };
        (ids.type_paths())
            .map_err(|what| FormatError::Invalid { at: at_of(&self.root.words, buffer), what })?;
        (ids.side_tables(&side))
            .map_err(|what| FormatError::Invalid { at: at_of(&self.root.words, buffer), what })?;
        if !self.has_exprs() {
            ensure(side.is_empty(), self.root, buffer, "side tables in a store without expressions")?;
        }

        let statements: Vec<Statement> = decoded(
            self.statements,
            buffer,
            |rec| statement_of(rec, &words, buffer),
            |statement| ids.statement(statement),
        )?;
        let arms: Vec<MatchArm> = decoded(
            self.arms,
            buffer,
            |rec| Ok(MatchArm { pat: idx(rec.pat), guard: opt_idx(rec.guard), expr: idx(rec.expr) }),
            |arm| {
                ids.pat(arm.pat)?;
                ids.expr_opt(arm.guard)?;
                ids.expr(arm.expr)
            },
        )?;
        let record_fields: Vec<RecordLitField> = decoded(
            self.record_fields,
            buffer,
            |rec| Ok(RecordLitField { name: field_name(rec, buffer)?, expr: idx(rec.id) }),
            |field| ids.expr(field.expr),
        )?;
        let field_pats: Vec<RecordFieldPat> = decoded(
            self.field_pats,
            buffer,
            |rec| Ok(RecordFieldPat { name: field_name(rec, buffer)?, pat: idx(rec.id) }),
            |field| ids.pat(field.pat),
        )?;
        let expr_lists: Vec<ExprId> =
            decoded(self.expr_lists, buffer, |&word| Ok(idx(word)), |&expr| ids.expr(expr))?;
        let pat_lists: Vec<PatId> =
            decoded(self.pat_lists, buffer, |&word| Ok(idx(word)), |&pat| ids.pat(pat))?;
        let type_lists: Vec<Option<TypeRefId>> =
            decoded(self.type_lists, buffer, |&word| Ok(opt_idx(word)), |&ty| ids.ty_opt(ty))?;
        let SideTables { names, paths, literals, generic_args, offset_ofs, asms } = side;
        let columns = HirColumns::<Frozen> {
            expr_kind: self.expr_kind.into(),
            expr_data: self.expr_data.into(),
            pat_kind: self.pat_kind.into(),
            pat_data: self.pat_data.into(),
            extra: self.extra.into(),
            expr_lists: expr_lists.into(),
            pat_lists: pat_lists.into(),
            statements: statements.into(),
            arms: arms.into(),
            record_fields: record_fields.into(),
            field_pats: field_pats.into(),
            type_lists: type_lists.into(),
            names: names.into(),
            paths: paths.into(),
            literals: literals.into(),
            generic_args: generic_args.into(),
            offset_ofs: offset_ofs.into(),
            asms: asms.into(),
        };
        (columns.check(bindings.len(), labels.len(), types.len()))
            .map_err(|what| FormatError::Invalid { at: at_of(self.root, buffer), what })?;

        let expr_only = match self.has_exprs() {
            false => None,
            true => Some(Box::new(ExpressionOnlyStore {
                columns,
                bindings,
                labels,
                binding_owners: self.binding_owners.into(),
                block_scopes: self.block_scopes().collect(),
                ident_hygiene: (self.ident_hygiene.iter())
                    .map(|pair| (packed_of(pair.a), hygiene_of(pair.b)))
                    .collect(),
                block_editions: (self.block_editions.iter())
                    .filter_map(|pair| Some((idx(pair.a), edition_of(pair.b)?)))
                    .collect(),
                overflow_inheriting: self.overflow_inheriting.iter().map(|&it| idx(it)).collect(),
                expr_roots: (self.expr_roots.iter())
                    .map(|root| ExprRoot {
                        root: idx(root.root),
                        exprs_end: idx(root.exprs_end),
                        pats_end: idx(root.pats_end),
                        bindings_end: idx(root.bindings_end),
                    })
                    .collect::<SmallVec<_>>(),
            })),
        };
        let type_hygiene = (self.root.flags & HAS_TYPE_HYGIENE != 0).then(|| {
            (self.type_hygiene.iter()).map(|pair| (idx(pair.a), hygiene_of(pair.b))).collect()
        });
        Ok(ExpressionStore { expr_only, types, lifetimes, type_hygiene })
    }
}

impl Persist for ExpressionStore {
    const SCHEMA: SchemaHash = schema_of(&[
        Part::Str("ExpressionStore"),
        Part::U64(3),
        Part::Schema(<StoreRoot as Record>::SCHEMA),
    ]);
    type Ref<'a> = ExpressionStoreRef<'a>;

    fn write(&self, out: &mut Writer) -> u64 {
        let columns = StoreColumns::write(self, out);
        let at = out.next::<StoreRoot>();
        let written = out.push(&columns.rec(at));
        debug_assert_eq!(written, at);
        at
    }

    fn read(buffer: &[u8]) -> Result<ExpressionStoreRef<'_>, FormatError> {
        let root = open_as::<StoreRoot>(buffer, Self::SCHEMA)?;
        ExpressionStoreRef::new(root, buffer)
    }
}

fn param_pair<Id>(param: &Param<Idx<Id>>) -> IdPair {
    IdPair { a: raw(param.formal), b: raw(param.user_written) }
}

fn param_of<Id>(pair: &IdPair) -> Param<Idx<Id>> {
    Param { formal: idx(pair.a), user_written: idx(pair.b) }
}

/// A [`Body`] read in place from its buffer.
#[derive(Clone, Copy)]
pub struct BodyRef<'a> {
    store: ExpressionStoreRef<'a>,
    params: &'a [IdPair],
    self_param: Option<&'a IdPair>,
}

impl<'a> BodyRef<'a> {
    pub fn store(self) -> ExpressionStoreRef<'a> {
        self.store
    }

    pub fn params_len(self) -> usize {
        self.params.len()
    }

    pub fn params(self) -> impl ExactSizeIterator<Item = Param<PatId>> + 'a {
        self.params.iter().map(param_of)
    }

    pub fn self_param(self) -> Option<Param<BindingId>> {
        self.self_param.map(param_of)
    }

    /// The body, back as the value that was written.
    pub fn decode(self) -> Result<Body, FormatError> {
        Ok(Body {
            store: self.store.decode()?,
            params: self.params().collect(),
            self_param: self.self_param(),
        })
    }
}

impl Persist for Body {
    const SCHEMA: SchemaHash = schema_of(&[
        Part::Str("Body"),
        Part::U64(3),
        Part::Schema(<BodyRoot as Record>::SCHEMA),
    ]);
    type Ref<'a> = BodyRef<'a>;

    fn write(&self, out: &mut Writer) -> u64 {
        let Body { store, params, self_param } = self;
        let columns = StoreColumns::write(store, out);
        let params: Vec<IdPair> = params.iter().map(param_pair).collect();
        let params = Column::push(out, &params);
        let self_param: Vec<IdPair> = self_param.iter().map(param_pair).collect();
        let self_param = Column::push(out, &self_param);
        let at = out.next::<BodyRoot>();
        let field = |offset: usize| at + offset as u64;
        let root = BodyRoot {
            store: columns.rec(field(offset_of!(BodyRoot, store))),
            params: params.slice(field(offset_of!(BodyRoot, params))),
            self_param: self_param.slice(field(offset_of!(BodyRoot, self_param))),
        };
        let written = out.push(&root);
        debug_assert_eq!(written, at);
        at
    }

    fn read(buffer: &[u8]) -> Result<BodyRef<'_>, FormatError> {
        let root = open_as::<BodyRoot>(buffer, Self::SCHEMA)?;
        let store = ExpressionStoreRef::new(&root.store, buffer)?;
        let params = root.params.follow(buffer)?;
        for param in params {
            let pats = store.pat_kind.len() as u32;
            let fits = param.a < pats && param.b < pats;
            ensure(fits, param, buffer, "a parameter past the patterns")?;
        }
        let self_params = root.self_param.follow(buffer)?;
        ensure(self_params.len() <= 1, root, buffer, "more than one `self` parameter")?;
        let self_param = self_params.first();
        if let Some(param) = self_param {
            let fits = param.a < store.bindings.len() as u32 && param.b < store.bindings.len() as u32;
            ensure(fits, param, buffer, "a `self` parameter past the bindings")?;
        }
        Ok(BodyRef { store, params, self_param })
    }
}

/// Where everything of a source map but its root was written.
struct SourceMapColumns {
    words: Placed,
    expr_map_back: Column,
    pat_map_back: Column,
    label_map_back: Column,
    types_map_back: Column,
    lifetime_map_back: Column,
    expr_map: Column,
    expr_map_ids: Column,
    pat_map: Column,
    pat_map_ids: Column,
    label_map: Column,
    label_map_ids: Column,
    types_map: Column,
    types_map_ids: Column,
    lifetime_map: Column,
    lifetime_map_ids: Column,
    expansions: Column,
    expansion_calls: Column,
    flags: u32,
}

impl SourceMapColumns {
    fn write(map: &ExpressionStoreSourceMap, out: &mut Writer) -> SourceMapColumns {
        let ExpressionStoreSourceMap {
            expr_only,
            types_map_back,
            types_map,
            lifetime_map_back,
            lifetime_map,
        } = map;
        let mut w = Words::new();
        let (types_map, types_map_ids) = table_columns(types_map, |&ty| raw(ty));
        let (lifetime_map, lifetime_map_ids) = table_columns(lifetime_map, |&lifetime| raw(lifetime));
        let (mut expr_map_back, mut pat_map_back, mut label_map_back) = (vec![], vec![], vec![]);
        let (mut expr_keys, mut expr_ids, mut pat_keys, mut pat_ids) = (vec![], vec![], vec![], vec![]);
        let (mut label_keys, mut label_ids, mut call_keys, mut calls) = (vec![], vec![], vec![], vec![]);
        if let Some(it) = expr_only.as_deref() {
            let ExpressionOnlySourceMap {
                expr_map,
                expr_map_back: exprs_back,
                pat_map,
                pat_map_back: pats_back,
                label_map,
                label_map_back: labels_back,
                binding_definitions,
                field_map_back,
                pat_field_map_back,
                template_map,
                expansions,
                diagnostics,
            } = it;
            put_arena_map(&mut w, binding_definitions);
            put_table(&mut w, field_map_back);
            put_table(&mut w, pat_field_map_back);
            put_template(&mut w, template_map.as_deref());
            w.put(diagnostics);
            expr_map_back = back_column(exprs_back);
            pat_map_back = back_column(pats_back);
            label_map_back = back_column(labels_back);
            (expr_keys, expr_ids) = table_columns(expr_map, |&id| packed_bits(id));
            (pat_keys, pat_ids) = table_columns(pat_map, |&id| packed_bits(id));
            (label_keys, label_ids) = table_columns(label_map, |&label| raw(label));
            (call_keys, calls) = table_columns(expansions, |&call| bits(call));
        }
        let words = w.place(out);
        SourceMapColumns {
            words,
            expr_map_back: Column::push(out, &expr_map_back),
            pat_map_back: Column::push(out, &pat_map_back),
            label_map_back: Column::push(out, &label_map_back),
            types_map_back: Column::push(out, &back_column(types_map_back)),
            lifetime_map_back: Column::push(out, &back_column(lifetime_map_back)),
            expr_map: Column::push(out, &expr_keys),
            expr_map_ids: Column::push(out, &expr_ids),
            pat_map: Column::push(out, &pat_keys),
            pat_map_ids: Column::push(out, &pat_ids),
            label_map: Column::push(out, &label_keys),
            label_map_ids: Column::push(out, &label_ids),
            types_map: Column::push(out, &types_map),
            types_map_ids: Column::push(out, &types_map_ids),
            lifetime_map: Column::push(out, &lifetime_map),
            lifetime_map_ids: Column::push(out, &lifetime_map_ids),
            expansions: Column::push(out, &call_keys),
            expansion_calls: Column::push(out, &calls),
            flags: if expr_only.is_some() { HAS_EXPRS } else { 0 },
        }
    }

    fn rec(&self, at: u64) -> SourceMapRoot {
        let field = |offset: usize| at + offset as u64;
        SourceMapRoot {
            expr_map_back: self.expr_map_back.slice(field(offset_of!(SourceMapRoot, expr_map_back))),
            pat_map_back: self.pat_map_back.slice(field(offset_of!(SourceMapRoot, pat_map_back))),
            label_map_back: (self.label_map_back)
                .slice(field(offset_of!(SourceMapRoot, label_map_back))),
            types_map_back: (self.types_map_back)
                .slice(field(offset_of!(SourceMapRoot, types_map_back))),
            lifetime_map_back: (self.lifetime_map_back)
                .slice(field(offset_of!(SourceMapRoot, lifetime_map_back))),
            expr_map: self.expr_map.slice(field(offset_of!(SourceMapRoot, expr_map))),
            expr_map_ids: self.expr_map_ids.slice(field(offset_of!(SourceMapRoot, expr_map_ids))),
            pat_map: self.pat_map.slice(field(offset_of!(SourceMapRoot, pat_map))),
            pat_map_ids: self.pat_map_ids.slice(field(offset_of!(SourceMapRoot, pat_map_ids))),
            label_map: self.label_map.slice(field(offset_of!(SourceMapRoot, label_map))),
            label_map_ids: (self.label_map_ids)
                .slice(field(offset_of!(SourceMapRoot, label_map_ids))),
            types_map: self.types_map.slice(field(offset_of!(SourceMapRoot, types_map))),
            types_map_ids: (self.types_map_ids)
                .slice(field(offset_of!(SourceMapRoot, types_map_ids))),
            lifetime_map: self.lifetime_map.slice(field(offset_of!(SourceMapRoot, lifetime_map))),
            lifetime_map_ids: (self.lifetime_map_ids)
                .slice(field(offset_of!(SourceMapRoot, lifetime_map_ids))),
            expansions: self.expansions.slice(field(offset_of!(SourceMapRoot, expansions))),
            expansion_calls: (self.expansion_calls)
                .slice(field(offset_of!(SourceMapRoot, expansion_calls))),
            words: self.words.rec(field(offset_of!(SourceMapRoot, words))),
            flags: self.flags,
        }
    }
}

fn put_template(w: &mut Words, template: Option<&FormatTemplate>) {
    match template {
        None => w.u32(0),
        Some(template) => {
            let FormatTemplate {
                format_args_to_captures,
                asm_to_captures,
                implicit_capture_to_source,
            } = template;
            w.u32(1);
            put_table(w, format_args_to_captures);
            put_table(w, asm_to_captures);
            put_table(w, implicit_capture_to_source);
        }
    }
}

fn get_template(r: &mut Read<'_>) -> Result<Option<Box<FormatTemplate>>, FormatError> {
    Ok(match r.tag(2, "a template map that is neither 0 nor 1")? {
        0 => None,
        _ => Some(Box::new(FormatTemplate {
            format_args_to_captures: get_table(r)?,
            asm_to_captures: get_table(r)?,
            implicit_capture_to_source: get_table(r)?,
        })),
    })
}

/// An [`ExpressionStoreSourceMap`] read in place from its buffer.
#[derive(Clone, Copy)]
pub struct ExpressionStoreSourceMapRef<'a> {
    buffer: &'a [u8],
    root: &'a SourceMapRoot,
    expr_map_back: &'a [Src],
    pat_map_back: &'a [Src],
    label_map_back: &'a [Src],
    types_map_back: &'a [Src],
    lifetime_map_back: &'a [Src],
    expr_map: &'a [Src],
    expr_map_ids: &'a [u32],
    pat_map: &'a [Src],
    pat_map_ids: &'a [u32],
    label_map: &'a [Src],
    label_map_ids: &'a [u32],
    types_map: &'a [Src],
    types_map_ids: &'a [u32],
    lifetime_map: &'a [Src],
    lifetime_map_ids: &'a [u32],
    expansions: &'a [Src],
    expansion_calls: &'a [u64],
}

impl<'a> ExpressionStoreSourceMapRef<'a> {
    /// The source map of `root`, in `buffer`, its columns checked.
    fn new(
        root: &'a SourceMapRoot,
        buffer: &'a [u8],
    ) -> Result<ExpressionStoreSourceMapRef<'a>, FormatError> {
        let map = ExpressionStoreSourceMapRef {
            buffer,
            root,
            expr_map_back: root.expr_map_back.follow(buffer)?,
            pat_map_back: root.pat_map_back.follow(buffer)?,
            label_map_back: root.label_map_back.follow(buffer)?,
            types_map_back: root.types_map_back.follow(buffer)?,
            lifetime_map_back: root.lifetime_map_back.follow(buffer)?,
            expr_map: root.expr_map.follow(buffer)?,
            expr_map_ids: root.expr_map_ids.follow(buffer)?,
            pat_map: root.pat_map.follow(buffer)?,
            pat_map_ids: root.pat_map_ids.follow(buffer)?,
            label_map: root.label_map.follow(buffer)?,
            label_map_ids: root.label_map_ids.follow(buffer)?,
            types_map: root.types_map.follow(buffer)?,
            types_map_ids: root.types_map_ids.follow(buffer)?,
            lifetime_map: root.lifetime_map.follow(buffer)?,
            lifetime_map_ids: root.lifetime_map_ids.follow(buffer)?,
            expansions: root.expansions.follow(buffer)?,
            expansion_calls: root.expansion_calls.follow(buffer)?,
        };
        ensure(root.flags & !HAS_EXPRS == 0, root, buffer, "source map flags out of range")?;
        if root.flags & HAS_EXPRS == 0 {
            let empty = map.expr_map_back.is_empty()
                && map.pat_map_back.is_empty()
                && map.label_map_back.is_empty()
                && map.expr_map.is_empty()
                && map.expr_map_ids.is_empty()
                && map.pat_map.is_empty()
                && map.pat_map_ids.is_empty()
                && map.label_map.is_empty()
                && map.label_map_ids.is_empty()
                && map.expansions.is_empty()
                && map.expansion_calls.is_empty();
            ensure(empty, root, buffer, "expression maps in a source map without expressions")?;
        }
        check_back_column::<Either<ast::Expr, ast::Pat>>(map.expr_map_back, buffer)?;
        check_back_column::<Either<ast::Expr, ast::Pat>>(map.pat_map_back, buffer)?;
        check_back_column::<Either<ast::Label, ast::BlockExpr>>(map.label_map_back, buffer)?;
        check_back_column::<ast::Type>(map.types_map_back, buffer)?;
        check_back_column::<ast::Lifetime>(map.lifetime_map_back, buffer)?;
        check_table::<ast::Expr, _>(map.expr_map, map.expr_map_ids, root, buffer)?;
        check_table::<ast::Pat, _>(map.pat_map, map.pat_map_ids, root, buffer)?;
        check_table::<Either<ast::Label, ast::BlockExpr>, _>(
            map.label_map,
            map.label_map_ids,
            root,
            buffer,
        )?;
        check_table::<ast::Type, _>(map.types_map, map.types_map_ids, root, buffer)?;
        check_table::<ast::Lifetime, _>(map.lifetime_map, map.lifetime_map_ids, root, buffer)?;
        check_table::<ast::MacroCall, _>(map.expansions, map.expansion_calls, root, buffer)?;
        for call in map.expansion_calls {
            ensure(salsa_id(*call).is_some(), call, buffer, "a macro call id of index 0")?;
        }
        Ok(map)
    }

    /// As [`ExpressionStoreSourceMap::node_expr`], by binary search over the keys in place.
    pub fn node_expr(self, src: ExprSource) -> Option<ExprOrPatId> {
        table_get(self.expr_map, self.expr_map_ids, &src).map(|bits| packed_of(bits).unpack())
    }

    pub fn node_pat(self, src: PatSource) -> Option<ExprOrPatId> {
        table_get(self.pat_map, self.pat_map_ids, &src).map(|bits| packed_of(bits).unpack())
    }

    pub fn node_label(self, src: LabelSource) -> Option<LabelId> {
        table_get(self.label_map, self.label_map_ids, &src).map(idx)
    }

    pub fn node_type(self, src: TypeSource) -> Option<TypeRefId> {
        table_get(self.types_map, self.types_map_ids, &src).map(idx)
    }

    pub fn node_lifetime(self, src: LifetimeSource) -> Option<LifetimeRefId> {
        table_get(self.lifetime_map, self.lifetime_map_ids, &src).map(idx)
    }

    /// As [`ExpressionStoreSourceMap::expansion`].
    pub fn expansion(self, src: InFile<MacroCallPtr>) -> Option<MacroCallId> {
        let call = table_get(self.expansions, self.expansion_calls, &src)?;
        salsa_id(call).map(<MacroCallId as FromId>::from_id)
    }

    pub fn has_exprs(self) -> bool {
        self.root.flags & HAS_EXPRS != 0
    }

    /// As [`ExpressionStoreSourceMap::expr_syntax`], `None` for a synthetic expression.
    pub fn expr_syntax(self, expr: ExprId) -> Option<ExprOrPatSource> {
        self.expr_map_back.get(raw(expr) as usize)?.get().ok()?
    }

    pub fn pat_syntax(self, pat: PatId) -> Option<ExprOrPatSource> {
        self.pat_map_back.get(raw(pat) as usize)?.get().ok()?
    }

    pub fn label_syntax(self, label: LabelId) -> Option<LabelSource> {
        self.label_map_back.get(raw(label) as usize)?.get().ok()?
    }

    pub fn type_syntax(self, ty: TypeRefId) -> Option<TypeSource> {
        self.types_map_back.get(raw(ty) as usize)?.get().ok()?
    }

    pub fn lifetime_syntax(self, lifetime: LifetimeRefId) -> Option<LifetimeSource> {
        self.lifetime_map_back.get(raw(lifetime) as usize)?.get().ok()?
    }

    /// The source map, back as the value that was written.
    pub fn decode(self) -> Result<ExpressionStoreSourceMap, FormatError> {
        let buffer = self.buffer;
        let mut r = Read::new(&self.root.words, buffer);
        let types_map = source_table(self.types_map, self.types_map_ids, buffer, idx)?;
        let lifetime_map = source_table(self.lifetime_map, self.lifetime_map_ids, buffer, idx)?;
        let expr_only = match self.has_exprs() {
            false => None,
            true => Some(Box::new(ExpressionOnlySourceMap {
                expr_map: source_table(self.expr_map, self.expr_map_ids, buffer, packed_of)?,
                expr_map_back: back_map(self.expr_map_back, buffer)?,
                pat_map: source_table(self.pat_map, self.pat_map_ids, buffer, packed_of)?,
                pat_map_back: back_map(self.pat_map_back, buffer)?,
                label_map: source_table(self.label_map, self.label_map_ids, buffer, idx)?,
                label_map_back: back_map(self.label_map_back, buffer)?,
                binding_definitions: get_arena_map(&mut r)?,
                field_map_back: get_table(&mut r)?,
                pat_field_map_back: get_table(&mut r)?,
                template_map: get_template(&mut r)?,
                expansions: source_table(self.expansions, self.expansion_calls, buffer, |call| {
                    <MacroCallId as FromId>::from_id(
                        salsa_id(call).expect("checked when the buffer was read"),
                    )
                })?,
                diagnostics: r.get()?,
            })),
        };
        Ok(ExpressionStoreSourceMap {
            expr_only,
            types_map_back: back_map(self.types_map_back, buffer)?,
            types_map,
            lifetime_map_back: back_map(self.lifetime_map_back, buffer)?,
            lifetime_map,
        })
    }
}

impl Persist for ExpressionStoreSourceMap {
    const SCHEMA: SchemaHash = schema_of(&[
        Part::Str("ExpressionStoreSourceMap"),
        Part::U64(2),
        Part::Schema(<SourceMapRoot as Record>::SCHEMA),
    ]);
    type Ref<'a> = ExpressionStoreSourceMapRef<'a>;

    fn write(&self, out: &mut Writer) -> u64 {
        let columns = SourceMapColumns::write(self, out);
        let at = out.next::<SourceMapRoot>();
        let written = out.push(&columns.rec(at));
        debug_assert_eq!(written, at);
        at
    }

    fn read(buffer: &[u8]) -> Result<ExpressionStoreSourceMapRef<'_>, FormatError> {
        let root = open_as::<SourceMapRoot>(buffer, Self::SCHEMA)?;
        ExpressionStoreSourceMapRef::new(root, buffer)
    }
}

/// A [`BodySourceMap`] read in place from its buffer.
#[derive(Clone, Copy)]
pub struct BodySourceMapRef<'a> {
    store: ExpressionStoreSourceMapRef<'a>,
    self_param: Option<&'a Src>,
}

impl<'a> BodySourceMapRef<'a> {
    pub fn store(self) -> ExpressionStoreSourceMapRef<'a> {
        self.store
    }

    /// As [`BodySourceMap::self_param_syntax`].
    pub fn self_param(self) -> Option<InFile<crate::expr_store::SelfParamPtr>> {
        self.self_param?.get().ok()?
    }

    /// The source map, back as the value that was written.
    pub fn decode(self) -> Result<BodySourceMap, FormatError> {
        Ok(BodySourceMap { self_param: self.self_param(), store: self.store.decode()? })
    }
}

impl Persist for BodySourceMap {
    const SCHEMA: SchemaHash = schema_of(&[
        Part::Str("BodySourceMap"),
        Part::U64(2),
        Part::Schema(<BodySourceMapRoot as Record>::SCHEMA),
    ]);
    type Ref<'a> = BodySourceMapRef<'a>;

    fn write(&self, out: &mut Writer) -> u64 {
        let BodySourceMap { self_param, store } = self;
        let columns = SourceMapColumns::write(store, out);
        let self_param: Vec<Src> = self_param.iter().map(Src::of).collect();
        let self_param = Column::push(out, &self_param);
        let at = out.next::<BodySourceMapRoot>();
        let field = |offset: usize| at + offset as u64;
        let root = BodySourceMapRoot {
            store: columns.rec(field(offset_of!(BodySourceMapRoot, store))),
            self_param: self_param.slice(field(offset_of!(BodySourceMapRoot, self_param))),
        };
        let written = out.push(&root);
        debug_assert_eq!(written, at);
        at
    }

    fn read(buffer: &[u8]) -> Result<BodySourceMapRef<'_>, FormatError> {
        let root = open_as::<BodySourceMapRoot>(buffer, Self::SCHEMA)?;
        let store = ExpressionStoreSourceMapRef::new(&root.store, buffer)?;
        let self_params = root.self_param.follow(buffer)?;
        ensure(self_params.len() <= 1, root, buffer, "more than one `self` parameter")?;
        let self_param = self_params.first();
        if let Some(src) = self_param {
            let found = src
                .get::<ast::SelfParam>()
                .map_err(|what| FormatError::Invalid { at: at_of(src, buffer), what })?;
            ensure(found.is_some(), src, buffer, "an empty `self` parameter")?;
        }
        Ok(BodySourceMapRef { store, self_param })
    }
}

fn put_binding(binding: &Binding, w: &mut Words) -> u32 {
    let Binding { name, mode, problems, hygiene } = binding;
    w.put(name);
    w.put(mode);
    w.put(problems);
    w.put(hygiene);
    0
}

fn get_binding((): (), r: &mut Read<'_>) -> Result<Binding, FormatError> {
    Ok(Binding { name: r.get()?, mode: r.get()?, problems: r.get()?, hygiene: r.get()? })
}

fn put_label(label: &Label, w: &mut Words) -> u32 {
    let Label { name } = label;
    w.put(name);
    0
}

fn get_label((): (), r: &mut Read<'_>) -> Result<Label, FormatError> {
    Ok(Label { name: r.get()? })
}

fn put_lifetime(lifetime: &LifetimeRef, w: &mut Words) -> u32 {
    let kind = match lifetime {
        LifetimeRef::Named(name) => {
            w.put(name);
            LifetimeRefKind::Named
        }
        LifetimeRef::Static => LifetimeRefKind::Static,
        LifetimeRef::Placeholder => LifetimeRefKind::Placeholder,
        LifetimeRef::Param(param) => {
            w.put(param);
            LifetimeRefKind::Param
        }
        LifetimeRef::Error => LifetimeRefKind::Error,
    };
    kind as u32
}

fn get_lifetime(kind: LifetimeRefKind, r: &mut Read<'_>) -> Result<LifetimeRef, FormatError> {
    Ok(match kind {
        LifetimeRefKind::Named => LifetimeRef::Named(r.get()?),
        LifetimeRefKind::Static => LifetimeRef::Static,
        LifetimeRefKind::Placeholder => LifetimeRef::Placeholder,
        LifetimeRefKind::Param => LifetimeRef::Param(r.get()?),
        LifetimeRefKind::Error => LifetimeRef::Error,
    })
}

/// `Code` for enums without fields: each variant its number.
macro_rules! unit_codes {
    ($($ty:ty { $($tag:literal => $variant:path),* $(,)? })*) => {$(
        impl Code for $ty {
            fn put(&self, w: &mut Words) {
                w.u32(match self { $($variant => $tag,)* });
            }

            fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
                Ok(match r.u32()? {
                    $($tag => $variant,)*
                    _ => return Err(r.invalid(concat!("a ", stringify!($ty), " out of range"))),
                })
            }
        }
    )*};
}

unit_codes! {
    Mutability { 0 => Mutability::Shared, 1 => Mutability::Mut }
    BindingAnnotation {
        0 => BindingAnnotation::Unannotated,
        1 => BindingAnnotation::Mutable,
        2 => BindingAnnotation::Ref,
        3 => BindingAnnotation::RefMut,
    }
    BindingProblems {
        0 => BindingProblems::BoundMoreThanOnce,
        1 => BindingProblems::BoundInconsistently,
        2 => BindingProblems::NotBoundAcrossAll,
    }
    InlineAsmKind {
        0 => InlineAsmKind::Asm, 1 => InlineAsmKind::GlobalAsm, 2 => InlineAsmKind::NakedAsm,
    }
    TraitBoundModifier {
        0 => TraitBoundModifier::None,
        1 => TraitBoundModifier::Maybe,
        2 => TraitBoundModifier::Const,
        3 => TraitBoundModifier::MaybeConst,
    }
    GenericArgsParentheses {
        0 => GenericArgsParentheses::No,
        1 => GenericArgsParentheses::ReturnTypeNotation,
        2 => GenericArgsParentheses::ParenSugar,
    }
    BuiltinInt {
        0 => BuiltinInt::Isize,
        1 => BuiltinInt::I8,
        2 => BuiltinInt::I16,
        3 => BuiltinInt::I32,
        4 => BuiltinInt::I64,
        5 => BuiltinInt::I128,
    }
    BuiltinUint {
        0 => BuiltinUint::Usize,
        1 => BuiltinUint::U8,
        2 => BuiltinUint::U16,
        3 => BuiltinUint::U32,
        4 => BuiltinUint::U64,
        5 => BuiltinUint::U128,
    }
    BuiltinFloat {
        0 => BuiltinFloat::F16, 1 => BuiltinFloat::F32, 2 => BuiltinFloat::F64, 3 => BuiltinFloat::F128,
    }
    ProcMacroKind {
        0 => ProcMacroKind::CustomDerive, 1 => ProcMacroKind::Bang, 2 => ProcMacroKind::Attr,
    }
    MissingBodyItemKind {
        0 => MissingBodyItemKind::AssocConst,
        1 => MissingBodyItemKind::AssocType,
        2 => MissingBodyItemKind::Const,
        3 => MissingBodyItemKind::Static,
        4 => MissingBodyItemKind::TypeAlias,
    }
}

/// `Code` for enums each variant of which holds one value: its number, then the value.
macro_rules! sum_codes {
    ($($ty:ident { $($tag:literal => $variant:ident),* $(,)? })*) => {$(
        impl Code for $ty {
            fn put(&self, w: &mut Words) {
                match self {
                    $($ty::$variant(it) => {
                        w.u32($tag);
                        it.put(w);
                    })*
                }
            }

            fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
                Ok(match r.u32()? {
                    $($tag => $ty::$variant(r.get()?),)*
                    _ => return Err(r.invalid(concat!("a ", stringify!($ty), " out of range"))),
                })
            }
        }
    )*};
}

crate::hir::persist::salsa_ids!(
    FunctionId,
    StructId,
    UnionId,
    EnumId,
    ConstId,
    StaticId,
    TraitId,
    TypeAliasId,
    ImplId,
    EnumVariantId,
    Macro2Id,
    MacroRulesId,
    ProcMacroId,
);

sum_codes! {
    AdtId { 0 => StructId, 1 => UnionId, 2 => EnumId }
    GenericDefId {
        0 => AdtId,
        1 => ConstId,
        2 => FunctionId,
        3 => ImplId,
        4 => StaticId,
        5 => TraitId,
        6 => TypeAliasId,
    }
    MacroId { 0 => Macro2Id, 1 => MacroRulesId, 2 => ProcMacroId }
    LangItemTarget {
        0 => EnumId,
        1 => FunctionId,
        2 => ImplId,
        3 => StaticId,
        4 => StructId,
        5 => UnionId,
        6 => TypeAliasId,
        7 => TraitId,
        8 => EnumVariantId,
        9 => ConstId,
        10 => MacroId,
    }
    GenericArg { 0 => Type, 1 => Lifetime, 2 => Const }
    UseArgRef { 0 => Name, 1 => Lifetime }
    InlineAsmRegOrRegClass { 0 => Reg, 1 => RegClass }
}

impl Code for TypeParamId {
    fn put(&self, w: &mut Words) {
        self.parent().put(w);
        self.local_id().put(w);
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        let parent = r.get()?;
        Ok(TypeParamId::from_unchecked(TypeOrConstParamId { parent, local_id: r.get()? }))
    }
}

impl Code for LifetimeParamId {
    fn put(&self, w: &mut Words) {
        let LifetimeParamId { parent, local_id } = self;
        parent.put(w);
        local_id.put(w);
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        Ok(LifetimeParamId { parent: r.get()?, local_id: r.get()? })
    }
}

impl Code for PathId {
    fn put(&self, w: &mut Words) {
        self.type_ref().put(w);
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        r.get().map(PathId::from_type_ref_unchecked)
    }
}

impl Code for ConstRef {
    fn put(&self, w: &mut Words) {
        self.expr.put(w);
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        Ok(ConstRef { expr: r.get()? })
    }
}

impl Code for Path {
    fn put(&self, w: &mut Words) {
        match self {
            Path::BarePath(path) => {
                w.u32(0);
                path.put(w);
            }
            Path::Normal(path) => {
                w.u32(1);
                path.put(w);
            }
            Path::LangItem(target, name) => {
                w.u32(2);
                target.put(w);
                name.put(w);
            }
        }
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        Ok(match r.tag(3, "a path kind out of range")? {
            0 => Path::BarePath(r.get()?),
            1 => Path::Normal(r.get()?),
            _ => {
                let target = r.get()?;
                Path::LangItem(target, r.get()?)
            }
        })
    }
}

impl Code for NormalPath {
    fn put(&self, w: &mut Words) {
        let NormalPath { generic_args, type_anchor, mod_path } = self;
        generic_args.put(w);
        type_anchor.put(w);
        mod_path.put(w);
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        Ok(NormalPath { generic_args: r.get()?, type_anchor: r.get()?, mod_path: r.get()? })
    }
}

impl Code for GenericArgs {
    fn put(&self, w: &mut Words) {
        let GenericArgs { args, has_self_type, bindings, parenthesized } = self;
        args.put(w);
        has_self_type.put(w);
        bindings.put(w);
        parenthesized.put(w);
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        Ok(GenericArgs {
            args: r.get()?,
            has_self_type: r.get()?,
            bindings: r.get()?,
            parenthesized: r.get()?,
        })
    }
}

impl Code for AssociatedTypeBinding {
    fn put(&self, w: &mut Words) {
        let AssociatedTypeBinding { name, args, type_ref, bounds } = self;
        name.put(w);
        args.put(w);
        type_ref.put(w);
        bounds.put(w);
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        Ok(AssociatedTypeBinding {
            name: r.get()?,
            args: r.get()?,
            type_ref: r.get()?,
            bounds: r.get()?,
        })
    }
}

impl Code for TypeBound {
    fn put(&self, w: &mut Words) {
        match self {
            TypeBound::Path(path, modifier) => {
                w.u32(0);
                path.put(w);
                modifier.put(w);
            }
            TypeBound::ForLifetime(names, path) => {
                w.u32(1);
                names.put(w);
                path.put(w);
            }
            TypeBound::Lifetime(lifetime) => {
                w.u32(2);
                lifetime.put(w);
            }
            TypeBound::Use(args) => {
                w.u32(3);
                args.put(w);
            }
            TypeBound::Error => w.u32(4),
        }
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        Ok(match r.tag(5, "a type bound out of range")? {
            0 => {
                let path = r.get()?;
                TypeBound::Path(path, r.get()?)
            }
            1 => {
                let names = r.get()?;
                TypeBound::ForLifetime(names, r.get()?)
            }
            2 => TypeBound::Lifetime(r.get()?),
            3 => TypeBound::Use(r.get()?),
            _ => TypeBound::Error,
        })
    }
}

impl Code for RefType {
    fn put(&self, w: &mut Words) {
        let RefType { ty, lifetime, mutability } = self;
        ty.put(w);
        lifetime.put(w);
        mutability.put(w);
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        Ok(RefType { ty: r.get()?, lifetime: r.get()?, mutability: r.get()? })
    }
}

impl Code for ArrayType {
    fn put(&self, w: &mut Words) {
        let ArrayType { ty, len } = self;
        ty.put(w);
        len.put(w);
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        Ok(ArrayType { ty: r.get()?, len: r.get()? })
    }
}

impl Code for ExternAbi {
    fn put(&self, w: &mut Words) {
        w.str(self.as_str());
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        let name = r.str()?;
        name.parse().map_err(|_| r.invalid("an ABI of no known name"))
    }
}

impl Code for FnType {
    fn put(&self, w: &mut Words) {
        let FnType { binder, params, is_varargs, is_unsafe, abi } = self;
        binder.put(w);
        params.put(w);
        is_varargs.put(w);
        is_unsafe.put(w);
        abi.put(w);
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        Ok(FnType {
            binder: r.get()?,
            params: r.get()?,
            is_varargs: r.get()?,
            is_unsafe: r.get()?,
            abi: r.get()?,
        })
    }
}

impl Code for FloatTypeWrapper {
    fn put(&self, w: &mut Words) {
        w.str(&self.to_string());
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        r.symbol().map(FloatTypeWrapper::new)
    }
}

impl Code for Literal {
    fn put(&self, w: &mut Words) {
        match self {
            Literal::String(text) => {
                w.u32(0);
                w.symbol(text);
            }
            Literal::ByteString(bytes) => {
                w.u32(1);
                put_bytes(w, bytes);
            }
            Literal::CString(bytes) => {
                w.u32(2);
                put_bytes(w, bytes);
            }
            Literal::Char(char) => {
                w.u32(3);
                char.put(w);
            }
            Literal::Bool(bool) => {
                w.u32(4);
                bool.put(w);
            }
            Literal::Int(value, ty) => {
                w.u32(5);
                value.put(w);
                ty.put(w);
            }
            Literal::Uint(value, ty) => {
                w.u32(6);
                value.put(w);
                ty.put(w);
            }
            Literal::Float(value, ty) => {
                w.u32(7);
                value.put(w);
                ty.put(w);
            }
        }
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        Ok(match r.tag(8, "a literal kind out of range")? {
            0 => Literal::String(r.symbol()?),
            1 => Literal::ByteString(get_bytes(r)?),
            2 => Literal::CString(get_bytes(r)?),
            3 => Literal::Char(r.get()?),
            4 => Literal::Bool(r.get()?),
            5 => {
                let value = r.get()?;
                Literal::Int(value, r.get()?)
            }
            6 => {
                let value = r.get()?;
                Literal::Uint(value, r.get()?)
            }
            _ => {
                let value = r.get()?;
                Literal::Float(value, r.get()?)
            }
        })
    }
}

impl Code for OffsetOf {
    fn put(&self, w: &mut Words) {
        let OffsetOf { container, fields } = self;
        container.put(w);
        fields.put(w);
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        Ok(OffsetOf { container: r.get()?, fields: r.get()? })
    }
}

impl Code for AsmOptions {
    fn put(&self, w: &mut Words) {
        w.u32(u32::from(self.bits()));
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        let bits: u16 = r.get()?;
        AsmOptions::from_bits(bits).ok_or_else(|| r.invalid("asm options of unknown bits"))
    }
}

impl Code for AsmPiece {
    fn put(&self, w: &mut Words) {
        match self {
            AsmPiece::Lit(text) => {
                w.u32(0);
                text.put(w);
            }
            AsmPiece::Operand { index, modifier } => {
                w.u32(1);
                index.put(w);
                modifier.put(w);
            }
        }
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        Ok(match r.tag(2, "an asm piece out of range")? {
            0 => AsmPiece::Lit(r.get()?),
            _ => AsmPiece::Operand { index: r.get()?, modifier: r.get()? },
        })
    }
}

impl Code for AsmOperand {
    fn put(&self, w: &mut Words) {
        match self {
            AsmOperand::In { reg, expr } => {
                w.u32(0);
                reg.put(w);
                expr.put(w);
            }
            AsmOperand::Out { reg, expr, late } => {
                w.u32(1);
                reg.put(w);
                expr.put(w);
                late.put(w);
            }
            AsmOperand::InOut { reg, expr, late } => {
                w.u32(2);
                reg.put(w);
                expr.put(w);
                late.put(w);
            }
            AsmOperand::SplitInOut { reg, in_expr, out_expr, late } => {
                w.u32(3);
                reg.put(w);
                in_expr.put(w);
                out_expr.put(w);
                late.put(w);
            }
            AsmOperand::Label(expr) => {
                w.u32(4);
                expr.put(w);
            }
            AsmOperand::Const(expr) => {
                w.u32(5);
                expr.put(w);
            }
            AsmOperand::Sym(path) => {
                w.u32(6);
                path.put(w);
            }
        }
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        Ok(match r.tag(7, "an asm operand out of range")? {
            0 => AsmOperand::In { reg: r.get()?, expr: r.get()? },
            1 => AsmOperand::Out { reg: r.get()?, expr: r.get()?, late: r.get()? },
            2 => AsmOperand::InOut { reg: r.get()?, expr: r.get()?, late: r.get()? },
            3 => AsmOperand::SplitInOut {
                reg: r.get()?,
                in_expr: r.get()?,
                out_expr: r.get()?,
                late: r.get()?,
            },
            4 => AsmOperand::Label(r.get()?),
            5 => AsmOperand::Const(r.get()?),
            _ => AsmOperand::Sym(r.get()?),
        })
    }
}

impl Code for InlineAsm {
    fn put(&self, w: &mut Words) {
        let InlineAsm { operands, template, options, kind, comment_only, clobber_abi } = self;
        operands.put(w);
        template.put(w);
        options.put(w);
        kind.put(w);
        comment_only.put(w);
        clobber_abi.put(w);
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        Ok(InlineAsm {
            operands: r.get()?,
            template: r.get()?,
            options: r.get()?,
            kind: r.get()?,
            comment_only: r.get()?,
            clobber_abi: r.get()?,
        })
    }
}

/// `Code` for the expanders a builtin macro names: their place among their type's `ALL`, which
/// lists them in declaration order.
macro_rules! listed_codes {
    ($($ty:ty),* $(,)?) => {$(
        impl Code for $ty {
            fn put(&self, w: &mut Words) {
                w.u32(*self as u32);
            }

            fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
                let index = r.u32()? as usize;
                (<$ty>::ALL.get(index).copied())
                    .ok_or_else(|| r.invalid(concat!("a ", stringify!($ty), " out of range")))
            }
        }
    )*};
}

listed_codes!(BuiltinFnLikeExpander, BuiltinAttrExpander, BuiltinDeriveExpander, EagerExpander);

impl Code for MacroCallStyles {
    fn put(&self, w: &mut Words) {
        w.u32(u32::from(self.bits()));
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        let bits: u8 = r.get()?;
        MacroCallStyles::from_bits(bits).ok_or_else(|| r.invalid("macro call styles of unknown bits"))
    }
}

impl Code for CustomProcMacroExpander {
    fn put(&self, w: &mut Words) {
        match self.listed() {
            Some(id) => {
                w.u32(0);
                w.u32(id);
            }
            None if self.is_missing() => w.u32(1),
            None if self.is_disabled() => w.u32(2),
            None => {
                debug_assert!(self.is_disabled_proc_attr());
                w.u32(3);
            }
        }
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        Ok(match r.tag(4, "a proc macro expander out of range")? {
            0 => {
                let id = r.u32()?;
                if id >= u32::MAX - 2 {
                    return Err(r.invalid("a proc macro id of a reserved value"));
                }
                CustomProcMacroExpander::new(id)
            }
            1 => CustomProcMacroExpander::missing_expander(),
            2 => CustomProcMacroExpander::disabled(),
            _ => CustomProcMacroExpander::disabled_proc_attr(),
        })
    }
}

impl Code for MacroDefKind {
    fn put(&self, w: &mut Words) {
        match self {
            MacroDefKind::Declarative(id, styles) => {
                w.u32(0);
                id.put(w);
                styles.put(w);
            }
            MacroDefKind::BuiltIn(id, expander) => {
                w.u32(1);
                id.put(w);
                expander.put(w);
            }
            MacroDefKind::BuiltInAttr(id, expander) => {
                w.u32(2);
                id.put(w);
                expander.put(w);
            }
            MacroDefKind::BuiltInDerive(id, expander) => {
                w.u32(3);
                id.put(w);
                expander.put(w);
            }
            MacroDefKind::BuiltInEager(id, expander) => {
                w.u32(4);
                id.put(w);
                expander.put(w);
            }
            MacroDefKind::UnimplementedBuiltIn(id) => {
                w.u32(5);
                id.put(w);
            }
            MacroDefKind::ProcMacro(id, expander, kind) => {
                w.u32(6);
                id.put(w);
                expander.put(w);
                kind.put(w);
            }
        }
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        Ok(match r.tag(7, "a macro kind out of range")? {
            0 => {
                let id = r.get()?;
                MacroDefKind::Declarative(id, r.get()?)
            }
            1 => {
                let id = r.get()?;
                MacroDefKind::BuiltIn(id, r.get()?)
            }
            2 => {
                let id = r.get()?;
                MacroDefKind::BuiltInAttr(id, r.get()?)
            }
            3 => {
                let id = r.get()?;
                MacroDefKind::BuiltInDerive(id, r.get()?)
            }
            4 => {
                let id = r.get()?;
                MacroDefKind::BuiltInEager(id, r.get()?)
            }
            5 => MacroDefKind::UnimplementedBuiltIn(r.get()?),
            _ => {
                let id = r.get()?;
                let expander = r.get()?;
                MacroDefKind::ProcMacro(id, expander, r.get()?)
            }
        })
    }
}

impl Code for MacroDefId {
    fn put(&self, w: &mut Words) {
        let MacroDefId { krate, edition, kind, local_inner, allow_internal_unsafe, edition_panic } =
            self;
        krate.put(w);
        edition.put(w);
        kind.put(w);
        local_inner.put(w);
        allow_internal_unsafe.put(w);
        edition_panic.put(w);
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        Ok(MacroDefId {
            krate: r.get()?,
            edition: r.get()?,
            kind: r.get()?,
            local_inner: r.get()?,
            allow_internal_unsafe: r.get()?,
            edition_panic: r.get()?,
        })
    }
}

impl Code for SyntaxNodePtr {
    fn put(&self, w: &mut Words) {
        w.u32(u32::from(u16::from(self.kind())));
        self.text_range().put(w);
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        let kind = r.u32()?;
        let kind = syntax_kind(kind).ok_or_else(|| r.invalid("a syntax kind out of range"))?;
        Ok(SyntaxNodePtr::from_parts(kind, r.get()?))
    }
}

impl<N: AstNode> Code for AstPtr<N> {
    fn put(&self, w: &mut Words) {
        self.syntax_node_ptr().put(w);
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        let raw = r.get()?;
        AstPtr::try_from_raw(raw)
            .ok_or_else(|| r.invalid("a syntax pointer of a kind its type does not cast"))
    }
}

impl Code for CfgOptions {
    fn put(&self, w: &mut Words) {
        let mut atoms: Vec<&CfgAtom> = self.into_iter().collect();
        atoms.sort_unstable();
        w.seq(atoms.into_iter());
    }

    /// Rebuilt from the default, which holds `true`, through the atoms' public API, which
    /// refuses `true` and `false` but makes the same set as the one written.
    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        let atoms: Vec<CfgAtom> = r.seq::<CfgAtom, _>()?;
        if atoms.windows(2).any(|pair| pair[0] >= pair[1]) {
            return Err(r.invalid("cfg options out of order"));
        }
        let mut has_true = false;
        let mut rest = Vec::with_capacity(atoms.len());
        for atom in atoms {
            match &atom {
                CfgAtom::Flag(flag) if *flag == sym::true_ => has_true = true,
                CfgAtom::Flag(key) | CfgAtom::KeyValue { key, .. }
                    if *key == sym::true_ || *key == sym::false_ =>
                {
                    return Err(r.invalid("a cfg option of `true` or `false` that is not `true`"));
                }
                _ => rest.push(atom),
            }
        }
        if !has_true {
            return Err(r.invalid("cfg options without `true`"));
        }
        let mut options = CfgOptions::default();
        options.extend(rest);
        Ok(options)
    }
}

impl Code for ExpressionStoreDiagnostics {
    fn put(&self, w: &mut Words) {
        use ExpressionStoreDiagnostics::*;
        match self {
            InactiveCode { node, cfg, opts } => {
                w.u32(0);
                node.put(w);
                cfg.put(w);
                opts.put(w);
            }
            UnresolvedMacroCall { node, path } => {
                w.u32(1);
                node.put(w);
                path.put(w);
            }
            UnreachableLabel { node, name } => {
                w.u32(2);
                node.put(w);
                name.put(w);
            }
            AwaitOutsideOfAsync { node, location } => {
                w.u32(3);
                node.put(w);
                location.put(w);
            }
            UndeclaredLabel { node, name } => {
                w.u32(4);
                node.put(w);
                name.put(w);
            }
            PatternArgInExternFn { node } => {
                w.u32(5);
                node.put(w);
            }
            FruInDestructuringAssignment { node } => {
                w.u32(6);
                node.put(w);
            }
            MissingBody { node, kind } => {
                w.u32(7);
                node.put(w);
                kind.put(w);
            }
        }
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        use ExpressionStoreDiagnostics::*;
        Ok(match r.tag(8, "a diagnostic kind out of range")? {
            0 => InactiveCode { node: r.get()?, cfg: r.get()?, opts: r.get()? },
            1 => UnresolvedMacroCall { node: r.get()?, path: r.get()? },
            2 => UnreachableLabel { node: r.get()?, name: r.get()? },
            3 => AwaitOutsideOfAsync { node: r.get()?, location: r.get()? },
            4 => UndeclaredLabel { node: r.get()?, name: r.get()? },
            5 => PatternArgInExternFn { node: r.get()? },
            6 => FruInDestructuringAssignment { node: r.get()? },
            _ => MissingBody { node: r.get()?, kind: r.get()? },
        })
    }
}

/// Writes `body` and `source_map` each into a buffer, reads them back in place and decoded,
/// and checks both are what was written.
#[cfg(test)]
pub(crate) fn assert_round_trip(body: &Body, source_map: &BodySourceMap) {
    let bytes = blaze_format::to_bytes(body);
    let buffer = blaze_format::Aligned::copy_from(&bytes);
    let read = Body::read(&buffer).expect("a body just written");
    tests::assert_reads(read.store(), &body.store);
    assert_eq!(read.params_len(), body.params.len());
    assert_eq!(read.params().collect::<Vec<_>>(), body.params.to_vec());
    assert_eq!(read.self_param(), body.self_param);
    let decoded = read.decode().expect("a body just written");
    assert_eq!(decoded, *body);
    assert_eq!(blaze_format::to_bytes(&decoded), bytes, "a body written again as other bytes");

    let bytes = blaze_format::to_bytes(source_map);
    let buffer = blaze_format::Aligned::copy_from(&bytes);
    let read = BodySourceMap::read(&buffer).expect("a source map just written");
    tests::assert_source_map_reads(read.store(), &source_map.store);
    assert_eq!(read.self_param(), source_map.self_param);
    let decoded = read.decode().expect("a source map just written");
    tests::assert_source_maps_eq(&decoded.store, &source_map.store);
    assert_eq!(decoded, *source_map);
    assert_eq!(blaze_format::to_bytes(&decoded), bytes, "a source map written again as other bytes");
}

/// [`assert_round_trip`] for a store without a body, as a signature's.
#[cfg(test)]
pub(crate) fn assert_store_round_trip(store: &ExpressionStore, source_map: &ExpressionStoreSourceMap) {
    let bytes = blaze_format::to_bytes(store);
    let buffer = blaze_format::Aligned::copy_from(&bytes);
    let read = ExpressionStore::read(&buffer).expect("a store just written");
    tests::assert_reads(read, store);
    let decoded = read.decode().expect("a store just written");
    assert_eq!(decoded, *store);
    assert_eq!(blaze_format::to_bytes(&decoded), bytes, "a store written again as other bytes");

    let bytes = blaze_format::to_bytes(source_map);
    let buffer = blaze_format::Aligned::copy_from(&bytes);
    let read = ExpressionStoreSourceMap::read(&buffer).expect("a source map just written");
    tests::assert_source_map_reads(read, source_map);
    let decoded = read.decode().expect("a source map just written");
    tests::assert_source_maps_eq(&decoded, source_map);
    assert_eq!(decoded, *source_map);
    assert_eq!(blaze_format::to_bytes(&decoded), bytes, "a source map written again as other bytes");
}

#[cfg(test)]
mod tests {
    use blaze_format::Aligned;
    use rustc_hash::FxHashSet;
    use test_fixture::WithFixture;

    use super::*;
    use crate::{
        DefWithBodyId, ModuleDefId,
        hir::{Expr, Pat},
        nameres::crate_def_map,
        signatures::FunctionSignature,
        test_db::TestDB,
        type_ref::TypeRef,
    };

    /// Which kind `expr` is, from the view, independently of the columns.
    fn expr_kind_of(expr: Expr<'_>) -> ExprKind {
        match expr {
            Expr::Missing => ExprKind::Missing,
            Expr::Path(_) => ExprKind::Path,
            Expr::If { .. } => ExprKind::If,
            Expr::Let { .. } => ExprKind::Let,
            Expr::Block { .. } => ExprKind::Block,
            Expr::Const(_) => ExprKind::Const,
            Expr::Loop { .. } => ExprKind::Loop,
            Expr::Call { .. } => ExprKind::Call,
            Expr::MethodCall { .. } => ExprKind::MethodCall,
            Expr::Match { .. } => ExprKind::Match,
            Expr::Continue { .. } => ExprKind::Continue,
            Expr::Break { .. } => ExprKind::Break,
            Expr::Return { .. } => ExprKind::Return,
            Expr::Become { .. } => ExprKind::Become,
            Expr::Yield { .. } => ExprKind::Yield,
            Expr::Yeet { .. } => ExprKind::Yeet,
            Expr::RecordLit { .. } => ExprKind::RecordLit,
            Expr::Field { .. } => ExprKind::Field,
            Expr::Await { .. } => ExprKind::Await,
            Expr::Cast { .. } => ExprKind::Cast,
            Expr::Ref { .. } => ExprKind::Ref,
            Expr::UnaryOp { .. } => ExprKind::UnaryOp,
            Expr::BinaryOp { .. } => ExprKind::BinaryOp,
            Expr::Assignment { .. } => ExprKind::Assignment,
            Expr::Index { .. } => ExprKind::Index,
            Expr::Closure { .. } => ExprKind::Closure,
            Expr::Tuple { .. } => ExprKind::Tuple,
            Expr::Array(_) => ExprKind::Array,
            Expr::Literal(_) => ExprKind::Literal,
            Expr::Underscore => ExprKind::Underscore,
            Expr::OffsetOf(_) => ExprKind::OffsetOf,
            Expr::InlineAsm(_) => ExprKind::InlineAsm,
            Expr::IncludeBytes => ExprKind::IncludeBytes,
        }
    }

    fn pat_kind_of(pat: Pat<'_>) -> PatKind {
        match pat {
            Pat::Missing => PatKind::Missing,
            Pat::Rest => PatKind::Rest,
            Pat::Wild => PatKind::Wild,
            Pat::Tuple { .. } => PatKind::Tuple,
            Pat::Or(_) => PatKind::Or,
            Pat::Record { .. } => PatKind::Record,
            Pat::Range { .. } => PatKind::Range,
            Pat::Slice { .. } => PatKind::Slice,
            Pat::Path(_) => PatKind::Path,
            Pat::Lit(_) => PatKind::Lit,
            Pat::Bind { .. } => PatKind::Bind,
            Pat::TupleStruct { .. } => PatKind::TupleStruct,
            Pat::Ref { .. } => PatKind::Ref,
            Pat::Box { .. } => PatKind::Box,
            Pat::Deref { .. } => PatKind::Deref,
            Pat::NotNull => PatKind::NotNull,
            Pat::Expr(_) => PatKind::Expr,
        }
    }

    fn type_kind_of(ty: TypeRef<'_>) -> TypeRefKind {
        match ty {
            TypeRef::Never => TypeRefKind::Never,
            TypeRef::Placeholder => TypeRefKind::Placeholder,
            TypeRef::Tuple(_) => TypeRefKind::Tuple,
            TypeRef::Path(_) => TypeRefKind::Path,
            TypeRef::RawPtr(..) => TypeRefKind::RawPtr,
            TypeRef::Reference(_) => TypeRefKind::Reference,
            TypeRef::Array(_) => TypeRefKind::Array,
            TypeRef::Slice(_) => TypeRefKind::Slice,
            TypeRef::Fn(_) => TypeRefKind::Fn,
            TypeRef::ImplTrait(_) => TypeRefKind::ImplTrait,
            TypeRef::DynTrait(_) => TypeRefKind::DynTrait,
            TypeRef::TypeParam(_) => TypeRefKind::TypeParam,
            TypeRef::PatternType(..) => TypeRefKind::PatternType,
            TypeRef::Error => TypeRefKind::Error,
        }
    }

    /// Checks what `read` reads in place against `store`.
    pub(super) fn assert_reads(read: ExpressionStoreRef<'_>, store: &ExpressionStore) {
        assert_eq!(read.has_exprs(), store.expr_only.is_some());
        assert_eq!(read.exprs_len(), store.exprs().count());
        assert_eq!(read.pats_len(), store.pats().count());
        assert_eq!(read.bindings_len(), store.bindings().count());
        assert_eq!(read.labels_len(), store.expr_only.as_ref().map_or(0, |it| it.labels.len()));
        assert_eq!(read.types_len(), store.types.len());
        assert_eq!(read.lifetimes_len(), store.lifetimes.len());
        for (id, expr) in store.exprs() {
            assert_eq!(read.expr_kind(id), Some(expr_kind_of(expr)), "{expr:?}");
            assert_eq!(read.inherits_overflow_checks(id), store.inherits_overflow_checks(id));
            assert_eq!(read.block_edition(id), store.block_edition(id));
        }
        for (id, pat) in store.pats() {
            assert_eq!(read.pat_kind(id), Some(pat_kind_of(pat)), "{pat:?}");
        }
        for (id, ty) in store.type_refs() {
            assert_eq!(read.type_kind(id), Some(type_kind_of(ty)), "{ty:?}");
        }
        for (id, lifetime) in store.lifetimes.iter() {
            let kind = LifetimeRefKind::from_u32(put_lifetime(lifetime, &mut Words::new()));
            assert_eq!(read.lifetime_kind(id), kind);
        }
        for (id, _) in store.bindings() {
            assert_eq!(read.binding_owner(id), store.binding_owner(id));
        }
        assert!(read.expr_roots().eq(store.expr_roots()));
        let blocks = store.expr_only.as_ref().map_or(&[][..], |it| &it.block_scopes);
        assert!(read.block_scopes().eq(blocks.iter().copied()));
    }

    /// Checks what `read` reads in place against `map`.
    pub(super) fn assert_source_map_reads(
        read: ExpressionStoreSourceMapRef<'_>,
        map: &ExpressionStoreSourceMap,
    ) {
        assert_eq!(read.has_exprs(), map.expr_only.is_some());
        if let Some(it) = map.expr_only.as_deref() {
            for (id, src) in it.expr_map_back.iter() {
                assert_eq!(read.expr_syntax(id), Some(*src));
            }
            for (id, src) in it.pat_map_back.iter() {
                assert_eq!(read.pat_syntax(id), Some(*src));
            }
            for (id, src) in it.label_map_back.iter() {
                assert_eq!(read.label_syntax(id), Some(*src));
            }
            for &(src, id) in it.expr_map.entries() {
                assert_eq!(read.node_expr(src), Some(id.unpack()));
            }
            for &(src, id) in it.pat_map.entries() {
                assert_eq!(read.node_pat(src), Some(id.unpack()));
            }
            for &(src, label) in it.label_map.entries() {
                assert_eq!(read.node_label(src), Some(label));
            }
            for &(src, call) in it.expansions.entries() {
                assert_eq!(read.expansion(src), Some(call));
            }
        }
        for (id, src) in map.types_map_back.iter() {
            assert_eq!(read.type_syntax(id), Some(*src));
        }
        for (id, src) in map.lifetime_map_back.iter() {
            assert_eq!(read.lifetime_syntax(id), Some(*src));
        }
        for &(src, ty) in map.types_map.entries() {
            assert_eq!(read.node_type(src), Some(ty));
        }
        for &(src, lifetime) in map.lifetime_map.entries() {
            assert_eq!(read.node_lifetime(src), Some(lifetime));
        }
    }

    /// Every field of two source maps equal, the ones their `PartialEq` skips too.
    pub(super) fn assert_source_maps_eq(a: &ExpressionStoreSourceMap, b: &ExpressionStoreSourceMap) {
        assert_eq!(a.types_map_back, b.types_map_back);
        assert_eq!(a.types_map, b.types_map);
        assert_eq!(a.lifetime_map_back, b.lifetime_map_back);
        assert_eq!(a.lifetime_map, b.lifetime_map);
        assert_eq!(a.expr_only.is_some(), b.expr_only.is_some());
        if let (Some(a), Some(b)) = (a.expr_only.as_deref(), b.expr_only.as_deref()) {
            assert_eq!(a.expr_map, b.expr_map);
            assert_eq!(a.expr_map_back, b.expr_map_back);
            assert_eq!(a.pat_map, b.pat_map);
            assert_eq!(a.pat_map_back, b.pat_map_back);
            assert_eq!(a.label_map, b.label_map);
            assert_eq!(a.label_map_back, b.label_map_back);
            assert_eq!(a.binding_definitions, b.binding_definitions);
            assert_eq!(a.field_map_back, b.field_map_back);
            assert_eq!(a.pat_field_map_back, b.pat_field_map_back);
            assert_eq!(a.template_map, b.template_map);
            assert_eq!(a.expansions, b.expansions);
            assert_eq!(a.diagnostics, b.diagnostics);
        }
    }

    /// The functions of the fixture's crate root, each body lowered and round-tripped.
    fn bodies(#[rust_analyzer::rust_fixture] ra_fixture: &str) -> (TestDB, Vec<DefWithBodyId>) {
        let db = TestDB::with_files(ra_fixture);
        let krate = db.fetch_test_crate();
        let def_map = crate_def_map(&db, krate);
        let mut defs: Vec<DefWithBodyId> = vec![];
        for (_, module) in def_map.modules(&db) {
            for decl in module.scope(&db).declarations() {
                if let ModuleDefId::FunctionId(it) = decl {
                    defs.push(it.into());
                }
            }
        }
        for &def in &defs {
            let (body, source_map) = Body::with_source_map(&db, def);
            assert_round_trip(body, source_map);
            if let Some((body, source_map)) = Body::relative(&db, def) {
                assert_round_trip(body, source_map);
            }
            if let DefWithBodyId::FunctionId(function) = def {
                let (signature, source_map) = FunctionSignature::with_source_map(&db, function);
                assert_store_round_trip(&signature.store, source_map);
            }
        }
        (db, defs)
    }

    const EVERY_KIND: &str = r#"
//- minicore: fmt, future, async_fn, iterator, range, try, option, coroutine, deref, index
struct S { a: u32, b: u32 }
enum E { A(u32), B { x: u32 }, C }
macro_rules! m { ($e:expr) => { $e }; }
macro_rules! p { ($p:pat) => { $p }; }

fn every_kind(x: u32, (y, _): (u32, u32)) -> u32 {
    let s = S { a: 1, b: 2 };
    let S { a, .. } = s;
    let t = S { a: 0, ..s };
    let _ = (1u8, 2i64, 3.0f32, 4.5, 'c', b'b', "s", b"bs", c"cs", true, 1_000u128, -7, 0x10usize);
    let arr = [1, 2, 3];
    let rep = [0u8; 4];
    let r = &arr[0];
    let rp = &raw const x;
    let _ = *r + 1 - 2 * 3 / 4 % 5 << 1 >> 1 & 1 | 1 ^ 1;
    let _ = !true && false || x == y && x != y && x < y && x <= y && x > y && x >= y;
    let _ = -(x as i64);
    let mut n = 0;
    n += 1;
    n = 2;
    (n, _) = (3, 4);
    let c = |a: u32, b| -> u32 { a + b };
    let mc = move || x;
    let fut = async { 1 };
    let ac = async move |z: u32| z;
    let co = #[coroutine] || { yield 1; };
    let tr = || -> Option<u32> { let v = Some(1)?; Some(v) };
    let tail = || become every_kind(1, (2, 3));
    let yeet = || -> Option<u32> { do yeet };
    let _ = c(1, 2);
    let _ = x.max::<u32>(1);
    let _ = s.a;
    let _ = x as u64;
    let _ = 0..1;
    let _ = ..;
    let _ = 1..=2;
    let _ = const { 5 };
    let _ = builtin#offset_of(S, a);
    let _ = builtin#format_args("{} {x} {:>5}", 1, 2.5);
    unsafe { builtin#asm("mov {0}, {1}", "nop", out(reg) n, in(reg) x, options(nostack, nomem)); }
    let _ = m!(1 + 1);
    let p!(q) = 5;
    'outer: loop {
        while x > 0 {
            break 'outer;
        }
        for i in 0..3 {
            continue;
        }
        break;
    }
    let v = 'blk: { break 'blk 1 };
    let _ = if let Some(w) = Some(1) { w } else if x == 0 { 0 } else { 1 };
    let Some(z) = Some(1) else { return 0 };
    match E::A(1) {
        E::A(0 | 1) => {}
        E::A(k @ 2..=5) if k > 3 => {}
        E::B { x: ref xx } => {}
        E::C => {}
        E::A(..) => {}
    }
    match &[1, 2, 3][..] {
        [first, .., last] => {}
        [] => {}
        _ => {}
    }
    match (1, 2) {
        (1, ..) => {}
        (j, l) => {}
    }
    let &bx = &1;
    let box bb = 1;
    let _ = async { fut.await };
    let _ = { struct Inner; 1 };
    macro_rules! inner { () => {} }
    #[cfg(any())]
    let off = 1;
    let _ = _;
    1
}
"#;

    #[test]
    fn every_kind_round_trips() {
        let (db, defs) = bodies(EVERY_KIND);
        let body = Body::of(&db, defs[0]);
        let buffer = Aligned::copy_from(&blaze_format::to_bytes(&*body));
        let read = Body::read(&buffer).expect("a body just written");
        let store = read.store();
        let kinds: FxHashSet<ExprKind> =
            (0..store.exprs_len() as u32).filter_map(|id| store.expr_kind(idx(id))).collect();
        for kind in [
            ExprKind::Path,
            ExprKind::If,
            ExprKind::Let,
            ExprKind::Block,
            ExprKind::Const,
            ExprKind::Loop,
            ExprKind::Call,
            ExprKind::MethodCall,
            ExprKind::Match,
            ExprKind::Continue,
            ExprKind::Break,
            ExprKind::Return,
            ExprKind::RecordLit,
            ExprKind::Field,
            ExprKind::Await,
            ExprKind::Cast,
            ExprKind::Ref,
            ExprKind::UnaryOp,
            ExprKind::BinaryOp,
            ExprKind::Assignment,
            ExprKind::Index,
            ExprKind::Closure,
            ExprKind::Tuple,
            ExprKind::Array,
            ExprKind::Literal,
            ExprKind::OffsetOf,
            ExprKind::InlineAsm,
        ] {
            assert!(kinds.contains(&kind), "no {kind:?} in the body");
        }
        let pats: FxHashSet<PatKind> =
            (0..store.pats_len() as u32).filter_map(|id| store.pat_kind(idx(id))).collect();
        for kind in [
            PatKind::Wild,
            PatKind::Tuple,
            PatKind::Or,
            PatKind::Record,
            PatKind::Range,
            PatKind::Slice,
            PatKind::Path,
            PatKind::Lit,
            PatKind::Bind,
            PatKind::TupleStruct,
            PatKind::Ref,
        ] {
            assert!(pats.contains(&kind), "no {kind:?} in the body");
        }
        assert_eq!(read.params_len(), 2);
    }

    #[test]
    fn signatures_round_trip() {
        bodies(
            r#"
//- minicore: fn, iterator, option, sized, clone
trait Tr<'a> { type Out; }
fn sig<'a, T: Clone + ?Sized, const N: usize>(
    a: &'a mut T,
    b: *const u8,
    c: [u8; N],
    d: &[T],
    e: fn(u32) -> !,
    g: impl Fn(u32) -> u32,
    h: &dyn Iterator<Item = u32>,
    i: (u32, &'static str),
    j: <T as Tr<'a>>::Out,
    k: impl for<'b> Tr<'b, Out = u32> + use<'a, T>,
) -> Option<[u8; { N + 1 }]>
where
    T: 'a,
{
    None
}
"#,
        );
    }

    #[test]
    fn closures_and_self_round_trip() {
        bodies(
            r#"
//- minicore: fn, future, async_fn
struct W(u32);
impl W {
    fn get(&self, v: u32) -> u32 {
        let f = |a: u32| {
            let inner = a + self.0;
            let g = move || inner;
            g()
        };
        f(v)
    }
}
async fn asynchronous(a: u32, (b, c): (u32, u32)) -> u32 { a + b + c }
fn main() {}
"#,
        );
    }

    /// `bytes` with the word at `at` replaced by `word`.
    fn with_word(bytes: &[u8], at: usize, word: u32) -> Aligned {
        let mut bytes = bytes.to_vec();
        bytes[at..at + size_of::<u32>()].copy_from_slice(&word.to_le_bytes());
        Aligned::copy_from(&bytes)
    }

    fn offset_in<T>(slice: &[T], buffer: &[u8]) -> usize {
        slice.as_ptr() as usize - buffer.as_ptr() as usize
    }

    #[test]
    fn a_corrupted_buffer_is_refused() {
        let (db, defs) = bodies(
            r#"
fn f() -> u32 {
    let t = (1, 2);
    if true { t.0 } else { 2 }
}
"#,
        );
        let body = Body::of(&db, defs[0]);
        let bytes = blaze_format::to_bytes(&body.store);
        let buffer = Aligned::copy_from(&bytes);
        let read = ExpressionStore::read(&buffer).expect("a store just written");
        let exprs = read.exprs_len() as u32;

        let data_len = at_of(&read.root.expr_data, &buffer) as usize + offset_of!(Slice<u32>, len);
        let short = with_word(&bytes, data_len, exprs - 1);
        assert!(ExpressionStore::read(&short).is_err(), "a data column of fewer words than nodes");

        let list = offset_in(read.expr_lists, &buffer);
        let past = with_word(&bytes, list, exprs);
        let past = ExpressionStore::read(&past).expect("only a list array changed");
        assert!(past.decode().is_err(), "an expression id past its arena in a list");

        let statement = offset_in(read.statements, &buffer) + offset_of!(StatementRec, kind);
        let bad = ExpressionStore::read(&with_word(&bytes, statement, 99)).expect("only a statement changed");
        assert!(bad.decode().is_err(), "a statement kind out of range");

        let pat = offset_in(read.statements, &buffer) + offset_of!(StatementRec, id);
        let past = with_word(&bytes, pat, read.pats_len() as u32);
        let past = ExpressionStore::read(&past).expect("only a statement changed");
        assert!(past.decode().is_err(), "a pattern id past its arena in a statement");

        let root = offset_in(read.expr_roots, &buffer) + offset_of!(ExprRootRec, root);
        assert!(ExpressionStore::read(&with_word(&bytes, root, exprs)).is_err());

        assert!(Body::read(&buffer).is_err(), "a store's buffer read as a body's");
        assert!(ExpressionStore::read(&Aligned::copy_from(&bytes[..bytes.len() - 16])).is_err());

        let (_, source_map) = Body::with_source_map(&db, defs[0]);
        let bytes = blaze_format::to_bytes(&source_map.store);
        let buffer = Aligned::copy_from(&bytes);
        let read = ExpressionStoreSourceMap::read(&buffer).expect("a source map just written");
        let slot = (read.expr_map_back.iter().position(|src| src.file_kind != 0))
            .expect("an expression with syntax");
        let src = offset_in(read.expr_map_back, &buffer) + slot * size_of::<Src>();
        assert!(ExpressionStoreSourceMap::read(&with_word(&bytes, src + offset_of!(Src, kind), u32::MAX)).is_err());
        assert!(ExpressionStoreSourceMap::read(&with_word(&bytes, src + offset_of!(Src, file_kind), 7)).is_err());
        assert!(ExpressionStoreSourceMap::read(&with_word(&bytes, src + offset_of!(Src, file), 0)).is_err());
    }

    #[test]
    fn a_corrupted_kind_or_list_range_is_refused() {
        let (db, defs) = bodies("fn f() -> (u32, u32, u32) { (1, 2, 3) }");
        let body = Body::of(&db, defs[0]);
        let columns = &body.store.expr_only.as_ref().expect("a body has expressions").columns;
        let (tuple, elements) = (body.store.exprs())
            .find_map(|(id, expr)| match expr {
                Expr::Tuple { exprs } if exprs.len() == 3 => Some((id, exprs)),
                _ => None,
            })
            .expect("the tuple");
        let start = (elements.as_ptr() as usize - columns.expr_lists.as_ptr() as usize)
            / size_of::<ExprId>();
        let range = (columns.extra.windows(2))
            .position(|pair| pair == [start as u32, 3])
            .expect("the tuple's list as a `(start, len)` pair in `extra`");

        let bytes = blaze_format::to_bytes(&body.store);
        let buffer = Aligned::copy_from(&bytes);
        let read = ExpressionStore::read(&buffer).expect("a store just written");
        assert_eq!(read.expr_kind(tuple), Some(ExprKind::Tuple));

        let kind = offset_in(read.expr_kind, &buffer) + raw(tuple) as usize;
        let mut bad = bytes.clone();
        bad[kind] = ExprKind::ALL.len() as u8;
        assert!(ExpressionStore::read(&Aligned::copy_from(&bad)).is_err(), "a kind out of range");

        let range_at = offset_in(read.extra, &buffer) + range * size_of::<u32>();
        let lists = read.expr_lists.len() as u32;
        let past = with_word(&bytes, range_at, lists);
        let past = ExpressionStore::read(&past).expect("only `extra` changed");
        assert!(past.decode().is_err(), "a list that starts past its array");

        let long = with_word(&bytes, range_at + size_of::<u32>(), lists + 1);
        let long = ExpressionStore::read(&long).expect("only `extra` changed");
        assert!(long.decode().is_err(), "a list that ends past its array");
    }

    #[test]
    fn a_corrupted_type_kind_or_list_range_is_refused() {
        let (db, defs) = bodies("fn f(a: (u32, u8)) {}");
        let DefWithBodyId::FunctionId(function) = defs[0] else { panic!("`f` is a function") };
        let (signature, _) = FunctionSignature::with_source_map(&db, function);
        let store: &ExpressionStore = &signature.store;
        let (tuple, elements) = (store.type_refs())
            .find_map(|(id, ty)| match ty {
                TypeRef::Tuple(elements) if elements.len() == 2 => Some((id, elements)),
                _ => None,
            })
            .expect("the tuple");
        let start = (elements.as_ptr() as usize - store.types.lists.as_ptr() as usize)
            / size_of::<TypeRefId>();
        let range = (store.types.extra.windows(2))
            .position(|pair| pair == [start as u32, 2])
            .expect("the tuple's list as a `(start, len)` pair in `extra`");

        let bytes = blaze_format::to_bytes(store);
        let buffer = Aligned::copy_from(&bytes);
        let read = ExpressionStore::read(&buffer).expect("a store just written");
        assert_eq!(read.type_kind(tuple), Some(TypeRefKind::Tuple));

        let kind = offset_in(read.ty_kind, &buffer) + raw(tuple) as usize;
        let mut bad = bytes.clone();
        bad[kind] = TypeRefKind::ALL.len() as u8;
        assert!(ExpressionStore::read(&Aligned::copy_from(&bad)).is_err(), "a type kind out of range");

        let range_at = offset_in(read.ty_extra, &buffer) + range * size_of::<u32>();
        let lists = read.ty_lists.len() as u32;
        let past = with_word(&bytes, range_at, lists);
        let past = ExpressionStore::read(&past).expect("only `ty_extra` changed");
        assert!(past.decode().is_err(), "a type list that starts past `lists`");

        let long = with_word(&bytes, range_at + size_of::<u32>(), lists + 1);
        let long = ExpressionStore::read(&long).expect("only `ty_extra` changed");
        assert!(long.decode().is_err(), "a type list that ends past `lists`");

        let element = offset_in(read.ty_lists, &buffer) + start * size_of::<u32>();
        let past = with_word(&bytes, element, read.types_len() as u32);
        let past = ExpressionStore::read(&past).expect("only `ty_lists` changed");
        assert!(past.decode().is_err(), "a tuple element past the types");
    }

    #[test]
    fn an_unsorted_source_table_is_refused() {
        let (db, defs) = bodies("fn f() -> u32 { let a = 1; a + 2 }");
        let (_, source_map) = Body::with_source_map(&db, defs[0]);
        let bytes = blaze_format::to_bytes(&source_map.store);
        let buffer = Aligned::copy_from(&bytes);
        let read = ExpressionStoreSourceMap::read(&buffer).expect("a source map just written");
        assert!(read.expr_map.len() >= 2, "a body of more than one expression");
        let (keys, size) = (offset_in(read.expr_map, &buffer), size_of::<Src>());

        let mut swapped = bytes.clone();
        let (first, second) = swapped[keys..keys + 2 * size].split_at_mut(size);
        first.swap_with_slice(second);
        let swapped = ExpressionStoreSourceMap::read(&Aligned::copy_from(&swapped));
        assert!(swapped.is_err(), "a source table out of order");

        let mut twice = bytes.clone();
        twice.copy_within(keys..keys + size, keys + size);
        let twice = ExpressionStoreSourceMap::read(&Aligned::copy_from(&twice));
        assert!(twice.is_err(), "a key twice in a source table");

        let expr_map = &source_map.store.expr_only.as_deref().expect("a body's source map").expr_map;
        let reversed: Box<[_]> = expr_map.entries().iter().rev().copied().collect();
        assert!(SourceTable::from_sorted(reversed).is_none(), "decoding re-sorts nothing");
    }
}
