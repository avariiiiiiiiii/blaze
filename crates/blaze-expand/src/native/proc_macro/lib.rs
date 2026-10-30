//! The `proc_macro` crate of blaze's in-process proc-macro bridge
//! (`crates/blaze-expand/src/native/`): the `proc_macro` blaze compiles a proc macro to
//! WebAssembly with (`blaze_macro_wasm::GUEST`), the same text (`api.rs`, included here and
//! concatenated there), with each of the module's imports a function of this crate that asks
//! the host of the expansion running on this thread ([`__native::Host`]).
//! A macro built against it sees what it sees under WebAssembly: the same span words, the
//! same answers, the same text of a stream.
//!
//! What differs is only what the module's exports did: an expansion is
//! [`__native::expand1`] or [`__native::expand2`], given blaze's token tree itself and giving
//! one back, built as the host builds one of the columns the module writes ([`wire`]), or
//! what its panic said. No bytes are written or read.

#![allow(clippy::all, unused, missing_docs)]

/// The module's imports, each asking [`__native::Host`] what the wasm host's function of the
/// same name answers (`crates/blaze-macro-wasm/src/lib.rs`, `add_host`).
mod sys {
    use super::__native::{BUFFER, host};

    /// Puts `bytes` in the buffer for [`take`]: their length.
    fn give(bytes: Vec<u8>) -> i32 {
        let len = bytes.len() as i32;
        BUFFER.with(|it| *it.borrow_mut() = bytes);
        len
    }

    fn give_text(text: Option<String>) -> i32 {
        match text {
            Some(text) => give(text.into_bytes()),
            None => -1,
        }
    }

    /// The text at `ptr`, as the wasm host reads it.
    ///
    /// # Safety
    ///
    /// `ptr` and `len` are those of a `&str` the caller holds.
    unsafe fn read(ptr: *const u8, len: usize) -> String {
        // SAFETY: the caller gives the `ptr` and `len` of a `&str` it holds (# Safety).
        // alloc: the text the host is handed, which it keeps.
        String::from_utf8_lossy(unsafe { std::slice::from_raw_parts(ptr, len) }).into_owned()
    }

    /// # Safety
    ///
    /// None: `unsafe` only to have the signature of the wasm guest's imports, which this
    /// `proc_macro` calls the same way.
    pub unsafe fn def_site() -> u64 {
        host(|it| it.def_site())
    }

    /// # Safety
    ///
    /// None: `unsafe` only to have the signature of the wasm guest's imports, which this
    /// `proc_macro` calls the same way.
    pub unsafe fn call_site() -> u64 {
        host(|it| it.call_site())
    }

    /// # Safety
    ///
    /// None: `unsafe` only to have the signature of the wasm guest's imports, which this
    /// `proc_macro` calls the same way.
    pub unsafe fn mixed_site() -> u64 {
        host(|it| it.mixed_site())
    }

    /// # Safety
    ///
    /// None: `unsafe` only to have the signature of the wasm guest's imports, which this
    /// `proc_macro` calls the same way.
    pub unsafe fn span_join(a: u64, b: u64) -> u64 {
        host(|it| it.span_join(a, b))
    }

    /// # Safety
    ///
    /// None: `unsafe` only to have the signature of the wasm guest's imports, which this
    /// `proc_macro` calls the same way.
    pub unsafe fn span_resolved_at(a: u64, b: u64) -> u64 {
        host(|it| it.span_resolved_at(a, b))
    }

    /// # Safety
    ///
    /// None: `unsafe` only to have the signature of the wasm guest's imports, which this
    /// `proc_macro` calls the same way.
    pub unsafe fn span_located_at(a: u64, b: u64) -> u64 {
        host(|it| it.span_located_at(a, b))
    }

    /// # Safety
    ///
    /// None: `unsafe` only to have the signature of the wasm guest's imports, which this
    /// `proc_macro` calls the same way.
    pub unsafe fn span_parent(a: u64) -> u64 {
        host(|it| it.span_parent(a))
    }

