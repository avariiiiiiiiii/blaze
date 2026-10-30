# Requests of mem-db-infer

hir-ty (all but `src/next_solver/`) onto `crates/blaze-mem`. Design and the rules every change
follows (R1 to R8): `docs/v2/plan/lanes/mem/db-infer.md`. The paths belong to lane db (db-ty and
its children; frontend-db's children change `db` parameters) and, for `method_resolution.rs`,
`method_resolution/`, `traits.rs`, `specialization.rs`, `opaques.rs`, `layout.rs`, `layout/`,
`drop.rs`, lane items; none is done, so these are requests. The file-by-file changes are in the
part files:

| part | files of `crates/hir-ty/src/` | request file |
|---|---|---|
| this file | `Cargo.toml`, `lib.rs`, `db.rs`, `tests.rs`, `tests/budget.rs` | `v2/requests/mem-db-infer.md` |
| ctx | `infer.rs`, `infer/{unify,columns,persist,fallback,diagnostics,ambiguity,at_error,mutability,opaques,path,autoderef}.rs` | `v2/requests/mem-db-infer-ctx.md` |
| expr | `infer/{expr,pat,coerce,callee,cast,op,place_op}.rs` | `v2/requests/mem-db-infer-expr.md` |
| closure | `infer/closure.rs`, `infer/closure/` | `v2/requests/mem-db-infer-closure.md` |
| lower | `lower.rs`, `lower/`, `generics.rs`, `utils.rs`, `variance.rs`, `variance/`, `dyn_compatibility.rs`, `builtin_derive.rs`, `effects.rs` | `v2/requests/mem-db-infer-lower.md` |
| method | `method_resolution.rs`, `method_resolution/`, `autoderef.rs`, `traits.rs`, `specialization.rs`, `opaques.rs`, `drop.rs`, `inhabitedness.rs`, `representability.rs` | `v2/requests/mem-db-infer-method.md` |
| eval | `layout.rs`, `layout/`, `consteval.rs`, `mir.rs`, `mir/`, `intrinsic.rs`, `target_feature.rs`, `target_feature/`, `lang_items.rs`, `primitive.rs`, `solver_errors.rs`, `evict.rs` | `v2/requests/mem-db-infer-eval.md` |

Order to apply: 1 to 3 here first (they add the dependencies, the feature and the tags every
part uses), then ctx (it changes `InferenceContext`'s and `columns.rs`'s types the others name),
then the other parts in any order, then 4 (the tests) last.

## 1. lane db: hir-ty's dependencies (`crates/hir-ty/Cargo.toml`)

`[dependencies]`, after `blaze-reclaim.workspace = true`:

```toml
blaze-mem.workspace = true
hashbrown = { workspace = true, features = ["nightly"] }
```

`blaze-mem` is solver-sync-glue's request 10 (`v2/requests/solver-sync-glue.md`): one line, not
two. `hashbrown` is a root workspace dependency (`Cargo.toml:294`); its `nightly` feature is
mem-db's root request 3, which makes `HashMap<K, V, S, A>`'s `A` `core::alloc::Allocator`, so
`&Frame` is one. If root request 3 turns `nightly` on in the workspace entry, the line is
`hashbrown.workspace = true`.

Add a `[dev-dependencies]` section:

```toml
[dev-dependencies]
blaze-alloc-count.workspace = true
test-fixture.workspace = true
```

`blaze-alloc-count` is solver-sync-glue's request 6 (one line, not two). `test-fixture` depends
on base-db, hir-expand and span, not on hir-ty, so this is no cycle. Run the manifest lint after.

> forwarded to db (from requests, pass 16; told by tmux).

## 2. lane db: `crates/hir-ty/src/lib.rs`

Before (line 4):

```rust
#![cfg_attr(feature = "in-rust-tree", feature(rustc_private))]
```

After:

```rust
#![cfg_attr(feature = "in-rust-tree", feature(rustc_private))]
#![feature(allocator_api)]
```

`Vec<T, &Frame>`, `Box<T, &Frame>` and hashbrown's `A` need it (contract 2 of
`docs/v2/plan/lanes/mem/db.md`); blaze builds with its pinned nightly.

