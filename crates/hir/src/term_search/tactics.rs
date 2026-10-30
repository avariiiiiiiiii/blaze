//! Tactics for term search
//!
//! All the tactics take following arguments
//! * `ctx` - Context for the term search
//! * `defs` - Set of items in scope at term search target location
//! * `lookup` - Lookup table for types
//! * `fuel` - The units of work left, which stop the iteration when they run out
//!
//! And they return iterator that yields type trees that unify with the `goal` type.

use std::iter;

use hir_ide::db::HirDatabase;
use itertools::Itertools;
use rustc_hash::FxHashSet;

use crate::{
    Adt, AssocItem, BuiltinType, GenericDef, GenericParam, HasAttrs, HasVisibility, Impl,
    ModuleDef, ScopeDef, Type, TypeParam, term_search::Expr,
};

use super::{Fuel, LookupTable, NewTypesKey, TermSearchCtx};

/// # Trivial tactic
///
/// Attempts to fulfill the goal by trying items in scope
/// Also works as a starting point to move all items in scope to lookup table.
///
/// # Arguments
/// * `ctx` - Context for the term search
/// * `defs` - Set of items in scope at term search target location
/// * `lookup` - Lookup table for types
///
/// Returns iterator that yields elements that unify with `goal`.
///
/// _Note that there is no use of calling this tactic in every iteration as the output does not
/// depend on the current state of `lookup`_
pub(super) fn trivial<'a, 'lt, 'db>(
    ctx: &'a TermSearchCtx<'_, 'db>,
    defs: &'a FxHashSet<ScopeDef<'db>>,
    lookup: &'lt mut LookupTable<'db>,
) -> impl Iterator<Item = Expr<'db>> + use<'a, 'db, 'lt> {
    let db = ctx.sema.db;
    defs.iter().filter_map(|def| {
        let expr = match def {
            ScopeDef::ModuleDef(ModuleDef::Const(it)) => Some(Expr::Const(*it)),
            ScopeDef::ModuleDef(ModuleDef::Static(it)) => Some(Expr::Static(*it)),
            ScopeDef::GenericParam(GenericParam::ConstParam(it)) => Some(Expr::ConstParam(*it)),
            ScopeDef::Local(it) => Some(Expr::Local(*it)),
            _ => None,
        }?;

        let ty = expr.ty(db);
        if ty.contains_unknown() {
            return None;
        }

        let unifies = ty.instantiate_with_errors(db).could_unify_with_deeply(db, &ctx.goal);
        // clone: the lookup keeps its own copy of the candidate, an ID here
        lookup.insert(db, ty, std::iter::once(expr.clone()));

        unifies.then_some(expr)
    })
}

/// # Associated constant tactic
///
/// Attempts to fulfill the goal by trying constants defined as associated items.
/// Only considers them on types that are in scope.
///
/// # Arguments
/// * `ctx` - Context for the term search
/// * `defs` - Set of items in scope at term search target location
/// * `lookup` - Lookup table for types
///
/// Returns iterator that yields elements that unify with `goal`.
///
/// _Note that there is no use of calling this tactic in every iteration as the output does not
/// depend on the current state of `lookup`_
pub(super) fn assoc_const<'a, 'lt, 'db>(
    ctx: &'a TermSearchCtx<'_, 'db>,
    defs: &'a FxHashSet<ScopeDef<'db>>,
    lookup: &'lt mut LookupTable<'db>,
) -> impl Iterator<Item = Expr<'db>> + use<'a, 'db, 'lt> {
    let db = ctx.sema.db;
    let module = ctx.scope.module();

    defs.iter()
        .filter_map(|def| match def {
            ScopeDef::ModuleDef(ModuleDef::Adt(it)) => Some(it),
            _ => None,
        })
        .flat_map(|it| Impl::all_for_type(db, it.ty(db)))
        .filter(|it| !it.is_unsafe(db))
        .flat_map(|it| it.items_iter(db))
        .filter(move |it| it.is_visible_from(db, module))
        .filter_map(AssocItem::as_const)
        .filter_map(|it| {
            if it.attrs(db).is_unstable() {
                return None;
            }

            let expr = Expr::Const(it);
            let ty = it.ty(db);

            if ty.contains_unknown() {
                return None;
            }

            let unifies = ty.instantiate_with_errors(db).could_unify_with_deeply(db, &ctx.goal);
            // clone: the lookup keeps its own copy of the candidate, an ID here
            lookup.insert(db, ty, std::iter::once(expr.clone()));

            unifies.then_some(expr)
        })
}

