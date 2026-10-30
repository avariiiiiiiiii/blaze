//! What is live: from the seeds, each live definition's signature and body walked, every path,
//! method call, field read and pattern in them making what it names live, with rustc's rules: a
//! variant in a pattern is not constructed, nor one a match arm's pattern needs within that arm;
//! a field written is not read, nor one of a struct expression (but of a union's); the fields of
//! a `repr(C)` or `repr(transparent)` type, and the public fields of a public type, are read; a
//! trait's associated types are live with it, and an inherent impl's items make its self type
//! live; the items of a trait impl are live once their trait's item and self type are, and the
//! impls of `#[derive(Clone)]` and `#[derive(Debug)]` read nothing. On the way, an assignment of
//! a variable or a field to itself is linted.
//!
//! The walk is over the syntax of each definition (as rustc's `intravisit` walks its HIR), its
//! macro calls expanded, names resolved through `hir::Semantics` and inference, a trait impl's
//! item named through a trait mapped back to the trait's item as typeck has it.
//!
//! Follows `compiler/rustc_passes/src/dead.rs` (`MarkSymbolVisitor`, `mark_live_symbols`,
//! `check_impl_or_impl_item_live`, `live_symbols_and_ignored_derived_traits`,
//! `check_for_self_assign`), `compiler/rustc_hir/src/pat_util.rs` (`necessary_variants`) and
//! `compiler/rustc_builtin_macros/src/deriving/` (what each derive's method reads)
//! (MIT or Apache-2.0).

use std::ops::ControlFlow;

use either::Either;
use hir::{Adjust, HirFileId, InFile, ModuleDef, PathResolution};
use hir_def::{AdtId, FieldId, builtin_derive::BuiltinDeriveImplTrait};
use rustc_hash::{FxHashMap, FxHashSet};
use syntax::{
    AstNode, SyntaxKind, SyntaxNode, SyntaxNodePtr,
    ast::{self, BinaryOp, HasAttrs, RangeItem},
};

use super::{
    defs::{DefIdx, Id, Kind, Parent, is_anon},
    res::Res,
    seed::{Crate, Seeds, WorkItem},
};
use crate::{
    lower::{Found, Phase},
    model::Diag,
};

/// What is live at one point of the pass.
#[derive(Clone, Debug, Default)]
pub(super) struct Live {
    live: Vec<bool>,
    /// The closures and anonymous constants walked, which items in them have as their parent.
    anon: FxHashSet<InFile<SyntaxNodePtr>>,
    /// rustc's `ignored_derived_traits`: of each ADT, the traits of the derives whose impls
    /// were not walked, in the order they were met.
    pub(super) ignored: FxHashMap<DefIdx, Vec<&'static str>>,
}

impl Live {
    pub(super) fn contains(&self, def: DefIdx) -> bool {
        self.live[def]
    }

    pub(super) fn parent_live(&self, parent: Parent) -> bool {
        match parent {
            Parent::Module => true,
            Parent::Def(it) => self.live[it],
            Parent::Anon(it) => self.anon.contains(&it),
        }
    }
}

/// rustc's `DeadCodeLivenessSummary`: what is live before what is public and what an `allow` or
/// `expect` covers are walked (for `dead_code_pub_in_binary`), and at the end.
pub(super) struct Liveness {
    pub(super) pre_deferred: Live,
    pub(super) last: Live,
}

/// rustc's `live_symbols_and_ignored_derived_traits`, with the self-assignments it lints.
pub(super) fn live(k: &Crate<'_, '_, '_>, seeds: Seeds) -> (Liveness, Vec<Found>) {
    let mut m = Mark {
        k,
        worklist: seeds.worklist,
        scanned: FxHashSet::default(),
        live: Live { live: vec![false; k.defs.len()], ..Live::default() },
        allow: vec![None; k.defs.len()],
        propagated: false,
        in_pat: false,
        ignore_variants: Vec::new(),
        repr_all_live: false,
        repr_simd: false,
        self_assigns: FxHashSet::default(),
        found: Vec::new(),
    };
    let mut unsolved = seeds.unsolved;
    m.mark_with_unsolved(&mut unsolved);
    let pre_deferred = m.live.clone();
    m.worklist.extend(seeds.deferred);
    m.mark_with_unsolved(&mut unsolved);
    (Liveness { pre_deferred, last: m.live }, m.found)
}