After the module list (after `pub mod traits;`, line 47):

```rust
#[cfg(test)]
mod tests;

#[cfg(test)]
#[global_allocator]
static ALLOC: blaze_alloc_count::Counting = blaze_alloc_count::Counting;
```

The allocator is solver-sync-glue's request 6: one static, not two (if it is in already, only
`mod tests;` is new).

New file `crates/hir-ty/src/tests.rs`:

```rust
//! Tests of hir-ty as a whole: what its queries allocate.

mod budget;
```

> forwarded to db (from requests, pass 16; told by tmux).

## 3. lane db: the tags, `crates/hir-ty/src/db.rs`

After the `use` block (after line 41, `};` closing `use crate::{..}`):

```rust
/// What hir-ty's inference, lowering, method resolution, layout, const eval and trait
/// solving allocate (docs/v2/plan/lanes/mem/db.md, contract 3).
pub(crate) const INFER_TAG: blaze_mem::Tag = blaze_mem::Tag::Infer;
/// What hir-ty's MIR queries allocate.
pub(crate) const MIR_TAG: blaze_mem::Tag = blaze_mem::Tag::Mir;
```

These are the lines that set every hir-ty query's tag: each `#[salsa::tracked(..)]` fn of
hir-ty (outside `next_solver/`, which mem-db-solver files) adds `mem_tag = crate::db::INFER_TAG`,
the MIR ones `mem_tag = crate::db::MIR_TAG`, once mem-db-salsa's option exists
(`v2/requests/mem-db-salsa.md`; asked by tmux to take a path to a `Tag` constant). The one
tracked fn of `db.rs` itself:

Before (`db.rs:551`):

```rust
#[salsa::tracked(returns(deref))]
fn closure_exprs(db: &dyn HirDatabase, owner: ExpressionStoreOwnerId, _: ()) -> Box<[ExprId]> {
```

After:

```rust
#[salsa::tracked(returns(deref), mem_tag = crate::db::INFER_TAG)]
fn closure_exprs(db: &dyn HirDatabase, owner: ExpressionStoreOwnerId, _: ()) -> Box<[ExprId]> {
```

(`db: &dyn HirDatabase` as it is when applied: frontend-db changes it to `&Db`.) The table of
every tracked fn with its line and tag is in "Tags of every query" below, from the parts.

The `HirDatabase` trait's MIR methods (`mir_body` and the rest, `db.rs:55` to `211`) are
implemented in hir-ty-mir (`hir_ty_mir::provide`, frontend-db's `hir_ty::providers`): their tag
is `MIR_TAG` there, which is mem-db-checks' or codegen's to file for hir-ty-mir, not here.

> forwarded to db (from requests, pass 16; told by tmux).

## 4. lane db: the budget tests, `crates/hir-ty/src/tests/budget.rs`

New file. It holds: the warm re-check of unchanged bodies to zero allocations; a cold `infer` of
each body to its owned result's allocations alone (counted as what cloning the result makes:
each owned heap block of the result is one allocation of its clone, interned types are
reference counts); the frame's peak to linear in the body's size.

