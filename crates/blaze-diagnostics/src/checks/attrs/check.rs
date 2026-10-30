//! The errors of `check_mod_attrs` (`CheckAttrVisitor::check_attributes`) of a node's parsed
//! attributes, in its order: an exported `macro_rules` with `#[inline]` first, then each parsed
//! attribute in turn, then the `#[repr]` hints together, `#[rustc_force_inline]` on a
//! coroutine, and `#[optimize(none)]` with `#[inline]`. Its lints are
//! `lints::syntax::attrs::check_attr`'s.
//!
//! - `check_rustc_allow_const_fn_unstable`, `check_dump_object_lifetime_defaults` (whose
//!   `object_lifetime_default` is read from the bounds as `resolve_bound_vars` does),
//!   `check_naked`, `check_track_caller`, `check_non_exhaustive`, `check_may_dangle`,
//!   `check_rustc_legacy_const_generics`, `check_doc_attrs` (`parsers::doc::check_doc`),
//!   `check_rustc_must_implement_one_of`, `check_linkage`.
//! - `check_repr`: E0692 (`transparent` with another hint or with
//!   `#[rustc_pass_indirectly_in_non_rustic_abis]`), E0566 (`Rust` with `C`, `simd` or an
//!   integer); the `conflicting_repr_hints` lint is `check_attr`'s.
//! - `check_rustc_force_inline`, `check_optimize_and_inline`,
//!   `check_non_exported_macro_for_invalid_attrs` (E0518).
//!
//! Not here: `check_proc_macro`'s `ProcMacroBadSig` compares the function's signature with
//! `fn(TokenStream) -> TokenStream` in the trait solver (asked of the front end,
//! `v2/requests/diagnostics.md`); `check_eii_impl` is of the externally implementable items'
//! attribute macros, which no builtin parser here takes. In this checkout E0517 (a hint on
//! what it cannot be applied to) is `repr`'s parser's target check (`parsers::codegen`), and
//! E0084 and E0587 are `rustc_hir_analysis`' (`check/check.rs`), not `check_mod_attrs`'.
//!
//! Follows rustc's `compiler/rustc_passes/src/check_attr.rs` and the texts of
//! `compiler/rustc_passes/src/diagnostics.rs`, `compiler/rustc_hir_analysis/src/collect/
//! resolve_bound_vars.rs` (`object_lifetime_default`), `compiler/rustc_hir/src/
//! weak_lang_items.rs` and `compiler/rustc_abi/src/extern_abi.rs` (`is_rustic_abi`) (MIT or
//! Apache-2.0).

use hir::{InFile, LangItem};
use rustc_hash::FxHashMap;
use syntax::{
    AstNode, SyntaxKind, SyntaxNode, TextRange, WalkEvent,
    ast::{self, HasGenericParams, HasName, HasTypeBounds, IsString},
};

use super::{
    accept::Session,
    parsed::{Inline, Optimize, Parsed, ParsedAttr, Repr},
    parsers::{Finalize, doc},
};
use crate::{
    cx::{Cx, def_range, signature_range},
    lints::syntax::attrs::{
        Owner, Target,
        target::{GenericParamKind, MethodKind},
    },
    model::Diag,
};

/// The errors `check_attributes` gives of `owner`'s attributes, parsed to `parsed`.
pub(crate) fn check(cx: &Cx<'_, '_>, session: &Session, owner: &Owner, parsed: &[ParsedAttr], errors: &mut Vec<Diag>) {
    let mut fx = Finalize::new(cx, session, owner, true, errors);
    non_exported_macro(&mut fx, parsed);
    for attr in parsed {
        one(&mut fx, parsed, attr);
    }
    repr(&mut fx, parsed);
    force_inline(&mut fx, parsed);
    optimize_and_inline(&mut fx, parsed);
}

