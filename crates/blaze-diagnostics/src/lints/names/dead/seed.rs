//! Where liveness starts: the entry function, what is public at `Reachable`, what an
//! `#[allow(dead_code)]` or `#[expect(dead_code)]` covers, what the linker or the compiler needs
//! (`#[used]`, `#[no_mangle]`, `#[export_name]`, a lang item), `const _`, the tests of a test
//! build; and the items of trait impls, live once their trait (or trait item) and self type are.
//!
//! Follows `compiler/rustc_passes/src/dead.rs` (`create_and_seed_worklist`,
//! `maybe_record_as_seed`, `has_allow_dead_code_or_lang_attr`) and
//! `compiler/rustc_middle/src/middle/codegen_fn_attrs.rs` (`contains_extern_indicator`)
//! (MIT or Apache-2.0).

use std::ops::ControlFlow;

use base_db::TargetKind;
use cfg::CfgAtom;
use hir::{HirFileId, InFile};
use hir_def::{ModuleDefId, ModuleId};
use hir_expand::MacroDefKind;
use intern::sym;
use smallvec::SmallVec;
use syntax::{
    AstNode, SmolStr, SyntaxNode,
    ast::{self, HasAttrs},
};

use super::{
    defs::{DefIdx, Defs, Id, Kind, Parent},
    level::{LevelPlus, Levels},
    reach::Reach,
};
use crate::cx::Cx;

/// rustc's `WorkItem`: a definition to mark live and walk, whether what reached it came from an
/// `allow` or `expect` of `dead_code` (`propagated`), and whether it has one itself (`own`), so
/// that it is walked but not made live, for an `expect` to be fulfilled.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) struct WorkItem {
    pub(super) id: DefIdx,
    pub(super) propagated: bool,
    pub(super) own: bool,
}

impl WorkItem {
    pub(super) fn new(id: DefIdx, comes_from_allow: bool) -> WorkItem {
        WorkItem { id, propagated: comes_from_allow, own: comes_from_allow }
    }
}

pub(super) struct Seeds {
    pub(super) worklist: Vec<WorkItem>,
    /// Walked after the rest: what is public at `Reachable`, and what an `allow` or `expect`
    /// covers.
    pub(super) deferred: Vec<WorkItem>,
    /// Trait impls and their items, live once their trait and self type are.
    pub(super) unsolved: Vec<DefIdx>,
}

/// What the pass reads of the crate, shared by its parts.
pub(super) struct Crate<'c, 'a, 'db> {
    pub(super) cx: &'c Cx<'a, 'db>,
    pub(super) defs: &'c Defs,
    pub(super) levels: &'c Levels,
    pub(super) reach: &'c Reach,
    /// rustc builds an executable of the crate: `None` where the front end does not know.
    pub(super) executable: Option<bool>,
}