struct Mark<'k, 'c, 'a, 'db> {
    k: &'k Crate<'c, 'a, 'db>,
    worklist: Vec<WorkItem>,
    scanned: FxHashSet<(DefIdx, bool)>,
    live: Live,
    /// `has_allow_dead_code_or_lang_attr` of each definition, once asked.
    allow: Vec<Option<Option<bool>>>,
    propagated: bool,
    in_pat: bool,
    ignore_variants: Vec<DefIdx>,
    repr_all_live: bool,
    repr_simd: bool,
    self_assigns: FxHashSet<InFile<SyntaxNodePtr>>,
    found: Vec<Found>,
}

impl Mark<'_, '_, '_, '_> {
    fn allow_or_lang(&mut self, def: DefIdx) -> Option<bool> {
        if let Some(it) = self.allow[def] {
            return it;
        }
        let it = self.k.allow_or_lang(def);
        self.allow[def] = Some(it);
        it
    }

    /// rustc's `own_comes_from_allow_expect`.
    fn own(&mut self, def: DefIdx) -> bool {
        self.propagated && self.allow_or_lang(def) == Some(true)
    }

    fn check_def_id(&mut self, def: DefIdx) {
        let own = self.own(def);
        if self.k.defs[def].kind != Kind::Field {
            self.worklist.push(WorkItem { id: def, propagated: self.propagated, own });
        }
        if !own {
            self.live.live[def] = true;
        }
    }

    fn insert_def_id(&mut self, def: DefIdx) {
        if !self.own(def) {
            self.live.live[def] = true;
        }
    }

    /// rustc's `handle_res`: in a pattern, only a constant or a type alias it names is used.
    fn handle_res(&mut self, res: Res) {
        match res {
            Res::Def(def) => {
                let in_pat_used = matches!(self.k.defs[def].kind, Kind::Const | Kind::AssocConst | Kind::TyAlias);
                if !self.in_pat || in_pat_used {
                    self.check_def_id(def);
                }
            }
            Res::Variant(variant) => {
                if self.in_pat {
                    return;
                }
                if let Parent::Def(adt) = self.k.defs[variant].parent {
                    self.check_def_id(adt);
                }
                if !self.ignore_variants.contains(&variant) {
                    self.check_def_id(variant);
                }
            }
            Res::SelfCtor | Res::None => {}
        }
    }

    /// `mark_live_symbols_and_ignored_derived_traits`: the worklist, then the trait impls and
    /// their items whose trait and self type are live, until none is left that is.
    fn mark_with_unsolved(&mut self, unsolved: &mut Vec<DefIdx>) {
        self.mark_live_symbols();
        let mut to_check = self.extract_live(unsolved);
        while !to_check.is_empty() {
            self.worklist.extend(to_check.drain(..).map(|id| WorkItem::new(id, false)));
            self.mark_live_symbols();
            to_check.extend(self.extract_live(unsolved));
        }
    }

    fn extract_live(&self, unsolved: &mut Vec<DefIdx>) -> Vec<DefIdx> {
        let mut live = Vec::new();
        unsolved.retain(|&it| {
            let is_live = self.impl_or_impl_item_live(it);
            if is_live {
                live.push(it);
            }
            !is_live
        });
        live
    }

    /// rustc's `check_impl_or_impl_item_live`.
    fn impl_or_impl_item_live(&self, def: DefIdx) -> bool {
        let defs = self.k.defs;
        let (impl_, trait_def) = match defs[def].kind {
            Kind::AssocFn { .. } | Kind::AssocConst | Kind::AssocTy => {
                let Parent::Def(impl_) = defs[def].parent else { return false };
                (impl_, self.k.trait_item_of(def))
            }
            _ => (def, self.k.impl_trait(def)),
        };
        if trait_def.is_some_and(|it| !self.live.live[it]) {
            return false;
        }
        self.k.impl_self_adt(impl_).is_none_or(|it| self.live.live[it])
    }

    fn mark_live_symbols(&mut self) {
        while let Some(work) = self.worklist.pop() {
            self.propagated = work.propagated;
            if !work.own {
                self.live.live[work.id] = true;
            }
            if !self.scanned.insert((work.id, work.propagated)) {
                continue;
            }
            self.visit(work.id);
        }
    }

