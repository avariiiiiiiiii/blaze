//! What the page's thread keeps of each workspace's records until a page is first asked for
//! (docs/web.md, "Where it comes from"): those since the build that said its functions whole,
//! and all of its last build's, to make the world from.
//!
//! A workspace's records are kept as runs, each a row of records of one build as they came. A
//! run's Began and Functions records (a cold build's are some megabytes) are one deflate
//! stream, written by the workspace's one encoder, whose state is reused from run to run; its
//! other records, which the next Began drops, are plain CBOR beside it. Each kept record is
//! its length (`u32`, little-endian) and its CBOR. What a Began or a whole object drops is
//! always a whole stream, so nothing is ever compressed twice.

use std::io::{Read, Write};

use blaze_protocol::events::{Event, Record};
use flate2::{Compression, read::DeflateDecoder, write::DeflateEncoder};

/// How many workspaces' programs the daemon keeps.
pub(crate) const KEPT: usize = 16;

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Kind {
    Began,
    Functions { object: u32 },
    Other,
}

const _: () = assert!(size_of::<Kind>() == 8);

impl Kind {
    fn of(record: &Record) -> Kind {
        match &record.event {
            Event::Began { .. } => Kind::Began,
            Event::Functions { object, .. } => Kind::Functions { object: *object },
            _ => Kind::Other,
        }
    }
}

#[derive(Default)]
pub(crate) struct Kept {
    /// The workspace whose build began last at the end.
    workspaces: Vec<Workspace>,
}

struct Workspace {
    root: u32,
    runs: Vec<Run>,
    /// The stream of the last run while it is written: `open` says it is.
    encoder: DeflateEncoder<Vec<u8>>,
    open: bool,
}

/// Records of one build, in a row as they came.
struct Run {
    build: u64,
    /// Each record's kind, in order: a Began's or a Functions' is in `deflated`, any other's in
    /// `other`.
    kinds: Vec<Kind>,
    /// Empty while the run is open.
    deflated: Vec<u8>,
    other: Vec<u8>,
    /// The objects its Functions are of.
    objects: Vec<u32>,
}

impl Kept {
    /// Keeps `record`, of build `build` of the workspace numbered `root`.
    pub(crate) fn keep(&mut self, root: u32, build: u64, record: &Record) {
        let mut at = match self.workspaces.iter().position(|it| it.root == root) {
            Some(at) => at,
            None => {
                if self.workspaces.len() >= KEPT {
                    let oldest = (0..self.workspaces.len())
                        .min_by_key(|&it| self.workspaces[it].build());
                    if let Some(oldest) = oldest {
                        self.workspaces.remove(oldest);
                    }
                }
                self.workspaces.push(Workspace::new(root));
                self.workspaces.len() - 1
            }
        };
        if matches!(record.event, Event::Began { .. }) {
            let began = self.workspaces.remove(at);
            self.workspaces.push(began);
            at = self.workspaces.len() - 1;
        }
        self.workspaces[at].keep(build, record);
    }

    /// Hands each record kept to `apply`, in the order the world takes them: each workspace's
    /// in order, the workspace whose build began last last.
    pub(crate) fn drain(self, mut apply: impl FnMut(u32, u64, Record)) {
        let mut inflated = Vec::new();
        for mut workspace in self.workspaces {
            workspace.close();
            for run in &workspace.runs {
                inflated.clear();
                let build = run.build;
                let read = DeflateDecoder::new(&run.deflated[..]).read_to_end(&mut inflated);
                if let Err(error) = read {
                    eprintln!("build page: the records kept of build {build} do not inflate: {error}");
                    continue;
                }
                let (mut kept, mut other) = (Items(&inflated), Items(&run.other));
                for kind in &run.kinds {
                    let item = if *kind == Kind::Other { other.next() } else { kept.next() };
                    match item.map(blaze_protocol::cbor::from_slice::<Record>) {
                        Some(Ok(record)) => apply(workspace.root, build, record),
                        Some(Err(error)) => {
                            eprintln!("build page: a record kept of build {build} does not decode: {error}")
                        }
                        None => {
                            eprintln!("build page: the records kept of build {build} end early");
                            break;
                        }
                    }
                }
            }
        }
    }

    /// The build and kind of each record kept of the workspace `at` in order.
    #[cfg(test)]
    pub(crate) fn kinds(&self, at: usize) -> Vec<(u64, Kind)> {
        let Some(workspace) = self.workspaces.get(at) else { return Vec::new() };
        let runs = workspace.runs.iter();
        runs.flat_map(|run| run.kinds.iter().map(|&kind| (run.build, kind))).collect()
    }
}

