//! The compiled tier's loader (macros-tier.md, "Running"): a macro crate's confined functions
//! and the shim stubs, compiled by copy-and-patch, laid out in one executable mapping with
//! their relocations resolved, the call table filled, and the region's image (statics,
//! constants, `Location`s) laid out with its own relocations resolved to guest offsets and
//! table pointers. The image is the same for every region, so one load serves every
//! expansion of the crate's version.
//!
//! Every target is resolved before the loader runs ([`To`]): the loader only places and
//! patches, and a relocation it can't write is an error naming the function, never a guess.
//! The code is written through the slices [`Executable::written`] gives at the addresses it
//! runs at, so nothing here is unsafe.

use blaze_inproc::exec::Executable;
use cranelift_codegen::{binemit::Reloc as Kind, ir};

use super::{
    region::{Bounds, GRANULE, Layout},
    table::{Sig, Table},
};
use crate::guest::Stop;

/// What a relocation resolves to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum To {
    /// Function `n`'s code (of [`Input::functions`]).
    Code(u32),
    /// Function `n`'s table pointer: the value a confined function pointer is.
    Pointer(u32),
    /// A host function, called directly: libm, compiler-rt, the copies stencil's ABI makes.
    Host(u64),
    /// Blob `n`'s guest offset (of [`Input::blobs`]).
    Blob(u32),
    /// A guest offset outside every blob: a cell of the header.
    Guest(u32),
    /// The table's host address and its size, which each indirect call checks against.
    Table,
    TableSize,
}

/// One relocation of a function's code, its target resolved.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct CodeReloc {
    pub(crate) at: u32,
    pub(crate) kind: Kind,
    pub(crate) to: To,
    pub(crate) addend: i64,
}

/// One function to place: a confined function of the macro, or a stub.
pub(crate) struct Function {
    /// Its name, for errors.
    pub(crate) name: String,
    pub(crate) bytes: Vec<u8>,
    /// A power of two.
    pub(crate) align: u32,
    pub(crate) relocs: Vec<CodeReloc>,
    /// Its table signature, when its address is taken: it gets an entry.
    pub(crate) entry: Option<Sig>,
    /// What the unwinder and the fault handler read of it.
    pub(crate) frame: u32,
    pub(crate) calls: Vec<Call>,
    pub(crate) traps: Vec<(u32, ir::TrapCode)>,
}

/// A call an unwind passes through, as stencil's `Code::calls` and `handlers` give it: where
/// it returns to, and where an unwind through it lands, if anywhere.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Call {
    pub(crate) ret: u32,
    pub(crate) landing: Option<u32>,
}

impl Call {
    /// The calls of `code`, each with the landing an unwind of any exception takes: a tagged
    /// handler is Cranelift's for a tag blaze's IR builder never makes, so the default one.
    pub(crate) fn of(code: &blaze_stencil::Code) -> Vec<Call> {
        code.calls
            .iter()
            .map(|call| {
                let handlers = code
                    .handlers
                    .get(call.handlers.start as usize..call.handlers.end as usize)
                    .expect("a call's handlers are its code's");
                let landing = handlers.iter().find_map(|it| match *it {
                    blaze_stencil::code::Handler::Default { landing } => Some(landing),
                    blaze_stencil::code::Handler::Tag { .. } => None,
                });
                Call { ret: call.ret, landing }
            })
            .collect() // alloc: once per function loaded
    }
}

/// One object of the region's image.
pub(crate) struct Blob {
    /// A power of two, at most [`GRANULE`].
    pub(crate) align: u32,
    pub(crate) init: Init,
    /// Written while it runs (a mutable static, a thread-local), so among the writable bytes
    /// that are copied back before each expansion.
    pub(crate) writable: bool,
    /// Its 8-byte absolute addresses: guest offsets and table pointers.
    pub(crate) relocs: Vec<(u32, To, i64)>,
}

pub(crate) enum Init {
    Bytes(Vec<u8>),
    Zeroed(u32),
}

impl Init {
    fn len(&self) -> usize {
        match self {
            Init::Bytes(bytes) => bytes.len(),
            Init::Zeroed(len) => *len as usize,
        }
    }
}

pub(crate) struct Input {
    pub(crate) functions: Vec<Function>,
    pub(crate) blobs: Vec<Blob>,
    pub(crate) bounds: Bounds,
}

/// A placed function, as the unwinder and the fault handler find it by address.
pub(crate) struct Placed {
    pub(crate) start: u64,
    pub(crate) len: u32,
    pub(crate) frame: u32,
    pub(crate) calls: Vec<Call>,
    pub(crate) traps: Vec<(u32, ir::TrapCode)>,
}

