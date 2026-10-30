//! The unwind paths lowering writes, each folded once per change of what it drops: rustc's
//! cached unwind block per scope (`Scope::cached_unwind_block`, `rustc_mir_build`'s
//! `builder/scope.rs`).
//!
//! The way out of a point that may panic drops what every scope holds there, innermost first. It
//! is a fold over the scopes' locals, outermost first, through `unwind_node`, whose memo makes
//! each step's block depend only on the block before it, the local and the kind of step. So each
//! scope keeps the block its fold ended at, and only the scopes that changed since are folded
//! again. A fold over a prefix that has not changed allocates no block, then or when it was first
//! folded, so blocks come out in the order the fold over everything at every ask gives them.
//!
//! A scope's fold depends on its locals and tail mark (`ScopeLocals`, which marks itself stale
//! on every change), on which of them are pending operands (`operands`) and which are
//! unscheduled temporaries (`unscheduled`). A change of either set goes through `Unwinds`, which
//! marks stale the outermost listed scope holding the local; everything inside a stale scope is
//! listed and folded again.

use std::ops::Deref;

use blaze_mem::Frame;

use crate::mir::{
    BasicBlockId, LocalId,
    framed::local_index,
};

use super::{DropScope, MirLowerCtx};

/// No position: the local is in no listed scope.
const NONE: u32 = u32::MAX;

/// A `ScopeLocals::bound` that has to be counted again.
const UNKNOWN: u32 = u32::MAX;

/// A set of locals: a bit per local index, in the frame, as long as the largest local put in.
pub(super) struct LocalSet<'f> {
    words: Vec<u64, &'f Frame>,
}

impl<'f> LocalSet<'f> {
    pub(super) fn new_in(frame: &'f Frame) -> Self {
        LocalSet { words: Vec::new_in(frame) }
    }

    pub(super) fn contains(&self, local: &LocalId) -> bool {
        let at = local_index(*local);
        self.words.get(at / 64).is_some_and(|&word| (word >> (at % 64)) & 1 == 1)
    }

    /// Whether `local` was not in the set.
    fn insert(&mut self, local: LocalId) -> bool {
        let at = local_index(local);
        if self.words.len() <= at / 64 {
            self.words.resize(at / 64 + 1, 0);
        }
        let word = &mut self.words[at / 64];
        let bit = 1u64 << (at % 64);
        let new = *word & bit == 0;
        *word |= bit;
        new
    }

    /// Whether `local` was in the set.
    fn remove(&mut self, local: LocalId) -> bool {
        let at = local_index(local);
        let Some(word) = self.words.get_mut(at / 64) else { return false };
        let bit = 1u64 << (at % 64);
        let had = *word & bit != 0;
        *word &= !bit;
        had
    }
}

/// The locals of a drop scope in the order they were declared, which is the reverse of the order
/// they are dropped in, and its tail mark. Reads go through `Deref`; every change goes through
/// `edit`, `push_new` or `set_tail`, which mark the scope for `Unwinds` to list and fold again.
#[derive(Debug)]
pub(super) struct ScopeLocals<'f> {
    list: Vec<LocalId, &'f Frame>,
    /// While the tail of a block before the 2024 edition is lowered: how many locals the block
    /// had declared before it. What the tail declares that is not an operand outlives the
    /// block's locals (`outlive_block`), also when the tail panics or leaves early.
    tail: Option<usize>,
    /// Changed since `Unwinds` last listed it.
    stale: bool,
    /// One more than the largest local index in `list`, or `UNKNOWN` after an `edit`: a local
    /// at or above it is not in the scope.
    bound: u32,
}

impl<'f> ScopeLocals<'f> {
    pub(super) fn new_in(frame: &'f Frame) -> Self {
        ScopeLocals { list: Vec::new_in(frame), tail: None, stale: true, bound: 0 }
    }

    pub(super) fn from_slice_in(locals: &[LocalId], frame: &'f Frame) -> Self {
        let mut list = Vec::with_capacity_in(locals.len(), frame);
        list.extend_from_slice(locals);
        ScopeLocals { list, tail: None, stale: true, bound: UNKNOWN }
    }

