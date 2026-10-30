//! `InvalidValue`, `EnumIntrinsicsNonEnums`, `MutableTransmutes`, `CheckTransmutes` and
//! `InteriorMutableConsts`: rustc's UI tests of `invalid_value`, `enum_intrinsics_non_enums`,
//! `mutable_transmutes`, `ptr_to_integer_transmute_in_consts`, `unnecessary_transmutes`,
//! `integer_to_ptr_transmutes` and `const_item_interior_mutations`, in `values/ui/` byte for byte
//! as rust-lang/rust `ad3d0bc141a` has them in `tests/ui/` (MIT or Apache-2.0), with the items
//! of std they use written as `library/` writes them.

use super::{Test, Ui, ui};

fn check(name: &str, test: Test, core: &str, std: &str, source: &str, stderr: &str) {
    let actual = ui(Ui { name, test, core, std: Some(("", std)), source, ..Ui::default() });
    assert_eq!(actual, stderr, "{name}");
}

/// `core::intrinsics::transmute`, as `std::mem` re-exports it.
const TRANSMUTE: &str = r#"
pub mod intrinsics {
    #[rustc_intrinsic]
    #[rustc_diagnostic_item = "transmute"]
    pub const unsafe fn transmute<Src, Dst>(src: Src) -> Dst;
}
pub mod mem {
    pub use crate::intrinsics::transmute;
}
"#;

const DISCRIMINANT: &str = r#"
pub mod mem {
    pub use core::mem::Discriminant;

    #[rustc_diagnostic_item = "mem_discriminant"]
    pub const fn discriminant<T>(v: &T) -> Discriminant<T> {
        loop {}
    }

    #[rustc_diagnostic_item = "mem_variant_count"]
    pub const fn variant_count<T>() -> usize {
        loop {}
    }
}
"#;

const UNINIT: &str = r#"
pub mod intrinsics {
    #[rustc_intrinsic]
    #[rustc_diagnostic_item = "transmute"]
    pub const unsafe fn transmute<Src, Dst>(src: Src) -> Dst;
}
pub mod mem {
    pub use crate::intrinsics::transmute;
    use core::mem::ManuallyDrop;

    #[deprecated(since = "1.39.0", note = "use `mem::MaybeUninit` instead")]
    #[rustc_diagnostic_item = "mem_uninitialized"]
    pub unsafe fn uninitialized<T>() -> T {
        loop {}
    }

    #[rustc_diagnostic_item = "mem_zeroed"]
    pub const unsafe fn zeroed<T>() -> T {
        loop {}
    }

    #[lang = "maybe_uninit"]
    #[rustc_diagnostic_item = "MaybeUninit"]
    #[repr(transparent)]
    pub union MaybeUninit<T> {
        uninit: (),
        value: ManuallyDrop<T>,
    }

    impl<T> MaybeUninit<T> {
        #[rustc_diagnostic_item = "maybe_uninit_uninit"]
        pub const fn uninit() -> MaybeUninit<T> {
            MaybeUninit { uninit: () }
        }

        #[rustc_diagnostic_item = "maybe_uninit_zeroed"]
        pub const fn zeroed() -> MaybeUninit<T> {
            loop {}
        }

        #[rustc_diagnostic_item = "assume_init"]
        pub const unsafe fn assume_init(self) -> T {
            loop {}
        }
    }
}
pub mod ptr {
    pub use core::ptr::NonNull;
}
pub mod num {
    pub use core::num::NonZero;
}
pub mod pat {
    pub use core::pattern_type;
}
"#;

const CELLS: &str = r#"
pub mod ops {
    pub use core::ops::Deref;
}
pub mod cell {
    use core::option::Option::{self, None};
    use core::result::Result;

    #[lang = "unsafe_cell"]
    #[repr(transparent)]
    pub struct UnsafeCell<T: ?Sized> {
        value: T,
    }

    impl<T> UnsafeCell<T> {
        pub const fn new(value: T) -> UnsafeCell<T> {
            UnsafeCell { value }
        }

        #[rustc_should_not_be_called_on_const_items]
        pub const unsafe fn replace(&self, value: T) -> T {
            loop {}
        }
    }

    impl<T: ?Sized> UnsafeCell<T> {
        #[rustc_should_not_be_called_on_const_items]
        pub const fn get(&self) -> *mut T {
            loop {}
        }

