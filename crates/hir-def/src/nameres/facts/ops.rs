//! What collecting a module does, as data: one [`DeclOp`] per mutation the old `ModCollector`
//! made of its `DefCollector`, in the order it made them.
//!
//! A module's [`OpLog`] is the value of its declaration query (round 0) or of one of its macro
//! steps (later rounds). A [`super::unit::Unit`] replays the ops whose target is one of its
//! members, which leaves each member's scope as the old collector left it after the same calls;
//! the driver reads the crate-level ones. Nothing here borrows an item tree, so a log is a plain
//! query value: an import keeps its path and raw visibility, an attribute directive its tree.

use std::ops::Range;

use either::Either;
use hir_expand::{
    ExpandTo, InFile, MacroCallId,
    attrs::{Attr, AttrId},
    builtin::BuiltinDeriveExpander,
    mod_path::ModPath,
    name::Name,
};
use la_arena::Idx;
use smallvec::SmallVec;
use span::SyntaxContext;
use syntax::ast;

use crate::{
    AstId, AstIdWithPath, BuiltinDeriveImplId, ConstId, ExternBlockId, ExternCrateId, FunctionId,
    ImplId, ItemContainerId, MacroId, ModuleDefId, ModuleId, ProcMacroId, UseId,
    item_scope::ImportOrExternCrate,
    item_tree::{ImportAlias, ImportKind, ModItemId, TreeId},
    nameres::{ModuleOrigin, diagnostics::DefDiagnostic, mod_resolution::ModDir},
    per_ns::PerNs,
    visibility::{RawVisibility, Visibility},
};

/// Where an import comes from: its use tree, its `use` item, whether it is the
/// `#[prelude_import]`, and its kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct ImportSource {
    pub(crate) use_tree: Idx<ast::UseTree>,
    pub(crate) id: UseId,
    pub(crate) is_prelude: bool,
    pub(crate) kind: ImportKind,
}

/// One use tree of a `use` item of `module`: the collector's `ImportDirective`, owned.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ImportOp {
    pub(crate) module: ModuleId,
    pub(crate) path: ModPath,
    pub(crate) alias: Option<ImportAlias>,
    /// The `use` item's visibility, one for all of its use trees.
    pub(crate) visibility: RawVisibility,
    pub(crate) source: ImportSource,
}

impl ImportOp {
    /// The name a plain import binds, if any (`as _` binds none).
    pub(crate) fn bound_name(&self) -> Option<Name> {
        match &self.alias {
            Some(ImportAlias::Alias(name)) => Some(*name),
            Some(ImportAlias::Underscore) => None,
            None => self.path.segments().last().copied(),
        }
    }
}

/// A macro call left for the fixed point: the collector's `MacroDirective`, owned.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct MacroOp {
    pub(crate) module: ModuleId,
    pub(crate) depth: u32,
    pub(crate) kind: MacroOpKind,
    pub(crate) container: ItemContainerId,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum MacroOpKind {
    FnLike {
        ast_id: AstIdWithPath<ast::MacroCall>,
        expand_to: ExpandTo,
        ctxt: SyntaxContext,
    },
    Derive {
        ast_id: AstIdWithPath<ast::Adt>,
        derive_attr: AttrId,
        derive_pos: usize,
        ctxt: SyntaxContext,
        /// The `#[derive]` call this derive is one argument of.
        derive_macro_id: MacroCallId,
    },
    Attr {
        ast_id: AstIdWithPath<ast::Item>,
        attr_id: AttrId,
        attr: Attr,
        mod_item: ModItemId,
        /// The tree `mod_item` is in; its item tree is `tree.item_tree(db, krate)`.
        tree: TreeId,
    },
}

/// A builtin derive recorded instead of expanded, waiting for the item it is on to be
/// collected again: the collector's `DeferredBuiltinDerive`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DeferredDerive {
    pub(crate) call_id: MacroCallId,
    pub(crate) derive: BuiltinDeriveExpander,
    pub(crate) module_id: ModuleId,
    pub(crate) depth: u32,
    pub(crate) container: ItemContainerId,
    pub(crate) derive_attr_id: AttrId,
    pub(crate) derive_index: u32,
    /// Its helper attributes' place in the item's `derive_helpers_in_scope` list.
    pub(crate) helpers_range: Range<usize>,
}