    /// # Safety
    ///
    /// None: `unsafe` only to have the signature of the wasm guest's imports, which this
    /// `proc_macro` calls the same way.
    pub unsafe fn span_source(a: u64) -> u64 {
        host(|it| it.span_source(a))
    }

    /// # Safety
    ///
    /// None: `unsafe` only to have the signature of the wasm guest's imports, which this
    /// `proc_macro` calls the same way.
    pub unsafe fn span_start(a: u64) -> u64 {
        host(|it| it.span_start(a))
    }

    /// # Safety
    ///
    /// None: `unsafe` only to have the signature of the wasm guest's imports, which this
    /// `proc_macro` calls the same way.
    pub unsafe fn span_end(a: u64) -> u64 {
        host(|it| it.span_end(a))
    }

    /// # Safety
    ///
    /// None: `unsafe` only to have the signature of the wasm guest's imports, which this
    /// `proc_macro` calls the same way.
    pub unsafe fn span_subspan(a: u64, start: u64, end: u64) -> u64 {
        host(|it| it.span_subspan(a, start, end))
    }

    /// # Safety
    ///
    /// None: `unsafe` only to have the signature of the wasm guest's imports, which this
    /// `proc_macro` calls the same way.
    pub unsafe fn span_pack(start: u32, len: u32, part: u32) -> u64 {
        host(|it| it.span_pack(start, len, part))
    }

    /// # Safety
    ///
    /// `out` is room for three `u32`s.
    pub unsafe fn span_unpack(a: u64, out: *mut u32) {
        let fields = host(|it| it.span_unpack(a));
        // SAFETY: the caller gives `out` room for three `u32`s (# Safety), which `fields` is,
        // and a value of this frame doesn't overlap it.
        unsafe { std::ptr::copy_nonoverlapping(fields.as_ptr(), out, 3) };
    }

    /// # Safety
    ///
    /// `out` is room for two `u32`s.
    pub unsafe fn span_byte_range(a: u64, out: *mut u32) {
        let [start, end] = host(|it| it.span_byte_range(a));
        // SAFETY: the caller gives `out` room for two `u32`s (# Safety).
        unsafe { out.write(start) };
        // SAFETY: the second of those two is inside the same room.
        let second = unsafe { out.add(1) };
        // SAFETY: `second` is the second `u32` of the caller's room.
        unsafe { second.write(end) };
    }

    /// # Safety
    ///
    /// None: `unsafe` only to have the signature of the wasm guest's imports, which this
    /// `proc_macro` calls the same way.
    pub unsafe fn span_line(a: u64) -> u32 {
        host(|it| it.span_line(a))
    }

    /// # Safety
    ///
    /// None: `unsafe` only to have the signature of the wasm guest's imports, which this
    /// `proc_macro` calls the same way.
    pub unsafe fn span_column(a: u64) -> u32 {
        host(|it| it.span_column(a))
    }

    /// The length of the text put in the buffer for [`take`], or -1 for none.
    ///
    /// # Safety
    ///
    /// None: `unsafe` only to have the signature of the wasm guest's imports, which this
    /// `proc_macro` calls the same way.
    pub unsafe fn span_debug(a: u64) -> i32 {
        give_text(host(|it| it.span_debug(a)))
    }

    /// # Safety
    ///
    /// None: `unsafe` only to have the signature of the wasm guest's imports, which this
    /// `proc_macro` calls the same way.
    pub unsafe fn span_file(a: u64) -> i32 {
        give_text(host(|it| it.span_file(a)))
    }

    /// # Safety
    ///
    /// None: `unsafe` only to have the signature of the wasm guest's imports, which this
    /// `proc_macro` calls the same way.
    pub unsafe fn span_local_file(a: u64) -> i32 {
        give_text(host(|it| it.span_local_file(a)))
    }

    /// # Safety
    ///
    /// None: `unsafe` only to have the signature of the wasm guest's imports, which this
    /// `proc_macro` calls the same way.
    pub unsafe fn span_source_text(a: u64) -> i32 {
        give_text(host(|it| it.span_source_text(a)))
    }

