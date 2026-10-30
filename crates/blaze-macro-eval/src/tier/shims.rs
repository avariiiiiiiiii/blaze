//! The compiled tier's shims (macros-tier.md, "Running"): what the call of each stub runs,
//! over the expansion's [`Memory`], as [`Dispatch`] for the machine's dispatcher. Every
//! address a shim is given is a guest offset, every access checked by the region as the
//! evaluator checks its own; a shim that can't do what it is asked stops the expansion
//! naming it ([`Escape::Stop`]), and the unwinder's raise goes up the macro's frames
//! ([`Escape::Raise`]).
//!
//! The run's shims (`os.rs`'s) run unchanged over the region, and the calls they give back
//! to the evaluator are answered as its thread shims answer them ([`Os::as_one_thread`]). The
//! bridge's `Closure::call` is the run's ([`Bridge`]): it may enter the macro again (the
//! buffer's `reserve`), with these shims.

use super::{
    bind::{Own, Refusal, Why},
    machine::{Dispatch, Escape, Running},
    memory::Memory,
    region::SPAN,
};
use crate::{
    guest::{Guest, Stop},
    os::{Answer, Os, Shim},
};

/// The most parameters a shim of the run's takes: `syscall`'s number and six.
const MOST_PARAMS: usize = 8;

/// `_Unwind_Backtrace`'s result when the walk ends: it walks no frames.
const URC_END_OF_STACK: u64 = 5;

/// What the stub of a binding runs.
#[derive(Debug)]
pub(crate) enum Bound {
    Own(Own),
    Os(Shim),
    /// The bridge's `Closure::call`.
    Bridge,
    /// A function with no shim, by its symbol: its call stops the expansion.
    Unshimmed(String),
}

/// One stub: its binding and the widths of its values.
#[derive(Debug)]
pub(crate) struct Stub {
    pub(crate) bound: Bound,
    /// The width in bytes of each parameter but `vmctx`.
    pub(crate) params: Vec<u8>,
    /// The width of its result, 0 for none.
    pub(crate) ret: u8,
}

impl Stub {
    /// Its slots, as [`super::glue::stub`] makes them: one per parameter (`vmctx` too) or
    /// result, whichever are more.
    pub(crate) fn slots(&self) -> usize {
        (self.params.len() + 1).max(usize::from(self.ret > 0))
    }
}

/// The bridge's `Closure::call`, which the run answers: its server and the client's buffer.
pub(crate) trait Bridge {
    /// The call with its stub's `slots` (its arguments, `vmctx` last; its result written to
    /// the first), entering the macro again through `shims` and `running` when it must.
    fn call(&mut self, shims: &mut Shims<'_, '_>, running: &Running<'_>, slots: &mut [[u8; 16]]) -> Result<(), Escape>;
}

impl From<Stop> for Escape {
    fn from(stop: Stop) -> Escape {
        Escape::Stop(stop)
    }
}

/// The shims of one expansion.
pub(crate) struct Shims<'s, 'e> {
    stubs: &'s [Stub],
    memory: &'s mut Memory,
    os: &'s mut Os<'e>,
    bridge: Option<&'s mut dyn Bridge>,
}

impl<'s, 'e> Shims<'s, 'e> {
    pub(crate) fn new(stubs: &'s [Stub], memory: &'s mut Memory, os: &'s mut Os<'e>, bridge: &'s mut dyn Bridge) -> Self {
        Shims { stubs, memory, os, bridge: Some(bridge) }
    }

    pub(crate) fn memory(&mut self) -> &mut Memory {
        self.memory
    }

    fn os(&mut self, shim: Shim, stub: &Stub, slots: &mut [[u8; 16]]) -> Result<(), Escape> {
        if stub.params.len() > MOST_PARAMS || stub.ret > 16 {
            // alloc: error path
            return Err(Stop(format!("calls a shim of {} parameters, more than any blaze has", stub.params.len())).into());
        }
        let mut given = [[0u8; 16]; MOST_PARAMS];
        given[..stub.params.len()].copy_from_slice(&slots[..stub.params.len()]);
        let mut args: [&[u8]; MOST_PARAMS] = [&[]; MOST_PARAMS];
        for ((arg, bytes), &width) in args.iter_mut().zip(&given).zip(&stub.params) {
            *arg = &bytes[..usize::from(width)];
        }
        let args = &args[..stub.params.len()];
        slots[0] = [0; 16];
        let ret = &mut slots[0][..usize::from(stub.ret)];
        if self.os.call(shim, &mut *self.memory, args, ret)? == Answer::Evaluator {
            self.os.as_one_thread(shim, &mut *self.memory, args, ret)?;
        }
        Ok(())
    }

