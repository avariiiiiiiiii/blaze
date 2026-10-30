//! E0107: a path or a method call given more or fewer generic arguments than its definition
//! takes ("struct takes 2 generic arguments but 1 generic argument was supplied", "missing
//! generics for struct `Ty`"), with the labels, suggestions and note rustc builds; and what the
//! generics kinds share: the segment a diagnostic is of, how rustc names a definition, its
//! parameters and where they are written, and the item a path is written in.
//!
//! Follows rustc's `compiler/rustc_hir_analysis/src/hir_ty_lowering/generics.rs`
//! (`check_generic_arg_count`), `compiler/rustc_hir_analysis/src/diagnostics/
//! wrong_number_of_generic_args.rs` (`WrongNumberOfGenericArgs`), `compiler/rustc_middle/src/
//! ty/util.rs` (`def_kind_descr`), `compiler/rustc_hir/src/def.rs` (`DefKind::descr`) and
//! `compiler/rustc_ast/src/ast.rs` (`GenericParam::span`) (MIT or Apache-2.0).

use blaze_items::{Fact, ItemId};
use either::Either;
use hir::{
    Adt, AsAssocItem as _, AssocItemContainer, GenericDef, HasSource as _, HirFileId, InFile,
    IncorrectGenericsLenKind,
};
use hir_def::{
    ImplId, TraitId,
    signatures::{TraitFlags, TraitSignature},
};
use syntax::{
    AstNode, AstPtr, SyntaxNode, SyntaxNodePtr, TextRange,
    ast::{self, HasArgList as _, HasGenericArgs as _, HasName as _},
};

use crate::{
    cx::Cx,
    lints::code_fact,
    lower::{Found, Phase},
    model::{Applicability, Diag, MultiSpan, Span},
};

/// The path segment, method call or associated type binding generic arguments are given to.
pub(crate) struct Segment {
    pub(crate) file: HirFileId,
    /// Its name: the segment's, the method's or the associated type's.
    pub(crate) name: ast::NameRef,
    /// Its arguments in angle brackets, when it has them.
    pub(crate) list: Option<ast::GenericArgList>,
    /// The segment, the method call or the binding.
    pub(crate) node: SyntaxNode,
}

impl Segment {
    /// The segment hir's diagnostics point to by its arguments, or by its name when it has none.
    pub(crate) fn of(
        cx: &Cx<'_, '_>,
        ptr: InFile<AstPtr<Either<ast::GenericArgList, ast::NameRef>>>,
    ) -> Option<Segment> {
        let node = cx.node(ptr);
        let file = node.file_id;
        let (list, owner) = match node.value {
            Either::Left(list) => {
                let owner = list.syntax().parent()?;
                (Some(list), owner)
            }
            Either::Right(name) => (None, name.syntax().parent()?),
        };
        Segment::of_node(file, owner, list)
    }

    /// The segment `node` is: a path segment, a method call or an associated type binding.
    pub(crate) fn of_node(
        file: HirFileId,
        node: SyntaxNode,
        list: Option<ast::GenericArgList>,
    ) -> Option<Segment> {
        let name = if let Some(it) = ast::PathSegment::cast(node.clone()) {
            it.name_ref()?
        } else if let Some(it) = ast::MethodCallExpr::cast(node.clone()) {
            it.name_ref()?
        } else {
            ast::AssocTypeArg::cast(node.clone())?.name_ref()?
        };
        let list = match list {
            Some(it) => Some(it),
            None => generic_arg_list(&node),
        };
        Some(Segment { file, name, list, node })
    }

    pub(crate) fn span(&self, cx: &Cx<'_, '_>, range: TextRange) -> Span {
        cx.span(InFile::new(self.file, range))
    }

    pub(crate) fn span_of(&self, cx: &Cx<'_, '_>, node: &impl AstNode) -> Span {
        self.span(cx, node.syntax().text_range())
    }

    pub(crate) fn name_span(&self, cx: &Cx<'_, '_>) -> Span {
        self.span_of(cx, &self.name)
    }

    pub(crate) fn name_text(&self) -> String {
        self.name.text().to_string()
    }

    /// Its generic arguments, without the associated item constraints: rustc's
    /// `GenericArgs::args`.
    pub(crate) fn args(&self) -> Vec<ast::GenericArg> {
        let Some(list) = &self.list else { return Vec::new() };
        list.generic_args().filter(|it| !matches!(it, ast::GenericArg::AssocTypeArg(_))).collect()
    }

    /// Its associated item constraints: rustc's `GenericArgs::constraints`.
    pub(crate) fn constraints(&self) -> Vec<ast::AssocTypeArg> {
        let Some(list) = &self.list else { return Vec::new() };
        list.generic_args()
            .filter_map(|it| match it {
                ast::GenericArg::AssocTypeArg(it) => Some(it),
                _ => None,
            })
            .collect()
    }

    /// The `Fn`-style arguments of a path segment, `Trait(A) -> B`.
    pub(crate) fn parenthesized(&self) -> Option<ast::ParenthesizedArgList> {
        ast::PathSegment::cast(self.node.clone())?.parenthesized_arg_list()
    }

    /// rustc's `GenericArgs::span_ext`: the arguments with their brackets, from `<` to `>` (the
    /// `::` of a turbofish left out), or the parentheses and the return type of `Fn`-style
    /// ones.
    pub(crate) fn span_ext(&self) -> Option<TextRange> {
        if let Some(list) = &self.list {
            let start = match list.l_angle_token() {
                Some(it) => it.text_range().start(),
                None => list.syntax().text_range().start(),
            };
            return Some(TextRange::new(start, list.syntax().text_range().end()));
        }
        let args = self.parenthesized()?;
        let end = match ast::PathSegment::cast(self.node.clone()).and_then(|it| it.ret_type()) {
            Some(ret) => ret.syntax().text_range().end(),
            None => args.syntax().text_range().end(),
        };
        Some(TextRange::new(args.syntax().text_range().start(), end))
    }