/// # Data constructor tactic
///
/// Attempts different data constructors for enums and structs in scope
///
/// Updates lookup by new types reached and returns iterator that yields
/// elements that unify with `goal`.
///
/// # Arguments
/// * `ctx` - Context for the term search
/// * `defs` - Set of items in scope at term search target location
/// * `lookup` - Lookup table for types
/// * `fuel` - The units of work left, which stop the iteration when they run out
pub(super) fn data_constructor<'a, 'lt, 'db>(
    ctx: &'a TermSearchCtx<'_, 'db>,
    _defs: &'a FxHashSet<ScopeDef<'db>>,
    lookup: &'lt mut LookupTable<'db>,
    types: &'lt mut Vec<Type<'db>>,
    fuel: &'a Fuel,
) -> impl Iterator<Item = Expr<'db>> + use<'a, 'db, 'lt> {
    let db = ctx.sema.db;
    let module = ctx.scope.module();
    lookup.wishlist_into(types);
    types.push(ctx.goal);
    types
        .drain(..)
        .filter_map(|ty| ty.as_adt().map(|adt| (adt, ty)))
        .filter(|_| fuel.should_continue())
        .filter_map(move |(adt, ty)| match adt {
            Adt::Struct(strukt) => {
                // Ignore unstable or not visible
                if strukt.is_unstable(db) || !strukt.is_visible_from(db, module) {
                    return None;
                }

                let generics = GenericDef::from(strukt);

                // We currently do not check lifetime bounds so ignore all types that have something to do
                // with them
                if !generics.lifetime_params(db).is_empty() {
                    return None;
                }

                if ty.contains_unknown() {
                    return None;
                }

                let fields = strukt.fields(db);
                // Check if all fields are visible, otherwise we cannot fill them
                if fields.iter().any(|it| !it.is_visible_from(db, module)) {
                    return None;
                }

                // Early exit if some param cannot be filled from lookup
                let param_exprs: Vec<Vec<Expr<'_>>> = fields
                    .into_iter()
                    .map(|field| {
                        lookup.find(db, &field.ty(db).instantiate(db, ty.type_arguments()))
                    })
                    .collect::<Option<_>>()?;

                // alloc: the generic args the candidates own
                let generics: Vec<_> = ty.type_arguments().collect();

                // Note that we need special case for 0 param constructors because of multi cartesian
                // product
                let exprs: Vec<Expr<'_>> = if param_exprs.is_empty() {
                    // alloc: the one candidate, with no params
                    vec![Expr::Struct { strukt, generics, params: Vec::new() }]
                } else {
                    param_exprs
                        .into_iter()
                        .multi_cartesian_product()
                        // clone: each candidate owns its generic args
                        .map(|params| Expr::Struct { strukt, generics: generics.clone(), params })
                        .collect() // alloc: the candidates, kept by the lookup and returned
                };

                lookup.insert(db, ty, exprs.iter().cloned());
                Some((ty, exprs))
            }
            Adt::Enum(enum_) => {
                // Ignore unstable or not visible
                if enum_.is_unstable(db) || !enum_.is_visible_from(db, module) {
                    return None;
                }

                let generics = GenericDef::from(enum_);
                // We currently do not check lifetime bounds so ignore all types that have something to do
                // with them
                if !generics.lifetime_params(db).is_empty() {
                    return None;
                }

                if ty.contains_unknown() {
                    return None;
                }

                // alloc: the generic args the candidates own, once per enum
                let generics: Vec<_> = ty.type_arguments().collect();
                // alloc: the candidates, kept by the lookup and returned
                let mut exprs: Vec<Expr<'_>> = Vec::new();
                for variant in enum_.variants_iter(db) {
                    // Early exit if some param cannot be filled from lookup
                    let Some(param_exprs) = variant
                        .fields_iter(db)
                        .map(|field| {
                            let ty = field.ty(db).instantiate(db, &generics);
                            lookup.find(db, &ty)
                        })
                        .collect::<Option<Vec<Vec<Expr<'_>>>>>()
                    else {
                        continue;
                    };

                    let start = exprs.len();
                    // Note that we need special case for 0 param constructors because of multi cartesian
                    // product
                    if param_exprs.is_empty() {
                        exprs.push(Expr::Variant {
                            variant,
                            // clone: each candidate owns its generic args
                            generics: generics.clone(),
                            params: Vec::new(), // alloc: empty
                        });
                    } else {
                        exprs.extend(param_exprs.into_iter().multi_cartesian_product().map(
                            // clone: each candidate owns its generic args
                            |params| Expr::Variant { variant, generics: generics.clone(), params },
                        ));
                    }
                    lookup.insert(db, ty, exprs[start..].iter().cloned());
                }

                Some((ty, exprs))
            }
            Adt::Union(_) => None,
        })
        .filter_map(|(ty, exprs)| {
            ty.instantiate_with_errors(db).could_unify_with_deeply(db, &ctx.goal).then_some(exprs)
        })
        .flatten()
}