```rust
//! What inference allocates (docs/v2/plan/lanes/mem/db-infer.md, "Budget tests"): nothing on a
//! re-check of bodies whose inputs did not change, its owned result alone on a run whose
//! dependencies are all memoized, and frame memory linear in the body.

use base_db::{Db, all_crates};
use blaze_alloc_count::{assert_at_most, measure};
use blaze_mem::{Tag, frame};
use hir_def::{AssocItemId, DefWithBodyId, LoweringMode, ModuleDefId, nameres::crate_def_map};
use salsa::{Database, Durability};
use test_fixture::ChangeFixture;

use crate::{InferenceResult, infer::infer_query_with_inspect};

const FIXTURE: &str = r#"
//- minicore: sized, copy, clone, add, mul, fn, option, deref
struct Point { x: u32, y: u32 }

impl Point {
    fn new(x: u32, y: u32) -> Point { Point { x, y } }
    fn sum(&self) -> u32 { self.x + self.y }
}

enum Shape { Dot(Point), Line(Point, Point) }

fn area(shape: &Shape) -> u32 {
    match shape {
        Shape::Dot(p) => p.sum(),
        Shape::Line(a, b) => {
            let d = a.sum() + b.sum();
            d * 2
        }
    }
}

fn first(points: &(Point, Point)) -> Option<&Point> {
    if points.0.x > points.1.x { Some(&points.0) } else { None }
}

fn main() {
    let p = Point::new(1, 2);
    let shapes = (Shape::Dot(p), Shape::Line(Point::new(3, 4), Point::new(5, 6)));
    let total = area(&shapes.0) + area(&shapes.1);
    let add = |a: u32| a + total;
    let _ = add(7);
    let pair = (Point::new(1, 1), Point::new(2, 2));
    let _ = first(&pair).map(|it| it.sum());
}
"#;

fn db_of(fixture: &str) -> Db {
    let mut db = Db::for_tests();
    ChangeFixture::parse(fixture).change.apply(&mut db);
    db
}

/// Every body of the fixture's own crates (not minicore's): functions, consts and statics in
/// modules and impls.
fn bodies(db: &Db) -> Vec<DefWithBodyId> {
    let mut bodies = Vec::new();
    for &krate in all_crates(db).iter() {
        if !krate.data(db).origin.is_local() {
            continue;
        }
        let def_map = crate_def_map(db, krate);
        for (_, module) in def_map.modules(db) {
            let scope = module.scope(db);
            for def in scope.declarations() {
                match def {
                    ModuleDefId::FunctionId(it) => bodies.push(it.into()),
                    ModuleDefId::ConstId(it) => bodies.push(it.into()),
                    ModuleDefId::StaticId(it) => bodies.push(it.into()),
                    _ => {}
                }
            }
            for it in scope.impls() {
                for &(_, item) in it.impl_items(db).items.iter() {
                    match item {
                        AssocItemId::FunctionId(it) => bodies.push(it.into()),
                        AssocItemId::ConstId(it) => bodies.push(it.into()),
                        AssocItemId::TypeAliasId(_) => {}
                    }
                }
            }
        }
    }
    assert!(!bodies.is_empty(), "the fixture has bodies");
    bodies
}

fn infer_all(db: &Db, bodies: &[DefWithBodyId]) {
    for &body in bodies {
        let _ = InferenceResult::of(db, body);
    }
}

#[test]
fn a_recheck_of_unchanged_bodies_allocates_nothing() {
    let mut db = db_of(FIXTURE);
    let bodies = bodies(&db);
    infer_all(&db, &bodies);
    // The first re-check after a write warms what verification keeps per worker; the second is
    // the steady state.
    db.synthetic_write(Durability::LOW);
    infer_all(&db, &bodies);
    db.synthetic_write(Durability::LOW);
    assert_at_most(0, "re-checking the inference of unchanged bodies", || {
        infer_all(&db, &bodies)
    });
}

#[test]
fn a_cold_infer_allocates_its_result_alone() {
    let db = db_of(FIXTURE);
    let bodies = bodies(&db);
    // Every query inference asks is memoized and every type it makes interned: what the run
    // below allocates is its own.
    infer_all(&db, &bodies);
    for &body in &bodies {
        frame::task(Tag::Infer, |_| {
            let (result, used) =
                measure(|| infer_query_with_inspect(&db, body, None, LoweringMode::Analysis));
            let (copy, owned) = measure(|| result.clone());
            assert!(copy == result, "a run gives the memoized result: {body:?}");
            assert!(
                used.allocations <= owned.allocations,
                "{body:?}: inference made {} allocations ({} bytes), its result owns {} ({} bytes)",
                used.allocations,
                used.bytes,
                owned.allocations,
                owned.bytes,
            );
        });
    }
}

/// `fn chain() -> u32` of `lets` statements, each adding one to the last.
fn chain(lets: usize) -> String {
    let mut text = String::from("//- minicore: sized, copy, add\nfn chain() -> u32 {\n    let x0: u32 = 1;\n");
    for at in 1..lets {
        text.push_str(&format!("    let x{at} = x{} + 1;\n", at - 1));
    }
    text.push_str(&format!("    x{}\n}}\n", lets - 1));
    text
}

/// The frame bytes a run of the inference of `chain(lets)` uses at most.
fn peak_of_chain(lets: usize) -> usize {
    let db = db_of(&chain(lets));
    let bodies = bodies(&db);
    assert_eq!(bodies.len(), 1);
    infer_all(&db, &bodies);
    frame::task(Tag::Infer, |frame| {
        let _ = infer_query_with_inspect(&db, bodies[0], None, LoweringMode::Analysis);
        frame.peak()
    })
}

#[test]
fn frame_use_is_linear_in_the_body() {
    let small = peak_of_chain(200);
    let large = peak_of_chain(800);
    assert!(small > 0, "inference's temporaries are frame values");
    // Four times the statements: at most four times the bytes and the fixed part again.
    assert!(large <= 4 * small + small / 2, "frame peak {small} bytes for 200 lets, {large} for 800");
}
```

