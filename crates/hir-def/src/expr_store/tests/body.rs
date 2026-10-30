mod block;

use crate::{
    DefWithBodyId, ModuleDefId,
    expr_store::path::{GenericArg, Path},
    hir::{Item, MatchArm, Statement},
    nameres::crate_def_map,
    test_db::{TestDB, TestDBExt},
};
use expect_test::{Expect, expect};
use la_arena::RawIdx;
use test_fixture::WithFixture;

use super::super::*;

fn lower(#[rust_analyzer::rust_fixture] ra_fixture: &str) -> (TestDB, DefWithBodyId) {
    let db = TestDB::with_files(ra_fixture);

    let krate = db.fetch_test_crate();
    let def_map = crate_def_map(&db, krate);
    let mut fn_def = None;
    'outer: for (_, module) in def_map.modules(&db) {
        for decl in module.scope(&db).declarations() {
            if let ModuleDefId::FunctionId(it) = decl {
                fn_def = Some(it);
                break 'outer;
            }
        }
    }
    let fn_def = fn_def.unwrap().into();

    Body::of(&db, fn_def);
    round_trip(&db, fn_def);
    (db, fn_def)
}

/// The body of `def` and its source maps, written and read back.
fn round_trip(db: &TestDB, def: DefWithBodyId) {
    let (body, source_map) = Body::with_source_map(db, def);
    crate::expr_store::persist::assert_round_trip(body, source_map);
    if let Some((body, source_map)) = Body::relative(db, def) {
        crate::expr_store::persist::assert_round_trip(body, source_map);
    }
}

fn pretty_print(#[rust_analyzer::rust_fixture] ra_fixture: &str, expect: Expect) {
    let db = TestDB::with_files(ra_fixture);

    let krate = db.fetch_test_crate();
    let def_map = crate_def_map(&db, krate);
    let mut fn_def = None;
    'outer: for (_, module) in def_map.modules(&db) {
        for decl in module.scope(&db).declarations() {
            if let ModuleDefId::FunctionId(it) = decl {
                fn_def = Some(it);
                break 'outer;
            }
        }
    }
    let fn_def = fn_def.unwrap().into();

    expect.assert_eq(&Body::of(&db, fn_def).pretty_print(&db, fn_def, Edition::CURRENT));
    round_trip(&db, fn_def);
}

fn def_map_at(#[rust_analyzer::rust_fixture] ra_fixture: &str) -> String {
    let (db, position) = TestDB::with_position(ra_fixture);

    let module = db.module_at_position(position);
    salsa::plumbing::attach(&db, || module.def_map(&db).dump(&db))
}

fn check_block_scopes_at(#[rust_analyzer::rust_fixture] ra_fixture: &str, expect: Expect) {
    let (db, position) = TestDB::with_position(ra_fixture);

    let module = db.module_at_position(position);
    let actual = salsa::plumbing::attach(&db, || format!("{module:#?}"));
    expect.assert_eq(&actual);
}

fn check_at(#[rust_analyzer::rust_fixture] ra_fixture: &str, expect: Expect) {
    let actual = def_map_at(ra_fixture);
    expect.assert_eq(&actual);
}