    /// A stream lexed from the text: its encoding's length in the buffer, or `-1 - len`
    /// with the error's text of `len` bytes there.
    ///
    /// # Safety
    ///
    /// `ptr` and `len` are those of a `&str` the caller holds.
    pub unsafe fn ts_from_str(ptr: *const u8, len: usize) -> i32 {
        // SAFETY: `ptr` and `len` are the caller's `&str` (this fn's # Safety), as `read` needs.
        let text = unsafe { read(ptr, len) };
        match host(|it| it.ts_from_str(&text)) {
            Ok(stream) => give(stream),
            Err(error) => -1 - give(error.into_bytes()),
        }
    }

    /// # Safety
    ///
    /// `ptr` and `len` are those of a `&str` the caller holds.
    pub unsafe fn literal_from_str(ptr: *const u8, len: usize) -> i32 {
        // SAFETY: `ptr` and `len` are the caller's `&str` (this fn's # Safety), as `read` needs.
        let text = unsafe { read(ptr, len) };
        match host(|it| it.literal_from_str(&text)) {
            Some(literal) => give(literal),
            None => -1,
        }
    }

    /// Copies the buffer to `to`, and empties it.
    ///
    /// # Safety
    ///
    /// `to` is room for as many bytes as the call that filled the buffer said it
    /// holds, as `taken` makes right after that call.
    pub unsafe fn take(to: *mut u8) {
        let buffer = BUFFER.with(|it| std::mem::take(&mut *it.borrow_mut()));
        // SAFETY: `to` has room for `buffer.len()` bytes (# Safety: what the call that filled
        // the buffer said), and a fresh `Vec` doesn't overlap it.
        unsafe { std::ptr::copy_nonoverlapping(buffer.as_ptr(), to, buffer.len()) };
    }

    // As the wasm host's, these grant nothing and show nothing.
    /// # Safety
    ///
    /// None: `unsafe` only to have the signature of the wasm guest's imports, which this
    /// `proc_macro` calls the same way.
    pub unsafe fn track_path(_ptr: *const u8, _len: usize) {}

    /// # Safety
    ///
    /// None: `unsafe` only to have the signature of the wasm guest's imports, which this
    /// `proc_macro` calls the same way.
    pub unsafe fn track_env_var(_ptr: *const u8, _len: usize) {}

    /// # Safety
    ///
    /// None: `unsafe` only to have the signature of the wasm guest's imports, which this
    /// `proc_macro` calls the same way.
    pub unsafe fn diagnostic(_level: u32, _ptr: *const u8, _len: usize, _span: u64) {}
}

include!("api.rs");

/// How blaze runs a macro natively (`crates/blaze-expand/src/native/`): the host the
/// functions of `sys` ask during one expansion on this thread, and the expansion itself,
/// given blaze's token tree and giving one back.
#[doc(hidden)]
pub mod __native {
    use std::{
        any::Any,
        cell::Cell,
        panic::AssertUnwindSafe,
        sync::Once,
    };

    use super::*;

