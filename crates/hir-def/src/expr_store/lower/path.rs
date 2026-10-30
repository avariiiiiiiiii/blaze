//! Transforms syntax into `Path` objects, ideally with accounting for hygiene

#[cfg(test)]
mod tests;

use std::iter;

use crate::{
    expr_store::{
        lower::{ExprCollector, generics::ImplTraitLowerFn},
        path::NormalPath,
    },
    type_ref::LifetimeRef,
};

use hir_expand::{
    mod_path::{ModPath, PathKind, resolve_crate_root},
    name::{AsName, Name},
};
use intern::{Interned, sym};
use syntax::{
    AstPtr,
    ast::{self, AstNode, HasGenericArgs},
};

use crate::expr_store::path::{GenericArg, GenericArgs, Path};

#[cfg(test)]
thread_local! {
    /// This is used to test `hir_segment_to_ast_segment()`. It's a hack, but it makes testing much easier.
    pub(super) static SEGMENT_LOWERING_MAP: std::cell::RefCell<rustc_hash::FxHashMap<ast::PathSegment, usize>> = std::cell::RefCell::default();
}

/// Converts an `ast::Path` to `Path`. Works with use trees.
/// It correctly handles `$crate` based path from macro call.
// If you modify the logic of the lowering, make sure to check if `hir_segment_to_ast_segment()`
// also needs an update.
pub(super) fn lower_path(
    collector: &mut ExprCollector<'_>,
    path: ast::Path,
    impl_trait_lower_fn: ImplTraitLowerFn<'_>,
) -> Option<Path> {
    let segments_base = collector.path_segments.len();
    let args_base = collector.path_generic_args.len();
    let lowered =
        lower_path_on_stacks(collector, path, impl_trait_lower_fn, segments_base, args_base);
    collector.path_segments.truncate(segments_base);
    collector.path_generic_args.truncate(args_base);
    lowered
}