/// A body that lowers to every kind of expression, pattern and statement.
const EVERY_VARIANT: &str = r#"
//- minicore: range, fmt
struct S { a: i32, b: i32 }
struct T(i32, i32);
const C: i32 = 0;
macro_rules! two { () => { 2 } }
macro_rules! wild { () => { _ } }
fn main(x: i32, s: S, v: &[i32], r: &mut i32) {
    let _ = const { 1 };
    let _ = ("s", b"ab", c"c", 'c', b'a', true, 3, 1i64, 2u8, 0x10, 1.5, 2.5f32, 2f64);
    let _ = (two!(), -x, !true, *r, &mut x, &raw const x, x as u8, nothing!());
    let _ = (x + 1 * 2, x == 1 && true, v[0], s.a, _, builtin # include_bytes);
    let _ = [1, 2];
    let _ = [0; 4];
    let _ = 0..x;
    let _ = ..;
    let _ = S { a: 1, ..s };
    let _ = S { a: 1, .. };
    let _ = builtin # offset_of(S, a);
    builtin # asm("");
    format_args!("hi");
    x += 1;
    x = 1;
    *r = 2;
    (x, _) = (1, 2);
    _ = x;
    let c = |a: i32| -> i32 { a };
    let _ = move || x;
    let _ = async || 1;
    let _ = async move { c.await };
    let _ = || { yield 1; };
    let _ = static || { yield; };
    let _ = unsafe { f(x) };
    let _ = s.m::<i32>(x);
    let _ = 'b: { break 'b 1 };
    'l: loop {
        if x > 0 { continue 'l; } else { break 'l; }
    }
    while x < 2 {}
    if let (.., y) = (x, x) {}
    match x {
        0 | 1 => {}
        C => {}
        -1 => {}
        2..=5 if true => {}
        ..=9 => {}
        ref mut z => {}
        w @ 10 => {}
        wild!() => {}
        const { 3 } => {}
        !null => {}
    }
    let S { a, .. } = s;
    let S { a: aa, b } = s;
    let T(.., t) = T(1, 2);
    let [first, .., last] = [1, 2, 3];
    let [head, rest @ ..] = [1, 2];
    let &p = &1;
    let &mut q = &mut 1;
    let box bx = b;
    let builtin # deref(d) = e;
    let Some(o) = opt else { return; };
    let typed: i32 = 1;
    super let kept = 1;
    struct Local;
    macro_rules! local { () => {} }
    do yeet x;
    become f(x);
    return x
}
"#;

#[test]
fn every_variant_pretty_prints() {
    pretty_print(
        EVERY_VARIANT,
        expect![[r#"
            fn main(x, s, v, r) {
                let _ = const { /* Idx::<Expr>(1) */ };
                let _ = ("s", "ab", "c\0", 'c', 97u8, true, 3, 1i64, 2u8, 16, 1.5, 2.5f32, 2f64, );
                let _ = (2, -x, !true, *r, &mut x, &raw x, x as u8, �, );
                let _ = (x + 1 * 2, x == 1 && true, v[0], s.a, _, include_bytes!(_), );
                let _ = [
                    1, 2,
                ];
                let _ = [
                    0; 4
                ];
                let _ = builtin#lang(Range){
                    start: 0,
                    end: x,
                };
                let _ = builtin#lang(RangeFull){
                };
                let _ = S{
                    a: 1,
                    ..s
                };
                let _ = S{
                    a: 1,
                    ..
                };
                let _ = builtin#offset_of(S, a);
                builtin#asm(_);
                builtin#lang(Arguments::from_str)(
                    "hi",
                );
                x += 1;
                x = 1;
                *r = 2;
                (x, _) = (1, 2, );
                _ = x;
                let c = |a: i32| -> i32 {
                    a
                };
                let _ = move || x;
                let _ = async || {
                    1
                };
                let _ = async move {
                    c.await
                };
                let _ = || {
                    yield 1;
                };
                let _ = static || {
                    yield;
                };
                let _ = unsafe {
                    f(
                        x,
                    )
                };
                let _ = s.m::<i32>(
                    x,
                );
                let _ = 'b: {
                    break 'b 1
                };
                'l: loop {
                    if x > 0 {
                        continue 'l;
                    } else {
                        break 'l;
                    }
                }
                loop if x < 2 {} else break
                if let (.., y) = (x, x, ) {}
                match x {
                    (0 | 1) => {},
                    C => {},
                    -1 => {},
                    2..=5 if true => {},
                    ..=9 => {},
                    ref mut z => {},
                    w @ 10 => {},
                    _ => {},
                    � => {},
                    !null => {},
                }
                let S {
                    a,
                    ..
                } = s;
                let S {
                    a: aa,
                    b,
                } = s;
                let T(, ..t) = T(
                    1,
                    2,
                );
                let [first, .., last, ] = [
                    1, 2, 3,
                ];
                let [head, rest @ .., ] = [
                    1, 2,
                ];
                let &p = &1;
                let &mut q = &mut 1;
                let box bx = b;
                let deref!(d) = e;
                let Some(o) = opt else {
                    return;
                };
                let typed: i32 = 1;
                let kept = 1;
                do yeet x;
                become f(
                    x,
                );
                return x
            }"#]],
    );
}

