//! Enumerating every value of small types, for the exhaustive tests of the permission state
//! machine. Miri: `tree_borrows/exhaustive.rs`.

pub(super) trait Exhaustive: Sized {
    fn exhaustive() -> Box<dyn Iterator<Item = Self>>;
}

macro_rules! precondition {
    ($cond:expr) => {
        if !$cond {
            continue;
        }
    };
}
pub(super) use precondition;

/// Miri's `!`: no values.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum Never {}

impl Exhaustive for Never {
    fn exhaustive() -> Box<dyn Iterator<Item = Self>> {
        Box::new(std::iter::empty())
    }
}

impl Exhaustive for () {
    fn exhaustive() -> Box<dyn Iterator<Item = Self>> {
        Box::new(std::iter::once(()))
    }
}

impl Exhaustive for bool {
    fn exhaustive() -> Box<dyn Iterator<Item = Self>> {
        Box::new([true, false].into_iter())
    }
}

impl<T: Exhaustive + 'static> Exhaustive for Option<T> {
    fn exhaustive() -> Box<dyn Iterator<Item = Self>> {
        Box::new(std::iter::once(None).chain(T::exhaustive().map(Some)))
    }
}

impl<T1: Exhaustive + Clone + 'static, T2: Exhaustive + 'static> Exhaustive for (T1, T2) {
    fn exhaustive() -> Box<dyn Iterator<Item = Self>> {
        Box::new(T1::exhaustive().flat_map(|t1| T2::exhaustive().map(move |t2| (t1.clone(), t2))))
    }
}

impl<T: Exhaustive + 'static> Exhaustive for [T; 1] {
    fn exhaustive() -> Box<dyn Iterator<Item = Self>> {
        Box::new(T::exhaustive().map(|t| [t]))
    }
}

impl<T: Exhaustive + Clone + 'static> Exhaustive for [T; 2] {
    fn exhaustive() -> Box<dyn Iterator<Item = Self>> {
        Box::new(T::exhaustive().flat_map(|t1| T::exhaustive().map(move |t2| [t1.clone(), t2])))
    }
}

impl<T: Exhaustive + Clone + 'static> Exhaustive for [T; 3] {
    fn exhaustive() -> Box<dyn Iterator<Item = Self>> {
        Box::new(
            <[T; 2]>::exhaustive()
                .flat_map(|[t1, t2]| T::exhaustive().map(move |t3| [t1.clone(), t2.clone(), t3])),
        )
    }
}
