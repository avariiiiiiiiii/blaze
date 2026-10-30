//! The jump table: the patch table of #275 (docs/v2/plan/lanes/link/notes.md, "The jump table").
//!
//! A call to a function through its slot goes to the slot's stub, which loads the table
//! cell (`Class::Table`'s first word: the address of the current table), then the slot's
//! entry of that table, and jumps there. A link that moves a function changes one entry; a
//! running program switches every entry at once by storing another table's address in the
//! table cell (blaze-live), and rolls a patch back by storing the old one.
//!
//! The stubs use only registers a call may clobber and no argument is passed in: `r11` on
//! x86-64 (neither SysV nor Win64 nor Cranelift passes arguments in it; `r10` is SysV's
//! static chain), `x16` and `x17` on AArch64 (the intra-procedure-call registers AAPCS64
//! sets aside for veneers).

use blaze_codegen::Arch;
use rayon::prelude::*;

use crate::model::Apply;

/// A fixup of a generated stub: at `at`, to the table cell or a GOT cell.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct StubFixup {
    pub at: u64,
    pub apply: Apply,
    pub addend: i64,
}

/// The size of one slot's stub.
pub(crate) fn stub_size(arch: Arch) -> u64 {
    match arch {
        Arch::X86_64 => 16,
        Arch::Aarch64 => 24,
        Arch::Wasm32 => 0,
    }
}

/// `mov r11, [rip + cell]; jmp [r11 + disp32]; int3; int3`, the displacement at 10.
const X86_STUB: [u8; 16] = [0x4c, 0x8b, 0x1d, 0, 0, 0, 0, 0x41, 0xff, 0xa3, 0, 0, 0, 0, 0xcc, 0xcc];
const X86_STUB_FIXUPS: [StubFixup; 1] = [StubFixup { at: 3, apply: Apply::Rel32, addend: -4 }];

/// adrp x16, cell; ldr x16, [x16, :lo12:cell]; movz x17, #low; movk x17, #high, lsl #16;
/// ldr x16, [x16, x17]; br x16.
const A64_STUB: [u32; 6] = [0x9000_0010, 0xf940_0210, 0xd280_0011, 0xf2a0_0011, 0xf871_6a10, 0xd61f_0200];
const A64_PAGE_FIXUPS: [StubFixup; 2] = [
    StubFixup { at: 0, apply: Apply::A64Page21, addend: 0 },
    StubFixup { at: 4, apply: Apply::A64Lo12 { shift: 3 }, addend: 0 },
];

/// The fixups of every slot's stub, from the stub's start: each to the table cell.
pub(crate) fn stub_fixups(arch: Arch) -> &'static [StubFixup] {
    match arch {
        Arch::X86_64 => &X86_STUB_FIXUPS,
        Arch::Aarch64 => &A64_PAGE_FIXUPS,
        Arch::Wasm32 => &[],
    }
}

/// Writes the stub of `slot` into `out`, [`stub_size`] bytes; its fixups are [`stub_fixups`].
pub(crate) fn write_stub(arch: Arch, slot: u32, out: &mut [u8]) {
    let offset = u64::from(slot) * 8;
    match arch {
        Arch::X86_64 => {
            let disp = u32::try_from(offset).expect("a slot below 2^29");
            out[..16].copy_from_slice(&X86_STUB);
            out[10..14].copy_from_slice(&disp.to_le_bytes());
        }
        Arch::Aarch64 => {
            let low = (offset & 0xffff) as u32;
            let high = ((offset >> 16) & 0xffff) as u32;
            let mut words = A64_STUB;
            words[2] |= low << 5;
            words[3] |= high << 5;
            for (to, word) in out[..24].chunks_exact_mut(4).zip(words) {
                to.copy_from_slice(&word.to_le_bytes());
            }
        }
        Arch::Wasm32 => {}
    }
}

/// The stubs of slots from 0, one after another, written in parallel over the whole of `out`
/// (a multiple of [`stub_size`] long).
pub(crate) fn stubs(arch: Arch, out: &mut [u8]) {
    let size = stub_size(arch) as usize;
    if size == 0 {
        return;
    }
    out.par_chunks_mut(size).with_min_len(4096).enumerate().for_each(|(slot, out)| {
        write_stub(arch, slot as u32, out);
    });
}

/// The stub of `slot`: its bytes and fixups.
#[cfg(test)]
pub(crate) fn stub(arch: Arch, slot: u32) -> (Vec<u8>, &'static [StubFixup]) {
    let mut code = vec![0; stub_size(arch) as usize];
    write_stub(arch, slot, &mut code);
    (code, stub_fixups(arch))
}

/// The size of one PLT stub: a jump through an import's GOT cell.
pub(crate) fn plt_size(arch: Arch) -> u64 {
    match arch {
        Arch::X86_64 | Arch::Aarch64 => 16,
        Arch::Wasm32 => 0,
    }
}