/// The place of an expression's variant in the declaration of [`Expr`]: a variant added there
/// stops this from compiling until [`EVERY_VARIANT`] has it too.
fn expr_variant(expr: Expr<'_>) -> usize {
    match expr {
        Expr::Missing => 0,
        Expr::Path(_) => 1,
        Expr::If { .. } => 2,
        Expr::Let { .. } => 3,
        Expr::Block { .. } => 4,
        Expr::Const(_) => 5,
        Expr::Loop { .. } => 6,
        Expr::Call { .. } => 7,
        Expr::MethodCall { .. } => 8,
        Expr::Match { .. } => 9,
        Expr::Continue { .. } => 10,
        Expr::Break { .. } => 11,
        Expr::Return { .. } => 12,
        Expr::Become { .. } => 13,
        Expr::Yield { .. } => 14,
        Expr::Yeet { .. } => 15,
        Expr::RecordLit { .. } => 16,
        Expr::Field { .. } => 17,
        Expr::Await { .. } => 18,
        Expr::Cast { .. } => 19,
        Expr::Ref { .. } => 20,
        Expr::UnaryOp { .. } => 21,
        Expr::BinaryOp { .. } => 22,
        Expr::Assignment { .. } => 23,
        Expr::Index { .. } => 24,
        Expr::Closure { .. } => 25,
        Expr::Tuple { .. } => 26,
        Expr::Array(_) => 27,
        Expr::Literal(_) => 28,
        Expr::Underscore => 29,
        Expr::OffsetOf(_) => 30,
        Expr::InlineAsm(_) => 31,
        Expr::IncludeBytes => 32,
    }
}
const EXPR_VARIANTS: usize = 33;

/// [`expr_variant`] for patterns.
fn pat_variant(pat: Pat<'_>) -> usize {
    match pat {
        Pat::Missing => 0,
        Pat::Rest => 1,
        Pat::Wild => 2,
        Pat::Tuple { .. } => 3,
        Pat::Or(_) => 4,
        Pat::Record { .. } => 5,
        Pat::Range { .. } => 6,
        Pat::Slice { .. } => 7,
        Pat::Path(_) => 8,
        Pat::Lit(_) => 9,
        Pat::Bind { .. } => 10,
        Pat::TupleStruct { .. } => 11,
        Pat::Ref { .. } => 12,
        Pat::Box { .. } => 13,
        Pat::Deref { .. } => 14,
        Pat::NotNull => 15,
        Pat::Expr(_) => 16,
    }
}
const PAT_VARIANTS: usize = 17;

/// [`expr_variant`] for statements, each optional field of a `let` and `super` apart, as each
/// is a word of its own in `extra` or a flag.
fn statement_variants(statement: Statement<'_>) -> impl Iterator<Item = usize> {
    let variants: [Option<usize>; 5] = match statement {
        Statement::Let { pat: _, type_ref, initializer, else_branch, is_super } => [
            Some(0),
            type_ref.map(|_| 4),
            initializer.map(|_| 5),
            else_branch.map(|_| 6),
            is_super.then_some(7),
        ],
        Statement::Expr { expr: _, has_semi } => [Some(1), has_semi.then_some(8), None, None, None],
        Statement::Item(Item::MacroDef(_)) => [Some(2), None, None, None, None],
        Statement::Item(Item::Other) => [Some(3), None, None, None, None],
    };
    variants.into_iter().flatten()
}
const STATEMENT_VARIANTS: usize = 9;

