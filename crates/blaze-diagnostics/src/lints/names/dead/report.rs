//! What is reported dead: of each module, its items that are not live (not under a parent
//! already reported), the dead items of its inherent impls and live traits together, the dead
//! variants of a live enum together, and the dead fields of each live variant together, each
//! group split by the level of the lint at its items; with the label of the item they are in,
//! the derives whose impls the pass ignored, how to remove a tuple struct's fields, and the enum
//! variant a dead associated item of an enum's impl is shadowed by. In an executable, first
//! what is public but used only through what is public, as `dead_code_pub_in_binary`.
//!
//! Follows `compiler/rustc_passes/src/dead.rs` (`check_mod_deathness`, `lint_dead_codes`,
//! `DeadVisitor`) and `compiler/rustc_passes/src/diagnostics.rs` (`MultipleDeadCodes`,
//! `ParentInfo`, `IgnoredDerivedImpls`, `ChangeFields`, `EnumVariantSameName`,
//! `DeadCodePubInBinaryNote`) (MIT or Apache-2.0).

use hir::InFile;
use hir_def::{AdtId, signatures::{StructFlags, StructSignature}};
use syntax::{
    AstNode, TextRange,
    ast::{self, HasName},
};

use super::{
    defs::{DefIdx, Id, Kind, Parent},
    level::LevelPlus,
    mark::{Live, Liveness},
    seed::Crate,
};
use crate::{
    lints::registry::Level,
    lower::{Found, Phase},
    model::{Applicability, Diag, Span},
};

