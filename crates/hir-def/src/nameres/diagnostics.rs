//! Diagnostics emitted during DefMap construction.

use std::ops::Not;

use cfg::CfgExpr;
use hir_expand::{ErasedAstId, ExpandErrorKind, MacroCallKind, attrs::AttrId, mod_path::ModPath};
use la_arena::Idx;
use syntax::ast;

use crate::{AstId, nameres::ModuleId};

#[derive(Debug, PartialEq, Eq)]
pub enum DefDiagnosticKind {
    UnresolvedModule { ast: AstId<ast::Module>, candidates: Box<[String]> },
    UnresolvedExternCrate { ast: AstId<ast::ExternCrate> },
    UnresolvedImport { id: AstId<ast::Use>, index: Idx<ast::UseTree> },
    /// Code `cfg` leaves out under its crate's cfg options (`Crate::cfg_options`), which are not
    /// copied into each diagnostic: a crate like `std` has thousands of these.
    UnconfiguredCode { ast_id: ErasedAstId, cfg: CfgExpr },
    UnresolvedMacroCall { ast: MacroCallKind, path: ModPath },
    UnimplementedBuiltinMacro { ast: AstId<ast::Macro> },
    InvalidDeriveTarget { ast: AstId<ast::Item>, id: AttrId },
    MalformedDerive { ast: AstId<ast::Adt>, id: AttrId },
    MacroDefError { ast: AstId<ast::Macro>, message: String },
    MacroError { ast: AstId<ast::Item>, path: ModPath, err: ExpandErrorKind },
    /// A `use` of a trait's associated items (`use Trait::item`, `use Trait::*`) that rustc's
    /// `finalize_imports` reports, or that blaze cannot list.
    TraitItemImport { id: AstId<ast::Use>, index: Idx<ast::UseTree>, why: TraitItemImport },
}

/// Why a [`DefDiagnosticKind::TraitItemImport`] is reported.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TraitItemImport {
    /// rustc's E0658, "`use` associated items of traits is unstable": the crate does not enable
    /// `import_trait_associated_functions`. The import still binds the items, as in rustc.
    Unstable,
    /// The trait is in the def map being collected and an item of it comes from a macro or
    /// may be an attribute macro's: listing them would expand macros that read this def map.
    /// blaze stops here instead of importing fewer items than rustc does; nothing is bound.
    ItemsFromMacros,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DefDiagnostics(Option<triomphe::ThinArc<(), DefDiagnostic>>);

impl DefDiagnostics {
    pub fn new(diagnostics: Vec<DefDiagnostic>) -> Self {
        Self(
            diagnostics
                .is_empty()
                .not()
                .then(|| triomphe::ThinArc::from_header_and_iter((), diagnostics.into_iter())),
        )
    }

    pub fn iter(&self) -> impl Iterator<Item = &DefDiagnostic> {
        self.0.as_ref().into_iter().flat_map(|it| &it.slice)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct DefDiagnostic {
    pub in_module: ModuleId,
    pub kind: DefDiagnosticKind,
}

impl DefDiagnostic {
    pub(super) fn unresolved_module(
        container: ModuleId,
        declaration: AstId<ast::Module>,
        candidates: Box<[String]>,
    ) -> Self {
        Self {
            in_module: container,
            kind: DefDiagnosticKind::UnresolvedModule { ast: declaration, candidates },
        }
    }

    pub(super) fn unresolved_extern_crate(
        container: ModuleId,
        declaration: AstId<ast::ExternCrate>,
    ) -> Self {
        Self {
            in_module: container,
            kind: DefDiagnosticKind::UnresolvedExternCrate { ast: declaration },
        }
    }

    pub(super) fn unresolved_import(
        container: ModuleId,
        id: AstId<ast::Use>,
        index: Idx<ast::UseTree>,
    ) -> Self {
        Self { in_module: container, kind: DefDiagnosticKind::UnresolvedImport { id, index } }
    }

    pub(crate) fn trait_item_import(
        container: ModuleId,
        id: AstId<ast::Use>,
        index: Idx<ast::UseTree>,
        why: TraitItemImport,
    ) -> Self {
        Self { in_module: container, kind: DefDiagnosticKind::TraitItemImport { id, index, why } }
    }

    pub fn macro_error(
        container: ModuleId,
        ast: AstId<ast::Item>,
        path: ModPath,
        err: ExpandErrorKind,
    ) -> Self {
        Self { in_module: container, kind: DefDiagnosticKind::MacroError { ast, path, err } }
    }

    pub fn unconfigured_code(container: ModuleId, ast_id: ErasedAstId, cfg: CfgExpr) -> Self {
        Self { in_module: container, kind: DefDiagnosticKind::UnconfiguredCode { ast_id, cfg } }
    }

    // FIXME: Whats the difference between this and unresolved_proc_macro
    pub(crate) fn unresolved_macro_call(
        container: ModuleId,
        ast: MacroCallKind,
        path: ModPath,
    ) -> Self {
        Self { in_module: container, kind: DefDiagnosticKind::UnresolvedMacroCall { ast, path } }
    }

    pub(super) fn unimplemented_builtin_macro(container: ModuleId, ast: AstId<ast::Macro>) -> Self {
        Self { in_module: container, kind: DefDiagnosticKind::UnimplementedBuiltinMacro { ast } }
    }

    pub(super) fn invalid_derive_target(
        container: ModuleId,
        ast: AstId<ast::Item>,
        id: AttrId,
    ) -> Self {
        Self { in_module: container, kind: DefDiagnosticKind::InvalidDeriveTarget { ast, id } }
    }

    pub(super) fn malformed_derive(container: ModuleId, ast: AstId<ast::Adt>, id: AttrId) -> Self {
        Self { in_module: container, kind: DefDiagnosticKind::MalformedDerive { ast, id } }
    }
}
