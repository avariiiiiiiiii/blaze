//! `mismatched_lifetime_syntaxes`: a lifetime of a function's return type that its parameters
//! write in another syntax: hidden in a path (`ContainsLifetime`), elided (`&T`, `'_`, `+ '_`)
//! or named (`'a`, `'static`). Each lifetime the return type has is grouped with the parameters'
//! lifetimes that resolve to it (an elided one to what elision gives it); a group whose members
//! are not all of one syntax is linted, with the fixes rustc suggests.
//!
//! Functions with bodies (rustc's `check_fn`), trait functions (`check_trait_item`, so a provided
//! one twice, as rustc does) and foreign functions are checked. Closures with a written return
//! type, which rustc's `check_fn` also checks with all their elided lifetimes resolved to one
//! `LifetimeKind::Infer`, are not yet.
//!
//! Follows `compiler/rustc_lint/src/lifetime_syntax.rs`, `compiler/rustc_hir/src/hir.rs`
//! (`Lifetime::suggestion`, `LifetimeSource`), `compiler/rustc_ast_lowering/src/path.rs` (the
//! spans of a path's hidden lifetimes), `compiler/rustc_resolve/src/late.rs`
//! (`resolve_fn_params`, `find_lifetime_for_self`) and `compiler/rustc_lint/src/lints.rs`
//! (`MismatchedLifetimeSyntaxes`) (MIT or Apache-2.0).

use blaze_items::Fact;
use hir::{InFile, ModuleDef, PathResolution};
use rustc_hash::FxHashMap;
use syntax::{
    AstNode, SyntaxKind, SyntaxNode, TextRange, TextSize,
    ast::{self, HasGenericArgs, HasName},
};

use super::walk::{Container, Item};
use crate::{
    cx::Cx,
    lower::{Found, Phase},
    model::{Applicability, Diag, Span, SuggestionStyle},
};

/// What a lifetime resolves to: a name, or one elided lifetime (or one a nested binder owns).
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
enum Res {
    Named(String),
    Fresh(u32),
}

/// rustc's `LifetimeSyntax`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Syntax {
    Implicit,
    ExplicitAnonymous,
    ExplicitBound,
}

/// rustc's `AngleBrackets` of a path that hides a lifetime.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Angle {
    Missing,
    Empty,
    Full,
}

/// rustc's `LifetimeSource`, without `Other`, which is never linted.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Source {
    Reference,
    Path(Angle),
    OutlivesBound,
    PreciseCapturing,
}

/// rustc's `LifetimeSyntaxCategory`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Category {
    Hidden,
    Elided,
    Named,
}

/// One lifetime of the signature: rustc's `Info`.
#[derive(Clone, Debug)]
struct Info {
    res: Res,
    name: String,
    syntax: Syntax,
    source: Source,
    /// The lifetime's own span: where an implicit one would be written.
    ident: TextRange,
    /// The innermost type it is in, and a key for that type.
    ty: TextRange,
    /// For a reference, where its referent starts.
    referent: Option<TextSize>,
}

impl Info {
    fn category(&self) -> Category {
        match (self.syntax, self.source) {
            (Syntax::Implicit, Source::Path(_)) => Category::Hidden,
            (Syntax::ExplicitBound, _) => Category::Named,
            _ => Category::Elided,
        }
    }

    fn reporting(&self) -> TextRange {
        match self.syntax {
            Syntax::Implicit => self.ty,
            _ => self.ident,
        }
    }

    /// The span that removes an explicit lifetime of a reference, up to its referent.
    fn removing(&self) -> TextRange {
        match (self.source, self.referent) {
            (Source::Reference, Some(referent)) => TextRange::new(self.ident.start(), referent.max(self.ident.start())),
            _ => self.ident,
        }
    }

    /// rustc's `Lifetime::suggestion`, of what is not a hidden lifetime of a path.
    fn suggestion(&self, name: &str) -> (TextRange, String) {
        match self.syntax {
            Syntax::Implicit => (self.ident, format!("{name} ")),
            _ => (self.ident, name.to_owned()),
        }
    }
}

/// The lints of `item` at rustc's `check_fn`, `check_trait_item` and `check_foreign_item`.
pub(super) fn item(cx: &Cx<'_, '_>, item: &Item, anonymous_in_impl_trait: bool) -> Vec<Found> {
    let ast::Item::Fn(f) = &item.node.value else { return Vec::new() };
    let times = match (&item.container, f.body().is_some()) {
        (Container::Trait(_), true) => 2,
        (Container::Trait(_) | Container::Extern(_), false) | (_, true) => 1,
        (_, false) => 0,
    };
    let mut found = Vec::new();
    for _ in 0..times {
        check(cx, item, f, anonymous_in_impl_trait, &mut found);
    }
    found
}