/// rustc's `check_mod_deathness` of every module.
pub(super) fn report(k: &Crate<'_, '_, '_>, liveness: &Liveness) -> Vec<Found> {
    let mut found = Vec::new();
    for module in 0..k.defs.modules.len() {
        if k.executable == Some(true) {
            let pre = &liveness.pre_deferred;
            let unused_pub = |def: DefIdx| k.reach.is_reachable(def) && !pre.contains(def);
            Report { k, lint: Lint::PubInBinary, live: pre, found: &mut found }.module(module, &unused_pub);
        }
        Report { k, lint: Lint::DeadCode, live: &liveness.last, found: &mut found }.module(module, &|_| true);
    }
    found
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Lint {
    DeadCode,
    PubInBinary,
}

impl Lint {
    fn name(self) -> &'static str {
        match self {
            Lint::DeadCode => "dead_code",
            Lint::PubInBinary => "dead_code_pub_in_binary",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ReportOn {
    TupleField,
    NamedField,
}

struct DeadItem {
    def: DefIdx,
    name: String,
    level: LevelPlus,
}

struct Report<'r, 'c, 'a, 'db> {
    k: &'r Crate<'c, 'a, 'db>,
    lint: Lint,
    live: &'r Live,
    found: &'r mut Vec<Found>,
}

impl Report<'_, '_, '_, '_> {
    /// rustc's `lint_dead_codes` of the free and foreign items of a module that `keep` keeps.
    fn module(&mut self, module: usize, keep: &dyn Fn(DefIdx) -> bool) {
        let defs = self.k.defs;
        for &item in &defs.modules[module].free {
            if !keep(item) {
                continue;
            }
            let d = &defs[item];
            let mut dead_codes = Vec::new();
            if d.kind == (Kind::Impl { of_trait: false }) || (d.kind == Kind::Trait && self.live.contains(item)) {
                for &child in &d.children {
                    if !self.is_live_code(child) {
                        dead_codes.push(self.dead_item(child));
                    }
                }
            }
            if !dead_codes.is_empty() {
                self.warn_multiple(item, "used", dead_codes, ReportOn::NamedField);
            }
            if !self.live.contains(item) {
                if !self.live.parent_live(d.parent) {
                    continue;
                }
                self.check_definition(item);
                continue;
            }
            if matches!(d.kind, Kind::Struct | Kind::Union | Kind::Enum) {
                let variants: Vec<DefIdx> = match d.kind {
                    Kind::Enum => d.children.clone(),
                    _ => vec![item],
                };
                let mut dead_variants = Vec::new();
                for variant in variants {
                    if !self.live.contains(variant) {
                        dead_variants.push(self.dead_item(variant));
                        continue;
                    }
                    let fields = &defs[variant].children;
                    let positional = fields.first().is_some_and(|&it| is_positional(&defs[it].name));
                    let report_on = if positional { ReportOn::TupleField } else { ReportOn::NamedField };
                    let dead_fields =
                        fields.iter().filter(|&&it| self.should_warn_about_field(it)).map(|&it| self.dead_item(it)).collect();
                    self.warn_multiple(variant, "read", dead_fields, report_on);
                }
                self.warn_multiple(item, "constructed", dead_variants, ReportOn::NamedField);
            }
        }
        for &item in &defs.modules[module].foreign {
            if keep(item) {
                self.check_definition(item);
            }
        }
    }

    fn dead_item(&self, def: DefIdx) -> DeadItem {
        let name = self.k.defs[def].name.clone().unwrap_or_default();
        DeadItem { def, name, level: self.level(def) }
    }

    /// rustc's `def_lint_level_plus`: `dead_code_pub_in_binary` is set only at the crate's root,
    /// one level for every item.
    fn level(&self, def: DefIdx) -> LevelPlus {
        match self.lint {
            Lint::DeadCode => self.k.level(def),
            Lint::PubInBinary => LevelPlus { level: Level::Allow, expect: None },
        }
    }

    fn is_live_code(&self, def: DefIdx) -> bool {
        match &self.k.defs[def].name {
            None => true,
            Some(name) => self.live.contains(def) || name.starts_with('_'),
        }
    }

    fn check_definition(&mut self, def: DefIdx) {
        if self.is_live_code(def) {
            return;
        }
        let participle = match self.k.defs[def].kind {
            Kind::AssocConst
            | Kind::AssocTy
            | Kind::AssocFn { .. }
            | Kind::Fn
            | Kind::Static
            | Kind::Const
            | Kind::TyAlias
            | Kind::Enum
            | Kind::Union
            | Kind::ForeignTy
            | Kind::Trait => "used",
            Kind::Struct => "constructed",
            _ => return,
        };
        let item = self.dead_item(def);
        self.lint_at_single_level(&[&item], participle, None, ReportOn::NamedField);
    }

    /// Not a field read, of `PhantomData`, nor a positional one of a type with no size (or
    /// whose layout is not known).
    fn should_warn_about_field(&self, field: DefIdx) -> bool {
        let defs = self.k.defs;
        if self.live.contains(field) {
            return false;
        }
        let Id::Field(id) = defs[field].id else { return false };
        let db = self.k.cx.db;
        let field_hir = hir::Field::from(id);
        let ty = field_hir.ty(db);
        if let Some(hir::Adt::Struct(it)) = ty.as_adt()
            && StructSignature::of(db, it.into()).flags.contains(StructFlags::IS_PHANTOM_DATA)
        {
            return false;
        }
        if is_positional(&defs[field].name) && field_hir.layout(db).map_or(true, |it| it.size() == 0) {
            return false;
        }
        true
    }

    fn warn_multiple(&mut self, parent: DefIdx, participle: &str, dead_codes: Vec<DeadItem>, report_on: ReportOn) {
        let mut dead_codes: Vec<&DeadItem> = dead_codes.iter().filter(|it| !it.name.starts_with('_')).collect();
        if dead_codes.is_empty() {
            return;
        }
        dead_codes.sort_by_key(|it| it.level.level);
        for group in dead_codes.chunk_by(|a, b| a.level == b.level) {
            self.lint_at_single_level(group, participle, Some(parent), report_on);
        }
    }

    fn lint_at_single_level(
        &mut self,
        dead_codes: &[&DeadItem],
        participle: &str,
        parent: Option<DefIdx>,
        report_on: ReportOn,
    ) {
        let k = self.k;
        let cx = k.cx;
        let defs = k.defs;
        let Some(first) = dead_codes.first() else { return };
        let Some(at) = defs[first.def].node else { return };
        let spans: Vec<Span> = dead_codes.iter().filter_map(|it| self.ident_or_def_span(it.def)).collect();
        if spans.len() != dead_codes.len() {
            return;
        }
        let mut descr = defs[first.def].kind.descr();
        if dead_codes.iter().any(|it| defs[it.def].kind.descr() != descr) {
            descr = "associated item";
        }
        let num = dead_codes.len();
        let names: Vec<&str> = dead_codes.iter().map(|it| it.name.as_str()).collect();
        let message = match (num > 6, num) {
            (true, _) => format!("multiple {descr}s are never {participle}"),
            (false, 1) => format!("{descr} {} is never {participle}", symbol_list(&names)),
            (false, _) => format!("{descr}s {} are never {participle}", symbol_list(&names)),
        };
        let mut diag = Diag::lint(self.lint.name(), message, spans[0].clone());
        for span in &spans[1..] {
            diag.primary(span.clone());
        }
        if self.lint == Lint::PubInBinary {
            diag.note(
                "in libraries, `pub` items can be used by dependent crates; in binaries, they cannot, so this `pub` item is unused",
            );
        }
        match report_on {
            ReportOn::TupleField => {
                let tuple_fields: &[DefIdx] = match parent {
                    Some(parent)
                        if defs[parent].kind == Kind::Struct
                            && defs[parent].children.first().is_some_and(|&it| is_positional(&defs[it].name)) =>
                    {
                        &defs[parent].children
                    }
                    _ => &[],
                };
                let trailing = match tuple_fields.len() >= num {
                    true => &tuple_fields[tuple_fields.len() - num..],
                    false => &[],
                };
                let fields = if num == 1 { "field" } else { "fields" };
                if dead_codes.iter().all(|it| trailing.contains(&it.def)) {
                    let this = if num == 1 { "this" } else { "these" };
                    diag.help(format!("consider removing {this} {fields}"));
                } else {
                    diag.multipart_suggestion(
                        format!(
                            "consider changing the {fields} to be of unit type to suppress this warning while preserving the field numbering, or remove the {fields}"
                        ),
                        spans.iter().map(|it| (it.clone(), "()".to_owned())).collect(),
                        Applicability::HasPlaceholders,
                    );
                }
            }
            ReportOn::NamedField => {
                for item in dead_codes {
                    if let Some(variant) = self.shadowing_variant(item)
                        && let Some(span) = self.def_span(variant)
                    {
                        let dead_descr = defs[item.def].kind.descr();
                        diag.span_note(
                            span,
                            format!(
                                "it is impossible to refer to the {dead_descr} `{}` because it is shadowed by this enum variant with the same name",
                                item.name
                            ),
                        );
                    }
                }
            }
        }
        if let Some(parent) = parent {
            let parent_descr = defs[parent].kind.descr();
            let span = match defs[parent].kind {
                Kind::Impl { .. } => self.def_span(parent),
                _ => self.ident_span(parent),
            };
            if let Some(span) = span {
                let label = if num == 1 { descr.to_owned() } else { format!("{descr}s") };
                diag.span_label(span, format!("{label} in this {parent_descr}"));
            }
        }
        let mut encl = parent.unwrap_or(first.def);
        if defs[encl].kind == Kind::Variant
            && let Parent::Def(adt) = defs[encl].parent
        {
            encl = adt;
        }
        if let Some(traits) = self.live.ignored.get(&encl)
            && let Some(name) = &defs[encl].name
        {
            let list: Vec<&str> = traits.to_vec();
            let note = match list.len() {
                1 => format!(
                    "`{name}` has a derived impl for the trait {}, but this is intentionally ignored during dead code analysis",
                    symbol_list(&list)
                ),
                _ => format!(
                    "`{name}` has derived impls for the traits {}, but these are intentionally ignored during dead code analysis",
                    symbol_list(&list)
                ),
            };
            diag.note(note);
        }
        crate::lints::code_fact(cx, &mut diag, at);
        self.found.push(Found::lint(diag, Phase::Late, at));
    }

    /// The variant of the enum an inherent impl is of with the name of `item`, an associated
    /// function or constant of the impl.
    fn shadowing_variant(&self, item: &DeadItem) -> Option<DefIdx> {
        let defs = self.k.defs;
        if !matches!(defs[item.def].kind, Kind::AssocFn { .. } | Kind::AssocConst) {
            return None;
        }
        let Parent::Def(impl_) = defs[item.def].parent else { return None };
        if defs[impl_].kind != (Kind::Impl { of_trait: false }) {
            return None;
        }
        let Id::Impl(id) = defs[impl_].id else { return None };
        let Some(hir::Adt::Enum(enum_)) = hir::Impl::from(id).self_ty(self.k.cx.db).as_adt() else { return None };
        let enum_ = defs.get(Id::Adt(AdtId::from(hir::Adt::Enum(enum_))))?;
        defs[enum_].children.iter().copied().find(|&it| defs[it].name.as_deref() == Some(item.name.as_str()))
    }

    /// rustc's `def_ident_span`, or its `def_span` where a definition has no name.
    fn ident_or_def_span(&self, def: DefIdx) -> Option<Span> {
        self.ident_span(def).or_else(|| self.def_span(def))
    }

    fn ident_span(&self, def: DefIdx) -> Option<Span> {
        let cx = self.k.cx;
        let node = cx.syntax(self.k.defs[def].node?);
        let name = ast::AnyHasName::cast(node.value.clone())?.name()?;
        Some(cx.span(node.with_value(name.syntax().text_range())))
    }

    /// rustc's `def_span`: of a variant or a field, all of it; of an item, its header.
    fn def_span(&self, def: DefIdx) -> Option<Span> {
        let cx = self.k.cx;
        let ptr = self.k.defs[def].node?;
        let node = cx.syntax(ptr);
        let range: TextRange = match self.k.defs[def].kind {
            Kind::Variant | Kind::Field => crate::cx::item_range(&node.value),
            _ => crate::cx::def_range(&node.value),
        };
        Some(cx.span(InFile::new(ptr.file_id, range)))
    }
}

/// A positional field's name: `0`, `1`.
fn is_positional(name: &Option<String>) -> bool {
    name.as_deref().is_some_and(|it| it.starts_with(|c: char| c.is_ascii_digit()))
}

/// rustc's `DiagSymbolList`: "`a`", "`a` and `b`", "`a`, `b`, and `c`".
fn symbol_list(names: &[&str]) -> String {
    match names {
        [] => String::new(),
        [one] => format!("`{one}`"),
        [a, b] => format!("`{a}` and `{b}`"),
        [init @ .., last] => {
            let init: Vec<String> = init.iter().map(|it| format!("`{it}`")).collect();
            format!("{}, and `{last}`", init.join(", "))
        }
    }
}