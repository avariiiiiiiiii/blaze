# Requests from mem-backend-cow

## M1 (lane mem, `crates/blaze-mem/src/pool.rs`): `Cache::share_from`, a shared array made in place

**What.** One constructor beside `Cache::share`, for slots holding an array:

```rust
impl<'p, E: Clone, const N: usize> Cache<'p, Counted<[E; N]>> {
    /// Places `head`, then clones of `fill` up to `N` elements, in a slot with one owner. Each
    /// element is written in place: the array never passes through the stack, so a slot of
    /// megabytes works, and a copy (`head` a whole array) is one pass over the slot.
    pub fn try_share_from(&mut self, head: &[E], fill: &E) -> Result<Shared<'p, [E; N]>, MemError> {
        assert!(head.len() <= N, "{} elements for a slot of {N}", head.len());
        if self.len == 0 {
            self.refill()?;
        }
        self.len -= 1;
        let index = self.items[self.len];
        let ptr = self.pool.slot(index);
        let slot = ptr.as_ptr();
        // SAFETY: a free slot is committed, aligned for `Counted<[E; N]>`, holds no value and is
        // held by this cache alone; its fields are written through raw pointers, never read.
        let items = unsafe {
            std::ptr::addr_of_mut!((*slot).owners).write(AtomicU32::new(1));
            std::ptr::addr_of_mut!((*slot).value).cast::<E>()
        };
        let mut written = Written { pool: self.pool, index, items, len: 0 };
        for item in head {
            // SAFETY: `written.len < head.len() <= N`: in the array, and nothing is there yet.
            unsafe { items.add(written.len).write(item.clone()) };
            written.len += 1;
        }
        while written.len < N {
            // SAFETY: as above, `written.len < N`.
            unsafe { items.add(written.len).write(fill.clone()) };
            written.len += 1;
        }
        mem::forget(written);
        Ok(Shared { pool: self.pool, index, ptr })
    }

    /// As [`Cache::try_share_from`]; stops when the pool is full.
    #[inline]
    #[track_caller]
    pub fn share_from(&mut self, head: &[E], fill: &E) -> Shared<'p, [E; N]> {
        match self.try_share_from(head, fill) {
            Ok(shared) => shared,
            Err(error) => fail(error),
        }
    }
}

/// The elements a `try_share_from` has written: dropped, and the slot returned, if a clone
/// panics.
struct Written<'p, E, T> {
    pool: &'p Pool<T>,
    index: u32,
    items: *mut E,
    len: usize,
}

impl<E, T> Drop for Written<'_, E, T> {
    fn drop(&mut self) {
        // SAFETY: the first `len` elements were written and nothing else owns them; the slot's
        // count is a plain atomic with nothing to drop.
        unsafe { std::ptr::slice_from_raw_parts_mut(self.items, self.len).drop_in_place() };
        self.pool.give_one(self.index);
    }
}
```

**Where.** `crates/blaze-mem/src/pool.rs`, after `impl<'p, T> Cache<'p, Counted<T>>` (`try_share` /
`share`). The fields it writes (`Counted::owners`, `Counted::value`) and `Cache::refill`,
`Pool::slot`, `Pool::give_one` are private to that file, which is why it can't live in the
caller.

**Why.** `crates/blaze-link/src/cow.rs` (L5 of `v2/requests/mem-backend-link.md`) now keeps every
chunk, page and table node of the image state as a `Shared<'static, [E; N]>` slot of a static
pool, and calls `cache.share_from(head, fill)` for every new one and every copy-on-write copy.
`Cache::share(value)` and `Shared::make_mut` take the array by value, so:

- the image state's longer lists get chunks up to 2^24 elements (512 MiB of `Fixup`s), which
  `share([fill; N])` would build on a 2 MiB worker stack;
- a column chunk (up to 88 KiB of `FunctionInfo`s) copied with `make_mut` goes slot -> stack ->
  slot, two passes, where one is the speed of light (a debug build makes more copies, and
  overflows test threads' stacks long before 512 MiB).

`cow.rs` therefore uses neither `share` nor `make_mut` for arrays: `unique` in `cow.rs` copies
with `share_from(&old[..], &old[0])` when `Shared::owners` is above 1, then takes
`Shared::get_mut`. **`cow.rs` does not compile until this lands** (it calls
`Cache::share_from`).

**Budget tests to add in `pool.rs`'s tests:**

- `share_from` against `share` for `[u32; 8]`, `[String; 5]` (not `Copy`): the same elements for
  `head` of lengths 0, 3 and 8.
- A 1 MiB `[u8; 1 << 20]` slot made with `share_from(&[], &7)` and copied with
  `share_from(&a[..], &0)` on a thread built with `std::thread::Builder::stack_size(64 << 10)`:
  it must not overflow, and both read as expected.
- `blaze_alloc_count::assert_at_most(0, ..)` for `share_from` once the cache is warm.
- A `fill` whose `clone` panics at the fifth element (`catch_unwind`): the four written are
  dropped once (count them), and `pool.carved()` does not grow on the next `share_from` (the slot
  came back).

> forwarded to mem (`crates/blaze-mem/src/pool.rs`; mem-next1); told by tmux (requests, pass 29).

## M2 (lane mem, `crates/blaze-mem/src/pool.rs`): `Shared::make_mut`'s signature

`pub fn make_mut(this: &mut Self, cache: &mut Cache<'p, Counted<T>>) -> &mut T` has two elided
input lifetimes and no `self` receiver, so the output's lifetime can't be elided (E0106). The
lifetime it means is `this`'s:

```rust
pub fn make_mut<'s>(this: &'s mut Self, cache: &mut Cache<'p, Counted<T>>) -> &'s mut T
```

`cow.rs` doesn't call it (see M1), so nothing waits on this; it is for blaze-mem's own build.

> forwarded to mem (`crates/blaze-mem/src/pool.rs`; mem-next1); told by tmux (requests, pass 29).

## M3 (lane mem, `crates/blaze-mem/src/pool.rs`): carving large slots commits 64 of them

`Pool::carve` commits `BATCH` (64) slots at once (`self.slots.ensure(end * SIZE)`). For the image
state's longer-list classes that is up to 64 x 512 MiB of commit for the first list of that
class (each class pool reserves 8 GiB, so in practice its whole reservation). The pages stay
untouched, so nothing becomes resident, but the commit is counted against `Tag::Link` and,
under `vm.overcommit_memory=2`, could be refused. A fix in blaze-mem's terms: carve
`clamp(2 MiB / SIZE, 1, 64)` slots per batch when `SIZE` is above 32 KiB, or commit a slot's
pages when it is handed out rather than when it is carved. Not needed for correctness; `cow.rs`
works either way.

> forwarded to mem (`crates/blaze-mem/src/pool.rs`; mem-next1); told by tmux (requests, pass 29).