/// A derive helper in scope for an item: its name, the derive macro, and the derive's call.
pub(crate) type DeriveHelper = (Name, MacroId, Either<MacroCallId, BuiltinDeriveImplId>);

/// One mutation. Ops naming a `module` change that module's state; the rest are crate-level
/// (the root's extern prelude, `macro_use` prelude, proc-macro tables) or side tables.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum DeclOp {
    // A module comes to exist.
    /// The module's own row: first op of its round-0 log, or, for one an expansion declares,
    /// right after its parent's `PushChild`.
    Init {
        module: ModuleId,
        origin: ModuleOrigin,
        visibility: Visibility,
        parent: Option<ModuleId>,
    },
    /// `parent` has the child `child` named `name` (`children_mut().insert`). Replacing one wakes
    /// every import, as a path can read whether a module is its parent's test harness.
    PushChild { parent: ModuleId, name: Name, child: ModuleId },
    /// The directory `mod` declarations of `module` resolve in (`mod_dirs.insert`).
    SetModDir { module: ModuleId, dir: ModDir },

    // The scope of `module`, one op per `ItemScope` method.
    Declare { module: ModuleId, def: ModuleDefId },
    /// `DefCollector::update`: push each resolution with `vis`, then propagate it to the
    /// module's glob importers.
    Update {
        module: ModuleId,
        resolutions: Box<[(Option<Name>, PerNs)]>,
        vis: Visibility,
        import: Option<ImportOrExternCrate>,
    },
    DefineImpl { module: ModuleId, imp: ImplId, is_trait_impl: bool, is_reservation: bool },
    DefineBuiltinDeriveImpl { module: ModuleId, imp: BuiltinDeriveImplId },
    DefineExternBlock { module: ModuleId, id: ExternBlockId },
    DefineExternCrateDecl { module: ModuleId, id: ExternCrateId },
    DefineUseDecl { module: ModuleId, id: UseId },
    DefineUnnamedConst { module: ModuleId, id: ConstId },
    /// Defines `mac` in the textual scope of `module` and wakes the imports that read `name` there.
    DefineLegacyMacro { module: ModuleId, name: Name, mac: MacroId },
    AddMacroInvoc { module: ModuleId, call: AstId<ast::MacroCall>, id: MacroCallId },
    AddAttrMacroInvoc { module: ModuleId, item: AstId<ast::Item>, id: MacroCallId },
    SetDeriveMacroInvoc {
        module: ModuleId,
        adt: AstId<ast::Adt>,
        call: Either<MacroCallId, BuiltinDeriveImplId>,
        attr: AttrId,
        idx: usize,
    },
    InitDeriveAttribute {
        module: ModuleId,
        adt: AstId<ast::Adt>,
        attr: AttrId,
        attr_call: MacroCallId,
        derive_calls: SmallVec<[Option<Either<MacroCallId, BuiltinDeriveImplId>>; 4]>,
    },

    // Directives.
    Import(ImportOp),
    Macro(MacroOp),

    // Crate-level, made by the root's collection only.
    ExternPrelude { name: Name, root: ModuleId, extern_crate: Option<ExternCrateId> },
    UnresolvedExternCrate { name: Name },
    MacroUsePrelude { name: Name, mac: MacroId, extern_crate: Option<ExternCrateId> },
    ExportedDerive { mac: MacroId, helpers: Box<[Name]> },
    ProcMacroFn { func: FunctionId, mac: ProcMacroId },

    // Side tables of the def map and of the macro steps.
    Diagnostic(DefDiagnostic),
    /// Extends the helpers in scope for `item`.
    DeriveHelpers { item: AstId<ast::Item>, helpers: Box<[DeriveHelper]> },
    /// The helpers at `range` of `item`'s list belong to `imp` now (a deferred builtin derive
    /// was collected).
    DeriveHelpersToImpl { item: AstId<ast::Item>, range: Range<usize>, imp: BuiltinDeriveImplId },
    /// Attributes of `item` up to and including `attr` are not collected again.
    SkipAttr { item: InFile<ModItemId>, attr: AttrId },
    /// `attr` of `item` was expanded; later attribute macros of `item` censor it.
    PrevActiveAttr { item: AstId<ast::Item>, attr: AttrId },
    DeferDerive { item: AstId<ast::Item>, derive: DeferredDerive },
    /// The builtin derives deferred for `item` are taken: collected, or dropped for an
    /// attribute macro after them.
    TakeDeferredDerives { item: AstId<ast::Item> },
    /// A macro with compilation-local state ran.
    SessionMacros,
}