    /// rustc's `visit_node` of a definition taken from the worklist.
    fn visit(&mut self, def: DefIdx) {
        let k = self.k;
        let db = k.cx.db;
        let defs = k.defs;
        let d = &defs[def];
        match d.id {
            Id::DeriveMethod(derive) => {
                let loc = derive.loc(db);
                let Some(adt) = defs.get(Id::Adt(loc.adt)) else { return };
                self.derived(adt, loc.trait_);
                return;
            }
            Id::Derive(derive) => {
                if let Some(adt) = defs.get(Id::Adt(derive.loc(db).adt)) {
                    self.check_def_id(adt);
                }
                return;
            }
            Id::Variant(_) | Id::Field(_) => return,
            _ => {}
        }
        let Some(ptr) = d.node else { return };
        let node = k.cx.syntax(ptr);
        let saved = (self.repr_all_live, self.repr_simd);
        self.repr_all_live = false;
        self.repr_simd = false;
        match d.kind {
            Kind::Struct | Kind::Union => {
                let (all, simd) = repr(k, &node.value);
                self.repr_all_live = all;
                self.repr_simd = simd;
                self.variant_data(def, def);
            }
            Kind::Enum => {
                for &variant in &d.children {
                    self.variant_data(def, variant);
                }
            }
            Kind::Trait => {
                for &item in &d.children {
                    if defs[item].kind == Kind::AssocTy {
                        self.check_def_id(item);
                    }
                }
            }
            _ => {}
        }
        if let Parent::Def(parent) = d.parent {
            match defs[parent].kind {
                Kind::Trait => self.check_def_id(parent),
                Kind::Impl { of_trait: false } => self.inherent_self_ty(parent),
                _ => {}
            }
        }
        self.walk(node.file_id, &node.value, true);
        (self.repr_all_live, self.repr_simd) = saved;
    }

    /// An inherent impl's item makes its self type live: an ADT, a foreign type, the principal
    /// trait of a `dyn`.
    fn inherent_self_ty(&mut self, impl_: DefIdx) {
        let k = self.k;
        let Id::Impl(id) = k.defs[impl_].id else { return };
        let self_ty = hir::Impl::from(id).self_ty(k.cx.db);
        let named = match (self_ty.as_adt(), self_ty.as_dyn_trait()) {
            (Some(adt), _) => k.defs.get(Id::Adt(AdtId::from(adt))),
            (None, Some(trait_)) => k.defs.get(Id::Trait(trait_.into())),
            (None, None) => None,
        };
        if let Some(it) = named {
            self.check_def_id(it);
        }
    }

    /// rustc's `visit_variant_data`: the fields of `variant` (of the ADT `adt`) live whatever
    /// reads them: all of a `repr(C)` or `repr(transparent)` type's, the positional ones of a
    /// `repr(simd)` type's, the public ones of a public type's.
    fn variant_data(&mut self, adt: DefIdx, variant: DefIdx) {
        let defs = self.k.defs;
        for &field in &defs[variant].children {
            let positional = defs[field].name.as_deref().is_some_and(|it| it.starts_with(|c: char| c.is_ascii_digit()));
            let live = self.repr_all_live
                || (positional && self.repr_simd)
                || (self.k.reach.is_reachable(adt) && self.k.reach.is_reachable(field));
            if live {
                self.live.live[field] = true;
            }
        }
    }

    /// What the method of a builtin derive's impl reads, as the code the derive writes does:
    /// `Clone` and `Debug` are `rustc_trivial_field_reads` and not walked; `PartialEq`,
    /// `PartialOrd`, `Ord` and `Hash` read every field; `Default` constructs the struct, or the
    /// enum's `#[default]` variant.
    fn derived(&mut self, adt: DefIdx, trait_: BuiltinDeriveImplTrait) {
        let defs = self.k.defs;
        match trait_ {
            BuiltinDeriveImplTrait::Clone | BuiltinDeriveImplTrait::Debug => {
                let name = match trait_ {
                    BuiltinDeriveImplTrait::Clone => "Clone",
                    _ => "Debug",
                };
                let ignored = self.live.ignored.entry(adt).or_default();
                if !ignored.contains(&name) {
                    ignored.push(name);
                }
            }
            BuiltinDeriveImplTrait::PartialEq
            | BuiltinDeriveImplTrait::PartialOrd
            | BuiltinDeriveImplTrait::Ord
            | BuiltinDeriveImplTrait::Hash => {
                let fields: Vec<DefIdx> = match defs[adt].kind {
                    Kind::Enum => defs[adt].children.iter().flat_map(|&it| defs[it].children.iter().copied()).collect(),
                    _ => defs[adt].children.clone(),
                };
                for field in fields {
                    self.insert_def_id(field);
                }
            }
            BuiltinDeriveImplTrait::Default => {
                let default = defs[adt].children.iter().copied().find(|&variant| {
                    defs[variant].kind == Kind::Variant
                        && defs[variant].node.is_some_and(|it| {
                            let node = self.k.cx.syntax(it);
                            ast::Variant::cast(node.value).is_some_and(|it| {
                                it.attrs().any(|attr| attr.simple_name().is_some_and(|it| it == "default"))
                            })
                        })
                });
                match default {
                    Some(variant) => self.handle_res(Res::Variant(variant)),
                    None => self.check_def_id(adt),
                }
            }
            BuiltinDeriveImplTrait::Copy
            | BuiltinDeriveImplTrait::Eq
            | BuiltinDeriveImplTrait::CoerceUnsized
            | BuiltinDeriveImplTrait::DispatchFromDyn
            | BuiltinDeriveImplTrait::Reborrow => {}
        }
    }

