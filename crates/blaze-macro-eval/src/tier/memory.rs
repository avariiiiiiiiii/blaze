//! A compiled expansion's memory as the shims reach it: its [`Region`] and its [`Heap`],
//! behind the same [`Guest`] the evaluator's memory is, so `os.rs`'s shims and the server
//! run unchanged over either tier.

use super::{
    heap::Heap,
    region::{Bounds, Layout, Region, cell},
};
use crate::guest::{Guest, Stop};

/// Heap bytes kept mapped between expansions: what a large derive's expansion reaches. An
/// expansion that reached more gives the rest back when it ends.
const KEEP: u32 = 64 << 20;

pub(crate) struct Memory {
    region: Region,
    heap: Heap,
}

impl Memory {
    /// A region holding an image of `writable` and `readonly` bytes, in `bounds`.
    pub(crate) fn new(writable: &[u8], readonly: &[u8], bounds: Bounds) -> Result<Memory, Stop> {
        let layout = Layout::new(len(writable)?, len(readonly)?, bounds)?;
        let heap = Heap::new(layout.heap.start..layout.heap.end);
        let mut region = Region::new(layout)?;
        region.install(writable, readonly)?;
        Ok(Memory { region, heap })
    }

    /// Ready for the next expansion: statics as `writable` gives them (the image's, the
    /// same every time), an empty heap and stack.
    pub(crate) fn reset(&mut self, writable: &[u8]) -> Result<(), Stop> {
        self.region.reset(writable)?;
        if self.heap.reached() > KEEP {
            self.region.shrink_heap(KEEP)?;
        }
        self.heap.reset();
        Ok(())
    }

    pub(crate) fn region(&self) -> &Region {
        &self.region
    }

    /// The region and the heap at once, for a shim that allocates and writes.
    pub(crate) fn parts(&mut self) -> (&mut Region, &mut Heap) {
        (&mut self.region, &mut self.heap)
    }

    /// The address of the running thread's `errno`: what `__errno_location` (Linux) and
    /// `__error` (macOS) return.
    pub(crate) fn errno_address(&self) -> u64 {
        u64::from(Layout::HEADER + cell::ERRNO)
    }
}

fn len(bytes: &[u8]) -> Result<u32, Stop> {
    // alloc: error path
    u32::try_from(bytes.len()).map_err(|_| Stop(format!("has an image of {} bytes", bytes.len())))
}

impl Guest for Memory {
    fn read(&self, addr: u64, len: usize) -> Result<&[u8], Stop> {
        self.region.read(addr, len)
    }

    fn write(&mut self, addr: u64, bytes: &[u8]) -> Result<(), Stop> {
        self.region.write(addr, bytes)
    }

    fn set_errno(&mut self, code: i32) -> Result<(), Stop> {
        let at = self.errno_address();
        self.region.write(at, &code.to_le_bytes())
    }

    fn alloc(&mut self, bytes: &[u8], align: usize) -> Result<u64, Stop> {
        let at = self.heap.alloc(&mut self.region, bytes.len() as u64, align as u64)?;
        self.region.write(u64::from(at), bytes)?;
        Ok(u64::from(at))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SMALL: Bounds = Bounds { stack: 1 << 16, heap: 16 << 20 };

    #[test]
    fn the_shims_reach_it_as_they_reach_the_evaluator() {
        let mut memory = Memory::new(b"static\0", b"const", SMALL).unwrap();
        let at = memory.alloc(b"VALUE=1\0", 1).unwrap();
        assert_eq!(memory.read_c_str(at, 100).unwrap(), b"VALUE=1");
        memory.set_errno(2).unwrap();
        assert_eq!(memory.read(memory.errno_address(), 4).unwrap(), &2i32.to_le_bytes());
        let statics = u64::from(memory.region().layout().writable.start);
        assert_eq!(memory.read_c_str(statics, 100).unwrap(), b"static");
        memory.write_u64(at, 7).unwrap();
        assert_eq!(memory.read_u64(at).unwrap(), 7);
    }

    #[test]
    fn reset_starts_each_expansion_from_the_image() {
        let mut memory = Memory::new(&[1; 8], &[], SMALL).unwrap();
        let statics = u64::from(memory.region().layout().writable.start);
        memory.write(statics, &[9; 8]).unwrap();
        let first = memory.alloc(&[5; 40], 8).unwrap();
        memory.set_errno(4).unwrap();
        memory.reset(&[1; 8]).unwrap();
        assert_eq!(memory.read(statics, 8).unwrap(), &[1; 8]);
        assert_eq!(memory.read(memory.errno_address(), 4).unwrap(), &[0; 4]);
        assert_eq!(memory.alloc(&[6; 40], 8).unwrap(), first, "the heap starts over");
    }
}