/// rustc's `check_fn_like`.
fn check(cx: &Cx<'_, '_>, item: &Item, f: &ast::Fn, anonymous_in_impl_trait: bool, found: &mut Vec<Found>) {
    let Some(params) = f.param_list() else { return };
    if params.self_param().is_none() && params.params().next().is_none() {
        return;
    }
    let Some(output) = f.ret_type().and_then(|it| it.ty()) else { return };
    let impl_self = match &item.container {
        Container::Impl(imp) => imp.value.self_ty(),
        _ => None,
    };
    let mut walk = Walk { cx, fresh: 0, anonymous_in_impl_trait, target: None, output: false };
    let mut inputs: Vec<Info> = Vec::new();
    // Per parameter: the lifetimes elision counts, and those of references to `Self`.
    let mut candidates: Vec<Vec<Res>> = Vec::new();
    let mut self_refs: Vec<Res> = Vec::new();
    if let Some(param) = params.self_param() {
        let mut own = Walked::default();
        match param.ty() {
            Some(ty) => {
                walk.ty(ty.syntax(), &Scope::default(), &mut own);
                for (inner, res) in &own.refs {
                    if names_self(cx, inner, impl_self.as_ref()) && !self_refs.contains(res) {
                        self_refs.push(res.clone());
                    }
                }
            }
            None => {
                // `&self` is `self: &Self`, whose type is all of it.
                if let Some(amp) = param.amp_token() {
                    let scope = Scope { ty: Some(param.syntax().text_range()), ..Scope::default() };
                    let (res, name, syntax, ident) = match param.lifetime() {
                        Some(lifetime) => (
                            walk.named(&lifetime, &scope),
                            lifetime.text().to_string(),
                            syntax_of(&lifetime),
                            lifetime.syntax().text_range(),
                        ),
                        None => (walk.fresh(), String::new(), Syntax::Implicit, TextRange::empty(amp.text_range().end())),
                    };
                    if let Some(res) = res {
                        let referent = param.name().map(|it| it.syntax().text_range().start());
                        self_refs.push(res.clone());
                        let info = Info { res, name, syntax, source: Source::Reference, ident, ty: ident, referent };
                        walk.push(&mut own, &scope, info);
                    }
                }
            }
        }
        inputs.extend(own.infos);
        candidates.push(own.candidates);
    }
    for param in params.params() {
        let Some(ty) = param.ty() else { continue };
        let mut own = Walked::default();
        walk.ty(ty.syntax(), &Scope::default(), &mut own);
        inputs.extend(own.infos);
        candidates.push(own.candidates);
    }
    walk.target = match self_refs.as_slice() {
        [one] => Some(one.clone()),
        [_, _, ..] => None,
        [] => elision(&candidates),
    };
    walk.output = true;
    let mut outputs = Walked::default();
    walk.ty(output.syntax(), &Scope::default(), &mut outputs);
    let mut groups: Vec<(Res, Vec<Info>, Vec<Info>)> = Vec::new();
    for info in outputs.infos {
        match groups.iter_mut().find(|(res, ..)| *res == info.res) {
            Some((_, _, outputs)) => outputs.push(info),
            None => groups.push((info.res.clone(), Vec::new(), vec![info])),
        }
    }
    if groups.is_empty() {
        return;
    }
    for info in inputs {
        if let Some((_, inputs, _)) = groups.iter_mut().find(|(res, ..)| *res == info.res) {
            inputs.push(info);
        }
    }
    for (_, inputs, outputs) in &groups {
        let Some(first) = inputs.first() else { continue };
        if inputs.iter().chain(outputs).all(|it| it.category() == first.category()) {
            continue;
        }
        found.push(mismatch(cx, item, inputs, outputs));
    }
}

/// rustc's `resolve_fn_params` without `self`: the one lifetime of the one parameter that has
/// lifetimes, if there is one.
fn elision(candidates: &[Vec<Res>]) -> Option<Res> {
    let mut elision: Option<Option<Res>> = None;
    for param in candidates {
        let mut distinct: Vec<&Res> = Vec::new();
        for res in param {
            if !distinct.contains(&res) {
                distinct.push(res);
            }
        }
        if distinct.is_empty() {
            continue;
        }
        elision = match (elision, distinct.as_slice()) {
            (None, [one]) => Some(Some((*one).clone())),
            _ => Some(None),
        };
    }
    elision.flatten()
}

