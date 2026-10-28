use proc_macro::{TokenStream, TokenTree};
use std::cell::RefCell;

thread_local! {
    static REGISTERED: RefCell<Option<(u32, String)>> = const { RefCell::new(None) };
}

#[proc_macro_attribute]
pub fn register(attr: TokenStream, item: TokenStream) -> TokenStream {
    let value = attr.to_string().parse().unwrap();
    REGISTERED.with(|held| {
        assert!(held.borrow().is_none(), "registration leaked from another crate or revision");
        *held.borrow_mut() = Some((value, item.to_string()));
    });
    item
}

#[proc_macro_attribute]
pub fn expose(attr: TokenStream, mut item: TokenStream) -> TokenStream {
    assert_ne!(attr.to_string(), "panic", "requested macro failure");
    let (value, saved) =
        REGISTERED.with(|held| held.borrow().clone().expect("class was not registered"));
    let name = saved.split_whitespace().nth(1).unwrap().trim_end_matches(';');
    let parsed: TokenStream = "probe".parse().unwrap();
    let parsed_span = parsed.into_iter().next().unwrap().span();
    let literal_span = "1".parse::<proc_macro::Literal>().unwrap().span();
    let call_site = proc_macro::Span::call_site();
    for span in [parsed_span, literal_span] {
        assert_eq!(
            (span.line(), span.column()),
            (call_site.line(), call_site.column()),
            "parsed tokens use this expansion's call site"
        );
    }
    let extra: TokenStream =
        format!("impl SAVED {{ pub fn value() -> u32 {{ {value} }} }}").parse().unwrap();
    item.extend(extra.into_iter().map(|token| match token {
        TokenTree::Ident(ident) if ident.to_string() == "SAVED" => {
            TokenTree::Ident(proc_macro::Ident::new(name, proc_macro::Span::call_site()))
        }
        other => other,
    }));
    item
}