    /// rustc's `intravisit` walk of `node` with the visitor's methods: the items in it are
    /// definitions of their own and not walked, nor attributes; macro calls are walked as what
    /// they expand to.
    fn walk(&mut self, file: HirFileId, node: &SyntaxNode, root: bool) {
        let kind = node.kind();
        if kind == SyntaxKind::ATTR {
            return;
        }
        if kind == SyntaxKind::MACRO_CALL {
            let in_item_list = node.parent().is_some_and(|it| {
                matches!(
                    it.kind(),
                    SyntaxKind::SOURCE_FILE
                        | SyntaxKind::ITEM_LIST
                        | SyntaxKind::ASSOC_ITEM_LIST
                        | SyntaxKind::EXTERN_ITEM_LIST
                        | SyntaxKind::MACRO_ITEMS
                )
            });
            if in_item_list && !root {
                return;
            }
            if let Some(call) = ast::MacroCall::cast(node.clone())
                && let Some(expansion) = self.k.cx.sema.expand_macro_call(&call)
            {
                stdx::ensure_sufficient_stack(|| self.walk(expansion.file_id, &expansion.value, false));
            }
            return;
        }
        if !root && ast::Item::can_cast(kind) {
            return;
        }
        if is_anon(node) {
            self.anon(file, node);
            return;
        }
        if ast::Pat::can_cast(kind) {
            self.pat(file, node);
            return;
        }
        match kind {
            SyntaxKind::PATH => {
                let path = ast::Path::cast(node.clone()).expect("a path");
                self.path(&path);
            }
            SyntaxKind::METHOD_CALL_EXPR => {
                let call = ast::MethodCallExpr::cast(node.clone()).expect("a method call");
                if let Some(function) = self.k.cx.sema.resolve_method_call(&call) {
                    let res = Res::function(self.k.cx, self.k.defs, function);
                    self.handle_res(res);
                }
            }
            SyntaxKind::FIELD_EXPR => {
                let field = ast::FieldExpr::cast(node.clone()).expect("a field expression");
                self.field_read(&field);
            }
            SyntaxKind::RECORD_EXPR => {
                let expr = ast::RecordExpr::cast(node.clone()).expect("a record expression");
                self.union_fields(&expr);
            }
            SyntaxKind::OFFSET_OF_EXPR => {
                let expr = ast::OffsetOfExpr::cast(node.clone()).expect("an `offset_of!`");
                for name in expr.fields() {
                    if let Some((Either::Right(field), _)) = self.k.cx.sema.resolve_offset_of_field(&name)
                        && let Some(field) = self.k.defs.get(Id::Field(FieldId::from(field)))
                    {
                        self.insert_def_id(field);
                    }
                }
            }
            SyntaxKind::BIN_EXPR => {
                let expr = ast::BinExpr::cast(node.clone()).expect("a binary expression");
                if let Some(BinaryOp::Assignment { op: None }) = expr.op_kind() {
                    self.assign(file, &expr);
                    return;
                }
            }
            SyntaxKind::MATCH_ARM => {
                let arm = ast::MatchArm::cast(node.clone()).expect("a match arm");
                let len = self.ignore_variants.len();
                if let Some(pat) = arm.pat() {
                    let mut needed = Vec::new();
                    self.necessary_variants(pat.syntax(), &mut needed);
                    for variant in needed {
                        if !self.ignore_variants[len..].contains(&variant) {
                            self.ignore_variants.push(variant);
                        }
                    }
                }
                self.children(file, node);
                self.ignore_variants.truncate(len);
                return;
            }
            _ => {}
        }
        self.children(file, node);
    }

    fn children(&mut self, file: HirFileId, node: &SyntaxNode) {
        for child in node.children() {
            stdx::ensure_sufficient_stack(|| self.walk(file, &child, false));
        }
    }

    /// A closure (live unless an `allow` it has came with what reached it) or an anonymous
    /// constant (live), walked out of any pattern it is in.
    fn anon(&mut self, file: HirFileId, node: &SyntaxNode) {
        let ptr = InFile::new(file, SyntaxNodePtr::new(node));
        let closure = node.kind() == SyntaxKind::CLOSURE_EXPR;
        let own = closure
            && self.propagated
            && self.k.levels.at(self.k.cx, &InFile::new(file, node.clone())).is_allow_or_expect();
        if !own {
            self.live.anon.insert(ptr);
        }
        let in_pat = std::mem::replace(&mut self.in_pat, false);
        self.children(file, node);
        self.in_pat = in_pat;
    }

