//! blaze's in-process proc-macro bridge: the `proc_macro` a macro's own code runs against
//! (`proc_macro/`, blaze's WebAssembly guest's with its imports answered in process) and the
//! host that answers it ([`host`]), for the proc macros that run on blaze's own compiler
//! (docs/v2/plan/design/compile-time.md, change 20). A macro sees what it sees as a module:
//! the same spans, numbered the same (the def, call and mixed sites 1, 2 and 3, then the
//! input's as they first appear), the same answers to what it asks of them, the same text of
//! a stream, so its output is the module's token for token and span for span. [`difference`]
//! names the first token where two outputs part.
//!
//! What it costs: an expansion allocates only what its output keeps. Its spans' table and its
//! encodings are each thread's, reused, and the output is read straight into rust-analyzer's
//! flat tree ([`wire::Reader`]).

mod host;
#[allow(dead_code)]
mod lex;
#[allow(dead_code)]
mod wire;

#[cfg(test)]
mod tests;

use std::cell::Cell;

use base_db::SourceDatabase;
use hir_expand::proc_macro::ProcMacroExpansionError;
pub use native_proc_macro::TokenStream;
use span::Span;
use tt::{Leaf, TokenTree, TopSubtree};

use self::{
    host::Host,
    wire::{Spans, Writer},
};

/// A macro's entry point, compiled against the bridge's `proc_macro`.
#[derive(Clone, Copy, Debug)]
pub enum Expander {
    /// A derive's or a function-like macro's: the item or the call's tokens.
    One(fn(TokenStream) -> TokenStream),
    /// An attribute macro's: the attribute's tokens, then the item's.
    Two(fn(TokenStream, TokenStream) -> TokenStream),
}

/// What an expansion on a thread writes and reads, kept for the next one there.
#[derive(Default)]
struct Scratch {
    spans: Spans,
    input: Vec<u8>,
    attr: Vec<u8>,
    output: Vec<u8>,
}

thread_local! {
    /// Taken for the length of an expansion: one that a database answer starts on this
    /// thread (`host.rs`) finds it empty and makes its own.
    static SCRATCH: Cell<Scratch> = Cell::new(Scratch::default());
}

/// Expands a call of the macro whose entry point is `expander`, as its module would. With no
/// `db`, what the macro asks of a span is answered as the wasm host answers with none.
pub fn expand(
    expander: Expander,
    db: Option<&dyn SourceDatabase>,
    subtree: &TopSubtree,
    attrs: Option<&TopSubtree>,
    def_site: Span,
    call_site: Span,
    mixed_site: Span,
) -> Result<TopSubtree, ProcMacroExpansionError> {
    let mut scratch = SCRATCH.take();
    scratch.spans.clear();
    scratch.output.clear();
    // As the wasm host numbers them: the sites 1, 2 and 3, the same spans or not, then
    // the input's as they first appear.
    for span in [def_site, call_site, mixed_site] {
        scratch.spans.push(span);
    }
    {
        let Scratch { spans, input, attr, .. } = &mut scratch;
        input.clear();
        attr.clear();
        let mut writer = Writer { out: std::mem::take(input), spans };
        writer.top(subtree);
        *input = std::mem::take(&mut writer.out);
        match attrs {
            Some(attrs) => {
                writer.out = std::mem::take(attr);
                writer.top(attrs);
                *attr = std::mem::take(&mut writer.out);
            }
            // rust-analyzer gives an attribute written without arguments
            // (`#[tokio::main]`) none, which the macro is given as an empty stream.
            None => attr.extend_from_slice(&0u32.to_le_bytes()),
        }
    }
    let mut host = Host { db, spans: std::mem::take(&mut scratch.spans), sites: [1, 2, 3] };
    let (input, attr, output) = (&scratch.input, &scratch.attr, &mut scratch.output);
    let ran = native_proc_macro::__native::with_host(&mut host, || match expander {
        Expander::One(f) => native_proc_macro::__native::expand1(f, input, output),
        Expander::Two(f) => native_proc_macro::__native::expand2(f, attr, input, output),
    });
    let read = ran.map(|()| wire::Reader::new(&scratch.output, &host.spans).top(call_site));
    scratch.spans = host.spans;
    SCRATCH.set(scratch);
    match read {
        Ok(Ok(tree)) => Ok(tree),
        Ok(Err(e)) => Err(ProcMacroExpansionError::System(e)),
        Err(panic) => Err(ProcMacroExpansionError::Panic(panic)),
    }
}

/// The first difference of an output of the bridge (`native`) from what the macro gives
/// elsewhere (`normal`: its wasm module, or rustc), or `None` when they are the same: every token tree, each delimiter, character, spacing, text, literal kind and
/// suffix, and of every span its range, anchor and syntax context.
pub fn difference(native: &TopSubtree, normal: &TopSubtree) -> Option<String> {
    let mut natively = native.as_token_trees().iter_flat_tokens();
    let mut normally = normal.as_token_trees().iter_flat_tokens();
    let mut at = 0usize;
    loop {
        match (natively.next(), normally.next()) {
            (None, None) => return None,
            (Some(ours), Some(theirs)) => {
                if let Some(what) = tree_difference(&ours, &theirs) {
                    return Some(format!("token tree {at}, {}: {what}", shown(&theirs)));
                }
            }
            (None, Some(theirs)) => {
                return Some(format!(
                    "the native output ends after {at} token trees, where the macro's goes on \
                     with {}",
                    shown(&theirs),
                ));
            }
            (Some(ours), None) => {
                return Some(format!(
                    "the native output goes on after the macro's {at} token trees with {}",
                    shown(&ours),
                ));
            }
        }
        at += 1;
    }
}