/// `lower_path` with the segments (last first while walking up the qualifiers) and the
/// segments' generic args on the collector's stacks above `s0` and `g0`. A path lowered inside
/// this one (in a generic arg or a `<T as Trait>`) pushes and pops above them.
fn lower_path_on_stacks(
    collector: &mut ExprCollector<'_>,
    mut path: ast::Path,
    impl_trait_lower_fn: ImplTraitLowerFn<'_>,
    s0: usize,
    g0: usize,
) -> Option<Path> {
    let mut kind = PathKind::Plain;
    let mut type_anchor = None;
    #[cfg(test)]
    let mut ast_segments = Vec::new();
    #[cfg(test)]
    let mut ast_segments_offset = 0;
    #[allow(unused_mut)]
    let mut push_segment = |_segment: &ast::PathSegment, segments: &mut Vec<Name>, name| {
        #[cfg(test)]
        ast_segments.push(_segment.clone());
        segments.push(name);
    };

    let old_lifetimes_constrained_by_input = if collector.is_argument_lt_bound_scope() {
        Some(std::mem::take(&mut collector.named_lifetime_store.lifetimes_constrained_by_input))
    } else {
        None
    };

    loop {
        let Some(segment) = path.segment() else {
            collector.path_segments.push(Name::missing());
            // We can end up here if for `path::`
            match qualifier(&path) {
                Some(it) => {
                    path = it;
                    continue;
                }
                None => break,
            }
        };

        if segment.coloncolon_token().is_some() {
            debug_assert!(path.qualifier().is_none()); // this can only occur at the first segment
            kind = PathKind::Abs;
        }

        match segment.kind()? {
            ast::PathSegmentKind::Name(name_ref) => {
                if name_ref.text() == "$crate" {
                    if path.qualifier().is_some() {
                        // FIXME: Report an error.
                        return None;
                    }
                    break kind = resolve_crate_root(
                        collector.db,
                        collector.expander.ctx_for_range(name_ref.syntax().text_range()),
                    )
                    .map(PathKind::DollarCrate)
                    .unwrap_or(PathKind::Crate);
                }
                let name = name_ref.as_name();
                let args = segment
                    .generic_arg_list()
                    .and_then(|it| collector.lower_generic_args(it, impl_trait_lower_fn))
                    .or_else(|| {
                        collector.lower_generic_args_from_fn_path(
                            segment.parenthesized_arg_list(),
                            segment.ret_type(),
                            impl_trait_lower_fn,
                        )
                    })
                    .or_else(|| {
                        segment.return_type_syntax().map(|_| GenericArgs::return_type_notation())
                    });
                if args.is_some() {
                    let segments = collector.path_segments.len() - s0;
                    collector.path_generic_args.resize(g0 + segments, None);
                    collector.path_generic_args.push(args);
                }
                push_segment(&segment, &mut collector.path_segments, name);
            }
            ast::PathSegmentKind::SelfTypeKw => {
                push_segment(
                    &segment,
                    &mut collector.path_segments,
                    Name::new_symbol_root(sym::Self_),
                );
            }
            ast::PathSegmentKind::Type { type_ref, trait_ref } => {
                debug_assert!(path.qualifier().is_none()); // this can only occur at the first segment

                let type_ref = type_ref?;
                let self_type = collector.for_path_type_projection(|collector| {
                    collector.lower_type_ref(type_ref, impl_trait_lower_fn)
                });

                match trait_ref {
                    // <T>::foo
                    None => {
                        type_anchor = Some(self_type);
                        kind = PathKind::Plain;
                    }
                    // <T as Trait<A>>::Foo desugars to Trait<Self=T, A>::Foo
                    Some(trait_ref) => {
                        let path = collector.for_path_type_projection(|collector| {
                            collector.lower_path(trait_ref.path()?, impl_trait_lower_fn)
                        })?;
                        let ptr = AstPtr::new(&trait_ref).upcast();
                        let Some(mod_path) = path.mod_path() else {
                            collector.alloc_path(path, ptr);
                            return None;
                        };
                        let num_segments = mod_path.segments().len();
                        kind = mod_path.kind;

                        collector.path_segments.extend(mod_path.segments().iter().cloned().rev());
                        #[cfg(test)]
                        {
                            ast_segments_offset = mod_path.segments().len();
                        }
                        let segments = collector.path_segments.len() - s0;
                        if let Some(path_generic_args) = path.generic_args() {
                            collector.path_generic_args.resize(g0 + segments - num_segments, None);
                            collector
                                .path_generic_args
                                .extend(path_generic_args.iter().rev().cloned());
                        } else {
                            collector.path_generic_args.resize(g0 + segments, None);
                        }
                        collector.alloc_path(path, ptr);

                        let self_type = GenericArg::Type(self_type);

                        // Insert the type reference (T in the above example) as Self parameter for the trait
                        let last_segment =
                            collector.path_generic_args.get_mut(g0 + segments - num_segments)?;
                        *last_segment = Some(match last_segment.take() {
                            Some(it) => GenericArgs {
                                args: iter::once(self_type)
                                    .chain(it.args.iter().cloned())
                                    .collect(),
                                has_self_type: true,
                                ..it
                            },
                            None => GenericArgs {
                                args: Box::new([self_type]),
                                has_self_type: true,
                                ..GenericArgs::empty()
                            },
                        });
                    }
                }
            }
            ast::PathSegmentKind::CrateKw => {
                if path.qualifier().is_some() {
                    // FIXME: Report an error.
                    return None;
                }
                kind = PathKind::Crate;
                break;
            }
            ast::PathSegmentKind::SelfKw => {
                if path.qualifier().is_some() {
                    // FIXME: Report an error.
                    return None;
                }
                // don't break out if `self` is the last segment of a path, this mean we got a
                // use tree like `foo::{self}` which we want to resolve as `foo`
                if collector.path_segments.len() > s0 {
                    kind = PathKind::SELF;
                    break;
                }
            }
            ast::PathSegmentKind::SuperKw => {
                let nested_super_count = if let PathKind::Super(n) = kind { n } else { 0 };
                kind = PathKind::Super(nested_super_count + 1);
            }
        }
        path = match qualifier(&path) {
            Some(it) => it,
            None => break,
        };
    }
    collector.path_segments[s0..].reverse();
    let segment_count = collector.path_segments.len() - s0;
    if collector.path_generic_args.len() > g0 || type_anchor.is_some() {
        collector.path_generic_args.resize(g0 + segment_count, None);
        collector.path_generic_args[g0..].reverse();
    }

    if segment_count == 0 && kind == PathKind::Plain && type_anchor.is_none() {
        // plain empty paths don't exist, this means we got a single `self` segment as our path
        kind = PathKind::SELF;
    }

    // handle local_inner_macros :
    // Basically, even in rustc it is quite hacky:
    // https://github.com/rust-lang/rust/blob/614f273e9388ddd7804d5cbc80b8865068a3744e/src/librustc_resolve/macros.rs#L456
    // We follow what it did anyway :)
    if segment_count == 1
        && kind == PathKind::Plain
        && let Some(_macro_call) = path.syntax().parent().and_then(ast::MacroCall::cast)
    {
        let syn_ctxt = collector.expander.ctx_for_range(path.segment()?.syntax().text_range());
        if let Some(macro_call_id) = syn_ctxt.outer_expn(collector.db)
            && hir_expand::MacroCallId::from(macro_call_id).loc(collector.db).def.local_inner
        {
            kind = match resolve_crate_root(collector.db, syn_ctxt) {
                Some(crate_root) => PathKind::DollarCrate(crate_root),
                None => PathKind::Crate,
            }
        }
    }

    #[cfg(test)]
    {
        ast_segments.reverse();
        SEGMENT_LOWERING_MAP
            .with_borrow_mut(|map| map.extend(ast_segments.into_iter().zip(ast_segments_offset..)));
    }

    if let Some(last_segment_args @ Some(GenericArgs { has_self_type: true, .. })) =
        collector.path_generic_args[g0..].last_mut()
    {
        // Well-formed code cannot have `<T as Trait>` without an associated item after,
        // and this causes panics in hir-ty lowering.
        *last_segment_args = None;
    }

    let generic_args: Box<[Option<GenericArgs>]> =
        collector.path_generic_args.drain(g0..).collect();
    let mod_path =
        Interned::new(ModPath::from_segments(kind, collector.path_segments.drain(s0..)));

    if let Some(old_lifetimes_constrained_by_input) = old_lifetimes_constrained_by_input {
        let type_alias_constrained_lifetimes = collector.get_constrained_lifetimes_if_type_alias(
            &mod_path,
            generic_args.last().and_then(|g| g.as_ref()),
        );
        if let Some(lifetimes) = type_alias_constrained_lifetimes {
            collector.named_lifetime_store.lifetimes_constrained_by_input =
                old_lifetimes_constrained_by_input;
            collector.named_lifetime_store.lifetimes_constrained_by_input.extend(
                lifetimes.filter_map(|lt_ref| match &collector.store.lifetimes[lt_ref] {
                    LifetimeRef::Named(name) => Some(name.clone()),
                    _ => None,
                }),
            );
        } else {
            collector
                .named_lifetime_store
                .lifetimes_constrained_by_input
                .extend(old_lifetimes_constrained_by_input);
        }
    }

    if type_anchor.is_none() && generic_args.is_empty() {
        return Some(Path::BarePath(mod_path));
    } else {
        return Some(Path::Normal(Box::new(NormalPath { type_anchor, mod_path, generic_args })));
    }
}