#[test]
fn every_variant_is_lowered() {
    let (db, def) = lower(EVERY_VARIANT);
    let body = Body::of(&db, def);
    let mut exprs = [false; EXPR_VARIANTS];
    body.exprs().for_each(|(_, expr)| exprs[expr_variant(expr)] = true);
    let missing: Vec<_> = (0..EXPR_VARIANTS).filter(|&it| !exprs[it]).collect();
    assert!(missing.is_empty(), "expression variants not lowered, by place in `Expr`: {missing:?}");
    let mut pats = [false; PAT_VARIANTS];
    body.pats().for_each(|(_, pat)| pats[pat_variant(pat)] = true);
    let missing: Vec<_> = (0..PAT_VARIANTS).filter(|&it| !pats[it]).collect();
    assert!(missing.is_empty(), "pattern variants not lowered, by place in `Pat`: {missing:?}");
    let mut statements = [false; STATEMENT_VARIANTS];
    let mut expression_without_semi = false;
    for (_, expr) in body.exprs() {
        let Expr::Block { statements: block, .. } = expr else { continue };
        for statement in block {
            statement_variants(statement).for_each(|it| statements[it] = true);
            expression_without_semi |= matches!(statement, Statement::Expr { has_semi: false, .. });
        }
    }
    let missing: Vec<_> = (0..STATEMENT_VARIANTS).filter(|&it| !statements[it]).collect();
    assert!(missing.is_empty(), "statement variants not lowered, by `statement_variants`: {missing:?}");
    assert!(expression_without_semi, "no expression statement without `;`");
}

/// `docs/v2/plan/design/data.md`: a HIR expression takes at most 12 bytes of its own columns,
/// with what it keeps in `extra` and the lists counted apart.
#[test]
fn every_variant_fits_the_column_budget() {
    let (db, def) = lower(EVERY_VARIANT);
    let body = Body::of(&db, def);
    let columns = body.hir_columns().expect("the body has expressions");
    let (exprs, pats) = (columns.expr_kind.len(), columns.pat_kind.len());
    assert_eq!(columns.expr_data.len(), exprs, "one data word per expression");
    assert_eq!(columns.pat_data.len(), pats, "one data word per pattern");
    assert_eq!(body.exprs().count(), exprs, "the view yields every expression");
    assert_eq!(body.pats().count(), pats, "the view yields every pattern");

    let expr_bytes = columns.expr_kind.len() + 4 * columns.expr_data.len();
    let pat_bytes = columns.pat_kind.len() + 4 * columns.pat_data.len();
    assert!(expr_bytes <= 12 * exprs, "{expr_bytes} bytes for {exprs} expressions");
    assert!(pat_bytes <= 12 * pats, "{pat_bytes} bytes for {pats} patterns");
    let statements = columns.stmt_kind.len();
    assert_eq!(columns.stmt_data.len(), statements, "one data word per statement");
    let statement_bytes = columns.stmt_kind.len() + 4 * columns.stmt_data.len();
    assert_eq!(statement_bytes, 5 * statements, "{statement_bytes} bytes for {statements} statements");

    let per = |n: usize, of: usize| n as f64 / of as f64;
    println!(
        "{exprs} expressions, {pats} patterns, {statements} statements: {:.2} bytes per \
         expression in `expr_kind` and `expr_data`, {:.2} per pattern, {:.2} per statement; \
         {:.2} words of `extra` per expression ({} words, shared with the patterns and the \
         `let`s); {:.2} `expr_lists` entries per expression",
        per(expr_bytes, exprs),
        per(pat_bytes, pats),
        per(statement_bytes, statements),
        per(columns.extra.len(), exprs),
        columns.extra.len(),
        per(columns.expr_lists.len(), exprs),
    );
}

