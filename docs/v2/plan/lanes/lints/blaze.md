---
status: plan
summary: blaze's own lints (`blaze::hot_clone`, `hot_alloc`, `hot_alloc_call`, `hot_dyn`, `std_hashmap`, the group `blaze::hot`) — what each flags, how to exempt a line, and what they miss; the page each finding's help line links to.
date: 2026-10-29
---
# blaze's own lints

`crates/blaze-lints/src/blaze.rs` and `src/blaze/`. They check the rules
`tools/v2/hot_lint.py` checks by pattern ([principles](../../goals/principles.md): allocate
like a game engine, no `dyn` in hot paths, no SipHash), but from types and resolutions: a
`.clone()` of a `Copy` struct is not flagged, a `String` behind a type alias is, and so is a
call of std's that allocates three calls down.

Each lint is allow by default. Turn the group on where code is hot:

```rust
#[deny(blaze::hot)]
fn step(world: &mut World) { /* ... */ }
```

or for a crate, `--deny=blaze::hot`. `#[allow(blaze::hot)]` (or `#[allow(blaze::hot_alloc)]`,
...) turns it off again for an item. `blaze::hot` is all five lints below.

## Exempting a line

A finding is dropped when its line, or the line above it, has a comment with the lint's tag and
a reason:

| lint | tag |
|---|---|
| `hot_clone` | `// clone: <reason>` |
| `hot_alloc`, `hot_alloc_call`, `std_hashmap` | `// alloc: <reason>` |
| `hot_dyn` | `// dyn: <reason>` |

```rust
let name = name.clone(); // clone: kept past the frame, once per load
// alloc: the error path
let message = format!("missing {name}");
```

The reason must not be empty: `// clone:` alone exempts nothing. A doc comment (`///`, `//!`)
is not an exemption. These are the tags `hot_lint.py` reads, so a line exempt for one is exempt
for the other.

## `hot_clone`

A call of `Clone::clone` (`x.clone()`, `Clone::clone(&x)`, `T::clone(&x)`) whose `Self` costs
more than a copy. A type is copy-like, and its clone not flagged, when it is `Copy`; a
reference, raw pointer, function or function pointer, primitive or `!`; or a tuple, array or
struct or enum whose `Clone` is built in or made by `#[derive(Clone)]` and every field of which
is copy-like. The message says why the clone costs:

- *allocates*: a hand-written `Clone` that reaches the allocator (`String`, `Vec`, `Box`);
- *bumps a reference count*: `Arc`, `Rc` and their `Weak`s;
- *runs a hand-written `Clone`*: one that does not allocate, but is code of its own;
- *clones a type not known to be `Copy`*: a type parameter bound by `Clone` and not `Copy`, a
  projection;
- *clones what a closure captures*.

When the cost is in a field (`#[derive(Clone)] struct Named { name: String }`), a note names
the field's type. Fix: borrow, copy an ID, or make the type not `Clone`.

## `hot_alloc`

A call that reaches the global allocator. A function reaches it when it is the allocator's
(`#[rustc_allocator]`, `#[rustc_allocator_zeroed]`, `#[rustc_reallocator]`: the
`__rust_alloc` family), the `exchange_malloc` lang item or the `box_new` intrinsic, or when a
call in its body reaches it. This is asked of the instance called, its generic arguments
given, with each trait method resolved to the impl that runs: std's `Vec::with_capacity`
allocates only through `A: Allocator` a few calls down, and `collect` only through
`B: FromIterator`, so a question about the function alone (not its arguments) would answer
"no" for both. Recursion is fine: a cycle of functions starts as not reaching and is iterated
to its answer.

The walk stops at a function of a workspace member (a crate whose origin is local): its
allocations are found in its own body, once, and a call of it is not a finding. It goes on
through std and registry crates, and through a workspace function they call back through a
generic only for `hot_alloc_call`. So a crate denying `hot_alloc` (`hot_alloc = "deny"` in
`[workspace.metadata.blaze.lints]`, or `#![deny(blaze::hot_alloc)]` in its root) is told where each allocation is made, not at every caller up the stack.

Flagged: `String::from`, `.to_string()`, `.to_owned()`, `.to_vec()`, `Vec::with_capacity`,
`.collect()` into an owning type, `Box::new`, `Arc::new`, a call of a registry crate's function
that does any of these. A macro call is reported once, at the call: `format!`, `vec![1, 2]`.

