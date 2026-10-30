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
use crate::{Image, LinkError};

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

/// A commit or rollback in flight.
pub(crate) struct Pending {
    steps: Steps,
    /// The step in flight: its first tag's low half, how many effects, which are done.
    from: u32,
    count: u32,
    done: Vec<u64>,
    left: u32,
    failed: Option<LinkError>,
    /// What the image goes back to if an effect fails before the rename: its state, and
    /// what becomes of the history.
    revert: Option<(State, History)>,
}

/// What a failed commit or rollback does to the history.
pub(crate) enum History {
    /// Drops the undo the commit pushed.
    Pop,
    /// Puts back the undo the rollback took.
    Push(Undo),
    /// Leaves it: the first commit pushed nothing.
    Keep,
}

/// How many commits back a rollback can go.
pub(crate) const HISTORY: usize = 16;

/// The file writes of one commit, and the message for running programs.
pub(crate) struct Writes {
    pub full: bool,
    pub len: u64,
    pub old_len: u64,
    pub writes: Vec<(u64, Vec<u8>)>,
    /// Message files to remove first, and the encoded one to write last.
    pub remove: Vec<u64>,
    pub message: Option<(u64, Vec<u8>)>,
}

impl Image {
    fn tag(&mut self) -> Tag {
        self.tags = self.tags.wrapping_add(1);
        Tag(self.output.tags | u64::from(self.tags))
    }

    fn message_name(&self, generation: u64) -> Option<FileName> {
        let live = self.output.live.as_ref()?;
        FileName::new(&format!("{}/{}", live.dir.as_str(), message_file(generation)))
    }

    pub(crate) fn steps(&mut self, writes: Writes) -> Result<Steps, LinkError> {
        let to = self.to.clone();
        let next = self.next.clone();
        let removes = writes.remove.len();
        let mut steps = Steps {
            effects: VecDeque::with_capacity(writes.writes.len() + removes + 7),
            counts: VecDeque::with_capacity(9),
            first: self.tags.wrapping_add(1),
        };
        let step = |steps: &mut Steps, effects: &mut dyn Iterator<Item = Effect>| {
            let before = steps.effects.len();
            steps.effects.extend(effects);
            let count = (steps.effects.len() - before) as u32;
            if count != 0 {
                steps.counts.push_back(count);
            }
        };
        let tag = self.tag();
        step(&mut steps, &mut std::iter::once(Effect::Remove { file: next.clone(), tag }));
        if writes.full {
            let tag = self.tag();
            step(&mut steps, &mut std::iter::once(Effect::Truncate { file: next.clone(), len: writes.len, tag }));
        } else {
            let tag = self.tag();
            step(&mut steps, &mut std::iter::once(Effect::Clone { from: to.clone(), to: next.clone(), tag }));
            if writes.len != writes.old_len {
                let tag = self.tag();
                step(&mut steps, &mut std::iter::once(Effect::Truncate { file: next.clone(), len: writes.len, tag }));
            }
        }
        let mut writes_of = writes.writes.into_iter().map(|(at, bytes)| Effect::WriteAt {
            file: next.clone(),
            at,
            bytes,
            tag: self.tag(),
        });
        step(&mut steps, &mut writes_of);
        drop(writes_of);
        let tag = self.tag();
        step(&mut steps, &mut std::iter::once(Effect::Sync { file: next.clone(), tag }));
        let tag = self.tag();
        step(&mut steps, &mut std::iter::once(Effect::Mode { file: next.clone(), mode: 0o755, tag }));
        let tag = self.tag();
        step(&mut steps, &mut std::iter::once(Effect::Rename { from: next, to, tag }));
        let mut files = Vec::with_capacity(removes);
        for generation in writes.remove {
            files.push(self.message_name(generation).ok_or_else(live_dir_error)?);
        }
        let mut removes = files.into_iter().map(|file| Effect::Remove { file, tag: self.tag() });
        step(&mut steps, &mut removes);
        drop(removes);
        if let Some((generation, bytes)) = writes.message {
            let file = self.message_name(generation).ok_or_else(live_dir_error)?;
            let tag = self.tag();
            step(&mut steps, &mut std::iter::once(Effect::Write { file, bytes, tag }));
        }
        Ok(steps)
    }

    pub(crate) fn begin(&mut self, steps: Steps, revert: (State, History), effects: &mut Vec<Effect>) {
        let from = steps.first;
        self.pending = Some(Pending {
            steps,
            from,
            count: 0,
            done: Vec::new(),
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
                    format!("writing {}: {why}", self.to)
                } else {
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
            if let Some((state, history)) = pending.revert {
                self.state = state;
                match history {
                    History::Push(undo) => self.remember(undo),
                    History::Pop => {
                        self.history.pop_back();
                    }
                    History::Keep => {}
                }
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
    LinkError { function: None, what: "the live directory names no file under the driver's root".into() }
}