impl DeclOp {
    /// The module whose state the op changes, `None` for a crate-level or side-table op.
    pub(crate) fn target(&self) -> Option<ModuleId> {
        match self {
            DeclOp::Init { module, .. }
            | DeclOp::SetModDir { module, .. }
            | DeclOp::Declare { module, .. }
            | DeclOp::Update { module, .. }
            | DeclOp::DefineImpl { module, .. }
            | DeclOp::DefineBuiltinDeriveImpl { module, .. }
            | DeclOp::DefineExternBlock { module, .. }
            | DeclOp::DefineExternCrateDecl { module, .. }
            | DeclOp::DefineUseDecl { module, .. }
            | DeclOp::DefineUnnamedConst { module, .. }
            | DeclOp::DefineLegacyMacro { module, .. }
            | DeclOp::AddMacroInvoc { module, .. }
            | DeclOp::AddAttrMacroInvoc { module, .. }
            | DeclOp::SetDeriveMacroInvoc { module, .. }
            | DeclOp::InitDeriveAttribute { module, .. } => Some(*module),
            DeclOp::PushChild { parent, .. } => Some(*parent),
            DeclOp::Import(it) => Some(it.module),
            DeclOp::Macro(it) => Some(it.module),
            DeclOp::ExternPrelude { .. }
            | DeclOp::UnresolvedExternCrate { .. }
            | DeclOp::MacroUsePrelude { .. }
            | DeclOp::ExportedDerive { .. }
            | DeclOp::ProcMacroFn { .. }
            | DeclOp::Diagnostic(_)
            | DeclOp::DeriveHelpers { .. }
            | DeclOp::DeriveHelpersToImpl { .. }
            | DeclOp::SkipAttr { .. }
            | DeclOp::PrevActiveAttr { .. }
            | DeclOp::DeferDerive { .. }
            | DeclOp::TakeDeferredDerives { .. }
            | DeclOp::SessionMacros => None,
        }
    }

    /// Whether a [`super::unit::Unit`] applies the op to its members' state: the scope, tree and
    /// import ops. Macro directives and the side tables belong to the macro steps and the
    /// driver.
    pub(crate) fn is_resolution_op(&self) -> bool {
        !matches!(
            self,
            DeclOp::Macro(_)
                | DeclOp::SetModDir { .. }
                | DeclOp::ExternPrelude { .. }
                | DeclOp::UnresolvedExternCrate { .. }
                | DeclOp::MacroUsePrelude { .. }
                | DeclOp::ExportedDerive { .. }
                | DeclOp::ProcMacroFn { .. }
                | DeclOp::Diagnostic(_)
                | DeclOp::DeriveHelpers { .. }
                | DeclOp::DeriveHelpersToImpl { .. }
                | DeclOp::SkipAttr { .. }
                | DeclOp::PrevActiveAttr { .. }
                | DeclOp::DeferDerive { .. }
                | DeclOp::TakeDeferredDerives { .. }
                | DeclOp::SessionMacros
        )
    }
}

/// The ops of one collection, in the order they were made.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct OpLog {
    pub(crate) ops: Vec<DeclOp>,
}

impl OpLog {
    pub(crate) fn push(&mut self, op: DeclOp) {
        self.ops.push(op);
    }

    pub(crate) fn iter(&self) -> std::slice::Iter<'_, DeclOp> {
        self.ops.iter()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.ops.is_empty()
    }

    pub(crate) fn shrink_to_fit(&mut self) {
        self.ops.shrink_to_fit();
    }

    /// The `#[macro_export]` ops and proc-macro definitions it makes in `root`'s scope, in order:
    /// what the root's own collection sees after collecting the module that made them.
    pub(crate) fn root_ops(&self, root: ModuleId) -> impl Iterator<Item = &DeclOp> + '_ {
        self.ops.iter().filter(move |op| {
            matches!(op, DeclOp::Declare { module, .. } | DeclOp::Update { module, .. } if *module == root)
        })
    }
}
