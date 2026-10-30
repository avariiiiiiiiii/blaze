//! Machinery for hygienic macros.
//!
//! Inspired by Matthew Flatt et al., “Macros That Work Together: Compile-Time Bindings, Partial
//! Expansion, and Definition Contexts,” *Journal of Functional Programming* 22, no. 2
//! (March 1, 2012): 181–216, <https://doi.org/10.1017/S0956796812000093>.
//!
//! Also see <https://rustc-dev-guide.rust-lang.org/macro-expansion.html#hygiene-and-hierarchies>
//!
//! # The Expansion Order Hierarchy
//!
//! `ExpnData` in rustc, rust-analyzer's version is `MacroCallLoc`. Traversing the hierarchy
//! upwards can be achieved by walking up `MacroCallLoc::kind`'s contained file id, as
//! `MacroFile`s are interned `MacroCallLoc`s.
//!
//! # The Macro Definition Hierarchy
//!
//! `SyntaxContextData` in rustc and rust-analyzer. Basically the same in both.
//!
//! # The Call-site Hierarchy
//!
//! `ExpnData::call_site` in rustc, `MacroCallLoc::call_site` in rust-analyzer.

pub use self::imp::*;

/// A syntax context describes a hierarchy tracking order of macro definitions.
#[cfg(feature = "salsa")]
mod imp {
    use std::{fmt, num::NonZeroU32};

    use salsa::{
        Database,
        plumbing::{AsId, FromId},
    };
    use syntax::Edition;

    use crate::{MacroCallId, Transparency};

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    struct SyntaxContextData {
        outer_expn: MacroCallId,
        outer_transparency: Transparency,
        edition: Edition,
        parent: SyntaxContext,
        opaque_if_not_self: Option<SyntaxContext>,
        opaque_and_semiopaque_if_not_self: Option<SyntaxContext>,
    }

    impl SyntaxContextData {
        #[inline]
        fn opaque(&self, ctx: SyntaxContext) -> SyntaxContext {
            self.opaque_if_not_self.unwrap_or(ctx)
        }

        #[inline]
        fn opaque_and_semiopaque(&self, ctx: SyntaxContext) -> SyntaxContext {
            self.opaque_and_semiopaque_if_not_self.unwrap_or(ctx)
        }
    }

    // `unsafe(no_lifetime)`: the values are never collected (`revisions = usize::MAX`), so a
    // `SyntaxContextImpl` held without the database's lifetime never outlives its slot.
    #[salsa::interned(unsafe(no_lifetime), revisions = usize::MAX)]
    struct SyntaxContextImpl {
        data: SyntaxContextData,
    }

    #[derive(Clone, Copy, PartialEq, Eq, Hash)]
    pub struct SyntaxContext(NonZeroU32);

    impl SyntaxContext {
        const MAX_ROOT_ID: u32 = salsa::Id::MAX_U32 + 1 + Edition::LATEST as u32;

        #[inline]
        pub fn into_u32(self) -> u32 {
            self.0.get() - 1
        }

        /// # Contract
        ///
        /// `u32` is what [`SyntaxContext::into_u32`] gave for a context of this database.
        /// Memory safety doesn't rest on it ([`SyntaxContext::id`] checks the bound salsa needs,
        /// and salsa's table checks the page, slot and type of every lookup): another value
        /// gives a panic, or another context's hygiene data. Panics on `u32::MAX`, which no
        /// context has.
        #[inline]
        pub fn from_u32(u32: u32) -> Self {
            match u32.checked_add(1).and_then(NonZeroU32::new) {
                Some(it) => Self(it),
                None => panic!("invalid SyntaxContext({u32})"),
            }
        }

        #[inline]
        fn id(self) -> Option<SyntaxContextImpl> {
            if self.is_root() {
                return None;
            }
            let index = self.0.get() - 1;
            assert!(index < salsa::Id::MAX_U32, "SyntaxContext({index}) is no root and no salsa id");
            // The id `Id::from_index(index)` would make (stored index `index + 1`, generation
            // 0), with the bound checked instead of trusted. Whether it is a `SyntaxContextImpl`
            // salsa's table checks on lookup (page, slot and the page's type), panicking if not.
            let id = salsa::Id::from_bits(u64::from(index) + 1);
            Some(SyntaxContextImpl::from_id(id))
        }