/// # Free function tactic
///
/// Attempts to call different functions in scope with parameters from lookup table.
/// Functions that include generics are not used for performance reasons.
///
/// Updates lookup by new types reached and returns iterator that yields
/// elements that unify with `goal`.
///
/// # Arguments
/// * `ctx` - Context for the term search
/// * `defs` - Set of items in scope at term search target location
/// * `lookup` - Lookup table for types
/// * `fuel` - The units of work left, which stop the iteration when they run out
pub(super) fn free_function<'a, 'lt, 'db>(
    ctx: &'a TermSearchCtx<'_, 'db>,
    defs: &'a FxHashSet<ScopeDef<'db>>,
    lookup: &'lt mut LookupTable<'db>,
    fuel: &'a Fuel,
) -> impl Iterator<Item = Expr<'db>> + use<'a, 'db, 'lt> {
    let db = ctx.sema.db;
    let module = ctx.scope.module();
    defs.iter()
        .filter_map(move |def| match def {
            ScopeDef::ModuleDef(ModuleDef::Function(it)) => {
                let generics = GenericDef::from(*it);

                // Ignore const params for now
                let type_params = generics
                    .type_or_const_params(db)
                    .into_iter()
                    .map(|it| it.as_type_param(db))
                    .collect::<Option<Vec<TypeParam>>>()?;

                // Ignore lifetimes as we do not check them
                if !generics.lifetime_params(db).is_empty() {
                    return None;
                }

                // Only account for stable type parameters for now, unstable params can be default
                // tho, for example in `Box<T, #[unstable] A: Allocator>`
                if type_params.iter().any(|it| it.is_unstable(db) && it.default(db).is_none()) {
                    return None;
                }

                let non_default_type_params_len =
                    type_params.iter().filter(|it| it.default(db).is_none()).count();

                // Ignore bigger number of generics for now as they kill the performance
                if non_default_type_params_len > 0 {
                    return None;
                }

                // With no type param left to fill, the one choice of generics is the defaults.
                if !fuel.should_continue() {
                    return None;
                }
                // alloc: the generic args the candidates own
                let generics: Vec<_> =
                    type_params.iter().map(|it| it.default(db)).collect::<Option<_>>()?;

                let ret_ty = it.ret_type(db).instantiate(db, &generics);
                // Filter out private and unsafe functions
                if !it.is_visible_from(db, module)
                    || it.is_unsafe_to_call(
                        db,
                        None,
                        crate::Crate::from(ctx.scope.resolver().krate()).edition(db),
                    )
                    || it.is_unstable(db)
                    || ret_ty.is_raw_ptr()
                {
                    return None;
                }

                // Early exit if some param cannot be filled from lookup
                let param_exprs: Vec<Vec<Expr<'_>>> = it
                    .params_without_self(db)
                    .into_iter()
                    .map(|field| {
                        let ty = &field.ty().instantiate(db, &generics);
                        match ty.is_mutable_reference() {
                            true => None,
                            false => lookup.find_autoref(db, ty),
                        }
                    })
                    .collect::<Option<_>>()?;

                // Note that we need special case for 0 param constructors because of multi cartesian
                // product
                let fn_exprs: Vec<Expr<'_>> = if param_exprs.is_empty() {
                    // alloc: the one candidate, with no params
                    vec![Expr::Function { func: *it, generics, params: Vec::new() }]
                } else {
                    param_exprs
                        .into_iter()
                        .multi_cartesian_product()
                        .map(|params| Expr::Function {
                            func: *it,
                            // clone: each candidate owns its generic args
                            generics: generics.clone(),
                            params,
                        })
                        .collect() // alloc: the candidates, kept by the lookup and returned
                };

                lookup.insert(db, ret_ty, fn_exprs.iter().cloned());
                Some((ret_ty, fn_exprs))
            }
            _ => None,
        })
        .filter_map(|(ty, exprs)| {
            ty.instantiate_with_errors(db).could_unify_with_deeply(db, &ctx.goal).then_some(exprs)
        })
        .flatten()
}