    /// rustc's `visit_path` (and `visit_expr` of a type-relative path): what `path` resolves
    /// to is live.
    fn path(&mut self, path: &ast::Path) {
        let cx = self.k.cx;
        let parent = path.syntax().parent().map(|it| it.kind());
        if matches!(parent, Some(SyntaxKind::MACRO_CALL | SyntaxKind::META)) {
            return;
        }
        let Some(res) = cx.sema.resolve_path(path) else { return };
        let value_or_pat = matches!(
            parent,
            Some(SyntaxKind::PATH_EXPR | SyntaxKind::PATH_PAT | SyntaxKind::TUPLE_STRUCT_PAT)
        );
        let self_ctor = value_or_pat && path.qualifier().is_none() && is_self(path);
        let res = Res::path(cx, self.k.defs, res, self_ctor);
        self.handle_res(res);
    }

    /// rustc's `handle_field_access`: a field of a struct or a union read.
    fn field_read(&mut self, field: &ast::FieldExpr) {
        if let Some(Either::Left(field)) = self.k.cx.sema.resolve_field(field)
            && let Some(field) = self.k.defs.get(Id::Field(FieldId::from(field)))
        {
            self.insert_def_id(field);
        }
    }

    /// rustc's `mark_as_used_if_union`: the fields a union expression names, of a union of
    /// more than one field, are read.
    fn union_fields(&mut self, expr: &ast::RecordExpr) {
        let cx = self.k.cx;
        let Some(ty) = cx.sema.type_of_expr(&ast::Expr::RecordExpr(expr.clone())) else { return };
        let Some(hir::Adt::Union(union)) = ty.original.as_adt() else { return };
        if union.fields(cx.db).len() <= 1 {
            return;
        }
        for field in expr.record_expr_field_list().into_iter().flat_map(|it| it.fields()) {
            if let Some((field, _, _)) = cx.sema.resolve_record_field(&field)
                && let Some(field) = self.k.defs.get(Id::Field(FieldId::from(field)))
            {
                self.insert_def_id(field);
            }
        }
    }

    /// rustc's `visit_pat`: what a pattern's paths name, its ADT, and the fields it binds.
    fn pat(&mut self, file: HirFileId, node: &SyntaxNode) {
        let cx = self.k.cx;
        self.in_pat = true;
        match ast::Pat::cast(node.clone()) {
            Some(ast::Pat::RecordPat(pat)) => {
                self.pat_adt(&ast::Pat::RecordPat(pat.clone()));
                if let Some(path) = pat.path() {
                    self.path(&path);
                }
                for field in pat.record_pat_field_list().into_iter().flat_map(|it| it.fields()) {
                    if matches!(field.pat(), Some(ast::Pat::WildcardPat(_))) {
                        continue;
                    }
                    if let Some((field, _)) = cx.sema.resolve_record_pat_field(&field)
                        && let Some(field) = self.k.defs.get(Id::Field(FieldId::from(field)))
                    {
                        self.insert_def_id(field);
                    }
                }
                if let Some(list) = pat.record_pat_field_list() {
                    self.children(file, list.syntax());
                }
            }
            Some(ast::Pat::TupleStructPat(pat)) => {
                self.pat_adt(&ast::Pat::TupleStructPat(pat.clone()));
                let variant = pat.path().and_then(|path| {
                    self.path(&path);
                    let res = Res::path(cx, self.k.defs, cx.sema.resolve_path(&path)?, false);
                    match res {
                        Res::Def(it) | Res::Variant(it) => Some(it),
                        Res::SelfCtor | Res::None => None,
                    }
                });
                let subpats: Vec<ast::Pat> = pat.fields().collect();
                if let Some(variant) = variant {
                    self.tuple_fields(variant, &subpats);
                }
                for sub in subpats {
                    stdx::ensure_sufficient_stack(|| self.walk(file, sub.syntax(), false));
                    self.in_pat = true;
                }
            }
            Some(pat @ ast::Pat::PathPat(_)) => {
                self.pat_adt(&pat);
                self.children(file, node);
            }
            Some(ast::Pat::IdentPat(pat)) => {
                if let Some(def) = cx.sema.resolve_bind_pat_to_const(&pat) {
                    self.pat_adt(&ast::Pat::IdentPat(pat.clone()));
                    let res = Res::def(cx, self.k.defs, def);
                    self.handle_res(res);
                }
                self.children(file, node);
            }
            _ => self.children(file, node),
        }
        self.in_pat = false;
    }