        #[rustc_should_not_be_called_on_const_items]
        pub const unsafe fn as_ref_unchecked(&self) -> &T {
            loop {}
        }

        #[rustc_should_not_be_called_on_const_items]
        pub const unsafe fn as_mut_unchecked(&self) -> &mut T {
            loop {}
        }
    }

    #[repr(transparent)]
    #[rustc_diagnostic_item = "SyncUnsafeCell"]
    pub struct SyncUnsafeCell<T: ?Sized> {
        value: UnsafeCell<T>,
    }

    impl<T> SyncUnsafeCell<T> {
        pub const fn new(value: T) -> Self {
            SyncUnsafeCell { value: UnsafeCell::new(value) }
        }
    }

    impl<T: ?Sized> SyncUnsafeCell<T> {
        #[rustc_should_not_be_called_on_const_items]
        pub const fn get(&self) -> *mut T {
            loop {}
        }
    }

    #[rustc_diagnostic_item = "Cell"]
    #[repr(transparent)]
    pub struct Cell<T: ?Sized> {
        value: UnsafeCell<T>,
    }

    impl<T> Cell<T> {
        pub const fn new(value: T) -> Cell<T> {
            Cell { value: UnsafeCell::new(value) }
        }

        #[rustc_should_not_be_called_on_const_items]
        pub const fn set(&self, val: T) {
            loop {}
        }

        #[rustc_should_not_be_called_on_const_items]
        pub fn swap(&self, other: &Self) {
            loop {}
        }

        #[rustc_should_not_be_called_on_const_items]
        pub const fn replace(&self, val: T) -> T {
            loop {}
        }
    }

    impl<T: Copy> Cell<T> {
        #[rustc_should_not_be_called_on_const_items]
        pub const fn get(&self) -> T {
            loop {}
        }

        #[rustc_should_not_be_called_on_const_items]
        pub fn update(&self, f: impl FnOnce(T) -> T) {
            loop {}
        }
    }

    #[rustc_diagnostic_item = "RefCell"]
    pub struct RefCell<T: ?Sized> {
        value: UnsafeCell<T>,
    }

    #[non_exhaustive]
    pub struct BorrowError {}

    #[non_exhaustive]
    pub struct BorrowMutError {}

    #[rustc_diagnostic_item = "RefCellRef"]
    pub struct Ref<'b, T: ?Sized + 'b> {
        value: &'b T,
    }

    #[rustc_diagnostic_item = "RefCellRefMut"]
    pub struct RefMut<'b, T: ?Sized + 'b> {
        value: &'b mut T,
    }

    impl<T> RefCell<T> {
        pub const fn new(value: T) -> RefCell<T> {
            RefCell { value: UnsafeCell::new(value) }
        }

        #[rustc_should_not_be_called_on_const_items]
        pub const fn replace(&self, t: T) -> T {
            loop {}
        }

        #[rustc_should_not_be_called_on_const_items]
        pub fn replace_with<F: FnOnce(&mut T) -> T>(&self, f: F) -> T {
            loop {}
        }

        #[rustc_should_not_be_called_on_const_items]
        pub const fn swap(&self, other: &Self) {
            loop {}
        }
    }

    impl<T: ?Sized> RefCell<T> {
        #[rustc_should_not_be_called_on_const_items]
        pub const fn borrow(&self) -> Ref<'_, T> {
            loop {}
        }

