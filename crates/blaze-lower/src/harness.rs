//! A crate root as the compiler sees it, which is not the file on disk: the root's text with a
//! module the compiler appends after it, a test crate's harness or a proc macro's list of its
//! macros ([`Lowered::append_roots`]).
//! What the module is, is the caller's: this crate writes it into the front end and keeps one
//! root file one text for every crate that has it.

use base_db::{Crate, FileText};
use hir_expand::proc_macro::ProcMacros;
use rustc_hash::FxHashSet;
use blaze_query::db::Durability;

use super::{ContentId, Db, LowerError, Lowered};

/// What a crate's root file is to be: `text`, the file on disk with the module named `module`
/// after it.
#[derive(Debug)]
pub struct Appended {
    pub text: String,
    pub module: &'static str,
}

impl Lowered {
    /// The root file of the crate of each of `units` made what `wanted` says it is for that
    /// crate (`None`: left as it is), with `Durability::LOW`, and the module it ends with set as
    /// the root's appended module, so a root that `#![cfg]` takes away still has it. Both are
    /// in the file's content ID, so the identity of every crate of that root follows. Returns
    /// how many roots were written: none when each already is what is wanted.
    ///
    /// One root file is one text. Units whose crates share a root (a lib's test of two
    /// targets, a check of tests beside a test) must want the same, or are refused naming
    /// both: in one call, and across calls until the next [`Lowered::refresh`], so a root
    /// made for one unit is never changed under it for another. A unit given again (once its
    /// proc macros load, [`macros_loaded`]) may make its root again, and so may the others of
    /// that root given with it. A crate of the same root that is not given here (the lib
    /// beside its test) reads the appended module too, which the caller's module leaves out by
    /// its own `#[cfg]`.
    ///
    /// A write, as [`Lowered::ran`] is: call it before [`Lowered::identity`] of these units.
    pub fn append_roots(
        &mut self,
        units: impl IntoIterator<Item = u32>,
        mut wanted: impl FnMut(&Db, usize, Crate) -> Result<Option<Appended>, LowerError>,
    ) -> Result<usize, LowerError> {
        // alloc: once a call, the units given and the roots made in it
        let units: Vec<u32> = units.into_iter().collect();
        let mut made: FxHashSet<base_db::FileId> = FxHashSet::default();
        let mut written = 0;
        for &unit in &units {
            let unit = unit as usize;
            let Some(krate) = self.crates.krate(unit) else { continue };
            let Some(appended) = wanted(&self.db, unit, krate)? else { continue };
            let file = krate.data(&self.db).root_file_id;
            let content = FileText::content_of(&appended.text, Some(appended.module));
            match self.appended.get(&file) {
                Some(&(_, was)) if was == content => continue,
                Some(&(by, _)) if made.contains(&file) || !units.contains(&(by as u32)) => {
                    return Err(self.two_roots(by, unit, file));
                }
                _ => {}
            }
            made.insert(file);
            self.appended.insert(file, (unit, content));
            let input = self.db.file_text(file);
            if input.content(&self.db) == content {
                continue;
            }
            if input.appended_root_module(&self.db) != Some(appended.module) {
                self.db.set_appended_root_module(file, Some(appended.module), Durability::LOW);
            }
            if **input.text(&self.db) != *appended.text {
                self.db.set_file_text_with_durability(file, &appended.text, Durability::LOW);
            }
            written += 1;
        }
        if written > 0 {
            self.identities.clear();
        }
        Ok(written)
    }

    fn two_roots(&self, first: usize, second: usize, file: base_db::FileId) -> LowerError {
        let (first, second) = (&self.crates.units[first].what, &self.crates.units[second].what);
        // alloc: the error's place and message
        let at = self.paths.get(&file).cloned().unwrap_or_else(|| format!("{file:?}"));
        LowerError::at(
            // alloc: as `at`
            format!(
                "{first} and {second} share the root file `{at}` and would each have it end with \
                 another module (a test harness, a proc macro's list), which one file can't"
            ),
            at,
        )
    }
}

/// Whether every proc macro `krate` reaches through its dependencies has its expanders loaded
/// ([`Lowered::macros`]): until then what its attributes and derives expand to (a
/// `#[tokio::test]`'s `#[test]`) is not in its items, so nothing listing them is made yet. A
/// proc macro that failed to load never is: the crate's build stops on it.
pub fn macros_loaded(db: &Db, krate: Crate) -> bool {
    let by_crate = ProcMacros::get(db).by_crate(db);
    // alloc: once a test crate a write, its closure walked
    let mut seen: FxHashSet<Crate> = FxHashSet::default();
    let mut stack = vec![krate]; // alloc: as `seen`
    while let Some(at) = stack.pop() {
        for dep in &at.data(db).dependencies {
            if !seen.insert(dep.crate_id) {
                continue;
            }
            if by_crate.get(&dep.crate_id).is_some_and(|it| it.macros().is_none()) {
                return false;
            }
            stack.push(dep.crate_id);
        }
    }
    true
}