    /// rustc's `GenericArgs::span`: what is between the angle brackets.
    pub(crate) fn inner_range(&self) -> Option<TextRange> {
        let list = self.list.as_ref()?;
        let start = list.l_angle_token()?.text_range().end();
        let end = match list.r_angle_token() {
            Some(it) => it.text_range().start(),
            None => list.syntax().text_range().end(),
        };
        Some(TextRange::new(start, end.max(start)))
    }

    /// The whole path the segment is of, when it is a path segment.
    pub(crate) fn path(&self) -> Option<ast::Path> {
        Some(ast::PathSegment::cast(self.node.clone())?.parent_path().top_path())
    }
}

fn generic_arg_list(node: &SyntaxNode) -> Option<ast::GenericArgList> {
    if let Some(it) = ast::PathSegment::cast(node.clone()) {
        return it.generic_arg_list();
    }
    if let Some(it) = ast::MethodCallExpr::cast(node.clone()) {
        return it.generic_arg_list();
    }
    ast::AssocTypeArg::cast(node.clone())?.generic_arg_list()
}

/// rustc's `def_descr` of a definition: "struct", "trait", "method", "associated type", ..
pub(crate) fn def_descr(cx: &Cx<'_, '_>, def: GenericDef) -> &'static str {
    let db = cx.db;
    match def {
        GenericDef::Function(it) => match it.as_assoc_item(db) {
            Some(_) if it.has_self_param(db) => "method",
            Some(_) => "associated function",
            None => "function",
        },
        GenericDef::Adt(Adt::Struct(_)) => "struct",
        GenericDef::Adt(Adt::Enum(_)) => "enum",
        GenericDef::Adt(Adt::Union(_)) => "union",
        GenericDef::Trait(it) => {
            match TraitSignature::of(db, TraitId::from(it)).flags.contains(TraitFlags::ALIAS) {
                true => "trait alias",
                false => "trait",
            }
        }
        GenericDef::TypeAlias(it) => match it.as_assoc_item(db) {
            Some(_) => "associated type",
            None => "type alias",
        },
        GenericDef::Impl(_) => "implementation",
        GenericDef::Const(it) => match it.as_assoc_item(db) {
            Some(_) => "associated constant",
            None => "constant",
        },
        GenericDef::Static(_) => "static",
    }
}

/// The definition as an item of hir, for paths and facts.
pub(crate) fn module_def(def: GenericDef) -> Option<hir::ModuleDef> {
    Some(match def {
        GenericDef::Function(it) => it.into(),
        GenericDef::Adt(it) => it.into(),
        GenericDef::Trait(it) => it.into(),
        GenericDef::TypeAlias(it) => it.into(),
        GenericDef::Const(it) => it.into(),
        GenericDef::Static(it) => it.into(),
        GenericDef::Impl(_) => return None,
    })
}

/// rustc's `def_path_str` of the definition.
pub(crate) fn def_path(cx: &Cx<'_, '_>, def: GenericDef) -> String {
    match module_def(def) {
        Some(it) => cx.def_path(it),
        None => def_descr(cx, def).to_owned(),
    }
}

/// The definition's name as written, for "`Ty`" in a fact.
pub(crate) fn def_name(cx: &Cx<'_, '_>, def: GenericDef) -> String {
    match def.name(cx.db) {
        Some(it) => it.display(cx.db, cx.edition).to_string(),
        None => def_descr(cx, def).to_owned(),
    }
}

pub(crate) fn item_id(def: GenericDef) -> Option<ItemId> {
    Some(match def {
        GenericDef::Function(it) => ItemId::Function(hir_def::FunctionId::try_from(it).ok()?),
        GenericDef::Adt(it) => ItemId::Adt(it.into()),
        GenericDef::Trait(it) => ItemId::Trait(it.into()),
        GenericDef::TypeAlias(it) => ItemId::Alias(it.into()),
        GenericDef::Impl(it) => ItemId::Impl(ImplId::try_from(it).ok()?),
        GenericDef::Const(it) => ItemId::Const(it.into()),
        GenericDef::Static(it) => ItemId::Static(it.into()),
    })
}

/// Records the fact of the generics of `def`: what changes when its parameters do.
pub(crate) fn generics_fact(cx: &Cx<'_, '_>, diag: &mut Diag, def: GenericDef) {
    if let Some(item) = item_id(def) {
        diag.fact(Fact::Signature(cx.key(item)), format!("the generics of `{}`", def_name(cx, def)));
    }
}

/// rustc's `def_ident_span`: the definition's name where it is written.
pub(crate) fn def_name_span(cx: &Cx<'_, '_>, def: GenericDef) -> Option<Span> {
    let db = cx.db;
    fn named<N: ast::HasName>(it: InFile<N>) -> Option<InFile<TextRange>> {
        let name = it.value.name()?;
        Some(it.with_value(name.syntax().text_range()))
    }
    let at = match def {
        GenericDef::Function(it) => named(it.source(db)?),
        GenericDef::Adt(it) => named(it.source(db)?),
        GenericDef::Trait(it) => named(it.source(db)?),
        GenericDef::TypeAlias(it) => named(it.source(db)?),
        GenericDef::Const(it) => named(it.source(db)?),
        GenericDef::Static(it) => named(it.source(db)?),
        GenericDef::Impl(_) => None,
    }?;
    Some(cx.span(at))
}

/// A generic parameter of a definition, as rustc's notes show it.
pub(crate) struct Param {
    pub(crate) name: String,
    /// rustc's `def_span` of it (`GenericParam::span`): a lifetime's or a type's name, to the
    /// end of its default when it has one; a const parameter whole.
    pub(crate) span: Option<Span>,
    pub(crate) is_type: bool,
    pub(crate) has_default: bool,
}

