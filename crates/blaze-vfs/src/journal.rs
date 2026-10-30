//! The VFS made durable, sans-IO: each head move becomes records appended to one file, and a
//! commit is durable once the sync after its records completes.
//!
//! - **Records** are framed `MAGIC, length, kind, payload, checksum`. A node record carries what
//!   the node is made of, never its ID: loading interns it again, so a record is checked by
//!   construction and a node is written once however many roots share it. A head record names a
//!   head, the root it moved to and the journal's sequence number; a removal names a head and
//!   a sequence number.
//! - **Group commit**: commits made while an append or a sync is in flight go out together in the
//!   next append (its bytes moved into the effect, not copied), and one sync makes them all
//!   durable.
//! - **Failures**: a failed append or sync asks for a rewrite (below), which holds the heads as
//!   they are then and so every move the batch recorded; its tickets go with it. A batch half
//!   written by a failed append is replaced with the rest of the file, and a record written
//!   twice loads once (a node is interned, a head keeps the highest sequence).
//! - **Recovery** loads a prefix: it reads records from the start and stops at the first that
//!   is not whole, fails its checksum or is malformed. It never looks for a record past one it
//!   refused, as a torn record's payload may hold whole records of another journal (a file that
//!   is one) that would verify. Before appending it cuts the file after the last good record and
//!   syncs the cut, so nothing past it (a record naming a node it lost) loads later once that
//!   node is made again; a torn record followed by new ones could also borrow their first bytes
//!   and pass its checksum, hiding them. A head whose root's nodes are not there (only a forged
//!   file has one) is dropped, and the file is rewritten as the heads recovered.
//! - **Compaction**: after a collection, the next flush replaces the whole file, in one
//!   [`Effect::Write`], with the nodes the heads reach and one record per head. Nothing is
//!   appended while it is in flight; a failed one is made again from the state then.

use blaze_core::{Completion, Done, Effect, FileName, Tag};
use blaze_ids::{ContentId, Hasher};
use rustc_hash::FxHashMap;

use crate::{
    Mode, Recovered, Ticket,
    tree::{DIR_MODE, NodeId, Tree, parts},
};

const MAGIC: [u8; 4] = *b"BZJ3";
pub(crate) const HEADER: usize = MAGIC.len() + 4 + 1;
const CHECKSUM: usize = 8;
const CHECKSUM_DOMAIN: &str = "blaze v2 vfs journal record 1";

const FILE: u8 = 1;
const DIR: u8 = 2;
const HEAD: u8 = 3;
const REMOVE: u8 = 4;
/// The sequence a rewrite was made at, so the numbers of removals it drops are not given again.
const SEQUENCE: u8 = 5;

/// Sequence numbers are below this: one more per head move from 0 never reaches it, so a
/// record at or above it is forged, and the next number never overflows.
const SEQUENCES: u64 = 1 << 63;

const APPEND: u64 = 0;
const SYNC: u64 = 1;
const CUT: u64 = 2;
const REWRITE: u64 = 3;
const TAGS: u64 = 4;

#[derive(Default)]
struct Batch {
    bytes: Vec<u8>,
    tickets: Vec<Ticket>,
}

/// What is in flight: at most one effect at a time, so the file's order is the order asked.
/// Each holds the tickets it makes durable.
enum Flight {
    Idle,
    Appending(Vec<Ticket>),
    Syncing(Vec<Ticket>),
    Cutting,
    Rewriting(Vec<Ticket>),
}

/// By node, one bit: whether a record of it is in the file, or will be before any record made
/// now.
#[derive(Default)]
struct Written(Vec<u64>);

impl Written {
    fn new(len: usize, all: bool) -> Written {
        let mut words = vec![if all { u64::MAX } else { 0 }; len.div_ceil(64)];
        if all && len % 64 != 0 {
            *words.last_mut().expect("a partial word") = (1 << (len % 64)) - 1;
        }
        Written(words)
    }

    fn get(&self, node: NodeId) -> bool {
        let at = node.0 as usize;
        self.0.get(at / 64).is_some_and(|word| word >> (at % 64) & 1 != 0)
    }

