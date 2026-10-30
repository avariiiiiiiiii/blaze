# Requests of db-ty-borrowck

## 1. db-ty-hir: fixture tests of the borrow checker's diagnostics in `crates/hir/src/tests.rs`

What: one test per fixture below, in `crates/hir/src/tests.rs` (db-ty-hir's), the pattern
`MOVED_TWICE` and `a_build_gets_no_mir_of_a_body_whose_borrow_check_failed` already use:
`TestDB::with_files(FIXTURE)`, `crate::attach_db`, `function(&db, name)`, then

```rust
let found: Vec<(&str, String)> = hir_ty_borrowck::borrowck(&db, function(&db, name))
    .diagnostics
    .iter()
    .map(|it| (it.code, it.message.clone()))
    .collect();
assert_eq!(found, expected, "{found:?}");
```

`expected` is the whole list, in order: the codes and order are rustc's for the same program
(the pinned nightly's), and each message is rustc's primary message.

Why: `crates/hir-ty-borrowck` moved its dataflow (move-out, initialization, liveness, loans in
scope) onto dense bitsets and gen/kill transfer functions, and its loans, moves and outlives
constraints onto columnar relations and CSRs. The diagnostics must stay the same, codes,
order, spans and messages, and the crate's own tests (the dataflow engine against a naive
per-statement model on generated graphs) do not see a whole body. These fixtures pin the
user-visible result. The crate cannot have them itself: `TestDB` and `function` are `hir`'s.

```rust
const BORROWCK: &str = r#"
//- minicore: sized, copy
struct S;
fn consume(_: S) {}
fn read(_: &S) {}
fn make() -> S { S }

fn use_after_move() {
    let s = S;
    consume(s);
    consume(s);
}
fn two_mutable_borrows() {
    let mut s = S;
    let a = &mut s;
    let b = &mut s;
    let _ = (a, b);
}
fn mutable_while_shared() {
    let mut s = S;
    let a = &s;
    let b = &mut s;
    read(a);
    let _ = b;
}
fn move_while_borrowed() {
    let s = S;
    let a = &s;
    consume(s);
    read(a);
}
fn assign_while_borrowed() {
    let mut x = 1u32;
    let a = &x;
    x = 2;
    let _ = *a;
}
fn does_not_live_long_enough() {
    let r;
    {
        let s = S;
        r = &s;
    }
    read(r);
}
fn temporary_dropped() {
    let r: &S;
    r = &make();
    read(r);
}
fn needless_mut() {
    let mut s = S;
    consume(s);
}
fn in_order() {
    let mut s = S;
    let a = &s;
    let b = &mut s;
    read(a);
    let _ = b;
    consume(s);
    consume(s);
}
"#;
```

| function | expected `(code, message)`, in order |
| --- | --- |
| `use_after_move` | `` ("E0382", "use of moved value: `s`") `` |
| `two_mutable_borrows` | `` ("E0499", "cannot borrow `s` as mutable more than once at a time") `` |
| `mutable_while_shared` | `` ("E0502", "cannot borrow `s` as mutable because it is also borrowed as immutable") `` |
| `move_while_borrowed` | `` ("E0505", "cannot move out of `s` because it is borrowed") `` |
| `assign_while_borrowed` | `` ("E0506", "cannot assign to `x` because it is borrowed") `` |
| `does_not_live_long_enough` | `` ("E0597", "`s` does not live long enough") `` |
| `temporary_dropped` | `` ("E0716", "temporary value dropped while borrowed") `` |
| `needless_mut` | `` ("unused_mut", "variable does not need to be mutable") `` |
| `in_order` | `("E0502", ..)` then `("E0382", ..)`, the messages as in the rows above |

`unused_mut` is a lint (`is_error()` false), so `needless_mut` also asserts
`hir_ty_borrowck::checked(&db, def).is_ok()`; every other function asserts
`Err(Refused::Errors { .. })`. If a message renders the place differently from rustc (the
backticks of E0382's name, say), the fixture is right and the checker is wrong: say so here
rather than change the expectation.

## 2. db (lane notes): name the borrow checker's techniques in `docs/v2/plan/lanes/db/`

What: a paragraph in the db lane notes (a new `borrowck.md` there, or the notes the lane keeps
for `hir-ty`), the state-of-the-art rule's "name it in your lane notes" for
`crates/hir-ty-borrowck`, which this lane does not own:

> The borrow checker is benchmarked against rustc's `rustc_borrowck` and `rustc_mir_dataflow`
> (`DenseBitSet`/`ChunkedBitSet` gen/kill, `WorkQueue` in RPO, `on_all_children_bits` over the
> move-path tree, `SparseIntervalMatrix` liveness, SCC region inference) and Polonius. It
> matches rustc's dataflow (u64-word bitsets, gen/kill composed once per block, a worklist in
> reverse postorder, Tarjan SCCs with universal regions as a bit matrix per SCC, interval
> sets of points) and beats it in three places. Move paths are numbered in preorder, so a
> subtree is a contiguous id range: a move or init sets or clears a word range and "any
> descendant uninitialized" is a word scan, where rustc visits every child bit by bit. Each
> block's effect is composed once into gen and kill words and the fixpoint applies those,
> where rustc re-applies every statement's effect on each visit of a block; one statically
> dispatched function per statement builds the composition and drives the final walk. Every
> relation (CFG edges both ways, moves and inits per point, loans per local and point, the
> outlives graph, Polonius edges) is one CSR built by an in-place counting sort, two
> allocations and no per-node `Vec`. The universal-region relation is Warshall on a bit
> matrix (`n³/64` word ops). Remaining: a per-body bump arena for the scratch vectors, and
> SIMD row unions where the matrices are wide enough to pay.

Why: the coordinator's state-of-the-art rule asks every lane to name the fastest known
technique for its part in its lane notes.
