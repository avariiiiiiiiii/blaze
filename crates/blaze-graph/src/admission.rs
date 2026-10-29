//! What a task is expected to hold, for the plane's memory admission (docs/v2/core.md section 4).
//!
//! Every task runs in a bump arena of its own, so what it held at most is measured, not
//! guessed. [`Estimates`] keeps, for each kind of task (a query kind), what tasks of it held,
//! and predicts the next one's peak from it: the [`crate::machine::Spec::memory`] a node is
//! added with. A task over its prediction is let finish and the estimate grows; nothing is
//! cut short or retried. The estimates are kept across runs, in the daemon's image.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// A kind of task whose tasks are alike in what they hold: a query kind's number
/// (`blaze_query::QueryKind`), or one of the plane's own (a link, a test run).
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(transparent)]
pub struct TaskKind(pub u32);

/// What tasks of one kind held.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Estimate {
    /// How many were measured.
    pub seen: u64,
    /// The most any of them held, in bytes.
    pub most: u64,
    /// A high quantile of what they held, in bytes: what a new task is predicted to hold.
    pub predicted: u64,
}

/// Each kind's [`Estimate`], learned as tasks finish.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq)]
pub struct Estimates {
    pub by_kind: BTreeMap<TaskKind, Estimate>,
    /// What a task of a kind never measured is predicted to hold.
    pub unseen: u64,
}

impl Estimates {
    /// The peak a task of `kind` is predicted to hold, in bytes.
    pub fn predict(&self, kind: TaskKind) -> u64 {
        let _ = kind;
        unimplemented!("v2: predicting a task's peak from its kind's estimate (v2-core)")
    }

    /// A task of `kind` finished having held `peak` bytes at most.
    pub fn observe(&mut self, kind: TaskKind, peak: u64) {
        let _ = (kind, peak);
        unimplemented!("v2: learning a kind's estimate from a measured peak (v2-core)")
    }
}

/// A task's arena: where everything the task allocates while it runs goes, so its peak is
/// measured and its results, once written into the zero-copy format, outlive it.
pub trait Arena {
    /// The most bytes it held at once.
    fn peak(&self) -> u64;
    /// The bytes it holds now.
    fn held(&self) -> u64;
}
