//! The `proc_macro` crate of a proc macro blaze compiles to WebAssembly, as dtolnay's watt
//! runs one: blaze puts this text, then the API it shares with the in-process bridge's
//! `proc_macro` (`crates/blaze-expand/src/native/proc_macro/api.rs`; both are
//! `blaze_macro_wasm::GUEST`), in place of the standard library's `library/proc_macro` root
//! when it compiles a proc-macro crate for `wasm32-wasip1` (`crates/blaze-macro-wasm/src/lib.rs`
//! says how the module is run). This file holds only the module's imports (`sys`), its
//! allocator and its exports. `proc-macro2`, `syn` and `quote` compile against it unchanged.
//!
//! Token trees live in the module's memory: a stream is a vector of them, an identifier or a
//! literal holds its text. A span is a word of the host's (its range and the index of its
//! anchor and context among the expansion's, packed as a token tree packs one), and what can
//! only be known of it there (joining two, its source text, its file) is asked of the host by
//! an import. The input of a macro arrives, and its output leaves, as a token tree's columns
//! ([`wire`]). Displaying a
//! stream follows the proc-macro server blaze expands native macros with
//! (`crates/proc-macro-srv/src/token_stream.rs`), so a macro that
//! compares `to_string()`s sees the same text either way.

#![allow(clippy::all, unused, missing_docs)]

mod sys {
    #[link(wasm_import_module = "blaze_proc_macro")]
    unsafe extern "C" {
        pub fn def_site() -> u64;
        pub fn call_site() -> u64;
        pub fn mixed_site() -> u64;
        pub fn span_join(a: u64, b: u64) -> u64;
        pub fn span_resolved_at(a: u64, b: u64) -> u64;
        pub fn span_located_at(a: u64, b: u64) -> u64;
        pub fn span_parent(a: u64) -> u64;
        pub fn span_source(a: u64) -> u64;
        pub fn span_start(a: u64) -> u64;
        pub fn span_end(a: u64) -> u64;
        pub fn span_subspan(a: u64, start: u64, end: u64) -> u64;
        pub fn span_byte_range(a: u64, out: *mut u32);
        pub fn span_line(a: u64) -> u32;
        pub fn span_column(a: u64) -> u32;
        /// The word of the span `start..start + len` of the expansion's part `part`, for one
        /// that doesn't pack.
        pub fn span_pack(start: u32, len: u32, part: u32) -> u64;
        /// Writes `[start, len, part]` of a span that doesn't pack to `out`.
        pub fn span_unpack(a: u64, out: *mut u32);
        /// The length of the text the host put in its buffer for [`take`], or -1 for none.
        pub fn span_debug(a: u64) -> i32;
        pub fn span_file(a: u64) -> i32;
        pub fn span_local_file(a: u64) -> i32;
        pub fn span_source_text(a: u64) -> i32;
        /// A stream lexed from the text, as a tree's columns: their length in the buffer, or
        /// `-1 - len` with the error's text of `len` bytes there.
        pub fn ts_from_str(ptr: *const u8, len: usize) -> i32;
        pub fn literal_from_str(ptr: *const u8, len: usize) -> i32;
        /// Copies the host's buffer to `to`.
        pub fn take(to: *mut u8);
        pub fn track_path(ptr: *const u8, len: usize);
        pub fn track_env_var(ptr: *const u8, len: usize);
        pub fn diagnostic(level: u32, ptr: *const u8, len: usize, span: u64);
        /// The macro's output, encoded.
        pub fn output(ptr: *const u8, len: usize);
        /// What a panic said, before the module traps.
        pub fn panicked(ptr: *const u8, len: usize);
        /// That the expansion left `bytes` of what it allocated live when it returned.
        pub fn retained(bytes: usize);
    }
}

/// The module's allocator: the standard library's, counting the bytes live, so that the host
/// learns whether an expansion left anything behind for a later one (`__blaze::expand1`).
struct Counted;

static LIVE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

fn live() -> usize {
    LIVE.load(std::sync::atomic::Ordering::Relaxed)
}

// SAFETY: every method passes its arguments to `System` unchanged and returns what it returns; the
// counter is only a statistic.
unsafe impl std::alloc::GlobalAlloc for Counted {
    unsafe fn alloc(&self, layout: std::alloc::Layout) -> *mut u8 {
        // SAFETY: the caller upholds `GlobalAlloc`'s contract for this call, which is `System`'s.
        let at = unsafe { std::alloc::System.alloc(layout) };
        if !at.is_null() {
            LIVE.fetch_add(layout.size(), std::sync::atomic::Ordering::Relaxed);
        }
        at
    }

    unsafe fn alloc_zeroed(&self, layout: std::alloc::Layout) -> *mut u8 {
        // SAFETY: the caller upholds `GlobalAlloc`'s contract for this call, which is `System`'s.
        let at = unsafe { std::alloc::System.alloc_zeroed(layout) };
        if !at.is_null() {
            LIVE.fetch_add(layout.size(), std::sync::atomic::Ordering::Relaxed);
        }
        at
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: std::alloc::Layout) {
        // SAFETY: the caller upholds `GlobalAlloc`'s contract for this call, which is `System`'s.
        unsafe { std::alloc::System.dealloc(ptr, layout) };
        LIVE.fetch_sub(layout.size(), std::sync::atomic::Ordering::Relaxed);
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: std::alloc::Layout, size: usize) -> *mut u8 {
        // SAFETY: the caller upholds `GlobalAlloc`'s contract for this call, which is `System`'s.
        let at = unsafe { std::alloc::System.realloc(ptr, layout, size) };
        if !at.is_null() {
            LIVE.fetch_add(size, std::sync::atomic::Ordering::Relaxed);
            LIVE.fetch_sub(layout.size(), std::sync::atomic::Ordering::Relaxed);
        }
        at
    }
}

