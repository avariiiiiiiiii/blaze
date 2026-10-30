//! rustc's lints: the ones the front end's queries give no diagnostic of, found as rustc's
//! passes find them, and the level of every lint found, decided as rustc decides it, with the
//! notes on where a level came from.
//!
//! - `levels`: lint levels from attributes, the command line and `--cap-lints`, and rustc's
//!   notes; the unknown, renamed and removed lint names; unfulfilled `expect`s.
//! - [`registry`]: every builtin lint of the pinned nightly, its groups, renamed and removed
//!   lints.
//! - [`edit_distance`]: rustc's choice of a similar name to suggest.
//! - `syntax`: what rustc's early lint pass, its expansion and its attribute checks find on
//!   the source as written (`unused_parens`, `unexpected_cfgs`, `unused_attributes`, ..).
//! - `names`: what name resolution, stability and the dead-code pass find of items and paths
//!   (`unused_imports`, `deprecated`, `dead_code`, ..).
//! - `bodies`: what type checking, match checking, unsafety checking, liveness and the MIR
//!   lints find in bodies (`unreachable_code`, `unused_variables`, `arithmetic_overflow`, ..).

mod bodies;
pub mod edit_distance;
mod levels;
mod names;
pub mod registry;
mod syntax;

pub(crate) use levels::level_at;
pub use levels::{CommandLine, Level};
pub use self::syntax::CheckCfg;

use blaze_items::{Fact, ItemId};
use hir::InFile;
use hir_def::{AdtId, ModuleDefId};
use ::syntax::{AstNode, SyntaxNode, SyntaxNodePtr, ast, ast::HasName};

use crate::{
    cx::Cx,
    lower::Found,
    model::Diag,
};

/// The lints rustc gives whether or not anything before them failed: its early lint pass on
/// the AST (`unused_parens`, ..), name resolution's (`unused_imports`, ..), the attribute and
/// stability checks, and what type checking, match checking, unsafety checking and liveness
/// lint in bodies (`unreachable_code`, `unused_variables`, ..), each with the [`Phase`] of the
/// pass that gives it.
///
/// [`Phase`]: crate::lower::Phase
pub fn early(cx: &Cx<'_, '_>, cmd: &CommandLine) -> Vec<Found> {
    let mut found = syntax::lints(cx, cmd);
    found.extend(names::before_abort(cx));
    found.extend(bodies::before_abort(cx));
    found
}

/// The lints rustc runs only when nothing before them failed (rustc's `misc_checking_3` and
/// late lint passes: `dead_code`, `non_upper_case_globals`, `unused_comparisons`, ..).
pub fn late(cx: &Cx<'_, '_>) -> Vec<Found> {
    let mut found = names::after_abort(cx);
    found.extend(bodies::after_abort(cx));
    found
}

/// The level of each lint found (attributes on the node and its ancestors, crate attributes,
/// `cfg_attr`, `#![warn(warnings)]`, `forbid`, `expect`, the command line, `--cap-lints`),
/// with rustc's notes (`#[warn(x)]` on by default, `#[deny(x)]` implied by
/// `#[deny(warnings)]`, `the lint level is defined here` with its span, the
/// future-incompatible note and issue link), the allowed ones dropped; the unknown and renamed
/// lints of the attributes and the command line; unfulfilled `expect`s unless `aborted`; then
/// the rest unchanged, in order. When `aborted`, what is of [`Phase::Late`] is dropped, as
/// rustc never gets to it.
///
/// [`Phase::Late`]: crate::lower::Phase::Late
pub fn decide(cx: &Cx<'_, '_>, cmd: &CommandLine, found: Vec<Found>, aborted: bool) -> Vec<Diag> {
    levels::decide(cx, cmd, found, aborted)
}

/// The item whose body `at` is in, and its name: a function, a const, a static, or the enum
/// of a discriminant; through a macro's expansion to where it was called.
pub(crate) fn body_item(cx: &Cx<'_, '_>, at: InFile<SyntaxNodePtr>) -> Option<(ItemId, String)> {
    let mut node: InFile<SyntaxNode> = cx.syntax(at);
    loop {
        for ancestor in node.value.ancestors() {
            let name = |it: &dyn HasName| it.name().map(|it| it.text().to_string());
            if let Some(it) = ast::Fn::cast(ancestor.clone()) {
                let def = cx.sema.to_def2(node.with_value(&it))?;
                let id = hir_def::FunctionId::try_from(def).ok()?;
                return Some((ItemId::Function(id), name(&it)?));
            }
            if let Some(it) = ast::Const::cast(ancestor.clone()) {
                let def = cx.sema.to_def2(node.with_value(&it))?;
                return Some((ItemId::Const(def.into()), name(&it).unwrap_or_else(|| "_".to_owned())));
            }
            if let Some(it) = ast::Static::cast(ancestor.clone()) {
                let def = cx.sema.to_def2(node.with_value(&it))?;
                return Some((ItemId::Static(def.into()), name(&it)?));
            }
            if let Some(it) = ast::Variant::cast(ancestor.clone()) {
                let def = cx.sema.to_def2(node.with_value(&it))?;
                let parent = def.parent_enum(cx.db);
                let name = parent.name(cx.db).display(cx.db, cx.edition).to_string();
                return Some((ItemId::Adt(AdtId::EnumId(parent.into())), name));
            }
        }
        let call = node.file_id.macro_file()?;
        node = call.loc(cx.db).to_node(cx.db);
    }
}

/// The item whose body `at` is in, and its path as rustc prints it (`def_path_str`).
pub(crate) fn body_path(cx: &Cx<'_, '_>, at: InFile<SyntaxNodePtr>) -> Option<(ItemId, String)> {
    let (item, _) = body_item(cx, at)?;
    let id = match item {
        ItemId::Function(id) => ModuleDefId::FunctionId(id),
        ItemId::Const(id) => ModuleDefId::ConstId(id),
        ItemId::Static(id) => ModuleDefId::StaticId(id),
        ItemId::Adt(id) => ModuleDefId::AdtId(id),
        _ => return None,
    };
    Some((item, cx.def_path(hir::ModuleDef::from(id))))
}

/// Records on `diag` the fact of the code of the body `at` is in: what to change to make it
/// go away.
pub(crate) fn code_fact(cx: &Cx<'_, '_>, diag: &mut Diag, at: InFile<SyntaxNodePtr>) {
    if let Some((item, name)) = body_item(cx, at) {
        diag.fact(Fact::Code(cx.key(item)), format!("the code of `{name}`"));
    }
}
