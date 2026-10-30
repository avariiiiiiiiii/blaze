//! Term search

use std::collections::hash_map::Entry;

use base_db::Db;
use hir_def::type_ref::Mutability;
use hir_ide::db::HirDatabase;
use itertools::Itertools;
use rustc_hash::{FxHashMap, FxHashSet};

use crate::{ModuleDef, ScopeDef, Semantics, SemanticsScope, Type};

mod expr;
pub use expr::Expr;

mod tactics;

/// Key for lookup table to query new types reached.
#[derive(Debug, Hash, PartialEq, Eq)]
enum NewTypesKey {
    ImplMethod,
    StructProjection,
}

/// Helper enum to squash big number of alternative trees into `Many` variant as there is too many
/// to take into account.
#[derive(Debug)]
enum AlternativeExprs<'db> {
    /// There are few trees, so we keep track of them all
    Few(FxHashSet<Expr<'db>>),
    /// There are too many trees to keep track of
    Many,
}

impl<'db> AlternativeExprs<'db> {
    /// Construct alternative trees
    ///
    /// # Arguments
    /// `threshold` - threshold value for many trees (more than that is many)
    /// `exprs` - expressions iterator
    fn new(threshold: usize, exprs: impl Iterator<Item = Expr<'db>>) -> AlternativeExprs<'db> {
        let mut it = AlternativeExprs::Few(Default::default());
        it.extend_with_threshold(threshold, exprs);
        it
    }

    /// Get type trees stored in alternative trees (or `Expr::Many` in case of many)
    ///
    /// # Arguments
    /// `ty` - Type of expressions queried (this is used to give type to `Expr::Many`)
    fn exprs(&self, ty: &Type<'db>) -> Vec<Expr<'db>> {
        match self {
            // alloc: the candidates a tactic combines; the table is written to while they live.
            AlternativeExprs::Few(exprs) => exprs.iter().cloned().collect(),
            AlternativeExprs::Many => vec![Expr::Many(*ty)], // alloc: the one candidate
        }
    }

    /// How many expressions `exprs` would return.
    fn len(&self) -> usize {
        match self {
            AlternativeExprs::Few(exprs) => exprs.len(),
            AlternativeExprs::Many => 1,
        }
    }

    /// Extend alternative expressions
    ///
    /// # Arguments
    /// `threshold` - threshold value for many trees (more than that is many)
    /// `exprs` - expressions iterator
    fn extend_with_threshold(&mut self, threshold: usize, exprs: impl Iterator<Item = Expr<'db>>) {
        match self {
            AlternativeExprs::Few(tts) => {
                for it in exprs {
                    if tts.len() > threshold {
                        *self = AlternativeExprs::Many;
                        break;
                    }

                    tts.insert(it);
                }
            }
            AlternativeExprs::Many => (),
        }
    }

    fn is_many(&self) -> bool {
        matches!(self, AlternativeExprs::Many)
    }
}

/// # Lookup table for term search
///
/// Lookup table keeps all the state during term search.
/// This means it knows what types and how are reachable.
///
/// The secondary functionality for lookup table is to keep track of new types reached since last
/// iteration as well as keeping track of which `ScopeDef` items have been used.
/// Both of them are to speed up the term search by leaving out types / ScopeDefs that likely do
/// not produce any new results.
#[derive(Default, Debug)]
struct LookupTable<'db> {
    /// All the `Expr`s in "value" produce the type of "key"
    data: FxHashMap<Type<'db>, AlternativeExprs<'db>>,
    /// New types reached since last query by the `NewTypesKey`
    new_types: FxHashMap<NewTypesKey, Vec<Type<'db>>>,
    /// Types queried but not present
    types_wishlist: FxHashSet<Type<'db>>,
    /// Threshold to squash trees to `Many`
    many_threshold: usize,
}

impl<'db> LookupTable<'db> {
    /// Initialize lookup table
    fn new(many_threshold: usize, goal: Type<'db>) -> Self {
        let mut res = Self { many_threshold, ..Default::default() };
        res.new_types.insert(NewTypesKey::ImplMethod, Vec::new()); // alloc: empty
        res.new_types.insert(NewTypesKey::StructProjection, Vec::new()); // alloc: empty
        res.types_wishlist.insert(goal);
        res
    }