    /// The list, to change.
    pub(super) fn edit(&mut self) -> &mut Vec<LocalId, &'f Frame> {
        self.stale = true;
        self.bound = UNKNOWN;
        &mut self.list
    }

    /// Declares `local` last.
    pub(super) fn push_new(&mut self, local: LocalId) {
        self.stale = true;
        if self.bound != UNKNOWN {
            self.bound = self.bound.max(local_index(local) as u32 + 1);
        }
        self.list.push(local);
    }

    /// Whether `local` is in the scope. A local declared after everything in it, which a new
    /// temporary always is, is answered without a scan.
    pub(super) fn holds(&mut self, local: LocalId) -> bool {
        if self.bound == UNKNOWN {
            self.bound = self.list.iter().map(|&it| local_index(it) as u32 + 1).max().unwrap_or(0);
        }
        local_index(local) < self.bound as usize && self.list.contains(&local)
    }

    pub(super) fn tail(&self) -> Option<usize> {
        self.tail
    }

    pub(super) fn set_tail(&mut self, tail: Option<usize>) {
        self.stale = true;
        self.tail = tail;
    }

    /// The block's own locals and what its tail declared, when a tail is being lowered.
    fn parts(&self) -> (&[LocalId], &[LocalId]) {
        match self.tail {
            Some(declared) if declared < self.list.len() => self.list.split_at(declared),
            _ => (&self.list, &[]),
        }
    }

    /// Appends the locals in the order they were declared. The temporaries of a tail before the
    /// 2024 edition come before the block's locals, as `outlive_block` leaves them once the tail
    /// is lowered; the tail's operands stay last.
    pub(super) fn declared_into(&self, operands: &LocalSet<'_>, out: &mut Vec<LocalId, &Frame>) {
        let (head, tail) = self.parts();
        if tail.is_empty() {
            out.extend_from_slice(head);
            return;
        }
        out.extend(tail.iter().filter(|it| !operands.contains(it)));
        out.extend_from_slice(head);
        out.extend(tail.iter().filter(|it| operands.contains(it)));
    }

    /// Appends the locals in the reverse of the order leaving the scope early drops them in:
    /// `declared_into`'s order with the pending operands moved to the end, each part in its
    /// order. rustc keeps the operands of an expression being lowered in that expression's own
    /// scope, inside the temporaries that live to the end of the statement.
    pub(super) fn leaving_into(&self, operands: &LocalSet<'_>, out: &mut Vec<LocalId, &Frame>) {
        let (head, tail) = self.parts();
        out.extend(tail.iter().filter(|it| !operands.contains(it)));
        out.extend(head.iter().filter(|it| !operands.contains(it)));
        out.extend(head.iter().filter(|it| operands.contains(it)));
        out.extend(tail.iter().filter(|it| operands.contains(it)));
    }
}

impl Clone for ScopeLocals<'_> {
    /// A copy is a scope `Unwinds` has not listed.
    fn clone(&self) -> Self {
        ScopeLocals { list: self.list.clone(), tail: self.tail, stale: true, bound: self.bound }
    }
}

impl Deref for ScopeLocals<'_> {
    type Target = [LocalId];

    fn deref(&self) -> &[LocalId] {
        &self.list
    }
}

/// A scope on the stack, as `Unwinds` reads it.
pub(super) trait HasLocals<'f> {
    fn locals_mut(&mut self) -> &mut ScopeLocals<'f>;
}

impl<'f> HasLocals<'f> for DropScope<'f> {
    fn locals_mut(&mut self) -> &mut ScopeLocals<'f> {
        &mut self.locals
    }
}

impl<'f> HasLocals<'f> for ScopeLocals<'f> {
    fn locals_mut(&mut self) -> &mut ScopeLocals<'f> {
        self
    }
}

/// What a fold needs of lowering: the unwind drop tree's nodes, and whether a local's value is
/// dropped on the way out.
pub(super) trait DropTree {
    /// The node that drops the value of `local` (`value`) or ends its storage, then goes on to
    /// `parent`, or to resuming the unwind when there is none.
    fn node(&mut self, parent: Option<BasicBlockId>, local: LocalId, value: bool) -> BasicBlockId;
    /// Whether the type of `local` is `Copy`, which nothing drops. Asked once a local.
    fn local_is_copy(&mut self, local: LocalId) -> bool;
    /// Whether `local` is a temporary its expression is being written to, which unwinding out
    /// of that expression does not drop.
    fn unscheduled(&self, local: LocalId) -> bool;
}