        #[inline]
        pub fn is_root(self) -> bool {
            (SyntaxContext::MAX_ROOT_ID - Edition::LATEST as u32) <= self.0.get()
                && self.0.get() <= (SyntaxContext::MAX_ROOT_ID - Edition::Edition2015 as u32)
        }

        #[inline]
        pub fn remove_root_edition(&mut self) {
            if self.is_root() {
                *self = Self::root(Edition::Edition2015);
            }
        }

        /// The root context, which is the parent of all other contexts. All `FileId`s have this context.
        #[inline]
        pub const fn root(edition: Edition) -> Self {
            let edition = edition as u32;
            // INVARIANT: Roots are valid `SyntaxContext`s
            SyntaxContext(NonZeroU32::new(SyntaxContext::MAX_ROOT_ID - edition).unwrap())
        }

        pub fn new<Db: ?Sized + Database>(
            db: &Db,
            outer_expn: MacroCallId,
            outer_transparency: Transparency,
            edition: Edition,
            parent: SyntaxContext,
            opaque_if_not_self: Option<SyntaxContext>,
            opaque_and_semiopaque_if_not_self: Option<SyntaxContext>,
        ) -> Self {
            let result = SyntaxContextImpl::new(
                db,
                SyntaxContextData {
                    outer_expn,
                    outer_transparency,
                    edition,
                    parent,
                    opaque_if_not_self,
                    opaque_and_semiopaque_if_not_self,
                },
            );
            SyntaxContext(NonZeroU32::new(result.as_id().index() + 1).unwrap())
        }

        /// Invariant: Only the root [`SyntaxContext`] has a [`None`] outer expansion.
        // FIXME: The None case needs to encode the context crate id. We can encode that as the MSB of
        // MacroCallId is reserved anyways so we can do bit tagging here just fine.
        // The bigger issue is that this will cause interning to now create completely separate chains
        // per crate. Though that is likely not a problem as `MacroCallId`s are already crate calling dependent.
        #[inline]
        pub fn outer_expn<Db: ?Sized + Database>(self, db: &Db) -> Option<MacroCallId> {
            self.id().map(|id| id.data(db).outer_expn)
        }

        #[inline]
        pub fn outer_transparency<Db: ?Sized + Database>(self, db: &Db) -> Transparency {
            match self.id() {
                Some(id) => id.data(db).outer_transparency,
                None => Transparency::Opaque,
            }
        }

        #[inline]
        pub fn edition<Db: ?Sized + Database>(self, db: &Db) -> Edition {
            match self.id() {
                Some(id) => id.data(db).edition,
                None => Edition::from_u32(SyntaxContext::MAX_ROOT_ID - self.0.get()),
            }
        }

        #[inline]
        pub fn parent<Db: ?Sized + Database>(self, db: &Db) -> SyntaxContext {
            match self.id() {
                Some(id) => id.data(db).parent,
                None => self,
            }
        }

        /// This context, but with all transparent and semi-opaque expansions filtered away.
        #[inline]
        pub fn opaque<Db: ?Sized + Database>(self, db: &Db) -> SyntaxContext {
            match self.id() {
                Some(id) => id.data(db).opaque(self),
                None => self,
            }
        }

        /// This context, but with all transparent expansions filtered away.
        #[inline]
        pub fn opaque_and_semiopaque<Db: ?Sized + Database>(self, db: &Db) -> SyntaxContext {
            match self.id() {
                Some(id) => id.data(db).opaque_and_semiopaque(self),
                None => self,
            }
        }

        #[inline]
        pub fn opaque_and_opaque_and_semiopaque<Db: ?Sized + Database>(
            self,
            db: &Db,
        ) -> (SyntaxContext, SyntaxContext) {
            match self.id() {
                Some(id) => {
                    let data = id.data(db);
                    (data.opaque(self), data.opaque_and_semiopaque(self))
                }
                None => (self, self),
            }
        }

        #[inline]
        pub fn outer_mark<Db: ?Sized + Database>(
            self,
            db: &Db,
        ) -> (Option<MacroCallId>, Transparency) {
            match self.id() {
                Some(id) => {
                    let data = id.data(db);
                    (Some(data.outer_expn), data.outer_transparency)
                }
                None => (None, Transparency::Opaque),
            }
        }

        #[inline]
        pub fn normalize_to_macros_2_0<Db: ?Sized + Database>(self, db: &Db) -> SyntaxContext {
            self.opaque(db)
        }

