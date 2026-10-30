//! The back-end memo (#1291): templates by the shape of what was compiled, so two instances
//! whose prepared IR is equal up to what their places hold compile once.
//!
//! Keyed by [`crate::backend::Shape::key`], which holds the settings, the architecture and the IR's canonical
//! bytes in full: BLAKE3 over all of it, so a hit is the same compile and not a near one.
//! Shared by every worker of a build and every build of the daemon; sharded so workers that
//! insert at once don't wait on each other.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use blaze_ids::ContentId;

use crate::template::Template;

const SHARDS: usize = 64;

pub struct Memo {
    shards: Vec<Mutex<HashMap<ContentId, Arc<Template>>>>,
}

impl Default for Memo {
    fn default() -> Memo {
        Memo::new()
    }
}

impl Memo {
    pub fn new() -> Memo {
        Memo { shards: (0..SHARDS).map(|_| Mutex::new(HashMap::new())).collect() }
    }

    fn shard(&self, shape: &ContentId) -> &Mutex<HashMap<ContentId, Arc<Template>>> {
        &self.shards[shape.0[0] as usize % SHARDS]
    }

    pub fn get(&self, shape: &ContentId) -> Option<Arc<Template>> {
        let shard = self.shard(shape).lock().unwrap_or_else(|it| it.into_inner());
        shard.get(shape).cloned()
    }

    /// Keeps `template` as what `shape` compiles to, and gives what is kept: the first, if
    /// two workers compiled one shape at once. Both are the same compile; the first is kept
    /// so every instantiation of the shape in a build shares one template.
    pub fn put(&self, shape: ContentId, template: Template) -> Arc<Template> {
        let mut shard = self.shard(&shape).lock().unwrap_or_else(|it| it.into_inner());
        shard.entry(shape).or_insert_with(|| Arc::new(template)).clone()
    }

    pub fn len(&self) -> usize {
        self.shards.iter().map(|it| it.lock().unwrap_or_else(|it| it.into_inner()).len()).sum()
    }

    /// Drops every template not of `kept`: what the daemon's collection keeps reachable.
    pub fn retain(&self, kept: &dyn Fn(&ContentId) -> bool) {
        for shard in &self.shards {
            shard.lock().unwrap_or_else(|it| it.into_inner()).retain(|shape, _| kept(shape));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Arch;

    fn template(byte: u8) -> Template {
        Template {
            arch: Arch::Aarch64,
            align: 4,
            code: vec![byte; 4],
            relocs: Vec::new(),
            data: Vec::new(),
            inlined: Vec::new(),
            side: None,
            refs: 0,
        }
    }

    #[test]
    fn the_first_template_of_a_shape_is_kept() {
        let memo = Memo::new();
        let shape = ContentId([5; 32]);
        assert!(memo.get(&shape).is_none());
        assert_eq!(*memo.put(shape, template(1)), template(1));
        assert_eq!(*memo.put(shape, template(2)), template(1));
        assert_eq!(*memo.get(&shape).unwrap(), template(1));
        assert_eq!(memo.len(), 1);
    }

    #[test]
    fn collection_keeps_what_it_is_told() {
        let memo = Memo::new();
        memo.put(ContentId([1; 32]), template(1));
        memo.put(ContentId([2; 32]), template(2));
        memo.retain(&|shape| shape.0[0] == 2);
        assert!(memo.get(&ContentId([1; 32])).is_none());
        assert!(memo.get(&ContentId([2; 32])).is_some());
    }
}