        #[rustc_should_not_be_called_on_const_items]
        pub const fn try_borrow(&self) -> Result<Ref<'_, T>, BorrowError> {
            loop {}
        }

        #[rustc_should_not_be_called_on_const_items]
        pub const fn borrow_mut(&self) -> RefMut<'_, T> {
            loop {}
        }

        #[rustc_should_not_be_called_on_const_items]
        pub const fn try_borrow_mut(&self) -> Result<RefMut<'_, T>, BorrowMutError> {
            loop {}
        }
    }

    pub struct OnceCell<T> {
        inner: UnsafeCell<Option<T>>,
    }

    impl<T> OnceCell<T> {
        pub const fn new() -> OnceCell<T> {
            OnceCell { inner: UnsafeCell::new(None) }
        }

        #[rustc_should_not_be_called_on_const_items]
        pub fn set(&self, value: T) -> Result<(), T> {
            loop {}
        }

        #[rustc_should_not_be_called_on_const_items]
        pub fn try_insert(&self, value: T) -> Result<&T, (&T, T)> {
            loop {}
        }

        #[rustc_should_not_be_called_on_const_items]
        pub fn get_or_init<F>(&self, f: F) -> &T
        where
            F: FnOnce() -> T,
        {
            loop {}
        }

        #[rustc_should_not_be_called_on_const_items]
        pub fn get_or_try_init<F, E>(&self, f: F) -> Result<&T, E>
        where
            F: FnOnce() -> Result<T, E>,
        {
            loop {}
        }
    }

    pub struct LazyCell<T, F = fn() -> T> {
        state: UnsafeCell<Option<T>>,
        init: F,
    }

    impl<T, F: FnOnce() -> T> LazyCell<T, F> {
        pub const fn new(f: F) -> LazyCell<T, F> {
            LazyCell { state: UnsafeCell::new(None), init: f }
        }

        #[rustc_should_not_be_called_on_const_items]
        pub fn force(this: &LazyCell<T, F>) -> &T {
            loop {}
        }
    }
}
"#;

#[test]
fn transmute_imut_to_mut() {
    check(
        "transmute-imut-to-mut.rs",
        Test::Fails,
        "sized",
        TRANSMUTE,
        include_str!("values/ui/transmute-imut-to-mut.rs"),
        include_str!("values/ui/transmute-imut-to-mut.stderr"),
    );
}

#[test]
fn lint_enum_intrinsics_non_enums() {
    check(
        "lint-enum-intrinsics-non-enums.rs",
        Test::Fails,
        "sized, discriminant",
        DISCRIMINANT,
        include_str!("values/ui/lint-enum-intrinsics-non-enums.rs"),
        include_str!("values/ui/lint-enum-intrinsics-non-enums.stderr"),
    );
}

#[test]
fn ptr_to_int_transmute_in_consts_issue_87525() {
    check(
        "ptr-to-int-transmute-in-consts-issue-87525.rs",
        Test::Fails,
        "sized, add",
        TRANSMUTE,
        include_str!("values/ui/ptr-to-int-transmute-in-consts-issue-87525.rs"),
        include_str!("values/ui/ptr-to-int-transmute-in-consts-issue-87525.stderr"),
    );
}

#[test]
fn int_to_ptr() {
    check(
        "int_to_ptr.rs",
        Test::Passes,
        "sized, add, fn",
        TRANSMUTE,
        include_str!("values/ui/int_to_ptr.rs"),
        include_str!("values/ui/int_to_ptr.stderr"),
    );
}

#[test]
fn unnecessary_transmutation() {
    check(
        "unnecessary-transmutation.rs",
        Test::Fails,
        "sized",
        TRANSMUTE,
        include_str!("values/ui/unnecessary-transmutation.rs"),
        include_str!("values/ui/unnecessary-transmutation.stderr"),
    );
}

/// `NonZero<u32>` is core's, printed untrimmed with its `core::num::niche_types` inner type;
/// it needs minicore's `non_zero` to have them (requested of the owner of `test-utils`).
#[test]
fn invalid_value() {
    check(
        "invalid_value.rs",
        Test::Fails,
        "sized, option, result, send, non_null, non_zero, pat, manually_drop, fn",
        UNINIT,
        include_str!("values/ui/invalid_value.rs"),
        include_str!("values/ui/invalid_value.stderr"),
    );
}

#[test]
fn const_item_interior_mutations_const_cell() {
    check(
        "const-item-interior-mutations-const-cell.rs",
        Test::Passes,
        "sized, copy, option, result, fn, add, deref",
        CELLS,
        include_str!("values/ui/const-item-interior-mutations-const-cell.rs"),
        include_str!("values/ui/const-item-interior-mutations-const-cell.stderr"),
    );
}

#[test]
fn const_item_interior_mutations_const_deref() {
    check(
        "const-item-interior-mutations-const-deref.rs",
        Test::Passes,
        "sized, copy, option, result, fn, deref",
        CELLS,
        include_str!("values/ui/const-item-interior-mutations-const-deref.rs"),
        "",
    );
}
