//! What a crate reads that its key ([`super::key`]) cannot name, because it is known only once
//! the crate is resolved: the process variables its `env!` and `option_env!` calls read (those
//! the crate's own variables do not have), and what each of its proc-macro expansions read
//! besides its call (files, listings, variables of the shell), as the expansion server traced
//! it.
//!
//! A crate's [`Reads`] are kept beside its image the way a run of user code keeps its shapes
//! (`blaze_usercode::index`): the names it read, without their values, in the first free of
//! [`SLOTS`] slots under the crate's key ([`slot`]), and the image under the key, those names
//! and the values they had ([`image_key`]). Serving asks each slot in turn, works out the
//! values of its names now, and gets the image they name: one get when nothing changed, and an
//! image is never used once a value it was made from is another. A crate that reads nothing of
//! the kind has its image under its key alone and no slot.
//!
//! An expansion whose reads are not known (one its server could not trace, one that read the
//! clock, one that asked the database about a span, one of a macro that keeps a session) makes
//! the crate's image leave out an input, so the crate is refused an image ([`Refused`]).

use std::sync::Arc;

use base_db::{Crate, Db};
use blaze_ids::{ContentId, Hasher};
use hir_expand::{
    HirFileId, MacroCallId, MacroCallKind, MacroDefKind, builtin::env_input_to_key,
    proc_macro::ExpansionReads,
};
use rustc_hash::FxHashSet;

use crate::{
    Lookup,
    deps::serve::{ImageStore, Refused, crate_name},
    nameres::crate_def_map,
};

/// The domain of the slots, the images filed by reads, and the reads' encoding.
pub const READS_DOMAIN: &str = "blaze v2 dependency image reads 1";

/// How many sets of names one key keeps: a crate's expansions read other names only as what
/// they read decides it, so one or two in practice.
pub const SLOTS: u32 = 16;

/// What a crate read besides the inputs of its key: each sorted, with no repeats.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Reads {
    /// The process variables `env!` and `option_env!` read.
    env: Vec<Box<str>>,
    /// Each expansion's names, as its server encodes them, with the hash of their values when
    /// it ran: known for reads made in this database, not for reads decoded from a slot.
    macros: Vec<(Arc<[u8]>, Option<ContentId>)>,
}

impl Reads {
    pub fn is_empty(&self) -> bool {
        self.env.is_empty() && self.macros.is_empty()
    }

    /// The reads of `krate` as it is resolved in `db`: of every macro call of its modules,
    /// `calls` (those its signatures name, [`super::signatures::encode`]), and each call any of
    /// them is in the expansion of; and every process variable the database noted the crate's
    /// `env!`s read, which covers an `env!` in the argument of another eager macro.
    pub(crate) fn of(db: &Db, krate: Crate, calls: &[MacroCallId]) -> Result<Reads, Refused> {
        // alloc: setup: the calls of one crate, walked once as its image is written
        let mut todo: Vec<MacroCallId> = calls.to_vec();
        for (_, module) in crate_def_map(db, krate).modules(db) {
            todo.extend(module.scope(db).all_macro_calls());
        }
        let own = krate.env(db);
        let mut seen = FxHashSet::default();
        let mut reads = Reads::default();
        while let Some(call) = todo.pop() {
            if !seen.insert(call) {
                continue;
            }
            let loc = call.lookup(db);
            if loc.krate != krate {
                continue;
            }
            if let HirFileId::MacroFile(parent) = loc.kind.file_id() {
                todo.push(parent);
            }
            match &loc.def.kind {
                MacroDefKind::ProcMacro(..) => match call.expansion_reads(db) {
                    ExpansionReads::Nothing => {}
                    ExpansionReads::Traced { shape, values } => {
                        // clone: a count of the expansion's names, once per expansion
                        reads.macros.push((shape.clone(), Some(*values)));
                    }
                    &ExpansionReads::Unknown(why) => {
                        // alloc: why a crate is refused, once per refused crate
                        let call = format!("{:?} of crate {}", call, crate_name(db, loc.def.krate));
                        return Err(Refused::ProcMacro { call, why });
                    }
                },
                MacroDefKind::BuiltInEager(_, expander) if expander.is_env_or_option_env() => {
                    if let MacroCallKind::FnLike { eager: Some(eager), .. } = &loc.kind
                        && let Some(name) = env_input_to_key(db, &eager.arg)
                        && own.get(name.as_str()).is_none()
                    {
                        reads.env.push(name.as_str().into());
                    }
                }
                _ => {}
            }
        }
        for (read, name) in db.files().env_read() {
            if read == krate && own.get(&name).is_none() {
                reads.env.push(name.into_boxed_str());
            }
        }
        reads.env.sort_unstable();
        reads.env.dedup();
        reads.macros.sort_unstable();
        reads.macros.dedup();
        if reads.macros.windows(2).any(|pair| pair[0].0 == pair[1].0) {
            return Err(Refused::Reads {
                why: "two expansions read the same names and found other values: an input \
                      changed while the crate was resolved",
            });
        }
        Ok(reads)
    }