/// Which of the two orders a fold goes through.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Chain {
    /// `ScopeLocals::leaving_into`: unwinding from a statement or a call, or out of a drop on
    /// the way out of `break`, `return` and the rest.
    Leaving,
    /// `ScopeLocals::declared_into`: out of a drop at the end of a scope.
    Declared,
}

/// A listed scope: where its locals start in both orders, and the block its fold ends at in
/// each, once folded.
#[derive(Clone, Copy)]
struct Link {
    start: u32,
    leaving_at: Option<BasicBlockId>,
    declared_at: Option<BasicBlockId>,
}

impl Link {
    fn at(&self, chain: Chain) -> Option<BasicBlockId> {
        match chain {
            Chain::Leaving => self.leaving_at,
            Chain::Declared => self.declared_at,
        }
    }
}

/// The last fold past a chain's link: from `base`, through `seq`, with `at[i]` the block after
/// the first `i` of them. It holds while no local's dropping changed (`epoch`).
struct Memo<'f> {
    epoch: u32,
    base: Option<BasicBlockId>,
    seq: Vec<LocalId, &'f Frame>,
    at: Vec<Option<BasicBlockId>, &'f Frame>,
}

impl<'f> Memo<'f> {
    fn new_in(frame: &'f Frame) -> Self {
        Memo { epoch: 0, base: None, seq: Vec::new_in(frame), at: Vec::new_in(frame) }
    }
}

/// The locals a fold past a link goes through.
#[derive(Clone, Copy)]
enum Seq<'s> {
    /// `declared[start..end]`.
    Declared { start: usize, end: usize },
    Given(&'s [LocalId]),
}

impl Seq<'_> {
    fn len(&self) -> usize {
        match *self {
            Seq::Declared { start, end } => end - start,
            Seq::Given(it) => it.len(),
        }
    }
}

/// The scopes as last listed, in both orders, and how far each order's links are folded.
pub(super) struct Unwinds<'f> {
    /// Every listed scope's locals in `declared_into`'s order, scope after scope, outermost
    /// first.
    declared: Vec<LocalId, &'f Frame>,
    /// The same in `leaving_into`'s order.
    leaving: Vec<LocalId, &'f Frame>,
    links: Vec<Link, &'f Frame>,
    leaving_folded: usize,
    declared_folded: usize,
    /// By local index: its first position in `declared`, or `NONE`.
    first: Vec<u32, &'f Frame>,
    /// By local index: 0 not asked yet, 1 `Copy`, 2 dropped.
    copy: Vec<u8, &'f Frame>,
    /// Whether a fold ends storage on the way out, as in a coroutine.
    storage: bool,
    /// Counts the changes of which locals a fold drops (`unscheduled`, `storage`).
    epoch: u32,
    leaving_memo: Memo<'f>,
    declared_memo: Memo<'f>,
}

impl<'f> Unwinds<'f> {
    pub(super) fn new_in(frame: &'f Frame) -> Self {
        Unwinds {
            declared: Vec::new_in(frame),
            leaving: Vec::new_in(frame),
            links: Vec::new_in(frame),
            leaving_folded: 0,
            declared_folded: 0,
            first: Vec::new_in(frame),
            copy: Vec::new_in(frame),
            storage: false,
            epoch: 0,
            leaving_memo: Memo::new_in(frame),
            declared_memo: Memo::new_in(frame),
        }
    }

    /// Whether folds end storage on the way out. Every link is folded again when it changes.
    pub(super) fn sync_storage(&mut self, storage: bool) {
        if self.storage != storage {
            self.storage = storage;
            self.leaving_folded = 0;
            self.declared_folded = 0;
            self.epoch = self.epoch.wrapping_add(1);
        }
    }