    /// What the wasm host answers a module's imports with (`crates/blaze-macro-wasm/src/lib.rs`,
    /// `add_host`), each function of the same name, and the words of a tree's spans. A span is
    /// a word of the host's, `u64::MAX` none; text is `None` where the wasm host gives none
    /// (-1).
    pub trait Host {
        fn def_site(&mut self) -> u64;
        fn call_site(&mut self) -> u64;
        fn mixed_site(&mut self) -> u64;
        fn span_join(&mut self, a: u64, b: u64) -> u64;
        fn span_resolved_at(&mut self, a: u64, at: u64) -> u64;
        fn span_located_at(&mut self, a: u64, b: u64) -> u64;
        fn span_parent(&mut self, a: u64) -> u64;
        fn span_source(&mut self, a: u64) -> u64;
        fn span_start(&mut self, a: u64) -> u64;
        fn span_end(&mut self, a: u64) -> u64;
        fn span_subspan(&mut self, a: u64, start: u64, end: u64) -> u64;
        /// The word of `start..start + len` of the expansion's part `part`, `u64::MAX` for a
        /// part it has not.
        fn span_pack(&mut self, start: u32, len: u32, part: u32) -> u64;
        /// `[start, len, part]` of a word, `[0, 0, u32::MAX]` for one it never gave out.
        fn span_unpack(&mut self, a: u64) -> [u32; 3];
        /// `[0, 0]` for a span it never gave out.
        fn span_byte_range(&mut self, a: u64) -> [u32; 2];
        fn span_line(&mut self, a: u64) -> u32;
        fn span_column(&mut self, a: u64) -> u32;
        fn span_debug(&mut self, a: u64) -> Option<String>;
        fn span_file(&mut self, a: u64) -> Option<String>;
        fn span_local_file(&mut self, a: u64) -> Option<String>;
        fn span_source_text(&mut self, a: u64) -> Option<String>;
        /// The stream of `text` as the columns [`wire::Reader`] reads, or why it is none.
        fn ts_from_str(&mut self, text: &str) -> Result<Vec<u8>, String>;
        /// The literal of `text` as the columns of a stream of it alone.
        fn literal_from_str(&mut self, text: &str) -> Option<Vec<u8>>;
        /// The word of `span`, which makes it one of the expansion's.
        fn pack(&mut self, span: span::Span) -> u64;
        /// The span of `word`, `None` for a word the expansion has no span of.
        fn get(&self, word: u64) -> Option<span::Span>;
    }

    /// Why an expansion gave no tree.
    #[derive(Debug)]
    pub enum Failed {
        /// What the macro's panic said.
        Panicked(String),
        /// What is wrong with what it gave back.
        Output(String),
    }