/// The lifetime parameters of `def`, in order.
pub(crate) fn lifetime_params(cx: &Cx<'_, '_>, def: GenericDef) -> Vec<Param> {
    let db = cx.db;
    def.lifetime_params(db)
        .into_iter()
        .map(|param| {
            let span = param.source(db).and_then(|src| {
                let lifetime = src.value.lifetime()?;
                Some(cx.span(src.with_value(lifetime.syntax().text_range())))
            });
            Param {
                name: param.name(db).display(db, cx.edition).to_string(),
                span,
                is_type: false,
                has_default: false,
            }
        })
        .collect()
}

/// The type and const parameters of `def` that are written in its generics, in order: not
/// a trait's `Self` nor an `impl Trait` argument's.
pub(crate) fn type_or_const_params(cx: &Cx<'_, '_>, def: GenericDef) -> Vec<Param> {
    let db = cx.db;
    def.type_or_const_params(db)
        .into_iter()
        .filter(|param| match param.split(db) {
            Either::Left(_) => true,
            Either::Right(it) => !it.is_implicit(db),
        })
        .map(|param| {
            let name = param.name(db).display(db, cx.edition).to_string();
            let source = param.source(db);
            let (range, is_type, has_default) = match source.as_ref().map(|it| &it.value) {
                Some(Either::Left(ast::TypeOrConstParam::Type(it))) => {
                    let start = match it.name() {
                        Some(name) => name.syntax().text_range().start(),
                        None => it.syntax().text_range().start(),
                    };
                    let range = match it.name() {
                        Some(name) => name.syntax().text_range(),
                        None => it.syntax().text_range(),
                    };
                    let range = match it.default_type() {
                        Some(default) => TextRange::new(start, default.syntax().text_range().end()),
                        None => range,
                    };
                    (Some(range), true, it.default_type().is_some())
                }
                Some(Either::Left(ast::TypeOrConstParam::Const(it))) => {
                    let start = match it.const_token() {
                        Some(token) => token.text_range().start(),
                        None => it.syntax().text_range().start(),
                    };
                    let range = TextRange::new(start, it.syntax().text_range().end());
                    (Some(range), false, it.default_val().is_some())
                }
                _ => (None, matches!(param.split(db), Either::Right(_)), false),
            };
            let span = match (source, range) {
                (Some(src), Some(range)) => Some(cx.span(src.with_value(range))),
                _ => None,
            };
            Param { name, span, is_type, has_default }
        })
        .collect()
}

/// The number of `impl Trait` arguments of `def`: rustc's `synth_type_param_count`.
fn synthetic_params(cx: &Cx<'_, '_>, def: GenericDef) -> usize {
    if matches!(def, GenericDef::Trait(_)) {
        return 0;
    }
    let db = cx.db;
    def.type_or_const_params(db)
        .into_iter()
        .filter(|param| matches!(param.split(db), Either::Right(it) if it.is_implicit(db)))
        .count()
}

/// Where a path is written: in the body of an item, or in the signature of one.
pub(crate) enum Site {
    Body,
    Signature,
}

/// Whether `node` is in a body (of a function, a const, a static, or an anonymous const in
/// a signature) or in an item's signature.
pub(crate) fn site(node: &SyntaxNode) -> Site {
    let range = node.text_range();
    for ancestor in node.ancestors() {
        if ast::ConstArg::can_cast(ancestor.kind()) || ast::BlockExpr::can_cast(ancestor.kind()) {
            return Site::Body;
        }
        let Some(item) = ast::Item::cast(ancestor) else { continue };
        let body = match &item {
            ast::Item::Fn(it) => it.body().map(|it| it.syntax().text_range()),
            ast::Item::Const(it) => it.body().map(|it| it.syntax().text_range()),
            ast::Item::Static(it) => it.body().map(|it| it.syntax().text_range()),
            _ => None,
        };
        return match body.is_some_and(|body| body.contains_range(range)) {
            true => Site::Body,
            false => Site::Signature,
        };
    }
    Site::Signature
}

/// The pass rustc gives an error of generic arguments in: the checks of items for a path in a
/// signature, type checking for one in a body.
pub(crate) fn phase_at(node: &SyntaxNode) -> Phase {
    match site(node) {
        Site::Body => Phase::Bodies,
        Site::Signature => Phase::Items,
    }
}

/// Records the fact of where `at` is written: the code of the body it is in, or the signature
/// of the item.
pub(crate) fn site_fact(cx: &Cx<'_, '_>, diag: &mut Diag, at: InFile<&SyntaxNode>) {
    if let Site::Body = site(at.value) {
        code_fact(cx, diag, at.map(SyntaxNodePtr::new));
        return;
    }
    let Some(item) = at.value.ancestors().find_map(ast::Item::cast) else { return };
    if let Some((id, what)) = item_of(cx, at.with_value(&item)) {
        diag.fact(Fact::Signature(cx.key(id)), what);
    }
}

/// The item `item` is, and its signature in words.
fn item_of(cx: &Cx<'_, '_>, item: InFile<&ast::Item>) -> Option<(ItemId, String)> {
    let sema = cx.sema;
    let named = |name: Option<ast::Name>| match name {
        Some(name) => format!("the signature of `{}`", name.text()),
        None => "the signature of `_`".to_owned(),
    };
    Some(match item.value {
        ast::Item::Fn(it) => {
            let def = sema.to_def2(item.with_value(it))?;
            (ItemId::Function(hir_def::FunctionId::try_from(def).ok()?), named(it.name()))
        }
        ast::Item::Struct(it) => {
            (ItemId::Adt(Adt::from(sema.to_def2(item.with_value(it))?).into()), named(it.name()))
        }
        ast::Item::Enum(it) => {
            (ItemId::Adt(Adt::from(sema.to_def2(item.with_value(it))?).into()), named(it.name()))
        }
        ast::Item::Union(it) => {
            (ItemId::Adt(Adt::from(sema.to_def2(item.with_value(it))?).into()), named(it.name()))
        }
        ast::Item::Trait(it) => (ItemId::Trait(sema.to_def2(item.with_value(it))?.into()), named(it.name())),
        ast::Item::TypeAlias(it) => {
            (ItemId::Alias(sema.to_def2(item.with_value(it))?.into()), named(it.name()))
        }
        ast::Item::Const(it) => (ItemId::Const(sema.to_def2(item.with_value(it))?.into()), named(it.name())),
        ast::Item::Static(it) => (ItemId::Static(sema.to_def2(item.with_value(it))?.into()), named(it.name())),
        ast::Item::Impl(it) => {
            let def = sema.to_def2(item.with_value(it))?;
            (ItemId::Impl(ImplId::try_from(def).ok()?), "the header of the impl".to_owned())
        }
        _ => return None,
    })
}

