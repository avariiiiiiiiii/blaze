//! The commit: the one serial step. A stage's writes go to a copy of the image (a clone that
//! shares its blocks, or a new file for a full link), which is synced and renamed over the
//! image, so a reader or a crash sees the old image or the new; then running programs are
//! told. Effects complete in any order: each step waits for all of its own.

use std::collections::VecDeque;

use blaze_core::{Completion, Done, Effect, FileName, Tag};
use blaze_live::patch::{Message, message_file};
use rustc_hash::FxHashSet;

use crate::state::State;
use crate::{Image, LinkError};

/// A state the image can go back to: the state before a commit, and the file's bytes it
/// changed.
pub(crate) struct Undo {
    pub state: State,
    pub full: bool,
    pub len: u64,
    pub writes: Vec<(u64, Vec<u8>)>,
}

/// A commit or rollback in flight.
pub(crate) struct Pending {
    steps: VecDeque<Vec<Effect>>,
    waiting: FxHashSet<u64>,
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
const HISTORY: usize = 16;

/// The file writes of one commit, and the message for running programs.
pub(crate) struct Writes {
    pub full: bool,
    pub len: u64,
    pub old_len: u64,
    pub writes: Vec<(u64, Vec<u8>)>,
    /// Message files to remove first, and the one to write last.
    pub remove: Vec<u64>,
    pub message: Option<(u64, Message)>,
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

    pub(crate) fn steps(&mut self, writes: Writes) -> Result<VecDeque<Vec<Effect>>, LinkError> {
        let to = self.to.clone();
        let next = self.next.clone();
        let mut steps = VecDeque::new();
        steps.push_back(vec![Effect::Remove { file: next.clone(), tag: self.tag() }]);
        if writes.full {
            steps.push_back(vec![Effect::Truncate { file: next.clone(), len: writes.len, tag: self.tag() }]);
        } else {
            steps.push_back(vec![Effect::Clone { from: to.clone(), to: next.clone(), tag: self.tag() }]);
            if writes.len != writes.old_len {
                steps.push_back(vec![Effect::Truncate { file: next.clone(), len: writes.len, tag: self.tag() }]);
            }
        }
        let mut step = Vec::new();
        for (at, bytes) in writes.writes {
            step.push(Effect::WriteAt { file: next.clone(), at, bytes, tag: self.tag() });
        }
        if !step.is_empty() {
            steps.push_back(step);
        }
        steps.push_back(vec![Effect::Sync { file: next.clone(), tag: self.tag() }]);
        steps.push_back(vec![Effect::Mode { file: next.clone(), mode: 0o755, tag: self.tag() }]);
        steps.push_back(vec![Effect::Rename { from: next, to, tag: self.tag() }]);
        let mut remove = Vec::new();
        for generation in writes.remove {
            let file = self.message_name(generation).ok_or_else(|| live_dir_error())?;
            remove.push(Effect::Remove { file, tag: self.tag() });
        }
        if !remove.is_empty() {
            steps.push_back(remove);
        }
        if let Some((generation, message)) = writes.message {
            let file = self.message_name(generation).ok_or_else(|| live_dir_error())?;
            steps.push_back(vec![Effect::Write { file, bytes: message.encode(), tag: self.tag() }]);
        }
        Ok(steps)
    }

    pub(crate) fn begin(&mut self, steps: VecDeque<Vec<Effect>>, revert: (State, History), effects: &mut Vec<Effect>) {
        self.pending = Some(Pending { steps, waiting: FxHashSet::default(), failed: None, revert: Some(revert) });
        self.next_step(effects);
    }

    fn next_step(&mut self, effects: &mut Vec<Effect>) {
        let pending = self.pending.as_mut().expect("a commit in flight");
        if let Some(step) = pending.steps.pop_front() {
            for effect in &step {
                pending.waiting.insert(effect.tag().0);
            }
            effects.extend(step);
        }
    }

    pub(crate) fn remember(&mut self, undo: Undo) {
        self.history.push(undo);
        if self.history.len() > HISTORY {
            self.history.remove(0);
        }
    }

    /// Whether `tag` is one of this image's effects in flight.
    pub fn owns(&self, tag: Tag) -> bool {
        self.pending.as_ref().is_some_and(|it| it.waiting.contains(&tag.0))
    }

    /// Takes the completion of one of the image's effects ([`Image::owns`]); asks for the
    /// next step's effects. When the commit or rollback is over, its result: the
    /// generation the image is at, or why it failed (the image is then as it was before).
    pub fn complete(&mut self, completion: &Completion, effects: &mut Vec<Effect>) -> Option<Result<u64, LinkError>> {
        let pending = self.pending.as_mut()?;
        if !pending.waiting.remove(&completion.tag.0) {
            return None;
        }
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
        if !pending.waiting.is_empty() {
            return None;
        }
        if let Some(failed) = pending.failed.take() {
            let pending = self.pending.take().expect("in flight");
            if let Some((state, history)) = pending.revert {
                self.state = state;
                match history {
                    History::Push(undo) => self.remember(undo),
                    History::Pop => {
                        self.history.pop();
                    }
                    History::Keep => {}
                }
            }
            return Some(Err(failed));
        }
        if pending.steps.is_empty() {
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
