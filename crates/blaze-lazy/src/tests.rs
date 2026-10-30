mod abi;
mod exec;
mod flight;
mod plan;
mod refuse;
mod sim;
mod stubs;
mod wire;

use blaze_codegen::{Arch, CompiledFunction, InstanceKey, Reloc, RelocKind, Target};
use blaze_ids::ContentId;
use blaze_items::{CodeHash, ItemKey};

use crate::plan::Function;

/// SplitMix64: the simulations' and property tests' randomness, the same for a seed on every
/// machine.
pub(crate) struct Rng(u64);

impl Rng {
    pub(crate) fn new(seed: u64) -> Rng {
        Rng(seed)
    }

    pub(crate) fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// Below `n`, which is not 0.
    pub(crate) fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    /// True one time in `n`.
    pub(crate) fn one_in(&mut self, n: u64) -> bool {
        self.below(n) == 0
    }
}

pub(crate) fn id(domain: &'static str, n: u32) -> ContentId {
    ContentId::of(domain, &n.to_le_bytes())
}

pub(crate) fn key(n: u32) -> InstanceKey {
    InstanceKey(id("blaze-lazy test instance", n))
}

pub(crate) fn function(n: u32) -> Function {
    Function { key: key(n), code: CodeHash(id("blaze-lazy test code", n)), item: ItemKey(id("blaze-lazy test item", n)) }
}

/// A compiled x86-64 function calling each of `callees` (`call rel32`) and taking the
/// address of each of `addressed` (`lea rel32`).
pub(crate) fn compiled(n: u32, callees: &[u32], addressed: &[u32]) -> CompiledFunction {
    let mut code = Vec::new();
    let mut relocs = Vec::new();
    for &callee in callees {
        code.push(0xe8);
        relocs.push(Reloc { at: code.len() as u32, kind: RelocKind::PcRel32, target: Target::Function(key(callee)), addend: -4 });
        code.extend_from_slice(&[0; 4]);
    }
    for &other in addressed {
        code.extend_from_slice(&[0x48, 0x8d, 0x05]);
        relocs.push(Reloc { at: code.len() as u32, kind: RelocKind::PcRel32, target: Target::Function(key(other)), addend: -4 });
        code.extend_from_slice(&[0; 4]);
    }
    code.push(0xc3);
    CompiledFunction {
        key: key(n),
        arch: Arch::X86_64,
        align: 16,
        code,
        relocs,
        data: Vec::new(),
        inlined: Vec::new(),
        side: None,
        unwind: None,
        lines: Vec::new(),
    }
}