/// Whether `ty`, a reference's referent in `self`'s type, names `Self` or the impl's self type
/// (rustc's `find_lifetime_for_self`).
fn names_self(cx: &Cx<'_, '_>, ty: &SyntaxNode, impl_self: Option<&ast::Type>) -> bool {
    let impl_def = impl_self.and_then(|it| match it {
        ast::Type::PathType(p) => p.path().and_then(|p| cx.sema.resolve_path(&p)),
        _ => None,
    });
    std::iter::once(ty.clone()).chain(ty.descendants()).filter_map(ast::PathType::cast).filter_map(|it| it.path()).any(|path| {
        if path.qualifier().is_none() && path.segment().is_some_and(|it| it.self_type_token().is_some()) {
            return true;
        }
        match (&impl_def, cx.sema.resolve_path(&path)) {
            (Some(PathResolution::Def(a)), Some(PathResolution::Def(b))) => *a == b,
            _ => false,
        }
    })
}

/// The lifetimes binders inside a type own, and whether elision is its own there.
#[derive(Clone, Default)]
struct Scope {
    bound: Vec<String>,
    /// In a `fn` type or `Fn(..)` sugar, which elide on their own.
    nested: bool,
    /// In an `impl Trait` of a parameter, whose elided lifetimes elision does not count.
    impl_trait: bool,
    /// The innermost type.
    ty: Option<TextRange>,
}

/// What a walk of one parameter's or the return type's type found.
#[derive(Default)]
struct Walked {
    infos: Vec<Info>,
    candidates: Vec<Res>,
    /// Each reference's referent and lifetime.
    refs: Vec<(SyntaxNode, Res)>,
}

struct Walk<'c, 'a, 'db> {
    cx: &'c Cx<'a, 'db>,
    fresh: u32,
    anonymous_in_impl_trait: bool,
    /// What the return type's elided lifetimes resolve to.
    target: Option<Res>,
    output: bool,
}