    /// The names, as a slot keeps them: the variables, then the expansions' names, each
    /// framed by its length.
    pub fn encode(&self) -> Vec<u8> {
        let len = self.env.iter().map(|it| it.len() + 4).sum::<usize>()
            + self.macros.iter().map(|it| it.0.len() + 4).sum::<usize>();
        // alloc: one slot's bytes, once per crate imaged
        let mut out = Vec::with_capacity(len + 9);
        out.push(VERSION);
        fn framed(out: &mut Vec<u8>, bytes: &[u8]) {
            out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
            out.extend_from_slice(bytes);
        }
        out.extend_from_slice(&(self.env.len() as u32).to_le_bytes());
        for name in &self.env {
            framed(&mut out, name.as_bytes());
        }
        out.extend_from_slice(&(self.macros.len() as u32).to_le_bytes());
        for (shape, _) in &self.macros {
            framed(&mut out, shape);
        }
        out
    }

    /// What [`Reads::encode`] wrote, its lengths checked against the bytes, its order and its
    /// names checked: the bytes came from a store, which checked only their hash.
    pub fn decode(bytes: &[u8]) -> Result<Reads, &'static str> {
        let mut rest = bytes;
        let mut take = |len: usize| -> Result<&[u8], &'static str> {
            if rest.len() < len {
                return Err("reads shorter than their lengths");
            }
            let (taken, after) = rest.split_at(len);
            rest = after;
            Ok(taken)
        };
        let count = |bytes: &[u8]| u32::from_le_bytes(bytes.try_into().expect("four bytes")) as usize;
        if take(1)? != [VERSION] {
            return Err("reads of another version");
        }
        let mut reads = Reads::default();
        let variables = count(take(4)?);
        for _ in 0..variables {
            let len = count(take(4)?);
            let name = std::str::from_utf8(take(len)?).map_err(|_| "a variable not UTF-8")?;
            reads.env.push(name.into()); // alloc: setup: a slot read, once per crate per session
        }
        let macros = count(take(4)?);
        for _ in 0..macros {
            let len = count(take(4)?);
            reads.macros.push((take(len)?.into(), None)); // alloc: setup, as above
        }
        drop(take);
        if !rest.is_empty() {
            return Err("bytes after the reads");
        }
        if !reads.env.windows(2).all(|pair| pair[0] < pair[1]) {
            return Err("variables out of order");
        }
        if !reads.macros.windows(2).all(|pair| pair[0].0 < pair[1].0) {
            return Err("expansions out of order");
        }
        Ok(reads)
    }

    /// The values the reads had as `db` resolved the crate: what the image is filed by.
    pub(crate) fn values_then(&self, db: &Db) -> ContentId {
        self.values(db, |_, then| Ok(then.expect("reads made in this database know their values")))
            .expect("values made in this database do not fail")
    }

    /// The values the reads have now: the process variables in `db`, each expansion's names
    /// worked out again by the driver ([`ImageStore::reads_now`]).
    pub(crate) fn values_now(&self, db: &Db, store: &dyn ImageStore) -> Result<ContentId, String> { // dyn: the driver's store, once per slot read
        self.values(db, |shape, _| store.reads_now(shape))
    }

    fn values(
        &self,
        db: &Db,
        mut expansion: impl FnMut(&[u8], Option<ContentId>) -> Result<ContentId, String>,
    ) -> Result<ContentId, String> {
        let mut hasher = Hasher::new(READS_DOMAIN);
        hasher.part(b"values");
        for name in &self.env {
            match base_db::process_env_var(db, name) {
                None => hasher.update(&[0]),
                Some(value) => hasher.update(&[1]).part(value.as_bytes()),
            };
        }
        for (shape, then) in &self.macros {
            hasher.id(&expansion(shape, *then)?);
        }
        Ok(hasher.finish())
    }
}

/// Slot `n` of the crate whose key is `key`: where the `n`th set of names it read is kept.
pub(crate) fn slot(key: ContentId, n: u32) -> ContentId {
    let mut hasher = Hasher::new(READS_DOMAIN);
    hasher.part(b"slot").id(&key).update(&n.to_le_bytes());
    hasher.finish()
}

/// Where the image of the crate whose key is `key` is, for the reads `record` (a slot's bytes)
/// and the values `values` they have.
pub(crate) fn image_key(key: ContentId, record: &[u8], values: ContentId) -> ContentId {
    let mut hasher = Hasher::new(READS_DOMAIN);
    hasher.part(b"image").id(&key).part(record).id(&values);
    hasher.finish()
}

#[cfg(test)]
impl Reads {
    pub(super) fn for_tests(env: &[&str], macros: &[(&[u8], ContentId)]) -> Reads {
        let mut reads = Reads {
            env: env.iter().map(|&it| it.into()).collect(),
            macros: macros.iter().map(|&(shape, values)| (shape.into(), Some(values))).collect(),
        };
        reads.env.sort_unstable();
        reads.macros.sort_unstable();
        reads
    }
}