/// `check_one_parsed_attribute`: the errors of one attribute.
fn one(fx: &mut Finalize<'_, '_, '_>, parsed: &[ParsedAttr], attr: &ParsedAttr) {
    match &attr.parsed {
        Parsed::RustcAllowConstFnUnstable => allow_const_fn_unstable(fx, attr.attr),
        Parsed::RustcDumpObjectLifetimeDefaults => object_lifetime_defaults(fx),
        Parsed::Naked => naked(fx),
        Parsed::TrackCaller => track_caller(fx, parsed, attr.attr),
        Parsed::NonExhaustive => non_exhaustive(fx, attr.attr),
        Parsed::MayDangle => may_dangle(fx, attr.attr),
        Parsed::RustcLegacyConstGenerics(indices) => legacy_const_generics(fx, attr.attr, indices),
        Parsed::Doc(it) => doc::check_doc(fx, it, attr.attr),
        Parsed::RustcMustImplementOneOf(names) => must_implement_one_of(fx, attr.attr, names),
        Parsed::Linkage(_) => linkage(fx, attr.attr),
        Parsed::Other(_)
        | Parsed::Inline(_)
        | Parsed::Optimize(_)
        | Parsed::Repr(_)
        | Parsed::Lang(_)
        | Parsed::ExportName(_)
        | Parsed::NoMangle
        | Parsed::ProcMacro(_)
        | Parsed::RustcPassIndirectlyInNonRusticAbis
        | Parsed::MacroExport => {}
    }
}

/// The node's syntax as `N`, when it is one.
fn node<N: AstNode>(fx: &Finalize<'_, '_, '_>) -> Option<N> {
    N::cast(fx.owner.node.value.clone())
}

/// A range of the file the node is in.
fn node_span(fx: &Finalize<'_, '_, '_>, range: TextRange) -> crate::model::Span {
    fx.cx.span(InFile::new(fx.owner.node.file_id, range))
}

/// Whether `node` has the keyword `kind` among its own tokens (`const trait`, `impl const`).
fn has_keyword(node: &SyntaxNode, kind: SyntaxKind) -> bool {
    node.children_with_tokens().any(|it| it.as_token().is_some_and(|it| it.kind() == kind))
}

/// `check_rustc_allow_const_fn_unstable`: on a function that is not a `const fn`.
fn allow_const_fn_unstable(fx: &mut Finalize<'_, '_, '_>, attr: usize) {
    if !matches!(fx.target(), Target::Fn | Target::Method(_)) {
        return;
    }
    let Some(function) = node::<ast::Fn>(fx) else { return };
    if is_const_fn(&function) {
        return;
    }
    let mut diag = Diag::error(None, "attribute should be applied to `const fn`", fx.attr_span(attr));
    diag.span_label(fx.target_span(), "not a `const fn`");
    fx.emit_err(diag);
}

/// rustc's `is_const_fn`: written `const`, or a method of a `const trait` or of a const impl.
fn is_const_fn(function: &ast::Fn) -> bool {
    if function.const_token().is_some() {
        return true;
    }
    let Some(container) = function.syntax().parent().and_then(|it| it.parent()) else { return false };
    match container.kind() {
        SyntaxKind::TRAIT | SyntaxKind::IMPL => has_keyword(&container, SyntaxKind::CONST_KW),
        _ => false,
    }
}

/// rustc's `ObjectLifetimeDefault` of one type parameter, as the dump prints it.
#[derive(Clone, PartialEq, Eq)]
enum ObjectLifetimeDefault {
    Empty,
    Static,
    Param(String),
    Ambiguous,
}

impl ObjectLifetimeDefault {
    fn text(&self) -> &str {
        match self {
            ObjectLifetimeDefault::Empty => "Empty",
            ObjectLifetimeDefault::Static => "'static",
            ObjectLifetimeDefault::Param(name) => name,
            ObjectLifetimeDefault::Ambiguous => "Ambiguous",
        }
    }
}