/// # Impl method tactic
///
/// Attempts to call methods on types from lookup table.
/// This includes both functions from direct impl blocks as well as functions from traits.
/// Methods defined in impl blocks that are generic and methods that are themselves have
/// generics are ignored for performance reasons.
///
/// Updates lookup by new types reached and returns iterator that yields
/// elements that unify with `goal`.
///
/// # Arguments
/// * `ctx` - Context for the term search
/// * `defs` - Set of items in scope at term search target location
/// * `lookup` - Lookup table for types
/// * `fuel` - The units of work left, which stop the iteration when they run out
pub(super) fn impl_method<'a, 'lt, 'db>(
    ctx: &'a TermSearchCtx<'_, 'db>,
    _defs: &'a FxHashSet<ScopeDef<'db>>,
    lookup: &'lt mut LookupTable<'db>,
    types: &'lt mut Vec<Type<'db>>,
    fuel: &'a Fuel,
) -> impl Iterator<Item = Expr<'db>> + use<'a, 'db, 'lt> {
    let db = ctx.sema.db;
    let module = ctx.scope.module();
    lookup.new_types(NewTypesKey::ImplMethod, types);
    types
        .drain(..)
        .filter(|ty| !ty.type_arguments().any(|it| it.contains_unknown()))
        .filter(|_| fuel.should_continue())
        .flat_map(|ty| {
            Impl::all_for_type(db, ty).into_iter().map(move |imp| (ty, imp))
        })
        .flat_map(|(ty, imp)| imp.items_iter(db).map(move |item| (imp, ty, item)))
        .filter_map(|(imp, ty, it)| match it {
            AssocItem::Function(f) => Some((imp, ty, f)),
            _ => None,
        })
        .filter(|_| fuel.should_continue())
        .filter_map(move |(imp, ty, it)| {
            let fn_generics = GenericDef::from(it);
            let imp_generics = GenericDef::from(imp);

            // Ignore all functions that have something to do with lifetimes as we don't check them
            if !fn_generics.lifetime_params(db).is_empty()
                || !imp_generics.lifetime_params(db).is_empty()
            {
                return None;
            }

            // Ignore functions without self param
            if !it.has_self_param(db) {
                return None;
            }

            // Filter out private and unsafe functions
            if !it.is_visible_from(db, module)
                || it.is_unsafe_to_call(
                    db,
                    None,
                    crate::Crate::from(ctx.scope.resolver().krate()).edition(db),
                )
                || it.is_unstable(db)
            {
                return None;
            }

            // Ignore functions with generics for now as they kill the performance
            // Also checking bounds for generics is problematic
            if !fn_generics.type_or_const_params(db).is_empty() {
                return None;
            }

            let ret_ty = it.ret_type(db).instantiate(db, ty.type_arguments());

            // Ignore functions that do not change the type
            if ty.instantiate_with_errors(db).could_unify_with_deeply(db, &ret_ty) {
                return None;
            }

            let self_ty = it
                .self_param(db)
                .expect("No self param")
                .ty(db)
                .instantiate(db, ty.type_arguments());

            // Ignore functions that have different self type
            if !self_ty.autoderef(db).any(|s_ty| ty == s_ty) {
                return None;
            }

            // Early exit if some param cannot be filled from lookup
            let param_exprs: Vec<Vec<Expr<'_>>> = it
                .params_without_self(db)
                .into_iter()
                .map(|field| {
                    lookup.find_autoref(db, &field.ty().instantiate(db, ty.type_arguments()))
                })
                .collect::<Option<_>>()?;

            let target_type_exprs = lookup.find(db, &ty).expect("Type not in lookup");

            // alloc: the generic args the candidates own
            let generics: Vec<_> = ty.type_arguments().collect();
            let fn_exprs: Vec<Expr<'_>> = std::iter::once(target_type_exprs)
                .chain(param_exprs)
                .multi_cartesian_product()
                .map(|mut params| {
                    // alloc: the candidate's receiver, an `Expr` node
                    let target = Box::new(params.remove(0));
                    Expr::Method {
                        func: it,
                        // clone: each candidate owns its generic args
                        generics: generics.clone(),
                        target,
                        params,
                    }
                })
                .collect(); // alloc: the candidates, returned

            Some((ret_ty, fn_exprs))
        })
        .filter_map(|(ty, exprs)| {
            ty.instantiate_with_errors(db).could_unify_with_deeply(db, &ctx.goal).then_some(exprs)
        })
        .flatten()
}