#[global_allocator]
static ALLOCATOR: Counted = Counted;


/// What blaze's list of a crate's macros calls (`crates/blaze-engine/src/macro_list.rs`): the
/// module's exports run a macro through these.
#[doc(hidden)]
pub mod __blaze {
    use super::*;

    /// Room for the host to write an input into, kept until the expansion ends.
    pub fn alloc(len: usize) -> *mut u8 {
        let mut room = vec![0u8; len];
        let at = room.as_mut_ptr();
        ROOM.with(|it| it.borrow_mut().push(room));
        at
    }

    thread_local! {
        static ROOM: RefCell<Vec<Vec<u8>>> = const { RefCell::new(Vec::new()) };
    }

    /// The `len` bytes the host wrote at `ptr`, found among the room [`alloc`] gave out rather
    /// than read through the pointer, so a caller of the safe `expand1` or `expand2` can't
    /// make it read memory that is not an input. A length of 0 is no bytes, found nowhere: the
    /// host passes a null pointer with it when there is no attribute.
    fn input(ptr: *const u8, len: usize) -> TokenStream {
        if len == 0 {
            return TokenStream::new();
        }
        ROOM.with(|room| {
            let room = room.borrow();
            let Some(bytes) = room.iter().find(|it| std::ptr::eq(it.as_ptr(), ptr)) else {
                panic!("the host gave an input at {ptr:?}, which is no room `alloc` gave out");
            };
            let Some(bytes) = bytes.get(..len) else {
                panic!("the host gave an input of {len} bytes in room of {}", bytes.len());
            };
            wire::Reader::new(bytes).input()
        })
    }

    fn hook() {
        std::panic::set_hook(Box::new(|info| {
            let payload = info.payload();
            let message = match (payload.downcast_ref::<&str>(), payload.downcast_ref::<String>()) {
                (Some(it), _) => (*it).to_owned(),
                (_, Some(it)) => it.clone(),
                _ => "Box<dyn Any>".to_owned(),
            };
            // SAFETY: `message` is borrowed for the call, so its `message.len()` bytes at
            // `message.as_ptr()` stay live and unchanged; the host only reads them, during the
            // call, and copies what it keeps.
            unsafe { sys::panicked(message.as_ptr(), message.len()) };
        }));
    }

    fn give(output: TokenStream) {
        let mut out = Vec::new();
        wire::write(&output, &mut out);
        // SAFETY: `out` is borrowed for the call, so its `out.len()` bytes at `out.as_ptr()` stay
        // live and unchanged; the host only reads them, during the call, and copies what it keeps.
        unsafe { sys::output(out.as_ptr(), out.len()) };
    }

    /// What is live before an expansion, less the room its input was written into, which it
    /// frees: what is live after it beyond that, it left for a later one.
    fn before() -> usize {
        live() - ROOM.with(|room| room.borrow().iter().map(Vec::capacity).sum::<usize>())
    }

    fn after(before: usize) {
        ROOM.with(|room| room.borrow_mut().clear());
        let left = live().saturating_sub(before);
        if left != 0 {
            // SAFETY: the `sys` call takes and gives integers only, so it reads and writes no
            // memory of this crate's, and nothing the caller holds can break.
            unsafe { sys::retained(left) };
        }
    }

    /// A derive or a function-like macro.
    pub fn expand1(f: fn(TokenStream) -> TokenStream, ptr: *const u8, len: usize) {
        hook();
        let before = before();
        give(f(input(ptr, len)));
        after(before);
    }

    /// An attribute.
    pub fn expand2(
        f: fn(TokenStream, TokenStream) -> TokenStream,
        attr: *const u8,
        attr_len: usize,
        ptr: *const u8,
        len: usize,
    ) {
        hook();
        let before = before();
        give(f(input(attr, attr_len), input(ptr, len)));
        after(before);
    }

    /// The list of macros, a line each: kind (0 derive, 1 attribute, 2 function-like), name
    /// and the derive's helper attributes, tab-separated.
    pub fn list(text: &str) {
        // SAFETY: `text` is borrowed for the call, so its `text.len()` bytes at `text.as_ptr()`
        // stay live and unchanged; the host only reads them, during the call, and copies what it
        // keeps.
        unsafe { sys::output(text.as_ptr(), text.len()) };
    }

    /// As [`list`], of the bytes of the static the module also carries in a custom section.
    pub fn list_bytes(text: &[u8]) {
        // SAFETY: `text` is borrowed for the call, so its `text.len()` bytes at `text.as_ptr()`
        // stay live and unchanged; the host only reads them, during the call, and copies what it
        // keeps.
        unsafe { sys::output(text.as_ptr(), text.len()) };
    }
}