/// `"s"` when `n` is not 1: rustc's `pluralize!`.
pub(crate) fn s(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

/// `"was"` or `"were"`: rustc's `pluralize!("was", n)`.
fn was(n: usize) -> &'static str {
    if n == 1 { "was" } else { "were" }
}

/// rustc's `GenericArgsInfo`: which arguments are missing or too many.
#[derive(Clone, Copy, Debug)]
enum Info {
    MissingLifetimes { missing: usize },
    ExcessLifetimes { redundant: usize },
    MissingTypesOrConsts { missing: usize, defaults: usize },
    ExcessTypesOrConsts { redundant: usize, defaults: usize, synth_provided: bool },
}

/// rustc's `WrongNumberOfGenericArgs`, of what hir found and the syntax of the segment.
struct Wrong<'a, 'b, 'db> {
    cx: &'a Cx<'b, 'db>,
    seg: &'a Segment,
    def: GenericDef,
    /// The const parameter the arguments are given to, which `def` is the parent of.
    param: Option<hir::ConstParam>,
    info: Info,
    /// The arguments written, constraints left out.
    args: &'a [ast::GenericArg],
    /// `def`'s lifetime and written type or const parameters, read once per error.
    lifetimes: &'a [Param],
    types: &'a [Param],
}

impl Wrong<'_, '_, '_> {
    fn descr(&self) -> &'static str {
        match self.param {
            Some(_) => "const parameter",
            None => def_descr(self.cx, self.def),
        }
    }

    fn def_span(&self) -> Option<Span> {
        let Some(param) = self.param else { return def_name_span(self.cx, self.def) };
        let src = param.merge().source(self.cx.db)?;
        let Either::Left(ast::TypeOrConstParam::Const(it)) = &src.value else { return None };
        Some(self.cx.span(src.with_value(it.name()?.syntax().text_range())))
    }

    fn missing_lifetimes(&self) -> bool {
        matches!(self.info, Info::MissingLifetimes { .. } | Info::ExcessLifetimes { .. })
    }

    fn kind(&self) -> &'static str {
        if self.missing_lifetimes() { "lifetime" } else { "generic" }
    }

    /// Angle brackets or `Fn`-style parentheses are written.
    fn has_brackets(&self) -> bool {
        self.seg.span_ext().is_some()
    }

    fn parenthesized(&self) -> bool {
        self.seg.list.is_none() && self.seg.parenthesized().is_some()
    }

    fn num_provided_lifetime_args(&self) -> usize {
        self.args.iter().filter(|it| matches!(it, ast::GenericArg::LifetimeArg(_))).count()
    }

    fn num_provided_type_or_const_args(&self) -> usize {
        if self.parenthesized() {
            return 1;
        }
        self.args.iter().filter(|it| !matches!(it, ast::GenericArg::LifetimeArg(_))).count()
    }

    fn num_provided_args(&self) -> usize {
        match self.missing_lifetimes() {
            true => self.num_provided_lifetime_args(),
            false => self.num_provided_type_or_const_args(),
        }
    }

    fn num_expected_lifetime_args(&self) -> usize {
        let provided = self.num_provided_lifetime_args();
        match self.info {
            Info::MissingLifetimes { missing } => provided + missing,
            Info::ExcessLifetimes { redundant } => provided - redundant,
            _ => 0,
        }
    }

    fn num_expected_type_or_const_args(&self) -> usize {
        let provided = self.num_provided_type_or_const_args();
        match self.info {
            Info::MissingTypesOrConsts { missing, .. } => provided + missing,
            Info::ExcessTypesOrConsts { redundant, .. } => provided - redundant,
            _ => 0,
        }
    }

    fn num_expected_type_or_const_args_including_defaults(&self) -> usize {
        let provided = self.num_provided_type_or_const_args();
        match self.info {
            Info::MissingTypesOrConsts { missing, defaults } => provided + missing - defaults,
            Info::ExcessTypesOrConsts { redundant, defaults, .. } => provided - redundant - defaults,
            _ => 0,
        }
    }

    fn num_excess_lifetime_args(&self) -> usize {
        match self.info {
            Info::ExcessLifetimes { redundant } => redundant,
            _ => 0,
        }
    }

    fn num_excess_type_or_const_args(&self) -> usize {
        match self.info {
            Info::ExcessTypesOrConsts { redundant, .. } => redundant,
            _ => 0,
        }
    }

    fn too_many_args_provided(&self) -> bool {
        matches!(self.info, Info::ExcessLifetimes { .. } | Info::ExcessTypesOrConsts { .. })
    }

    /// Where in the arguments the types and consts start: after the lifetimes written.
    fn lifetime_args_offset(&self) -> usize {
        match self.missing_lifetimes() {
            true => 0,
            false => self.num_provided_lifetime_args(),
        }
    }

    fn num_default_params(&self) -> usize {
        match self.info {
            Info::MissingTypesOrConsts { defaults, .. } | Info::ExcessTypesOrConsts { defaults, .. } => {
                defaults
            }
            _ => 0,
        }
    }

    fn quantifier_and_bound(&self) -> (&'static str, usize) {
        if self.num_default_params() == 0 {
            return match self.missing_lifetimes() {
                true => ("", self.num_expected_lifetime_args()),
                false => ("", self.num_expected_type_or_const_args()),
            };
        }
        match self.info {
            Info::MissingLifetimes { .. } => ("at least ", self.num_expected_lifetime_args()),
            Info::MissingTypesOrConsts { .. } => {
                ("at least ", self.num_expected_type_or_const_args_including_defaults())
            }
            Info::ExcessLifetimes { .. } => ("at most ", self.num_expected_lifetime_args()),
            Info::ExcessTypesOrConsts { .. } => ("at most ", self.num_expected_type_or_const_args()),
        }
    }

    fn span(&self, range: TextRange) -> Span {
        self.seg.span(self.cx, range)
    }

    fn arg_range(arg: &ast::GenericArg) -> TextRange {
        arg.syntax().text_range()
    }

    fn message(&self) -> String {
        let descr = self.descr();
        if !self.has_brackets() {
            return format!("missing generics for {descr} `{}`", def_path(self.cx, self.def));
        }
        let (quantifier, bound) = self.quantifier_and_bound();
        let kind = self.kind();
        let provided = match self.missing_lifetimes() {
            true => {
                let n = self.num_provided_lifetime_args();
                (format!("{n} lifetime argument{}", s(n)), was(n))
            }
            false => {
                let n = self.num_provided_type_or_const_args();
                (format!("{n} generic argument{}", s(n)), was(n))
            }
        };
        format!(
            "{descr} takes {quantifier}{bound} {kind} argument{} but {} {} supplied",
            s(bound),
            provided.0,
            provided.1
        )
    }

    fn notify(&self, diag: &mut Diag) {
        let (quantifier, bound) = self.quantifier_and_bound();
        let provided = self.num_provided_args();
        diag.span_label(
            self.seg.name_span(self.cx),
            format!("expected {quantifier}{bound} {} argument{}", self.kind(), s(bound)),
        );
        if self.too_many_args_provided() || self.parenthesized() {
            return;
        }
        let args = self.args.iter().skip(self.lifetime_args_offset()).take(provided).enumerate();
        for (i, arg) in args {
            let label = match i + 1 == provided {
                true => format!("supplied {provided} {} argument{}", self.kind(), s(provided)),
                false => String::new(),
            };
            diag.span_label(self.span(Self::arg_range(arg)), label);
        }
    }

    fn suggest(&self, diag: &mut Diag) {
        match self.has_brackets() && !self.parenthesized() {
            false => self.suggest_adding_args(diag),
            true => match self.too_many_args_provided() {
                false => self.suggest_adding_args(diag),
                true => {
                    self.suggest_moving_args_from_assoc_fn_to_trait(diag);
                    self.suggest_removing_args_or_generics(diag);
                }
            },
        }
    }

    fn suggest_adding_args(&self, diag: &mut Diag) {
        if self.parenthesized() {
            return;
        }
        match self.info {
            Info::MissingLifetimes { .. } => self.suggest_adding_lifetime_args(diag),
            Info::MissingTypesOrConsts { .. } => self.suggest_adding_type_and_const_args(diag),
            Info::ExcessLifetimes { .. } | Info::ExcessTypesOrConsts { .. } => {}
        }
    }

    fn suggest_adding_lifetime_args(&self, diag: &mut Diag) {
        let missing = self.num_expected_lifetime_args() - self.num_provided_lifetime_args();
        let msg = format!("add missing {} argument{}", self.kind(), s(missing));
        // hir reports lifetimes left out of a path entirely as `MissingLifetime` or
        // `ElidedLifetimesInPath`: here at least one is written, and rustc repeats the first.
        let Some(first) = self.args.iter().find_map(|it| match it {
            ast::GenericArg::LifetimeArg(it) => it.lifetime(),
            _ => None,
        }) else {
            return;
        };
        let lifetimes = vec![first.text().to_string(); missing].join(", ");
        let last = self
            .args
            .iter()
            .filter(|it| matches!(it, ast::GenericArg::LifetimeArg(_)))
            .nth(self.num_provided_lifetime_args() - 1);
        let Some(last) = last else { return };
        let at = self.span(Self::arg_range(last)).shrink_to_hi();
        diag.suggestion_verbose(at, msg, format!(", {lifetimes}"), Applicability::HasPlaceholders);
    }

    fn suggest_adding_type_and_const_args(&self, diag: &mut Diag) {
        let missing = self.num_expected_type_or_const_args_including_defaults()
            - self.num_provided_type_or_const_args();
        let msg = format!("add missing {} argument{}", self.kind(), s(missing));
        let suggested = self.type_or_const_args_suggestions_from_param_names(missing);
        match &self.seg.list {
            None => {
                let at = self.seg.name_span(self.cx).shrink_to_hi();
                diag.suggestion_verbose(at, msg, format!("<{suggested}>"), Applicability::HasPlaceholders);
            }
            Some(_) => {
                let Some(inner) = self.seg.inner_range() else { return };
                let offset = self.lifetime_args_offset() + self.num_provided_type_or_const_args();
                let (at, is_first) = match offset.checked_sub(1).and_then(|it| self.args.get(it)) {
                    None => (self.span(inner).shrink_to_lo(), true),
                    Some(arg) => (self.span(Self::arg_range(arg)).shrink_to_hi(), false),
                };
                let prefix = if is_first { "" } else { ", " };
                let suffix = if is_first && !self.seg.constraints().is_empty() { ", " } else { "" };
                diag.suggestion_verbose(
                    at,
                    msg,
                    format!("{prefix}{suggested}{suffix}"),
                    Applicability::HasPlaceholders,
                );
            }
        }
    }

    /// rustc's `get_type_or_const_args_suggestions_from_param_names`: the names of the
    /// parameters left out, `_` for a type that a method call's turbofish or the function's
    /// arguments infer.
    fn type_or_const_args_suggestions_from_param_names(&self, take: usize) -> String {
        let in_method_call = self.is_in_a_method_call();
        let inputs = self.fn_input_names();
        let mut out = String::new();
        for param in self.types.iter().skip(self.num_provided_type_or_const_args()).take(take) {
            if !out.is_empty() {
                out.push_str(", ");
            }
            match param.is_type && (in_method_call || inputs.contains(&param.name)) {
                true => out.push('_'),
                false => out.push_str(&param.name),
            }
        }
        out
    }

    /// Whether the first expression around the segment, beyond the one it is the path of, is a
    /// method call with a turbofish.
    fn is_in_a_method_call(&self) -> bool {
        let mut exprs = self.seg.node.ancestors().skip(1).filter_map(ast::Expr::cast).peekable();
        let is_path_expr = matches!(exprs.peek(), Some(ast::Expr::PathExpr(it))
            if self.seg.path().is_some_and(|path| it.path().is_some_and(|it| it.syntax() == path.syntax())));
        if is_path_expr {
            exprs.next();
        }
        matches!(exprs.next(), Some(ast::Expr::MethodCallExpr(it)) if it.generic_arg_list().is_some())
    }

    /// The names of the function's parameters whose type is a bare path, `x: T`: rustc's
    /// `is_used_in_input`.
    fn fn_input_names(&self) -> Vec<String> {
        let GenericDef::Function(f) = self.def else { return Vec::new() };
        let Some(src) = f.source(self.cx.db) else { return Vec::new() };
        let Some(params) = src.value.param_list() else { return Vec::new() };
        params
            .params()
            .filter_map(|param| match param.ty()? {
                ast::Type::PathType(it) => {
                    let path = it.path()?;
                    let segment = path.as_single_segment()?;
                    if segment.generic_arg_list().is_some() {
                        return None;
                    }
                    Some(segment.name_ref()?.text().to_string())
                }
                _ => None,
            })
            .collect()
    }

    /// rustc's `suggest_moving_args_from_assoc_fn_to_trait`: `Into::into::<T>(x)` as
    /// `Into::<T>::into(x)`.
    fn suggest_moving_args_from_assoc_fn_to_trait(&self, diag: &mut Diag) {
        let db = self.cx.db;
        let (GenericDef::Function(f), None) = (self.def, self.param) else { return };
        let Some(AssocItemContainer::Trait(trait_)) = f.as_assoc_item(db).map(|it| it.container(db)) else {
            return;
        };
        if self.num_expected_type_or_const_args() + self.num_expected_lifetime_args() > 0 {
            return;
        }
        let excess = self.num_excess_type_or_const_args() + self.num_excess_lifetime_args();
        let trait_generics = GenericDef::Trait(trait_).params(db).len().saturating_sub(1);
        let msg = format!(
            "consider moving {these} generic argument{s} to the `{name}` trait, which takes up to {num} argument{s}",
            these = if excess == 1 { "this" } else { "these" },
            s = s(excess),
            name = trait_.name(db).display(db, self.cx.edition),
            num = trait_generics,
        );
        let Some(span_ext) = self.seg.span_ext() else { return };
        let Some(generics) = self.cx.snippet(&self.span(span_ext)) else { return };
        let name = self.seg.name_text();
        let name_span = self.seg.name_span(self.cx);
        if let Some(path) = self.seg.path()
            && path.syntax().parent().is_some_and(|it| ast::PathExpr::can_cast(it.kind()))
        {
            let Some(first) = path.first_segment() else { return };
            let supplied = Segment::of_node(self.seg.file, first.syntax().clone(), None)
                .map_or(0, |it| {
                    it.args().iter().filter(|it| !matches!(it, ast::GenericArg::LifetimeArg(_))).count()
                });
            if supplied + excess != trait_generics {
                return;
            }
            let removed = self.span(TextRange::new(self.seg.name.syntax().text_range().end(), span_ext.end()));
            diag.multipart_suggestion(
                msg,
                vec![(name_span, format!("{generics}::{name}")), (removed, String::new())],
                Applicability::MaybeIncorrect,
            );
            return;
        }
        let Some(call) = ast::MethodCallExpr::cast(self.seg.node.clone()) else { return };
        if excess != trait_generics {
            return;
        }
        let Some(receiver) = call.receiver() else { return };
        let Some(rcvr) = self.cx.snippet(&self.span_of(&receiver)) else { return };
        let args: Vec<ast::Expr> = call.arg_list().map(|it| it.args().collect()).unwrap_or_default();
        let rest = match (args.first(), args.last()) {
            (Some(first), Some(last)) => {
                let range = first.syntax().text_range().cover(last.syntax().text_range());
                match self.cx.snippet(&self.span(range)) {
                    Some(it) => it.to_owned(),
                    None => return,
                }
            }
            _ => String::new(),
        };
        let comma = if args.is_empty() { "" } else { ", " };
        let trait_path = self.cx.def_path(trait_.into());
        diag.suggestion_verbose(
            self.span_of(&call),
            msg,
            format!("{trait_path}::{generics}::{name}({rcvr}{comma}{rest})"),
            Applicability::MaybeIncorrect,
        );
    }

    fn span_of(&self, node: &impl AstNode) -> Span {
        self.seg.span_of(self.cx, node)
    }

    /// rustc's `suggest_removing_args_or_generics`.
    fn suggest_removing_args_or_generics(&self, diag: &mut Diag) {
        let redundant_lt = self.num_excess_lifetime_args();
        let redundant_ty = self.num_excess_type_or_const_args();
        let unbound = self.unbound_associated_types();
        let remove_entire_generics = redundant_lt + redundant_ty >= self.args.len();

        if unbound.len() == redundant_ty && !unbound.is_empty() {
            if self.is_in_trait_impl() {
                return;
            }
            let mut unbound = unbound.into_iter();
            let parts: Vec<(Span, String)> = self
                .args
                .iter()
                .skip(self.num_expected_type_or_const_args())
                .filter_map(|arg| match arg {
                    ast::GenericArg::TypeArg(_) => {
                        let name = unbound.next()?;
                        Some((self.span(Self::arg_range(arg)).shrink_to_lo(), format!("{name} = ")))
                    }
                    _ => None,
                })
                .collect();
            if !parts.is_empty() {
                let s = s(parts.len());
                let article = if parts.len() == 1 { "an " } else { "" };
                diag.multipart_suggestion(
                    format!("turn the generic argument{s} into {article}associated item binding{s}"),
                    parts,
                    Applicability::MaybeIncorrect,
                );
            }
        } else if remove_entire_generics {
            let Some(span_ext) = self.seg.span_ext() else { return };
            let start = self.seg.name.syntax().text_range().end();
            let range = TextRange::new(start, span_ext.end().max(start));
            let msg = match self.parenthesized() {
                true => "remove the unnecessary parenthetical generics",
                false => "remove the unnecessary generics",
            };
            if !range.is_empty() {
                diag.suggestion(self.span(range), msg, "", Applicability::MaybeIncorrect);
            }
        } else if redundant_lt > 0 && redundant_ty > 0 {
            self.remove_lifetime_args(diag);
            self.remove_type_or_const_args(diag);
        } else if redundant_lt > 0 {
            self.remove_lifetime_args(diag);
        } else {
            self.remove_type_or_const_args(diag);
        }
    }

    fn remove_lifetime_args(&self, diag: &mut Diag) {
        let expected = self.num_expected_lifetime_args();
        let mut spans = Vec::new();
        let mut found_redundant = false;
        for arg in self.args {
            if let ast::GenericArg::LifetimeArg(_) = arg {
                spans.push(Self::arg_range(arg));
                if spans.len() > expected {
                    found_redundant = true;
                }
            } else if found_redundant {
                break;
            }
        }
        let (Some(&lo), Some(&hi)) = (spans.get(expected.saturating_sub(1)), spans.last()) else { return };
        let range = TextRange::empty(lo.end()).cover(hi);
        let n = spans.len() - expected;
        diag.suggestion(
            self.span(range),
            format!("remove the lifetime argument{}", s(n)),
            "",
            Applicability::MaybeIncorrect,
        );
    }

    fn remove_type_or_const_args(&self, diag: &mut Diag) {
        let expected = self.num_expected_type_or_const_args();
        let mut spans = Vec::new();
        let mut found_redundant = false;
        for arg in self.args {
            match arg {
                ast::GenericArg::TypeArg(_) | ast::GenericArg::ConstArg(_) => {
                    spans.push(Self::arg_range(arg));
                    if spans.len() > expected {
                        found_redundant = true;
                    }
                }
                _ if found_redundant => break,
                _ => {}
            }
        }
        let (Some(&lo), Some(&hi)) = (spans.get(expected.saturating_sub(1)), spans.last()) else { return };
        let range = TextRange::empty(lo.end()).cover(hi);
        let n = spans.len() - expected;
        diag.suggestion(
            self.span(range),
            format!("remove the unnecessary generic argument{}", s(n)),
            "",
            Applicability::MaybeIncorrect,
        );
    }

    /// The associated types of a trait, in order, that the arguments bind no constraint of:
    /// rustc's `get_unbound_associated_item`.
    fn unbound_associated_types(&self) -> Vec<String> {
        let db = self.cx.db;
        let (GenericDef::Trait(trait_), None) = (self.def, self.param) else { return Vec::new() };
        let bound: Vec<String> = self
            .seg
            .constraints()
            .iter()
            .filter_map(|it| Some(it.name_ref()?.text().to_string()))
            .collect();
        trait_
            .items(db)
            .into_iter()
            .filter_map(|item| item.as_type_alias())
            .map(|it| it.name(db).display(db, self.cx.edition).to_string())
            .filter(|name| !bound.contains(name))
            .collect()
    }

    /// Whether the segment names the trait of the impl it is the header of.
    fn is_in_trait_impl(&self) -> bool {
        if !matches!(self.def, GenericDef::Trait(_)) || self.param.is_some() {
            return false;
        }
        let Some(path) = self.seg.path() else { return false };
        let Some(ty) = path.syntax().parent().and_then(ast::PathType::cast) else { return false };
        let Some(impl_) = ty.syntax().parent().and_then(ast::Impl::cast) else { return false };
        impl_.trait_().is_some_and(|it| it.syntax() == ty.syntax())
    }

    /// rustc's `show_definition`: "struct defined here, with 1 generic parameter: `T`".
    fn show_definition(&self, diag: &mut Diag) {
        let Some(def_span) = self.def_span() else { return };
        let mut spans = MultiSpan::of(def_span);
        let (quantifier, bound) = self.quantifier_and_bound();
        let params = match bound {
            0 => String::new(),
            _ => {
                let params = match self.missing_lifetimes() {
                    true => self.lifetimes,
                    false => self.types,
                };
                let mut names = String::from(": ");
                for (i, param) in params.iter().take(bound).enumerate() {
                    if let Some(span) = &param.span {
                        spans.labels.push((span.clone(), String::new()));
                    }
                    if i > 0 {
                        names.push_str(", ");
                    }
                    names.push('`');
                    names.push_str(&param.name);
                    names.push('`');
                }
                names
            }
        };
        let msg = format!(
            "{} defined here, with {quantifier}{bound} {} parameter{}{params}",
            self.descr(),
            self.kind(),
            s(bound),
        );
        diag.span_note(spans, msg);
    }

    fn note_synth_provided(&self, diag: &mut Diag) {
        if let Info::ExcessTypesOrConsts { synth_provided: true, .. } = self.info {
            diag.note("`impl Trait` cannot be explicitly specified as a generic argument");
        }
    }

    /// rustc delays the error of a trait in an impl's header given constraints named as its
    /// parameters (`impl Foo<T: Default> for u8`), giving the constraints' error instead.
    fn all_params_are_bound(&self) -> bool {
        let db = self.cx.db;
        let in_impl = self
            .seg
            .node
            .ancestors()
            .find_map(ast::Item::cast)
            .is_some_and(|it| matches!(it, ast::Item::Impl(_)));
        if !in_impl || !matches!(self.info, Info::MissingTypesOrConsts { .. }) {
            return false;
        }
        let constraints: Vec<String> = self
            .seg
            .constraints()
            .iter()
            .filter_map(|it| Some(it.name_ref()?.text().to_string()))
            .collect();
        let params: Vec<String> = self
            .lifetimes
            .iter()
            .chain(self.types)
            .map(|it| it.name.clone())
            .chain(
                self.def
                    .type_or_const_params(db)
                    .into_iter()
                    .filter(|it| matches!(it.split(db), Either::Right(p) if p.is_implicit(db)))
                    .filter(|_| !matches!(self.def, GenericDef::Trait(_)))
                    .map(|it| it.name(db).display(db, self.cx.edition).to_string()),
            )
            .collect();
        if constraints != params {
            return false;
        }
        let GenericDef::Trait(trait_) = self.def else { return true };
        let assoc: Vec<String> = trait_
            .items_with_supertraits(db)
            .into_iter()
            .filter_map(|it| it.as_type_alias())
            .map(|it| it.name(db).display(db, self.cx.edition).to_string())
            .collect();
        !constraints.iter().any(|it| assoc.contains(it))
    }
}