impl Walk<'_, '_, '_> {
    fn fresh(&mut self) -> Option<Res> {
        self.fresh += 1;
        Some(Res::Fresh(self.fresh))
    }

    /// What an elided lifetime resolves to at `scope`: its own, or in the return type outside
    /// a nested binder what elision gives (none when it fails: E0106).
    fn elided(&mut self, scope: &Scope) -> Option<Res> {
        if self.output && !scope.nested {
            return self.target.clone();
        }
        self.fresh()
    }

    fn named(&mut self, lifetime: &ast::Lifetime, scope: &Scope) -> Option<Res> {
        let name = lifetime.text().to_string();
        if name == "'_" {
            return self.elided(scope);
        }
        if scope.bound.contains(&name) {
            return self.fresh();
        }
        Some(Res::Named(name))
    }

    /// Whether elision counts `res` written at `scope` as a lifetime of its parameter.
    fn counts(&self, scope: &Scope, res: &Res) -> bool {
        let elided = matches!(res, Res::Fresh(_));
        !scope.nested && (!scope.impl_trait || !elided || self.anonymous_in_impl_trait)
    }

    /// Records `info`, whose `ty` is the innermost type of `scope`.
    fn push(&self, out: &mut Walked, scope: &Scope, mut info: Info) {
        if self.counts(scope, &info.res) {
            out.candidates.push(info.res.clone());
        }
        info.ty = scope.ty.unwrap_or(info.ident);
        out.infos.push(info);
    }

    /// The lifetimes of `node`, in the order rustc's `walk_unambig_ty` visits them.
    fn ty(&mut self, node: &SyntaxNode, scope: &Scope, out: &mut Walked) {
        let mut scope = scope.clone();
        if ast::Type::can_cast(node.kind()) {
            scope.ty = Some(node.text_range());
        }
        match node.kind() {
            SyntaxKind::CONST_ARG | SyntaxKind::FOR_BINDER => return,
            SyntaxKind::FN_PTR_TYPE => scope.nested = true,
            // `Fn(..) -> ..`, whose return type is the segment's, not the argument list's.
            SyntaxKind::PATH_SEGMENT if node.children().any(|it| it.kind() == SyntaxKind::PARENTHESIZED_ARG_LIST) => {
                scope.nested = true
            }
            SyntaxKind::IMPL_TRAIT_TYPE if !self.output => scope.impl_trait = true,
            _ => {}
        }
        if let Some(binder) = node.children().find(|it| it.kind() == SyntaxKind::FOR_BINDER) {
            for param in binder.descendants().filter_map(ast::LifetimeParam::cast) {
                if let Some(lifetime) = param.lifetime() {
                    scope.bound.push(lifetime.text().to_string());
                }
            }
        }
        if let Some(r) = ast::RefType::cast(node.clone()) {
            let referent = r.ty().map(|it| it.syntax().text_range().start());
            let (res, syntax, ident, name) = match r.lifetime() {
                Some(lifetime) => {
                    (self.named(&lifetime, &scope), syntax_of(&lifetime), lifetime.syntax().text_range(), lifetime.text().to_string())
                }
                None => {
                    let at = r.amp_token().map_or(node.text_range().start(), |it| it.text_range().end());
                    (self.elided(&scope), Syntax::Implicit, TextRange::empty(at), String::new())
                }
            };
            if let Some(res) = res {
                if let Some(inner) = r.ty() {
                    out.refs.push((inner.syntax().clone(), res.clone()));
                }
                let info = Info { res, name, syntax, source: Source::Reference, ident, ty: ident, referent };
                self.push(out, &scope, info);
            }
            if let Some(inner) = r.ty() {
                self.ty(inner.syntax(), &scope, out);
            }
            return;
        }
        if let Some(path) = ast::PathType::cast(node.clone()).and_then(|it| it.path()) {
            let hidden = self.hidden(&path);
            if hidden > 0 {
                let segment = path.segment();
                let args = segment.as_ref().and_then(|it| it.generic_arg_list());
                let (angle, ident) = match &args {
                    None => {
                        let name = segment.as_ref().and_then(|it| it.name_ref()).map(|it| it.syntax().text_range());
                        (Angle::Missing, name.unwrap_or(node.text_range()))
                    }
                    Some(args) => {
                        let after = args.l_angle_token().map_or(args.syntax().text_range().start(), |it| it.text_range().end());
                        let angle = if args.generic_args().next().is_none() { Angle::Empty } else { Angle::Full };
                        (angle, TextRange::empty(after))
                    }
                };
                for _ in 0..hidden {
                    if let Some(res) = self.elided(&scope) {
                        let source = Source::Path(angle);
                        let info = Info { res, name: String::new(), syntax: Syntax::Implicit, source, ident, ty: ident, referent: None };
                        self.push(out, &scope, info);
                    }
                }
            }
        }
        if let Some(lifetime) = ast::Lifetime::cast(node.clone()) {
            let Some(source) = source_of(node) else { return };
            if let Some(res) = self.named(&lifetime, &scope) {
                let (name, syntax, ident) = (lifetime.text().to_string(), syntax_of(&lifetime), node.text_range());
                self.push(out, &scope, Info { res, name, syntax, source, ident, ty: ident, referent: None });
            }
            return;
        }
        for child in node.children() {
            if let Some(lifetime) = ast::Lifetime::cast(child.clone())
                && source_of(&child).is_none()
            {
                // An object lifetime bound or a declaration: elision still counts the former.
                if node.kind() == SyntaxKind::TYPE_BOUND
                    && let Some(res) = self.named(&lifetime, &scope)
                    && self.counts(&scope, &res)
                {
                    out.candidates.push(res);
                }
                continue;
            }
            self.ty(&child, &scope, out);
        }
    }

    /// How many lifetimes a path leaves out: all of its type's, when it gives none.
    fn hidden(&self, path: &ast::Path) -> usize {
        let given = path.segment().and_then(|it| it.generic_arg_list()).is_some_and(|it| it.lifetime_args().next().is_some());
        if given {
            return 0;
        }
        let def = match self.cx.sema.resolve_path(path) {
            Some(PathResolution::Def(ModuleDef::Adt(it))) => hir::GenericDef::Adt(it),
            Some(PathResolution::Def(ModuleDef::TypeAlias(it))) => hir::GenericDef::TypeAlias(it),
            Some(PathResolution::Def(ModuleDef::Trait(it))) => hir::GenericDef::Trait(it),
            _ => return 0,
        };
        def.lifetime_params(self.cx.db).len()
    }
}