impl Crate<'_, '_, '_> {
    /// The node whose attributes set the level at `def`: its own, or for what a builtin derive
    /// makes (a sibling of its ADT in rustc's HIR), what the ADT is in.
    fn level_node(&self, def: DefIdx) -> Option<InFile<SyntaxNode>> {
        let defs = self.defs;
        if let Some(node) = defs[def].node {
            return Some(self.cx.syntax(node));
        }
        let derive = match defs[def].id {
            Id::Derive(it) | Id::DeriveMethod(it) => it,
            _ => return None,
        };
        let adt = defs.get(Id::Adt(derive.loc(self.cx.db).adt))?;
        let node = self.cx.syntax(defs[adt].node?);
        let parent = node.value.parent()?;
        Some(node.with_value(parent))
    }

    /// The level of `dead_code` at `def` (rustc's `lint_level_spec_at_node`).
    pub(super) fn level(&self, def: DefIdx) -> LevelPlus {
        match self.level_node(def) {
            Some(node) => self.levels.at(self.cx, &node),
            None => self.levels.at(self.cx, &self.crate_root()),
        }
    }

    fn crate_root(&self) -> InFile<SyntaxNode> {
        let db = self.cx.db;
        let source = self.cx.krate.root_module(db).definition_source(db);
        let file: HirFileId = source.file_id;
        InFile::new(file, self.cx.sema.parse_or_expand(file))
    }

    /// rustc's `has_allow_dead_code_or_lang_attr`: `Some(true)` when `dead_code` is allowed or
    /// expected at `def`, `Some(false)` when the linker or the compiler needs it, else `None`.
    pub(super) fn allow_or_lang(&self, def: DefIdx) -> Option<bool> {
        if self.level(def).is_allow_or_expect() {
            return Some(true);
        }
        let d = &self.defs[def];
        let node = d.node.map(|it| self.cx.syntax(it))?;
        let names = attr_names(self.cx, &node.value);
        let codegen = matches!(d.kind, Kind::Fn | Kind::AssocFn { .. } | Kind::Static);
        let used_like = codegen
            && names.iter().any(|(name, value)| match name.as_str() {
                "no_mangle" | "export_name" | "used" | "rustc_std_internal_symbol" => true,
                "linkage" => value.as_deref() != Some("internal"),
                _ => false,
            });
        let lang = names.iter().any(|(name, _)| matches!(name.as_str(), "lang" | "rustc_canonical_symbol"));
        (used_like || lang).then_some(false)
    }

    /// The item of the local trait that `def`, an item of a trait impl, implements.
    pub(super) fn trait_item_of(&self, def: DefIdx) -> Option<DefIdx> {
        let defs = self.defs;
        let Parent::Def(impl_) = defs[def].parent else { return None };
        let trait_ = self.impl_trait(impl_)?;
        let name = defs[def].name.as_deref()?;
        let kind = defs[def].kind;
        let same_kind = |it: Kind| match (it, kind) {
            (Kind::AssocFn { .. }, Kind::AssocFn { .. }) => true,
            _ => it == kind,
        };
        defs[trait_]
            .children
            .iter()
            .copied()
            .find(|&it| same_kind(defs[it].kind) && defs[it].name.as_deref() == Some(name))
    }

    /// The local trait a trait impl is of.
    pub(super) fn impl_trait(&self, impl_: DefIdx) -> Option<DefIdx> {
        match self.defs[impl_].id {
            Id::Impl(id) => {
                let trait_ = hir::Impl::from(id).trait_(self.cx.db)?;
                self.defs.get(Id::Trait(trait_.into()))
            }
            _ => None,
        }
    }

    /// The local ADT an impl's self type is (rustc's `type_of(impl)` is `ty::Adt`), not behind
    /// a reference or in another type.
    pub(super) fn impl_self_adt(&self, impl_: DefIdx) -> Option<DefIdx> {
        let db = self.cx.db;
        match self.defs[impl_].id {
            Id::Impl(id) => {
                let adt = hir::Impl::from(id).self_ty(db).as_adt()?;
                self.defs.get(Id::Adt(adt.into()))
            }
            Id::Derive(id) => self.defs.get(Id::Adt(id.loc(db).adt)),
            _ => None,
        }
    }

    /// rustc's `entry_fn`: the `main` the crate root names, of an executable without
    /// `#![no_main]`. Where the front end does not know whether the crate is an executable,
    /// its `main` is taken to be the entry, which can only make fewer items dead.
    fn entry(&self) -> Option<DefIdx> {
        if self.executable == Some(false) || crate::checks::has_crate_attr(self.cx, "no_main") {
            return None;
        }
        let db = self.cx.db;
        let root: ModuleId = self.cx.krate.root_module(db).into();
        let scope = root.def_map(db).module(db, root).scope(db);
        let (_, per_ns) = scope.entries().find(|(name, _)| *name.symbol() == sym::main)?;
        match per_ns.values?.def {
            ModuleDefId::FunctionId(it) => self.defs.get(Id::Fn(it)),
            _ => None,
        }
    }

    /// `main` under `--test`, kept live as the would-be entry of a normal build.
    fn test_main(&self) -> Option<DefIdx> {
        if !self.is_test_build() {
            return None;
        }
        let db = self.cx.db;
        let root: ModuleId = self.cx.krate.root_module(db).into();
        let scope = root.def_map(db).module(db, root).scope(db);
        let (_, per_ns) = scope.entries().find(|(name, _)| *name.symbol() == sym::main)?;
        match per_ns.values?.def {
            ModuleDefId::FunctionId(it) => self.defs.get(Id::Fn(it)),
            _ => None,
        }
    }

    pub(super) fn is_test_build(&self) -> bool {
        self.cx.krate.cfg(self.cx.db).check_atom(&CfgAtom::Flag(sym::test))
    }

    /// The `#[test]` and `#[bench]` functions, which under `--test` the harness's `main` calls
    /// through the constants it makes of them.
    fn tests(&self) -> Vec<DefIdx> {
        let db = self.cx.db;
        if !self.is_test_build() {
            return Vec::new();
        }
        (0..self.defs.len())
            .filter(|&def| {
                let d = &self.defs[def];
                let Some(node) = d.node else { return false };
                let Some(call) = node.file_id.macro_file() else { return false };
                let test = match call.loc(db).def.kind {
                    MacroDefKind::BuiltInAttr(_, it) => it.is_test() || it.is_bench(),
                    _ => false,
                };
                let top = self.cx.syntax(node).value.parent().is_some_and(|it| it.parent().is_none());
                test && top && matches!(d.kind, Kind::Fn)
            })
            .collect()
    }
}

