//! The commit: the one serial step. A stage's writes go to a copy of the image (a clone that
//! shares its blocks, or a new file for a full link), which is synced and renamed over the
//! image, so a reader or a crash sees the old image or the new; then running programs are
//! told. Effects complete in any order: each step waits for all of its own.
//!
//! The effects of every step are one queue, cut by each step's count, and their tags are
//! given out one after another, so the effects in flight are a range of tags and a bitset
//! of those done: no set, no per-step vector.

#![forbid(unsafe_code)]

use std::collections::VecDeque;

use blaze_core::{Completion, Done, Effect, FileName, Tag};
use blaze_live::patch::message_file;

use crate::state::State;
use crate::{Image, LinkError, Output};

/// A state the image can go back to: the state before a commit, and the ranges of its file
/// the commit overwrote (read from `state` when rolling back).
pub(crate) struct Undo {
    pub state: State,
    pub full: bool,
    pub len: u64,
    pub writes: Vec<(u64, u64)>,
}

/// The effects of a commit or rollback, step by step.
pub(crate) struct Steps {
    effects: VecDeque<Effect>,
    /// How many effects each step has, in order.
    counts: VecDeque<u32>,
    /// The low half of the first effect's tag.
    first: u32,
}

impl Steps {
    /// Adds a step of `effects`; a step of none is left out.
    fn step(&mut self, effects: impl IntoIterator<Item = Effect>) {
        let before = self.effects.len();
        self.effects.extend(effects);
        let count = (self.effects.len() - before) as u32;
        if count != 0 {
            self.counts.push_back(count);
        }
    }
}

/// A commit or rollback in flight.
pub(crate) struct Pending {
    steps: Steps,
    /// The step in flight: its first tag's low half, how many effects, which are done.
    from: u32,
    count: u32,
    done: Vec<u64>,
    left: u32,
    failed: Option<LinkError>,
    /// What the image goes back to if an effect fails before the rename.
    revert: Option<Revert>,
}

/// What a failed commit or rollback puts back.
pub(crate) enum Revert {
    /// A commit: the state before it is the undo it pushed, the history's last.
    Pop,
    /// A rollback: the state it left, and the undo it took (whose state the image now has).
    Push { state: State, full: bool, len: u64, writes: Vec<(u64, u64)> },
    /// The first commit, which pushed nothing: the state before it.
    Keep(State),
}

/// How many commits back a rollback can go.
pub(crate) const HISTORY: usize = 16;

/// The file writes of one commit, and the message for running programs.
pub(crate) struct Writes {
    pub full: bool,
    pub len: u64,
    pub old_len: u64,
    pub writes: Vec<(u64, Vec<u8>)>,
    /// The message file to remove first (a rollback's: the generation it undoes), and the
    /// encoded one to write last.
    pub remove: Option<u64>,
    pub message: Option<(u64, Vec<u8>)>,
}

/// The image's tags, given out one after another: the high half is the output's.
struct Tags<'t> {
    high: u64,
    low: &'t mut u32,
}

impl Tags<'_> {
    fn next(&mut self) -> Tag {
        *self.low = self.low.wrapping_add(1);
        Tag(self.high | u64::from(*self.low))
    }
}

/// A copy of a file name for an effect.
fn owned(name: &FileName) -> FileName {
    // clone: each effect owns its file name (`blaze_core::Effect`), a few per commit
    name.clone()
}

fn message_name(output: &Output, generation: u64) -> Option<FileName> {
    let live = output.live.as_ref()?;
    // alloc: the message file's name, once per commit; the effect owns it
    FileName::new(&format!("{}/{}", live.dir.as_str(), message_file(generation)))
}

impl Image {
    pub(crate) fn steps(&mut self, writes: Writes) -> Result<Steps, LinkError> {
        let mut tags = Tags { high: self.output.tags, low: &mut self.tags };
        let (to, next) = (&self.to, &self.next);
        let mut steps = Steps {
            effects: VecDeque::with_capacity(writes.writes.len() + 8),
            counts: VecDeque::with_capacity(9),
            first: tags.low.wrapping_add(1),
        };
        steps.step([Effect::Remove { file: owned(next), tag: tags.next() }]);
        if writes.full {
            steps.step([Effect::Truncate { file: owned(next), len: writes.len, tag: tags.next() }]);
        } else {
            steps.step([Effect::Clone { from: owned(to), to: owned(next), tag: tags.next() }]);
            if writes.len != writes.old_len {
                steps.step([Effect::Truncate { file: owned(next), len: writes.len, tag: tags.next() }]);
            }
        }
        steps.step(
            writes.writes.into_iter().map(|(at, bytes)| Effect::WriteAt { file: owned(next), at, bytes, tag: tags.next() }),
        );
        steps.step([Effect::Sync { file: owned(next), tag: tags.next() }]);
        steps.step([Effect::Mode { file: owned(next), mode: 0o755, tag: tags.next() }]);
        steps.step([Effect::Rename { from: owned(next), to: owned(to), tag: tags.next() }]);
        if let Some(generation) = writes.remove {
            let file = message_name(&self.output, generation).ok_or_else(live_dir_error)?;
            steps.step([Effect::Remove { file, tag: tags.next() }]);
        }
        if let Some((generation, bytes)) = writes.message {
            let file = message_name(&self.output, generation).ok_or_else(live_dir_error)?;
            steps.step([Effect::Write { file, bytes, tag: tags.next() }]);
        }
        Ok(steps)
    }