#[test]
fn every_path_is_a_row_of_the_store_columns() {
    let (db, def) = lower(EVERY_VARIANT);
    let body = Body::of(&db, def);
    let (mut bare, mut lang, mut turbofish) = (0, 0, 0);
    for (_, expr) in body.exprs() {
        match expr {
            Expr::Path(path) | Expr::RecordLit { path, .. } => match path {
                Path::BarePath(_) => bare += 1,
                Path::LangItem(..) => lang += 1,
                Path::Normal(_) => {}
            },
            Expr::MethodCall { generic_args: Some(args), .. } => {
                assert!(matches!(args.args, [GenericArg::Type(_)]), "`m::<i32>`: {args:?}");
                turbofish += 1;
            }
            _ => {}
        }
    }
    let mut pat_paths = 0;
    for (_, pat) in body.pats() {
        if let Pat::Path(_) | Pat::Record { .. } | Pat::TupleStruct { .. } = pat {
            pat_paths += 1;
        }
    }
    assert!(bare > 0, "no bare path expression");
    assert!(lang > 0, "no lang item path (`0..x` and `..` desugar to them)");
    assert_eq!(turbofish, 1, "the one turbofish, `s.m::<i32>(x)`");
    let rows = body.types.paths.iter().len();
    assert!(rows >= bare + lang + pat_paths, "{rows} path rows");
}

#[test]
fn your_stack_belongs_to_me() {
    cov_mark::check!(your_stack_belongs_to_me);
    lower(
        r#"
#![recursion_limit = "32"]
macro_rules! n_nuple {
    ($e:tt) => ();
    ($($rest:tt)*) => {{
        (n_nuple!($($rest)*)None,)
    }};
}
fn main() { n_nuple!(1,2,3); }
"#,
    );
}

#[test]
fn your_stack_belongs_to_me2() {
    cov_mark::check!(overflow_but_not_me);
    lower(
        r#"
#![recursion_limit = "32"]
macro_rules! foo {
    () => {{ foo!(); foo!(); }}
}
fn main() { foo!(); }
"#,
    );
}

#[test]
fn recursion_limit() {
    lower(
        r#"
#![recursion_limit = "2"]
macro_rules! n_nuple {
    ($e:tt) => ();
    ($first:tt $($rest:tt)*) => {{
        n_nuple!($($rest)*)
    }};
}
fn main() { n_nuple!(1,2,3); }
"#,
    );
}

#[test]
fn issue_3642_bad_macro_stackover() {
    lower(
        r#"
#[macro_export]
macro_rules! match_ast {
    (match $node:ident { $($tt:tt)* }) => { match_ast!(match ($node) { $($tt)* }) };

    (match ($node:expr) {
        $( ast::$ast:ident($it:ident) => $res:expr, )*
        _ => $catch_all:expr $(,)?
    }) => {{
        $( if let Some($it) = ast::$ast::cast($node.clone()) { $res } else )*
        { $catch_all }
    }};
}

fn main() {
    let anchor = match_ast! {
        match parent {
            as => {},
            _ => return None
        }
    };
}"#,
    );
}

#[test]
fn macro_resolve() {
    // Regression test for a path resolution bug introduced with inner item handling.
    lower(
        r#"
macro_rules! vec {
    () => { () };
    ($elem:expr; $n:expr) => { () };
    ($($x:expr),+ $(,)?) => { () };
}
mod m {
    fn outer() {
        let _ = vec![FileSet::default(); self.len()];
    }
}
"#,
    );
}

#[test]
fn unresolved_macro_at_the_end_of_a_block_is_missing() {
    pretty_print(
        r#"
fn main() -> u8 {
    nothing_of_this_name!()
}
"#,
        expect![[r#"
            fn main() {
                �
            }"#]],
    );
}

#[test]
fn desugar_for_loop() {
    pretty_print(
        r#"
//- minicore: iterator, range
fn main() {
    for ident in 0..10 {
        foo();
        bar()
    }
}
"#,
        expect![[r#"
            fn main() {
                match builtin#lang(into_iter)(
                    builtin#lang(Range){
                        start: 0,
                        end: 10,
                    },
                ) {
                    mut <ra@gennew>0 => loop {
                        match builtin#lang(next)(
                            &mut <ra@gennew>0,
                        ) {
                            builtin#lang(None) => break,
                            builtin#lang(Some)(ident) => {
                                foo();
                                bar()
                            },
                        }
                    },
                }
            }"#]],
    );
}