/// A loaded crate: its code, executable, its table, and its region's image.
pub(crate) struct Image {
    exec: Executable,
    code_len: usize,
    table: Table,
    /// By function, in address order.
    placed: Vec<Placed>,
    names: Vec<String>,
    pub(crate) layout: Layout,
    pub(crate) writable: Vec<u8>,
    pub(crate) readonly: Vec<u8>,
    /// Each function's table pointer, if it has an entry.
    pointers: Vec<Option<u64>>,
}

/// Bytes of a veneer: a jump through an address stored in it, for a call out of reach.
const VENEER: usize = 16;

impl Image {
    pub(crate) fn load(input: Input) -> Result<Image, Stop> {
        let Input { functions, blobs, bounds } = input;

        // The table's pointers are known before any code is placed: index times 16.
        let entries = functions.iter().filter(|it| it.entry.is_some()).count();
        let mut table = Table::with_capacity(entries);
        // alloc: once per image
        let mut pointers = Vec::with_capacity(functions.len());
        let mut next = 1u64;
        for function in &functions {
            pointers.push(function.entry.map(|_| {
                let pointer = next * super::table::ENTRY;
                next += 1;
                pointer
            }));
        }

        // The region's image: writable blobs, then read-only ones, each at its alignment.
        let (mut writable_len, mut readonly_len) = (0usize, 0usize);
        // alloc: once per image
        let mut at = Vec::with_capacity(blobs.len());
        for (n, blob) in blobs.iter().enumerate() {
            if !blob.align.is_power_of_two() || blob.align > GRANULE {
                // alloc: error path
                return Err(Stop(format!("has data {n} aligned to {} bytes, past the region's {GRANULE}", blob.align)));
            }
            let len = if blob.writable { &mut writable_len } else { &mut readonly_len };
            let start = len.next_multiple_of(blob.align as usize);
            at.push(start);
            *len = start + blob.init.len();
        }
        let size = |len: usize| {
            // alloc: error path
            u32::try_from(len).map_err(|_| Stop(format!("has {len} bytes of statics and constants, past 4 GiB")))
        };
        let layout = Layout::new(size(writable_len)?, size(readonly_len)?, bounds)?;
        let offsets: Vec<u32> = blobs
            .iter()
            .zip(&at)
            .map(|(blob, &start)| {
                let part = if blob.writable { layout.writable.start } else { layout.readonly.start };
                part + start as u32
            })
            .collect(); // alloc: once per image
        let resolve_data = |to: To| -> Result<u64, Stop> {
            Ok(match to {
                To::Blob(n) => u64::from(*offsets.get(n as usize).ok_or_else(|| Stop(format!("names data {n} of {}", offsets.len())))?), // alloc: error path
                To::Guest(at) => u64::from(at),
                To::Pointer(n) => pointer_of(&pointers, &functions, n)?,
                // alloc: error path
                other => return Err(Stop(format!("has data holding {other:?}, which no guest address is"))),
            })
        };
        // alloc: once per image, the bytes each region is made from
        let mut writable = vec![0u8; writable_len];
        let mut readonly = vec![0u8; readonly_len]; // alloc: as above
        for ((blob, &start), n) in blobs.iter().zip(&at).zip(0..) {
            let bytes = if blob.writable { &mut writable } else { &mut readonly };
            let end = start + blob.init.len();
            if let Init::Bytes(init) = &blob.init {
                bytes[start..end].copy_from_slice(init);
            }
            for &(field, to, addend) in &blob.relocs {
                let field = field as usize;
                if field + 8 > blob.init.len() {
                    // alloc: error path
                    return Err(Stop(format!("has a relocation at {field} of data {n}, past its {} bytes", blob.init.len())));
                }
                let value = resolve_data(to)?.wrapping_add(addend as u64);
                bytes[start + field..start + field + 8].copy_from_slice(&value.to_le_bytes());
            }
        }

        // The code: each function at its alignment, then one veneer per call that may need one.
        let mut starts = Vec::with_capacity(functions.len()); // alloc: once per image
        let mut end = 0usize;
        let mut calls = 0usize;
        for function in &functions {
            if !function.align.is_power_of_two() {
                // alloc: error path
                return Err(Stop(format!("has `{}` aligned to {} bytes", function.name, function.align)));
            }
            end = end.next_multiple_of(function.align.max(16) as usize);
            starts.push(end);
            end += function.bytes.len();
            calls += function
                .relocs
                .iter()
                .filter(|it| matches!(it.kind, Kind::X86CallPCRel4 | Kind::X86CallPLTRel4 | Kind::Arm64Call))
                .count();
        }
        let veneers = end.next_multiple_of(16);
        let code_len = veneers + calls * VENEER;

        let mut failed: Option<Stop> = None;
        let mut code_at = 0u64;
        let write = |code: &mut [u8], _: &mut [u8]| {
            let base = code.as_ptr() as u64;
            code_at = base;
            let mut writer = Writer { code, base, next_veneer: veneers };
            for (function, &start) in functions.iter().zip(&starts) {
                writer.code[start..start + function.bytes.len()].copy_from_slice(&function.bytes);
            }
            for ((function, &start), n) in functions.iter().zip(&starts).zip(0u32..) {
                for reloc in &function.relocs {
                    let target = match reloc.to {
                        To::Code(callee) => match starts.get(callee as usize) {
                            Some(&at) => base + at as u64,
                            None => {
                                // alloc: error path
                                failed = Some(Stop(format!("has `{}` calling function {callee} of {}", function.name, starts.len())));
                                return;
                            }
                        },
                        To::Host(address) => address,
                        To::Table => table.address(),
                        To::TableSize => (entries as u64 + 1) * super::table::ENTRY,
                        other => match resolve_data(other) {
                            Ok(it) => it,
                            Err(stop) => {
                                failed = Some(stop);
                                return;
                            }
                        },
                    };
                    let place = start + reloc.at as usize;
                    if reloc.at as usize + len_of(reloc.kind) > function.bytes.len() {
                        // alloc: error path
                        failed = Some(Stop(format!("has a {:?} in `{}` past its {} bytes", reloc.kind, function.name, function.bytes.len())));
                        return;
                    }
                    if let Err(what) = writer.relocate(place, reloc.kind, target, reloc.addend) {
                        // alloc: error path
                        failed = Some(Stop(format!("has `{}` (function {n}) with {what}", function.name)));
                        return;
                    }
                }
            }
        };
        let exec = Executable::written(code_len.max(1), 0, write)
            // alloc: error path
            .map_err(|it| Stop(format!("could not be given executable memory: {}", it.what)))?;
        if let Some(stop) = failed {
            return Err(stop);
        }
        for (function, &start) in functions.iter().zip(&starts) {
            if let Some(sig) = function.entry {
                let pushed = table.push(code_at + start as u64, sig);
                debug_assert!(pushed.is_some(), "the table was made for every entry");
            }
        }
        let mut placed = Vec::with_capacity(functions.len()); // alloc: once per image
        let mut names = Vec::with_capacity(functions.len()); // alloc: once per image
        for (function, &start) in functions.into_iter().zip(&starts) {
            placed.push(Placed {
                start: code_at + start as u64,
                len: function.bytes.len() as u32,
                frame: function.frame,
                calls: function.calls,
                traps: function.traps,
            });
            names.push(function.name);
        }
        Ok(Image { exec, code_len, table, placed, names, layout, writable, readonly, pointers })
    }