impl Workspace {
    fn new(root: u32) -> Workspace {
        Workspace {
            root,
            runs: Vec::new(),
            encoder: DeflateEncoder::new(Vec::new(), Compression::fast()),
            open: false,
        }
    }

    fn build(&self) -> u64 {
        self.runs.last().map_or(0, |it| it.build)
    }

    /// As [`crate::world::World::apply`] does: a Began ends the last build's records but for its
    /// functions, and the first object of a build said whole makes the program anew.
    fn keep(&mut self, build: u64, record: &Record) {
        let kind = Kind::of(record);
        match &record.event {
            Event::Began { .. } => {
                let mut said: Vec<u64> = Vec::new();
                for run in &self.runs {
                    if !run.objects.is_empty() && said.last() != Some(&run.build) {
                        said.push(run.build);
                    }
                }
                // A run of a build that said no functions holds only its Began.
                self.retain(|run| {
                    run.other.clear();
                    run.kinds.retain(|it| *it != Kind::Other);
                    !run.kinds.is_empty() && said.contains(&run.build)
                });
            }
            Event::Functions { whole: true, object, .. } => {
                let said =
                    self.runs.iter().any(|it| it.build == build && it.objects.contains(object));
                if !said {
                    self.retain(|run| run.build == build);
                }
            }
            _ => {}
        }
        let item = match blaze_protocol::cbor::to_vec(record) {
            Ok(item) => item,
            Err(error) => {
                eprintln!("build page: a record of build {build} does not encode: {error}");
                return;
            }
        };
        if !(self.open && self.runs.last().is_some_and(|it| it.build == build)) {
            self.close();
            self.runs.push(Run {
                build,
                kinds: Vec::new(),
                deflated: Vec::new(),
                other: Vec::new(),
                objects: Vec::new(),
            });
            self.open = true;
        }
        let Some(run) = self.runs.last_mut() else { return };
        let len = (item.len() as u32).to_le_bytes();
        if kind == Kind::Other {
            run.other.extend_from_slice(&len);
            run.other.extend_from_slice(&item);
        } else {
            let written =
                self.encoder.write_all(&len).and_then(|()| self.encoder.write_all(&item));
            if let Err(error) = written {
                eprintln!("build page: a record of build {build} is not kept: {error}");
                return;
            }
        }
        if let Kind::Functions { object } = kind
            && !run.objects.contains(&object)
        {
            run.objects.push(object);
        }
        run.kinds.push(kind);
    }

    /// Keeps the runs `keep` says to; the stream of the last, if it goes, goes with it.
    fn retain(&mut self, mut keep: impl FnMut(&mut Run) -> bool) {
        let last = self.runs.len().wrapping_sub(1);
        let mut at = 0;
        let mut dropped_open = false;
        let open = self.open;
        self.runs.retain_mut(|run| {
            let kept = keep(run);
            dropped_open |= !kept && open && at == last;
            at += 1;
            kept
        });
        if dropped_open {
            self.open = false;
            // What it held is dropped: the encoder only starts a stream of its own again.
            let _ = self.encoder.reset(Vec::new());
        }
    }

    /// Ends the stream of the last run.
    fn close(&mut self) {
        if !std::mem::take(&mut self.open) {
            return;
        }
        let finished = self.encoder.reset(Vec::new());
        let Some(run) = self.runs.last_mut() else { return };
        match finished {
            Ok(mut deflated) => {
                deflated.shrink_to_fit();
                run.deflated = deflated;
            }
            Err(error) => {
                let build = run.build;
                eprintln!("build page: the records kept of build {build} do not deflate: {error}")
            }
        }
    }
}

/// Items each a `u32` little-endian length and that many bytes.
struct Items<'a>(&'a [u8]);

impl<'a> Iterator for Items<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<&'a [u8]> {
        let (len, rest) = self.0.split_first_chunk::<4>()?;
        let len = u32::from_le_bytes(*len) as usize;
        if rest.len() < len {
            return None;
        }
        let (item, rest) = rest.split_at(len);
        self.0 = rest;
        Some(item)
    }
}

/// `records` as kept and drained, for the tests: the stream round-trips.
#[cfg(test)]
pub(crate) fn round_trip(
    records: &[(u32, u64, Record)],
) -> (Vec<(u64, Kind)>, Vec<(u32, u64, Record)>) {
    let mut kept = Kept::default();
    for (root, build, record) in records {
        kept.keep(*root, *build, record);
    }
    let kinds = kept.kinds(kept.workspaces.len().wrapping_sub(1));
    let mut out = Vec::new();
    kept.drain(|root, build, record| out.push((root, build, record)));
    (kinds, out)
}