/// The source of a lifetime written in a type, by where it is: `None` for a declaration and for
/// a `dyn Trait`'s bound (rustc's `LifetimeSource::Other`), and in a reference (which
/// [`Walk::ty`] reads at the reference).
fn source_of(lifetime: &SyntaxNode) -> Option<Source> {
    let parent = lifetime.parent()?;
    match parent.kind() {
        SyntaxKind::LIFETIME_ARG => Some(Source::Path(Angle::Full)),
        SyntaxKind::USE_BOUND_GENERIC_ARGS => Some(Source::PreciseCapturing),
        SyntaxKind::TYPE_BOUND => {
            let list = parent.parent()?;
            match list.parent()?.kind() {
                SyntaxKind::IMPL_TRAIT_TYPE => Some(Source::OutlivesBound),
                _ => None,
            }
        }
        _ => None,
    }
}

fn syntax_of(lifetime: &ast::Lifetime) -> Syntax {
    match lifetime.text() == "'_" {
        true => Syntax::ExplicitAnonymous,
        false => Syntax::ExplicitBound,
    }
}

/// rustc's `emit_mismatch_diagnostic` of one group.
fn mismatch(cx: &Cx<'_, '_>, item: &Item, inputs: &[Info], outputs: &[Info]) -> Found {
    let file = item.node.file_id;
    let span = |range: TextRange| cx.span(InFile::new(file, range));
    let mut bound: Option<&Info> = None;
    let mut to_bound: Vec<&Info> = Vec::new();
    let mut to_mixed_implicit: Vec<&Info> = Vec::new();
    let mut to_mixed_anonymous: Vec<&Info> = Vec::new();
    let mut to_implicit: Vec<&Info> = Vec::new();
    let mut to_anonymous: Vec<&Info> = Vec::new();
    let mut allow_implicit = true;
    let (mut saw_reference, mut saw_path) = (false, false);
    for info in inputs.iter().chain(outputs) {
        if info.syntax == Syntax::ExplicitBound {
            bound = Some(info);
        }
        match (info.syntax, info.source) {
            (Syntax::Implicit, Source::Reference) => {
                to_anonymous.push(info);
                to_bound.push(info);
            }
            (Syntax::ExplicitAnonymous, Source::Reference) => {
                to_implicit.push(info);
                to_bound.push(info);
            }
            (Syntax::Implicit, Source::Path(_)) => {
                to_mixed_anonymous.push(info);
                to_anonymous.push(info);
                to_bound.push(info);
            }
            (Syntax::ExplicitAnonymous, _) => to_bound.push(info),
            (Syntax::ExplicitBound, Source::Reference) => {
                to_implicit.push(info);
                to_mixed_implicit.push(info);
                to_anonymous.push(info);
            }
            (Syntax::ExplicitBound, _) => {
                to_mixed_anonymous.push(info);
                to_anonymous.push(info);
            }
            (Syntax::Implicit, _) => {}
        }
        if !matches!(info.source, Source::Reference) {
            allow_implicit = false;
        }
        match info.source {
            Source::Reference => saw_reference = true,
            Source::Path(_) => saw_path = true,
            _ => {}
        }
    }
    let categorized = |infos: &[Info]| {
        let mut by = [Vec::new(), Vec::new(), Vec::new()];
        for info in infos {
            let index = match info.category() {
                Category::Hidden => 0,
                Category::Elided => 1,
                Category::Named => 2,
            };
            by[index].push(info.reporting());
        }
        by
    };
    let [in_hidden, in_elided, in_named] = categorized(inputs);
    let [out_hidden, out_elided, out_named] = categorized(outputs);
    let counts = [
        in_hidden.len() + out_hidden.len(),
        in_elided.len() + out_elided.len(),
        in_named.len() + out_named.len(),
    ];
    let message = match counts {
        [_, _, 0] => "hiding a lifetime that's elided elsewhere is confusing",
        [_, 0, _] => "hiding a lifetime that's named elsewhere is confusing",
        [0, _, _] => "eliding a lifetime that's named elsewhere is confusing",
        _ => "hiding or eliding a lifetime that's named elsewhere is confusing",
    };
    let primary: Vec<Span> = in_hidden.iter().chain(&in_elided).chain(&out_hidden).chain(&out_elided).map(|&it| span(it)).collect();
    let first = primary.first().cloned().unwrap_or_else(|| span(item.node.value.syntax().text_range()));
    let mut diag = Diag::lint("mismatched_lifetime_syntaxes", message, first);
    diag.span.primary = primary;
    for &it in &in_hidden {
        diag.span_label(span(it), "the lifetime is hidden here");
    }
    for &it in &in_elided {
        diag.span_label(span(it), "the lifetime is elided here");
    }
    for &it in &in_named {
        diag.span_label(span(it), "the lifetime is named here");
    }
    let mut hidden_counts: Vec<(TextRange, usize)> = Vec::new();
    for &it in &out_hidden {
        match hidden_counts.iter_mut().find(|(range, _)| *range == it) {
            Some((_, count)) => *count += 1,
            None => hidden_counts.push((it, 1)),
        }
    }
    for (range, count) in hidden_counts {
        let label = match count {
            1 => "the same lifetime is hidden here",
            _ => "the same lifetimes are hidden here",
        };
        diag.span_label(span(range), label);
    }
    for &it in &out_elided {
        diag.span_label(span(it), "the same lifetime is elided here");
    }
    for &it in &out_named {
        diag.span_label(span(it), "the same lifetime is named here");
    }
    diag.help("the same lifetime is referred to in inconsistent ways, making the signature confusing");

    let mut suggestions: Vec<(String, Vec<(TextRange, String)>)> = Vec::new();
    if let Some(bound) = bound {
        suggestions.push((format!("consistently use `{}`", bound.name), for_lifetime(&bound.name, &to_bound)));
    }
    let mixed = saw_reference
        && saw_path
        && (!to_mixed_implicit.is_empty() || !to_mixed_anonymous.is_empty())
        && bound.is_none();
    if mixed {
        let message = match to_mixed_implicit.is_empty() {
            true => "use `'_` for type paths",
            false => "remove the lifetime name from references and use `'_` for type paths",
        };
        let mut parts: Vec<(TextRange, String)> = to_mixed_implicit.iter().map(|it| (it.removing(), String::new())).collect();
        parts.extend(for_lifetime("'_", &to_mixed_anonymous));
        suggestions.push((message.to_owned(), parts));
    }
    if !to_implicit.is_empty() && allow_implicit && bound.is_none() {
        let parts = to_implicit.iter().map(|it| (it.removing(), String::new())).collect();
        suggestions.push(("remove the lifetime name from references".to_owned(), parts));
    }
    if !to_anonymous.is_empty() && !mixed && bound.is_none() {
        suggestions.push(("consistently use `'_`".to_owned(), for_lifetime("'_", &to_anonymous)));
    }
    for (index, (message, parts)) in suggestions.into_iter().enumerate() {
        let parts = parts.into_iter().map(|(range, code)| (span(range), code)).collect();
        let (applicability, style) = match index {
            0 => (Applicability::MachineApplicable, SuggestionStyle::ShowAlways),
            _ => (Applicability::MaybeIncorrect, SuggestionStyle::CompletelyHidden),
        };
        diag.suggestion_with_style(parts, message, applicability, style);
    }
    if let Some((id, name)) = super::item_id(cx, item) {
        diag.fact(Fact::Signature(cx.key(id)), format!("the signature of `{name}`"));
    }
    Found::lint(diag, Phase::Late, item.ptr())
}

