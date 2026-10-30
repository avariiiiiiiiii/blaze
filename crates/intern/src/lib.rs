//! Global interning in sharded, append-only arenas (docs/v2/plan/lanes/format.md section 5, "IDs, two
//! kinds"): an interned value is stored once, never moves and is never freed, and its handle is
//! a `Copy` pointer compared by address.

mod arena;
mod intern;
mod intern_slice;
mod mapped;
mod symbol;
mod sync;
mod table;

pub use self::intern::{InternStorage, Internable, Interned, InternedRef, impl_internable};
pub use self::intern_slice::{
    HeaderSlice, HeaderWithLength, InternSliceStorage, InternedSlice, InternedSliceRef,
    SliceInternable, impl_slice_internable,
};
pub use self::mapped::{Id, Mapped};
pub use self::symbol::{
    KnownNames, Symbol, order_key, restore_symbols, symbols as sym, symbols_schema, write_symbols,
};
pub use self::table::{Live, RestoreError};
