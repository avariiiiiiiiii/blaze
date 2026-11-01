//! An impl whose header leaves a lifetime out holds for every lifetime there, as rustc's
//! resolver makes each a fresh lifetime parameter of the impl: core's `impl_fn_for_zst!` writes
//! `impl Fn<(&u8,)> for IsAsciiWhitespace`, which `slice::Split`'s `P: FnMut(&T) -> bool` must
//! meet for `str::split_ascii_whitespace` and `SplitAsciiWhitespace`'s `next` and `size_hint`
//! to resolve their calls (the 00:03 self-build: cc's build-script dependencies refused their
//! MIR, `UnresolvedMethod`). Queue 01225, whose hir-def half is 01358. On a failure the message
//! has each body's diagnostics and each method call with no resolution.

use hir_def::{
    AssocItemId, DefWithBodyId, ModuleDefId, expr_store::Body, hir::Expr, nameres::crate_def_map,
    signatures::FunctionSignature,
};
use hir_expand::fixture;

use crate::{InferBodyId, InferenceResult};

const FIXTURE: &str = r#"
//- minicore: sized, fn, option, copy, slice
#![feature(unboxed_closures, fn_traits)]
pub trait Iterator {
    type Item;
    fn next(&mut self) -> Option<Self::Item>;
    fn size_hint(&self) -> (usize, Option<usize>) {
        (0, None)
    }
    fn filter<P>(self, predicate: P) -> Filter<Self, P>
    where
        Self: Sized,
        P: FnMut(&Self::Item) -> bool,
    {
        Filter { iter: self, predicate }
    }
    fn map<B, F>(self, f: F) -> Map<Self, F>
    where
        Self: Sized,
        F: FnMut(Self::Item) -> B,
    {
        Map { iter: self, f }
    }
}
pub struct Split<'a, T, P> {
    v: &'a [T],
    pred: P,
}
impl<'a, T, P> Iterator for Split<'a, T, P>
where
    P: FnMut(&T) -> bool,
{
    type Item = &'a [T];
    fn next(&mut self) -> Option<&'a [T]> {
        None
    }
}
pub struct Filter<I, P> {
    iter: I,
    predicate: P,
}
impl<I: Iterator, P> Iterator for Filter<I, P>
where
    P: FnMut(&I::Item) -> bool,
{
    type Item = I::Item;
    fn next(&mut self) -> Option<I::Item> {
        None
    }
}
pub struct Map<I, F> {
    iter: I,
    f: F,
}
impl<B, I: Iterator, F> Iterator for Map<I, F>
where
    F: FnMut(I::Item) -> B,
{
    type Item = B;
    fn next(&mut self) -> Option<B> {
        None
    }
}

macro_rules! impl_fn_for_zst {
    ($(
        $vis:vis struct $Name: ident impl$( <$( $lifetime : lifetime ),+> )? Fn =
            |$( $arg: ident: $ArgTy: ty ),*| -> $ReturnTy: ty
            $body: block;
    )+) => {
        $(
            $vis struct $Name;

            impl $( <$( $lifetime ),+> )? Fn<($( $ArgTy, )*)> for $Name {
                extern "rust-call" fn call(&self, ($( $arg, )*): ($( $ArgTy, )*)) -> $ReturnTy {
                    $body
                }
            }

            impl $( <$( $lifetime ),+> )? FnMut<($( $ArgTy, )*)> for $Name {
                extern "rust-call" fn call_mut(
                    &mut self,
                    ($( $arg, )*): ($( $ArgTy, )*)
                ) -> $ReturnTy {
                    Fn::call(&*self, ($( $arg, )*))
                }
            }

            impl $( <$( $lifetime ),+> )? FnOnce<($( $ArgTy, )*)> for $Name {
                type Output = $ReturnTy;

                extern "rust-call" fn call_once(self, ($( $arg, )*): ($( $ArgTy, )*)) -> $ReturnTy {
                    Fn::call(&self, ($( $arg, )*))
                }
            }
        )+
    }
}
impl_fn_for_zst! {
    pub struct IsAsciiWhitespace impl Fn = |byte: &u8| -> bool {
        *byte == 32
    };

    pub struct BytesIsNotEmpty impl<'a, 'b> Fn = |s: &'a &'b [u8]| -> bool {
        true
    };

    pub struct UnsafeBytesToStr impl<'a> Fn = |bytes: &'a [u8]| -> &'a [u8] {
        bytes
    };
}

pub struct SplitAsciiWhitespace<'a> {
    inner: Map<Filter<Split<'a, u8, IsAsciiWhitespace>, BytesIsNotEmpty>, UnsafeBytesToStr>,
}
fn split(v: &[u8]) -> Split<'_, u8, IsAsciiWhitespace> {
    Split { v, pred: IsAsciiWhitespace }
}
fn split_ascii_whitespace(v: &[u8]) -> SplitAsciiWhitespace<'_> {
    let inner = split(v).filter(BytesIsNotEmpty).map(UnsafeBytesToStr);
    SplitAsciiWhitespace { inner }
}
impl<'a> Iterator for SplitAsciiWhitespace<'a> {
    type Item = &'a [u8];
    fn next(&mut self) -> Option<&'a [u8]> {
        self.inner.next()
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.inner.size_hint()
    }
}

pub trait Pred<A> {}
impl Pred<&u8> for IsAsciiWhitespace {}
fn need<P: for<'r> Pred<&'r u8>>(_: P) {}
fn plain() {
    need(IsAsciiWhitespace)
}
"#;

#[test]
fn an_elided_impl_header_lifetime_holds_for_every_lifetime() {
    let db = fixture::db(FIXTURE).into_db();
    crate::next_solver::attach_db(&db, || {
        let mut funcs = Vec::new();
        for (_, module) in crate_def_map(&db, fixture::test_crate(&db)).modules(&db) {
            let scope = module.scope(&db);
            funcs.extend(scope.declarations().filter_map(|def| match def {
                ModuleDefId::FunctionId(it) => Some(it),
                _ => None,
            }));
            for imp in scope.impls() {
                funcs.extend(imp.impl_items(&db).items.iter().filter_map(|&(_, item)| match item {
                    AssocItemId::FunctionId(it) => Some(it),
                    _ => None,
                }));
            }
        }
        let mut failed = String::new();
        let mut checked = Vec::new();
        for func in funcs {
            let name = FunctionSignature::of(&db, func).name.as_str().to_owned();
            let def = DefWithBodyId::FunctionId(func);
            let body = Body::of(&db, def);
            let infer = InferenceResult::of(&db, InferBodyId::DefWithBodyId(def));
            if !infer.diagnostics().is_empty() {
                failed.push_str(&format!("`{name}`: diagnostics {:?}\n", infer.diagnostics()));
            }
            for (id, expr) in body.exprs() {
                let Expr::MethodCall { receiver, method_name, .. } = expr else { continue };
                if infer.method_resolution(id).is_some() {
                    continue;
                }
                failed.push_str(&format!(
                    "`{name}`: `.{}` unresolved on {:?}\n",
                    method_name.as_str(),
                    infer.expr_ty(receiver),
                ));
            }
            checked.push(name);
        }
        for name in ["split_ascii_whitespace", "next", "size_hint", "plain"] {
            assert!(checked.iter().any(|it| it == name), "`{name}` was not checked: {checked:?}");
        }
        assert!(failed.is_empty(), "{failed}");
    });
}