    /// The fields a tuple struct pattern's subpatterns bind (not `_`), past its `..`.
    fn tuple_fields(&mut self, variant: DefIdx, subpats: &[ast::Pat]) {
        let defs = self.k.defs;
        let fields = &defs[variant].children;
        let dotdot = subpats.iter().position(|it| matches!(it, ast::Pat::RestPat(_)));
        let pats: Vec<&ast::Pat> = subpats.iter().filter(|it| !matches!(it, ast::Pat::RestPat(_))).collect();
        let dotdot = dotdot.unwrap_or(pats.len());
        let missing = fields.len().saturating_sub(pats.len());
        for (index, pat) in pats.iter().enumerate() {
            if matches!(pat, ast::Pat::WildcardPat(_)) {
                continue;
            }
            let index = if index < dotdot { index } else { index + missing };
            if let Some(&field) = fields.get(index) {
                self.insert_def_id(field);
            }
        }
    }

    /// The ADT a pattern is of is live (rustc's `check_def_id(adt.did())` of a struct, tuple
    /// struct or path pattern).
    fn pat_adt(&mut self, pat: &ast::Pat) {
        let Some(ty) = self.k.cx.sema.type_of_pat(pat) else { return };
        if let Some(adt) = ty.original.as_adt()
            && let Some(def) = self.k.defs.get(Id::Adt(AdtId::from(adt)))
        {
            self.check_def_id(def);
        }
    }

    /// rustc's `necessary_variants` of a match arm's pattern: the variants its paths name
    /// (resolved, not through a type), outside `|` patterns.
    fn necessary_variants(&self, node: &SyntaxNode, out: &mut Vec<DefIdx>) {
        let cx = self.k.cx;
        let path = match node.kind() {
            SyntaxKind::OR_PAT => return,
            SyntaxKind::PATH_PAT => ast::PathPat::cast(node.clone()).and_then(|it| it.path()),
            SyntaxKind::TUPLE_STRUCT_PAT => ast::TupleStructPat::cast(node.clone()).and_then(|it| it.path()),
            SyntaxKind::RECORD_PAT => ast::RecordPat::cast(node.clone()).and_then(|it| it.path()),
            SyntaxKind::IDENT_PAT => {
                if let Some(pat) = ast::IdentPat::cast(node.clone())
                    && let Some(ModuleDef::EnumVariant(it)) = cx.sema.resolve_bind_pat_to_const(&pat)
                    && let Some(variant) = self.k.defs.get(Id::Variant(it.into()))
                    && !out.contains(&variant)
                {
                    out.push(variant);
                }
                None
            }
            _ => None,
        };
        if let Some(path) = path
            && self.is_resolved(&path)
            && let Some(res) = cx.sema.resolve_path(&path)
            && let Res::Variant(variant) = Res::path(cx, self.k.defs, res, false)
            && !out.contains(&variant)
        {
            out.push(variant);
        }
        for child in node.children() {
            if ast::Pat::can_cast(child.kind()) || child.kind() == SyntaxKind::RECORD_PAT_FIELD_LIST {
                self.necessary_variants(&child, out);
            } else if child.kind() == SyntaxKind::RECORD_PAT_FIELD {
                for it in child.children() {
                    self.necessary_variants(&it, out);
                }
            }
        }
    }

    /// Whether rustc's resolver resolves `path` (`QPath::Resolved`), rather than typeck through
    /// a type (`QPath::TypeRelative`: `Self::A`, `Alias::A`, `<E>::A`).
    fn is_resolved(&self, path: &ast::Path) -> bool {
        let Some(qualifier) = path.qualifier() else { return true };
        if qualifier.segments().any(|it| it.type_anchor().is_some()) {
            return false;
        }
        matches!(
            self.k.cx.sema.resolve_path(&qualifier),
            Some(PathResolution::Def(ModuleDef::Module(_) | ModuleDef::Adt(hir::Adt::Enum(_))))
        ) && !is_self(&qualifier)
    }

    /// `lhs = rhs`: rustc's `handle_assign` of the place written (its fields not read), or the
    /// pattern a destructuring assignment is lowered to; then `rhs`, and the self-assignment
    /// lint.
    fn assign(&mut self, file: HirFileId, expr: &ast::BinExpr) {
        if let Some(lhs) = expr.lhs() {
            match destructures(&lhs) {
                true => self.assign_pattern(file, &lhs),
                false => self.handle_assign(file, &lhs),
            }
            if let Some(rhs) = expr.rhs() {
                if !destructures(&lhs) {
                    self.self_assign(file, expr, &lhs, &rhs);
                }
                stdx::ensure_sufficient_stack(|| self.walk(file, rhs.syntax(), false));
            }
        } else if let Some(rhs) = expr.rhs() {
            self.walk(file, rhs.syntax(), false);
        }
    }