        #[inline]
        pub fn normalize_to_macro_rules<Db: ?Sized + Database>(self, db: &Db) -> SyntaxContext {
            self.opaque_and_semiopaque(db)
        }

        #[inline]
        pub fn is_opaque<Db: ?Sized + Database>(self, db: &Db) -> bool {
            !self.is_root() && self.outer_transparency(db).is_opaque()
        }

        #[inline]
        pub fn remove_mark<Db: ?Sized + Database>(
            &mut self,
            db: &Db,
        ) -> (Option<MacroCallId>, Transparency) {
            match self.id() {
                Some(id) => {
                    let data = id.data(db);
                    *self = data.parent;
                    (Some(data.outer_expn), data.outer_transparency)
                }
                None => (None, Transparency::Opaque),
            }
        }

        pub fn marks<Db: ?Sized + Database>(
            self,
            db: &Db,
        ) -> impl Iterator<Item = (MacroCallId, Transparency)> {
            // alloc: only for a macros 1.0 call in a macros 2.0 expansion (`apply_mark`)
            let mut marks = self.marks_rev(db).collect::<Vec<_>>();
            marks.reverse();
            marks.into_iter()
        }

        #[inline]
        pub fn marks_rev<Db: ?Sized + Database>(
            self,
            db: &Db,
        ) -> impl Iterator<Item = (MacroCallId, Transparency)> {
            let data = move |ctx: SyntaxContext| ctx.id().map(|it| it.data(db));
            std::iter::successors(data(self), move |ctx| data(ctx.parent))
                .map(|ctx| (ctx.outer_expn, ctx.outer_transparency))
        }
    }

    impl fmt::Display for SyntaxContext {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            if self.is_root() {
                write!(
                    f,
                    "ROOT{}",
                    Edition::from_u32(SyntaxContext::MAX_ROOT_ID - self.0.get()).number()
                )
            } else {
                write!(f, "{}", self.0.get())
            }
        }
    }

    impl fmt::Debug for SyntaxContext {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            if f.alternate() {
                fmt::Display::fmt(self, f)
            } else {
                f.debug_tuple("SyntaxContext").field(&self.0).finish()
            }
        }
    }
}

#[cfg(not(feature = "salsa"))]
mod imp {
    use std::{fmt, num::NonZeroU32};

    #[derive(Copy, Clone, PartialEq, PartialOrd, Eq, Ord, Hash)]
    pub struct SyntaxContext(NonZeroU32);

    impl SyntaxContext {
        #[inline]
        pub fn into_u32(self) -> u32 {
            self.0.get() - 1
        }

        /// Panics on `u32::MAX`, which no context has.
        #[inline]
        pub fn from_u32(u32: u32) -> Self {
            match u32.checked_add(1).and_then(NonZeroU32::new) {
                Some(it) => Self(it),
                None => panic!("invalid SyntaxContext({u32})"),
            }
        }
    }

    impl fmt::Display for SyntaxContext {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "{}", self.0.get())
        }
    }

    impl fmt::Debug for SyntaxContext {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            if f.alternate() {
                fmt::Display::fmt(self, f)
            } else {
                f.debug_tuple("SyntaxContext").field(&self.0).finish()
            }
        }
    }
}

/// A property of a macro expansion that determines how identifiers
/// produced by that expansion are resolved.
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Hash, Debug)]
pub enum Transparency {
    /// Identifier produced by a transparent expansion is always resolved at call-site.
    /// Call-site spans in procedural macros, hygiene opt-out in `macro` should use this.
    Transparent,
    /// Identifier produced by a semi-opaque expansion may be resolved
    /// either at call-site or at definition-site.
    /// If it's a local variable, label or `$crate` then it's resolved at def-site.
    /// Otherwise it's resolved at call-site.
    /// `macro_rules` macros behave like this, built-in macros currently behave like this too,
    /// but that's an implementation detail.
    SemiOpaque,
    /// Identifier produced by an opaque expansion is always resolved at definition-site.
    /// Def-site spans in procedural macros, identifiers from `macro` by default use this.
    Opaque,
}

impl Transparency {
    /// Returns `true` if the transparency is [`Opaque`].
    ///
    /// [`Opaque`]: Transparency::Opaque
    pub fn is_opaque(&self) -> bool {
        matches!(self, Self::Opaque)
    }
}