fn tree_difference(ours: &TokenTree, theirs: &TokenTree) -> Option<String> {
    match (ours, theirs) {
        (TokenTree::Subtree(ours), TokenTree::Subtree(theirs)) => {
            let (a, b) = (ours.delimiter, theirs.delimiter);
            if a.kind != b.kind {
                return Some(format!(
                    "its delimiter is {:?} natively and {:?} by the macro",
                    a.kind, b.kind
                ));
            }
            span_difference("its opening delimiter's", a.open, b.open)
                .or_else(|| span_difference("its closing delimiter's", a.close, b.close))
                .or_else(|| {
                    (ours.len != theirs.len).then(|| {
                        format!(
                            "it holds {} token trees natively and {} by the macro",
                            ours.len, theirs.len
                        )
                    })
                })
        }
        (TokenTree::Leaf(ours), TokenTree::Leaf(theirs)) => leaf_difference(ours, theirs),
        _ => Some(format!(
            "it is {} natively and {} by the macro",
            tree_kind(ours),
            tree_kind(theirs)
        )),
    }
}

fn leaf_difference(ours: &Leaf, theirs: &Leaf) -> Option<String> {
    let differs = |what: &str, a: &dyn std::fmt::Debug, b: &dyn std::fmt::Debug| {
        Some(format!("its {what} is {a:?} natively and {b:?} by the macro"))
    };
    match (ours, theirs) {
        (Leaf::Ident(a), Leaf::Ident(b)) => {
            if a.sym != b.sym {
                return differs("text", &a.sym.as_str(), &b.sym.as_str());
            }
            if a.is_raw != b.is_raw {
                return differs("rawness", &a.is_raw, &b.is_raw);
            }
            span_difference("its", a.span, b.span)
        }
        (Leaf::Punct(a), Leaf::Punct(b)) => {
            if a.char != b.char {
                return differs("character", &a.char, &b.char);
            }
            if a.spacing != b.spacing {
                return differs("spacing", &a.spacing, &b.spacing);
            }
            span_difference("its", a.span, b.span)
        }
        (Leaf::Literal(a), Leaf::Literal(b)) => {
            if a.kind != b.kind {
                return differs("literal kind", &a.kind, &b.kind);
            }
            if a.text() != b.text() {
                return differs("text", &a.text(), &b.text());
            }
            if a.suffix() != b.suffix() {
                return differs("suffix", &a.suffix(), &b.suffix());
            }
            span_difference("its", a.span, b.span)
        }
        _ => Some(format!(
            "it is {} natively and {} by the macro",
            leaf_kind(ours),
            leaf_kind(theirs)
        )),
    }
}

fn span_difference(whose: &str, ours: Span, theirs: Span) -> Option<String> {
    if ours.range != theirs.range {
        return Some(format!(
            "{whose} range is {:?} natively and {:?} by the macro",
            ours.range, theirs.range
        ));
    }
    if ours.anchor != theirs.anchor {
        return Some(format!(
            "{whose} anchor is {:?} natively and {:?} by the macro",
            ours.anchor, theirs.anchor
        ));
    }
    if ours.ctx != theirs.ctx {
        return Some(format!(
            "{whose} syntax context is {:?} natively and {:?} by the macro",
            ours.ctx, theirs.ctx
        ));
    }
    None
}

fn tree_kind(tree: &TokenTree) -> &'static str {
    match tree {
        TokenTree::Subtree(_) => "a group",
        TokenTree::Leaf(leaf) => leaf_kind(leaf),
    }
}

fn leaf_kind(leaf: &Leaf) -> &'static str {
    match leaf {
        Leaf::Ident(_) => "an identifier",
        Leaf::Punct(_) => "a punctuation mark",
        Leaf::Literal(_) => "a literal",
    }
}

/// A token tree as a reader finds it in an error.
fn shown(tree: &TokenTree) -> String {
    match tree {
        TokenTree::Subtree(subtree) => match subtree.delimiter.kind {
            tt::DelimiterKind::Parenthesis => "the group `(`".to_owned(),
            tt::DelimiterKind::Brace => "the group `{`".to_owned(),
            tt::DelimiterKind::Bracket => "the group `[`".to_owned(),
            tt::DelimiterKind::Invisible => "a group without delimiters".to_owned(),
        },
        TokenTree::Leaf(Leaf::Ident(it)) => format!("`{}{}`", it.is_raw.as_str(), it.sym.as_str()),
        TokenTree::Leaf(Leaf::Punct(it)) => format!("`{}`", it.char),
        TokenTree::Leaf(Leaf::Literal(it)) => format!("the literal `{}{}`", it.text(), it.suffix()),
    }
}