/// `check_dump_object_lifetime_defaults`: an error naming each of the node's own type
/// parameters' object lifetime default, at the parameter (a trait's `Self` first, at the trait;
/// a function's `impl Trait` arguments last).
fn object_lifetime_defaults(fx: &mut Finalize<'_, '_, '_>) {
    let item = fx.owner.node.value.clone();
    let Some(generic) = ast::AnyHasGenericParams::cast(item.clone()) else { return };
    let predicates: Vec<ast::WherePred> =
        generic.where_clause().map(|it| it.predicates().collect()).unwrap_or_default();
    let mut found: Vec<(TextRange, ObjectLifetimeDefault)> = Vec::new();
    if let Some(trait_) = ast::Trait::cast(item.clone()) {
        let mut set = LifetimeSet::Empty;
        set.add(trait_.type_bound_list());
        for predicate in predicates.iter().filter(|it| bounds_param(it, "Self")) {
            set.add(predicate.type_bound_list());
        }
        found.push((def_range(&item), set.default()));
    }
    for param in generic.generic_param_list().into_iter().flat_map(|it| it.generic_params()) {
        let ast::GenericParam::TypeParam(param) = param else { continue };
        let Some(name) = param.name() else { continue };
        let mut set = LifetimeSet::Empty;
        set.add(param.type_bound_list());
        for predicate in predicates.iter().filter(|it| bounds_param(it, name.text().as_str())) {
            set.add(predicate.type_bound_list());
        }
        found.push((name.syntax().text_range(), set.default()));
    }
    if let Some(function) = ast::Fn::cast(item) {
        for param in function.param_list().into_iter().flat_map(|it| it.params()) {
            let Some(ty) = param.ty() else { continue };
            for synthetic in ty.syntax().descendants().filter(|it| it.kind() == SyntaxKind::IMPL_TRAIT_TYPE) {
                found.push((synthetic.text_range(), ObjectLifetimeDefault::Empty));
            }
        }
    }
    for (range, default) in found {
        let diag = Diag::error(None, default.text(), node_span(fx, range));
        fx.emit_err(diag);
    }
}

/// Whether a `where` predicate bounds exactly the parameter `name`, without `for<..>`.
fn bounds_param(predicate: &ast::WherePred, name: &str) -> bool {
    if predicate.for_binder().is_some() {
        return false;
    }
    let Some(ast::Type::PathType(path)) = predicate.ty() else { return false };
    let Some(path) = path.path() else { return false };
    path.qualifier().is_none()
        && path.segment().is_some_and(|it| it.generic_arg_list().is_none() && it.syntax().text() == name)
}

/// rustc's `Set1` of the lifetimes a type parameter outlives.
enum LifetimeSet {
    Empty,
    One(String),
    Many,
}

impl LifetimeSet {
    fn add(&mut self, bounds: Option<ast::TypeBoundList>) {
        for bound in bounds.into_iter().flat_map(|it| it.bounds()) {
            let Some(lifetime) = bound.lifetime() else { continue };
            let name = lifetime.syntax().text().to_string();
            *self = match std::mem::replace(self, LifetimeSet::Many) {
                LifetimeSet::Empty => LifetimeSet::One(name),
                LifetimeSet::One(it) if it == name => LifetimeSet::One(it),
                _ => LifetimeSet::Many,
            };
        }
    }

    fn default(self) -> ObjectLifetimeDefault {
        match self {
            LifetimeSet::Empty => ObjectLifetimeDefault::Empty,
            LifetimeSet::One(it) if it == "'static" => ObjectLifetimeDefault::Static,
            LifetimeSet::One(it) if it == "'_" => ObjectLifetimeDefault::Ambiguous,
            LifetimeSet::One(it) => ObjectLifetimeDefault::Param(it),
            LifetimeSet::Many => ObjectLifetimeDefault::Ambiguous,
        }
    }
}

/// A function's ABI as rustc names it: `Rust` without `extern`, `C` for `extern` alone.
fn abi(function: &ast::Fn) -> String {
    let Some(abi) = function.abi() else { return "Rust".to_owned() };
    match abi.string_token().and_then(ast::String::cast) {
        Some(it) => it.value().map(|it| it.into_owned()).unwrap_or_else(|_| it.text_without_quotes().to_owned()),
        None => "C".to_owned(),
    }
}

/// `check_naked`: `#[unsafe(naked)]` on a function of a Rust ABI, unstable.
fn naked(fx: &mut Finalize<'_, '_, '_>) {
    if !matches!(
        fx.target(),
        Target::Fn | Target::Method(MethodKind::Trait { body: true } | MethodKind::Inherent)
    ) {
        return;
    }
    let Some(function) = node::<ast::Fn>(fx) else { return };
    let abi = abi(&function);
    if !matches!(abi.as_str(), "Rust" | "rust-call" | "rust-cold" | "rust-preserve-none" | "tail") {
        return;
    }
    let span = node_span(fx, signature_range(&function));
    fx.gate("naked_functions_rustic_abi", span, format!("`#[naked]` is currently unstable on `extern \"{abi}\"` functions"));
}

/// The weak lang items (`weak_lang_items!`): called through `extern` declarations.
const WEAK_LANG_ITEMS: &[&str] = &["panic_impl", "eh_personality"];