    fn set(&mut self, node: NodeId) {
        let at = node.0 as usize;
        self.0[at / 64] |= 1 << (at % 64);
    }

    /// Room for `len` nodes; the new ones are not written.
    fn grow(&mut self, len: usize) {
        self.0.resize(len.div_ceil(64), 0);
    }
}

pub(crate) struct Journal {
    file: FileName,
    tags: u64,
    written: Written,
    sequence: u64,
    /// The sequence of each head, as last written.
    heads: FxHashMap<Box<str>, u64>,
    /// Records made and not yet handed to an append.
    open: Batch,
    flight: Flight,
    /// The length to cut the file to before appending.
    cut: Option<u64>,
    /// A rewrite is asked for at the next flush.
    compact: bool,
    /// Something was appended since the file was last rewritten.
    dirty: bool,
    next_ticket: u64,
}

/// What [`Journal::recover`] rebuilt.
pub(crate) struct Loaded {
    pub(crate) tree: Tree,
    pub(crate) heads: FxHashMap<Box<str>, NodeId>,
    pub(crate) journal: Journal,
    pub(crate) found: Recovered,
}

impl Journal {
    /// The journal whose file holds `bytes`, and the tree and heads it holds.
    pub(crate) fn recover(file: FileName, tags: u64, bytes: &[u8]) -> Loaded {
        let mut tree = Tree::new();
        let mut found = Recovered::default();
        let mut moves: FxHashMap<Box<str>, Vec<(u64, Option<ContentId>)>> = FxHashMap::default();
        let mut sequence = 0;
        let mut valid = 0;
        while let Some((kind, payload, len)) = record(&bytes[valid..]) {
            if load(&mut tree, kind, payload, &mut moves, &mut sequence).is_none() {
                break;
            }
            found.records += 1;
            valid += len;
        }
        found.skipped = bytes.len() - valid;
        let mut journal = Journal {
            file,
            tags,
            written: Written::new(tree.len(), true),
            sequence,
            heads: FxHashMap::default(),
            open: Batch::default(),
            flight: Flight::Idle,
            cut: (valid < bytes.len()).then_some(valid as u64),
            // Set below if a head is dropped.
            compact: false,
            dirty: !bytes.is_empty(),
            next_ticket: 0,
        };
        let mut heads = FxHashMap::default();
        let mut names: Vec<Box<str>> = moves.keys().cloned().collect();
        names.sort_unstable();
        for name in names {
            let mut moves = moves.remove(&name).expect("a head read");
            // The latest record whose root survived; a later one whose nodes did not is dropped.
            moves.sort_unstable();
            while let Some((sequence, root)) = moves.pop() {
                let Some(root) = root else { break };
                let Some(root) = tree.node(&root).filter(|it| tree.is_dir(*it)) else {
                    found.heads_dropped += 1;
                    // Its record stays in the prefix: rewrite the file as the heads are, or it
                    // comes back once a commit makes that root again.
                    journal.compact = true;
                    continue;
                };
                heads.insert(name.clone(), root);
                journal.heads.insert(name, sequence);
                break;
            }
        }
        Loaded { tree, heads, journal, found }
    }

    /// Records that `head` moved to `root`; the ticket is in what [`Journal::complete`] returns
    /// once that is durable.
    pub(crate) fn record_head(&mut self, tree: &Tree, head: &str, root: NodeId) -> Ticket {
        self.written.grow(tree.len());
        write_node(tree, &mut self.written, &mut self.open.bytes, root);
        self.sequence = next(self.sequence);
        self.heads.insert(head.into(), self.sequence);
        head_record(&mut self.open.bytes, self.sequence, Some(&tree.id(root)), head);
        self.ticket()
    }

    /// Records that `head` is gone.
    pub(crate) fn record_removal(&mut self, head: &str) -> Ticket {
        self.sequence = next(self.sequence);
        self.heads.remove(head);
        head_record(&mut self.open.bytes, self.sequence, None, head);
        self.ticket()
    }