#[test]
fn desugar_builtin_format_args() {
    pretty_print(
        r#"
//- minicore: fmt
fn main() {
    let are = "are";
    let count = 10;
    builtin#format_args("\u{1b}hello {count:02} {} friends, we {are:?} {0}{last}", "fancy", orphan = (), last = "!");
    builtin#format_args("hello world");
    builtin#format_args("hello world", orphan = ());
}
"#,
        expect![[r#"
        fn main() {
            let are = "are";
            let count = 10;
            {
                let <ra@gennew>0 = (&(), &count, &are, );
                let <ra@gennew>0 = [
                    builtin#lang(Argument::new_display)(
                        <ra@gennew>0.1,
                    ), builtin#lang(Argument::new_debug)(
                        <ra@gennew>0.2,
                    ),
                ];
                ();
                "fancy";
                "!";
                unsafe {
                    builtin#lang(Arguments::new)(
                        "\x07\x1bhello \xc3 \x00\x00i\x02\x00\x13 fancy friends, we \xc0\x07 fancy!\x00",
                        &<ra@gennew>0,
                    )
                }
            };
            builtin#lang(Arguments::from_str)(
                "hello world",
            );
            {
                ();
                builtin#lang(Arguments::from_str)(
                    "hello world",
                )
            };
        }"#]],
    )
}

#[test]
fn test_macro_hygiene() {
    pretty_print(
        r##"
//- minicore: fmt, from
//- /main.rs
mod error;

use crate::error::error;

fn main() {
    // _ = forces body expansion instead of block def map expansion
    _ = error!("Failed to resolve path `{}`", node.text());
}
//- /error.rs
macro_rules! _error {
    ($fmt:expr, $($arg:tt)+) => {$crate::error::intermediate!(format_args!($fmt, $($arg)+))}
}
pub(crate) use _error as error;
macro_rules! _intermediate {
    ($arg:expr) => {$crate::error::SsrError::new($arg)}
}
pub(crate) use _intermediate as intermediate;

pub struct SsrError(pub(crate) core::fmt::Arguments);

impl SsrError {
    pub(crate) fn new(message: impl Into<core::fmt::Arguments>) -> SsrError {
        SsrError(message.into())
    }
}
"##,
        expect![[r#"
        fn main() {
            _ = ra_test_fixture::error::SsrError::new(
                {
                    let <ra@gennew>0 = (&node.text(), );
                    let <ra@gennew>0 = [
                        builtin#lang(Argument::new_display)(
                            <ra@gennew>0.0,
                        ),
                    ];
                    unsafe {
                        builtin#lang(Arguments::new)(
                            "\x18Failed to resolve path `\xc0\x01`\x00",
                            &<ra@gennew>0,
                        )
                    }
                },
            );
        }"#]],
    )
}