    /// Lists the scopes again from the outermost one that changed, or the first one past those
    /// listed. Allocates no block: folding is left until a way out is asked for.
    pub(super) fn list<S: HasLocals<'f>>(&mut self, scopes: &mut [S], operands: &LocalSet<'_>) {
        let listed = self.links.len();
        let mut depth = 0;
        while depth < listed && depth < scopes.len() && !scopes[depth].locals_mut().stale {
            depth += 1;
        }
        if depth == listed && depth == scopes.len() {
            return;
        }
        let start = self.links.get(depth).map_or(self.declared.len(), |it| it.start as usize);
        for &local in &self.declared[start..] {
            let slot = &mut self.first[local_index(local)];
            if *slot as usize >= start {
                *slot = NONE;
            }
        }
        self.declared.truncate(start);
        self.leaving.truncate(start);
        self.links.truncate(depth);
        self.leaving_folded = self.leaving_folded.min(depth);
        self.declared_folded = self.declared_folded.min(depth);
        for scope in &mut scopes[depth..] {
            let scope = scope.locals_mut();
            let start = self.declared.len();
            scope.declared_into(operands, &mut self.declared);
            scope.leaving_into(operands, &mut self.leaving);
            for position in start..self.declared.len() {
                let at = local_index(self.declared[position]);
                if self.first.len() <= at {
                    self.first.resize(at + 1, NONE);
                }
                if self.first[at] == NONE {
                    self.first[at] = position as u32;
                }
            }
            self.links.push(Link { start: start as u32, leaving_at: None, declared_at: None });
            scope.stale = false;
        }
    }

    /// Puts `local` in `operands`: what is in scope is dropped in another order from then.
    pub(super) fn mark_operand<S: HasLocals<'f>>(
        &mut self,
        scopes: &mut [S],
        operands: &mut LocalSet<'_>,
        local: LocalId,
    ) {
        if operands.insert(local) {
            self.restale(scopes, local);
        }
    }

    /// Puts `local` in `unscheduled`: unwinding no longer drops it.
    pub(super) fn unschedule<S: HasLocals<'f>>(
        &mut self,
        scopes: &mut [S],
        unscheduled: &mut LocalSet<'_>,
        local: LocalId,
    ) {
        if unscheduled.insert(local) {
            self.restale(scopes, local);
            self.epoch = self.epoch.wrapping_add(1);
        }
    }

    /// Takes `local` out of `unscheduled`: unwinding drops it again.
    pub(super) fn reschedule<S: HasLocals<'f>>(
        &mut self,
        scopes: &mut [S],
        unscheduled: &mut LocalSet<'_>,
        local: LocalId,
    ) {
        if unscheduled.remove(local) {
            self.restale(scopes, local);
            self.epoch = self.epoch.wrapping_add(1);
        }
    }

    /// Marks stale the outermost listed scope holding `local`. A scope that holds it and has not
    /// changed since it was listed holds it at or after its first position, so it is that scope
    /// or one inside it, and is listed again with it.
    fn restale<S: HasLocals<'f>>(&mut self, scopes: &mut [S], local: LocalId) {
        let Some(position) = self.first_of(local) else { return };
        if let Some(scope) = scopes.get_mut(self.depth_of(position)) {
            scope.locals_mut().stale = true;
        }
    }

    fn first_of(&self, local: LocalId) -> Option<usize> {
        match self.first.get(local_index(local)) {
            Some(&position) if position != NONE => Some(position as usize),
            _ => None,
        }
    }

    /// The listed scope `declared[position]` is in.
    fn depth_of(&self, position: usize) -> usize {
        self.links.partition_point(|it| it.start as usize <= position) - 1
    }

    /// Where the listed scope at `depth` ends in both orders.
    fn end_of(&self, depth: usize) -> usize {
        self.links.get(depth + 1).map_or(self.declared.len(), |it| it.start as usize)
    }

    /// The way out from inside every listed scope, through their leaving orders. Call `list`
    /// first.
    pub(super) fn here<T: DropTree>(&mut self, tree: &mut T) -> Option<BasicBlockId> {
        self.fold_chain(tree, Chain::Leaving, self.links.len())
    }

    /// The way out of the drop of `locals[at]` at the end of a scope that drops `locals`: what
    /// is declared before it, then, in a coroutine, its own storage. A local still in a listed
    /// scope (the scopes `drop_until_scope` and `guard_failed` drop) goes after what is declared
    /// before it there; one of a scope that was popped goes after every listed scope and
    /// `locals[..at]`. Call `list` first.
    pub(super) fn from_drop<T: DropTree>(
        &mut self,
        tree: &mut T,
        locals: &[LocalId],
        at: usize,
    ) -> Option<BasicBlockId> {
        let local = locals[at];
        let unwind = match self.first_of(local) {
            Some(position) => {
                let depth = self.depth_of(position);
                let base = self.fold_chain(tree, Chain::Declared, depth);
                let start = self.links[depth].start as usize;
                self.fold_memo(tree, Chain::Declared, base, Seq::Declared { start, end: position })
            }
            None => {
                let base = self.fold_chain(tree, Chain::Declared, self.links.len());
                self.fold_memo(tree, Chain::Declared, base, Seq::Given(&locals[..at]))
            }
        };
        self.storage_step(tree, unwind, local)
    }

    /// The way out of the drop of `locals[position]` when leaving early through the scope at
    /// `index`, whose leaving order is `locals`: every scope outside it, then `locals[..position]`,
    /// then, in a coroutine, its own storage. Call `list` first.
    pub(super) fn before<T: DropTree>(
        &mut self,
        tree: &mut T,
        index: usize,
        locals: &[LocalId],
        position: usize,
    ) -> Option<BasicBlockId> {
        let base = self.fold_chain(tree, Chain::Leaving, index);
        let unwind = self.fold_memo(tree, Chain::Leaving, base, Seq::Given(&locals[..position]));
        self.storage_step(tree, unwind, locals[position])
    }

    fn storage_step<T: DropTree>(
        &mut self,
        tree: &mut T,
        at: Option<BasicBlockId>,
        local: LocalId,
    ) -> Option<BasicBlockId> {
        match self.storage {
            true => Some(tree.node(at, local, false)),
            false => at,
        }
    }

    /// Folds `chain`'s links up to the first `upto` scopes; the block after them.
    fn fold_chain<T: DropTree>(
        &mut self,
        tree: &mut T,
        chain: Chain,
        upto: usize,
    ) -> Option<BasicBlockId> {
        loop {
            let depth = match chain {
                Chain::Leaving => self.leaving_folded,
                Chain::Declared => self.declared_folded,
            };
            if depth >= upto {
                break;
            }
            let mut at = match depth.checked_sub(1) {
                Some(outer) => self.links[outer].at(chain),
                None => None,
            };
            for position in self.links[depth].start as usize..self.end_of(depth) {
                let local = match chain {
                    Chain::Leaving => self.leaving[position],
                    Chain::Declared => self.declared[position],
                };
                at = self.step(tree, at, local);
            }
            match chain {
                Chain::Leaving => {
                    self.links[depth].leaving_at = at;
                    self.leaving_folded = depth + 1;
                }
                Chain::Declared => {
                    self.links[depth].declared_at = at;
                    self.declared_folded = depth + 1;
                }
            }
        }
        match upto.checked_sub(1) {
            Some(last) => self.links[last].at(chain),
            None => None,
        }
    }

    /// Folds `seq` from `base`, past what the last fold of `chain`'s memo shared with it.
    fn fold_memo<T: DropTree>(
        &mut self,
        tree: &mut T,
        chain: Chain,
        base: Option<BasicBlockId>,
        seq: Seq<'_>,
    ) -> Option<BasicBlockId> {
        let (len, epoch) = (seq.len(), self.epoch);
        let memo = self.memo_mut(chain);
        if memo.at.is_empty() || memo.base != base || memo.epoch != epoch {
            memo.seq.clear();
            memo.at.clear();
            memo.at.push(base);
            memo.base = base;
            memo.epoch = epoch;
        }
        let known = self.memo(chain).seq.len().min(len);
        let mut same = 0;
        while same < known && self.memo(chain).seq[same] == self.seq_at(seq, same) {
            same += 1;
        }
        let memo = self.memo_mut(chain);
        memo.seq.truncate(same);
        memo.at.truncate(same + 1);
        let mut at = memo.at[same];
        for index in same..len {
            let local = self.seq_at(seq, index);
            at = self.step(tree, at, local);
            let memo = self.memo_mut(chain);
            memo.seq.push(local);
            memo.at.push(at);
        }
        at
    }

    fn memo(&self, chain: Chain) -> &Memo<'f> {
        match chain {
            Chain::Leaving => &self.leaving_memo,
            Chain::Declared => &self.declared_memo,
        }
    }

    fn memo_mut(&mut self, chain: Chain) -> &mut Memo<'f> {
        match chain {
            Chain::Leaving => &mut self.leaving_memo,
            Chain::Declared => &mut self.declared_memo,
        }
    }

    fn seq_at(&self, seq: Seq<'_>, index: usize) -> LocalId {
        match seq {
            Seq::Declared { start, .. } => self.declared[start + index],
            Seq::Given(it) => it[index],
        }
    }

    /// One local of a fold: its storage in a coroutine, then its value unless nothing drops it.
    fn step<T: DropTree>(
        &mut self,
        tree: &mut T,
        mut at: Option<BasicBlockId>,
        local: LocalId,
    ) -> Option<BasicBlockId> {
        if self.storage {
            at = Some(tree.node(at, local, false));
        }
        if !self.is_copy(tree, local) && !tree.unscheduled(local) {
            at = Some(tree.node(at, local, true));
        }
        at
    }

    fn is_copy<T: DropTree>(&mut self, tree: &mut T, local: LocalId) -> bool {
        let at = local_index(local);
        if self.copy.len() <= at {
            self.copy.resize(at + 1, 0);
        }
        match self.copy[at] {
            0 => {
                let copy = tree.local_is_copy(local);
                self.copy[at] = if copy { 1 } else { 2 };
                copy
            }
            known => known == 1,
        }
    }
}