pub(crate) fn lower(cx: &Cx<'_, '_>, d: &hir::IncorrectGenericsLen) -> Vec<Found> {
    let node = cx.node(d.generics_or_segment);
    let node_span = cx.span(node.as_ref().map(|it| it.syntax().text_range()));
    let provided = d.provided as usize;
    let expected = d.expected as usize;
    let types = type_or_const_params(cx, d.def);
    let info = match d.kind {
        IncorrectGenericsLenKind::Lifetimes => match provided > expected {
            true => Info::ExcessLifetimes { redundant: provided - expected },
            false => Info::MissingLifetimes { missing: expected - provided },
        },
        IncorrectGenericsLenKind::TypesAndConsts => {
            let defaults = types.iter().filter(|it| it.has_default).count();
            match provided > expected {
                true => Info::ExcessTypesOrConsts {
                    redundant: provided - expected,
                    defaults,
                    synth_provided: provided <= expected + synthetic_params(cx, d.def),
                },
                false => Info::MissingTypesOrConsts { missing: expected - provided, defaults },
            }
        }
    };
    let Some(seg) = Segment::of(cx, d.generics_or_segment) else {
        let kind = match d.kind {
            IncorrectGenericsLenKind::Lifetimes => "lifetime",
            IncorrectGenericsLenKind::TypesAndConsts => "generic",
        };
        let message = format!(
            "{} takes {expected} {kind} argument{} but {provided} {kind} argument{} {} supplied",
            def_descr(cx, d.def),
            s(expected),
            s(provided),
            was(provided)
        );
        let mut diag = Diag::error(Some("E0107"), message, node_span);
        generics_fact(cx, &mut diag, d.def);
        return vec![Found::new(diag, Phase::Items)];
    };
    let args = seg.args();
    let lifetimes = lifetime_params(cx, d.def);
    let wrong = Wrong {
        cx,
        seg: &seg,
        def: d.def,
        param: None,
        info,
        args: &args,
        lifetimes: &lifetimes,
        types: &types,
    };
    if wrong.all_params_are_bound() {
        return Vec::new();
    }
    let mut diag = Diag::error(Some("E0107"), wrong.message(), seg.name_span(cx));
    wrong.notify(&mut diag);
    wrong.suggest(&mut diag);
    wrong.show_definition(&mut diag);
    wrong.note_synth_provided(&mut diag);
    generics_fact(cx, &mut diag, d.def);
    site_fact(cx, &mut diag, InFile::new(seg.file, &seg.node));
    vec![Found::new(diag, phase_at(&seg.node))]
}

