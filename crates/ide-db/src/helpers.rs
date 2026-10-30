//! Random assortment of ide helpers for high-level ide features that don't fit in any other module.

use hir::{Crate, ItemInNs, Name, Semantics};
use span::Edition;
use syntax::{
    AstToken, SyntaxKind, SyntaxToken, ToSmolStr, TokenAtOffset,
    ast::{self, make, syntax_factory::SyntaxFactory},
};

use crate::{
    RootDatabase,
    defs::{Definition, IdentClass},
    generated,
};

pub fn item_name(db: &RootDatabase, item: ItemInNs) -> Option<Name> {
    match item {
        ItemInNs::Types(module_def_id) => module_def_id.name(db),
        ItemInNs::Values(module_def_id) => module_def_id.name(db),
        ItemInNs::Macros(macro_def_id) => Some(macro_def_id.name(db)),
    }
}

/// Picks the token with the highest rank returned by the passed in function.
pub fn pick_best_token(
    tokens: TokenAtOffset<SyntaxToken>,
    f: impl Fn(SyntaxKind) -> usize,
) -> Option<SyntaxToken> {
    tokens.max_by_key(move |t| f(t.kind()))
}
pub fn pick_token<T: AstToken>(mut tokens: TokenAtOffset<SyntaxToken>) -> Option<T> {
    tokens.find_map(T::cast)
}

/// Converts the mod path struct into its ast representation.
pub fn mod_path_to_ast(path: &hir::ModPath, edition: Edition) -> ast::Path {
    let _p = tracing::info_span!("mod_path_to_ast").entered();

    let mut segments = Vec::new();
    let mut is_abs = false;
    match path.kind {
        hir::PathKind::Plain => {}
        hir::PathKind::SELF => segments.push(make::path_segment_self()),
        hir::PathKind::Super(n) => segments.extend((0..n).map(|_| make::path_segment_super())),
        hir::PathKind::DollarCrate(_) | hir::PathKind::Crate => {
            segments.push(make::path_segment_crate())
        }
        hir::PathKind::Abs => is_abs = true,
    }

    segments.extend(path.segments().iter().map(|segment| {
        make::path_segment(make::name_ref(&segment.display_no_db(edition).to_smolstr()))
    }));
    make::path_from_segments(segments, is_abs)
}

pub fn mod_path_to_ast_with_factory(
    make: &SyntaxFactory,
    path: &hir::ModPath,
    edition: Edition,
) -> ast::Path {
    let _p = tracing::info_span!("mod_path_to_ast").entered();

    let mut segments = Vec::new();
    let mut is_abs = false;
    match path.kind {
        hir::PathKind::Plain => {}
        hir::PathKind::SELF => segments.push(make.path_segment_self()),
        hir::PathKind::Super(n) => segments.extend((0..n).map(|_| make.path_segment_super())),
        hir::PathKind::DollarCrate(_) | hir::PathKind::Crate => {
            segments.push(make.path_segment_crate())
        }
        hir::PathKind::Abs => is_abs = true,
    }

    segments.extend(path.segments().iter().map(|segment| {
        make.path_segment(make.name_ref(&segment.display_no_db(edition).to_smolstr()))
    }));
    make.path_from_segments(segments, is_abs)
}

/// Checks if the given lint is equal or is contained by the other lint which may or may not be a group.
pub fn lint_eq_or_in_group(lint: &str, lint_is: &str) -> bool {
    if lint == lint_is {
        return true;
    }

    if let Some(group) = generated::lints::DEFAULT_LINT_GROUPS
        .iter()
        .chain(generated::lints::CLIPPY_LINT_GROUPS.iter())
        .chain(generated::lints::RUSTDOC_LINT_GROUPS.iter())
        .find(|&check| check.lint.label == lint_is)
    {
        group.children.contains(&lint)
    } else {
        false
    }
}

pub fn is_editable_crate(krate: Crate, db: &RootDatabase) -> bool {
    let root_file = krate.root_file(db);
    let source_root_id = db.file_source_root(root_file).source_root_id(db);
    !db.source_root(source_root_id).source_root(db).is_library
}

// FIXME: This is a weird function
pub fn get_definition<'db>(
    sema: &Semantics<'db, RootDatabase>,
    token: SyntaxToken,
) -> Option<Definition<'db>> {
    for token in sema.descend_into_macros_exact(token) {
        let def = IdentClass::classify_token(sema, &token).map(IdentClass::definitions_no_ops);
        if let Some(&[x]) = def.as_deref() {
            return Some(x);
        }
    }
    None
}