impl DropTree for MirLowerCtx<'_, '_, '_> {
    fn node(&mut self, parent: Option<BasicBlockId>, local: LocalId, value: bool) -> BasicBlockId {
        self.unwind_node(parent, local, value)
    }

    fn local_is_copy(&mut self, local: LocalId) -> bool {
        let ty = self.local(local).ty.as_ref();
        self.is_copy(ty)
    }

    fn unscheduled(&self, local: LocalId) -> bool {
        self.unscheduled.contains(&local)
    }
}

impl<'f> MirLowerCtx<'_, 'f, '_> {
    /// The scopes listed, for a fold. `put_unwinds` gives them back.
    fn take_unwinds(&mut self) -> Box<Unwinds<'f>, &'f Frame> {
        let mut unwinds = self.unwinds.take().expect("one unwind path is asked at a time");
        unwinds.sync_storage(self.result.coroutine.is_some());
        unwinds.list(&mut self.drop_scopes[..], &self.operand_locals);
        unwinds
    }

    fn put_unwinds(&mut self, unwinds: Box<Unwinds<'f>, &'f Frame>) {
        self.unwinds = Some(unwinds);
    }

    /// The unwind drop tree's way out from where lowering is: every scope's drops.
    pub(super) fn unwind_here(&mut self) -> Option<BasicBlockId> {
        let mut unwinds = self.take_unwinds();
        let unwind = unwinds.here(self);
        self.put_unwinds(unwinds);
        unwind
    }

