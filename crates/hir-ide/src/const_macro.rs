//! Const macros (docs/future/const-macros.md): a proc macro whose body is a function of a
//! macro crate of the analysis graph, expanded by calling that function in blaze's const
//! evaluator, in this process.
//!
//! Stage 0: the function takes the input tokens as text and returns its output as text
//! (`fn(&str) -> String` for a derive or a function-like macro, `fn(&str, &str) -> String`,
//! attribute first, for an attribute), and every output token gets the call site's span.

use std::any::Any;

use base_db::{Crate, Env, SourceDatabase, all_crates};
use hir_def::{FunctionId, ModuleDefId, MacroId, nameres::crate_def_map};
use hir_expand::{
    name::Name,
    parse_to_token_tree_static_span,
    proc_macro::{ProcMacroExpander, ProcMacroExpansionError, ProcMacroKind},
    tt,
};
use hir_ty::db::HirDatabase;
use intern::Symbol;
use span::{FileId, Span};

use crate::{display::DisplayTarget, mir::call_with_strs, mir_pretty::MirEvalErrorPretty};

/// The expander of the const macro `name`, a function of the macro crate whose root is `root`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConstMacroExpander {
    pub root: FileId,
    pub name: Symbol,
    pub kind: ProcMacroKind,
}

/// Lets `db` run const macros: their expander is given the database as a `SourceDatabase`,
/// and the evaluator needs it as a `HirDatabase`.
pub fn enable(db: &dyn HirDatabase) {
    HirDatabase::zalsa_register_downcaster(db);
}

/// `db` as the `HirDatabase` it is.
///
/// # Panics
///
/// When [`enable`] was not called on `db`.
fn hir_db(db: &dyn SourceDatabase) -> &dyn HirDatabase {
    let caster = salsa::plumbing::views(db).downcaster_for::<dyn HirDatabase>();
    // SAFETY: a database's views are made for that database's own type.
    unsafe { caster.downcast_unchecked(db.into()) }
}

impl ConstMacroExpander {
    /// The macro crate and the function that is the macro.
    fn function(&self, db: &dyn SourceDatabase) -> Result<(Crate, FunctionId), String> {
        let krate = all_crates(db)
            .iter()
            .copied()
            .find(|krate| {
                let data = krate.data(db);
                data.root_file_id == self.root && data.target_kind.is_proc_macro()
            })
            .ok_or_else(|| format!("no macro crate has the root of `{}`", self.name))?;
        let def_map = crate_def_map(db, krate);
        let root = def_map.module(db, def_map.root_module_id()).scope(db);
        let Some(MacroId::ProcMacroId(id)) =
            root.get(&Name::new_symbol_root(self.name.clone())).take_macros()
        else {
            return Err(format!("the macro crate defines no proc macro `{}`", self.name));
        };
        root.declarations()
            .find_map(|it| match it {
                ModuleDefId::FunctionId(func) if def_map.fn_as_proc_macro(func) == Some(id) => {
                    Some((krate, func))
                }
                _ => None,
            })
            .ok_or_else(|| format!("the proc macro `{}` is not a function", self.name))
    }
}

impl ProcMacroExpander for ConstMacroExpander {
    fn expand(
        &self,
        db: &dyn SourceDatabase,
        subtree: &tt::TopSubtree,
        attrs: Option<&tt::TopSubtree>,
        _env: &Env,
        _def_site: Span,
        call_site: Span,
        _mixed_site: Span,
        _current_dir: String,
    ) -> Result<tt::TopSubtree, ProcMacroExpansionError> {
        let (krate, func) = self.function(db).map_err(ProcMacroExpansionError::System)?;
        let hir = hir_db(db);
        let input = subtree.to_string();
        let attr = attrs.map(ToString::to_string);
        let args: Vec<&str> = attr.as_deref().into_iter().chain([input.as_str()]).collect();
        let output = call_with_strs(hir, func, &args).map_err(|error| {
            let mut text = format!("the const macro `{}` failed: ", self.name);
            let display_target = DisplayTarget::from_crate(hir, krate);
            let printed = error.pretty_print(
                &mut text,
                hir,
                |file, range| format!("{file:?} {range:?}"),
                display_target,
            );
            match printed {
                Ok(()) => ProcMacroExpansionError::Panic(text),
                Err(_) => ProcMacroExpansionError::Panic(format!("{text}{error:?}")),
            }
        })?;
        parse_to_token_tree_static_span(krate.data(db).edition, call_site, &output).ok_or_else(
            || {
                ProcMacroExpansionError::Panic(format!(
                    "the const macro `{}` returned text that is not tokens: {output:?}",
                    self.name
                ))
            },
        )
    }

    fn eq_dyn(&self, other: &dyn ProcMacroExpander) -> bool {
        (other as &dyn Any).downcast_ref::<Self>() == Some(self)
    }
}
