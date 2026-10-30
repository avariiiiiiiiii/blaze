//! The masked IR of every function a daemon's builds have folded (#1292,
//! docs/v2/plan/lanes/fastcompile/hashcons.md): each by the key it is stored under, its form
//! hash-consed. A function unchanged since an earlier build is neither read from the store
//! nor hashed again; a new one's form is hashed once, when it is interned.
//!
//! A stored key names the masked IR as the store's own entry under [`crate::query::keys::masked`]
//! does: the instance key holds its code, type arguments and settings, and the functions it
//! builds in. Append-only: reads take no lock and write nothing.

use std::sync::LazyLock;

use blaze_ids::{Idx, Interner, Sharded};

use crate::{
    InstanceKey,
    fold::{Constant, Form, Forms, Masked, Masking},
};

/// A function's masked IR as kept: its form and its constants' values.
struct Known {
    form: Idx<Form>,
    constants: Box<[Constant]>,
}

pub struct Masks {
    forms: Forms,
    /// By stored key; `None` for a function its backend can't fold.
    by_key: Sharded<Option<Known>>,
}

impl Default for Masks {
    fn default() -> Masks {
        Masks::new()
    }
}

impl Masks {
    pub fn new() -> Masks {
        Masks { forms: Forms::new(), by_key: Sharded::new() }
    }

    /// The masks every build of this process keeps.
    pub fn shared() -> &'static Masks {
        static SHARED: LazyLock<Masks> = LazyLock::new(Masks::new);
        &SHARED
    }

    /// How many distinct forms are kept.
    pub fn forms(&self) -> usize {
        self.forms.len()
    }

    fn masking(&self, idx: Idx<Option<Known>>) -> Option<Masking<'_>> {
        let known = self.by_key.get(idx).as_ref()?;
        Some(Masking { form: self.forms.handle(known.form), constants: &known.constants })
    }

    /// What is kept for the function stored under `stored`: `None` if nothing is, `Some(None)`
    /// if it can't be folded. Hashes nothing.
    pub(super) fn get(&self, stored: &InstanceKey) -> Option<Option<Masking<'_>>> {
        self.by_key.find(&stored.0).map(|idx| self.masking(idx))
    }

    /// Keeps `masked` for the function stored under `stored`, interning its form: hashed
    /// here, unless a racing build kept it first.
    pub(super) fn put(&self, stored: &InstanceKey, masked: Option<Masked>) -> Option<Masking<'_>> {
        let idx = self.by_key.intern(stored.0, || {
            masked.map(|it| {
                let (form, constants) = it.into_parts();
                // alloc: once per function the daemon first folds
                Known { form: self.forms.intern(form).idx(), constants: constants.into_boxed_slice() }
            })
        });
        self.masking(idx)
    }
}