    /// The tier's own shim `own` of the words `a`, its result to `ret`.
    fn own(&mut self, own: Own, a: &[u64; MOST_PARAMS], ret: &mut [u8; 16]) -> Result<(), Escape> {
        let n = || match usize::try_from(a[2]) {
            Ok(n) if a[2] <= SPAN => Ok(n),
            // alloc: error path
            _ => Err(Stop(format!("passed a length of {} bytes, past its memory", a[2]))),
        };
        let (region, heap) = self.memory.parts();
        let value = match own {
            Own::Alloc => u64::from(heap.alloc(region, a[0], a[1])?),
            Own::AllocZeroed => u64::from(heap.alloc_zeroed(region, a[0], a[1])?),
            Own::Dealloc => {
                heap.dealloc(a[0], a[1], a[2])?;
                0
            }
            Own::Realloc => u64::from(heap.realloc(region, a[0], a[1], a[2], a[3])?),
            Own::AllocError => {
                // alloc: error path
                return Err(Stop(format!("ran out of memory asking for {} bytes aligned to {}", a[0], a[1])).into());
            }
            Own::Malloc => u64::from(heap.c_alloc(region, a[0], 16, false)?),
            Own::Calloc => match a[0].checked_mul(a[1]) {
                Some(size) => u64::from(heap.c_alloc(region, size, 16, true)?),
                None => 0,
            },
            Own::CRealloc => u64::from(heap.c_realloc(region, a[0], a[1])?),
            Own::Free => {
                heap.c_free(a[0])?;
                0
            }
            Own::PosixMemalign => {
                let at = heap.c_alloc(region, a[2], a[1].max(16), false)?;
                region.write(a[0], &u64::from(at).to_le_bytes())?;
                0
            }
            Own::AlignedAlloc => u64::from(heap.c_alloc(region, a[1], a[0].max(16), false)?),
            Own::Memcpy | Own::Memmove => {
                region.copy_within(a[1], a[0], n()?)?;
                a[0]
            }
            Own::Memset => {
                region.fill(a[0], n()?, a[1] as u8)?;
                a[0]
            }
            Own::Memcmp => {
                let n = n()?;
                let order = region.read(a[0], n)?.cmp(region.read(a[1], n)?);
                u64::from(order as i32 as u32)
            }
            Own::Memchr => {
                let found = region.read(a[0], n()?)?.iter().position(|&it| it == a[1] as u8);
                found.map_or(0, |at| a[0] + at as u64)
            }
            Own::Memrchr => {
                let found = region.read(a[0], n()?)?.iter().rposition(|&it| it == a[1] as u8);
                found.map_or(0, |at| a[0] + at as u64)
            }
            Own::Strlen => self.memory.read_c_str(a[0], SPAN as usize)?.len() as u64,
            Own::Errno => self.memory.errno_address(),
            Own::Abort => return Err(Stop("called `abort`".to_owned()).into()), // alloc: error path
            Own::RaiseException | Own::UnwindResume => return Err(Escape::Raise(a[0])),
            Own::DeleteException => {
                // alloc: error path
                return Err(Stop("deleted a foreign exception, which no macro has".to_owned()).into());
            }
            Own::Backtrace => URC_END_OF_STACK,
        };
        ret[..8].copy_from_slice(&value.to_le_bytes());
        ret[8..].fill(0);
        Ok(())
    }
}

impl Dispatch for Shims<'_, '_> {
    fn slots(&self, id: u32) -> Option<usize> {
        self.stubs.get(id as usize).map(Stub::slots)
    }

    fn call(&mut self, running: &Running<'_>, id: u32, slots: &mut [[u8; 16]]) -> Result<(), Escape> {
        let stubs = self.stubs;
        let Some(stub) = stubs.get(id as usize) else {
            return Err(Stop(format!("called binding {id}, which it has none of")).into()); // alloc: error path
        };
        match &stub.bound {
            Bound::Own(own) => {
                let mut words = [0u64; MOST_PARAMS];
                for (word, slot) in words.iter_mut().zip(slots.iter()).take(stub.params.len()) {
                    *word = u64::from_le_bytes(slot[..8].try_into().expect("a slot is 16 bytes"));
                }
                self.own(*own, &words, &mut slots[0])
            }
            Bound::Os(shim) => self.os(*shim, stub, slots),
            Bound::Bridge => {
                let Some(bridge) = self.bridge.take() else {
                    // alloc: error path
                    return Err(Stop("called the bridge's dispatch from inside it".to_owned()).into());
                };
                let done = bridge.call(self, running, slots);
                self.bridge = Some(bridge);
                done
            }
            Bound::Unshimmed(symbol) => {
                // alloc: error path
                Err(Refusal { name: symbol.clone(), why: Why::Function }.stop().into()) // clone: error path
            }
        }
    }
}

#[cfg(test)]
mod tests;