/// `check_track_caller`: `#[track_caller]` on a weak lang item.
fn track_caller(fx: &mut Finalize<'_, '_, '_>, parsed: &[ParsedAttr], attr: usize) {
    if fx.target() != Target::Fn {
        return;
    }
    let Some(name) = parsed.iter().find_map(|it| match &it.parsed {
        Parsed::Lang(name) => Some(name.as_str()),
        _ => None,
    }) else {
        return;
    };
    if !WEAK_LANG_ITEMS.contains(&name) {
        return;
    }
    let Some(function) = node::<ast::Fn>(fx) else { return };
    let what = match name {
        "panic_impl" => "`#[panic_handler]`".to_owned(),
        other => format!("`{other}` lang item"),
    };
    let message = format!("{what} function is not allowed to have `#[track_caller]`");
    let mut diag = Diag::error(None, message.clone(), fx.attr_span(attr));
    diag.span_label(node_span(fx, signature_range(&function)), message);
    fx.emit_err(diag);
}

/// `check_non_exhaustive`: on a struct with default field values.
fn non_exhaustive(fx: &mut Finalize<'_, '_, '_>, attr: usize) {
    if fx.target() != Target::Struct {
        return;
    }
    let Some(ast::FieldList::RecordFieldList(fields)) = node::<ast::Struct>(fx).and_then(|it| it.field_list()) else {
        return;
    };
    if !fields.fields().any(|it| it.default_val().is_some()) {
        return;
    }
    let mut diag = Diag::error(
        None,
        "`#[non_exhaustive]` can't be used to annotate items with default field values",
        fx.attr_span(attr),
    );
    diag.span_label(fx.target_span(), "this struct has default field values");
    fx.emit_err(diag);
}

/// `check_may_dangle`: on anything but a lifetime or type parameter of an `impl Drop`.
fn may_dangle(fx: &mut Finalize<'_, '_, '_>, attr: usize) {
    let Target::GenericParam { kind: GenericParamKind::Lifetime | GenericParamKind::Type, .. } = fx.target() else {
        return;
    };
    let in_drop_impl = || {
        let list = fx.owner.node.value.parent().filter(|it| it.kind() == SyntaxKind::GENERIC_PARAM_LIST)?;
        let impl_ = list.parent().and_then(ast::Impl::cast)?;
        let trait_ = fx.cx.sema.to_def(&impl_)?.trait_(fx.cx.db)?;
        Some(hir::Trait::lang(fx.cx.db, fx.cx.krate, LangItem::Drop)? == trait_)
    };
    if in_drop_impl() == Some(true) {
        return;
    }
    let diag = Diag::error(
        None,
        "`#[may_dangle]` must be applied to a lifetime or type generic parameter in `Drop` impl",
        fx.attr_span(attr),
    );
    fx.emit_err(diag);
}

/// `check_rustc_legacy_const_generics`, of a function item.
fn legacy_const_generics(fx: &mut Finalize<'_, '_, '_>, attr: usize, indices: &[(usize, TextRange)]) {
    if fx.target() != Target::Fn {
        return;
    }
    let Some(function) = node::<ast::Fn>(fx) else { return };
    let mut params: Vec<(bool, TextRange)> = function
        .generic_param_list()
        .into_iter()
        .flat_map(|it| it.generic_params())
        .map(|it| (matches!(it, ast::GenericParam::ConstParam(_)), it.syntax().text_range()))
        .collect();
    let inputs: Vec<ast::Param> = function.param_list().into_iter().flat_map(|it| it.params()).collect();
    for param in &inputs {
        let Some(ty) = param.ty() else { continue };
        params.extend(
            ty.syntax().descendants().filter(|it| it.kind() == SyntaxKind::IMPL_TRAIT_TYPE).map(|it| (false, it.text_range())),
        );
    }
    if let Some(&(_, range)) = params.iter().find(|(is_const, _)| !is_const) {
        let mut diag = Diag::error(None, "#[rustc_legacy_const_generics] functions must only have const generics", fx.attr_span(attr));
        diag.span_label(node_span(fx, range), "non-const generic parameter");
        fx.emit_err(diag);
        return;
    }
    if indices.len() != params.len() {
        let generics = match function.generic_param_list() {
            Some(list) => list.syntax().text_range(),
            None => {
                let end = function.name().map_or(function.syntax().text_range().start(), |it| it.syntax().text_range().end());
                TextRange::empty(end)
            }
        };
        let mut diag = Diag::error(
            None,
            "#[rustc_legacy_const_generics] must have one index for each generic parameter",
            fx.attr_span(attr),
        );
        diag.span_label(node_span(fx, generics), "generic parameters");
        fx.emit_err(diag);
        return;
    }
    let self_param = function.param_list().and_then(|it| it.self_param()).is_some();
    let arg_count = inputs.len() + usize::from(self_param) + params.len();
    for &(index, range) in indices {
        if index < arg_count {
            continue;
        }
        let span = fx.span(attr, range);
        let label = match arg_count {
            1 => "there is only 1 argument".to_owned(),
            n => format!("there are only {n} arguments"),
        };
        let mut diag = Diag::error(None, "index exceeds number of arguments", span.clone());
        diag.span_label(span, label);
        fx.emit_err(diag);
    }
}

