//! Builtin derives.
//!
//! Each derive writes its whole expansion into one [`tt::TopSubtreeBuilder`]. Where the output
//! was once made of nested `quote!`s spliced into each other, it keeps the invisible group each
//! splice added (see [`in_group`]), so the token trees, spans and encoding are the same.

use std::ops::Range;

use base_db::SourceDatabase;
use either::Either;
use intern::{Symbol, sym};
use itertools::Itertools;
use parser::SyntaxKind;
use rustc_hash::{FxHashMap, FxHashSet};
use smallvec::SmallVec;
use span::{Edition, Span};
use syntax_bridge::DocCommentDesugarMode;
use tracing::debug;

use crate::{
    ExpandError, ExpandErrorKind, ExpandResult, MacroCallId, MacroCallKind,
    builtin::quote::{ToTokenTree, dollar_crate, group},
    hygiene::span_with_def_site_ctxt,
    name::{self, AsName, Name},
    span_map::ExpansionSpanMap,
    tt,
};
use syntax::{
    ast::{
        self, AstNode, FieldList, HasAttrs, HasGenericArgs, HasGenericParams, HasModuleItem,
        HasName, HasTypeBounds,
    },
    syntax_editor::{GetOrCreateWhereClause, SyntaxEditor},
};

#[cfg(test)]
mod reference;
#[cfg(test)]
mod tests;

type Builder = tt::TopSubtreeBuilder;

macro_rules! register_builtin {
    ( $($trait:ident => $expand:ident),* $(,)? ) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum BuiltinDeriveExpander {
            $($trait),*
        }

        impl BuiltinDeriveExpander {
            /// Every variant, in declaration order.
            pub const ALL: &[Self] = &[$(Self::$trait),*];

            pub fn expander(&self) -> fn(&dyn SourceDatabase, Span, &tt::TopSubtree) -> ExpandResult<tt::TopSubtree>  {
                match *self {
                    $( BuiltinDeriveExpander::$trait => $expand, )*
                }
            }

            /// The builtin derive `name` names: its declared index and one load.
            fn find_by_name(name: &name::Name) -> Option<Self> {
                static BY_SYMBOL: intern::KnownNames =
                    intern::KnownNames::new(&[$( sym::Declared::$trait, )*]);
                BY_SYMBOL.get(name.symbol()).map(|at| Self::ALL[usize::from(at)])
            }

            /// Each builtin derive's name, in declaration order.
            #[cfg(test)]
            const NAMES: &[intern::Symbol] = &[$( sym::$trait, )*];

            /// [`Self::find_by_name`] as a chain of compares, the reference it is tested by.
            #[cfg(test)]
            fn find_by_name_by_compare(name: &name::Name) -> Option<Self> {
                match name {
                    $( id if id == &sym::$trait => Some(BuiltinDeriveExpander::$trait), )*
                     _ => None,
                }
            }
        }
    };
}

impl BuiltinDeriveExpander {
    pub fn expand(
        &self,
        db: &dyn SourceDatabase,
        id: MacroCallId,
        tt: &tt::TopSubtree,
        span: Span,
    ) -> ExpandResult<tt::TopSubtree> {
        let span = span_with_def_site_ctxt(db, span, id.into(), Edition::CURRENT);
        let expanded = if let BuiltinDeriveExpander::Clone = self
            && let MacroCallKind::Derive { ast_id, .. } = &id.loc(db).kind
            && derives_copy(&ast_id.to_node(db))
        {
            clone_expand_with(db, span, tt, true)
        } else {
            self.expander()(db, span, tt)
        };
        if from_derive_const(db, id) { expanded.map(as_const_impls) } else { expanded }
    }
}

/// Whether the derive is listed in a `#[derive_const(..)]`, whose impls are `const impl`s.
fn from_derive_const(db: &dyn SourceDatabase, id: MacroCallId) -> bool {
    let MacroCallKind::Derive { derive_macro_id, .. } = id.loc(db).kind else { return false };
    matches!(
        derive_macro_id.loc(db).def.kind,
        crate::MacroDefKind::BuiltInAttr(_, crate::builtin::BuiltinAttrExpander::DeriveConst)
    )
}

/// `impl ..` items of a derive's expansion as `const impl ..`.
fn as_const_impls(expanded: tt::TopSubtree) -> tt::TopSubtree {
    let mut builder = tt::TopSubtreeBuilder::new(expanded.top_subtree().delimiter);
    let mut items = expanded.token_trees().iter();
    while let Some(next) = items.peek() {
        if let tt::TtElement::Leaf(tt::Leaf::Ident(ident)) = &next
            && ident.sym == sym::kw_impl
        {
            builder.push(tt::Leaf::Ident(tt::Ident {
                sym: sym::const_,
                span: ident.span,
                is_raw: tt::IdentIsRaw::No,
            }));
        }
        let Some(item) = items.next_as_view() else { break };
        builder.extend_with_tt(item);
    }
    builder.build()
}

/// Whether the words of `attr`'s text (its runs of letters, digits and `_`, leaving out those in
/// `skip`) start with `head` and then have `word`. A word never spans two tokens, so the tokens
/// are split one by one and nothing is copied.
fn attr_words_have(attr: &ast::Attr, head: &str, skip: &[&str], word: &str) -> bool {
    let mut first = true;
    for token in attr.syntax().descendants_with_tokens().filter_map(|it| it.into_token()) {
        for it in token.text().split(|c: char| !c.is_alphanumeric() && c != '_') {
            if it.is_empty() || skip.contains(&it) {
                continue;
            }
            if first {
                if it != head {
                    return false;
                }
                first = false;
            } else if it == word {
                return true;
            }
        }
    }
    false
}

/// Whether one of the item's `#[derive]`s lists `Copy`.
fn derives_copy(adt: &ast::Adt) -> bool {
    adt.attrs().any(|attr| attr_words_have(&attr, "derive", &[], "Copy"))
}

register_builtin! {
    Copy => copy_expand,
    Clone => clone_expand,
    Default => default_expand,
    Debug => debug_expand,
    Hash => hash_expand,
    Ord => ord_expand,
    PartialOrd => partial_ord_expand,
    Eq => eq_expand,
    PartialEq => partial_eq_expand,
    CoercePointee => coerce_pointee_expand,
    Reborrow => reborrow_expand,
    CoerceShared => coerce_shared_expand,
    ConstParamTy => const_param_ty_expand,
    From => from_expand,
}

