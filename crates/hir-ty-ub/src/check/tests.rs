use hir_ty_mir::mir::Shadow as _;

use super::{Shadow, UNINIT, report::thread_name, valid};
use crate::{
    ids::{Span, ThreadId},
    memory::MemError,
};

// rustc_const_eval validity.rs, `fmt_range`: every arm.
#[test]
fn fmt_range_words() {
    assert_eq!(valid::fmt_range(0, 1, u8::MAX.into()), "less or equal to 1");
    assert_eq!(valid::fmt_range(1, u8::MAX.into(), u8::MAX.into()), "greater or equal to 1");
    assert_eq!(valid::fmt_range(2, 2, u8::MAX.into()), "equal to 2");
    assert_eq!(valid::fmt_range(1, 10, u8::MAX.into()), "in the range 1..=10");
    assert_eq!(valid::fmt_range(10, 1, u8::MAX.into()), "less or equal to 1, or greater or equal to 10");
}

#[test]
fn hex_is_two_digits_a_byte() {
    assert_eq!(valid::hex(2, 1), "0x02");
    assert_eq!(valid::hex(0xd800, 4), "0x0000d800");
    assert_eq!(valid::hex(0, 8), "0x0000000000000000");
}

#[test]
fn merge_joins_touching_and_overlapping() {
    let mut cells = vec![Span::new(8, 4), Span::new(0, 4), Span::new(4, 2), Span::new(10, 6), Span::new(20, 0)];
    valid::merge(&mut cells);
    assert_eq!(cells, vec![Span::new(0, 6), Span::new(8, 8)]);
}

#[test]
fn value_error_display() {
    let whole = valid::ValueError { ty: "bool".into(), path: String::new(), msg: "encountered 0x02, but expected a boolean".into() };
    assert_eq!(whole.to_string(), "constructing invalid value of type bool: encountered 0x02, but expected a boolean");
    let field = valid::ValueError {
        ty: "Option<bool>".into(),
        path: ".<enum-variant(Some)>.0".into(),
        msg: "encountered 0x02, but expected a boolean".into(),
    };
    assert_eq!(
        field.to_string(),
        "constructing invalid value of type Option<bool>: at .<enum-variant(Some)>.0, encountered 0x02, but expected a boolean"
    );
}

#[test]
fn thread_names_are_miris() {
    assert_eq!(thread_name(ThreadId::MAIN), "main");
    assert_eq!(thread_name(ThreadId(3)), "unnamed-3");
}

// The evaluator calls `require_init` with no machine at hand: the first failure waits in the
// slot for `run_test`, later ones don't replace it.
#[test]
fn require_init_keeps_the_first_failure() {
    UNINIT.take();
    let mut value = Shadow::init(4);
    value.put(4, &Shadow(crate::memory::Bytes::uninit(4)));
    assert!(value.require_init(0..4).is_ok());
    assert!(value.require_init(2..8).is_err());
    assert!(value.require_init(6..8).is_err());
    assert_eq!(UNINIT.take(), Some(MemError::Uninit(None)));
    assert_eq!(UNINIT.take(), None);
}
