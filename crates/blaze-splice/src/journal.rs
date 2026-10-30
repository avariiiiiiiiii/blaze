//! What a splice wrote over, to put back before the next (docs/v2/plan/design/memory.md,
//! "tables"): the bytes of every run in one [`RegionVec`] and the runs in another, tagged
//! [`Tag::Link`]. Each splice empties it and records its runs again into the room the last one
//! committed, so a warm splice records and puts back its runs with no allocation.
//!
//! An empty journal reserves nothing: a [`Baseline::view`](crate::Baseline::view), which never
//! splices, costs no region.

use blaze_mem::{MemError, RegionVec, Tag};

/// The most bytes one journal holds of what a splice wrote over.
const BYTES: usize = 1 << 32;

/// The most runs one journal holds.
const RUNS: usize = 1 << 22;

#[derive(Clone, Copy)]
struct Run {
    offset: u64,
    start: usize,
    len: usize,
}

struct Kept {
    bytes: RegionVec<u8>,
    runs: RegionVec<Run>,
}

/// Runs of a file, each with the bytes it had, in the order they were recorded.
#[derive(Default)]
pub(crate) struct Journal {
    kept: Option<Kept>,
}

impl Journal {
    fn kept(&mut self) -> Result<&mut Kept, MemError> {
        if self.kept.is_none() {
            self.kept = Some(Kept {
                bytes: RegionVec::new(BYTES, Tag::Link)?,
                runs: RegionVec::new(RUNS, Tag::Link)?,
            });
        }
        match &mut self.kept {
            Some(kept) => Ok(kept),
            None => unreachable!("just made"),
        }
    }

    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.kept.as_ref().is_none_or(|kept| kept.runs.is_empty())
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.kept.as_ref().map_or(0, |kept| kept.runs.len())
    }

    /// Records the run of `len` bytes at `offset`, which `fill` reads into the room it is
    /// given, and gives what it read. A `fill` that fails records nothing.
    pub(crate) fn record(
        &mut self,
        offset: u64,
        len: usize,
        fill: impl FnOnce(&mut [u8]) -> anyhow::Result<()>,
    ) -> anyhow::Result<&[u8]> {
        const ZEROS: [u8; 4096] = [0; 4096];
        let kept = self.kept()?;
        let start = kept.bytes.len();
        kept.bytes.reserve(len)?;
        kept.runs.reserve(1)?;
        while kept.bytes.len() < start + len {
            let more = (start + len - kept.bytes.len()).min(ZEROS.len());
            kept.bytes.try_extend_from_slice(&ZEROS[..more])?;
        }
        if let Err(error) = fill(&mut kept.bytes[start..]) {
            kept.bytes.truncate(start);
            return Err(error);
        }
        kept.runs.try_push(Run { offset, start, len })?;
        Ok(&kept.bytes[start..])
    }

    /// Records `bytes` as what the run at `offset` had.
    pub(crate) fn push(&mut self, offset: u64, bytes: &[u8]) -> Result<(), MemError> {
        let kept = self.kept()?;
        let start = kept.bytes.len();
        kept.runs.reserve(1)?;
        kept.bytes.try_extend_from_slice(bytes)?;
        kept.runs.try_push(Run { offset, start, len: bytes.len() })?;
        Ok(())
    }

    /// Every run with the bytes it had, in the order they were recorded.
    pub(crate) fn iter(&self) -> impl DoubleEndedIterator<Item = (u64, &[u8])> + Clone {
        let (bytes, runs): (&[u8], &[Run]) = match &self.kept {
            Some(kept) => (kept.bytes.as_slice(), kept.runs.as_slice()),
            None => (&[][..], &[][..]),
        };
        runs.iter().map(move |run| (run.offset, &bytes[run.start..run.start + run.len]))
    }

    /// Forgets every run, keeping the room they took.
    pub(crate) fn clear(&mut self) {
        if let Some(kept) = &mut self.kept {
            kept.runs.clear();
            kept.bytes.clear();
        }
    }

    /// Makes this journal hold what `other` holds, into the room it has.
    pub(crate) fn copy_from(&mut self, other: &Journal) -> Result<(), MemError> {
        self.clear();
        for (offset, bytes) in other.iter() {
            self.push(offset, bytes)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runs(round: u64) -> Vec<(u64, Vec<u8>)> {
        (0..40u64)
            .map(|it| (it * 4096 + round, vec![(it + round) as u8; (it % 17 + 1) as usize]))
            .collect()
    }

    fn held(journal: &Journal) -> Vec<(u64, Vec<u8>)> {
        journal.iter().map(|(offset, bytes)| (offset, bytes.to_vec())).collect()
    }

    #[test]
    fn it_is_a_list_of_runs() {
        let mut journal = Journal::default();
        assert!(journal.is_empty());
        assert_eq!(journal.iter().count(), 0);
        let mut model = Vec::new();
        for round in 0..3 {
            journal.clear();
            model.clear();
            for (offset, bytes) in runs(round) {
                let read = journal
                    .record(offset, bytes.len(), |room| {
                        room.copy_from_slice(&bytes);
                        Ok(())
                    })
                    .unwrap()
                    .to_vec();
                assert_eq!(read, bytes);
                model.push((offset, bytes));
            }
            assert_eq!(journal.len(), model.len());
            assert_eq!(held(&journal), model);
            let backwards: Vec<_> = journal.iter().rev().map(|(offset, _)| offset).collect();
            let expected: Vec<_> = model.iter().rev().map(|(offset, _)| *offset).collect();
            assert_eq!(backwards, expected);
        }
    }

    #[test]
    fn a_failed_read_records_nothing() {
        let mut journal = Journal::default();
        journal.push(1, b"one").unwrap();
        let failed = journal.record(2, 8, |_| anyhow::bail!("the file ends early"));
        assert!(failed.is_err());
        journal.push(3, b"three").unwrap();
        assert_eq!(held(&journal), [(1, b"one".to_vec()), (3, b"three".to_vec())]);
    }

    #[test]
    fn a_copy_is_put_back() {
        let (mut journal, mut saved) = (Journal::default(), Journal::default());
        for (offset, bytes) in runs(5) {
            journal.push(offset, &bytes).unwrap();
        }
        saved.copy_from(&journal).unwrap();
        journal.clear();
        journal.push(9, b"other").unwrap();
        journal.copy_from(&saved).unwrap();
        assert_eq!(held(&journal), runs(5));
    }

    #[test]
    fn a_warm_splice_records_and_puts_back_with_no_allocation() {
        let (mut journal, mut saved) = (Journal::default(), Journal::default());
        let rounds: Vec<_> = (0..4).map(runs).collect();
        let record = |journal: &mut Journal, saved: &mut Journal, runs: &[(u64, Vec<u8>)]| {
            saved.copy_from(journal).unwrap();
            let mut put_back = 0;
            for (_, bytes) in journal.iter().rev() {
                put_back += bytes.len();
            }
            journal.clear();
            for (offset, bytes) in runs {
                journal
                    .record(*offset, bytes.len(), |room| {
                        room.copy_from_slice(bytes);
                        Ok(())
                    })
                    .unwrap();
            }
            put_back
        };
        record(&mut journal, &mut saved, &rounds[0]);
        record(&mut journal, &mut saved, &rounds[1]);
        blaze_alloc_count::assert_at_most(0, "a warm splice's undo journal", || {
            record(&mut journal, &mut saved, &rounds[2]);
            record(&mut journal, &mut saved, &rounds[3]);
        });
        assert_eq!(held(&journal), rounds[3]);
        assert_eq!(held(&saved), rounds[2]);
    }
}