/// An associated item of a trait, as `check_rustc_must_implement_one_of` looks at it.
struct AssocItem {
    name: String,
    /// rustc's `def_span`.
    range: TextRange,
    /// A function, with whether it has a default body.
    function: Option<bool>,
}

fn assoc_items(trait_: &ast::Trait) -> Vec<AssocItem> {
    let mut items = Vec::new();
    for item in trait_.assoc_item_list().into_iter().flat_map(|it| it.assoc_items()) {
        let (name, range, function) = match &item {
            ast::AssocItem::Fn(it) => (it.name(), signature_range(it), Some(it.body().is_some())),
            ast::AssocItem::Const(it) => {
                let start = crate::cx::item_range(it.syntax()).start();
                let end = it.ty().map(|ty| ty.syntax().text_range().end()).or_else(|| it.name().map(|n| n.syntax().text_range().end()));
                (it.name(), TextRange::new(start, end.unwrap_or(start)), None)
            }
            ast::AssocItem::TypeAlias(it) => {
                let start = crate::cx::item_range(it.syntax()).start();
                let end = it
                    .generic_param_list()
                    .map(|g| g.syntax().text_range().end())
                    .or_else(|| it.name().map(|n| n.syntax().text_range().end()));
                (it.name(), TextRange::new(start, end.unwrap_or(start)), None)
            }
            ast::AssocItem::MacroCall(_) => continue,
        };
        let Some(name) = name else { continue };
        items.push(AssocItem { name: name.text().to_string(), range, function });
    }
    items
}

/// `check_rustc_must_implement_one_of`: each name must be a function of the trait with a
/// default body, named once.
fn must_implement_one_of(fx: &mut Finalize<'_, '_, '_>, attr: usize, names: &[(String, TextRange)]) {
    if fx.target() != Target::Trait {
        return;
    }
    let Some(trait_) = node::<ast::Trait>(fx) else { return };
    let items = assoc_items(&trait_);
    let attr_span = fx.attr_span(attr);
    for (name, range) in names {
        match items.iter().find(|it| it.name == *name) {
            Some(AssocItem { function: Some(true), .. }) => {}
            Some(AssocItem { function: Some(false), range, .. }) => {
                let mut diag = Diag::error(None, "function doesn't have a default implementation", node_span(fx, *range));
                diag.span_note(attr_span.clone(), "required by this annotation");
                fx.emit_err(diag);
            }
            Some(AssocItem { function: None, range, .. }) => {
                let mut diag = Diag::error(None, "not a function", node_span(fx, *range));
                diag.span_note(attr_span.clone(), "required by this annotation");
                diag.note("all `#[rustc_must_implement_one_of]` arguments must be associated function names");
                fx.emit_err(diag);
            }
            None => {
                let diag = Diag::error(None, "function not found in this trait", fx.span(attr, *range));
                fx.emit_err(diag);
            }
        }
    }
    let mut seen: FxHashMap<&str, TextRange> = FxHashMap::default();
    for (name, range) in names {
        if let Some(dup) = seen.insert(name.as_str(), *range) {
            let mut diag = Diag::error(None, "functions names are duplicated", fx.span(attr, dup));
            diag.primary(fx.span(attr, *range));
            diag.note("all `#[rustc_must_implement_one_of]` arguments must be unique");
            fx.emit_err(diag);
        }
    }
}

