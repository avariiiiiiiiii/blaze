//! rustc's printing of types, paths, trait references and predicates in diagnostics:
//! trimmed paths (`trimmed_def_paths`), visible paths through re-exports, generic arguments
//! without their defaults, regions as rustc shows them and named in binders, closures by
//! their span, `impl Trait` and `dyn Trait` with `Fn` sugar, and rustc's short names of a
//! type's kind (`sort_string`, `prefix_string`). Ported from
//! `compiler/rustc_middle/src/ty/print/pretty.rs`, `compiler/rustc_middle/src/ty/print/mod.rs`,
//! `compiler/rustc_middle/src/ty/error.rs` and the type mismatch label of
//! `compiler/rustc_trait_selection/src/error_reporting/infer/mod.rs` (MIT or Apache-2.0).
//!
//! The printer writes into one `String` and keeps its binder names in a stack of small
//! vectors; the per-crate tables of `paths` are built once and read without locks.

mod consts;
pub(crate) mod paths;
mod printer;
mod types;

use hir::AsAssocItem as _;
use hir_def::ModuleDefId;
use hir_ty::next_solver::Ty;

pub(crate) use paths::Paths;

use crate::cx::Cx;
use paths::Def;
use printer::{Mode, Printer};

/// A type as rustc prints it in a message: `{ty}` of a `Ty`.
pub fn ty(cx: &Cx<'_, '_>, ty: &hir::Type<'_>) -> String {
    ty_in(cx, ty.to_next_solver(), Mode::NORMAL)
}

/// A type with no path trimmed: rustc's `with_no_trimmed_paths!`.
pub fn ty_untrimmed(cx: &Cx<'_, '_>, ty: &hir::Type<'_>) -> String {
    ty_in(cx, ty.to_next_solver(), Mode::UNTRIMMED)
}

fn ty_in(cx: &Cx<'_, '_>, ty: Ty<'_>, mode: Mode) -> String {
    let mut p = Printer::new(cx, mode, false);
    p.print_type(ty);
    p.finish()
}

/// rustc's `def_path_str`: the path of a definition with its own generic parameters left
/// out, value paths in value namespace.
pub fn def_path(cx: &Cx<'_, '_>, def: hir::ModuleDef) -> String {
    def_path_in(cx, def, Mode::NORMAL)
}

/// rustc's `def_path_str` under `with_no_trimmed_paths!`.
pub fn def_path_untrimmed(cx: &Cx<'_, '_>, def: hir::ModuleDef) -> String {
    def_path_in(cx, def, Mode::UNTRIMMED)
}

fn def_path_in(cx: &Cx<'_, '_>, def: hir::ModuleDef, mode: Mode) -> String {
    if let hir::ModuleDef::BuiltinType(it) = def {
        return it.name().as_str().to_owned();
    }
    match ModuleDefId::try_from(def).ok().and_then(Def::of) {
        Some(def) => {
            let mut p = Printer::new(cx, mode, def.is_value());
            p.print_def_path(def, &[]);
            p.finish()
        }
        None => builtin_derive_method(cx, def, mode),
    }
}

/// A method of a built-in derive's impl, which has no definition of its own: rustc's path of
/// an impl's item, `<S as Trait>::name`, the impl being where the type is.
fn builtin_derive_method(cx: &Cx<'_, '_>, def: hir::ModuleDef, mode: Mode) -> String {
    let db = cx.db;
    let hir::ModuleDef::Function(f) = def else {
        unreachable!("only a built-in derive's method has no `ModuleDefId`: {def:?}")
    };
    let Some(hir::AssocItemContainer::Impl(imp)) = f.as_assoc_item(db).map(|it| it.container(db))
    else {
        unreachable!("a built-in derive's method is in its impl")
    };
    let trait_ = imp.trait_(db).expect("a built-in derive's impl is of its trait");
    let mut p = Printer::new(cx, mode, false);
    p.write_str("<");
    p.print_type(imp.self_ty(db).to_next_solver());
    p.write_str(" as ");
    p.print_def_path(Def::Trait(trait_.into()), &[]);
    p.write_str(">::");
    p.write_name(&f.name(db));
    p.finish()
}

/// rustc's `TraitRef` printing: `<T as Trait<U>>`.
pub fn trait_ref(cx: &Cx<'_, '_>, tr: &hir::TraitRef<'_>) -> String {
    let tr = tr.to_next_solver();
    let mut p = Printer::new(cx, Mode::NORMAL, false);
    p.write_str("<");
    p.print_type(tr.self_ty());
    p.write_str(" as ");
    p.print_trait_path(tr);
    p.write_str(">");
    p.finish()
}

/// rustc's `print_only_trait_path`: `Trait<U>`, its own arguments without their defaults.
pub fn trait_path(cx: &Cx<'_, '_>, tr: &hir::TraitRef<'_>) -> String {
    let mut p = Printer::new(cx, Mode::NORMAL, false);
    p.print_trait_path(tr.to_next_solver());
    p.finish()
}

/// rustc's `print_modifiers_and_trait_path`: `!` for a negative bound, then the trait with
/// `Fn(u8)` sugar for a parenthesized trait (`TraitRefPrintSugared`).
pub fn trait_path_sugared(cx: &Cx<'_, '_>, pred: &hir::TraitPredicate<'_>) -> String {
    let pred = pred.to_next_solver();
    let mut p = Printer::new(cx, Mode::NORMAL, false);
    if pred.polarity == rustc_type_ir::PredicatePolarity::Negative {
        p.write_str("!");
    }
    p.print_trait_sugared(pred.trait_ref);
    p.finish()
}

/// rustc's `TraitPredicate` printing: `T: Trait<U>`, `T: !Trait`, `F: Fn(u8)`.
pub fn predicate(cx: &Cx<'_, '_>, pred: &hir::TraitPredicate<'_>) -> String {
    let pred = pred.to_next_solver();
    let mut p = Printer::new(cx, Mode::NORMAL, false);
    p.print_trait_predicate(pred);
    p.finish()
}

/// rustc's `Ty::sort_string`: what kind of type it is, or the type in backticks, shortened
/// to rustc's limit.
pub fn sort_string(cx: &Cx<'_, '_>, ty: &hir::Type<'_>) -> String {
    types::sort_string(cx, ty.to_next_solver(), Mode::NORMAL)
}

/// rustc's `Ty::prefix_string`: "struct", "reference", "fn item".
pub fn prefix_string(cx: &Cx<'_, '_>, ty: &hir::Type<'_>) -> String {
    types::prefix_string(cx, ty.to_next_solver())
}

/// The label rustc puts on a type mismatch (`TypeError::Sorts`): "expected `u32`, found
/// `&str`", from the two types with their regions erased and paths forced trimmed, or rustc's
/// `TypeError` text, "found a different" when even that reads the same.
pub fn sorts(cx: &Cx<'_, '_>, expected: &hir::Type<'_>, found: &hir::Type<'_>) -> String {
    let (expected, found) = (expected.to_next_solver(), found.to_next_solver());
    let short = Mode::ERASED.forced();
    let e = types::sort_string(cx, expected, short);
    let f = types::sort_string(cx, found, short);
    if e != f {
        return format!("expected {e}, found {f}");
    }
    let e = types::sort_string(cx, expected, Mode::NORMAL);
    let f = types::sort_string(cx, found, Mode::NORMAL);
    if e == f {
        format!("expected {e}, found a different {f}")
    } else {
        format!("expected {e}, found {f}")
    }
}