/// rustc's `build_mismatch_suggestions_for_lifetime`: each lifetime written as `name`, the hidden
/// ones of one path together.
fn for_lifetime(name: &str, infos: &[&Info]) -> Vec<(TextRange, String)> {
    let mut parts = Vec::new();
    let mut paths: Vec<((TextRange, Angle), (TextRange, usize))> = Vec::new();
    let mut index: FxHashMap<(TextRange, Angle), usize> = FxHashMap::default();
    for info in infos {
        if let (Syntax::Implicit, Source::Path(angle)) = (info.syntax, info.source) {
            let at = match angle {
                Angle::Missing => TextRange::empty(info.ident.end()),
                Angle::Empty | Angle::Full => info.ident,
            };
            let key = (info.ty, angle);
            match index.get(&key) {
                Some(&at) => paths[at].1.1 += 1,
                None => {
                    index.insert(key, paths.len());
                    paths.push((key, (at, 1)));
                }
            }
            continue;
        }
        parts.push(info.suggestion(name));
    }
    for ((_, angle), (at, count)) in paths {
        let repeated = vec![name; count].join(", ");
        let code = match angle {
            Angle::Missing => format!("<{repeated}>"),
            Angle::Empty => repeated,
            Angle::Full => format!("{repeated}, "),
        };
        parts.push((at, code));
    }
    parts
}