    fn handle_assign(&mut self, file: HirFileId, place: &ast::Expr) {
        let derefs = self
            .k
            .cx
            .sema
            .expr_adjustments(place)
            .is_some_and(|it| it.iter().any(|it| matches!(it.kind, Adjust::Deref(_))));
        match place {
            ast::Expr::FieldExpr(field) if !derefs => {
                if let Some(base) = field.expr() {
                    self.handle_assign(file, &base);
                }
            }
            ast::Expr::ParenExpr(it) if !derefs => {
                if let Some(inner) = it.expr() {
                    self.handle_assign(file, &inner);
                }
            }
            _ => stdx::ensure_sufficient_stack(|| self.walk(file, place.syntax(), false)),
        }
    }

    /// The left of a destructuring assignment as the pattern rustc lowers it to, each place in
    /// it assigned.
    fn assign_pattern(&mut self, file: HirFileId, lhs: &ast::Expr) {
        let cx = self.k.cx;
        match lhs {
            ast::Expr::ParenExpr(it) => {
                if let Some(inner) = it.expr() {
                    self.assign_pattern(file, &inner);
                }
            }
            ast::Expr::TupleExpr(it) => {
                for field in it.fields() {
                    self.assign_pattern(file, &field);
                }
            }
            ast::Expr::ArrayExpr(it) => {
                for field in it.exprs() {
                    self.assign_pattern(file, &field);
                }
            }
            ast::Expr::UnderscoreExpr(_) => {}
            ast::Expr::RangeExpr(it) if it.start().is_none() && it.end().is_none() => {}
            ast::Expr::RecordExpr(it) => {
                self.expr_adt(lhs);
                if let Some(path) = it.path() {
                    self.in_pat = true;
                    self.path(&path);
                    self.in_pat = false;
                }
                for field in it.record_expr_field_list().into_iter().flat_map(|it| it.fields()) {
                    let value = field.expr();
                    if matches!(value, Some(ast::Expr::UnderscoreExpr(_))) {
                        continue;
                    }
                    if let Some((def, _, _)) = cx.sema.resolve_record_field(&field)
                        && let Some(def) = self.k.defs.get(Id::Field(FieldId::from(def)))
                    {
                        self.insert_def_id(def);
                    }
                    if let Some(value) = value {
                        self.assign_pattern(file, &value);
                    }
                }
            }
            ast::Expr::CallExpr(it) => {
                let path = match it.expr() {
                    Some(ast::Expr::PathExpr(it)) => it.path(),
                    _ => None,
                };
                let Some(path) = path else {
                    self.handle_assign(file, lhs);
                    return;
                };
                self.expr_adt(lhs);
                self.in_pat = true;
                self.path(&path);
                self.in_pat = false;
                let res = cx.sema.resolve_path(&path).map(|it| Res::path(cx, self.k.defs, it, false));
                let args: Vec<ast::Expr> = it.arg_list().into_iter().flat_map(|it| it.args()).collect();
                if let Some(Res::Def(variant) | Res::Variant(variant)) = res {
                    let defs = self.k.defs;
                    let fields = &defs[variant].children;
                    let is_rest = |it: &ast::Expr| {
                        matches!(it, ast::Expr::RangeExpr(it) if it.start().is_none() && it.end().is_none())
                    };
                    let dotdot = args.iter().position(is_rest);
                    let pats: Vec<&ast::Expr> = args.iter().filter(|it| !is_rest(it)).collect();
                    let dotdot = dotdot.unwrap_or(pats.len());
                    let missing = fields.len().saturating_sub(pats.len());
                    for (index, pat) in pats.iter().enumerate() {
                        if matches!(pat, ast::Expr::UnderscoreExpr(_)) {
                            continue;
                        }
                        let index = if index < dotdot { index } else { index + missing };
                        if let Some(&field) = fields.get(index) {
                            self.insert_def_id(field);
                        }
                    }
                }
                for arg in &args {
                    self.assign_pattern(file, arg);
                }
            }
            ast::Expr::PathExpr(it) => {
                let constructs = it.path().and_then(|path| cx.sema.resolve_path(&path)).is_some_and(|it| {
                    matches!(it, PathResolution::Def(ModuleDef::Adt(_) | ModuleDef::EnumVariant(_)))
                });
                match (constructs, it.path()) {
                    (true, Some(path)) => {
                        self.expr_adt(lhs);
                        self.in_pat = true;
                        self.path(&path);
                        self.in_pat = false;
                    }
                    _ => self.handle_assign(file, lhs),
                }
            }
            _ => self.handle_assign(file, lhs),
        }
    }