Also flagged, though it allocates nothing yet: a call that makes a value of the global
allocator's, which allocates from the global heap once it grows (rule `bump-allocate`):
`Vec::new()`, `vec![]`, `String::new()`, `VecDeque::default()`, `FxHashMap::default()`. A value is
the global allocator's when a type argument of it is `Global`, or it is `String` or std's
`HashMap` or `HashSet` (a map hashing with `RandomState` is `std_hashmap`'s finding instead). A
call makes one when its callee is a trait's method other than `clone`, or a method of the type's
own impl; a function that hands one on (`mem::take`) does not. Fix: name the allocator
(`Vec::new_in(frame)`, from `blaze_mem::frame::current`), grow a buffer the caller keeps, or say
why it needs the global heap with `// alloc: <why>`.

Not flagged:

- A collection that names its allocator: `Vec::new_in(frame)`.
- A value of the global allocator's made by a method of another type
  (`opt.unwrap_or_default()`, `s.into_bytes()`) or a struct of your own holding one
  (`Index::default()`): only the call that makes the collection itself is found.
- `Clone::clone`: `hot_clone` says it.
- A `&mut self` method on a place (`buf.push(x)`, `self.out.extend(..)`): a buffer the caller
  keeps grows only past its capacity, which a reused buffer reaches once. On a temporary it is
  flagged.

What it misses, and why:

- A call of a workspace function that allocates: `hot_alloc` finds it in that function, and
  `hot_alloc_call` at the call.
- Operators (`a + b` on `String`), `Drop`, and calls a pattern or `for` loop makes: only call and
  method call expressions are followed.
- A call through `dyn` or through a type parameter of the hot function itself (`f: F` with
  `F: Fn()`, `T: Clone`): which impl runs is not known in the function, so the call reaches
  nothing. Through `dyn` it is `hot_dyn`'s.
- A closure handed to a function (`map(|x| x.to_string())`) where the closure, not the
  function, allocates and the closure's body is not this body's: a closure written in the hot
  body is part of it, and its calls are checked there.
- An instance whose generic arguments grow past 256 type nodes (a function that instantiates
  itself with ever larger arguments, which rustc refuses at its `type_length_limit`): not
  followed.
- Derived methods other than `clone` and `default` (`fmt`, `hash`, `eq`), which have no body of
  their own: counted as not reaching. Formatting allocates only when written into a `String`,
  which the `format!` call is flagged for.

## `hot_alloc_call`

A call that reaches the global allocator only through a function of a workspace member, which
`hot_alloc` does not walk into. It is in `blaze::hot` and not turned on by `hot_alloc`: an item
marked `#[deny(blaze::hot)]` promises that nothing it calls allocates, the workspace's own
helpers included, so the call of `helper()` is a finding there, through any depth of calls.
Under `hot_alloc` alone the same allocation is one finding in `helper` itself.

## `hot_dyn`

A call that goes through a vtable: a method call or path call resolved to a trait's method with
`Self` a `dyn Trait` once references and boxes are peeled (`shape.area()` with
`shape: &dyn Shape`, `Shape::area(&*boxed)`), and a call of a `dyn Fn*` value through any
pointer (`f(x)` with `f: &dyn Fn(u32)` or `Box<dyn Fn()>`). `dyn` written in a type, with no
call through it, is not flagged. Fix: a concrete type, a generic, or a `match` on a tag.

## `std_hashmap`

std's `HashMap` or `HashSet` hashing with `RandomState` (SipHash, seeded per map). Two forms:

- a written type: `HashMap<K, V>` or `HashMap<K, V, RandomState>` in a signature, a field, a type
  alias, a `let`'s type, a turbofish;
- an expression that makes one: `HashMap::new()`, `HashSet::with_capacity(n)`,
  `Default::default()`, `.collect()` (with no turbofish, which is a written type of its own).

Not flagged: any other hasher (`rustc_hash::FxHashMap`, `HashMap<K, V, FxBuildHasher>`, a
hasher parameter `S`). A `let m: HashMap<K, V> = HashMap::new();` has two findings, the type and
the call; one `// alloc:` comment exempts both. Fix: `rustc_hash::FxHashMap` (or `FxHashSet`), or
index by content hash.

Misses: a `HashMap` imported under another name (`use std::collections::HashMap as Map;`
then `Map<K, V>`) in a written type, which is found by its last segment's name before it is
resolved; the expression form still finds what such a type makes.

## Cost

The per-node part looks at path types only, and resolves one only when its last segment is
`HashMap` or `HashSet`. The body part walks the body's HIR once, after the body's inference,
and allocates only to report. Whether an instance reaches the allocator is a salsa query
(`reaches_alloc`), shared by every body that calls it and kept across edits that do not change
the functions it reaches.