    fn ticket(&mut self) -> Ticket {
        let ticket = Ticket(self.next_ticket);
        self.next_ticket += 1;
        self.open.tickets.push(ticket);
        ticket
    }

    /// The sequence number `head` was last written with.
    pub(crate) fn sequence(&self, head: &str) -> Option<u64> {
        self.heads.get(head).copied()
    }

    /// The largest sequence number given out.
    pub(crate) fn last_sequence(&self) -> u64 {
        self.sequence
    }

    /// Starts the next effect if none is in flight: a rewrite if one was asked for, else the cut
    /// recovery asked for, else an append of what was recorded since the last.
    pub(crate) fn flush(
        &mut self,
        tree: &Tree,
        heads: &FxHashMap<Box<str>, NodeId>,
        effects: &mut Vec<Effect>,
    ) {
        if !matches!(self.flight, Flight::Idle) {
            return;
        }
        if self.compact {
            self.compact = false;
            // The rewrite replaces the whole file, the torn tail with it.
            self.cut = None;
            self.dirty = false;
            self.written = Written::new(tree.len(), false);
            let mut names: Vec<&Box<str>> = heads.keys().collect();
            names.sort_unstable();
            let mut bytes = Vec::new();
            for name in &names {
                write_node(tree, &mut self.written, &mut bytes, heads[*name]);
            }
            for name in &names {
                let sequence = self.heads[&***name];
                head_record(&mut bytes, sequence, Some(&tree.id(heads[*name])), name);
            }
            frame(&mut bytes, SEQUENCE, |out| out.extend_from_slice(&self.sequence.to_le_bytes()));
            // What was recorded and not appended is in the rewrite: the heads as they are now.
            let tickets = std::mem::take(&mut self.open).tickets;
            effects.push(Effect::Write { file: self.file.clone(), bytes, tag: self.tag(REWRITE) });
            self.flight = Flight::Rewriting(tickets);
            return;
        }
        if let Some(len) = self.cut {
            effects.push(Effect::Truncate { file: self.file.clone(), len, tag: self.tag(CUT) });
            self.flight = Flight::Cutting;
            return;
        }
        if self.open.tickets.is_empty() && self.open.bytes.is_empty() {
            return;
        }
        let Batch { bytes, tickets } = std::mem::take(&mut self.open);
        effects.push(Effect::Append { file: self.file.clone(), bytes, tag: self.tag(APPEND) });
        self.dirty = true;
        self.flight = Flight::Appending(tickets);
    }

    fn tag(&self, offset: u64) -> Tag {
        Tag(self.tags + offset)
    }

    pub(crate) fn owns(&self, tag: Tag) -> bool {
        tag.0.checked_sub(self.tags).is_some_and(|it| it < TAGS)
    }

    /// Takes the completion of one of its effects; returns the tickets now durable.
    pub(crate) fn complete(
        &mut self,
        tree: &Tree,
        heads: &FxHashMap<Box<str>, NodeId>,
        completion: Completion,
        effects: &mut Vec<Effect>,
    ) -> Vec<Ticket> {
        let offset = completion.tag.0 - self.tags;
        let mut durable = Vec::new();
        match (offset, completion.done, std::mem::replace(&mut self.flight, Flight::Idle)) {
            (APPEND, Done::Appended, Flight::Appending(tickets)) => {
                effects.push(Effect::Sync { file: self.file.clone(), tag: self.tag(SYNC) });
                self.flight = Flight::Syncing(tickets);
            }
            (SYNC, Done::Synced, Flight::Syncing(tickets)) => durable = tickets,
            (CUT, Done::Truncated, Flight::Cutting) => {
                self.cut = None;
                effects.push(Effect::Sync { file: self.file.clone(), tag: self.tag(SYNC) });
                self.flight = Flight::Syncing(Vec::new());
            }
            (CUT, Done::Failed(_), Flight::Cutting) => {}
            (REWRITE, Done::Written, Flight::Rewriting(tickets)) => durable = tickets,
            // A failed rewrite left the file as it was. A failed append may have left part of
            // its batch, and after a failed sync the kernel may have dropped the pages it could
            // not write, so a second sync could succeed without them. Either way the next
            // effect is a rewrite made of the state then, which holds every move recorded since
            // and replaces whatever the file has.
            (
                APPEND | SYNC | REWRITE,
                Done::Failed(_),
                Flight::Appending(tickets) | Flight::Syncing(tickets) | Flight::Rewriting(tickets),
            ) => {
                let later = std::mem::take(&mut self.open).tickets;
                self.open.tickets = tickets;
                self.open.tickets.extend(later);
                self.compact = true;
            }
            (offset, done, _) => {
                unreachable!("the journal has nothing in flight as tag +{offset} for {done:?}")
            }
        }
        self.flush(tree, heads, effects);
        durable
    }

