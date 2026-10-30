//! proc-macro server backend based on [`proc_macro_api::msg::SpanId`] as the backing span.
//! This backend is rather inflexible, used by RustRover and older rust-analyzer versions.
use std::ops::{Bound, Range};

use intern::Symbol;
use rustc_proc_macro::bridge::server;

use crate::{
    ProcMacroClientHandle,
    bridge::{Diagnostic, ExpnGlobals, Literal, TokenTree},
    server_impl::literal_from_str,
};

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct SpanId(pub u32);

impl std::fmt::Debug for SpanId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

type Span = SpanId;

pub struct SpanIdServer<'a> {
    pub call_site: Span,
    pub def_site: Span,
    pub mixed_site: Span,
    pub callback: Option<ProcMacroClientHandle<'a>>,
}

impl server::Server for SpanIdServer<'_> {
    type TokenStream = crate::token_stream::TokenStream<Span>;
    type Span = Span;
    type Symbol = Symbol;

    fn globals(&mut self) -> ExpnGlobals<Self::Span> {
        ExpnGlobals {
            def_site: self.def_site,
            call_site: self.call_site,
            mixed_site: self.mixed_site,
        }
    }

    fn intern_symbol(ident: &str) -> Self::Symbol {
        Symbol::intern(ident)
    }

    fn with_symbol_string(symbol: &Self::Symbol, f: impl FnOnce(&str)) {
        f(symbol.as_str())
    }

    fn track_env_var(&mut self, _: &str, _: Option<&str>) {}

    fn track_path(&mut self, _: &str) {}

    fn literal_from_str(&mut self, s: &str) -> Result<Literal<Self::Span>, String> {
        literal_from_str(s, self.call_site)
            // alloc: error path, the message the macro is given.
            .map_err(|()| "cannot parse string into literal".to_string())
    }

    fn emit_diagnostic(&mut self, _: Diagnostic<Self::Span>) {}

    fn ts_drop(&mut self, stream: Self::TokenStream) {
        drop(stream);
    }

    fn ts_clone(&mut self, stream: &Self::TokenStream) -> Self::TokenStream {
        // clone: the stream's `Arc`, which the bridge asks a second handle of.
        stream.clone()
    }

    fn ts_is_empty(&mut self, stream: &Self::TokenStream) -> bool {
        stream.is_empty()
    }
    fn ts_from_str(&mut self, src: &str) -> Result<Self::TokenStream, String> {
        Self::TokenStream::from_str(src, self.call_site)
            // alloc: error path, the message the macro is given.
            .map_err(|e| format!("failed to parse str to token stream: {e}"))
    }
    fn ts_to_string(&mut self, stream: &Self::TokenStream) -> String {
        // alloc: the text the macro asks for, which it owns.
        stream.to_string()
    }
    fn ts_from_token_tree(&mut self, tree: TokenTree<Self::Span>) -> Self::TokenStream {
        Self::TokenStream::of_tree(tree)
    }

    fn ts_expand_expr(&mut self, self_: &Self::TokenStream) -> Result<Self::TokenStream, ()> {
        // clone: the stream's `Arc`, given back unexpanded.
        Ok(self_.clone())
    }

    fn ts_concat_trees(
        &mut self,
        base: Option<Self::TokenStream>,
        trees: Vec<TokenTree<Self::Span>>,
    ) -> Self::TokenStream {
        Self::TokenStream::concat_trees(base, trees)
    }

    fn ts_concat_streams(
        &mut self,
        base: Option<Self::TokenStream>,
        streams: Vec<Self::TokenStream>,
    ) -> Self::TokenStream {
        Self::TokenStream::concat_streams(base, streams)
    }

    fn ts_into_trees(&mut self, stream: Self::TokenStream) -> Vec<TokenTree<Self::Span>> {
        stream.into_trees()
    }

    fn span_debug(&mut self, span: Self::Span) -> String {
        // alloc: the text the macro asks for, which it owns.
        format!("{:?}", span.0)
    }
    fn span_file(&mut self, _span: Self::Span) -> String {
        // alloc: none, an empty `String` the bridge's signature asks for.
        String::new()
    }
    fn span_local_file(&mut self, _span: Self::Span) -> Option<String> {
        None
    }
    fn span_save_span(&mut self, _span: Self::Span) -> usize {
        0
    }
    fn span_recover_proc_macro_span(&mut self, _id: usize) -> Self::Span {
        self.call_site
    }
    /// Recent feature, not yet in the proc_macro
    ///
    /// See PR:
    /// https://github.com/rust-lang/rust/pull/55780
    fn span_source_text(&mut self, _span: Self::Span) -> Option<String> {
        None
    }

    fn span_parent(&mut self, _span: Self::Span) -> Option<Self::Span> {
        None
    }
    fn span_source(&mut self, span: Self::Span) -> Self::Span {
        span
    }
    fn span_byte_range(&mut self, _span: Self::Span) -> Range<usize> {
        Range { start: 0, end: 0 }
    }
    fn span_join(&mut self, first: Self::Span, _second: Self::Span) -> Option<Self::Span> {
        // Just return the first span again, because some macros will unwrap the result.
        Some(first)
    }
    fn span_subspan(
        &mut self,
        span: Self::Span,
        _start: Bound<usize>,
        _end: Bound<usize>,
    ) -> Option<Self::Span> {
        // Just return the span again, because some macros will unwrap the result.
        Some(span)
    }
    fn span_resolved_at(&mut self, _span: Self::Span, _at: Self::Span) -> Self::Span {
        self.call_site
    }

    fn span_end(&mut self, _self_: Self::Span) -> Self::Span {
        self.call_site
    }

    fn span_start(&mut self, _self_: Self::Span) -> Self::Span {
        self.call_site
    }

    fn span_line(&mut self, _span: Self::Span) -> usize {
        1
    }

    fn span_column(&mut self, _span: Self::Span) -> usize {
        1
    }

    fn symbol_normalize_and_validate_ident(&mut self, string: &str) -> Result<Self::Symbol, ()> {
        // FIXME: nfc-normalize and validate idents
        Ok(<Self as server::Server>::intern_symbol(string))
    }
}