    pub(crate) fn table(&self) -> &Table {
        &self.table
    }

    /// Function `n`'s code address.
    pub(crate) fn code(&self, n: u32) -> Option<u64> {
        self.placed.get(n as usize).map(|it| it.start)
    }

    /// Function `n`'s table pointer, if it has an entry.
    pub(crate) fn pointer(&self, n: u32) -> Option<u64> {
        self.pointers.get(n as usize).copied().flatten()
    }

    /// Whether `pc` is in the image's code.
    pub(crate) fn contains(&self, pc: u64) -> bool {
        let start = self.exec.code_address();
        (start..start + self.code_len as u64).contains(&pc)
    }

    /// The function `pc` is in, with its name: a binary search of the placed functions.
    pub(crate) fn function_at(&self, pc: u64) -> Option<(&Placed, &str)> {
        let after = self.placed.partition_point(|it| it.start <= pc);
        let at = after.checked_sub(1)?;
        let placed = &self.placed[at];
        (pc < placed.start + u64::from(placed.len)).then(|| (placed, self.names[at].as_str()))
    }
}

fn pointer_of(pointers: &[Option<u64>], functions: &[Function], n: u32) -> Result<u64, Stop> {
    match pointers.get(n as usize) {
        Some(Some(pointer)) => Ok(*pointer),
        // alloc: error path
        Some(None) => Err(Stop(format!("takes the address of `{}`, which has no table entry", functions[n as usize].name))),
        // alloc: error path
        None => Err(Stop(format!("takes the address of function {n} of {}", functions.len()))),
    }
}

/// The bytes a relocation of `kind` writes.
fn len_of(kind: Kind) -> usize {
    match kind {
        Kind::Abs8 => 8,
        _ => 4,
    }
}

