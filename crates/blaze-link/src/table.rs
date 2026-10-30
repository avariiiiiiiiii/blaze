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

/// The stub of `slot`: its bytes, and the fixups that point it at the table cell.
pub(crate) fn stub(arch: Arch, slot: u32) -> (Vec<u8>, Vec<StubFixup>) {
    let offset = u64::from(slot) * 8;
    match arch {
        Arch::X86_64 => {
            let disp = u32::try_from(offset).expect("a slot below 2^28");
            let mut code = vec![0x4c, 0x8b, 0x1d, 0, 0, 0, 0, 0x41, 0xff, 0xa3];
            code.extend_from_slice(&disp.to_le_bytes());
            code.extend_from_slice(&[0xcc, 0xcc]);
            (code, vec![StubFixup { at: 3, apply: Apply::Rel32, addend: -4 }])
        }
        Arch::Aarch64 => {
            let low = (offset & 0xffff) as u32;
            let high = ((offset >> 16) & 0xffff) as u32;
            let words = [
                0x9000_0010,               // adrp x16, cell
                0xf940_0210,               // ldr x16, [x16, :lo12:cell]
                0xd280_0011 | (low << 5),  // movz x17, #low
                0xf2a0_0011 | (high << 5), // movk x17, #high, lsl #16
                0xf871_6a10,               // ldr x16, [x16, x17]
                0xd61f_0200,               // br x16
            ];
            let code = words.iter().flat_map(|it: &u32| it.to_le_bytes()).collect();
            let fixups = vec![
                StubFixup { at: 0, apply: Apply::A64Page21, addend: 0 },
                StubFixup { at: 4, apply: Apply::A64Lo12 { shift: 3 }, addend: 0 },
            ];
            (code, fixups)
        }
        Arch::Wasm32 => (Vec::new(), Vec::new()),
    }
}

/// The size of one PLT stub: a jump through an import's GOT cell.
pub(crate) fn plt_size(arch: Arch) -> u64 {
    match arch {
        Arch::X86_64 | Arch::Aarch64 => 16,
        Arch::Wasm32 => 0,
    }
}

/// A PLT stub: its bytes, and the fixups that point it at the GOT cell.
pub(crate) fn plt(arch: Arch) -> (Vec<u8>, Vec<StubFixup>) {
    match arch {
        Arch::X86_64 => {
            let mut code = vec![0xff, 0x25, 0, 0, 0, 0];
            code.resize(16, 0xcc);
            (code, vec![StubFixup { at: 2, apply: Apply::Rel32, addend: -4 }])
        }
        Arch::Aarch64 => {
            let words: [u32; 4] = [
                0x9000_0010, // adrp x16, cell
                0xf940_0211, // ldr x17, [x16, :lo12:cell]
                0xd61f_0220, // br x17
                0xd420_0000, // brk #0
            ];
            let code = words.iter().flat_map(|it| it.to_le_bytes()).collect();
            let fixups = vec![
                StubFixup { at: 0, apply: Apply::A64Page21, addend: 0 },
                StubFixup { at: 4, apply: Apply::A64Lo12 { shift: 3 }, addend: 0 },
            ];
            (code, fixups)
        }
        Arch::Wasm32 => (Vec::new(), Vec::new()),
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
    fn capacity_leaves_room_only_when_incremental() {
        assert_eq!(capacity(10, false), 10);
        assert_eq!(capacity(10, true), 10 + (1 << 16));
        assert_eq!(capacity(1 << 20, true), 2 << 20);
    }
}
