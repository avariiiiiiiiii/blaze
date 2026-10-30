//! A value lent to a thread-local for the length of a call.

use std::{cell::RefCell, thread::LocalKey};

/// Runs `f` with `*value` moved into `slot`, and moves what `slot` holds back into `*value` when
/// `f` returns or unwinds: state that the code `f` calls reads and adds to through the
/// thread-local rather than a parameter. Lends of one slot do not nest.
pub fn lend<T: Default + 'static, R>(
    slot: &'static LocalKey<RefCell<Option<T>>>,
    value: &mut T,
    f: impl FnOnce() -> R,
) -> R {
    struct Return<'v, T: 'static> {
        slot: &'static LocalKey<RefCell<Option<T>>>,
        value: &'v mut T,
    }

    impl<T: 'static> Drop for Return<'_, T> {
        fn drop(&mut self) {
            if let Some(lent) = self.slot.with(|slot| slot.borrow_mut().take()) {
                *self.value = lent;
            }
        }
    }

    slot.with(|slot| {
        let mut slot = slot.borrow_mut();
        assert!(slot.is_none(), "scopes lending one thread-local do not nest");
        *slot = Some(std::mem::take(value));
    });
    let _return = Return { slot, value };
    f()
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, panic};

    use super::lend;

    thread_local! {
        static SLOT: RefCell<Option<Vec<u32>>> = const { RefCell::new(None) };
    }

    fn push(it: u32) {
        SLOT.with(|slot| slot.borrow_mut().as_mut().expect("in a lend").push(it));
    }

    #[test]
    fn what_is_added_in_the_lend_comes_back() {
        let mut value = vec![1];
        assert_eq!(
            lend(&SLOT, &mut value, || {
                push(2);
                7
            }),
            7
        );
        assert_eq!(value, [1, 2]);
        assert!(SLOT.with(|slot| slot.borrow().is_none()));
    }

    #[test]
    fn what_is_added_before_an_unwind_comes_back() {
        let mut value = Vec::new();
        let unwound = panic::catch_unwind(panic::AssertUnwindSafe(|| {
            lend(&SLOT, &mut value, || -> () {
                push(3);
                panic!("unwinds")
            })
        }));
        assert!(unwound.is_err());
        assert_eq!(value, [3]);
        assert!(SLOT.with(|slot| slot.borrow().is_none()));
    }

    #[test]
    #[should_panic(expected = "do not nest")]
    fn lends_do_not_nest() {
        let (mut outer, mut inner) = (Vec::new(), Vec::new());
        lend(&SLOT, &mut outer, || lend(&SLOT, &mut inner, || ()));
    }
}