pub fn find_builtin_derive(ident: &name::Name) -> Option<BuiltinDeriveExpander> {
    BuiltinDeriveExpander::find_by_name(ident)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum FieldsKind {
    Record,
    Tuple,
    Unit,
}

/// A struct's fields or an enum's variant: `fields[start..end]` of its [`AdtShape`].
#[derive(Debug)]
struct Variant {
    /// The variant's name; the type's for a struct.
    name: tt::Ident,
    kind: FieldsKind,
    start: u32,
    end: u32,
}

impl Variant {
    fn fields(&self) -> Range<usize> {
        self.start as usize..self.end as usize
    }

    fn is_empty(&self) -> bool {
        self.start == self.end
    }
}

#[derive(Debug)]
enum AdtKind {
    Struct,
    Enum { default_variant: Option<usize> },
    Union,
}

/// The variants, and all their fields in one list: a record field by its name, a tuple field
/// `i` as `f{i}`. The span of each field is where it is written, in the derive's hygiene: what
/// the expansion says of the field (as that moving it out of a packed struct is E0507) points
/// there, as rustc's does.
#[derive(Debug)]
struct AdtShape {
    kind: AdtKind,
    /// One for a struct, none for a union.
    variants: Vec<Variant>,
    fields: Vec<tt::Ident>,
}

struct BasicAdtInfo {
    name: tt::Ident,
    shape: AdtShape,
    /// first field is the name, and
    /// second field is `Some(ty)` if it's a const param of type `ty`, `None` if it's a type param.
    /// third fields is where bounds, if any
    param_types: Vec<AdtParam>,
    /// Each lifetime parameter as declared, bounds and all, and its name: the impl has them
    /// all, as rustc's does, or its self type would elide them (E0726).
    lifetimes: Vec<(tt::TopSubtree, tt::TopSubtree)>,
    where_clause: Vec<tt::TopSubtree>,
    associated_types: Vec<tt::TopSubtree>,
    /// Whether the fields can be misaligned, and so have to be copied out to be used.
    packed: bool,
    /// Whether the impl also bounds the type parameters and associated types by `Copy`, after
    /// the item's own `where` clause: what the fields of a packed struct are made of, since
    /// they are copied out (rustc's `needs_copy_as_bound_if_packed`).
    copy_bounds: bool,
}

struct AdtParam {
    name: tt::TopSubtree,
    /// `None` if this is a type parameter.
    const_ty: Option<tt::TopSubtree>,
    bounds: Option<tt::TopSubtree>,
}

/// `a` then `b`, interned, with no heap allocation for a name of up to 64 bytes.
fn joined(a: &str, b: &str) -> Symbol {
    let mut buf: SmallVec<[u8; 64]> = SmallVec::with_capacity(a.len() + b.len());
    buf.extend_from_slice(a.as_bytes());
    buf.extend_from_slice(b.as_bytes());
    Symbol::intern(std::str::from_utf8(&buf).expect("two `str`s joined are one"))
}

/// `f{i}`, the name of tuple field `i`.
fn tuple_field_symbol(i: usize) -> Symbol {
    let mut digits = [0u8; 20];
    let mut at = digits.len();
    let mut n = i;
    loop {
        at -= 1;
        digits[at] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    joined("f", std::str::from_utf8(&digits[at..]).expect("ASCII digits"))
}

/// Writes what `f` writes in an invisible group at `span`: what splicing a `quote!` at `span`
/// of anything but one delimited group adds.
#[inline]
fn in_group(b: &mut Builder, span: Span, f: impl FnOnce(&mut Builder)) {
    b.open(tt::DelimiterKind::Invisible, span);
    f(b);
    b.close(span);
}

/// What splicing an empty `quote!` adds.
#[inline]
fn empty_group(b: &mut Builder, span: Span) {
    in_group(b, span, |_| {})
}

#[inline]
fn delimited(b: &mut Builder, kind: tt::DelimiterKind, span: Span, f: impl FnOnce(&mut Builder)) {
    b.open(kind, span);
    f(b);
    b.close(span);
}

fn punct(char: char, spacing: tt::Spacing, span: Span) -> tt::Punct {
    tt::Punct { char, spacing, span }
}

/// Two punctuation characters joined, as `quote!` can't write them (`=>`, `&&`, `==`).
fn joint(b: &mut Builder, span: Span, first: char, second: char) {
    in_group(b, span, |b| {
        b.push(punct(first, tt::Spacing::Joint, span).into());
        b.push(punct(second, tt::Spacing::Alone, span).into());
    })
}

fn fat_arrow(b: &mut Builder, span: Span) {
    joint(b, span, '=', '>')
}

fn and_and(b: &mut Builder, span: Span) {
    joint(b, span, '&', '&')
}

fn eq_eq(b: &mut Builder, span: Span) {
    joint(b, span, '=', '=')
}

impl BasicAdtInfo {
    fn is_empty_enum(&self) -> bool {
        matches!(self.shape.kind, AdtKind::Enum { .. }) && self.shape.variants.is_empty()
    }

    fn is_enum(&self) -> bool {
        matches!(self.shape.kind, AdtKind::Enum { .. })
    }

    fn packed_struct(&self) -> bool {
        self.packed && matches!(self.shape.kind, AdtKind::Struct)
    }

    /// What a pattern over `self` (`suffix` `_self`) or `other` (`_other`) binds each field of
    /// [`AdtShape::fields`] to: the name of the field may be that of a tuple struct or a constant
    /// in scope, which a binding can't have. One interning per field per derive.
    fn bindings(&self, suffix: &str) -> Vec<tt::Ident> {
        let mut out = Vec::with_capacity(self.shape.fields.len());
        out.extend(self.shape.fields.iter().map(|it| tt::Ident {
            sym: joined(it.sym.as_str(), suffix),
            span: it.span,
            is_raw: tt::IdentIsRaw::No,
        }));
        out
    }

    /// `Name` or `Name::Variant`.
    fn write_path(&self, b: &mut Builder, span: Span, v: &Variant) {
        let name = &self.name;
        if self.is_enum() {
            let variant = &v.name;
            in_group(b, span, |b| quote_into!(b, span => #name :: #variant))
        } else {
            in_group(b, span, |b| quote_into!(b, span => #name))
        }
    }

    /// The variant's pattern or expression: its path, and what `field` writes in the place of
    /// each field (by its index in [`AdtShape::fields`]).
    fn write_variant(
        &self,
        b: &mut Builder,
        span: Span,
        v: &Variant,
        field: impl Fn(&mut Builder, usize),
    ) {
        match v.kind {
            FieldsKind::Unit => self.write_path(b, span, v),
            FieldsKind::Record => in_group(b, span, |b| {
                self.write_path(b, span, v);
                delimited(b, tt::DelimiterKind::Brace, span, |b| {
                    for i in v.fields() {
                        let it = &self.shape.fields[i];
                        in_group(b, span, |b| {
                            quote_into!(b, span => #it :);
                            field(b, i);
                            quote_into!(b, span => ,);
                        });
                    }
                });
            }),
            FieldsKind::Tuple => in_group(b, span, |b| {
                self.write_path(b, span, v);
                delimited(b, tt::DelimiterKind::Parenthesis, span, |b| {
                    for i in v.fields() {
                        in_group(b, span, |b| {
                            field(b, i);
                            quote_into!(b, span => ,);
                        });
                    }
                });
            }),
        }
    }

    /// The variant's pattern, which binds each field to its name in `bindings`.
    fn write_pattern(&self, b: &mut Builder, span: Span, v: &Variant, bindings: &[tt::Ident]) {
        self.write_variant(b, span, v, |b, i| {
            let bound = &bindings[i];
            in_group(b, span, |b| quote_into!(b, span => #bound))
        })
    }

    /// For a packed struct, a `let` of each field of `self` (or `other`) to a copy out of it, to
    /// the name in `bindings` the patterns of the other derives bind it to.
    fn write_packed_lets(&self, b: &mut Builder, bindings: &[tt::Ident], other: bool) {
        let Some(v) = self.shape.variants.first() else { return };
        for i in v.fields() {
            let field = &self.shape.fields[i];
            let at = field.span;
            let bound = &bindings[i];
            let index = i - v.start as usize;
            let place = group(at, |b| match v.kind {
                FieldsKind::Tuple => quote_into!(b, at => #index),
                _ => quote_into!(b, at => #field),
            });
            in_group(b, at, |b| {
                if other {
                    quote_into!(b, at => let #bound = { other . #place };)
                } else {
                    quote_into!(b, at => let #bound = { self . #place };)
                }
            });
        }
    }

    /// A `match` of `self` against each variant's pattern, with the arm `body` writes for it. A
    /// packed struct's fields are copied out of it instead, as rustc's derives do
    /// (`{ self.x }`): a field may be misaligned, so isn't borrowed, and moving one that isn't
    /// `Copy` is E0507, where the field is.
    fn write_match_self(
        &self,
        b: &mut Builder,
        span: Span,
        selfs: &[tt::Ident],
        body: impl Fn(&mut Builder, &Variant),
    ) {
        if self.packed_struct() {
            delimited(b, tt::DelimiterKind::Brace, span, |b| {
                self.write_packed_lets(b, selfs, false);
                body(b, &self.shape.variants[0]);
            });
            return;
        }
        in_group(b, span, |b| {
            quote_into!(b, span => match self);
            delimited(b, tt::DelimiterKind::Brace, span, |b| {
                for v in &self.shape.variants {
                    in_group(b, span, |b| {
                        self.write_pattern(b, span, v, selfs);
                        fat_arrow(b, span);
                        body(b, v);
                        quote_into!(b, span => ,);
                    });
                }
            });
        });
    }

    /// A `match` of `self` and `other` against each of `arms`' two patterns, with the arm
    /// `body` writes for it, and `fallback` if none matches. A packed struct's fields are copied
    /// out of both, as by [`Self::write_match_self`].
    fn write_match_both<'a>(
        &'a self,
        b: &mut Builder,
        span: Span,
        (selfs, others): (&[tt::Ident], &[tt::Ident]),
        arms: impl Iterator<Item = &'a Variant>,
        body: impl Fn(&mut Builder, &Variant),
        fallback: impl FnOnce(&mut Builder),
    ) {
        if self.packed_struct() {
            delimited(b, tt::DelimiterKind::Brace, span, |b| {
                self.write_packed_lets(b, selfs, false);
                self.write_packed_lets(b, others, true);
                body(b, &self.shape.variants[0]);
            });
            return;
        }
        in_group(b, span, |b| {
            quote_into!(b, span => match (self, other));
            delimited(b, tt::DelimiterKind::Brace, span, |b| {
                for v in arms {
                    in_group(b, span, |b| {
                        delimited(b, tt::DelimiterKind::Parenthesis, span, |b| {
                            self.write_pattern(b, span, v, selfs);
                            quote_into!(b, span => ,);
                            self.write_pattern(b, span, v, others);
                        });
                        fat_arrow(b, span);
                        body(b, v);
                        quote_into!(b, span => ,);
                    });
                }
                quote_into!(b, span => _unused);
                fat_arrow(b, span);
                fallback(b);
            });
        });
    }
}

// FIXME: This whole thing needs a refactor. Each derive requires its special values, and the result is a mess.
fn parse_adt(
    db: &dyn SourceDatabase,
    tt: &tt::TopSubtree,
    call_site: Span,
) -> Result<BasicAdtInfo, ExpandError> {
    let (adt, tm) = to_adt_syntax(db, tt, call_site)?;
    parse_adt_from_syntax(&adt, &tm, call_site)
}

/// Pushes the fields of `list` to `fields`, and says how they are written.
fn push_fields(
    fields: &mut Vec<tt::Ident>,
    call_site: Span,
    tm: &ExpansionSpanMap,
    list: Option<FieldList>,
) -> Result<FieldsKind, ExpandError> {
    let at_field = |span: Span| Span { ctx: call_site.ctx, ..span };
    Ok(match list {
        None => FieldsKind::Unit,
        Some(FieldList::RecordFieldList(it)) => {
            for field in it.fields() {
                let name = name_to_token(call_site, tm, field.name())?;
                fields.push(tt::Ident { span: at_field(name.span), ..name });
            }
            FieldsKind::Record
        }
        Some(FieldList::TupleFieldList(it)) => {
            for (i, field) in it.fields().enumerate() {
                let at = field
                    .ty()
                    .map_or_else(|| field.syntax().text_range(), |ty| ty.syntax().text_range());
                fields.push(tt::Ident {
                    sym: tuple_field_symbol(i),
                    span: at_field(tm.span_at(at.start())),
                    is_raw: tt::IdentIsRaw::No,
                });
            }
            FieldsKind::Tuple
        }
    })
}

fn len_u32(len: usize) -> u32 {
    u32::try_from(len).expect("fewer than 2^32 fields")
}

fn parse_adt_from_syntax(
    adt: &ast::Adt,
    tm: &span::SpanMap,
    call_site: Span,
) -> Result<BasicAdtInfo, ExpandError> {
    let mut fields = Vec::new();
    let mut variants = Vec::new();
    // A struct's one variant takes its name, which is read last, as the errors are.
    let mut struct_fields = None;
    let (name, generic_param_list, where_clause, kind) = match &adt {
        ast::Adt::Struct(it) => {
            struct_fields = Some(push_fields(&mut fields, call_site, tm, it.field_list())?);
            (it.name(), it.generic_param_list(), it.where_clause(), AdtKind::Struct)
        }
        ast::Adt::Enum(it) => {
            let mut default_variant = None;
            for (i, variant) in it.variant_list().into_iter().flat_map(|it| it.variants()).enumerate()
            {
                if default_variant.is_none()
                    && variant.attrs().any(|it| it.simple_name() == Some("default".into()))
                {
                    default_variant = Some(i);
                }
                let name = name_to_token(call_site, tm, variant.name())?;
                let start = len_u32(fields.len());
                let kind = push_fields(&mut fields, call_site, tm, variant.field_list())?;
                variants.push(Variant { name, kind, start, end: len_u32(fields.len()) });
            }
            (
                it.name(),
                it.generic_param_list(),
                it.where_clause(),
                AdtKind::Enum { default_variant },
            )
        }
        ast::Adt::Union(it) => {
            (it.name(), it.generic_param_list(), it.where_clause(), AdtKind::Union)
        }
    };

    let to_tt = |node: &syntax::SyntaxNode| {
        syntax_bridge::syntax_node_to_token_tree(
            node,
            tm,
            call_site,
            DocCommentDesugarMode::ProcMacro,
        )
    };
    let lifetimes = generic_param_list
        .iter()
        .flat_map(|param_list| param_list.lifetime_params())
        .filter_map(|param| Some((to_tt(param.syntax()), to_tt(param.lifetime()?.syntax()))))
        .collect();
    let mut param_names: SmallVec<[Name; 4]> = SmallVec::new();
    let param_types = generic_param_list
        .into_iter()
        .flat_map(|param_list| param_list.type_or_const_params())
        .map(|param| {
            let name = match param.name() {
                Some(it) => {
                    let name = it.as_name();
                    if !param_names.contains(&name) {
                        param_names.push(name);
                    }
                    to_tt(it.syntax())
                }
                None => tt::TopSubtree::empty(::tt::DelimSpan { open: call_site, close: call_site }),
            };
            let bounds = match &param {
                ast::TypeOrConstParam::Type(it) => it.type_bound_list().map(|it| to_tt(it.syntax())),
                ast::TypeOrConstParam::Const(_) => None,
            };
            let const_ty = if let ast::TypeOrConstParam::Const(param) = param {
                let ty = param.ty().map(|ty| to_tt(ty.syntax())).unwrap_or_else(|| {
                    tt::TopSubtree::empty(::tt::DelimSpan { open: call_site, close: call_site })
                });
                Some(ty)
            } else {
                None
            };
            AdtParam { name, const_ty, bounds }
        })
        .collect();

    let where_clause = match where_clause {
        Some(w) => w.predicates().map(|it| to_tt(it.syntax())).collect(),
        None => Vec::new(),
    };

    // For a generic parameter `T`, when shorthand associated type `T::Assoc` appears in field
    // types (of any variant for enums), we generate trait bound for it. It sounds reasonable to
    // also generate trait bound for qualified associated type `<T as Trait>::Assoc`, but rustc
    // does not do that for some unknown reason.
    //
    // See the analogous function in rustc [find_type_parameters()] and rust-lang/rust#50730.
    // [find_type_parameters()]: https://github.com/rust-lang/rust/blob/1.70.0/compiler/rustc_builtin_macros/src/deriving/generic/mod.rs#L378

    // It's cumbersome to deal with the distinct structures of ADTs, so let's just get untyped
    // `SyntaxNode` that contains fields and look for descendant `ast::PathType`s. Of note is that
    // we should not inspect `ast::PathType`s in parameter bounds and where clauses. A type
    // without type parameters has none, and its fields aren't walked.
    let associated_types = if param_names.is_empty() {
        Vec::new()
    } else {
        let field_list = match adt {
            ast::Adt::Enum(it) => it.variant_list().map(|list| list.syntax().clone()),
            ast::Adt::Struct(it) => it.field_list().map(|list| list.syntax().clone()),
            ast::Adt::Union(it) => it.record_field_list().map(|list| list.syntax().clone()),
        };
        field_list
            .into_iter()
            .flat_map(|it| it.descendants())
            .filter_map(ast::PathType::cast)
            .filter_map(|p| {
                let name = p.path()?.qualifier()?.as_single_name_ref()?.as_name();
                param_names.contains(&name).then_some(p)
            })
            .map(|it| to_tt(it.syntax()))
            .collect()
    };
    let name_token = name_to_token(call_site, tm, name)?;
    if let Some(kind) = struct_fields {
        variants.push(Variant { name: name_token.clone(), kind, start: 0, end: len_u32(fields.len()) });
    }
    let packed = adt.attrs().any(|attr| attr_words_have(&attr, "repr", &["r"], "packed"));
    Ok(BasicAdtInfo {
        name: name_token,
        shape: AdtShape { kind, variants, fields },
        param_types,
        lifetimes,
        where_clause,
        associated_types,
        packed,
        copy_bounds: false,
    })
}

fn to_adt_syntax(
    db: &dyn SourceDatabase,
    tt: &tt::TopSubtree,
    call_site: Span,
) -> Result<(ast::Adt, span::SpanMap), ExpandError> {
    let (parsed, tm) = crate::token_tree_to_syntax_node(db, tt, crate::ExpandTo::Items);
    let macro_items = ast::MacroItems::cast(parsed.syntax_node())
        .ok_or_else(|| ExpandError::other(call_site, "invalid item definition"))?;
    let item =
        macro_items.items().next().ok_or_else(|| ExpandError::other(call_site, "no item found"))?;
    let adt = ast::Adt::cast(item.syntax().clone())
        .ok_or_else(|| ExpandError::other(call_site, "expected struct, enum or union"))?;
    Ok((adt, tm))
}

fn name_to_token(
    call_site: Span,
    token_map: &ExpansionSpanMap,
    name: Option<ast::Name>,
) -> Result<tt::Ident, ExpandError> {
    let name = name.ok_or_else(|| {
        debug!("parsed item has no name");
        ExpandError::other(call_site, "missing name")
    })?;
    let span = token_map.span_at(name.syntax().text_range().start());

    let name_token = tt::Ident::new(name.text(), span);
    Ok(name_token)
}

fn empty(span: Span) -> tt::TopSubtree {
    tt::TopSubtree::empty(tt::DelimSpan::from_single(span))
}

/// The builder of a derive's expansion, whose top subtree is dropped if it holds one subtree.
fn write_derive(span: Span, f: impl FnOnce(&mut Builder)) -> tt::TopSubtree {
    let mut b = Builder::new(tt::Delimiter {
        kind: tt::DelimiterKind::Invisible,
        open: span,
        close: span,
    });
    f(&mut b);
    b.build_skip_top_subtree()
}

/// Given that we are deriving a trait `DerivedTrait` for a type like:
///
/// ```ignore (only-for-syntax-highlight)
/// struct Struct<'a, ..., 'z, A, B: DeclaredTrait, C, ..., Z> where C: WhereTrait {
///     a: A,
///     b: B::Item,
///     b1: <B as DeclaredTrait>::Item,
///     c1: <C as WhereTrait>::Item,
///     c2: Option<<C as WhereTrait>::Item>,
///     ...
/// }
/// ```
///
/// create an impl like:
///
/// ```ignore (only-for-syntax-highlight)
/// impl<'a, ..., 'z, A, B: DeclaredTrait, C, ... Z> where
///     C:                       WhereTrait,
///     A: DerivedTrait + B1 + ... + BN,
///     B: DerivedTrait + B1 + ... + BN,
///     C: DerivedTrait + B1 + ... + BN,
///     B::Item:                 DerivedTrait + B1 + ... + BN,
///     <C as WhereTrait>::Item: DerivedTrait + B1 + ... + BN,
///     ...
/// {
///     ...
/// }
/// ```
///
/// where B1, ..., BN are the bounds given by `bounds_paths`. Z is a phantom type, and
/// therefore does not get bound by the derived trait.
fn expand_simple_derive<P: ToTokenTree + Copy>(
    invoc_span: Span,
    info: Result<BasicAdtInfo, ExpandError>,
    trait_path: P,
    allow_unions: bool,
    copy_if_packed: bool,
    make_trait_body: impl FnOnce(&mut Builder, &BasicAdtInfo),
) -> ExpandResult<tt::TopSubtree> {
    let mut info = match info {
        Ok(info) => info,
        Err(e) => {
            return ExpandResult::new(
                tt::TopSubtree::empty(tt::DelimSpan { open: invoc_span, close: invoc_span }),
                e,
            );
        }
    };
    if !allow_unions && matches!(info.shape.kind, AdtKind::Union) {
        return ExpandResult::new(
            empty(invoc_span),
            ExpandError::builtin_derive(invoc_span, "this trait cannot be derived for unions"),
        );
    }
    info.copy_bounds = info.packed && copy_if_packed;
    let no_extra_params = group(invoc_span, |_| {});
    ExpandResult::ok(write_derive(invoc_span, |b| {
        write_impl(b, invoc_span, &info, trait_path, true, &no_extra_params, |b| {
            make_trait_body(b, &info)
        })
    }))
}

/// `impl<..> Trait for Name<..> where .. { .. }`, the body written by `trait_body`.
fn write_impl<P: ToTokenTree + Copy, X: ToTokenTree + Copy>(
    b: &mut Builder,
    s: Span,
    info: &BasicAdtInfo,
    trait_path: P,
    constrain_to_trait: bool,
    extra_impl_params: X,
    trait_body: impl FnOnce(&mut Builder),
) {
    quote_into!(b, s => impl <);
    for (param, _) in &info.lifetimes {
        in_group(b, s, |b| quote_into!(b, s => #param ,));
    }
    for param in &info.param_types {
        let name = &param.name;
        in_group(b, s, |b| match &param.const_ty {
            Some(ty) => quote_into!(b, s => const #name : #ty ,),
            None if constrain_to_trait => quote_into!(b, s => #name : #trait_path ,),
            None => quote_into!(b, s => #name ,),
        });
    }
    let name = &info.name;
    quote_into!(b, s => #extra_impl_params > #trait_path for #name <);
    for (_, lifetime) in &info.lifetimes {
        in_group(b, s, |b| quote_into!(b, s => #lifetime ,));
    }
    for param in &info.param_types {
        let name = &param.name;
        in_group(b, s, |b| quote_into!(b, s => #name ,));
    }
    quote_into!(b, s => > where);
    for predicate in &info.where_clause {
        in_group(b, s, |b| quote_into!(b, s => #predicate ,));
    }
    if info.copy_bounds {
        let krate = &dollar_crate(s);
        let types = info.param_types.iter().filter(|it| it.const_ty.is_none()).map(|it| &it.name);
        for ty in types.chain(&info.associated_types) {
            in_group(b, s, |b| {
                in_group(b, s, |b| quote_into!(b, s => #ty : #krate::marker::Copy));
                quote_into!(b, s => ,);
            });
        }
    }
    for param in &info.param_types {
        if let Some(bounds) = &param.bounds {
            let name = &param.name;
            in_group(b, s, |b| quote_into!(b, s => #name : #bounds ,));
        }
    }
    if constrain_to_trait {
        for ty in &info.associated_types {
            in_group(b, s, |b| quote_into!(b, s => #ty : #trait_path ,));
        }
    }
    delimited(b, tt::DelimiterKind::Brace, s, trait_body);
}

fn copy_expand(
    db: &dyn SourceDatabase,
    span: Span,
    tt: &tt::TopSubtree,
) -> ExpandResult<tt::TopSubtree> {
    copy_derive(span, parse_adt(db, tt, span))
}

fn copy_derive(
    span: Span,
    info: Result<BasicAdtInfo, ExpandError>,
) -> ExpandResult<tt::TopSubtree> {
    let krate = &dollar_crate(span);
    let trait_path = group(span, |b| quote_into!(b, span => #krate::marker::Copy));
    expand_simple_derive(span, info, &trait_path, true, false, |b, _| empty_group(b, span))
}

/// `#[derive(ConstParamTy)]` (`adt_const_params`): the marker trait, named apart from the
/// macro in `core::marker`.
fn const_param_ty_expand(
    db: &dyn SourceDatabase,
    span: Span,
    tt: &tt::TopSubtree,
) -> ExpandResult<tt::TopSubtree> {
    const_param_ty_derive(span, parse_adt(db, tt, span))
}

fn const_param_ty_derive(
    span: Span,
    info: Result<BasicAdtInfo, ExpandError>,
) -> ExpandResult<tt::TopSubtree> {
    let krate = &dollar_crate(span);
    let trait_path = group(span, |b| quote_into!(b, span => #krate::marker::ConstParamTy_));
    expand_simple_derive(span, info, &trait_path, true, false, |b, _| empty_group(b, span))
}

/// `#[derive(From)]` (`derive_from`): `From` of the type of a struct's one field, as rustc's
/// (`deriving/from.rs`): the impl keeps the struct's generics and bounds, and adds no bound of
/// its own.
fn from_expand(
    db: &dyn SourceDatabase,
    span: Span,
    tt: &tt::TopSubtree,
) -> ExpandResult<tt::TopSubtree> {
    let (adt, span_map) = match to_adt_syntax(db, tt, span) {
        Ok(it) => it,
        Err(err) => return ExpandResult::new(empty(span), err),
    };
    from_derive(span, &adt, &span_map)
}

fn from_derive(span: Span, adt: &ast::Adt, span_map: &span::SpanMap) -> ExpandResult<tt::TopSubtree> {
    let strukt = match adt {
        ast::Adt::Struct(it) => it,
        ast::Adt::Enum(_) => {
            return ExpandResult::new(
                empty(span),
                ExpandError::builtin_derive(span, "`#[derive(From)]` used on an enum"),
            );
        }
        ast::Adt::Union(_) => {
            return ExpandResult::new(
                empty(span),
                ExpandError::builtin_derive(span, "`#[derive(From)]` used on a union"),
            );
        }
    };
    let mut field_types: SmallVec<[Option<ast::Type>; 2]> = SmallVec::new();
    match strukt.field_list() {
        Some(FieldList::RecordFieldList(it)) => {
            field_types.extend(it.fields().map(|it| it.ty()).take(2))
        }
        Some(FieldList::TupleFieldList(it)) => {
            field_types.extend(it.fields().map(|it| it.ty()).take(2))
        }
        None => {}
    }
    let field_ty = match &field_types[..] {
        [Some(ty)] => syntax_bridge::syntax_node_to_token_tree(
            ty.syntax(),
            span_map,
            span,
            DocCommentDesugarMode::ProcMacro,
        ),
        [None] => {
            return ExpandResult::new(empty(span), ExpandError::other(span, "missing field type"));
        }
        fields => {
            let count = if fields.is_empty() { "no fields" } else { "multiple fields" };
            return ExpandResult::new(
                empty(span),
                ExpandError::builtin_derive(
                    span,
                    format!("`#[derive(From)]` used on a struct with {count}"),
                ),
            );
        }
    };
    let info = match parse_adt_from_syntax(adt, span_map, span) {
        Ok(it) => it,
        Err(err) => return ExpandResult::new(empty(span), err),
    };
    let krate = &dollar_crate(span);
    let field_ty = &field_ty;
    let record_field = match info.shape.variants.first() {
        Some(Variant { kind: FieldsKind::Record, .. }) => info.shape.fields.first(),
        _ => None,
    };
    let body = group(span, |b| match record_field {
        Some(field) => quote_into!(b, span => Self { #field : value }),
        None => quote_into!(b, span => Self ( value )),
    });
    let trait_path = group(span, |b| quote_into!(b, span => #krate::convert::From<#field_ty>));
    let no_extra_params = group(span, |_| {});
    ExpandResult::ok(write_derive(span, |b| {
        write_impl(b, span, &info, &trait_path, false, &no_extra_params, |b| {
            in_group(b, span, |b| {
                quote_into!(b, span =>
                    #[inline]
                    fn from(value : #field_ty) -> Self { #body }
                )
            })
        })
    }))
}

fn reborrow_expand(
    db: &dyn SourceDatabase,
    span: Span,
    tt: &tt::TopSubtree,
) -> ExpandResult<tt::TopSubtree> {
    reborrow_derive(span, parse_adt(db, tt, span))
}

fn reborrow_derive(
    span: Span,
    info: Result<BasicAdtInfo, ExpandError>,
) -> ExpandResult<tt::TopSubtree> {
    let krate = &dollar_crate(span);
    let trait_path = group(span, |b| quote_into!(b, span => #krate::marker::Reborrow));
    expand_reborrow_marker_derive(span, info, &trait_path, "Reborrow")
}

fn coerce_shared_expand(
    db: &dyn SourceDatabase,
    span: Span,
    tt: &tt::TopSubtree,
) -> ExpandResult<tt::TopSubtree> {
    let (adt, span_map) = match to_adt_syntax(db, tt, span) {
        Ok(it) => it,
        Err(err) => {
            return ExpandResult::new(empty(span), err);
        }
    };
    let ast::Adt::Struct(strukt) = &adt else {
        return ExpandResult::new(
            empty(span),
            ExpandError::builtin_derive(span, "`CoerceShared` can only be derived on `struct`s"),
        );
    };
    let Some(target) = coerce_shared_target(db, span, strukt) else {
        return ExpandResult::new(
            empty(span),
            ExpandError::builtin_derive(
                span,
                "`derive(CoerceShared)` requires exactly one `#[coerce_shared(Target)]` attribute",
            ),
        );
    };
    let info = match parse_adt_from_syntax(&adt, &span_map, span) {
        Ok(it) => it,
        Err(err) => {
            return ExpandResult::new(empty(span), err);
        }
    };

    let krate = &dollar_crate(span);
    let target = &target;
    let trait_path =
        group(span, |b| quote_into!(b, span => #krate::marker::CoerceShared<#target>));
    let no_extra_params = group(span, |_| {});
    ExpandResult::ok(write_derive(span, |b| {
        write_impl(b, span, &info, &trait_path, false, &no_extra_params, |b| empty_group(b, span))
    }))
}

fn expand_reborrow_marker_derive<P: ToTokenTree + Copy>(
    span: Span,
    info: Result<BasicAdtInfo, ExpandError>,
    trait_path: P,
    trait_name: &'static str,
) -> ExpandResult<tt::TopSubtree> {
    let info = match info {
        Ok(info) => info,
        Err(err) => {
            return ExpandResult::new(empty(span), err);
        }
    };
    if !matches!(info.shape.kind, AdtKind::Struct) {
        return ExpandResult::new(
            empty(span),
            ExpandError::builtin_derive(
                span,
                format!("`{trait_name}` can only be derived on `struct`s"),
            ),
        );
    }

    let no_extra_params = group(span, |_| {});
    ExpandResult::ok(write_derive(span, |b| {
        write_impl(b, span, &info, trait_path, false, &no_extra_params, |b| empty_group(b, span))
    }))
}

fn coerce_shared_target(
    db: &dyn SourceDatabase,
    span: Span,
    strukt: &ast::Struct,
) -> Option<tt::TopSubtree> {
    let mut attrs = strukt
        .attrs()
        .filter(|attr| attr.as_simple_call().is_some_and(|(name, _)| name == "coerce_shared"));
    let attr = attrs.next()?;
    if attrs.next().is_some() {
        return None;
    }
    let (_, target_tokens) = attr.as_simple_call()?;
    let l_paren = target_tokens.l_paren_token()?;
    let r_paren = target_tokens.r_paren_token()?;

    let mut span_map = span::SpanMap::empty();
    span_map.push(target_tokens.syntax().text_range().start(), span);
    span_map.push(target_tokens.syntax().text_range().end(), span);

    let remove = FxHashSet::from_iter([l_paren.into(), r_paren.into()]);
    let target = syntax_bridge::syntax_node_to_token_tree_modified(
        target_tokens.syntax(),
        &span_map,
        FxHashMap::default(),
        remove,
        span,
        DocCommentDesugarMode::ProcMacro,
        |_, _| (true, Vec::new()),
    );

    let (parse, _) = crate::token_tree_to_syntax_node(db, &target, crate::ExpandTo::Type);
    if !parse.errors().is_empty() {
        return None;
    }
    ast::Type::cast(parse.syntax_node())?;
    Some(target)
}

fn clone_expand(
    db: &dyn SourceDatabase,
    span: Span,
    tt: &tt::TopSubtree,
) -> ExpandResult<tt::TopSubtree> {
    clone_expand_with(db, span, tt, false)
}

fn clone_expand_with(
    db: &dyn SourceDatabase,
    span: Span,
    tt: &tt::TopSubtree,
    copy: bool,
) -> ExpandResult<tt::TopSubtree> {
    clone_derive(span, parse_adt(db, tt, span), copy)
}

/// `copy` is whether `Copy` is derived next to `Clone`, which makes cloning a copy for a type
/// that has no type parameters.
fn clone_derive(
    span: Span,
    info: Result<BasicAdtInfo, ExpandError>,
    copy: bool,
) -> ExpandResult<tt::TopSubtree> {
    let krate = &dollar_crate(span);
    let trait_path = group(span, |b| quote_into!(b, span => #krate::clone::Clone));
    expand_simple_derive(span, info, &trait_path, true, true, |b, adt| {
        let copy = copy && adt.param_types.iter().all(|param| param.const_ty.is_some());
        let star = punct('*', tt::Spacing::Alone, span);
        if copy || matches!(adt.shape.kind, AdtKind::Union) {
            return in_group(b, span, |b| {
                quote_into!(b, span =>
                    fn clone(&self) -> Self {
                        #star self
                    }
                )
            });
        }
        if adt.is_empty_enum() {
            return in_group(b, span, |b| {
                quote_into!(b, span =>
                    fn clone(&self) -> Self {
                        match #star self {}
                    }
                )
            });
        }
        let selfs = adt.bindings("_self");
        let clone_field = |b: &mut Builder, i: usize| {
            let bound = &selfs[i];
            // A packed struct's field is a copy out of it, which is cloned by reference.
            in_group(b, span, |b| {
                if adt.packed {
                    quote_into!(b, span => #krate::clone::Clone::clone(&#bound))
                } else {
                    quote_into!(b, span => #krate::clone::Clone::clone(#bound))
                }
            })
        };
        in_group(b, span, |b| {
            quote_into!(b, span => fn clone(&self) -> Self);
            delimited(b, tt::DelimiterKind::Brace, span, |b| {
                adt.write_match_self(b, span, &selfs, |b, v| {
                    adt.write_variant(b, span, v, clone_field)
                })
            });
        });
    })
}

fn default_expand(
    db: &dyn SourceDatabase,
    span: Span,
    tt: &tt::TopSubtree,
) -> ExpandResult<tt::TopSubtree> {
    default_derive(span, parse_adt(db, tt, span))
}

fn default_derive(
    span: Span,
    info: Result<BasicAdtInfo, ExpandError>,
) -> ExpandResult<tt::TopSubtree> {
    let krate = &dollar_crate(span);
    let adt = match info {
        Ok(info) => info,
        Err(e) => {
            return ExpandResult::new(
                tt::TopSubtree::empty(tt::DelimSpan { open: span, close: span }),
                e,
            );
        }
    };
    let (variant, constrain_to_trait) = match &adt.shape.kind {
        AdtKind::Struct => (&adt.shape.variants[0], true),
        AdtKind::Enum { default_variant: Some(d) } => (&adt.shape.variants[*d], false),
        AdtKind::Enum { default_variant: None } => {
            return ExpandResult::new(
                empty(span),
                ExpandError::new(span, ExpandErrorKind::DefaultNoVariant),
            );
        }
        AdtKind::Union => {
            return ExpandResult::new(
                empty(span),
                ExpandError::builtin_derive(span, "this trait cannot be derived for unions"),
            );
        }
    };
    let trait_path = group(span, |b| quote_into!(b, span => #krate::default::Default));
    let no_extra_params = group(span, |_| {});
    ExpandResult::ok(write_derive(span, |b| {
        write_impl(b, span, &adt, &trait_path, constrain_to_trait, &no_extra_params, |b| {
            in_group(b, span, |b| {
                quote_into!(b, span => fn default() -> Self);
                delimited(b, tt::DelimiterKind::Brace, span, |b| {
                    adt.write_variant(b, span, variant, |b, _| {
                        in_group(b, span, |b| {
                            quote_into!(b, span => #krate::default::Default::default())
                        })
                    })
                });
            })
        })
    }))
}

fn debug_expand(
    db: &dyn SourceDatabase,
    span: Span,
    tt: &tt::TopSubtree,
) -> ExpandResult<tt::TopSubtree> {
    debug_derive(span, parse_adt(db, tt, span))
}

fn debug_derive(
    span: Span,
    info: Result<BasicAdtInfo, ExpandError>,
) -> ExpandResult<tt::TopSubtree> {
    let krate = &dollar_crate(span);
    let trait_path = group(span, |b| quote_into!(b, span => #krate::fmt::Debug));
    expand_simple_derive(span, info, &trait_path, false, true, |b, adt| {
        if adt.is_empty_enum() {
            let star = punct('*', tt::Spacing::Alone, span);
            return in_group(b, span, |b| {
                quote_into!(b, span =>
                    fn fmt(&self, f: &mut #krate::fmt::Formatter) -> #krate::fmt::Result {
                        match #star self {}
                    }
                )
            });
        }
        let selfs = adt.bindings("_self");
        // The `&` that rustc's `Debug` puts before the last field of a packed struct, which may
        // be unsized: a `&dyn Debug` is made of a reference to the copy out of it.
        let by_ref = |b: &mut Builder, v: &Variant, i: usize| {
            in_group(b, span, |b| {
                if adt.packed && i + 1 == v.end as usize {
                    b.push(punct('&', tt::Spacing::Alone, span).into());
                }
            })
        };
        // Spelled out in full, as rustc does: a method of the same name on some other trait must
        // not be picked up.
        let for_variant = |b: &mut Builder, v: &Variant| {
            let name = v.name.sym.as_str();
            match v.kind {
                FieldsKind::Record => delimited(b, tt::DelimiterKind::Brace, span, |b| {
                    quote_into!(b, span =>
                        let mut ra_expand_builder = #krate::fmt::Formatter::debug_struct(f, #name);
                    );
                    for i in v.fields() {
                        let field_name = adt.shape.fields[i].sym.as_str();
                        let bound = &selfs[i];
                        let by = group(span, |b| by_ref(b, v, i));
                        in_group(b, span, |b| {
                            quote_into!(b, span =>
                                #krate::fmt::DebugStruct::field(&mut ra_expand_builder, #field_name, & #by #bound);
                            )
                        });
                    }
                    quote_into!(b, span =>
                        #krate::fmt::DebugStruct::finish(&mut ra_expand_builder)
                    );
                }),
                FieldsKind::Tuple => delimited(b, tt::DelimiterKind::Brace, span, |b| {
                    quote_into!(b, span =>
                        let mut ra_expand_builder = #krate::fmt::Formatter::debug_tuple(f, #name);
                    );
                    for i in v.fields() {
                        let bound = &selfs[i];
                        let by = group(span, |b| by_ref(b, v, i));
                        in_group(b, span, |b| {
                            quote_into!(b, span =>
                                #krate::fmt::DebugTuple::field(&mut ra_expand_builder, & #by #bound);
                            )
                        });
                    }
                    quote_into!(b, span =>
                        #krate::fmt::DebugTuple::finish(&mut ra_expand_builder)
                    );
                }),
                FieldsKind::Unit => in_group(b, span, |b| {
                    quote_into!(b, span => #krate::fmt::Formatter::write_str(f, #name))
                }),
            }
        };
        in_group(b, span, |b| {
            quote_into!(b, span => fn fmt(&self, f: &mut #krate::fmt::Formatter) -> #krate::fmt::Result);
            delimited(b, tt::DelimiterKind::Brace, span, |b| {
                adt.write_match_self(b, span, &selfs, for_variant)
            });
        });
    })
}

fn hash_expand(
    db: &dyn SourceDatabase,
    span: Span,
    tt: &tt::TopSubtree,
) -> ExpandResult<tt::TopSubtree> {
    hash_derive(span, parse_adt(db, tt, span))
}

fn hash_derive(
    span: Span,
    info: Result<BasicAdtInfo, ExpandError>,
) -> ExpandResult<tt::TopSubtree> {
    let krate = &dollar_crate(span);
    let trait_path = group(span, |b| quote_into!(b, span => #krate::hash::Hash));
    expand_simple_derive(span, info, &trait_path, false, true, |b, adt| {
        if adt.is_empty_enum() {
            let star = punct('*', tt::Spacing::Alone, span);
            return in_group(b, span, |b| {
                quote_into!(b, span =>
                    fn hash<H: #krate::hash::Hasher>(&self, ra_expand_state: &mut H) {
                        match #star self {}
                    }
                )
            });
        }
        let selfs = adt.bindings("_self");
        in_group(b, span, |b| {
            quote_into!(b, span => fn hash<H: #krate::hash::Hasher>(&self, ra_expand_state: &mut H));
            delimited(b, tt::DelimiterKind::Brace, span, |b| {
                // rustc leaves the discriminant out when there is only one variant.
                in_group(b, span, |b| {
                    if adt.is_enum() && adt.shape.variants.len() > 1 {
                        quote_into!(b, span =>
                            #krate::hash::Hash::hash(&#krate::mem::discriminant(self), ra_expand_state);
                        )
                    }
                });
                adt.write_match_self(b, span, &selfs, |b, v| {
                    delimited(b, tt::DelimiterKind::Brace, span, |b| {
                        for i in v.fields() {
                            let bound = &selfs[i];
                            in_group(b, span, |b| {
                                quote_into!(b, span =>
                                    #krate::hash::Hash::hash(&#bound, ra_expand_state);
                                )
                            });
                        }
                    })
                });
            });
        });
    })
}

fn eq_expand(
    db: &dyn SourceDatabase,
    span: Span,
    tt: &tt::TopSubtree,
) -> ExpandResult<tt::TopSubtree> {
    eq_derive(span, parse_adt(db, tt, span))
}

fn eq_derive(span: Span, info: Result<BasicAdtInfo, ExpandError>) -> ExpandResult<tt::TopSubtree> {
    let krate = &dollar_crate(span);
    let trait_path = group(span, |b| quote_into!(b, span => #krate::cmp::Eq));
    expand_simple_derive(span, info, &trait_path, true, true, |b, _| empty_group(b, span))
}

fn partial_eq_expand(
    db: &dyn SourceDatabase,
    span: Span,
    tt: &tt::TopSubtree,
) -> ExpandResult<tt::TopSubtree> {
    partial_eq_derive(span, parse_adt(db, tt, span))
}

fn partial_eq_derive(
    span: Span,
    info: Result<BasicAdtInfo, ExpandError>,
) -> ExpandResult<tt::TopSubtree> {
    let krate = &dollar_crate(span);
    let trait_path = group(span, |b| quote_into!(b, span => #krate::cmp::PartialEq));
    expand_simple_derive(span, info, &trait_path, false, true, |b, adt| {
        let selfs = adt.bindings("_self");
        let others = adt.bindings("_other");
        let bindings = (&selfs[..], &others[..]);
        // As rustc's: an enum of several variants compares the discriminants, and then the
        // fields of the variants that have any. A match of each variant against itself is a
        // comparison for each, which LLVM leaves as a chain.
        let discriminants = adt.is_enum() && adt.shape.variants.len() > 1;
        let arms = || adt.shape.variants.iter().filter(move |v| !discriminants || !v.is_empty());
        let fields_eq = |b: &mut Builder, v: &Variant| write_fields_eq(b, span, krate, v, bindings);
        if discriminants {
            let fielded = arms().next().is_some();
            return in_group(b, span, |b| {
                quote_into!(b, span => fn eq(&self, other: &Self) -> bool);
                delimited(b, tt::DelimiterKind::Brace, span, |b| {
                    quote_into!(b, span =>
                        let __self_discr = #krate::intrinsics::discriminant_value(self);
                        let __arg1_discr = #krate::intrinsics::discriminant_value(other);
                        __self_discr
                    );
                    eq_eq(b, span);
                    quote_into!(b, span => __arg1_discr);
                    in_group(b, span, |b| {
                        if fielded {
                            and_and(b, span);
                            adt.write_match_both(b, span, bindings, arms(), fields_eq, |b| {
                                in_group(b, span, |b| quote_into!(b, span => true))
                            });
                        }
                    });
                });
            });
        }
        in_group(b, span, |b| {
            quote_into!(b, span => fn eq(&self, other: &Self) -> bool);
            delimited(b, tt::DelimiterKind::Brace, span, |b| {
                adt.write_match_both(b, span, bindings, arms(), fields_eq, |b| {
                    in_group(b, span, |b| quote_into!(b, span => false))
                })
            });
        });
    })
}

/// Whether the fields of `self`'s and `other`'s variant `v` are equal, each with the next. A
/// single comparison gets no second group: a `quote!` of one group is that group.
fn write_fields_eq(
    b: &mut Builder,
    span: Span,
    krate: &tt::Ident,
    v: &Variant,
    (selfs, others): (&[tt::Ident], &[tt::Ident]),
) {
    // By path, as rustc does: a type's own `eq` would be found first.
    let eq = |b: &mut Builder, i: usize| {
        let (t1, t2) = (&selfs[i], &others[i]);
        in_group(b, span, |b| quote_into!(b, span => #krate::cmp::PartialEq::eq(&#t1, &#t2)))
    };
    let fields = v.fields();
    match fields.len() {
        0 => in_group(b, span, |b| quote_into!(b, span => true)),
        1 => eq(b, fields.start),
        _ => in_group(b, span, |b| {
            eq(b, fields.start);
            for i in fields.start + 1..fields.end {
                in_group(b, span, |b| {
                    and_and(b, span);
                    eq(b, i);
                });
            }
        }),
    }
}

/// The start of `match Ord::cmp(&left, &right) { Equal => { .., up to where what follows when
/// they are equal goes; [`write_compare_close`] ends it. `partial` is `PartialOrd`'s.
fn write_compare_open(
    b: &mut Builder,
    span: Span,
    krate: &tt::Ident,
    partial: bool,
    left: impl FnOnce(&mut Builder),
    right: impl FnOnce(&mut Builder),
) {
    b.open(tt::DelimiterKind::Invisible, span);
    if partial {
        quote_into!(b, span => match #krate::cmp::PartialOrd::partial_cmp);
    } else {
        quote_into!(b, span => match #krate::cmp::Ord::cmp);
    }
    delimited(b, tt::DelimiterKind::Parenthesis, span, |b| {
        quote_into!(b, span => &);
        left(b);
        quote_into!(b, span => , &);
        right(b);
    });
    b.open(tt::DelimiterKind::Brace, span);
    write_ordering_equal(b, span, krate, partial, false);
    fat_arrow(b, span);
    b.open(tt::DelimiterKind::Brace, span);
}

/// `} c => return c, }`, the end of [`write_compare_open`].
fn write_compare_close(b: &mut Builder, span: Span) {
    b.close(span);
    quote_into!(b, span => c);
    fat_arrow(b, span);
    quote_into!(b, span => return c,);
    b.close(span);
    b.close(span);
}

/// `Ordering::Equal`, or `Option::Some(Ordering::Equal)` for `partial`, in a group of its own
/// when `grouped`.
fn write_ordering_equal(b: &mut Builder, span: Span, krate: &tt::Ident, partial: bool, grouped: bool) {
    let write = |b: &mut Builder| {
        if partial {
            quote_into!(b, span => #krate::option::Option::Some(#krate::cmp::Ordering::Equal))
        } else {
            quote_into!(b, span => #krate::cmp::Ordering::Equal)
        }
    };
    if grouped { in_group(b, span, write) } else { write(b) }
}

/// The comparison of `self` and `other` of `Ord` (`PartialOrd` for `partial`): the
/// discriminants first (always for `PartialOrd`, for an enum for `Ord`), then the fields of
/// the variants that have any, each in turn.
fn write_ordering(b: &mut Builder, span: Span, krate: &tt::Ident, adt: &BasicAdtInfo, partial: bool) {
    let selfs = adt.bindings("_self");
    let others = adt.bindings("_other");
    let enumeration = adt.is_enum();
    // The discriminants are compared first: a variant without fields is then equal.
    let arms = adt.shape.variants.iter().filter(|v| !enumeration || !v.is_empty());
    let arm = |b: &mut Builder, v: &Variant| {
        for i in v.fields() {
            let (t1, t2) = (&selfs[i], &others[i]);
            write_compare_open(
                b,
                span,
                krate,
                partial,
                |b| in_group(b, span, |b| quote_into!(b, span => #t1)),
                |b| in_group(b, span, |b| quote_into!(b, span => #t2)),
            );
        }
        write_ordering_equal(b, span, krate, partial, true);
        for _ in v.fields() {
            write_compare_close(b, span);
        }
    };
    let discriminants = partial || enumeration;
    let discriminant = |b: &mut Builder, of_self: bool| {
        in_group(b, span, |b| {
            if of_self {
                quote_into!(b, span => #krate::intrinsics::discriminant_value(self))
            } else {
                quote_into!(b, span => #krate::intrinsics::discriminant_value(other))
            }
        })
    };
    if discriminants {
        write_compare_open(b, span, krate, partial, |b| discriminant(b, true), |b| {
            discriminant(b, false)
        });
    }
    adt.write_match_both(b, span, (&selfs[..], &others[..]), arms, arm, |b| {
        write_ordering_equal(b, span, krate, partial, true)
    });
    if discriminants {
        write_compare_close(b, span);
    }
}

fn ord_expand(
    db: &dyn SourceDatabase,
    span: Span,
    tt: &tt::TopSubtree,
) -> ExpandResult<tt::TopSubtree> {
    ord_derive(span, parse_adt(db, tt, span))
}

fn ord_derive(span: Span, info: Result<BasicAdtInfo, ExpandError>) -> ExpandResult<tt::TopSubtree> {
    let krate = &dollar_crate(span);
    let trait_path = group(span, |b| quote_into!(b, span => #krate::cmp::Ord));
    expand_simple_derive(span, info, &trait_path, false, true, |b, adt| {
        in_group(b, span, |b| {
            quote_into!(b, span => fn cmp(&self, other: &Self) -> #krate::cmp::Ordering);
            delimited(b, tt::DelimiterKind::Brace, span, |b| {
                write_ordering(b, span, krate, adt, false)
            });
        })
    })
}

fn partial_ord_expand(
    db: &dyn SourceDatabase,
    span: Span,
    tt: &tt::TopSubtree,
) -> ExpandResult<tt::TopSubtree> {
    partial_ord_derive(span, parse_adt(db, tt, span))
}

fn partial_ord_derive(
    span: Span,
    info: Result<BasicAdtInfo, ExpandError>,
) -> ExpandResult<tt::TopSubtree> {
    let krate = &dollar_crate(span);
    let trait_path = group(span, |b| quote_into!(b, span => #krate::cmp::PartialOrd));
    expand_simple_derive(span, info, &trait_path, false, true, |b, adt| {
        in_group(b, span, |b| {
            quote_into!(b, span =>
                fn partial_cmp(&self, other: &Self) -> #krate::option::Option<#krate::cmp::Ordering>
            );
            delimited(b, tt::DelimiterKind::Brace, span, |b| {
                write_ordering(b, span, krate, adt, true)
            });
        })
    })
}

/// The `DispatchFromDyn` and `CoerceUnsized` impls of `CoercePointee`, each in a group of its
/// own, from the struct with the `__S` bounds added.
fn write_coerce_pointee_impls(
    span: Span,
    info: &BasicAdtInfo,
    self_for_traits: &tt::TopSubtree,
) -> tt::TopSubtree {
    let krate = &dollar_crate(span);
    let dispatch_from_dyn =
        group(span, |b| quote_into!(b, span => #krate::ops::DispatchFromDyn<#self_for_traits>));
    let coerce_unsized =
        group(span, |b| quote_into!(b, span => #krate::ops::CoerceUnsized<#self_for_traits>));
    let added_param = group(span, |b| quote_into!(b, span => __S));
    write_derive(span, |b| {
        in_group(b, span, |b| {
            write_impl(b, span, info, &dispatch_from_dyn, false, &added_param, |b| {
                empty_group(b, span)
            })
        });
        in_group(b, span, |b| {
            write_impl(b, span, info, &coerce_unsized, false, &added_param, |b| {
                empty_group(b, span)
            })
        });
    })
}

fn coerce_pointee_expand(
    db: &dyn SourceDatabase,
    span: Span,
    tt: &tt::TopSubtree,
) -> ExpandResult<tt::TopSubtree> {
    let (adt, _span_map) = match to_adt_syntax(db, tt, span) {
        Ok(it) => it,
        Err(err) => {
            return ExpandResult::new(tt::TopSubtree::empty(tt::DelimSpan::from_single(span)), err);
        }
    };
    let (editor, adt) = SyntaxEditor::with_ast_node(&adt);
    let make = editor.make();
    let ast::Adt::Struct(strukt) = &adt else {
        return ExpandResult::new(
            tt::TopSubtree::empty(tt::DelimSpan::from_single(span)),
            ExpandError::builtin_derive(span, "`CoercePointee` can only be derived on `struct`s"),
        );
    };
    let has_at_least_one_field = strukt
        .field_list()
        .map(|it| match it {
            ast::FieldList::RecordFieldList(it) => it.fields().next().is_some(),
            ast::FieldList::TupleFieldList(it) => it.fields().next().is_some(),
        })
        .unwrap_or(false);
    if !has_at_least_one_field {
        return ExpandResult::new(
            tt::TopSubtree::empty(tt::DelimSpan::from_single(span)),
            ExpandError::builtin_derive(
                span,
                "`CoercePointee` can only be derived on `struct`s with at least one field",
            ),
        );
    }
    let is_repr_transparent = strukt.attrs().any(|attr| {
        attr.as_simple_call().is_some_and(|(name, tt)| {
            name == "repr"
                && tt.syntax().children_with_tokens().any(|it| {
                    it.into_token().is_some_and(|it| {
                        it.kind() == SyntaxKind::IDENT && it.text() == "transparent"
                    })
                })
        })
    });
    if !is_repr_transparent {
        return ExpandResult::new(
            tt::TopSubtree::empty(tt::DelimSpan::from_single(span)),
            ExpandError::builtin_derive(
                span,
                "`CoercePointee` can only be derived on `struct`s with `#[repr(transparent)]`",
            ),
        );
    }
    let type_params = strukt
        .generic_param_list()
        .into_iter()
        .flat_map(|generics| {
            generics.generic_params().filter_map(|param| match param {
                ast::GenericParam::TypeParam(param) => Some(param),
                _ => None,
            })
        })
        .collect_vec();
    if type_params.is_empty() {
        return ExpandResult::new(
            tt::TopSubtree::empty(tt::DelimSpan::from_single(span)),
            ExpandError::builtin_derive(
                span,
                "`CoercePointee` can only be derived on `struct`s that are generic over at least one type",
            ),
        );
    }
    let (pointee_param, pointee_param_idx) = if type_params.len() == 1 {
        // Regardless of the only type param being designed as `#[pointee]` or not, we can just use it as such.
        (type_params[0].clone(), 0)
    } else {
        let mut pointees = type_params.iter().cloned().enumerate().filter(|(_, param)| {
            param.attrs().any(|attr| {
                let is_pointee = attr.as_simple_atom().is_some_and(|name| name == "pointee");
                if is_pointee {
                    // Remove the `#[pointee]` attribute so it won't be present in the generated
                    // impls (where we cannot resolve it).
                    editor.delete(attr.syntax());
                }
                is_pointee
            })
        });
        match (pointees.next(), pointees.next()) {
            (Some((pointee_idx, pointee)), None) => (pointee, pointee_idx),
            (None, _) => {
                return ExpandResult::new(
                    tt::TopSubtree::empty(tt::DelimSpan::from_single(span)),
                    ExpandError::builtin_derive(
                        span,
                        "exactly one generic type parameter must be marked \
                                as `#[pointee]` to derive `CoercePointee` traits",
                    ),
                );
            }
            (Some(_), Some(_)) => {
                return ExpandResult::new(
                    tt::TopSubtree::empty(tt::DelimSpan::from_single(span)),
                    ExpandError::builtin_derive(
                        span,
                        "only one type parameter can be marked as `#[pointee]` \
                                when deriving `CoercePointee` traits",
                    ),
                );
            }
        }
    };
    let (Some(struct_name), Some(pointee_param_name)) = (strukt.name(), pointee_param.name())
    else {
        return ExpandResult::new(
            tt::TopSubtree::empty(tt::DelimSpan::from_single(span)),
            ExpandError::other(span, "invalid item"),
        );
    };

    {
        let mut pointee_has_maybe_sized_bound = false;
        if let Some(bounds) = pointee_param.type_bound_list() {
            pointee_has_maybe_sized_bound |= bounds.bounds().any(is_maybe_sized_bound);
        }
        if let Some(where_clause) = strukt.where_clause() {
            pointee_has_maybe_sized_bound |= where_clause.predicates().any(|pred| {
                let Some(ast::Type::PathType(ty)) = pred.ty() else { return false };
                let is_not_pointee = ty.path().is_none_or(|path| {
                    let is_pointee = path
                        .as_single_name_ref()
                        .is_some_and(|name| name.text() == pointee_param_name.text());
                    !is_pointee
                });
                if is_not_pointee {
                    return false;
                }
                pred.type_bound_list()
                    .is_some_and(|bounds| bounds.bounds().any(is_maybe_sized_bound))
            })
        }
        if !pointee_has_maybe_sized_bound {
            return ExpandResult::new(
                tt::TopSubtree::empty(tt::DelimSpan::from_single(span)),
                ExpandError::builtin_derive(
                    span,
                    format!(
                        "`derive(CoercePointee)` requires `{pointee_param_name}` to be marked `?Sized`"
                    ),
                ),
            );
        }
    }

    const ADDED_PARAM: &str = "__S";

    let mut new_predicates: Vec<ast::WherePred> = Vec::new();

    {
        // # Rewrite generic parameter bounds
        // For each bound `U: ..` in `struct<U: ..>`, make a new bound with `__S` in place of `#[pointee]`
        // Example:
        // ```
        // struct<
        //     U: Trait<T>,
        //     #[pointee] T: Trait<T> + ?Sized,
        //     V: Trait<T>> ...
        // ```
        // ... generates this `impl` generic parameters
        // ```
        // impl<
        //     U: Trait<T>,
        //     T: Trait<T> + ?Sized,
        //     V: Trait<T>
        // >
        // where
        //     U: Trait<__S>,
        //     __S: Trait<__S> + ?Sized,
        //     V: Trait<__S> ...
        // ```
        for param in &type_params {
            let Some(param_name) = param.name() else { continue };
            if let Some(bounds) = param.type_bound_list() {
                // If the target type is the pointee, duplicate the bound as whole.
                // Otherwise, duplicate only bounds that mention the pointee.
                let is_pointee = param_name.text() == pointee_param_name.text();
                let new_bounds = bounds.bounds().filter_map(|bound| {
                    let new_bound = substitute_type_bound(
                        bound.clone(),
                        pointee_param_name.text(),
                        ADDED_PARAM,
                    );

                    if is_pointee {
                        return new_bound.or(Some(bound));
                    }
                    new_bound
                });

                let new_bounds_target = if is_pointee {
                    make.name_ref(ADDED_PARAM)
                } else {
                    make.name_ref(param_name.text())
                };
                new_predicates.push(make.where_pred(
                    Either::Right(
                        make.ty_path_from_segments([make.path_segment(new_bounds_target)], false),
                    ),
                    new_bounds,
                ));
            }
        }

        // # Rewrite `where` clauses
        //
        // Move on to `where` clauses.
        // Example:
        // ```
        // struct MyPointer<#[pointee] T, ..>
        // where
        //   U: Trait<V> + Trait<T>,
        //   Companion<T>: Trait<T>,
        //   T: Trait<T> + ?Sized,
        // { .. }
        // ```
        // ... will have a impl prelude like so
        // ```
        // impl<..> ..
        // where
        //   U: Trait<V> + Trait<T>,
        //   U: Trait<__S>,
        //   Companion<T>: Trait<T>,
        //   Companion<__S>: Trait<__S>,
        //   T: Trait<T> + ?Sized,
        //   __S: Trait<__S> + ?Sized,
        // ```
        //
        // We should also write a few new `where` bounds from `#[pointee] T` to `__S`
        // as well as any bound that indirectly involves the `#[pointee] T` type.
        for predicate in strukt.where_clause().into_iter().flat_map(|wc| wc.predicates()) {
            let Some(pred_target) = predicate.ty() else { continue };

            // If the target type references the pointee, duplicate the bound as whole.
            // Otherwise, duplicate only bounds that mention the pointee.
            if let Some(predicate_with_substituted_target) =
                substitute_where_pred(&predicate, pointee_param_name.text(), ADDED_PARAM)
            {
                new_predicates.push(predicate_with_substituted_target);
            } else if let Some(bounds) = predicate.type_bound_list() {
                let new_bounds = bounds.bounds().filter_map(|bound| {
                    substitute_type_bound(bound, pointee_param_name.text(), ADDED_PARAM)
                });
                new_predicates.push(make.where_pred(Either::Right(pred_target), new_bounds));
            }
        }
    }

    {
        // # Add `Unsize<__S>` bound to `#[pointee]` at the generic parameter location
        //
        // Find the `#[pointee]` parameter and add an `Unsize<__S>` bound to it.
        new_predicates.push(
            make.where_pred(
                Either::Right(make.ty_path_from_segments(
                    [make.path_segment(make.name_ref(pointee_param_name.text()))],
                    false,
                )),
                [make.type_bound(
                    make.ty_path_from_segments(
                        [
                            make.path_segment(make.name_ref("core")),
                            make.path_segment(make.name_ref("marker")),
                            make.generic_ty_path_segment(
                                make.name_ref("Unsize"),
                                [make
                                    .type_arg(make.ty_path_from_segments(
                                        [make.path_segment(make.name_ref(ADDED_PARAM))],
                                        false,
                                    ))
                                    .into()],
                            ),
                        ],
                        true,
                    ),
                )],
            ),
        );
    }

    let self_for_traits = {
        // Replace the `#[pointee]` with `__S`.
        let mut type_param_idx = 0;
        let self_params_for_traits = strukt
            .generic_param_list()
            .into_iter()
            .flat_map(|params| params.generic_params())
            .filter_map(|param| {
                Some(match param {
                    ast::GenericParam::ConstParam(param) => {
                        ast::GenericArg::ConstArg(make.expr_const_value(param.name()?.text()))
                    }
                    ast::GenericParam::LifetimeParam(param) => {
                        make.lifetime_arg(param.lifetime()?).into()
                    }
                    ast::GenericParam::TypeParam(param) => {
                        let name = if pointee_param_idx == type_param_idx {
                            make.name_ref(ADDED_PARAM)
                        } else {
                            make.name_ref(param.name()?.text())
                        };
                        type_param_idx += 1;
                        make.type_arg(make.ty_path_from_segments([make.path_segment(name)], false))
                            .into()
                    }
                })
            });

        make.path_from_segments(
            [make.generic_ty_path_segment(
                make.name_ref(struct_name.text()),
                self_params_for_traits,
            )],
            false,
        )
    };

    strukt.get_or_create_where_clause(&editor, new_predicates.into_iter());
    let edit = editor.finish();
    let strukt = ast::Struct::cast(edit.new_root().clone()).unwrap();
    let adt = ast::Adt::Struct(strukt.clone());

    let mut span_map = span::SpanMap::empty();
    // One span for them all.
    span_map.push(adt.syntax().text_range().end(), span);

    let self_for_traits = syntax_bridge::syntax_node_to_token_tree(
        self_for_traits.syntax(),
        &span_map,
        span,
        DocCommentDesugarMode::ProcMacro,
    );
    let info = match parse_adt_from_syntax(&adt, &span_map, span) {
        Ok(it) => it,
        Err(err) => {
            return ExpandResult::new(tt::TopSubtree::empty(tt::DelimSpan::from_single(span)), err);
        }
    };

    return ExpandResult::ok(write_coerce_pointee_impls(span, &info, &self_for_traits));

    fn is_maybe_sized_bound(bound: ast::TypeBound) -> bool {
        if bound.question_mark_token().is_none() {
            return false;
        }
        let Some(ast::Type::PathType(ty)) = bound.ty() else {
            return false;
        };
        let Some(path) = ty.path() else {
            return false;
        };
        return segments_eq(&path, &["Sized"])
            || segments_eq(&path, &["core", "marker", "Sized"])
            || segments_eq(&path, &["std", "marker", "Sized"]);

        fn segments_eq(path: &ast::Path, expected: &[&str]) -> bool {
            path.segments().zip_longest(expected.iter().copied()).all(|value| {
                value.both().is_some_and(|(segment, expected)| {
                    segment.name_ref().is_some_and(|name| name.text() == expected)
                })
            })
        }
    }

    /// Returns true if any substitution was performed.
    fn substitute_type_bound(
        bound: ast::TypeBound,
        param_name: &str,
        replacement: &str,
    ) -> Option<ast::TypeBound> {
        let (editor, bound) = SyntaxEditor::with_ast_node(&bound);
        let substituted = bound
            .ty()
            .is_some_and(|ty| substitute_type_in_bound(&editor, ty, param_name, replacement));
        if !substituted {
            return None;
        }

        let edit = editor.finish();
        Some(ast::TypeBound::cast(edit.new_root().clone()).unwrap())
    }

    fn substitute_where_pred(
        predicate: &ast::WherePred,
        param_name: &str,
        replacement: &str,
    ) -> Option<ast::WherePred> {
        let (editor, predicate) = SyntaxEditor::with_ast_node(predicate);
        let substituted = predicate
            .ty()
            .is_some_and(|ty| substitute_type_in_bound(&editor, ty, param_name, replacement));
        if substituted && let Some(bounds) = predicate.type_bound_list() {
            for bound in bounds.bounds() {
                if let Some(ty) = bound.ty() {
                    substitute_type_in_bound(&editor, ty, param_name, replacement);
                }
            }
        }
        if !substituted {
            return None;
        }

        let edit = editor.finish();
        Some(ast::WherePred::cast(edit.new_root().clone()).unwrap())
    }

    fn substitute_type_in_bound(
        editor: &SyntaxEditor,
        ty: ast::Type,
        param_name: &str,
        replacement: &str,
    ) -> bool {
        let make = editor.make();
        return match ty {
            ast::Type::ArrayType(ty) => ty
                .ty()
                .is_some_and(|ty| substitute_type_in_bound(editor, ty, param_name, replacement)),
            ast::Type::DynTraitType(ty) => {
                go_bounds(editor, ty.type_bound_list(), param_name, replacement)
            }
            ast::Type::FnPtrType(ty) => any_long(
                ty.param_list()
                    .into_iter()
                    .flat_map(|params| params.params().filter_map(|param| param.ty()))
                    .chain(ty.ret_type().and_then(|it| it.ty())),
                |ty| substitute_type_in_bound(editor, ty, param_name, replacement),
            ),
            ast::Type::ForType(ty) => ty
                .ty()
                .is_some_and(|ty| substitute_type_in_bound(editor, ty, param_name, replacement)),
            ast::Type::ImplTraitType(ty) => {
                go_bounds(editor, ty.type_bound_list(), param_name, replacement)
            }
            ast::Type::ParenType(ty) => ty
                .ty()
                .is_some_and(|ty| substitute_type_in_bound(editor, ty, param_name, replacement)),
            ast::Type::PathType(ty) => ty.path().is_some_and(|path| {
                if path.as_single_name_ref().is_some_and(|name| name.text() == param_name) {
                    editor.replace(
                        path.syntax(),
                        make.path_from_segments(
                            [make.path_segment(make.name_ref(replacement))],
                            false,
                        )
                        .syntax(),
                    );
                    return true;
                }

                any_long(
                    path.segments()
                        .filter_map(|segment| segment.generic_arg_list())
                        .flat_map(|it| it.generic_args())
                        .filter_map(|generic_arg| match generic_arg {
                            ast::GenericArg::TypeArg(ty) => ty.ty(),
                            _ => None,
                        }),
                    |ty| substitute_type_in_bound(editor, ty, param_name, replacement),
                )
            }),
            ast::Type::PtrType(ty) => ty
                .ty()
                .is_some_and(|ty| substitute_type_in_bound(editor, ty, param_name, replacement)),
            ast::Type::RefType(ty) => ty
                .ty()
                .is_some_and(|ty| substitute_type_in_bound(editor, ty, param_name, replacement)),
            ast::Type::SliceType(ty) => ty
                .ty()
                .is_some_and(|ty| substitute_type_in_bound(editor, ty, param_name, replacement)),
            ast::Type::TupleType(ty) => any_long(ty.fields(), |ty| {
                substitute_type_in_bound(editor, ty, param_name, replacement)
            }),
            ast::Type::PatternType(ty) => ty
                .ty()
                .is_some_and(|ty| substitute_type_in_bound(editor, ty, param_name, replacement)),
            ast::Type::InferType(_) | ast::Type::MacroType(_) | ast::Type::NeverType(_) => false,
        };

        fn go_bounds(
            editor: &SyntaxEditor,
            bounds: Option<ast::TypeBoundList>,
            param_name: &str,
            replacement: &str,
        ) -> bool {
            bounds.is_some_and(|bounds| {
                any_long(bounds.bounds(), |bound| {
                    bound.ty().is_some_and(|ty| {
                        substitute_type_in_bound(editor, ty, param_name, replacement)
                    })
                })
            })
        }

        /// Like [`Iterator::any()`], but not short-circuiting.
        fn any_long<I: Iterator, F: FnMut(I::Item) -> bool>(iter: I, mut f: F) -> bool {
            let mut result = false;
            iter.for_each(|item| result |= f(item));
            result
        }
    }
}