fn qualifier(path: &ast::Path) -> Option<ast::Path> {
    if let Some(q) = path.qualifier() {
        return Some(q);
    }
    // FIXME: this bottom up traversal is not too precise.
    // Should we handle do a top-down analysis, recording results?
    let use_tree_list =
        path.syntax().ancestor_where(ast::UseTreeList::can_cast).and_then(ast::UseTreeList::cast)?;
    let use_tree = use_tree_list.parent_use_tree();
    use_tree.path()
}

/// This function finds the AST segment that corresponds to the HIR segment
/// with index `segment_idx` on the path that is lowered from `path`.
pub fn hir_segment_to_ast_segment(path: &ast::Path, segment_idx: u32) -> Option<ast::PathSegment> {
    // Too tightly coupled to `lower_path()`, but unfortunately we cannot decouple them,
    // as keeping source maps for all paths segments will have a severe impact on memory usage.

    let mut segments = path.segments();
    if let Some(ast::PathSegmentKind::Type { trait_ref: Some(trait_ref), .. }) =
        segments.clone().next().and_then(|it| it.kind())
    {
        segments.next();
        return find_segment(trait_ref.path()?.segments().chain(segments), segment_idx);
    }
    return find_segment(segments, segment_idx);

    fn find_segment(
        segments: impl Iterator<Item = ast::PathSegment>,
        segment_idx: u32,
    ) -> Option<ast::PathSegment> {
        segments
            .filter(|segment| match segment.kind() {
                Some(
                    ast::PathSegmentKind::CrateKw
                    | ast::PathSegmentKind::SelfKw
                    | ast::PathSegmentKind::SuperKw
                    | ast::PathSegmentKind::Type { .. },
                )
                | None => false,
                Some(ast::PathSegmentKind::Name(name)) => name.text() != "$crate",
                Some(ast::PathSegmentKind::SelfTypeKw) => true,
            })
            .nth(segment_idx as usize)
    }
}