    /// Find all `Expr`s that unify with the `ty`
    fn find(&mut self, db: &'db Db, ty: &Type<'db>) -> Option<Vec<Expr<'db>>> {
        let Some((t, tts)) = self.data.iter().find(|(t, _)| t.could_unify_with_deeply(db, ty))
        else {
            self.types_wishlist.insert(*ty);
            return None;
        };

        // Collapse suggestions if there are many
        if tts.len() > self.many_threshold {
            return Some(vec![Expr::Many(*ty)]); // alloc: the one candidate
        }

        Some(tts.exprs(t))
    }

    /// Same as find but automatically creates shared reference of types in the lookup
    ///
    /// For example if we have type `i32` in data and we query for `&i32` it map all the type
    /// trees we have for `i32` with `Expr::Reference` and returns them.
    fn find_autoref(&mut self, db: &'db Db, ty: &Type<'db>) -> Option<Vec<Expr<'db>>> {
        if let Some((t, tts)) = self.data.iter().find(|(t, _)| t.could_unify_with_deeply(db, ty)) {
            // Collapse suggestions if there are many
            if tts.len() > self.many_threshold {
                return Some(vec![Expr::Many(*ty)]); // alloc: the one candidate
            }
            return Some(tts.exprs(t));
        }

        if let Some((t, tts)) = self.data.iter().find(|(t, _)| {
            t.add_reference(db, Mutability::Shared).could_unify_with_deeply(db, ty)
        }) {
            if tts.len() > self.many_threshold {
                return Some(vec![Expr::Many(*ty)]); // alloc: the one candidate
            }
            let exprs = tts.exprs(t).into_iter();
            // alloc: collected in place over `exprs`'s buffer, a box per candidate's `&`
            return Some(exprs.map(|expr| Expr::Reference(Box::new(expr))).collect());
        }

        self.types_wishlist.insert(*ty);
        None
    }

    /// Insert new type trees for type
    ///
    /// Note that the types have to be the same, unification is not enough as unification is not
    /// transitive. For example `Vec<i32>` and `FxHashSet<i32>` both unify with `Iterator<Item = i32>`,
    /// but they clearly do not unify themselves.
    fn insert(
        &mut self,
        db: &'db Db,
        ty: Type<'db>,
        exprs: impl Iterator<Item = Expr<'db>>,
    ) {
        let ty = ty.instantiate_with_errors(db);
        match self.data.entry(ty) {
            Entry::Occupied(mut entry) => {
                let it = entry.get_mut();
                it.extend_with_threshold(self.many_threshold, exprs);
                if it.is_many() {
                    self.types_wishlist.remove(entry.key());
                }
            }
            Entry::Vacant(entry) => {
                for it in self.new_types.values_mut() {
                    it.push(*entry.key());
                }
                entry.insert(AlternativeExprs::new(self.many_threshold, exprs));
            }
        }
    }

    /// Query new types reached since last query by key, into `out` (cleared first); the
    /// table keeps `out`'s old buffer for the next types it reaches.
    ///
    /// Create new key if you wish to query it to avoid conflicting with existing queries.
    fn new_types(&mut self, key: NewTypesKey, out: &mut Vec<Type<'db>>) {
        out.clear();
        if let Some(it) = self.new_types.get_mut(&key) {
            std::mem::swap(it, out);
        }
    }

    /// Fill `out` (cleared first) with the types queried but not found, in the set's order.
    fn wishlist_into(&self, out: &mut Vec<Type<'db>>) {
        out.clear();
        out.extend(self.types_wishlist.iter().copied());
    }
}

/// Context for the `term_search` function
#[derive(Debug)]
pub struct TermSearchCtx<'a, 'db> {
    /// Semantics for the program
    pub sema: &'a Semantics<'db>,
    /// Semantic scope, captures context for the term search
    pub scope: &'a SemanticsScope<'db>,
    /// Target / expected output type
    pub goal: Type<'db>,
    /// Configuration for term search
    pub config: TermSearchConfig,
}

