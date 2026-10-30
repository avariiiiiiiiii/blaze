//@ check-fail
//@ run-rustfix

#![allow(dead_code)] // For the rustfix-ed code.

use std::mem::ManuallyDrop;
use std::ops::Deref;

unsafe fn test_const(ptr: *const [u8]) {
    let _ = (*ptr)[..16];
    //~^ ERROR implicit autoref
}

struct Test {
    field: [u8],
}


unsafe fn test_overloaded_deref_const(ptr: *const ManuallyDrop<Test>) {
    let _ = (*ptr).field;
    //~^ ERROR implicit autoref
    let _ = &raw const (*ptr).field;
    //~^ ERROR implicit autoref
}

unsafe fn test_overloaded_deref_mut(ptr: *mut ManuallyDrop<Test>) {
    let _ = (*ptr).field;
    //~^ ERROR implicit autoref
}

unsafe fn test_double_overloaded_deref_const(ptr: *const ManuallyDrop<ManuallyDrop<Test>>) {
    let _ = (*ptr).field;
    //~^ ERROR implicit autoref
}

unsafe fn test_manually_overloaded_deref() {
    struct W<T>(T);

    impl<T> Deref for W<T> {
        type Target = T;
        fn deref(&self) -> &T {
            &self.0
        }
    }

    let w: W<i32> = W(5);
    let w = &raw const w;
    let _p: *const i32 = &raw const **w;
    //~^ ERROR implicit autoref
}

unsafe fn test_no_attr(ptr: *mut ManuallyDrop<u8>) {
    // Should not warn, as `ManuallyDrop::write` is not
    // annotated with `#[rustc_no_implicit_auto_ref]`
    ptr.write(ManuallyDrop::new(1));
}

fn main() {}