/// E0107 of generic arguments given to a static or a const parameter named by a value path,
/// which take none: rustc counts the last segment's arguments as any value's
/// (`probe_generic_path_segments`), the lifetimes' error first, then the types' and consts'.
pub(crate) fn takes_none(
    cx: &Cx<'_, '_>,
    seg: &Segment,
    def: GenericDef,
    param: Option<hir::ConstParam>,
) -> Vec<Found> {
    let args = seg.args();
    let lifetimes = args.iter().filter(|it| matches!(it, ast::GenericArg::LifetimeArg(_))).count();
    let others = match seg.list.is_none() && seg.parenthesized().is_some() {
        true => 1,
        false => args.len() - lifetimes,
    };
    let infos = [
        (lifetimes > 0).then_some(Info::ExcessLifetimes { redundant: lifetimes }),
        (others > 0).then_some(Info::ExcessTypesOrConsts {
            redundant: others,
            defaults: 0,
            synth_provided: false,
        }),
    ];
    let mut found = Vec::new();
    for info in infos.into_iter().flatten() {
        // A static and a const parameter take no generics: there are none to list.
        let wrong = Wrong { cx, seg, def, param, info, args: &args, lifetimes: &[], types: &[] };
        let mut diag = Diag::error(Some("E0107"), wrong.message(), seg.name_span(cx));
        wrong.notify(&mut diag);
        wrong.suggest(&mut diag);
        wrong.show_definition(&mut diag);
        generics_fact(cx, &mut diag, def);
        site_fact(cx, &mut diag, InFile::new(seg.file, &seg.node));
        found.push(Found::new(diag, phase_at(&seg.node)));
    }
    found
}