/// # Struct projection tactic
///
/// Attempts different struct fields (`foo.bar.baz`)
///
/// Updates lookup by new types reached and returns iterator that yields
/// elements that unify with `goal`.
///
/// # Arguments
/// * `ctx` - Context for the term search
/// * `defs` - Set of items in scope at term search target location
/// * `lookup` - Lookup table for types
/// * `fuel` - The units of work left, which stop the iteration when they run out
pub(super) fn struct_projection<'a, 'lt, 'db>(
    ctx: &'a TermSearchCtx<'_, 'db>,
    _defs: &'a FxHashSet<ScopeDef<'db>>,
    lookup: &'lt mut LookupTable<'db>,
    types: &'lt mut Vec<Type<'db>>,
    fuel: &'a Fuel,
) -> impl Iterator<Item = Expr<'db>> + use<'a, 'db, 'lt> {
    let db = ctx.sema.db;
    let module = ctx.scope.module();
    lookup.new_types(NewTypesKey::StructProjection, types);
    types
        .drain(..)
        .map(|ty| {
            let targets = lookup.find(db, &ty).expect("Expr not in lookup");
            (ty, targets)
        })
        .filter(|_| fuel.should_continue())
        .flat_map(move |(ty, targets)| {
            ty.fields(db).into_iter().filter_map(move |(field, filed_ty)| {
                if !field.is_visible_from(db, module) {
                    return None;
                }
                let exprs = targets
                    // clone: each field access owns its target, a result value
                    .clone()
                    .into_iter()
                    // alloc: the access's target, an `Expr` node
                    .map(move |target| Expr::Field { field, expr: Box::new(target) });
                Some((filed_ty, exprs))
            })
        })
        .filter_map(|(ty, exprs)| {
            ty.instantiate_with_errors(db).could_unify_with_deeply(db, &ctx.goal).then_some(exprs)
        })
        .flatten()
}