/// `check_linkage`: on an extern `static mut`, or a `const fn`.
fn linkage(fx: &mut Finalize<'_, '_, '_>, attr: usize) {
    match fx.target() {
        Target::ForeignStatic if node::<ast::Static>(fx).is_some_and(|it| it.mut_token().is_some()) => {
            let mut diag = Diag::error(None, "extern mutable statics are incompatible with the `linkage` attribute", fx.attr_span(attr));
            diag.note(
                "the `linkage` attribute on extern statics generates a symbol that contains the address of another static. Making the extern static mutable would allow changing the address, rather than the static the address is pointing to",
            );
            fx.emit_err(diag);
        }
        Target::Fn if node::<ast::Fn>(fx).is_some_and(|it| it.const_token().is_some()) => {
            let mut diag = Diag::error(None, "`const fn` are incompatible with the `linkage` attribute", fx.attr_span(attr));
            diag.note("`const fn` may be called at compile time, which happens before linking");
            fx.emit_err(diag);
        }
        _ => {}
    }
}

/// The first parsed attribute that is `what` makes of it, with its index.
fn find<'p, T>(parsed: &'p [ParsedAttr], what: impl Fn(&'p Parsed) -> Option<T>) -> Option<(T, usize)> {
    parsed.iter().find_map(|it| what(&it.parsed).map(|found| (found, it.attr)))
}

/// `check_repr`'s errors.
fn repr(fx: &mut Finalize<'_, '_, '_>, parsed: &[ParsedAttr]) {
    let Some((reprs, first)) = find(parsed, |it| match it {
        Parsed::Repr(reprs) => Some(reprs.as_slice()),
        _ => None,
    }) else {
        return;
    };
    let mut ints = 0;
    let (mut rust, mut c, mut simd, mut transparent) = (false, false, false, false);
    for (repr, _) in reprs {
        match repr {
            Repr::Rust => rust = true,
            Repr::C => c = true,
            Repr::Simd => simd = true,
            Repr::Transparent => transparent = true,
            Repr::Int(_) => ints += 1,
            Repr::Align(_) | Repr::Packed(_) => {}
        }
    }
    let hints: Vec<crate::model::Span> = reprs.iter().map(|(_, range)| fx.span(first, *range)).collect();
    let target = fx.target().name();
    if transparent && reprs.len() > 1 {
        let diag = multi(Some("E0692"), format!("transparent {target} cannot have other repr hints"), hints.clone());
        fx.emit_err(diag);
    }
    if transparent
        && let Some(((), pass)) = find(parsed, |it| matches!(it, Parsed::RustcPassIndirectlyInNonRusticAbis).then_some(()))
    {
        let spans = vec![fx.target_span(), fx.attr_span(pass)];
        let diag = multi(Some("E0692"), format!("transparent {target} cannot have other repr hints"), spans);
        fx.emit_err(diag);
    }
    if rust && (ints > 0 || c || simd) {
        let diag = multi(Some("E0566"), "conflicting representation hints", hints);
        fx.emit_err(diag);
    }
}

/// An error whose primary spans are `spans`.
fn multi(code: Option<&str>, message: impl Into<String>, spans: Vec<crate::model::Span>) -> Diag {
    let mut spans = spans.into_iter();
    let Some(first) = spans.next() else { unreachable!("a repr error has a span") };
    let mut diag = Diag::error(code, message, first);
    for span in spans {
        diag.primary(span);
    }
    diag
}

/// `check_rustc_force_inline`: the coroutines of a function with `#[rustc_force_inline]` (an
/// `async`, `gen` or `async gen` function's body, and each such block or closure in it), each
/// at the function.
fn force_inline(fx: &mut Finalize<'_, '_, '_>, parsed: &[ParsedAttr]) {
    let Some(((), attr)) = find(parsed, |it| matches!(it, Parsed::Inline(Inline::Force)).then_some(())) else { return };
    let Some(function) = node::<ast::Fn>(fx) else { return };
    let mut coroutines = usize::from(function.async_token().is_some() || function.gen_token().is_some());
    if let Some(body) = function.body() {
        let mut events = body.syntax().preorder();
        while let Some(event) = events.next() {
            let WalkEvent::Enter(node) = event else { continue };
            if node != *body.syntax() && ast::Item::can_cast(node.kind()) {
                events.skip_subtree();
                continue;
            }
            if let Some(block) = ast::BlockExpr::cast(node.clone())
                && (block.async_token().is_some() || block.gen_token().is_some())
            {
                coroutines += 1;
            } else if let Some(closure) = ast::ClosureExpr::cast(node)
                && (closure.async_token().is_some() || closure.gen_token().is_some())
            {
                if ast::HasAttrs::attrs(&closure).any(|it| it.simple_name().as_deref() == Some("rustc_force_inline")) {
                    continue;
                }
                coroutines += 1;
            }
        }
    }
    let span = node_span(fx, signature_range(&function));
    for _ in 0..coroutines {
        let mut diag =
            Diag::error(None, "attribute cannot be applied to a `async`, `gen` or `async gen` function", fx.attr_span(attr));
        diag.span_label(span.clone(), "`async`, `gen` or `async gen` function");
        fx.emit_err(diag);
    }
}

