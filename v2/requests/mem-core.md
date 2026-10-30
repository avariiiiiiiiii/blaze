
## dedup: the plane's Kahn order onto `radix::Csr`

From `dedup-order` (report `/tmp/blaze-lanes/v2-dedup-order.md`). `blaze_radix::Csr<T>` (reached as
`blaze_ids::radix::Csr`; `crates/blaze-radix/src/csr.rs`) is now the one stable counting sort into
compressed sparse rows: `Csr::build(rows, pairs)` / `csr.rebuild(rows, pairs, &mut cursors)` walk a
cloneable iterator of `(row: u32, item)` twice (count, then place at each row's cursor), with
`row(r) -> &[T]`, `range(r)`, `items()`, `into_parts() -> (starts, items)` (`starts` has
`rows + 1` entries). A warm `rebuild` allocates nothing. Its tests have the old copies as the
proptest reference.

`crates/blaze-graph/src/plane.rs` (already `use blaze_radix as radix;`):

- `struct Batch`: `then_at: Vec<u32>, then: Vec<u32>,` become `then: radix::Csr<u32>, cursors: Vec<u32>,`
  (the doc comment above `waiting` stays).
- `Batch::sort`: replace from `self.waiting.clear();` through `self.then_at[0] = 0;` with

```rust
        let (deps, deps_at) = (&self.deps, &self.deps_at);
        let of = |at: usize| deps[deps_at[at] as usize..deps_at[at + 1] as usize].iter().filter(|&&dep| dep & NEW != 0);
        self.waiting.clear();
        self.waiting.extend((0..len).map(|at| of(at).count() as u32));
        self.then.rebuild(len, (0..len).flat_map(|at| of(at).map(move |&dep| (dep & !NEW, at as u32))), &mut self.cursors);
```

  and in the Kahn loop

```rust
            for then in self.then_at[at]..self.then_at[at + 1] {
                let next = self.then[then as usize] as usize;
```

  becomes

```rust
            for then in self.then.range(at) {
                let next = self.then.items()[then] as usize;
```

Why: one counting sort in the tree; warm batches still allocate nothing.

## dedup: three lines in `blaze-core` for the usercode queue's wheel move

From `dedup-order` (report `/tmp/blaze-lanes/v2-dedup-order.md`). `blaze-usercode`'s test queue had
its own timing wheel (`crates/blaze-usercode/src/test/queue/wheel.rs`); it moves onto
`blaze_core::wheel::Wheel` (request `## dedup: the test queue onto blaze_core::wheel::Wheel` in
`v2/requests/usercode.md`). The copy and its tests now live in the new file
`crates/blaze-core/src/wheel/differential_tests.rs` (mine, written): the copy as the spec of a proptest
differential, its three tests adapted. They need:

1. `crates/blaze-core/src/wheel.rs`, after `pub fn is_empty`:

```rust
    /// The timers made so far, set or free: what the slab holds, which a steady stream of
    /// timers does not grow.
    pub fn slots(&self) -> usize {
        self.timers.slots()
    }
```

2. `crates/blaze-core/src/wheel.rs`, beside `#[cfg(test)] mod reference;`:

```rust
#[cfg(test)]
mod differential_tests;
```

3. `crates/blaze-core/Cargo.toml` `[dev-dependencies]`: `proptest.workspace = true` (a root
   workspace dependency already).

## dedup: seeded RNG onto blaze_ids::rng

From `dedup-codec-rng-tools` (report `/tmp/blaze-lanes/v2-dedup-codec-rng-tools.md`). File
`crates/blaze-graph/src/exec.rs` (your report names it). The xorshift stream in
`fn the_throttle_matches_a_queue_per_subsystem` moves onto `blaze_ids::rng::SplitMix64`
(`blaze-graph` already has `blaze-ids` in `[dev-dependencies]`, and its `tests.rs` uses it). The
test pins no values.

1. In `mod tests {`, among its `use` lines, add `use blaze_ids::rng::SplitMix64;`.

2. Before:

```rust
            let mut state = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
            let mut rand = move |below: u64| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state % below
            };
```

after:

```rust
            let mut rng = SplitMix64::new(seed);
```

and the five uses in that function: `rand(8) == 0` becomes `rng.one_in(8)`,
`tags[rand(4) as usize]` (twice) becomes `tags[rng.index(4)]`, `rand(3) > 0` becomes
`!rng.one_in(3)`, and `started.swap_remove(rand(started.len() as u64) as usize)` becomes
`started.swap_remove(rng.index(started.len()))`.

Also, so you know: I edited `crates/blaze-core/src/wheel/differential_tests.rs` (untracked, not
yet declared in `wheel.rs`) the same way: its xorshift in `the_wheel_fires_what_a_list_would`
is now `let mut rng = SplitMix64::new(seed);` with `rng.below(..)`/`rng.index(..)`, and the
file has `use blaze_ids::rng::SplitMix64;` (blaze-core has `blaze-ids` as a dev-dependency now).
If it is declared as a module of the library, keep it under `#[cfg(test)]`.