    /// Follows a collection: each node's new number, and a rewrite asked for if the file holds
    /// anything the heads may no longer reach.
    pub(crate) fn collected(&mut self, renumbered: &[Option<NodeId>], len: usize) {
        let mut written = Written::new(len, false);
        for (old, new) in renumbered.iter().enumerate() {
            if let Some(new) = new
                && self.written.get(NodeId(old as u32))
            {
                written.set(*new);
            }
        }
        self.written = written;
        self.compact |= self.dirty;
    }
}

/// The sequence number after `sequence`, which is below [`SEQUENCES`].
fn next(sequence: u64) -> u64 {
    let next = sequence + 1;
    assert!(next < SEQUENCES, "2^63 head moves in one journal");
    next
}

/// Records `node` and everything under it not recorded yet, children first, so a record only
/// names nodes recorded before it. Each record is framed in place in `out`.
fn write_node(tree: &Tree, written: &mut Written, out: &mut Vec<u8>, node: NodeId) {
    let mut stack = vec![(node, false)];
    while let Some((node, children_done)) = stack.pop() {
        if written.get(node) {
            continue;
        }
        if let Some(bytes) = tree.bytes(node) {
            out.reserve(HEADER + bytes.len() + CHECKSUM);
            frame(out, FILE, |out| out.extend_from_slice(bytes));
        } else if !children_done {
            stack.push((node, true));
            let entries = tree.entries(node).expect("a directory");
            stack.extend(entries.iter().filter(|it| !written.get(it.1)).map(|it| (it.1, false)));
            continue;
        } else {
            let entries = tree.entries(node).expect("a directory");
            frame(out, DIR, |out| {
                for (name, child, mode) in entries.iter() {
                    let name = tree.name_str(name);
                    out.extend_from_slice(&(name.len() as u32).to_le_bytes());
                    out.extend_from_slice(name.as_bytes());
                    out.extend_from_slice(&tree.code(child, mode).to_le_bytes());
                    out.extend_from_slice(&tree.id(child).0);
                }
            });
        }
        written.set(node);
    }
}

fn head_record(out: &mut Vec<u8>, sequence: u64, root: Option<&ContentId>, head: &str) {
    let kind = if root.is_some() { HEAD } else { REMOVE };
    frame(out, kind, |out| {
        out.extend_from_slice(&sequence.to_le_bytes());
        if let Some(root) = root {
            out.extend_from_slice(&root.0);
        }
        out.extend_from_slice(head.as_bytes());
    });
}

fn checksum(kind: u8, payload: &[u8]) -> [u8; CHECKSUM] {
    let id = Hasher::new(CHECKSUM_DOMAIN).update(&[kind]).update(payload).finish();
    id.0[..CHECKSUM].try_into().expect("eight bytes")
}

/// Appends one record whose payload `payload` writes, in place: the length is filled in after.
fn frame(out: &mut Vec<u8>, kind: u8, payload: impl FnOnce(&mut Vec<u8>)) {
    let start = out.len();
    out.extend_from_slice(&MAGIC);
    out.extend_from_slice(&[0; 4]);
    out.push(kind);
    payload(out);
    let len = u32::try_from(out.len() - start - HEADER).expect("a record under 4 GiB");
    out[start + MAGIC.len()..start + MAGIC.len() + 4].copy_from_slice(&len.to_le_bytes());
    let sum = checksum(kind, &out[start + HEADER..]);
    out.extend_from_slice(&sum);
}