/// `check_optimize_and_inline`: `#[optimize(none)]` with an `#[inline]` that is not `never`.
fn optimize_and_inline(fx: &mut Finalize<'_, '_, '_>, parsed: &[ParsedAttr]) {
    let Some(((), optimize)) = find(parsed, |it| matches!(it, Parsed::Optimize(Optimize::DoNotOptimize)).then_some(()))
    else {
        return;
    };
    let Some((inline, at)) = find(parsed, |it| match it {
        Parsed::Inline(kind) => Some(*kind),
        _ => None,
    }) else {
        return;
    };
    if inline == Inline::Never {
        return;
    }
    let optimize_span = fx.attr_span(optimize);
    let mut diag = Diag::error(None, "`#[optimize(none)]` cannot be used with `#[inline]` attributes", optimize_span.clone());
    diag.span_label(optimize_span, "`#[optimize(none)]` here");
    diag.span_label(fx.attr_span(at), "`#[inline]` here");
    fx.emit_err(diag);
}

/// `check_non_exported_macro_for_invalid_attrs`: E0518, `#[inline]` on a `macro_rules` that is
/// not `#[macro_export]`.
fn non_exported_macro(fx: &mut Finalize<'_, '_, '_>, parsed: &[ParsedAttr]) {
    if fx.target() != Target::MacroDef || node::<ast::MacroRules>(fx).is_none() {
        return;
    }
    if parsed.iter().any(|it| matches!(it.parsed, Parsed::MacroExport)) {
        return;
    }
    let Some((_, attr)) = find(parsed, |it| match it {
        Parsed::Inline(kind) if *kind != Inline::Force => Some(()),
        _ => None,
    }) else {
        return;
    };
    let span = fx.attr_span(attr);
    let mut diag = Diag::error(Some("E0518"), "attribute should be applied to function or closure", span.clone());
    diag.span_label(span, "not a function or closure");
    fx.emit_err(diag);
}

#[cfg(test)]
mod tests {
    use syntax::{AstNode, SourceFile, ast};

    use super::{LifetimeSet, ObjectLifetimeDefault, bounds_param};

    fn first<N: AstNode>(text: &str) -> N {
        let parse = SourceFile::parse(text, span::Edition::Edition2021);
        parse.tree().syntax().descendants().find_map(N::cast).unwrap()
    }

    #[test]
    fn one_lifetime_twice_is_that_lifetime() {
        let param: ast::TypeParam = first("struct S<'a, T: 'a + 'a>(&'a T);");
        let mut set = LifetimeSet::Empty;
        set.add(ast::HasTypeBounds::type_bound_list(&param));
        assert!(set.default() == ObjectLifetimeDefault::Param("'a".to_owned()));
    }

    #[test]
    fn two_lifetimes_are_ambiguous_and_static_is_static() {
        let param: ast::TypeParam = first("struct S<'a, 'b, T: 'a + 'b>(&'a T);");
        let mut set = LifetimeSet::Empty;
        set.add(ast::HasTypeBounds::type_bound_list(&param));
        assert!(set.default() == ObjectLifetimeDefault::Ambiguous);
        let param: ast::TypeParam = first("struct S<T: 'static>(T);");
        let mut set = LifetimeSet::Empty;
        set.add(ast::HasTypeBounds::type_bound_list(&param));
        assert!(set.default() == ObjectLifetimeDefault::Static);
    }

    #[test]
    fn a_where_predicate_bounds_only_its_own_parameter() {
        let pred: ast::WherePred = first("struct S<T>(T) where T: 'static;");
        assert!(bounds_param(&pred, "T"));
        assert!(!bounds_param(&pred, "U"));
        let pred: ast::WherePred = first("struct S<T>(T) where for<'a> T: 'a;");
        assert!(!bounds_param(&pred, "T"));
    }
}