/// Configuration options for the term search
#[derive(Debug, Clone, Copy)]
pub struct TermSearchConfig {
    /// Indicate when to squash multiple trees to `Many` as there are too many to keep track
    pub many_alternatives_threshold: usize,
    /// Fuel for term search in "units of work"
    pub fuel: u64,
}

impl Default for TermSearchConfig {
    fn default() -> Self {
        Self { many_alternatives_threshold: 1, fuel: 1200 }
    }
}

/// The units of work a term search has left: it stops iterating when they run out.
struct Fuel(std::cell::Cell<u64>);

impl Fuel {
    fn new(units: u64) -> Fuel {
        Fuel(std::cell::Cell::new(units))
    }

    /// Spends a unit; whether there was one to spend.
    fn should_continue(&self) -> bool {
        let remaining = self.0.get();
        self.0.set(remaining.saturating_sub(1));
        if remaining == 0 {
            tracing::debug!("fuel exhausted");
        }
        remaining > 0
    }
}

/// # Term search
///
/// Search for terms (expressions) that unify with the `goal` type.
///
/// # Arguments
/// * `ctx` - Context for term search
///
/// Internally this function uses Breadth First Search to find path to `goal` type.
/// The general idea is following:
/// 1. Populate lookup (frontier for BFS) from values (local variables, statics, constants, etc)
///    as well as from well knows values (such as `true/false` and `()`)
/// 2. Iteratively expand the frontier (or contents of the lookup) by trying different type
///    transformation tactics. For example functions take as from set of types (arguments) to some
///    type (return type). Other transformations include methods on type, type constructors and
///    projections to struct fields (field access).
/// 3. If we run out of fuel (term search takes too long) we stop iterating.
/// 4. Return all the paths (type trees) that take us to the `goal` type.
///
/// Note that there are usually more ways we can get to the `goal` type but some are discarded to
/// reduce the memory consumption. It is also unlikely anyone is willing ti browse through
/// thousands of possible responses so we currently take first 10 from every tactic.
pub fn term_search<'db>(ctx: &TermSearchCtx<'_, 'db>) -> Vec<Expr<'db>> {
    let module = ctx.scope.module();
    let mut defs = FxHashSet::default();
    defs.insert(ScopeDef::ModuleDef(ModuleDef::Module(module)));

    ctx.scope.process_all_names(&mut |_, def| {
        defs.insert(def);
    });

    let mut lookup = LookupTable::new(ctx.config.many_alternatives_threshold, ctx.goal);
    let fuel = &Fuel::new(ctx.config.fuel);
    // The types each tactic walks this iteration, reused across tactics and iterations.
    let mut types = Vec::new(); // alloc: empty

    // Try trivial tactic first, also populates lookup table
    // alloc: the result list, once a search
    let mut solutions: Vec<Expr<'db>> = tactics::trivial(ctx, &defs, &mut lookup).collect();
    // Use well known types tactic before iterations as it does not depend on other tactics
    solutions.extend(tactics::famous_types(ctx, &defs, &mut lookup));
    solutions.extend(tactics::assoc_const(ctx, &defs, &mut lookup));

    while fuel.should_continue() {
        solutions.extend(tactics::data_constructor(ctx, &defs, &mut lookup, &mut types, fuel));
        solutions.extend(tactics::free_function(ctx, &defs, &mut lookup, fuel));
        solutions.extend(tactics::impl_method(ctx, &defs, &mut lookup, &mut types, fuel));
        solutions.extend(tactics::struct_projection(ctx, &defs, &mut lookup, &mut types, fuel));
        solutions.extend(tactics::impl_static_method(ctx, &defs, &mut lookup, &mut types, fuel));
        solutions.extend(tactics::make_tuple(ctx, &defs, &mut lookup, &mut types, fuel));
    }

    // alloc: the returned suggestions, once a search
    solutions.into_iter().filter(|it| !it.is_many()).unique().collect()
}

#[cfg(test)]
mod fuel_tests {
    use super::Fuel;

    #[test]
    fn fuel_lets_as_many_steps_as_it_has_units() {
        let fuel = Fuel::new(2);
        assert!(fuel.should_continue());
        assert!(fuel.should_continue());
        assert!(!fuel.should_continue());
        assert!(!fuel.should_continue(), "spent fuel stays spent");
        assert!(!Fuel::new(0).should_continue());
    }
}
