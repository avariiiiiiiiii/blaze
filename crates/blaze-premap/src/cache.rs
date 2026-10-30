//! The images of a machine: one per key, opened once (parsed, checked, written into its
//! memory object) and shared by every view and run, never one per view. A build picks the
//! image of its key, or else the newest image of its dependency graph, whose dependency
//! instances it lacks are linked with the workspace while the image of its key is made in
//! the background; an image is made again only when its key changes and a build needs what
//! the key adds.

use std::sync::Arc;

use rustc_hash::{FxHashMap, FxHashSet};

use crate::PremapError;
use crate::image::View;
use crate::key::{GraphKey, ImageKey};
use crate::map::Memory;

/// Dependency graphs kept at once: a machine's branches and toolchains, not its history.
pub const GRAPHS: usize = 8;

/// An opened image.
pub struct Ready {
    pub key: ImageKey,
    pub graph: GraphKey,
    bytes: Box<[u8]>,
    memory: Memory,
}

impl Ready {
    /// Checks `bytes` as the image of `key` and fills its memory object.
    pub fn open(graph: GraphKey, key: ImageKey, bytes: Box<[u8]>) -> Result<Ready, PremapError> {
        let memory = Memory::of(&View::parse(&bytes, key)?)?;
        Ok(Ready { key, graph, bytes, memory })
    }

    pub fn view(&self) -> View<'_> {
        View::reopen(&self.bytes, self.key)
    }

    pub fn memory(&self) -> &Memory {
        &self.memory
    }
}

/// What a build links against.
pub enum Pick {
    /// This image: the build's key or the newest of its graph.
    Use(Arc<Ready>),
    /// No image of the graph is open and the build's key is being made: wait for it.
    Wait,
    /// No image of the graph is open or being made: make the build's key, after looking in
    /// the store, having claimed it ([`Images::finish`] gives it up).
    Make,
    /// Making the build's key failed: deterministic, so it fails again.
    Failed(PremapError),
}

#[derive(Default)]
pub struct Images {
    open: FxHashMap<ImageKey, Arc<Ready>>,
    newest: FxHashMap<GraphKey, ImageKey>,
    making: FxHashSet<ImageKey>,
    failed: FxHashMap<ImageKey, PremapError>,
    used: FxHashMap<GraphKey, u64>,
    clock: u64,
}

impl Images {
    pub fn pick(&mut self, graph: GraphKey, key: ImageKey) -> Pick {
        self.clock += 1;
        self.used.insert(graph, self.clock);
        if let Some(ready) = self.open.get(&key) {
            return Pick::Use(ready.clone());
        }
        if let Some(error) = self.failed.get(&key) {
            return Pick::Failed(error.clone());
        }
        if let Some(newest) = self.newest.get(&graph) {
            return Pick::Use(self.open[newest].clone());
        }
        if self.making.insert(key) { Pick::Make } else { Pick::Wait }
    }

    /// Claims making `key` in the background, for a build that used an older image of its
    /// graph and spilled: `false` when it is open or being made already.
    pub fn claim(&mut self, key: ImageKey) -> bool {
        !self.open.contains_key(&key) && !self.failed.contains_key(&key) && self.making.insert(key)
    }

    /// The end of making `key`, claimed by [`Images::pick`] or [`Images::claim`]: it becomes
    /// its graph's newest image, and the image it replaces is closed once no run maps it.
    pub fn finish(&mut self, key: ImageKey, made: Result<Ready, PremapError>) -> Result<Arc<Ready>, PremapError> {
        assert!(self.making.remove(&key), "finishing a dependency image nothing claimed");
        let ready = match made {
            Ok(ready) => Arc::new(ready),
            Err(error) => {
                self.failed.insert(key, error.clone());
                return Err(error);
            }
        };
        assert!(ready.key == key, "finishing a dependency image under another key");
        if let Some(old) = self.newest.insert(ready.graph, key) {
            self.open.remove(&old);
        }
        self.open.insert(key, ready.clone());
        self.used.entry(ready.graph).or_insert(self.clock);
        while self.newest.len() > GRAPHS {
            let (&graph, _) = self
                .used
                .iter()
                .filter(|(graph, _)| self.newest.contains_key(graph))
                .min_by_key(|(_, at)| **at)
                .expect("more graphs than none");
            let key = self.newest.remove(&graph).expect("a graph with an image");
            self.open.remove(&key);
            self.used.remove(&graph);
        }
        Ok(ready)
    }

    pub fn get(&self, key: &ImageKey) -> Option<Arc<Ready>> {
        self.open.get(key).cloned()
    }

    pub fn len(&self) -> usize {
        self.open.len()
    }

    pub fn is_empty(&self) -> bool {
        self.open.is_empty()
    }
}