    /// The way out of a drop of `locals[at]` of a scope that drops `locals`: what is declared
    /// before it, then, in a coroutine, its own storage.
    pub(super) fn unwind_from_drop(&mut self, locals: &[LocalId], at: usize) -> Option<BasicBlockId> {
        let mut unwinds = self.take_unwinds();
        let unwind = unwinds.from_drop(self, locals, at);
        self.put_unwinds(unwinds);
        unwind
    }

    /// The way out of a drop of `locals[position]` when leaving the scope at `index` early,
    /// `locals` being that scope's leaving order: what every scope outside it declared and
    /// `locals[..position]`, then, in a coroutine, its own storage.
    pub(super) fn unwind_before(
        &mut self,
        index: usize,
        locals: &[LocalId],
        position: usize,
    ) -> Option<BasicBlockId> {
        let mut unwinds = self.take_unwinds();
        let unwind = unwinds.before(self, index, locals, position);
        self.put_unwinds(unwinds);
        unwind
    }

    /// Records that `local` holds an operand of an aggregate or a call being lowered.
    pub(super) fn mark_operand(&mut self, local: LocalId) {
        let unwinds = self.unwinds.as_mut().expect("not inside a fold");
        unwinds.mark_operand(&mut self.drop_scopes[..], &mut self.operand_locals, local);
    }

    /// While an expression is written to the temporary `local`: what unwinds out of it does not
    /// drop `local`'s value (rustc's `as_temp` schedules the drop once the expression is done).
    pub(super) fn unschedule(&mut self, local: LocalId) {
        let unwinds = self.unwinds.as_mut().expect("not inside a fold");
        unwinds.unschedule(&mut self.drop_scopes[..], &mut self.unscheduled, local);
    }

    /// Once the expression written to `local` is done: unwinding drops it.
    pub(super) fn reschedule(&mut self, local: LocalId) {
        let unwinds = self.unwinds.as_mut().expect("not inside a fold");
        unwinds.reschedule(&mut self.drop_scopes[..], &mut self.unscheduled, local);
    }
}

#[cfg(test)]
mod tests;