#[cfg(test)]
mod tests {
    use syntax::Edition;

    use super::*;

    #[test]
    fn test_root_edition_is_root() {
        for edition in Edition::iter() {
            let ctx = SyntaxContext::root(edition);
            assert!(ctx.is_root(), "{edition} root should be identified as root");
        }
    }

    #[test]
    fn test_root_edition_editions() {
        let db = salsa::DatabaseImpl::new();
        for edition in Edition::iter() {
            let ctx = SyntaxContext::root(edition);
            assert_eq!(edition, ctx.edition(&db), "{edition} root should have edition {edition}");
        }
    }

    #[test]
    fn test_roots_do_not_overlap_with_salsa_ids() {
        for edition in Edition::iter() {
            let root = SyntaxContext::root(edition);
            let root_u32 = root.into_u32();
            assert!(
                root_u32 >= salsa::Id::MAX_U32,
                "Root context for {:?} (value {}) must be >= salsa::Id::MAX_U32 ({}) to avoid collision",
                edition,
                root_u32,
                salsa::Id::MAX_U32
            );
        }
    }

    #[test]
    fn test_non_root_value_is_not_root() {
        for edition in Edition::iter() {
            let ctx = SyntaxContext::from_u32(edition as u32 + 1);
            assert!(!ctx.is_root(), "{edition} root should be identified as root");
        }
    }

    #[test]
    fn test_interned_context_round_trips_through_u32() {
        let db = salsa::DatabaseImpl::new();
        let root = SyntaxContext::root(Edition::Edition2015);
        let ctx = SyntaxContext::new(
            &db,
            // Stored as the expansion of the context and never looked up.
            crate::MacroCallId(salsa::Id::from_bits(2)),
            Transparency::Opaque,
            Edition::Edition2021,
            root,
            Some(root),
            Some(root),
        );

        let round_tripped = SyntaxContext::from_u32(ctx.into_u32());
        assert_eq!(round_tripped.edition(&db), Edition::Edition2021);
        assert_eq!(round_tripped.parent(&db), root);
    }

    /// A value past the roots, which no salsa id has, panics when looked up instead of making
    /// an `Id` out of `from_index`'s range.
    #[test]
    #[should_panic(expected = "no root and no salsa id")]
    fn a_context_past_the_roots_panics() {
        let db = salsa::DatabaseImpl::new();
        let ctx = SyntaxContext::from_u32(u32::MAX - 1);
        assert!(!ctx.is_root());
        ctx.parent(&db);
    }

    #[test]
    #[should_panic(expected = "invalid SyntaxContext")]
    fn the_last_u32_is_no_context() {
        let _ = SyntaxContext::from_u32(u32::MAX);
    }

    /// What name resolution asks of a context, warm: each part of its data, its marks walked
    /// outward, and interning a context that exists again, none of which allocates.
    #[test]
    fn warm_context_reads_allocate_nothing() {
        assert!(blaze_alloc_count::installed());
        let db = salsa::DatabaseImpl::new();
        let root = SyntaxContext::root(Edition::Edition2021);
        let transparencies =
            [Transparency::Opaque, Transparency::SemiOpaque, Transparency::Transparent];
        let make = |n: u64, parent: SyntaxContext| {
            SyntaxContext::new(
                &db,
                // Stored as the expansion of the context and never looked up.
                crate::MacroCallId(salsa::Id::from_bits(n + 2)),
                transparencies[n as usize % 3],
                Edition::Edition2021,
                parent,
                Some(root),
                Some(root),
            )
        };
        let mut chain = vec![root];
        for n in 0..8 {
            let parent = *chain.last().unwrap();
            chain.push(make(n, parent));
        }
        let read = || {
            for &ctx in &chain {
                std::hint::black_box((
                    ctx.outer_mark(&db),
                    ctx.edition(&db),
                    ctx.parent(&db),
                    ctx.opaque_and_opaque_and_semiopaque(&db),
                    ctx.is_opaque(&db),
                    ctx.marks_rev(&db).count(),
                ));
            }
        };
        read();
        blaze_alloc_count::assert_at_most(0, "warm context reads", read);
        let again =
            blaze_alloc_count::assert_at_most(0, "interning a context that exists", || {
                make(0, root)
            });
        assert_eq!(again, chain[1]);
        assert_eq!(chain[8].marks_rev(&db).count(), 8);
    }
}
