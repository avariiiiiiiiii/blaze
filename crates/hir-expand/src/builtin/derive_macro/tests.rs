//! The derives written into one builder against [`super::reference`], the nested `quote!`s
//! they replace: over the same items, the same trees (encoding, spans, symbols), the same
//! `Debug` and `Display`, the same errors.

use span::{Edition, EditionedFileId, FileId, ROOT_ERASED_FILE_AST_ID, Span, SpanAnchor, SyntaxContext};
use syntax::{AstNode, SourceFile, TextRange, TextSize, ast};

use super::reference;
use crate::{ExpandResult, builtin::quote::dollar_crate, tt};

fn span_at(range: TextRange, ctx: SyntaxContext) -> Span {
    Span {
        range,
        anchor: SpanAnchor {
            file_id: EditionedFileId::new(FileId::from_raw(1), Edition::CURRENT),
            ast_id: ROOT_ERASED_FILE_AST_ID,
        },
        ctx,
    }
}

fn call_site() -> Span {
    span_at(TextRange::empty(TextSize::new(0)), SyntaxContext::root(Edition::Edition2015))
}

/// The first item of `text`, and a span map giving each token a span of its own, as the token
/// tree of a real item has: more than the sixteen spans the encoding keeps unsorted.
fn item(text: &str) -> (ast::Adt, span::SpanMap) {
    let file = SourceFile::parse(text, Edition::CURRENT).tree();
    let adt = file.syntax().descendants().find_map(ast::Adt::cast).expect("an item");
    let mut map = span::SpanMap::empty();
    for token in file.syntax().descendants_with_tokens().filter_map(|it| it.into_token()) {
        let range = token.text_range();
        map.push(range.end(), span_at(range, SyntaxContext::root(Edition::CURRENT)));
    }
    map.finish();
    (adt, map)
}

struct Input<'a>(&'a ast::Adt, &'a span::SpanMap);

impl reference::ItemSource for Input<'_> {
    fn adt(&self) -> (&ast::Adt, &span::SpanMap) {
        (self.0, self.1)
    }
}

const ITEMS: &[&str] = &[
    "struct Unit;",
    "struct Empty {}",
    "struct EmptyTuple();",
    "struct Tuple(u8, String);",
    "struct Record { a: u8, b: String, c: Vec<u8> }",
    "struct One { only: u8 }",
    "struct Generic<'a, 'b: 'a, T: Clone + 'a, U, const N: usize> where U: Default, T::Assoc: Copy {
        a: &'a T, b: T::Assoc, c: [U; N], d: <T as Tr>::X, e: PhantomData<&'b ()>, f: Option<U::Out>,
    }",
    "struct Bounds<T: ?Sized + Send, F: Fn(T) -> T>(Box<T>, F) where for<'x> &'x T: Debug;",
    "#[repr(packed)] struct Packed { a: u8, b: u32 }",
    "#[repr(C, packed(2))] struct PackedTuple<T: Tr>(T, T::Item);",
    "#[repr(packed)] struct PackedUnit;",
    "#[repr(r#packed)] struct PackedRaw(u8);",
    "#[derive(Copy)] #[repr(C)] struct NotPacked(u8);",
    "struct r#Raw { r#type: u8, r#match: u16 }",
    "struct Unicode { café: u8, naïve: u16 }",
    "struct Wide { f0: u8, f1: u8, f2: u8, f3: u8, f4: u8, f5: u8, f6: u8, f7: u8, f8: u8, f9: u8,
        f10: u8, f11: u8, f12: u8, f13: u8, f14: u8, f15: u8, f16: u8, f17: u8, f18: u8 }",
    "struct WideTuple(u8, u8, u8, u8, u8, u8, u8, u8, u8, u8, u8, u8, u8);",
    "enum Never {}",
    "enum One { A }",
    "enum Fielded { A(u8) }",
    "enum Many { A, B, C }",
    "enum Mixed { A, B(u8), C { x: u8, y: String }, D(), E {} }",
    "enum Fields1 { A(u8), B }",
    "enum Defaulted<T> where T: Clone { A, #[default] B(T, T::Out), C { t: T } }",
    "enum DefaultedStruct { #[default] A { a: u8, b: u8 }, B }",
    "enum Discriminants { A = 1, B = 2 }",
    "union U { a: u8, b: u16 }",
    "union GenericUnion<T: Copy> { a: T, b: u16 }",
];