/// rustc's `crate_types().contains(&CrateType::Executable)`, from the target cargo builds, else
/// the crate's `#![crate_type]`s (none: `bin`).
pub(super) fn executable(cx: &Cx<'_, '_>) -> Option<bool> {
    match cx.krate.target_kind(cx.db) {
        TargetKind::Bin | TargetKind::Test | TargetKind::Bench | TargetKind::BuildScript => Some(true),
        TargetKind::Lib { .. } => Some(false),
        TargetKind::Example => None,
        TargetKind::Other => {
            let types: Vec<String> = crate::checks::crate_attrs(cx)
                .iter()
                .filter(|it| it.simple_name().is_some_and(|it| it == "crate_type"))
                .filter_map(|it| string_value(it.syntax()))
                .collect();
            Some(types.is_empty() || types.iter().any(|it| it == "bin"))
        }
    }
}

/// rustc's `create_and_seed_worklist`.
pub(super) fn seeds(k: &Crate<'_, '_, '_>) -> Seeds {
    let defs = k.defs;
    let mut seeds = Seeds { worklist: Vec::new(), deferred: Vec::new(), unsolved: Vec::new() };
    if let Some(main) = k.entry() {
        seeds.worklist.push(WorkItem::new(main, false));
    }
    if let Some(main) = k.test_main() {
        seeds.worklist.push(WorkItem::new(main, false));
    }
    for test in k.tests() {
        seeds.worklist.push(WorkItem::new(test, false));
    }
    for def in 0..defs.len() {
        if k.reach.is_reachable(def) {
            seeds.deferred.push(WorkItem::new(def, false));
        }
    }
    for &owner in &defs.owners {
        maybe_record_as_seed(k, owner, &mut seeds);
    }
    seeds
}

/// rustc's `maybe_record_as_seed` of an owner (an item, a trait's or an impl's item, a foreign
/// item).
fn maybe_record_as_seed(k: &Crate<'_, '_, '_>, owner: DefIdx, seeds: &mut Seeds) {
    let defs = k.defs;
    let push = |item: WorkItem, seeds: &mut Seeds| match item.own {
        true => seeds.deferred.push(item),
        false => seeds.worklist.push(item),
    };
    let allow = k.allow_or_lang(owner);
    if let Some(comes_from_allow) = allow {
        push(WorkItem::new(owner, comes_from_allow), seeds);
    }
    let d = &defs[owner];
    match d.kind {
        Kind::Enum => {
            if let Some(comes_from_allow) = allow {
                for &variant in &d.children {
                    push(WorkItem::new(variant, comes_from_allow), seeds);
                }
            }
        }
        Kind::AssocFn { .. } | Kind::AssocConst | Kind::AssocTy if allow.is_none() => {
            let Parent::Def(parent) = d.parent else { return };
            if let Kind::Impl { of_trait: true } = defs[parent].kind {
                if let Some(trait_item) = k.trait_item_of(owner)
                    && let Some(comes_from_allow) = k.allow_or_lang(trait_item)
                {
                    push(WorkItem::new(owner, comes_from_allow), seeds);
                }
                seeds.unsolved.push(owner);
            }
        }
        Kind::Impl { of_trait: true } if allow.is_none() => {
            if let Some(trait_) = k.impl_trait(owner)
                && let Some(comes_from_allow) = k.allow_or_lang(trait_)
            {
                push(WorkItem::new(owner, comes_from_allow), seeds);
            }
            seeds.unsolved.push(owner);
        }
        Kind::Const if d.name.as_deref() == Some("_") => push(WorkItem::new(owner, false), seeds),
        _ => {}
    }
}

/// The names of the attributes of `node` (its `cfg_attr`s expanded), each with its string value
/// (`#[export_name = ".."]`), `unsafe(..)` looked through.
fn attr_names(cx: &Cx<'_, '_>, node: &SyntaxNode) -> SmallVec<[(SmolStr, Option<String>); 2]> {
    let mut names = SmallVec::new();
    let Some(owner) = ast::AnyHasAttrs::cast(node.clone()) else { return names };
    if owner.attrs().next().is_none() {
        return names;
    }
    let cfg = cx.krate.cfg(cx.db);
    hir_expand::attrs::expand_cfg_attr::<()>(owner.attrs(), || cfg, |meta, _| {
        if let Some(name) = meta.path().and_then(|it| it.as_single_name_ref()) {
            names.push((SmolStr::new(name.text()), string_value(meta.syntax())));
        }
        ControlFlow::Continue(())
    });
    names
}

/// What is between the first and the last `"` of `node`: an attribute's string value.
fn string_value(node: &SyntaxNode) -> Option<String> {
    let text = node.text().to_string();
    let start = text.find('"')?;
    let end = text.rfind('"')?;
    (end > start).then(|| text[start + 1..end].to_owned())
}