    fn expr_adt(&mut self, expr: &ast::Expr) {
        let Some(ty) = self.k.cx.sema.type_of_expr(expr) else { return };
        if let Some(adt) = ty.original.as_adt()
            && let Some(def) = self.k.defs.get(Id::Adt(AdtId::from(adt)))
        {
            self.check_def_id(def);
        }
    }

    /// rustc's `check_for_self_assign`: `x = x` or `a.b = a.b`, not from a macro's expansion.
    fn self_assign(&mut self, file: HirFileId, expr: &ast::BinExpr, lhs: &ast::Expr, rhs: &ast::Expr) {
        let cx = self.k.cx;
        if !same_place(cx, lhs, rhs) {
            return;
        }
        if crate::lints::syntax::from_expansion(cx, InFile::new(file, expr.syntax())) {
            return;
        }
        let at = InFile::new(file, SyntaxNodePtr::new(expr.syntax()));
        if !self.self_assigns.insert(at) {
            return;
        }
        let Some(ty) = cx.sema.type_of_expr(lhs) else { return };
        let what = match unparen(lhs) {
            ast::Expr::FieldExpr(_) => "field",
            _ => "variable",
        };
        let message = format!("useless assignment of {what} of type `{}` to itself", cx.ty(&ty.original));
        let mut diag = Diag::lint("dead_code", message, cx.span_of(at));
        crate::lints::code_fact(cx, &mut diag, at);
        self.found.push(Found::lint(diag, Phase::Late, at));
    }
}

/// Whether the left of an assignment is a pattern rustc lowers a destructuring assignment to.
fn destructures(lhs: &ast::Expr) -> bool {
    match lhs {
        ast::Expr::ParenExpr(it) => it.expr().is_some_and(|it| destructures(&it)),
        ast::Expr::TupleExpr(_)
        | ast::Expr::ArrayExpr(_)
        | ast::Expr::RecordExpr(_)
        | ast::Expr::CallExpr(_)
        | ast::Expr::UnderscoreExpr(_) => true,
        _ => false,
    }
}

fn unparen(expr: &ast::Expr) -> ast::Expr {
    match expr {
        ast::Expr::ParenExpr(it) => it.expr().map_or_else(|| expr.clone(), |it| unparen(&it)),
        _ => expr.clone(),
    }
}

/// `check_for_self_assign_helper`: the same local, or the same field of the same place.
fn same_place(cx: &crate::cx::Cx<'_, '_>, lhs: &ast::Expr, rhs: &ast::Expr) -> bool {
    match (unparen(lhs), unparen(rhs)) {
        (ast::Expr::PathExpr(l), ast::Expr::PathExpr(r)) => {
            let local = |it: &ast::PathExpr| match cx.sema.resolve_path(&it.path()?)? {
                PathResolution::Local(it) => Some(it),
                _ => None,
            };
            matches!((local(&l), local(&r)), (Some(l), Some(r)) if l == r)
        }
        (ast::Expr::FieldExpr(l), ast::Expr::FieldExpr(r)) => {
            let name = |it: &ast::FieldExpr| it.name_ref().map(|it| it.text().to_string());
            match (name(&l), name(&r), l.expr(), r.expr()) {
                (Some(a), Some(b), Some(l), Some(r)) if a == b => same_place(cx, &l, &r),
                _ => false,
            }
        }
        _ => false,
    }
}

/// Whether `path` is the one segment `Self`.
fn is_self(path: &ast::Path) -> bool {
    path.segment().is_some_and(|it| it.self_type_token().is_some())
}

/// What the `#[repr]`s of a struct or a union make live: all its fields (`C`, `transparent`),
/// and its positional ones (`simd`).
fn repr(k: &Crate<'_, '_, '_>, node: &SyntaxNode) -> (bool, bool) {
    let cx = k.cx;
    let (mut all, mut simd) = (false, false);
    let Some(owner) = ast::AnyHasAttrs::cast(node.clone()) else { return (false, false) };
    let cfg = cx.krate.cfg(cx.db);
    hir_expand::attrs::expand_cfg_attr::<()>(owner.attrs(), || cfg, |meta, _| {
        if meta.simple_name().is_some_and(|it| it == "repr")
            && let ast::Meta::TokenTreeMeta(meta) = meta
            && let Some(tt) = meta.token_tree()
        {
            for token in tt.syntax().descendants_with_tokens().filter_map(|it| it.into_token()) {
                match token.text() {
                    "C" | "transparent" => all = true,
                    "simd" => simd = true,
                    _ => {}
                }
            }
        }
        ControlFlow::Continue(())
    });
    (all, simd)
}