type Derive = fn(&Input<'_>, Span) -> [ExpandResult<tt::TopSubtree>; 2];

fn derives() -> [(&'static str, Derive); 12] {
    macro_rules! pair {
        ($old:ident, $new:ident $(, $arg:expr)?) => {
            |input: &Input<'_>, span: Span| {
                let dummy = tt::TopSubtree::empty(tt::DelimSpan::from_single(span));
                [
                    reference::$old(input, span, &dummy $(, $arg)?),
                    super::$new(span, super::parse_adt_from_syntax(input.0, input.1, span) $(, $arg)?),
                ]
            }
        };
    }
    [
        ("Copy", pair!(copy_expand, copy_derive)),
        ("Clone", pair!(clone_expand_with, clone_derive, false)),
        ("Clone with Copy", pair!(clone_expand_with, clone_derive, true)),
        ("Default", pair!(default_expand, default_derive)),
        ("Debug", pair!(debug_expand, debug_derive)),
        ("Hash", pair!(hash_expand, hash_derive)),
        ("Ord", pair!(ord_expand, ord_derive)),
        ("PartialOrd", pair!(partial_ord_expand, partial_ord_derive)),
        ("Eq", pair!(eq_expand, eq_derive)),
        ("PartialEq", pair!(partial_eq_expand, partial_eq_derive)),
        ("ConstParamTy", pair!(const_param_ty_expand, const_param_ty_derive)),
        ("Reborrow", pair!(reborrow_expand, reborrow_derive)),
    ]
}

fn assert_same(what: &str, old: &ExpandResult<tt::TopSubtree>, new: &ExpandResult<tt::TopSubtree>) {
    assert_eq!(format!("{:#?}", old.value), format!("{:#?}", new.value), "{what}");
    assert_eq!(old.value.to_string(), new.value.to_string(), "{what}");
    assert!(old.value == new.value, "{what}: the encodings differ");
    assert_eq!(old.err, new.err, "{what}");
}

#[test]
fn derives_match_nested_quotes() {
    let span = call_site();
    for text in ITEMS {
        let (adt, map) = item(text);
        let input = Input(&adt, &map);
        for (name, derive) in derives() {
            let [old, new] = derive(&input, span);
            assert_same(&format!("{name} of `{text}`"), &old, &new);
        }
    }
}

#[test]
fn from_matches_nested_quotes() {
    let span = call_site();
    for text in [
        "struct W(u8);",
        "struct R { inner: Vec<u8> }",
        "struct G<'a, T: Clone>(&'a T) where T: Send;",
        "struct Two(u8, u8);",
        "struct None;",
        "enum E { A(u8) }",
    ] {
        let (adt, map) = item(text);
        let new = super::from_derive(span, &adt, &map);
        let old = reference_from(span, &adt, &map);
        assert_same(&format!("From of `{text}`"), &old, &new);
    }
}

/// rustc's `From` derive as the old code wrote it, for a struct with one field.
fn reference_from(span: Span, adt: &ast::Adt, map: &span::SpanMap) -> ExpandResult<tt::TopSubtree> {
    let ast::Adt::Struct(strukt) = adt else { return super::from_derive(span, adt, map) };
    let ty = match strukt.field_list() {
        Some(ast::FieldList::RecordFieldList(it)) if it.fields().count() == 1 => {
            it.fields().next().and_then(|it| it.ty())
        }
        Some(ast::FieldList::TupleFieldList(it)) if it.fields().count() == 1 => {
            it.fields().next().and_then(|it| it.ty())
        }
        _ => return super::from_derive(span, adt, map),
    };
    let field_ty = syntax_bridge::syntax_node_to_token_tree(
        ty.expect("a type").syntax(),
        map,
        span,
        syntax_bridge::DocCommentDesugarMode::ProcMacro,
    );
    let info = reference::parse_adt_from_syntax(adt, map, span).expect("an item");
    let krate = dollar_crate(span);
    let value = || tt::Ident::new("value", span);
    let record_field = match strukt.field_list() {
        Some(ast::FieldList::RecordFieldList(_)) => {
            Some(reference::name_to_token(span, map, strukt.field_list().and_then(|it| match it {
                ast::FieldList::RecordFieldList(it) => it.fields().next().and_then(|it| it.name()),
                ast::FieldList::TupleFieldList(_) => None,
            })).expect("a name"))
        }
        _ => None,
    };
    let body = match record_field {
        Some(field) => {
            let field = tt::Ident { span: tt::Span { ctx: span.ctx, ..field.span }, ..field };
            let value = value();
            quote! {span => Self { #field : #value } }
        }
        None => {
            let value = value();
            quote! {span => Self ( #value ) }
        }
    };
    let trait_path = {
        let field_ty = field_ty.clone();
        quote! {span => #krate::convert::From<#field_ty> }
    };
    ExpandResult::ok(reference::expand_simple_derive_with_parsed(
        span,
        info,
        trait_path,
        |_| {
            let value = value();
            quote! {span =>
                #[inline]
                fn from(#value : #field_ty) -> Self { #body }
            }
        },
        false,
        tt::TopSubtree::empty(tt::DelimSpan::from_single(span)),
    ))
}

#[test]
fn coerce_pointee_impls_match_nested_quotes() {
    let span = call_site();
    let text = "#[repr(transparent)] struct Ptr<'a, T: ?Sized, const N: usize>(&'a T) where T: Tr;";
    let (adt, map) = item(text);
    let self_for_traits = quote! {span => Ptr<'a, __S, N> };
    let new = super::write_coerce_pointee_impls(
        span,
        &super::parse_adt_from_syntax(&adt, &map, span).expect("an item"),
        &self_for_traits,
    );
    let info = reference::parse_adt_from_syntax(&adt, &map, span).expect("an item");
    let krate = dollar_crate(span);
    let impl_of = |trait_path: tt::TopSubtree| {
        reference::expand_simple_derive_with_parsed(
            span,
            info.clone(),
            trait_path,
            |_| quote! {span => },
            false,
            quote! {span => __S },
        )
    };
    let dispatch_from_dyn =
        impl_of(quote! {span => #krate::ops::DispatchFromDyn<#self_for_traits> });
    let coerce_unsized = impl_of(quote! {span => #krate::ops::CoerceUnsized<#self_for_traits> });
    let old = quote! {span => #dispatch_from_dyn #coerce_unsized };
    assert_same("CoercePointee", &ExpandResult::ok(old), &ExpandResult::ok(new));
}

/// The variants' field ranges tile the one field list in order, and each binding is the
/// `format!` of its field's name the old code interned, at the field's span.
#[test]
fn shape_is_one_field_list() {
    let span = call_site();
    for text in ITEMS {
        let (adt, map) = item(text);
        let new = super::parse_adt_from_syntax(&adt, &map, span).expect("an item");
        let mut at = 0;
        for v in &new.shape.variants {
            assert_eq!(v.start as usize, at, "{text}");
            at = v.end as usize;
        }
        assert_eq!(at, new.shape.fields.len(), "{text}");
        for (field, bound) in new.shape.fields.iter().zip(new.bindings("_other")) {
            assert_eq!(bound, tt::Ident::new(&format!("{}_other", field.sym), field.span), "{text}");
        }
    }
}

#[test]
fn derives_copy_reads_words_as_the_text_did() {
    for (text, copy) in [
        ("#[derive(Copy)] struct A;", true),
        ("#[derive(Clone, Copy)] struct A;", true),
        ("#[derive(core::marker::Copy)] struct A;", true),
        ("#[derive(Clone)] struct A;", false),
        ("#[doc = \"derive Copy\"] struct A;", false),
        ("#[derive(CopyNot)] struct A;", false),
        ("#[derive(Clone /* Copy */)] struct A;", true),
    ] {
        let (adt, _) = item(text);
        assert_eq!(super::derives_copy(&adt), copy, "{text}");
    }
}

#[test]
fn joined_names() {
    assert_eq!(super::tuple_field_symbol(0), intern::Symbol::intern("f0"));
    assert_eq!(super::tuple_field_symbol(12345), intern::Symbol::intern("f12345"));
    assert_eq!(super::tuple_field_symbol(usize::MAX), intern::Symbol::intern(&format!("f{}", usize::MAX)));
    let long = "x".repeat(100);
    assert_eq!(super::joined(&long, "_self"), intern::Symbol::intern(&format!("{long}_self")));
}

#[test]
fn builtin_derives_found_as_by_compare() {
    use intern::{Symbol, sym};

    use super::BuiltinDeriveExpander;
    use crate::name::Name;

    let mut symbols: Vec<Symbol> = BuiltinDeriveExpander::NAMES.to_vec();
    let texts: Vec<String> = symbols.iter().map(|it| it.as_str().to_owned()).collect();
    for text in &texts {
        symbols.push(Symbol::intern(text));
        symbols.push(Symbol::intern(&format!("{text}_")));
        symbols.push(Symbol::intern(&text.to_lowercase()));
        symbols.push(Symbol::intern(&text[..text.len() - 1]));
    }
    symbols.extend([sym::self_, sym::derive, sym::copy, sym::sized, Symbol::intern("Serialize")]);
    for symbol in symbols {
        let name = Name::new_symbol_root(symbol);
        assert_eq!(
            BuiltinDeriveExpander::find_by_name(&name),
            BuiltinDeriveExpander::find_by_name_by_compare(&name),
            "{name:?}"
        );
    }
    for (at, symbol) in BuiltinDeriveExpander::NAMES.iter().enumerate() {
        let name = Name::new_symbol_root(symbol.clone());
        assert_eq!(BuiltinDeriveExpander::find_by_name(&name), Some(BuiltinDeriveExpander::ALL[at]));
    }
}

/// A struct of `fields` fields with a field of each kind of type the derives read.
fn wide_struct(fields: usize) -> String {
    let fields: Vec<String> = (0..fields).map(|i| format!("f{i}: u{}", 8 << (i % 4))).collect();
    format!("struct Wide {{ {} }}", fields.join(", "))
}

/// `#[derive(Clone)]` of a struct, from its syntax to its expansion (the reparse of the input
/// tree before it is the bridge's): no allocation per field. What grows with the field count is
/// the output builder's columns doubling, about 4 columns × log2 of the growth, so going from 16
/// fields to 256 adds at most 48 allocations where one per field would add 240. Measured warm:
/// the field bindings' symbols are interned by the first run.
#[test]
fn a_derive_allocates_nothing_per_field() {
    let used = |fields: usize| {
        let text = wide_struct(fields);
        let (adt, map) = item(&text);
        let span = call_site();
        let run = || super::clone_derive(span, super::parse_adt_from_syntax(&adt, &map, span), false);
        let warm = run();
        let (again, used) = blaze_alloc_count::measure(run);
        assert!(again.value == warm.value && again.err.is_none(), "{fields} fields");
        used.allocations
    };
    let (small, big) = (used(16), used(256));
    assert!(small <= 64, "`Clone` of 16 fields: {small} allocations, budget 64");
    assert!(big <= small + 48, "`Clone` of 256 fields: {big} allocations, {small} for 16");
}