    thread_local! {
        // dyn: the host is blaze-expand's, which this crate can't name.
        static HOST: Cell<Option<*mut (dyn Host + 'static)>> = const { Cell::new(None) };
        /// What a function that gives text or a stream back put there, for `sys::take`.
        // alloc: none, an empty buffer reused by every call.
        pub(crate) static BUFFER: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
        /// Whether a macro runs on this thread, whose panic is its output rather than
        /// something to print.
        static EXPANDING: Cell<bool> = const { Cell::new(false) };
    }

    /// Runs `f` with `host` answering every function of `sys` called on this thread.
    // dyn: the host is blaze-expand's, which this crate can't name.
    pub fn with_host<R>(host: &mut dyn Host, f: impl FnOnce() -> R) -> R {
        // dyn: the host is blaze-expand's, which this crate can't name.
        struct Restore(Option<*mut (dyn Host + 'static)>);
        impl Drop for Restore {
            fn drop(&mut self) {
                HOST.with(|it| it.set(self.0));
            }
        }
        // dyn: the host is blaze-expand's, which this crate can't name.
        let host: *mut (dyn Host + '_) = host;
        // SAFETY: only the lifetime changes. The pointer is read by `host` alone, on this
        // thread, while `f` runs: `Restore` puts the one before it back when `f` returns or
        // unwinds, before the borrow it came from ends.
        let host = unsafe {
            // dyn: the host is blaze-expand's, which this crate can't name.
            std::mem::transmute::<*mut (dyn Host + '_), *mut (dyn Host + 'static)>(host)
        };
        let _restore = Restore(HOST.with(|it| it.replace(Some(host))));
        f()
    }

    /// Runs `f` with the host set on this thread, taken out of [`HOST`] while `f` runs: a
    /// host whose answer calls a function of `sys` itself is told so by the panic below.
    // dyn: the host is blaze-expand's, which this crate can't name.
    pub(crate) fn host<R>(f: impl FnOnce(&mut dyn Host) -> R) -> R {
        // dyn: the host is blaze-expand's, which this crate can't name.
        struct Restore(*mut (dyn Host + 'static));
        impl Drop for Restore {
            fn drop(&mut self) {
                HOST.with(|it| it.set(Some(self.0)));
            }
        }
        let Some(host) = HOST.with(Cell::take) else {
            panic!("procedural macro API is used outside of a procedural macro");
        };
        let _restore = Restore(host);
        // SAFETY: `host` is the pointer `with_host` set from a `&mut dyn Host` it holds the
        // borrow of while it is set, on this thread. It is out of `HOST` until `_restore`
        // puts it back as `f` returns or unwinds, so no other call makes a second reference
        // to it meanwhile: one of `sys` inside `f` panics, and a nested `with_host` (a host's
        // answer expanding another macro) sets another host and gives back `None`. Two live
        // `&mut` of one host would be undefined behavior.
        f(unsafe { &mut *host })
    }

    /// Gives every span of `tree` its word, in the order the wasm host gives a module's input
    /// them (`wire::relative`): each token tree's, a subtree's closing one right after its
    /// opening one, then the top subtree's two.
    fn registered(tree: &tt::TopSubtree) {
        host(|host| {
            for token in tree.token_trees().iter_flat_tokens() {
                match token {
                    tt::TokenTree::Leaf(leaf) => {
                        host.pack(*leaf.span());
                    }
                    tt::TokenTree::Subtree(subtree) => {
                        host.pack(subtree.delimiter.open);
                        host.pack(subtree.delimiter.close);
                    }
                }
            }
            let top = tree.top_subtree().delimiter;
            host.pack(top.open);
            host.pack(top.close);
        })
    }

    fn word(span: span::Span) -> Span {
        Span(host(|host| host.pack(span)))
    }

    fn delimiter_of(kind: tt::DelimiterKind) -> Delimiter {
        match kind {
            tt::DelimiterKind::Parenthesis => Delimiter::Parenthesis,
            tt::DelimiterKind::Brace => Delimiter::Brace,
            tt::DelimiterKind::Bracket => Delimiter::Bracket,
            tt::DelimiterKind::Invisible => Delimiter::None,
        }
    }

    fn kind_of(delimiter: Delimiter) -> tt::DelimiterKind {
        match delimiter {
            Delimiter::Parenthesis => tt::DelimiterKind::Parenthesis,
            Delimiter::Brace => tt::DelimiterKind::Brace,
            Delimiter::Bracket => tt::DelimiterKind::Bracket,
            Delimiter::None => tt::DelimiterKind::Invisible,
        }
    }

    /// The macro's kind of `kind`, by tt's number of it, which is the wire's.
    fn of_tt(kind: tt::LitKind) -> LitKind {
        let (code, hashes) = kind.code();
        LitKind::from_code(u32::from(code), hashes)
    }

    /// tt's kind of `kind`, by the wire's number of it, which is tt's.
    fn to_tt(kind: LitKind) -> tt::LitKind {
        let (code, hashes) = kind.code();
        tt::LitKind::from_code(code as u8, hashes as u8).expect("every kind's number is tt's")
    }

    /// What a macro is given of `tree`, as the module reads it of the tree's columns
    /// (`wire::Reader::input`): what its top subtree holds when that has no delimiters, else
    /// the top subtree, each group's whole span made before what it holds.
    fn given(tree: &tt::TopSubtree) -> TokenStream {
        let top = tree.top_subtree().delimiter;
        let delimiter = delimiter_of(top.kind);
        let (open, close) = (word(top.open), word(top.close));
        if delimiter == Delimiter::None {
            return inside(tree.iter(), open, delimiter);
        }
        let entire = wire::entire(open, close);
        let stream = inside(tree.iter(), open, delimiter);
        // alloc: the macro's own token stream, made as proc_macro's API makes one, only when the macro runs.
        TokenStream(Rc::new(vec![TokenTree::Group(Group { delimiter, stream, open, close, entire })]))
    }

    /// What a group holds, without the parentheses of a passed-on fragment
    /// (`wire::Reader::inside`).
    fn inside(trees: tt::TtIter<'_>, open: Span, delimiter: Delimiter) -> TokenStream {
        if delimiter == Delimiter::None {
            // clone: an iterator over the tree, two indices.
            let mut probe = trees.clone();
            if let (Some(tt::TtElement::Subtree(subtree, inner)), None) = (probe.next(), probe.next())
            {
                let wrapper = subtree.delimiter.kind == tt::DelimiterKind::Parenthesis
                    && word(subtree.delimiter.open) == open
                    && word(subtree.delimiter.close) == open;
                if wrapper {
                    return stream(inner);
                }
            }
        }
        stream(trees)
    }

    fn stream(trees: tt::TtIter<'_>) -> TokenStream {
        // alloc: the macro's own token stream, made as proc_macro's API makes one, only when the macro runs.
        let mut out = Vec::new();
        for tree in trees {
            out.push(match tree {
                tt::TtElement::Subtree(subtree, inner) => {
                    let delimiter = delimiter_of(subtree.delimiter.kind);
                    let (open, close) = (word(subtree.delimiter.open), word(subtree.delimiter.close));
                    let entire = wire::entire(open, close);
                    let stream = inside(inner, open, delimiter);
                    TokenTree::Group(Group { delimiter, stream, open, close, entire })
                }
                tt::TtElement::Leaf(tt::Leaf::Punct(punct)) => TokenTree::Punct(Punct {
                    ch: punct.char,
                    joint: punct.spacing == tt::Spacing::Joint,
                    span: word(punct.span),
                }),
                tt::TtElement::Leaf(tt::Leaf::Ident(ident)) => TokenTree::Ident(Ident {
                    sym: ident.sym.as_str().into(),
                    kind: if ident.is_raw.yes() { IdentKind::Raw } else { IdentKind::Normal },
                    span: word(ident.span),
                }),
                tt::TtElement::Leaf(tt::Leaf::Literal(literal)) => {
                    let (text, suffix) = literal.text_and_suffix();
                    TokenTree::Literal(Literal {
                        kind: of_tt(literal.kind),
                        symbol: text.into(),
                        suffix: (!suffix.is_empty()).then(|| suffix.into()),
                        span: word(literal.span),
                    })
                }
            });
        }
        // alloc: the macro's own token stream, made as proc_macro's API makes one, only when the macro runs.
        TokenStream(Rc::new(out))
    }

    /// The span of `word`, or why the output can't have it.
    // dyn: the host is blaze-expand's, which this crate can't name.
    fn unpacked(host: &mut dyn Host, word: Span) -> Result<span::Span, Failed> {
        host.get(word.0).ok_or_else(|| {
            // alloc: the message of an output refused.
            Failed::Output(format!("it gave a span {:#x} that its expansion has not", word.0))
        })
    }

    /// The tree blaze's builder makes of `stream`, as the wasm host reads it of the columns
    /// the module writes (`wire::write`): its one group as the top subtree, else a top without
    /// delimiters at the call site; a negative number as a minus sign and the number.
    fn built(stream: &TokenStream) -> Result<tt::TopSubtree, Failed> {
        host(|host| {
            let (delimiter, open, close, stream) = match &stream.0[..] {
                [TokenTree::Group(group)] => {
                    (group.delimiter, group.open, group.close, &group.stream)
                }
                _ => {
                    let at = Span(host.call_site());
                    (Delimiter::None, at, at, stream)
                }
            };
            let (open, close) = (unpacked(host, open)?, unpacked(host, close)?);
            let top = tt::Delimiter { open, close, kind: kind_of(delimiter) };
            let mut builder = tt::TopSubtreeBuilder::new(top);
            push(host, &mut builder, stream)?;
            Ok(builder.build())
        })
    }

    fn push(
        // dyn: the host is blaze-expand's, which this crate can't name.
        host: &mut dyn Host,
        builder: &mut tt::TopSubtreeBuilder,
        stream: &TokenStream,
    ) -> Result<(), Failed> {
        for tree in stream.0.iter() {
            match tree {
                TokenTree::Group(group) => {
                    builder.open(kind_of(group.delimiter), unpacked(host, group.open)?);
                    push(host, builder, &group.stream)?;
                    builder.close(unpacked(host, group.close)?);
                }
                TokenTree::Punct(punct) => {
                    let spacing = if punct.joint { tt::Spacing::Joint } else { tt::Spacing::Alone };
                    let span = unpacked(host, punct.span)?;
                    builder.push(tt::Leaf::Punct(tt::Punct { char: punct.ch, spacing, span }));
                }
                TokenTree::Ident(ident) => {
                    let is_raw =
                        if ident.kind == IdentKind::Raw { tt::IdentIsRaw::Yes } else { tt::IdentIsRaw::No };
                    let span = unpacked(host, ident.span)?;
                    let sym = intern::Symbol::intern(&ident.sym);
                    builder.push(tt::Leaf::Ident(tt::Ident { sym, span, is_raw }));
                }
                TokenTree::Literal(literal) => {
                    let span = unpacked(host, literal.span)?;
                    let mut text = &*literal.symbol;
                    let number = matches!(literal.kind, LitKind::Integer | LitKind::Float);
                    if number && text.starts_with('-') {
                        let minus = tt::Punct { char: '-', spacing: tt::Spacing::Alone, span };
                        builder.push(tt::Leaf::Punct(minus));
                        text = &text[1..];
                    }
                    let suffix = literal.suffix.as_deref().unwrap_or("");
                    let kind = to_tt(literal.kind);
                    builder.push(tt::Leaf::Literal(tt::Literal::new(text, span, kind, suffix)));
                }
            }
        }
        Ok(())
    }

    /// What a panic's payload says, as the module's panic hook has it.
    // dyn: a panic's payload, read only when the macro panicked.
    fn message(payload: &(dyn Any + Send)) -> String {
        match (payload.downcast_ref::<&str>(), payload.downcast_ref::<String>()) {
            // alloc: the message of a panic.
            (Some(it), _) => (*it).to_owned(),
            // alloc: the message of a panic.
            (_, Some(it)) => it.clone(),
            // alloc: the message of a panic.
            _ => "Box<dyn Any>".to_owned(),
        }
    }

    /// Keeps a macro's panic off stderr, as the module's hook does, and leaves every other
    /// panic to the hook before it.
    fn hook() {
        static HOOKED: Once = Once::new();
        HOOKED.call_once(|| {
            let before = std::panic::take_hook();
            // alloc: the hook, set once per process.
            std::panic::set_hook(Box::new(move |info| {
                if !EXPANDING.try_with(Cell::get).unwrap_or(false) {
                    before(info);
                }
            }));
        });
    }

    fn caught<R>(f: impl FnOnce() -> R) -> Result<R, Failed> {
        hook();
        let was = EXPANDING.replace(true);
        let ran = std::panic::catch_unwind(AssertUnwindSafe(f));
        EXPANDING.set(was);
        ran.map_err(|payload| Failed::Panicked(message(&*payload)))
    }

    /// A derive or a function-like macro, given `input`, run with the host [`with_host`]
    /// set: its output, or what its panic said.
    pub fn expand1(
        f: fn(TokenStream) -> TokenStream,
        input: &tt::TopSubtree,
    ) -> Result<tt::TopSubtree, Failed> {
        registered(input);
        let output = caught(|| f(given(input)))?;
        built(&output)
    }

    /// An attribute, given `attr` (none for one written without arguments, an empty stream
    /// to the macro) and `input`, run with the host [`with_host`] set: its output, or what its
    /// panic said.
    pub fn expand2(
        f: fn(TokenStream, TokenStream) -> TokenStream,
        attr: Option<&tt::TopSubtree>,
        input: &tt::TopSubtree,
    ) -> Result<tt::TopSubtree, Failed> {
        registered(input);
        if let Some(attr) = attr {
            registered(attr);
        }
        let output = caught(|| {
            let attr = attr.map_or_else(TokenStream::new, given);
            f(attr, given(input))
        })?;
        built(&output)
    }
}