Notes for whoever applies it:
- `infer_query_with_inspect` (`infer.rs:136`) is `pub`; the test calls it as it is when applied
  (frontend-db changes its `db` to `&Db`, which `&db` is either way). Under mem-db-infer-ctx's
  change it opens `frame::current` itself (rule R1), which inside the test's `frame::task` is the
  test's frame: that is what makes `frame.peak()` the run's.
- The first test needs mem-db-salsa's zero-allocation deep verify and mem-db-def's hit paths as
  well as this part's: it holds all of them for inference's dependencies.
- The second holds mem-db-solver's part too (inference variables and goals in the frame).
- `peak()` is the most the frame's cursor was since the task began (`frame.rs:88`).

> forwarded to db (from requests, pass 16; told by tmux).

## Tags of every query

Filled in from the parts (each part lists its tracked fns with line and tag).

> forwarded to db (from requests, pass 16; told by tmux).

## What the parts found

Filled in when the parts are done.

> forwarded to db (from requests, pass 16; told by tmux).

## dedup: byte cursor onto blaze_format::cursor

From `dedup-cursor-front` (report `/tmp/blaze-lanes/v2-dedup-cursor-front.md`; the cursor is
`crates/blaze-format/src/cursor.rs`, API in `docs/v2/plan/lanes/dedup/cursor.md`). `hir-ty`
already depends on `blaze-format`; no `Cargo.toml` change.

`crates/hir-ty/src/method_resolution/frozen.rs`:

1. Imports: add `use blaze_format::cursor::{Put, Reader};`.
2. `fn part` becomes
   ```rust
   /// Part `which` of frozen bytes: the first is after its length.
   fn part(bytes: &[u8], which: usize) -> R<&[u8]> {
       let mut at = Reader::new(bytes);
       let first = at.bytes_u32()?;
       Ok(if which == 0 { first } else { at.take_rest() })
   }
   ```
3. At the end of the writer (the fn that ends `let inherent = enc.finish()?;`):
   ```rust
       let mut out = len.to_le_bytes().to_vec();
       out.extend_from_slice(&traits);
   ```
   becomes
   ```rust
       let mut out = Vec::with_capacity(4 + traits.len() + inherent.len());
       out.put_u32(len);
       out.put_bytes(&traits);
   ```
   (the `let len = u32::try_from(..).map_err(|_| "impls over 4 GiB")?;` line above it stays).

Same bytes written, same accepted; the two error words ("no length", "a part past the end")
become the cursor's ("at byte 0: 4 bytes wanted, 2 left"), which no test matches.

> applied: `method_resolution/frozen.rs` is items' (DONE): `part` reads through `Reader::bytes_u32`/`take_rest`, the writer puts `len` and both parts through `Put` into one `Vec` sized once (same bytes) (requests, pass 18).