/// # Famous types tactic
///
/// Attempts different values of well known types such as `true` or `false`.
///
/// Updates lookup by new types reached and returns iterator that yields
/// elements that unify with `goal`.
///
/// _Note that there is no point of calling it iteratively as the output is always the same_
///
/// # Arguments
/// * `ctx` - Context for the term search
/// * `defs` - Set of items in scope at term search target location
/// * `lookup` - Lookup table for types
pub(super) fn famous_types<'a, 'lt, 'db>(
    ctx: &'a TermSearchCtx<'_, 'db>,
    _defs: &'a FxHashSet<ScopeDef<'db>>,
    lookup: &'lt mut LookupTable<'db>,
) -> impl Iterator<Item = Expr<'db>> + use<'a, 'db, 'lt> {
    let db = ctx.sema.db;
    let bool_ty = BuiltinType::bool().ty(db);
    let unit_ty = Type::new_unit(db);
    [
        Expr::FamousType { ty: bool_ty, value: "true" },
        Expr::FamousType { ty: bool_ty, value: "false" },
        Expr::FamousType { ty: unit_ty, value: "()" },
    ]
    .into_iter()
    .inspect(|exprs| {
        // clone: once a search, the lookup's copy of a value with no heap parts
        lookup.insert(db, exprs.ty(db), std::iter::once(exprs.clone()));
    })
    .filter(|expr| expr.ty(db).instantiate_with_errors(db).could_unify_with_deeply(db, &ctx.goal))
}

/// # Impl static method (without self type) tactic
///
/// Attempts different functions from impl blocks that take no self parameter.
///
/// Updates lookup by new types reached and returns iterator that yields
/// elements that unify with `goal`.
///
/// # Arguments
/// * `ctx` - Context for the term search
/// * `defs` - Set of items in scope at term search target location
/// * `lookup` - Lookup table for types
/// * `fuel` - The units of work left, which stop the iteration when they run out
pub(super) fn impl_static_method<'a, 'lt, 'db>(
    ctx: &'a TermSearchCtx<'_, 'db>,
    _defs: &'a FxHashSet<ScopeDef<'db>>,
    lookup: &'lt mut LookupTable<'db>,
    types: &'lt mut Vec<Type<'db>>,
    fuel: &'a Fuel,
) -> impl Iterator<Item = Expr<'db>> + use<'a, 'db, 'lt> {
    let db = ctx.sema.db;
    let module = ctx.scope.module();
    lookup.wishlist_into(types);
    types.push(ctx.goal);
    types
        .drain(..)
        .filter(|ty| !ty.type_arguments().any(|it| it.contains_unknown()))
        .filter(|_| fuel.should_continue())
        .flat_map(|ty| {
            Impl::all_for_type(db, ty).into_iter().map(move |imp| (ty, imp))
        })
        .filter(|(_, imp)| !imp.is_unsafe(db))
        .flat_map(|(ty, imp)| imp.items_iter(db).map(move |item| (imp, ty, item)))
        .filter_map(|(imp, ty, it)| match it {
            AssocItem::Function(f) => Some((imp, ty, f)),
            _ => None,
        })
        .filter(|_| fuel.should_continue())
        .filter_map(move |(imp, ty, it)| {
            let fn_generics = GenericDef::from(it);
            let imp_generics = GenericDef::from(imp);

            // Ignore all functions that have something to do with lifetimes as we don't check them
            if !fn_generics.lifetime_params(db).is_empty()
                || !imp_generics.lifetime_params(db).is_empty()
            {
                return None;
            }

            // Ignore functions with self param
            if it.has_self_param(db) {
                return None;
            }

            // Filter out private and unsafe functions
            if !it.is_visible_from(db, module)
                || it.is_unsafe_to_call(
                    db,
                    None,
                    crate::Crate::from(ctx.scope.resolver().krate()).edition(db),
                )
                || it.is_unstable(db)
            {
                return None;
            }

            // Ignore functions with generics for now as they kill the performance
            // Also checking bounds for generics is problematic
            if !fn_generics.type_or_const_params(db).is_empty() {
                return None;
            }

            let ret_ty = it.ret_type(db).instantiate(db, ty.type_arguments());

            // Early exit if some param cannot be filled from lookup
            let param_exprs: Vec<Vec<Expr<'_>>> = it
                .params_without_self(db)
                .into_iter()
                .map(|field| {
                    lookup.find_autoref(db, &field.ty().instantiate(db, ty.type_arguments()))
                })
                .collect::<Option<_>>()?;

            // Note that we need special case for 0 param constructors because of multi cartesian
            // product
            // alloc: the generic args the candidates own
            let generics = ty.type_arguments().collect();
            let fn_exprs: Vec<Expr<'_>> = if param_exprs.is_empty() {
                // alloc: the one candidate, with no params
                vec![Expr::Function { func: it, generics, params: Vec::new() }]
            } else {
                param_exprs
                    .into_iter()
                    .multi_cartesian_product()
                    // clone: each candidate owns its generic args
                    .map(|params| Expr::Function { func: it, generics: generics.clone(), params })
                    .collect() // alloc: the candidates, kept by the lookup and returned
            };

            lookup.insert(db, ret_ty, fn_exprs.iter().cloned());

            Some((ret_ty, fn_exprs))
        })
        .filter_map(|(ty, exprs)| {
            ty.instantiate_with_errors(db).could_unify_with_deeply(db, &ctx.goal).then_some(exprs)
        })
        .flatten()
}