/// Writes the relocations into the code while it is writable, at the address it runs at.
struct Writer<'a> {
    code: &'a mut [u8],
    base: u64,
    next_veneer: usize,
}

impl Writer<'_> {
    /// One relocation at `place` against `target` plus `addend`, as a static linker writes it
    /// (S + A - P), through a veneer for a call out of reach.
    fn relocate(&mut self, place: usize, kind: Kind, target: u64, addend: i64) -> Result<(), String> {
        let what = (target as i64).wrapping_add(addend);
        let pc = self.base as i64 + place as i64;
        match kind {
            Kind::Abs8 => self.put(place, &(what as u64).to_le_bytes()),
            Kind::Abs4 => {
                // alloc: error path
                let value = u32::try_from(what).map_err(|_| format!("an Abs4 of {what:#x}"))?;
                self.put(place, &value.to_le_bytes());
            }
            Kind::X86PCRel4 => {
                let delta = what.wrapping_sub(pc);
                // alloc: error path
                let disp = i32::try_from(delta).map_err(|_| format!("a PCRel4 of {delta:#x}"))?;
                self.put(place, &disp.to_le_bytes());
            }
            Kind::X86CallPCRel4 | Kind::X86CallPLTRel4 => {
                let disp = match i32::try_from(what.wrapping_sub(pc)) {
                    Ok(disp) => disp,
                    Err(_) => {
                        let veneer = self.veneer(target)? as i64;
                        let delta = (veneer + addend).wrapping_sub(pc);
                        // alloc: error path
                        i32::try_from(delta).map_err(|_| format!("a call whose veneer is {delta:#x} away"))?
                    }
                };
                self.put(place, &disp.to_le_bytes());
            }
            Kind::Arm64Call => {
                let mut delta = what.wrapping_sub(pc);
                if !(-(1 << 27)..1 << 27).contains(&delta) {
                    delta = (self.veneer(what as u64)? as i64).wrapping_sub(pc);
                }
                if delta & 3 != 0 || !(-(1 << 27)..1 << 27).contains(&delta) {
                    // alloc: error path
                    return Err(format!("a call of {delta:#x}"));
                }
                self.patch(place, 0x03ff_ffff, ((delta >> 2) as u32) & 0x03ff_ffff);
            }
            Kind::Aarch64AdrPrelPgHi21 => {
                let delta = (what & !0xfff) - (pc & !0xfff);
                if !(-(1i64 << 32)..1i64 << 32).contains(&delta) {
                    // alloc: error path
                    return Err(format!("an adrp of {delta:#x}"));
                }
                let pages = (delta >> 12) as u32;
                self.patch(place, (3 << 29) | (0x7_ffff << 5), ((pages & 3) << 29) | (((pages >> 2) & 0x7_ffff) << 5));
            }
            Kind::Aarch64AddAbsLo12Nc => self.patch(place, 0xfff << 10, ((what as u32) & 0xfff) << 10),
            // alloc: error path
            other => return Err(format!("a {other:?} relocation, which static, confined code never has")),
        }
        Ok(())
    }

    /// A veneer jumping to `target`, in the area after the functions.
    fn veneer(&mut self, target: u64) -> Result<u64, String> {
        let at = self.next_veneer;
        if at + VENEER > self.code.len() {
            return Err("more veneers than calls".to_owned()); // alloc: error path
        }
        self.next_veneer += VENEER;
        let mut bytes = [0u8; VENEER];
        if cfg!(target_arch = "x86_64") {
            // jmp qword [rip + 0], the address after it.
            bytes[..6].copy_from_slice(&[0xff, 0x25, 0, 0, 0, 0]);
            bytes[6..14].copy_from_slice(&target.to_le_bytes());
        } else {
            // ldr x16, #8; br x16; the address.
            bytes[..4].copy_from_slice(&0x5800_0050u32.to_le_bytes());
            bytes[4..8].copy_from_slice(&0xd61f_0200u32.to_le_bytes());
            bytes[8..].copy_from_slice(&target.to_le_bytes());
        }
        self.put(at, &bytes);
        Ok(self.base + at as u64)
    }

    fn put(&mut self, place: usize, bytes: &[u8]) {
        self.code[place..place + bytes.len()].copy_from_slice(bytes);
    }

    fn patch(&mut self, place: usize, mask: u32, bits: u32) {
        let inst = u32::from_le_bytes(self.code[place..place + 4].try_into().expect("four bytes"));
        self.put(place, &((inst & !mask) | bits).to_le_bytes());
    }
}

#[cfg(test)]
mod tests;