/// One record of `kind` holding `payload`, framed with a good checksum: for tests that forge one.
#[cfg(test)]
pub(crate) fn framed(kind: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    frame(&mut out, kind, |out| out.extend_from_slice(payload));
    out
}

#[cfg(test)]
pub(crate) const KINDS: (u8, u8, u8, u8, u8) = (FILE, DIR, HEAD, REMOVE, SEQUENCE);

/// The record at the start of `bytes`, its payload and its whole length, if it is a whole record
/// with a good checksum.
fn record(bytes: &[u8]) -> Option<(u8, &[u8], usize)> {
    if bytes.len() < HEADER + CHECKSUM || bytes[..4] != MAGIC {
        return None;
    }
    let len = u32::from_le_bytes(bytes[4..8].try_into().ok()?) as usize;
    let kind = bytes[8];
    let end = HEADER.checked_add(len)?;
    let payload = bytes.get(HEADER..end)?;
    let sum = bytes.get(end..end.checked_add(CHECKSUM)?)?;
    (checksum(kind, payload) == *sum).then_some((kind, payload, end + CHECKSUM))
}

/// Loads one record; `None` when it names a node that is not there or is malformed (a
/// directory's names not distinct path components in order, a sequence number at or above
/// [`SEQUENCES`]). `sequence` becomes the largest any record names.
fn load(
    tree: &mut Tree,
    kind: u8,
    payload: &[u8],
    moves: &mut FxHashMap<Box<str>, Vec<(u64, Option<ContentId>)>>,
    sequence: &mut u64,
) -> Option<()> {
    match kind {
        FILE => {
            tree.file(std::sync::Arc::from(payload));
        }
        DIR => {
            let mut entries = Vec::new();
            let mut rest = payload;
            let mut last: Option<&str> = None;
            while !rest.is_empty() {
                let len = u32::from_le_bytes(rest.get(..4)?.try_into().ok()?) as usize;
                let name = std::str::from_utf8(rest.get(4..4 + len)?).ok()?;
                // As the tree writes them: one component each, sorted by bytes, so distinct.
                if parts(name).ok()?.len() != 1 || last.is_some_and(|it| it >= name) {
                    return None;
                }
                last = Some(name);
                let code = u32::from_le_bytes(rest.get(4 + len..8 + len)?.try_into().ok()?);
                let id = ContentId(rest.get(8 + len..8 + len + 32)?.try_into().ok()?);
                rest = &rest[8 + len + 32..];
                let child = tree.node(&id)?;
                // A directory is its node's kind; the code must say the same.
                let mode = match code {
                    DIR_MODE if tree.is_dir(child) => Mode::Regular,
                    _ if tree.is_dir(child) => return None,
                    _ => Mode::from_git(code)?,
                };
                let name = tree.name(name);
                entries.push((name, child, mode));
            }
            tree.dir(entries);
        }
        HEAD => {
            let at = sequence_at(payload)?;
            let root = ContentId(payload.get(8..40)?.try_into().ok()?);
            let name = std::str::from_utf8(&payload[40..]).ok()?;
            moves.entry(name.into()).or_default().push((at, Some(root)));
            *sequence = (*sequence).max(at);
        }
        REMOVE => {
            let at = sequence_at(payload)?;
            let name = std::str::from_utf8(&payload[8..]).ok()?;
            moves.entry(name.into()).or_default().push((at, None));
            *sequence = (*sequence).max(at);
        }
        SEQUENCE => {
            let at = sequence_at(payload).filter(|_| payload.len() == 8)?;
            *sequence = (*sequence).max(at);
        }
        _ => return None,
    }
    Some(())
}

/// The sequence number a record's payload starts with, if it is below [`SEQUENCES`].
fn sequence_at(payload: &[u8]) -> Option<u64> {
    let at = u64::from_le_bytes(payload.get(..8)?.try_into().ok()?);
    (at < SEQUENCES).then_some(at)
}