/// `jmp [rip + cell]`, padded with `int3`.
const X86_PLT: [u8; 16] = [0xff, 0x25, 0, 0, 0, 0, 0xcc, 0xcc, 0xcc, 0xcc, 0xcc, 0xcc, 0xcc, 0xcc, 0xcc, 0xcc];
const X86_PLT_FIXUPS: [StubFixup; 1] = [StubFixup { at: 2, apply: Apply::Rel32, addend: -4 }];
/// adrp x16, cell; ldr x17, [x16, :lo12:cell]; br x17; brk #0.
const A64_PLT: [u8; 16] = words_le([0x9000_0010, 0xf940_0211, 0xd61f_0220, 0xd420_0000]);

const fn words_le(words: [u32; 4]) -> [u8; 16] {
    let mut out = [0; 16];
    let mut at = 0;
    while at < 4 {
        let bytes = words[at].to_le_bytes();
        out[at * 4] = bytes[0];
        out[at * 4 + 1] = bytes[1];
        out[at * 4 + 2] = bytes[2];
        out[at * 4 + 3] = bytes[3];
        at += 1;
    }
    out
}

/// A PLT stub: its bytes, and the fixups that point it at the GOT cell. Both static: making a
/// stub allocates nothing.
pub(crate) fn plt(arch: Arch) -> (&'static [u8], &'static [StubFixup]) {
    match arch {
        Arch::X86_64 => (&X86_PLT, &X86_PLT_FIXUPS),
        Arch::Aarch64 => (&A64_PLT, &A64_PAGE_FIXUPS),
        Arch::Wasm32 => (&[], &[]),
    }
}

/// How many slots a table made for `functions` has room for: with room to grow for an
/// incremental image, as many as there are otherwise.
pub(crate) fn capacity(functions: u64, incremental: bool) -> u64 {
    if incremental { (functions * 2).max(functions + (1 << 16)) } else { functions }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reloc::{Ctx, apply};

    fn relocate(arch: Arch, code: &mut [u8], fixups: &[StubFixup], p: u64, s: u64) {
        for fixup in fixups {
            let ctx = Ctx {
                arch,
                format: crate::Format::Elf,
                s,
                a: fixup.addend,
                p: p + fixup.at,
                g: 0,
                m: 0,
                base: 0,
                tls: None,
            };
            apply(fixup.apply, &ctx, &mut code[fixup.at as usize..]).unwrap();
        }
    }

    #[test]
    fn an_x86_stub_loads_the_cell_then_jumps_through_the_slot() {
        let (mut code, fixups) = stub(Arch::X86_64, 5);
        assert_eq!(code.len() as u64, stub_size(Arch::X86_64));
        relocate(Arch::X86_64, &mut code, &fixups, 0x1000, 0x5000);
        // mov r11, [rip + disp]: rip is the end of the instruction, 0x1007.
        assert_eq!(&code[..3], [0x4c, 0x8b, 0x1d]);
        assert_eq!(i32::from_le_bytes(code[3..7].try_into().unwrap()), 0x5000 - 0x1007);
        // jmp [r11 + 5 * 8].
        assert_eq!(&code[7..10], [0x41, 0xff, 0xa3]);
        assert_eq!(u32::from_le_bytes(code[10..14].try_into().unwrap()), 40);
    }

    #[test]
    fn an_aarch64_stub_reaches_any_of_2_pow_29_slots() {
        let slot = 0x1_2345;
        let (mut code, fixups) = stub(Arch::Aarch64, slot);
        assert_eq!(code.len() as u64, stub_size(Arch::Aarch64));
        relocate(Arch::Aarch64, &mut code, &fixups, 0x10_0000, 0x40_0008);
        let word = |at: usize| u32::from_le_bytes(code[at..at + 4].try_into().unwrap());
        let offset = u64::from(slot) * 8;
        assert_eq!((word(8) >> 5) & 0xffff, (offset & 0xffff) as u32);
        assert_eq!((word(12) >> 5) & 0xffff, (offset >> 16) as u32);
        // The cell's low 12 bits, scaled for the 8-byte load.
        assert_eq!((word(4) >> 10) & 0xfff, 8 >> 3);
    }

    #[test]
    fn the_bulk_stubs_are_each_slots_stub() {
        for arch in [Arch::X86_64, Arch::Aarch64] {
            let size = stub_size(arch) as usize;
            let mut all = vec![0; size * 9000];
            stubs(arch, &mut all);
            assert_eq!(all.len(), size * 9000);
            for slot in [0u32, 1, 4095, 4096, 8191, 8999] {
                let (one, _) = stub(arch, slot);
                assert_eq!(&all[slot as usize * size..][..size], &one[..], "{arch:?} slot {slot}");
            }
        }
        let (x86, fixups) = plt(Arch::X86_64);
        assert_eq!((x86.len() as u64, &x86[..2], fixups[0].at), (plt_size(Arch::X86_64), &[0xff, 0x25][..], 2));
        let (a64, _) = plt(Arch::Aarch64);
        assert_eq!(u32::from_le_bytes(a64[8..12].try_into().unwrap()), 0xd61f_0220);
    }

    #[test]
    fn capacity_leaves_room_only_when_incremental() {
        assert_eq!(capacity(10, false), 10);
        assert_eq!(capacity(10, true), 10 + (1 << 16));
        assert_eq!(capacity(1 << 20, true), 2 << 20);
    }
}
