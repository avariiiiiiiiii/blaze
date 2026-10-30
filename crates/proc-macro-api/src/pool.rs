//! The connections of one proc-macro server program: each serves one request at a time, so a
//! request claims one for itself ([`ProcMacroServerPool::claim`]) and no lock is held while it
//! runs. When every connection is claimed, by expansions on other threads or by the one whose
//! sub-request this thread is answering from the database, another is opened: as many
//! connections as expansions in flight, so no expansion waits on another and none can wait on
//! itself.
use std::{
    fmt, io,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicUsize, Ordering},
    },
};

use crate::{
    MacroDylib, ProcMacro, ServerError,
    process::{Claimed, ProcMacroServerProcess},
};

/// Opens another connection to the same server program.
pub(crate) type Open = Box<dyn Fn() -> io::Result<ProcMacroServerProcess> + Send + Sync>;

/// Chunk `k` holds `FIRST << k` connections, so a connection never moves once added and a
/// reader never locks; `CHUNKS` of them hold more than any machine runs at once.
const FIRST: usize = 4;
const CHUNKS: usize = 24;

/// The connections, append-only: one is added with a release store into a slot of its own.
struct Connections {
    chunks: [OnceLock<Box<[OnceLock<ProcMacroServerProcess>]>>; CHUNKS],
    /// How many slots were handed out; a slot handed out and not set yet is skipped.
    next: AtomicUsize,
}

fn chunk_of(n: usize) -> (usize, usize) {
    let biased = n / FIRST + 1;
    let chunk = (usize::BITS - 1 - biased.leading_zeros()) as usize;
    (chunk, n - FIRST * ((1 << chunk) - 1))
}

impl Connections {
    fn new() -> Connections {
        Connections { chunks: std::array::from_fn(|_| OnceLock::new()), next: AtomicUsize::new(0) }
    }

    fn push(&self, process: ProcMacroServerProcess) -> Result<(), ServerError> {
        let n = self.next.fetch_add(1, Ordering::AcqRel);
        let (chunk, at) = chunk_of(n);
        if chunk >= CHUNKS {
            return Err(ServerError {
                message: format!("{n} connections to one proc-macro server are open at once"),
                io: None,
            });
        }
        let slots =
            self.chunks[chunk].get_or_init(|| (0..FIRST << chunk).map(|_| OnceLock::new()).collect());
        if slots[at].set(process).is_err() {
            unreachable!("connection slot {n} handed out twice");
        }
        Ok(())
    }

    fn iter(&self) -> impl Iterator<Item = &ProcMacroServerProcess> {
        let len = self.next.load(Ordering::Acquire);
        (0..len).filter_map(|n| {
            let (chunk, at) = chunk_of(n);
            self.chunks.get(chunk)?.get()?.get(at)?.get()
        })
    }
}

struct Inner {
    connections: Connections,
    version: u32,
    open: Open,
}

#[derive(Clone)]
pub(crate) struct ProcMacroServerPool(Arc<Inner>);

impl fmt::Debug for ProcMacroServerPool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProcMacroServerPool")
            .field("version", &self.0.version)
            .field("connections", &self.0.connections.iter().count())
            .finish()
    }
}

impl ProcMacroServerPool {
    /// The pool of the connections `first`, which are at least one, that opens more by `open`.
    pub(crate) fn new(first: Vec<ProcMacroServerProcess>, open: Open) -> Result<Self, ServerError> {
        let version = first.first().expect("a pool of at least one connection").version();
        let inner = Inner { connections: Connections::new(), version, open };
        for process in first {
            inner.connections.push(process)?;
        }
        Ok(ProcMacroServerPool(Arc::new(inner)))
    }

    /// Why the server is gone, once every connection to it has ended.
    pub(crate) fn exited(&self) -> Option<&ServerError> {
        let mut first = None;
        for process in self.0.connections.iter() {
            let exited = process.exited()?;
            first.get_or_insert(exited);
        }
        first
    }

    /// A connection no request runs on, the caller's until the claim is dropped: another one
    /// is opened when each is claimed. An error once every connection has ended, as their
    /// server has.
    pub(crate) fn claim(&self) -> Result<Claimed<'_>, ServerError> {
        loop {
            let mut live = false;
            for process in self.0.connections.iter().filter(|it| it.exited().is_none()) {
                live = true;
                if let Some(claimed) = process.claim() {
                    return Ok(claimed);
                }
            }
            if !live {
                return Err(ServerError {
                    message: "all proc-macro server workers have exited".into(),
                    io: None,
                });
            }
            let process = (self.0.open)().map_err(|error| ServerError {
                message: "every connection to the proc-macro server is in use and another could \
                          not be opened"
                    .into(),
                io: Some(Arc::new(error)),
            })?;
            if process.version() != self.0.version {
                return Err(ServerError {
                    message: format!(
                        "a new connection to the proc-macro server speaks API {}, the others {}",
                        process.version(),
                        self.0.version
                    ),
                    io: None,
                });
            }
            // Claimed by the next pass unless another thread claims it first.
            self.0.connections.push(process)?;
        }
    }

    /// The macros of `dylib`, listed by one connection: a server loads a library when it is
    /// first asked to expand one of its macros, so no other connection needs to be told.
    pub(crate) fn load_dylib(&self, dylib: &MacroDylib) -> Result<Vec<ProcMacro>, ServerError> {
        let _span = tracing::info_span!("ProcMacroServer::load_dylib").entered();

        let dylib_path = Arc::new(dylib.path.clone());
        let dylib_last_modified =
            std::fs::metadata(dylib_path.as_path()).ok().and_then(|m| m.modified().ok());

        let macros = self
            .claim()?
            .find_proc_macros(&dylib.path)?
            .map_err(|e| ServerError { message: e, io: None })?;

        Ok(macros
            .into_iter()
            .map(|(name, kind)| ProcMacro {
                pool: self.clone(),
                name: name.into(),
                kind,
                dylib_path: dylib_path.clone(),
                dylib_last_modified,
            })
            .collect())
    }

    pub(crate) fn version(&self) -> u32 {
        self.0.version
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slots_cover_every_index_in_order() {
        let mut expected = (0, 0);
        for n in 0..10_000 {
            assert_eq!(chunk_of(n), expected, "{n}");
            expected.1 += 1;
            if expected.1 == FIRST << expected.0 {
                expected = (expected.0 + 1, 0);
            }
        }
    }
}
