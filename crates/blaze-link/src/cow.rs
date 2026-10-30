//! A vector cloned in O(len / CHUNK): chunks are shared until written, so a stage's copy of
//! the image's state costs a pointer per chunk, and only what it changes is copied.

use std::sync::Arc;

const CHUNK: usize = 1024;

#[derive(Debug)]
pub(crate) struct CowVec<T> {
    chunks: Vec<Arc<Vec<T>>>,
    len: usize,
}

impl<T> Clone for CowVec<T> {
    fn clone(&self) -> Self {
        CowVec { chunks: self.chunks.clone(), len: self.len }
    }
}

impl<T> Default for CowVec<T> {
    fn default() -> Self {
        CowVec { chunks: Vec::new(), len: 0 }
    }
}

impl<T: Clone> CowVec<T> {
    pub fn len(&self) -> usize {
        self.len
    }

    pub fn get(&self, at: usize) -> Option<&T> {
        if at >= self.len {
            return None;
        }
        Some(&self.chunks[at / CHUNK][at % CHUNK])
    }

    pub fn get_mut(&mut self, at: usize) -> Option<&mut T> {
        if at >= self.len {
            return None;
        }
        Some(&mut Arc::make_mut(&mut self.chunks[at / CHUNK])[at % CHUNK])
    }

    pub fn push(&mut self, value: T) -> usize {
        let at = self.len;
        if at % CHUNK == 0 {
            self.chunks.push(Arc::new(Vec::with_capacity(CHUNK)));
        }
        Arc::make_mut(self.chunks.last_mut().expect("a chunk was just made")).push(value);
        self.len += 1;
        at
    }

    /// Sets `at`, growing the vector with `fill` to reach it.
    pub fn set(&mut self, at: usize, value: T, fill: impl Fn() -> T) {
        while self.len <= at {
            self.push(fill());
        }
        *self.get_mut(at).expect("grown to it") = value;
    }

    pub fn iter(&self) -> impl Iterator<Item = &T> {
        self.chunks.iter().flat_map(|chunk| chunk.iter())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_clone_shares_until_written() {
        let mut a = CowVec::default();
        for n in 0..3000u32 {
            a.push(n);
        }
        let mut b = a.clone();
        *b.get_mut(5).unwrap() = 99;
        b.set(4000, 7, || 0);
        assert_eq!(a.get(5), Some(&5));
        assert_eq!(b.get(5), Some(&99));
        assert_eq!(a.len(), 3000);
        assert_eq!(b.len(), 4001);
        assert_eq!(b.get(3500), Some(&0));
        assert!(Arc::ptr_eq(&a.chunks[1], &b.chunks[1]));
        assert!(!Arc::ptr_eq(&a.chunks[0], &b.chunks[0]));
        assert_eq!(a.iter().copied().sum::<u32>(), (0..3000).sum());
    }
}