#[test]
fn regression_10300() {
    pretty_print(
        r#"
//- minicore: concat, panic, fmt
mod private {
    pub use core::concat;
}

macro_rules! m {
    () => {
        panic!(concat!($crate::private::concat!("cc")));
    };
}

fn f(a: i32, b: u32) -> String {
    m!();
}
"#,
        expect![[r#"
            fn f(a, b) {
                {
                    core::panicking::panic_fmt(
                        builtin#lang(Arguments::from_str)(
                            "cc",
                        ),
                    );
                };
            }"#]],
    )
}

#[test]
fn destructuring_assignment_tuple_macro() {
    // This is a funny one. `let m!()() = Bar()` is an error in rustc, because `m!()()` isn't a valid pattern,
    // but in destructuring assignment it is valid, because `m!()()` is a valid expression, and destructuring
    // assignments start their lives as expressions. So we have to do the same.

    pretty_print(
        r#"
struct Bar();

macro_rules! m {
    () => { Bar };
}

fn foo() {
    m!()() = Bar();
}
"#,
        expect![[r#"
        fn foo() {
            Bar() = Bar();
        }"#]],
    )
}

#[test]
fn shadowing_record_variant() {
    let (db, def) = lower(
        r#"
enum A {
    B { field: i32 },
}
fn f() {
    use A::*;
    match () {
        B => {}
    };
}
    "#,
    );
    let body = Body::of(&db, def);
    assert_eq!(body.assert_expr_only().bindings.len(), 1, "should have a binding for `B`");
    assert_eq!(
        body[BindingId::from_raw(RawIdx::from_u32(0))].name.as_str(),
        "B",
        "should have a binding for `B`",
    );
}

#[test]
fn shadowing_tuple_struct_with_invisible_ctor() {
    let (db, def) = lower(
        r#"
mod x {
    pub struct CrateNum(u32);
}

use x::CrateNum;

pub struct CrateNumVal(CrateNum);

fn main() {
    let CrateNumVal(CrateNum) = loop {};
}
    "#,
    );
    let body = Body::of(&db, def);
    assert_eq!(body.assert_expr_only().bindings.len(), 1, "should have a binding for `CrateNum`");
    assert_eq!(
        body[BindingId::from_raw(RawIdx::from_u32(0))].name.as_str(),
        "CrateNum",
        "should have a binding for `CrateNum`",
    );
}

#[test]
fn regression_pretty_print_bind_pat() {
    pretty_print(
        r#"
fn foo() {
    let v @ u = 123;
}
"#,
        expect![[r#"
        fn foo() {
            let v @ u = 123;
        }"#]],
    );
}

#[test]
fn skip_skips_body() {
    pretty_print(
        r#"
#[rust_analyzer::skip]
async fn foo(a: (), b: i32) -> u32 {
    0 + 1 + b()
}
"#,
        expect!["fn foo(�, �) �"],
    );
}

#[test]
fn range_bounds_are_hir_exprs() {
    let (db, body) = lower(
        r#"
pub const L: i32 = 6;
mod x {
    pub const R: i32 = 100;
}
const fn f(x: i32) -> i32 {
    match x {
        -1..=5 => x * 10,
        L..=x::R => x * 100,
        _ => x,
    }
}"#,
    );

    let body = Body::of(&db, body);
    let mtch_arms = body
        .exprs()
        .find_map(|(_, expr)| {
            if let Expr::Match { arms, .. } = expr {
                return Some(arms);
            }

            None
        })
        .unwrap();

    let MatchArm { pat, .. } = mtch_arms[1];
    match body.pat(pat) {
        Pat::Range { start, end, range_type: _ } => {
            let hir_start = body.expr(start.unwrap());
            let hir_end = body.expr(end.unwrap());

            assert!(matches!(hir_start, Expr::Path { .. }));
            assert!(matches!(hir_end, Expr::Path { .. }));
        }
        _ => {}
    }
}

#[test]
fn three_dot_range_pattern_is_inclusive() {
    let (db, body) = lower(
        r#"
fn f(x: char) -> bool {
    match x {
        'a'...'z' => true,
        _ => false,
    }
}"#,
    );

    let body = Body::of(&db, body);
    let ranges = body.pats().filter(|(_, pat)| {
        matches!(
            pat,
            Pat::Range {
                start: Some(_),
                end: Some(_),
                range_type: syntax::ast::RangeOp::Inclusive
            }
        )
    });
    assert_eq!(ranges.count(), 1);
}

#[test]
fn print_hir_precedences() {
    pretty_print(
        r#"
fn main() {
    _ = &(1 - (2 - 3) + 4 * 5 * (6 + 7));
    _ = 1 + 2 < 3 && true && 4 < 5 && (a || b || c) || d && e;
    if let _ = 2 && true && let _ = 3 {}
    break a && b || (return) || (return 2);
    let r = &2;
    let _ = &mut (*r as i32)
}
"#,
        expect![[r#"
        fn main() {
            _ = &((1 - (2 - 3)) + (4 * 5) * (6 + 7));
            _ = 1 + 2 < 3 && true && 4 < 5 && (a || b || c) || d && e;
            if let _ = 2 && true && let _ = 3 {}
            break a && b || (return) || (return 2);
            let r = &2;
            let _ = &mut (*r as i32);
        }"#]],
    )
}

#[test]
fn async_fn_weird_param_patterns() {
    pretty_print(
        r#"
async fn main(&self, param1: i32, ref mut param2: i32, _: i32, param4 @ _: i32, 123: i32) {}
"#,
        expect![[r#"
            fn main(self, mut param1, mut param2, mut <ra@gennew>0, mut param4, mut <ra@gennew>1) async {
                let self = self;
                let param1 = param1;
                let mut param2 = param2;
                let ref mut param2 = param2;
                let mut <ra@gennew>0 = <ra@gennew>0;
                let _ = <ra@gennew>0;
                let mut param4 = param4;
                let param4 @ _ = param4;
                let mut <ra@gennew>1 = <ra@gennew>1;
                let 123 = <ra@gennew>1;
                {}
            }"#]],
    )
}

#[test]
fn array_element_cfg() {
    pretty_print(
        r#"
fn foo() {
    [
        (),
        #[cfg(false)]
        ()
    ];
}
    "#,
        expect![[r#"
        fn foo() {
            [
                (),
            ];
        }"#]],
    );
}

#[test]
fn block_tail_cfg() {
    pretty_print(
        r#"
macro_rules! foo {
    () => {
        1
    };
}

fn foo() -> i64 {
    #[cfg(true)]
    {
        5
    }
    #[cfg(false)]
    foo!()
}
    "#,
        expect![[r#"
            fn foo() {
                {
                    5
                }
            }"#]],
    );
    pretty_print(
        r#"
fn foo() -> i64 {
    #[cfg(true)]
    {
        5
    }
    #[cfg(false)]
    {
        4
    }
    #[cfg(false)]
    {
        3
    }
}
    "#,
        expect![[r#"
            fn foo() {
                {
                    5
                }
            }"#]],
    );
    pretty_print(
        r#"
macro_rules! m {
    () => {
        { 5 }
        #[cfg(false)]
        { 4 }
    };
}

fn foo() -> i64 {
    m!()
}
    "#,
        expect![[r#"
            fn foo() {
                {
                    5
                }
            }"#]],
    );
}

#[test]
fn weird_cfgs() {
    pretty_print(
        r#"
macro_rules! falsify {
    ( $($t:tt)* ) => { #[cfg(false)] $($t)* };
}

struct Foo();

fn foo() {
    foo(falsify!(1));
    (falsify!(1),);
    [falsify!(1)];
    Foo(falsify!(1));
    foo(#[cfg(false)] 1);
    (#[cfg(false)] 1,);
    [#[cfg(false)] 1];
    Foo(#[cfg(false)] 1);
    (|#[cfg(false)] a| {})();

    builtin # asm(
        "",
        #[cfg(false)] x = const 4,
        #[cfg(false)] options(),
        #[cfg(false)] clobber_abi("C"),
    );
}
    "#,
        expect![[r#"
            fn foo() {
                foo();
                ();
                [];
                Foo();
                foo();
                ();
                [];
                Foo();
                (|| {})();
                builtin#asm(_);
            }"#]],
    );
}

#[test]
fn ids_are_the_ids_of_the_decoded_nodes() {
    let (db, def) = lower(EVERY_VARIANT);
    let body = Body::of(&db, def);
    assert_eq!(body.expr_ids().len(), body.expr_count());
    assert!(body.expr_ids().eq(body.exprs().map(|(id, _)| id)));
    assert_eq!(body.pat_ids().len(), body.pat_count());
    assert!(body.pat_ids().eq(body.pats().map(|(id, _)| id)));
    assert_eq!(ExpressionStore::empty().expr_ids().len(), 0);
    assert_eq!(ExpressionStore::empty().pat_ids().len(), 0);
}