    pub(crate) fn begin(&mut self, steps: Steps, revert: Revert, effects: &mut Vec<Effect>) {
        let from = steps.first;
        let most = steps.counts.iter().copied().max().unwrap_or(0) as usize;
        self.pending = Some(Pending {
            steps,
            from,
            count: 0,
            done: Vec::with_capacity(most.div_ceil(64)),
            left: 0,
            failed: None,
            revert: Some(revert),
        });
        self.next_step(effects);
    }

    fn next_step(&mut self, effects: &mut Vec<Effect>) {
        let pending = self.pending.as_mut().expect("a commit in flight");
        let Some(count) = pending.steps.counts.pop_front() else { return };
        pending.from = pending.from.wrapping_add(pending.count);
        pending.count = count;
        pending.left = count;
        pending.done.clear();
        pending.done.resize((count as usize).div_ceil(64), 0);
        effects.extend(pending.steps.effects.drain(..count as usize));
    }

    pub(crate) fn remember(&mut self, undo: Undo) {
        self.history.push_back(undo);
        if self.history.len() > HISTORY {
            self.history.pop_front();
        }
    }

    /// Of a tag, its place in the step in flight, if it is one of its effects not yet done.
    fn in_flight(&self, tag: Tag) -> Option<usize> {
        let pending = self.pending.as_ref()?;
        if tag.0 & !0xffff_ffff != self.output.tags {
            return None;
        }
        let at = (tag.0 as u32).wrapping_sub(pending.from);
        if at >= pending.count {
            return None;
        }
        let at = at as usize;
        (pending.done[at / 64] & (1 << (at % 64)) == 0).then_some(at)
    }

    /// Whether `tag` is one of this image's effects in flight.
    pub fn owns(&self, tag: Tag) -> bool {
        self.in_flight(tag).is_some()
    }

    /// Takes the completion of one of the image's effects ([`Image::owns`]); asks for the
    /// next step's effects. When the commit or rollback is over, its result: the
    /// generation the image is at, or why it failed (the image is then as it was before).
    pub fn complete(&mut self, completion: &Completion, effects: &mut Vec<Effect>) -> Option<Result<u64, LinkError>> {
        let at = self.in_flight(completion.tag)?;
        let pending = self.pending.as_mut().expect("in flight");
        pending.done[at / 64] |= 1 << (at % 64);
        pending.left -= 1;
        match &completion.done {
            Done::Failed(why) => {
                let what = if pending.revert.is_some() {
                    // alloc: error path
                    format!("writing {}: {why}", self.to)
                } else {
                    // alloc: error path
                    format!("{} is written, but running programs were not told: {why}", self.to)
                };
                pending.failed.get_or_insert(LinkError { function: None, what });
            }
            Done::Renamed => pending.revert = None,
            _ => {}
        }
        if pending.left != 0 {
            return None;
        }
        if let Some(failed) = pending.failed.take() {
            let pending = self.pending.take().expect("in flight");
            match pending.revert {
                Some(Revert::Pop) => {
                    let undo = self.history.pop_back().expect("the commit pushed its undo");
                    self.state = undo.state;
                }
                Some(Revert::Push { state, full, len, writes }) => {
                    let undone = std::mem::replace(&mut self.state, state);
                    self.remember(Undo { state: undone, full, len, writes });
                }
                Some(Revert::Keep(state)) => self.state = state,
                None => {}
            }
            return Some(Err(failed));
        }
        if pending.steps.counts.is_empty() {
            self.pending = None;
            return Some(Ok(self.state.generation));
        }
        self.next_step(effects);
        None
    }
}

fn live_dir_error() -> LinkError {
    // alloc: error path
    LinkError { function: None, what: "the live directory names no file under the driver's root".into() }
}