/// # Make tuple tactic
///
/// Attempts to create tuple types if any are listed in types wishlist
///
/// Updates lookup by new types reached and returns iterator that yields
/// elements that unify with `goal`.
///
/// # Arguments
/// * `ctx` - Context for the term search
/// * `defs` - Set of items in scope at term search target location
/// * `lookup` - Lookup table for types
/// * `fuel` - The units of work left, which stop the iteration when they run out
pub(super) fn make_tuple<'a, 'lt, 'db>(
    ctx: &'a TermSearchCtx<'_, 'db>,
    _defs: &'a FxHashSet<ScopeDef<'db>>,
    lookup: &'lt mut LookupTable<'db>,
    types: &'lt mut Vec<Type<'db>>,
    fuel: &'a Fuel,
) -> impl Iterator<Item = Expr<'db>> + use<'a, 'db, 'lt> {
    let db = ctx.sema.db;

    lookup.wishlist_into(types);
    types
        .drain(..)
        .filter(|_| fuel.should_continue())
        .filter(|ty| ty.is_tuple())
        .filter_map(move |ty| {
            // Double check to not contain unknown
            if ty.contains_unknown() {
                return None;
            }

            // Early exit if some param cannot be filled from lookup
            let param_exprs: Vec<Vec<Expr<'db>>> =
                ty.type_arguments().map(|field| lookup.find(db, &field)).collect::<Option<_>>()?;

            let exprs: Vec<Expr<'db>> = param_exprs
                .into_iter()
                .multi_cartesian_product()
                .filter(|_| fuel.should_continue())
                .map(|params| {
                    let tuple_ty = Type::new_tuple(db, params.iter().map(|it| it.ty(db)));

                    let expr = Expr::Tuple { ty: tuple_ty, params };
                    // clone: the lookup keeps its own copy of the candidate
                    lookup.insert(db, tuple_ty, iter::once(expr.clone()));
                    expr
                })
                .collect(); // alloc: the candidates, returned

            Some(exprs)
        })
        .flatten()
        .filter_map(|expr| {
            expr.ty(db)
                .instantiate_with_errors(db)
                .could_unify_with_deeply(db, &ctx.goal)
                .then_some(expr)
        })
}
