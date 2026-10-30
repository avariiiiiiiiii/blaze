//! Mach-O relocatable objects (`MH_OBJECT`), read as ld64 reads them: a section split into
//! one [`Section`] per atom, every relocation made explicit as a [`NativeFixup`].
//!
//! The load commands, the nlist, string and relocation tables are read in place. Atoms are one
//! sorted column of start offsets for the whole file (each Mach-O section a range of it), found
//! by binary search; fixups go to one column, sorted once, each atom a run of it. An atom's
//! bytes are a range of the input ([`Data::Input`]) and every name is in the object's one
//! [`Names`] arena: nothing is allocated per atom, per section or per symbol.

#![forbid(unsafe_code)]

use std::ops::Range;

use object::LittleEndian as LE;
use object::macho::{self, RelocationInfo};
use object::read::macho::{MachHeader as _, Nlist as _, Section as _, Segment as _};

use blaze_codegen::Arch;

use crate::model::{
    Apply, Binding, Data, Name, Names, NativeFixup, Object, Section, SectionKind, Symbol, SymbolDef,
    SymbolKind,
};

type Header = macho::MachHeader64<LE>;
type Nlist = macho::Nlist64<LE>;
type Relocation = macho::Relocation<LE>;

/// No symbol: an `N_STAB` entry's in [`Reader::map`], an atom without its section symbol yet in
/// [`Reader::section_symbols`].
const NONE: u32 = u32::MAX;

/// A thin (not universal) Mach-O relocatable object (`MH_OBJECT`) for `arch`.
pub(super) fn read(name: &str, bytes: &[u8], arch: Arch) -> Result<Object, String> {
    // `object` reads every Mach-O struct in place at any alignment (feature `unaligned`).
    let data = bytes;
    let magic = match data {
        [a, b, c, d, ..] => u32::from_be_bytes([*a, *b, *c, *d]),
        _ => return Err(format!("{name}: too short for a Mach-O header")),
    };
    match magic {
        macho::MH_CIGAM_64 => {}
        macho::MH_MAGIC_64 => return Err(format!("{name}: a big-endian Mach-O object")),
        macho::MH_MAGIC | macho::MH_CIGAM => return Err(format!("{name}: a 32-bit Mach-O object")),
        macho::FAT_MAGIC | macho::FAT_CIGAM | macho::FAT_MAGIC_64 | macho::FAT_CIGAM_64 => {
            return Err(format!("{name}: a universal (fat) file, not a thin Mach-O object"));
        }
        other => return Err(format!("{name}: not a Mach-O object (magic {other:#010x})")),
    }
    let header = Header::parse(data, 0).map_err(|e| format!("{name}: {e}"))?;
    let cpu = match arch {
        Arch::Aarch64 => macho::CPU_TYPE_ARM64,
        Arch::X86_64 => macho::CPU_TYPE_X86_64,
        Arch::Wasm32 => return Err(format!("{name}: a Mach-O object in a wasm32 link")),
    };
    let found = header.cputype(LE);
    if found != cpu {
        return Err(format!("{name}: a Mach-O object for CPU type {found:#x}, not {arch:?} ({cpu:#x})"));
    }
    let filetype = header.filetype(LE);
    if filetype != macho::MH_OBJECT {
        return Err(format!("{name}: Mach-O file type {filetype}, not a relocatable object (MH_OBJECT)"));
    }
    let subsections = header.flags(LE) & macho::MH_SUBSECTIONS_VIA_SYMBOLS != 0;

    let mut commands = header.load_commands(LE, data, 0).map_err(|e| format!("{name}: {e}"))?;
    let mut headers = Vec::new();
    let mut symtab = None;
    let mut libraries = Vec::new();
    while let Some(command) = commands.next().map_err(|e| format!("{name}: {e}"))? {
        match command.cmd() {
            macho::LC_SEGMENT_64 => {
                if let Some((segment, sections)) =
                    command.segment_64().map_err(|e| format!("{name}: {e}"))?
                {
                    headers.extend(
                        segment.sections(LE, sections).map_err(|e| format!("{name}: {e}"))?,
                    );
                }
            }
            macho::LC_SEGMENT => return Err(format!("{name}: a 32-bit segment (LC_SEGMENT)")),
            macho::LC_SYMTAB => symtab = command.symtab().map_err(|e| format!("{name}: {e}"))?,
            macho::LC_LINKER_OPTION => libraries.push(linker_option(name, command.raw_data())?),
            _ => {}
        }
    }

    let mut nlists: &[Nlist] = &[];
    let mut raw: Vec<&[u8]> = Vec::new();
    // The arena's bytes: every symbol's core name (at most one byte longer than its Mach-O
    // name) and every section's `SEG,SECT` (at most 33 bytes), so it is allocated once.
    let mut arena = 33 * headers.len();
    if let Some(symtab) = symtab {
        let table = symtab.symbols::<Header, _>(LE, data).map_err(|e| format!("{name}: {e}"))?;
        let strings = table.strings();
        nlists = table.iter().as_slice();
        raw.reserve_exact(nlists.len());
        for nlist in nlists {
            let it = nlist.name(LE, strings).map_err(|e| format!("{name}: {e}"))?;
            if nlist.n_type() & macho::N_STAB == 0 {
                arena += it.len() + 1;
            }
            raw.push(it);
        }
    }
    let mut names = Names::with_capacity(arena);
    let pushed = |e: String| format!("{name}: {e}");
    // Every atom's section symbol is named so.
    let empty = names.push("").map_err(pushed)?;

    let mut inputs = Vec::with_capacity(headers.len());
    let mut full = String::with_capacity(33);
    for header in headers {
        let segment = text(name, header.segment_name())?;
        let section = text(name, header.name())?;
        full.clear();
        full.push_str(segment);
        full.push(',');
        full.push_str(section);
        let flags = header.flags(LE);
        let what =
            super::classify(segment, section, flags).map_err(|e| format!("{name}: {full}: {e}"))?;
        let size = header.size(LE);
        let kind = flags & macho::SECTION_TYPE;
        let bytes = match kind {
            macho::S_ZEROFILL | macho::S_GB_ZEROFILL | macho::S_THREAD_LOCAL_ZEROFILL => None,
            _ => Some(
                header
                    .data(LE, data)
                    .map_err(|()| format!("{name}: {full}: its bytes are outside the file"))?,
            ),
        };
        let align = header.align(LE);
        if align >= 64 {
            return Err(format!("{name}: {full}: alignment 2^{align}"));
        }
        let relocations = header.relocations(LE, data).map_err(|e| format!("{name}: {full}: {e}"))?;
        let splits = subsections
            && matches!(what, SectionKind::Alloc(..))
            && !matches!(
                kind,
                macho::S_CSTRING_LITERALS
                    | macho::S_4BYTE_LITERALS
                    | macho::S_8BYTE_LITERALS
                    | macho::S_16BYTE_LITERALS
            );
        inputs.push(Input {
            full: names.push(&full).map_err(pushed)?,
            addr: header.addr(LE),
            size,
            flags,
            align: 1 << align,
            offset: header.file_range(LE).map_or(0, |(offset, _)| offset),
            bytes,
            relocations,
            relocated: what != SectionKind::Dropped,
            eh_frame: what == SectionKind::EhFrame,
            what,
            splits,
            bounds: 0..0,
            first: 0,
            atoms: 0,
        });
    }

    // Every symbol that bounds an atom or a symbol's size, by section: one sort of the file's
    // `(section, offset)` pairs, then each section's run of them a range of `bounds`.
    let mut points: Vec<(u32, u64)> = Vec::with_capacity(nlists.len());
    for (nlist, &raw) in nlists.iter().zip(&raw) {
        if nlist.n_type() & macho::N_STAB != 0 || nlist.n_type() & macho::N_TYPE != macho::N_SECT {
            continue;
        }
        let (s, offset) = position(name, &inputs, &names, nlist, raw)?;
        if !local_label(raw) && nlist.n_desc(LE) & macho::N_ALT_ENTRY == 0 {
            points.push((s as u32, offset));
        }
    }
    points.sort_unstable();
    points.dedup();
    let mut bounds: Vec<u64> = Vec::with_capacity(points.len());
    let mut starts: Vec<u64> = Vec::with_capacity(inputs.len() + points.len());
    let mut next = 0;
    for (s, input) in inputs.iter_mut().enumerate() {
        let from = bounds.len();
        while next < points.len() && points[next].0 as usize == s {
            bounds.push(points[next].1);
            next += 1;
        }
        input.bounds = from as u32..bounds.len() as u32;
        input.first = u32::try_from(starts.len()).map_err(|_| format!("{name}: too many atoms"))?;
        starts.push(0);
        if input.splits {
            starts.extend(bounds[from..].iter().copied().filter(|&it| it != 0));
        }
        input.atoms = (starts.len() - input.first as usize) as u32;
    }
    drop(points);

    let mut sections = Vec::with_capacity(starts.len());
    let mut relocations = 0;
    for input in &mut inputs {
        let first = input.first as usize;
        let atoms = &starts[first..first + input.atoms as usize];
        let retain = input.flags & macho::S_ATTR_NO_DEAD_STRIP != 0;
        for (j, &start) in atoms.iter().enumerate() {
            let end = atoms.get(j + 1).copied().unwrap_or(input.size);
            // `start <= end <= size`, and `offset..offset + size` is in the input (`bytes`).
            let data = match input.bytes {
                None => Data::Zero(end - start),
                Some(_) => Data::Input(input.offset + start..input.offset + end),
            };
            let align =
                if start == 0 { input.align } else { input.align.min(1 << start.trailing_zeros()) };
            // The last atom takes the kind: a `Class::Named` holds a `String`, cloned only for
            // the other atoms of a split named section.
            let what = if j + 1 == atoms.len() {
                std::mem::replace(&mut input.what, SectionKind::Dropped)
            } else {
                input.what.clone()
            };
            sections.push(Section { name: input.full, what, align, data, fixups: 0..0, retain });
        }
        if input.relocated {
            relocations += input.relocations.len();
        }
    }

    let mut reader = Reader {
        name,
        arch,
        section_symbols: vec![NONE; sections.len()],
        inputs,
        nlists,
        raw,
        names,
        empty,
        map: Vec::new(),
        bounds,
        starts,
        sections,
        symbols: Vec::with_capacity(nlists.len()),
        fixups: Vec::with_capacity(relocations),
    };
    reader.read_symbols()?;
    for s in 0..reader.inputs.len() {
        if reader.inputs[s].relocated {
            reader.relocate(s)?;
        }
    }
    for s in 0..reader.inputs.len() {
        if reader.inputs[s].eh_frame {
            reader.eh_frame(s)?;
        }
    }
    let Reader { mut sections, symbols, mut fixups, names, .. } = reader;
    if u32::try_from(fixups.len()).is_err() {
        return Err(format!("{name}: more than 2^32 relocations"));
    }
    // Stable: fixups of one place keep the order they were read in. Each atom's are then one
    // run of the object's column.
    fixups.sort_by_key(|it| (it.0, it.1.at));
    let mut at = 0;
    for group in fixups.chunk_by(|a, b| a.0 == b.0) {
        let end = at + group.len() as u32;
        sections[group[0].0 as usize].fixups = at..end;
        at = end;
    }
    let fixups = fixups.into_iter().map(|it| it.1).collect();
    Ok(Object {
        sections,
        symbols,
        fixups,
        groups: Vec::new(),
        grouped: Vec::new(),
        names,
        imports: Vec::new(),
        libraries,
        directives: Vec::new(),
    })
}

fn text<'a>(name: &str, bytes: &'a [u8]) -> Result<&'a str, String> {
    std::str::from_utf8(bytes)
        .map_err(|_| format!("{name}: a section name that isn't UTF-8: {bytes:?}"))
}

/// An `LC_LINKER_OPTION`: its words joined by a space (`-framework CoreFoundation`).
fn linker_option(name: &str, command: &[u8]) -> Result<String, String> {
    let Some(&[a, b, c, d]) = command.get(8..12) else {
        return Err(format!("{name}: a truncated LC_LINKER_OPTION"));
    };
    let count = u32::from_le_bytes([a, b, c, d]);
    let mut rest = &command[12..];
    let mut option = String::with_capacity(rest.len());
    for i in 0..count {
        let end = rest
            .iter()
            .position(|&it| it == 0)
            .ok_or_else(|| format!("{name}: an LC_LINKER_OPTION with fewer than {count} strings"))?;
        let word = std::str::from_utf8(&rest[..end])
            .map_err(|_| format!("{name}: an LC_LINKER_OPTION that isn't UTF-8"))?;
        if i != 0 {
            option.push(' ');
        }
        option.push_str(word);
        rest = &rest[end + 1..];
    }
    Ok(option)
}

/// ld64 starts no atom at an assembler-local label (`L…`, `l…`, `ltmp0`).
fn local_label(name: &[u8]) -> bool {
    matches!(name.first(), Some(b'L' | b'l'))
}

/// The section (index into the inputs) an `N_SECT` symbol is in, and its offset there.
fn position(
    name: &str,
    inputs: &[Input<'_>],
    names: &Names,
    nlist: &Nlist,
    symbol: &[u8],
) -> Result<(usize, u64), String> {
    let shown = || String::from_utf8_lossy(symbol).into_owned();
    let n_sect = usize::from(nlist.n_sect());
    let input = n_sect
        .checked_sub(1)
        .and_then(|it| inputs.get(it))
        .ok_or_else(|| format!("{name}: symbol {} is in section {n_sect}, which the object doesn't have", shown()))?;
    let value = nlist.n_value(LE);
    match value.checked_sub(input.addr) {
        Some(offset) if offset <= input.size => Ok((n_sect - 1, offset)),
        _ => Err(format!("{name}: symbol {} at {value:#x} is outside {}", shown(), names.at(input.full))),
    }
}

/// A Mach-O section as the file has it.
struct Input<'a> {
    /// `SEG,SECT`, in the object's names.
    full: Name,
    addr: u64,
    size: u64,
    flags: u32,
    align: u64,
    /// Where `bytes` start in the input.
    offset: u64,
    /// `None` when zero-filled.
    bytes: Option<&'a [u8]>,
    relocations: &'a [Relocation],
    /// Moved into its last atom once the atoms are made.
    what: SectionKind,
    /// Not [`SectionKind::Dropped`]: its relocations are read.
    relocated: bool,
    eh_frame: bool,
    splits: bool,
    /// Its symbols that aren't local labels or alternate entries: [`Reader::bounds`] of this
    /// range, sorted offsets from its start, where a symbol's size ends.
    bounds: Range<u32>,
    /// Its atoms: [`Object::sections`] `first..first + atoms`, which start at
    /// [`Reader::starts`] of the same indexes (the first at 0).
    first: u32,
    atoms: u32,
}

/// A relocation read, before it is put in its atom.
struct Fix {
    /// From the start of the Mach-O section.
    at: u64,
    width: u64,
    apply: Apply,
    symbol: u32,
    addend: i64,
    minus: Option<u32>,
}

struct Reader<'a> {
    name: &'a str,
    arch: Arch,
    inputs: Vec<Input<'a>>,
    nlists: &'a [Nlist],
    /// Each nlist entry's name, from the string table.
    raw: Vec<&'a [u8]>,
    /// The object's names: its sections' and symbols'.
    names: Names,
    /// `""`, every section symbol's name.
    empty: Name,
    /// From an nlist index to the index in `symbols`; [`NONE`] for `N_STAB` entries.
    map: Vec<u32>,
    /// Every section's [`Input::bounds`], one after the other: 8 bytes a symbol.
    bounds: Vec<u64>,
    /// Each atom's start, from its Mach-O section's start, by index in `sections`: 8 bytes an
    /// atom, an atom found by a binary search of its section's run.
    starts: Vec<u64>,
    sections: Vec<Section>,
    symbols: Vec<Symbol>,
    /// The `SymbolKind::Section` symbol of each atom once a relocation needs it, else [`NONE`].
    section_symbols: Vec<u32>,
    /// Every fixup read and the atom (index in `sections`) it is in, in reading order.
    fixups: Vec<(u32, NativeFixup)>,
}

impl Reader<'_> {
    /// Section `s`'s `SEG,SECT`, for an error.
    fn full(&self, s: usize) -> &str {
        self.names.at(self.inputs[s].full)
    }

    /// The atom of section `s` holding `offset`, the last one starting at or before it: its
    /// index in `sections`, its start and its end.
    fn atom(&self, s: usize, offset: u64) -> (u32, u64, u64) {
        let input = &self.inputs[s];
        let starts = &self.starts[input.first as usize..][..input.atoms as usize];
        let j = starts.partition_point(|&it| it <= offset) - 1;
        (input.first + j as u32, starts[j], starts.get(j + 1).copied().unwrap_or(input.size))
    }

    fn read_symbols(&mut self) -> Result<(), String> {
        let name = self.name;
        let nlists = self.nlists;
        self.map = vec![NONE; nlists.len()];
        for (index, nlist) in nlists.iter().enumerate() {
            let n_type = nlist.n_type();
            if n_type & macho::N_STAB != 0 {
                continue;
            }
            let raw = self.raw[index];
            let shown = || String::from_utf8_lossy(raw).into_owned();
            let n_desc = nlist.n_desc(LE);
            let value = nlist.n_value(LE);
            let external = n_type & macho::N_EXT != 0;
            let (def, kind) = match n_type & macho::N_TYPE {
                macho::N_SECT => {
                    let (s, offset) = position(name, &self.inputs, &self.names, nlist, raw)?;
                    let (section, start, end) = self.atom(s, offset);
                    let input = &self.inputs[s];
                    let bounds = &self.bounds[input.bounds.start as usize..input.bounds.end as usize];
                    let next =
                        bounds.get(bounds.partition_point(|&it| it <= offset)).copied().unwrap_or(end);
                    let def = SymbolDef::Section {
                        section,
                        value: offset - start,
                        size: next.min(end) - offset,
                    };
                    let kind = match input.flags & macho::SECTION_TYPE {
                        macho::S_THREAD_LOCAL_REGULAR
                        | macho::S_THREAD_LOCAL_ZEROFILL
                        | macho::S_THREAD_LOCAL_INIT_FUNCTION_POINTERS => SymbolKind::Tls,
                        _ if input.flags
                            & (macho::S_ATTR_PURE_INSTRUCTIONS | macho::S_ATTR_SOME_INSTRUCTIONS)
                            != 0 =>
                        {
                            SymbolKind::Function
                        }
                        _ => SymbolKind::Data,
                    };
                    (def, kind)
                }
                macho::N_ABS => (SymbolDef::Absolute(value), SymbolKind::Other),
                macho::N_UNDF if value != 0 && external => (
                    SymbolDef::Common { size: value, align: 1 << ((n_desc >> 8) & 0x0f) },
                    SymbolKind::Data,
                ),
                macho::N_UNDF => (SymbolDef::Undefined, SymbolKind::Other),
                macho::N_INDR => {
                    return Err(format!("{name}: symbol {} is an indirect symbol (N_INDR)", shown()));
                }
                other => {
                    return Err(format!("{name}: symbol {} has n_type {other:#x}", shown()));
                }
            };
            let weak = match def {
                SymbolDef::Undefined => n_desc & macho::N_WEAK_REF != 0,
                SymbolDef::Section { .. } | SymbolDef::Absolute(_) => n_desc & macho::N_WEAK_DEF != 0,
                SymbolDef::Common { .. } | SymbolDef::Fallback { .. } => false,
            };
            let binding = if !external {
                Binding::Local
            } else if weak {
                Binding::Weak
            } else {
                Binding::Global
            };
            self.map[index] = self.symbols.len() as u32;
            self.symbols.push(Symbol {
                name: super::push_core_name(&mut self.names, raw).map_err(|e| format!("{name}: {e}"))?,
                def,
                binding,
                kind,
                hidden: n_type & macho::N_PEXT != 0,
            });
        }
        Ok(())
    }

    /// Reads section `s`'s relocation table in place: each entry is decoded where a reader
    /// takes it, never copied.
    fn relocate(&mut self, s: usize) -> Result<(), String> {
        let relocations = self.inputs[s].relocations;
        if relocations.iter().any(|it| it.r_word0.get(LE) & macho::R_SCATTERED != 0) {
            return Err(format!(
                "{}: a scattered relocation in {}, which a 64-bit object can't have",
                self.name,
                self.full(s)
            ));
        }
        let mut i = 0;
        match self.arch {
            Arch::Aarch64 => {
                while i < relocations.len() {
                    i += self.aarch64(s, &relocations[i..])?;
                }
            }
            Arch::X86_64 => {
                while i < relocations.len() {
                    i += self.x86_64(s, &relocations[i..])?;
                }
            }
            Arch::Wasm32 if relocations.is_empty() => {}
            Arch::Wasm32 => return Err(format!("{}: a Mach-O object in a wasm32 link", self.name)),
        }
        Ok(())
    }

    /// Where a relocation is, for an error.
    fn place_of(&self, s: usize, at: u64) -> String {
        format!("{}: {} at {at:#x}", self.name, self.full(s))
    }

    /// Reads the relocations starting at `relocations[0]`; returns how many it took.
    fn aarch64(&mut self, s: usize, relocations: &[Relocation]) -> Result<usize, String> {
        let r = relocations[0].info(LE);
        match r.r_type {
            macho::ARM64_RELOC_ADDEND => {
                let addend = i64::from(((r.r_symbolnum << 8) as i32) >> 8);
                let Some(next) = relocations.get(1).map(|it| it.info(LE)) else {
                    return Err(format!(
                        "{}: ARM64_RELOC_ADDEND with no relocation after it",
                        self.place_of(s, r.r_address.into())
                    ));
                };
                if !matches!(
                    next.r_type,
                    macho::ARM64_RELOC_PAGE21 | macho::ARM64_RELOC_PAGEOFF12 | macho::ARM64_RELOC_BRANCH26
                ) {
                    return Err(format!(
                        "{}: ARM64_RELOC_ADDEND before {}, not PAGE21, PAGEOFF12 or BRANCH26",
                        self.place_of(s, next.r_address.into()),
                        arm64_name(next.r_type)
                    ));
                }
                self.aarch64_one(s, next, addend)?;
                Ok(2)
            }
            macho::ARM64_RELOC_SUBTRACTOR => {
                self.subtractor(s, relocations, macho::ARM64_RELOC_UNSIGNED, arm64_name)?;
                Ok(2)
            }
            _ => {
                self.aarch64_one(s, r, 0)?;
                Ok(1)
            }
        }
    }

    fn aarch64_one(&mut self, s: usize, r: RelocationInfo, addend: i64) -> Result<(), String> {
        let at = u64::from(r.r_address);
        // GOT and TLV cells hold the symbol itself: no addend moves it to another atom.
        let (apply, offset, anchor) = match (r.r_type, r.r_pcrel, r.r_length) {
            (macho::ARM64_RELOC_UNSIGNED, false, 2 | 3) => return self.unsigned(s, r),
            (macho::ARM64_RELOC_BRANCH26, true, 2) => {
                let call = self.read(s, at, 2, false)? & 0x8000_0000 != 0;
                (if call { Apply::A64Call26 } else { Apply::A64Jump26 }, addend, true)
            }
            (macho::ARM64_RELOC_PAGE21, true, 2) => (Apply::A64Page21, addend, true),
            (macho::ARM64_RELOC_PAGEOFF12, false, 2) => {
                (Apply::A64Lo12 { shift: self.lo12_shift(s, at)? }, addend, true)
            }
            (macho::ARM64_RELOC_GOT_LOAD_PAGE21, true, 2) => (Apply::A64GotPage21, 0, false),
            (macho::ARM64_RELOC_GOT_LOAD_PAGEOFF12, false, 2) => (Apply::A64GotLo12, 0, false),
            (macho::ARM64_RELOC_POINTER_TO_GOT, true, 2) => {
                (Apply::GotRel32, self.read(s, at, 2, true)?, false)
            }
            (macho::ARM64_RELOC_TLVP_LOAD_PAGE21, true, 2) => (Apply::TlvPage21, 0, false),
            (macho::ARM64_RELOC_TLVP_LOAD_PAGEOFF12, false, 2) => (Apply::TlvLo12, 0, false),
            (kind, pcrel, length) => {
                return Err(format!(
                    "{}: {} (type {kind}, pcrel {pcrel}, {} bytes) is a relocation blaze doesn't read",
                    self.place_of(s, at),
                    arm64_name(kind),
                    1u32 << length
                ));
            }
        };
        if !r.r_extern {
            return Err(format!(
                "{}: {} against a section rather than a symbol",
                self.place_of(s, at),
                arm64_name(r.r_type)
            ));
        }
        let (symbol, addend) = self.extern_target(r.r_symbolnum, offset, anchor)?;
        self.place(s, Fix { at, width: 4, apply, symbol, addend, minus: None })
    }

    /// The low-12-bit field's shift: what the instruction scales its immediate by.
    fn lo12_shift(&self, s: usize, at: u64) -> Result<u8, String> {
        let insn = self.read(s, at, 2, false)? as u32;
        if insn & 0x1f00_0000 == 0x1100_0000 {
            return Ok(0);
        }
        if insn & 0x3b00_0000 == 0x3900_0000 {
            let size = (insn >> 30) as u8;
            let simd = insn & 1 << 26 != 0;
            return Ok(if simd && size == 0 && insn & 1 << 23 != 0 { 4 } else { size });
        }
        Err(format!(
            "{}: ARM64_RELOC_PAGEOFF12 on instruction {insn:#010x}, neither an ADD (immediate) nor a load or store with an unsigned offset",
            self.place_of(s, at)
        ))
    }

    fn x86_64(&mut self, s: usize, relocations: &[Relocation]) -> Result<usize, String> {
        let r = relocations[0].info(LE);
        let at = u64::from(r.r_address);
        let (apply, tail, anchor) = match (r.r_type, r.r_pcrel, r.r_length) {
            (macho::X86_64_RELOC_UNSIGNED, false, 2 | 3) => {
                self.unsigned(s, r)?;
                return Ok(1);
            }
            (macho::X86_64_RELOC_SUBTRACTOR, ..) => {
                self.subtractor(s, relocations, macho::X86_64_RELOC_UNSIGNED, x86_64_name)?;
                return Ok(2);
            }
            (macho::X86_64_RELOC_SIGNED, true, 2) => (Apply::Rel32, 0, true),
            (macho::X86_64_RELOC_SIGNED_1, true, 2) => (Apply::Rel32, 1, true),
            (macho::X86_64_RELOC_SIGNED_2, true, 2) => (Apply::Rel32, 2, true),
            (macho::X86_64_RELOC_SIGNED_4, true, 2) => (Apply::Rel32, 4, true),
            (macho::X86_64_RELOC_BRANCH, true, 2) => (Apply::X86Branch32, 0, true),
            (macho::X86_64_RELOC_GOT_LOAD | macho::X86_64_RELOC_GOT, true, 2) => {
                (Apply::X86GotPcRel32, 0, false)
            }
            (macho::X86_64_RELOC_TLV, true, 2) => (Apply::TlvRel32, 0, false),
            (kind, pcrel, length) => {
                return Err(format!(
                    "{}: {} (type {kind}, pcrel {pcrel}, {} bytes) is a relocation blaze doesn't read",
                    self.place_of(s, at),
                    x86_64_name(kind),
                    1u32 << length
                ));
            }
        };
        let implicit = self.read(s, at, 2, true)?;
        // The field is relative to the end of the instruction: `tail` bytes after it.
        let bias = -4 - tail;
        let (symbol, addend) = if r.r_extern {
            let (symbol, offset) = self.extern_target(r.r_symbolnum, implicit, anchor)?;
            (symbol, offset + bias)
        } else {
            if !anchor {
                return Err(format!(
                    "{}: {} against a section rather than a symbol",
                    self.place_of(s, at),
                    x86_64_name(r.r_type)
                ));
            }
            let target = (self.inputs[s].addr + at)
                .wrapping_add((4 + tail) as u64)
                .wrapping_add_signed(implicit);
            let (symbol, extra) = self.at_address(r.r_symbolnum, target)?;
            (symbol, extra + bias)
        };
        self.place(s, Fix { at, width: 4, apply, symbol, addend, minus: None })?;
        Ok(1)
    }

    /// An `UNSIGNED` on either CPU: an absolute address, or a `__thread_vars` descriptor's
    /// offset field.
    fn unsigned(&mut self, s: usize, r: RelocationInfo) -> Result<(), String> {
        let at = u64::from(r.r_address);
        let input = &self.inputs[s];
        let apply = if input.flags & macho::SECTION_TYPE == macho::S_THREAD_LOCAL_VARIABLES
            && at % 24 == 16
        {
            if r.r_length != 3 {
                return Err(format!(
                    "{}: a thread-local descriptor's offset relocated in {} bytes, not 8",
                    self.place_of(s, at),
                    1u32 << r.r_length
                ));
            }
            Apply::DtpOff64
        } else if r.r_length == 3 {
            Apply::Abs64
        } else {
            Apply::Abs32
        };
        let implicit = self.read(s, at, r.r_length, false)?;
        let (symbol, addend) = if r.r_extern {
            self.extern_target(r.r_symbolnum, implicit, true)?
        } else {
            self.at_address(r.r_symbolnum, implicit as u64)?
        };
        self.place(s, Fix { at, width: 1 << r.r_length, apply, symbol, addend, minus: None })
    }

    /// `SUBTRACTOR` (the symbol subtracted) then `UNSIGNED` (the symbol added) of one place:
    /// one `Delta64`/`Delta32`.
    fn subtractor(
        &mut self,
        s: usize,
        relocations: &[Relocation],
        unsigned: u8,
        names: fn(u8) -> &'static str,
    ) -> Result<(), String> {
        let r = relocations[0].info(LE);
        let at = u64::from(r.r_address);
        if r.r_pcrel || !matches!(r.r_length, 2 | 3) {
            return Err(format!(
                "{}: a SUBTRACTOR of {} bytes{}",
                self.place_of(s, at),
                1u32 << r.r_length,
                if r.r_pcrel { ", pc-relative" } else { "" }
            ));
        }
        if !r.r_extern {
            return Err(format!("{}: a SUBTRACTOR of a section rather than a symbol", self.place_of(s, at)));
        }
        let Some(next) = relocations.get(1).map(|it| it.info(LE)) else {
            return Err(format!("{}: a SUBTRACTOR with no UNSIGNED after it", self.place_of(s, at)));
        };
        if next.r_type != unsigned
            || next.r_address != r.r_address
            || next.r_length != r.r_length
            || next.r_pcrel
        {
            return Err(format!(
                "{}: a SUBTRACTOR followed by {} at {:#x}, not an UNSIGNED of the same place and size",
                self.place_of(s, at),
                names(next.r_type),
                next.r_address
            ));
        }
        let implicit = self.read(s, at, r.r_length, true)?;
        // Both symbols are taken where they are: the bytes may hold more than the addend
        // (the offset of the place from the subtracted symbol), so they don't name an atom.
        let (minus, _) = self.extern_target(r.r_symbolnum, 0, false)?;
        let (symbol, addend) = if next.r_extern {
            self.extern_target(next.r_symbolnum, implicit, false)?
        } else {
            self.at_address(next.r_symbolnum, implicit as u64)?
        };
        let apply = if r.r_length == 3 { Apply::Delta64 } else { Apply::Delta32 };
        self.place(s, Fix { at, width: 1 << r.r_length, apply, symbol, addend, minus: Some(minus) })
    }

    /// The target of an extern relocation to nlist entry `index` plus `offset`. With `anchor`,
    /// a symbol local to the object (not `N_EXT`, or an `L` label) is replaced by the atom at
    /// the address the relocation names, as ld64 does: `ltmp0 + 0x40` is in a later atom.
    fn extern_target(&mut self, index: u32, offset: i64, anchor: bool) -> Result<(u32, i64), String> {
        let name = self.name;
        let nlists = self.nlists;
        let (Some(nlist), Some(symbol)) =
            (nlists.get(index as usize), self.map.get(index as usize).copied())
        else {
            return Err(format!("{name}: a relocation names symbol {index}, which the object doesn't have"));
        };
        if symbol == NONE {
            return Err(format!("{name}: a relocation names debugging symbol {index} (N_STAB)"));
        }
        let n_type = nlist.n_type();
        let local = n_type & macho::N_TYPE == macho::N_SECT
            && (n_type & macho::N_EXT == 0 || self.raw[index as usize].first() == Some(&b'L'));
        if !anchor || !local {
            return Ok((symbol, offset));
        }
        let addr = nlist.n_value(LE).wrapping_add_signed(offset);
        self.at_address(u32::from(nlist.n_sect()), addr)
    }

    /// The section symbol of the atom of section `ordinal` (1-based over the file's sections)
    /// at `addr`, and `addr`'s offset from it.
    fn at_address(&mut self, ordinal: u32, addr: u64) -> Result<(u32, i64), String> {
        let name = self.name;
        let s = (ordinal as usize)
            .checked_sub(1)
            .filter(|&it| it < self.inputs.len())
            .ok_or_else(|| format!("{name}: a relocation names section {ordinal}, which the object doesn't have"))?;
        let input = &self.inputs[s];
        let offset = match addr.checked_sub(input.addr) {
            Some(offset) if offset <= input.size => offset,
            _ => return Err(format!("{name}: a relocation to {addr:#x}, outside {}", self.full(s))),
        };
        let (section, start, _) = self.atom(s, offset);
        Ok((self.section_symbol(section), (offset - start) as i64))
    }

    fn section_symbol(&mut self, section: u32) -> u32 {
        let found = self.section_symbols[section as usize];
        if found != NONE {
            return found;
        }
        let symbol = self.symbols.len() as u32;
        self.symbols.push(Symbol {
            name: self.empty,
            def: SymbolDef::Section {
                section,
                value: 0,
                size: self.sections[section as usize].data.len(),
            },
            binding: Binding::Local,
            kind: SymbolKind::Section,
            hidden: false,
        });
        self.section_symbols[section as usize] = symbol;
        symbol
    }

    /// The little-endian value of `1 << length` bytes at `at` of section `s`.
    fn read(&self, s: usize, at: u64, length: u8, signed: bool) -> Result<i64, String> {
        let width = 1usize << length;
        let Some(bytes) = self.inputs[s].bytes else {
            return Err(format!("{}: a relocation in a zero-filled section", self.place_of(s, at)));
        };
        let field = usize::try_from(at)
            .ok()
            .and_then(|at| bytes.get(at..at.checked_add(width)?))
            .ok_or_else(|| format!("{}: a relocation past the end of the section", self.place_of(s, at)))?;
        let mut word = [0u8; 8];
        word[..width].copy_from_slice(field);
        let value = u64::from_le_bytes(word);
        if signed && width < 8 {
            let shift = 64 - 8 * width as u32;
            Ok(((value << shift) as i64) >> shift)
        } else {
            Ok(value as i64)
        }
    }

    /// Puts `fix` in the atom of section `s` it is in.
    fn place(&mut self, s: usize, fix: Fix) -> Result<(), String> {
        let (section, start, end) = self.atom(s, fix.at);
        if fix.at.checked_add(fix.width).is_none_or(|it| it > end) {
            return Err(format!(
                "{}: a relocation of {} bytes that reaches past its atom ({start:#x}..{end:#x})",
                self.place_of(s, fix.at),
                fix.width
            ));
        }
        self.fixups.push((section, NativeFixup {
            at: fix.at - start,
            apply: fix.apply,
            symbol: fix.symbol,
            addend: fix.addend,
            minus: fix.minus,
        }));
        Ok(())
    }

    /// An error in section `s`'s call frame information at `at`.
    fn frame_error(&self, s: usize, at: usize, what: &str) -> String {
        format!("{}: {} at {at:#x}: {what}", self.name, self.full(s))
    }

    /// The pointers of `__eh_frame` that no relocation covers: clang leaves an FDE's
    /// pc-relative `pc_begin` and LSDA as the distance in the object (ld64 reads them so),
    /// which only holds while both ends stay where they were. Each becomes an explicit
    /// `Rel32`/`Rel64` to the atom it points at.
    fn eh_frame(&mut self, s: usize) -> Result<(), String> {
        let Some(bytes) = self.inputs[s].bytes else {
            return Ok(());
        };
        // `__eh_frame` is never split: its one atom's fixups are the section's.
        let first = self.inputs[s].first;
        let mut covered: Vec<u64> =
            self.fixups.iter().filter(|it| it.0 == first).map(|it| it.1.at).collect();
        covered.sort_unstable();
        // In entry order, so by offset: an FDE finds its CIE by binary search.
        let mut cies: Vec<(usize, Cie)> = Vec::new();
        let mut fields: Vec<(usize, u8)> = Vec::new();
        let mut at = 0;
        while at < bytes.len() {
            let mut entry = Cursor { bytes, at };
            let length = entry.u32().map_err(|it| self.frame_error(s, at, it))?;
            if length == 0 {
                break;
            }
            if length == u32::MAX {
                return Err(self.frame_error(s, at, "a 64-bit call frame entry"));
            }
            let start = at + 4;
            let end = start
                .checked_add(length as usize)
                .filter(|&it| it <= bytes.len())
                .ok_or_else(|| self.frame_error(s, at, "a call frame entry longer than the section"))?;
            let mut c = Cursor { bytes: &bytes[..end], at: start };
            let id = c.u32().map_err(|it| self.frame_error(s, start, it))?;
            if id == 0 {
                let cie = Cie::parse(&mut c, &mut fields).map_err(|it| self.frame_error(s, start, &it))?;
                cies.push((at, cie));
            } else {
                // The CIE pointer is the distance back from itself to the CIE's length field.
                let cie = start
                    .checked_sub(id as usize)
                    .and_then(|it| cies.binary_search_by_key(&it, |(cie, _)| *cie).ok())
                    .map(|found| cies[found].1)
                    .ok_or_else(|| self.frame_error(s, start, "an FDE whose CIE isn't before it"))?;
                fields.push((c.at, cie.fde));
                c.pointer(cie.fde).map_err(|it| self.frame_error(s, c.at, &it))?;
                c.pointer(cie.fde & 0x0f).map_err(|it| self.frame_error(s, c.at, &it))?;
                if cie.z {
                    c.uleb().map_err(|it| self.frame_error(s, c.at, it))?;
                    if let Some(lsda) = cie.lsda {
                        fields.push((c.at, lsda));
                    }
                }
            }
            at = end;
        }
        let mut by_addr: Option<Vec<u32>> = None;
        for (at, encoding) in fields {
            if encoding == DW_EH_PE_OMIT || covered.binary_search(&(at as u64)).is_ok() {
                continue;
            }
            let length = match encoding & 0x0f {
                0x03 | 0x0b => 2,
                0x00 | 0x04 | 0x0c => 3,
                _ => {
                    let what = format!("pointer encoding {encoding:#x} without a relocation");
                    return Err(self.frame_error(s, at, &what));
                }
            };
            match encoding & 0x70 {
                0x10 => {
                    let value = self.read(s, at as u64, length, true)?;
                    let target = (self.inputs[s].addr + at as u64).wrapping_add_signed(value);
                    if by_addr.is_none() {
                        by_addr = Some(self.by_addr()?);
                    }
                    let sorted = by_addr.as_deref().unwrap_or(&[]);
                    let ordinal = self.ordinal_of(sorted, target).ok_or_else(|| {
                        let what = format!("a pc-relative pointer to {target:#x}, in no section");
                        self.frame_error(s, at, &what)
                    })?;
                    let (symbol, addend) = self.at_address(ordinal, target)?;
                    let apply = if length == 3 { Apply::Rel64 } else { Apply::Rel32 };
                    let fix = Fix { at: at as u64, width: 1 << length, apply, symbol, addend, minus: None };
                    self.place(s, fix)?;
                }
                0x00 => {
                    if self.read(s, at as u64, length, false)? != 0 {
                        return Err(self.frame_error(s, at, "an absolute pointer without a relocation"));
                    }
                }
                _ => return Err(self.frame_error(s, at, &format!("pointer encoding {encoding:#x}"))),
            }
        }
        Ok(())
    }

    /// The sections that hold bytes, by address: 4 bytes a section, an address found by binary
    /// search. Two of them sharing an address is an error, as the atom an address names would
    /// be ambiguous.
    fn by_addr(&self) -> Result<Vec<u32>, String> {
        let inputs = &self.inputs;
        let mut out: Vec<u32> =
            (0..inputs.len() as u32).filter(|&it| inputs[it as usize].size != 0).collect();
        out.sort_unstable_by_key(|&it| (inputs[it as usize].addr, it));
        for pair in out.windows(2) {
            let (a, b) = (&inputs[pair[0] as usize], &inputs[pair[1] as usize]);
            if a.addr.saturating_add(a.size) > b.addr {
                let (a, b) = (self.names.at(a.full), self.names.at(b.full));
                return Err(format!("{}: sections {a} and {b} overlap", self.name));
            }
        }
        Ok(out)
    }

    /// The 1-based ordinal of the section holding `addr`, of those [`Reader::by_addr`] gives.
    fn ordinal_of(&self, by_addr: &[u32], addr: u64) -> Option<u32> {
        let i = by_addr.partition_point(|&it| self.inputs[it as usize].addr <= addr).checked_sub(1)?;
        let s = by_addr[i] as usize;
        let input = &self.inputs[s];
        (addr - input.addr < input.size).then_some(s as u32 + 1)
    }
}

const DW_EH_PE_OMIT: u8 = 0xff;

/// What an FDE needs of its CIE.
#[derive(Clone, Copy)]
struct Cie {
    /// The encoding of `pc_begin` (`R`).
    fde: u8,
    /// The encoding of the LSDA pointer (`L`).
    lsda: Option<u8>,
    /// Whether the augmentation has data (`z`).
    z: bool,
}

impl Cie {
    /// Reads a CIE after its id, noting its personality pointer in `fields`.
    fn parse(c: &mut Cursor<'_>, fields: &mut Vec<(usize, u8)>) -> Result<Cie, String> {
        let version = c.u8()?;
        if version != 1 && version != 3 {
            return Err(format!("a CIE of version {version}"));
        }
        let augmentation = c.string()?;
        c.uleb()?;
        c.uleb()?;
        if version == 1 {
            c.u8()?;
        } else {
            c.uleb()?;
        }
        let mut cie = Cie { fde: 0, lsda: None, z: false };
        match augmentation {
            [] => {}
            [b'z', rest @ ..] => {
                cie.z = true;
                c.uleb()?;
                for &it in rest {
                    match it {
                        b'P' => {
                            let encoding = c.u8()?;
                            fields.push((c.at, encoding));
                            c.pointer(encoding)?;
                        }
                        b'L' => cie.lsda = Some(c.u8()?),
                        b'R' => cie.fde = c.u8()?,
                        b'S' | b'B' | b'G' => {}
                        other => {
                            return Err(format!(
                                "a CIE augmentation {:?} ({:?} unknown)",
                                String::from_utf8_lossy(augmentation),
                                other as char
                            ));
                        }
                    }
                }
            }
            _ => {
                return Err(format!(
                    "a CIE augmentation {:?}",
                    String::from_utf8_lossy(augmentation)
                ));
            }
        }
        Ok(cie)
    }
}

struct Cursor<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], &'static str> {
        let end = self.at.checked_add(n).filter(|&it| it <= self.bytes.len()).ok_or("a truncated call frame entry")?;
        let taken = &self.bytes[self.at..end];
        self.at = end;
        Ok(taken)
    }

    fn u8(&mut self) -> Result<u8, &'static str> {
        Ok(self.take(1)?[0])
    }

    fn u32(&mut self) -> Result<u32, &'static str> {
        let bytes = self.take(4)?;
        Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    /// An unsigned or signed LEB128, skipped.
    fn uleb(&mut self) -> Result<(), &'static str> {
        while self.u8()? & 0x80 != 0 {}
        Ok(())
    }

    fn string(&mut self) -> Result<&'a [u8], &'static str> {
        let rest = &self.bytes[self.at..];
        let end = rest.iter().position(|&it| it == 0).ok_or("an unterminated CIE augmentation")?;
        self.at += end + 1;
        Ok(&rest[..end])
    }

    /// Skips a pointer of `encoding`'s format.
    fn pointer(&mut self, encoding: u8) -> Result<(), String> {
        if encoding == DW_EH_PE_OMIT {
            return Ok(());
        }
        match encoding & 0x0f {
            0x00 | 0x04 | 0x0c => self.take(8).map(drop)?,
            0x02 | 0x0a => self.take(2).map(drop)?,
            0x03 | 0x0b => self.take(4).map(drop)?,
            0x01 | 0x09 => self.uleb()?,
            other => return Err(format!("pointer format {other:#x}")),
        }
        Ok(())
    }
}

fn arm64_name(kind: u8) -> &'static str {
    match kind {
        macho::ARM64_RELOC_UNSIGNED => "ARM64_RELOC_UNSIGNED",
        macho::ARM64_RELOC_SUBTRACTOR => "ARM64_RELOC_SUBTRACTOR",
        macho::ARM64_RELOC_BRANCH26 => "ARM64_RELOC_BRANCH26",
        macho::ARM64_RELOC_PAGE21 => "ARM64_RELOC_PAGE21",
        macho::ARM64_RELOC_PAGEOFF12 => "ARM64_RELOC_PAGEOFF12",
        macho::ARM64_RELOC_GOT_LOAD_PAGE21 => "ARM64_RELOC_GOT_LOAD_PAGE21",
        macho::ARM64_RELOC_GOT_LOAD_PAGEOFF12 => "ARM64_RELOC_GOT_LOAD_PAGEOFF12",
        macho::ARM64_RELOC_POINTER_TO_GOT => "ARM64_RELOC_POINTER_TO_GOT",
        macho::ARM64_RELOC_TLVP_LOAD_PAGE21 => "ARM64_RELOC_TLVP_LOAD_PAGE21",
        macho::ARM64_RELOC_TLVP_LOAD_PAGEOFF12 => "ARM64_RELOC_TLVP_LOAD_PAGEOFF12",
        macho::ARM64_RELOC_ADDEND => "ARM64_RELOC_ADDEND",
        macho::ARM64_RELOC_AUTHENTICATED_POINTER => "ARM64_RELOC_AUTHENTICATED_POINTER",
        _ => "an unknown ARM64 relocation",
    }
}

fn x86_64_name(kind: u8) -> &'static str {
    match kind {
        macho::X86_64_RELOC_UNSIGNED => "X86_64_RELOC_UNSIGNED",
        macho::X86_64_RELOC_SIGNED => "X86_64_RELOC_SIGNED",
        macho::X86_64_RELOC_BRANCH => "X86_64_RELOC_BRANCH",
        macho::X86_64_RELOC_GOT_LOAD => "X86_64_RELOC_GOT_LOAD",
        macho::X86_64_RELOC_GOT => "X86_64_RELOC_GOT",
        macho::X86_64_RELOC_SUBTRACTOR => "X86_64_RELOC_SUBTRACTOR",
        macho::X86_64_RELOC_SIGNED_1 => "X86_64_RELOC_SIGNED_1",
        macho::X86_64_RELOC_SIGNED_2 => "X86_64_RELOC_SIGNED_2",
        macho::X86_64_RELOC_SIGNED_4 => "X86_64_RELOC_SIGNED_4",
        macho::X86_64_RELOC_TLV => "X86_64_RELOC_TLV",
        _ => "an unknown x86-64 relocation",
    }
}

#[cfg(test)]
mod tests {
    use blaze_codegen::Arch;
    use object::macho;

    use super::read;
    use crate::model::{Apply, Binding, Data, NativeFixup, Object, SectionKind, Symbol, SymbolDef, SymbolKind};

    const TEXT: u32 = macho::S_ATTR_PURE_INSTRUCTIONS | macho::S_ATTR_SOME_INSTRUCTIONS;
    const EH_FRAME: u32 = macho::S_COALESCED
        | macho::S_ATTR_NO_TOC
        | macho::S_ATTR_STRIP_STATIC_SYMS
        | macho::S_ATTR_LIVE_SUPPORT;
    const SECT: u8 = macho::N_SECT;
    const EXT: u8 = macho::N_EXT;

    struct Sect {
        segment: &'static str,
        name: &'static str,
        addr: u64,
        data: Vec<u8>,
        size: u64,
        zerofill: bool,
        align: u32,
        flags: u32,
        relocations: Vec<[u32; 2]>,
    }

    struct Sym {
        name: &'static str,
        n_type: u8,
        n_sect: u8,
        n_desc: u16,
        n_value: u64,
    }

    /// A hand-made `MH_OBJECT`: one `LC_SEGMENT_64`, the linker options, `LC_SYMTAB`.
    struct Obj {
        cpu: u32,
        subsections: bool,
        sections: Vec<Sect>,
        symbols: Vec<Sym>,
        options: Vec<Vec<&'static str>>,
    }

    fn w16(out: &mut Vec<u8>, value: u16) {
        out.extend_from_slice(&value.to_le_bytes());
    }

    fn w32(out: &mut Vec<u8>, value: u32) {
        out.extend_from_slice(&value.to_le_bytes());
    }

    fn w64(out: &mut Vec<u8>, value: u64) {
        out.extend_from_slice(&value.to_le_bytes());
    }

    fn name16(out: &mut Vec<u8>, name: &str) {
        let mut field = [0u8; 16];
        field[..name.len()].copy_from_slice(name.as_bytes());
        out.extend_from_slice(&field);
    }

    fn rel(address: u32, symbolnum: u32, pcrel: bool, length: u32, external: bool, kind: u8) -> [u32; 2] {
        [
            address,
            (symbolnum & 0x00ff_ffff)
                | u32::from(pcrel) << 24
                | length << 25
                | u32::from(external) << 27
                | u32::from(kind) << 28,
        ]
    }

    fn words(words: &[u32]) -> Vec<u8> {
        words.iter().flat_map(|it| it.to_le_bytes()).collect()
    }

    impl Obj {
        fn new(cpu: u32) -> Obj {
            Obj { cpu, subsections: true, sections: Vec::new(), symbols: Vec::new(), options: Vec::new() }
        }

        /// Adds a section; returns its 1-based ordinal.
        fn section(
            &mut self,
            segment: &'static str,
            name: &'static str,
            addr: u64,
            data: Vec<u8>,
            align: u32,
            flags: u32,
        ) -> u8 {
            let size = data.len() as u64;
            self.sections.push(Sect {
                segment,
                name,
                addr,
                data,
                size,
                zerofill: false,
                align,
                flags,
                relocations: Vec::new(),
            });
            self.sections.len() as u8
        }

        fn zerofill(&mut self, name: &'static str, addr: u64, size: u64, align: u32) -> u8 {
            self.sections.push(Sect {
                segment: "__DATA",
                name,
                addr,
                data: Vec::new(),
                size,
                zerofill: true,
                align,
                flags: macho::S_ZEROFILL,
                relocations: Vec::new(),
            });
            self.sections.len() as u8
        }

        /// Adds an nlist entry; returns its index.
        fn symbol(&mut self, name: &'static str, n_type: u8, n_sect: u8, n_desc: u16, n_value: u64) -> u32 {
            self.symbols.push(Sym { name, n_type, n_sect, n_desc, n_value });
            self.symbols.len() as u32 - 1
        }

        fn relocate(&mut self, ordinal: u8, relocation: [u32; 2]) {
            self.sections[usize::from(ordinal) - 1].relocations.push(relocation);
        }

        fn bytes(&self) -> Vec<u8> {
            let options: Vec<Vec<u8>> = (self.options.iter())
                .map(|words| {
                    let mut strings = Vec::new();
                    for word in words {
                        strings.extend_from_slice(word.as_bytes());
                        strings.push(0);
                    }
                    let size = (12 + strings.len()).next_multiple_of(8);
                    let mut out = Vec::new();
                    w32(&mut out, macho::LC_LINKER_OPTION);
                    w32(&mut out, size as u32);
                    w32(&mut out, words.len() as u32);
                    out.extend(strings);
                    out.resize(size, 0);
                    out
                })
                .collect();
            let segment = 72 + 80 * self.sections.len();
            let commands = segment + options.iter().map(Vec::len).sum::<usize>() + 24;
            let mut at = 32 + commands;
            let mut offsets = Vec::new();
            for section in &self.sections {
                at = at.next_multiple_of(8);
                offsets.push(at);
                at += section.data.len();
            }
            let mut reloffs = Vec::new();
            for section in &self.sections {
                at = at.next_multiple_of(8);
                reloffs.push(at);
                at += 8 * section.relocations.len();
            }
            let symoff = at.next_multiple_of(8);
            let stroff = symoff + 16 * self.symbols.len();
            let mut strings = vec![0u8];
            let mut strx = Vec::new();
            for symbol in &self.symbols {
                strx.push(strings.len() as u32);
                strings.extend_from_slice(symbol.name.as_bytes());
                strings.push(0);
            }

            let mut out = Vec::new();
            w32(&mut out, macho::MH_MAGIC_64);
            w32(&mut out, self.cpu);
            w32(&mut out, 0);
            w32(&mut out, macho::MH_OBJECT);
            w32(&mut out, 2 + options.len() as u32);
            w32(&mut out, commands as u32);
            w32(&mut out, if self.subsections { macho::MH_SUBSECTIONS_VIA_SYMBOLS } else { 0 });
            w32(&mut out, 0);
            w32(&mut out, macho::LC_SEGMENT_64);
            w32(&mut out, segment as u32);
            name16(&mut out, "");
            let vmsize = self.sections.iter().map(|it| it.addr + it.size).max().unwrap_or(0);
            w64(&mut out, 0);
            w64(&mut out, vmsize);
            w64(&mut out, (32 + commands) as u64);
            w64(&mut out, (at - 32 - commands) as u64);
            w32(&mut out, 7);
            w32(&mut out, 7);
            w32(&mut out, self.sections.len() as u32);
            w32(&mut out, 0);
            for (i, section) in self.sections.iter().enumerate() {
                name16(&mut out, section.name);
                name16(&mut out, section.segment);
                w64(&mut out, section.addr);
                w64(&mut out, section.size);
                w32(&mut out, if section.zerofill { 0 } else { offsets[i] as u32 });
                w32(&mut out, section.align);
                let relocated = !section.relocations.is_empty();
                w32(&mut out, if relocated { reloffs[i] as u32 } else { 0 });
                w32(&mut out, section.relocations.len() as u32);
                w32(&mut out, section.flags);
                w32(&mut out, 0);
                w32(&mut out, 0);
                w32(&mut out, 0);
            }
            for option in &options {
                out.extend_from_slice(option);
            }
            w32(&mut out, macho::LC_SYMTAB);
            w32(&mut out, 24);
            w32(&mut out, symoff as u32);
            w32(&mut out, self.symbols.len() as u32);
            w32(&mut out, stroff as u32);
            w32(&mut out, strings.len() as u32);
            assert_eq!(out.len(), 32 + commands);
            for (i, section) in self.sections.iter().enumerate() {
                out.resize(offsets[i], 0);
                out.extend_from_slice(&section.data);
            }
            for (i, section) in self.sections.iter().enumerate() {
                out.resize(reloffs[i], 0);
                for [word0, word1] in &section.relocations {
                    w32(&mut out, *word0);
                    w32(&mut out, *word1);
                }
            }
            out.resize(symoff, 0);
            for (symbol, strx) in self.symbols.iter().zip(strx) {
                w32(&mut out, strx);
                out.push(symbol.n_type);
                out.push(symbol.n_sect);
                w16(&mut out, symbol.n_desc);
                w64(&mut out, symbol.n_value);
            }
            out.extend_from_slice(&strings);
            out
        }

        fn read(&self, arch: Arch) -> Result<Object, String> {
            read("test.o", &self.bytes(), arch)
        }
    }

    fn find<'a>(object: &'a Object, raw: &str) -> (u32, &'a Symbol) {
        let name = crate::macho::core_name(raw.as_bytes()).unwrap();
        let index = object.symbols.iter().position(|it| object.name(it.name) == name).unwrap();
        (index as u32, &object.symbols[index])
    }

    fn fixups(object: &Object, section: usize) -> &[NativeFixup] {
        object.section_fixups(&object.sections[section])
    }

    fn def(object: &Object, raw: &str) -> SymbolDef {
        find(object, raw).1.def
    }

    fn fixup(at: u64, apply: Apply, symbol: u32, addend: i64) -> NativeFixup {
        NativeFixup { at, apply, symbol, addend, minus: None }
    }

    const NOP: u32 = 0xd503_201f;

    #[test]
    fn atoms_split_at_symbols_not_at_local_labels() {
        let mut obj = Obj::new(macho::CPU_TYPE_ARM64);
        let text = obj.section("__TEXT", "__text", 0, words(&[NOP; 6]), 4, TEXT);
        let data = obj.section("__DATA", "__data", 24, vec![0; 16], 3, 0);
        obj.symbol("ltmp0", SECT, text, 0, 0);
        obj.symbol("_a", SECT | EXT, text, 0, 0);
        obj.symbol("_b", SECT | EXT, text, 0, 8);
        obj.symbol("Lfoo", SECT, text, 0, 12);
        obj.symbol("_c", SECT, text, 0, 16);
        obj.symbol("_d", SECT | EXT, data, 0, 32);

        // Read from an unaligned copy too: the reader must not need 8-aligned input.
        let bytes = obj.bytes();
        let mut shifted = vec![0u8];
        shifted.extend_from_slice(&bytes);
        let object = read("test.o", &shifted[1..], Arch::Aarch64).unwrap();

        assert_eq!(object.sections.len(), 5);
        let sizes: Vec<u64> = object.sections.iter().map(|it| it.data.len()).collect();
        assert_eq!(sizes, [8, 8, 8, 8, 8]);
        let aligns: Vec<u64> = object.sections.iter().map(|it| it.align).collect();
        assert_eq!(aligns, [16, 8, 16, 8, 8]);
        for section in &object.sections[..3] {
            assert_eq!(object.name(section.name), "__TEXT,__text");
            assert!(matches!(section.what, SectionKind::Alloc(..)));
        }
        assert_eq!(object.name(object.sections[3].name), "__DATA,__data");
        let nops = words(&[NOP; 2]);
        assert_eq!(object.sections[1].data.bytes(&shifted[1..]), Ok(Some(&nops[..])));
        assert_eq!(object.sections[1].data.bytes(&bytes), Ok(Some(&nops[..])));

        assert_eq!(def(&object, "ltmp0"), SymbolDef::Section { section: 0, value: 0, size: 8 });
        assert_eq!(def(&object, "_a"), SymbolDef::Section { section: 0, value: 0, size: 8 });
        assert_eq!(def(&object, "_b"), SymbolDef::Section { section: 1, value: 0, size: 8 });
        assert_eq!(def(&object, "Lfoo"), SymbolDef::Section { section: 1, value: 4, size: 4 });
        assert_eq!(def(&object, "_c"), SymbolDef::Section { section: 2, value: 0, size: 8 });
        assert_eq!(def(&object, "_d"), SymbolDef::Section { section: 4, value: 0, size: 8 });
        assert_eq!(find(&object, "_a").1.kind, SymbolKind::Function);
        assert_eq!(find(&object, "_d").1.kind, SymbolKind::Data);
        assert_eq!(find(&object, "_c").1.binding, Binding::Local);
        assert_eq!(find(&object, "_a").1.binding, Binding::Global);
        assert!(object.groups.is_empty());

        obj.subsections = false;
        let whole = obj.read(Arch::Aarch64).unwrap();
        assert_eq!(whole.sections.len(), 2);
        assert_eq!(def(&whole, "_b"), SymbolDef::Section { section: 0, value: 8, size: 8 });
    }

    #[test]
    fn zerofill_atoms_are_zero() {
        let mut obj = Obj::new(macho::CPU_TYPE_ARM64);
        let bss = obj.zerofill("__bss", 0, 32, 4);
        obj.symbol("_x", SECT | EXT, bss, 0, 0);
        obj.symbol("_y", SECT | EXT, bss, 0, 16);
        let object = obj.read(Arch::Aarch64).unwrap();
        let data: Vec<&Data> = object.sections.iter().map(|it| &it.data).collect();
        assert_eq!(data, [&Data::Zero(16), &Data::Zero(16)]);
    }

    #[test]
    fn addend_branches_and_pageoff12_shifts() {
        let mut obj = Obj::new(macho::CPU_TYPE_ARM64);
        let code = [
            0x9000_0000, // adrp x0, _t@PAGE + 16
            0x9100_0000, // add x0, x0, _t@PAGEOFF
            0xf940_0001, // ldr x1, [x0, _t@PAGEOFF]
            0xb940_0001, // ldr w1, [x0, _t@PAGEOFF]
            0x3dc0_0000, // ldr q0, [x0, _t@PAGEOFF]
            0x9400_0000, // bl _t - 8
            0x1400_0000, // b _t
        ];
        let text = obj.section("__TEXT", "__text", 0, words(&code), 2, TEXT);
        obj.symbol("_f", SECT | EXT, text, 0, 0);
        let t = obj.symbol("_t", EXT, 0, 0, 0);
        obj.relocate(text, rel(0, 16, false, 2, false, macho::ARM64_RELOC_ADDEND));
        obj.relocate(text, rel(0, t, true, 2, true, macho::ARM64_RELOC_PAGE21));
        for at in [4, 8, 12, 16] {
            obj.relocate(text, rel(at, t, false, 2, true, macho::ARM64_RELOC_PAGEOFF12));
        }
        obj.relocate(text, rel(20, 0x00ff_fff8, false, 2, false, macho::ARM64_RELOC_ADDEND));
        obj.relocate(text, rel(20, t, true, 2, true, macho::ARM64_RELOC_BRANCH26));
        obj.relocate(text, rel(24, t, true, 2, true, macho::ARM64_RELOC_BRANCH26));
        let object = obj.read(Arch::Aarch64).unwrap();
        let (t, _) = find(&object, "_t");
        assert_eq!(fixups(&object, 0), [
            fixup(0, Apply::A64Page21, t, 16),
            fixup(4, Apply::A64Lo12 { shift: 0 }, t, 0),
            fixup(8, Apply::A64Lo12 { shift: 3 }, t, 0),
            fixup(12, Apply::A64Lo12 { shift: 2 }, t, 0),
            fixup(16, Apply::A64Lo12 { shift: 4 }, t, 0),
            fixup(20, Apply::A64Call26, t, -8),
            fixup(24, Apply::A64Jump26, t, 0),
        ]);
    }

    #[test]
    fn got_and_tlv_relocations() {
        let mut obj = Obj::new(macho::CPU_TYPE_ARM64);
        let code = [0x9000_0000, 0xf940_0000, 0x9000_0000, 0xf940_0000];
        let text = obj.section("__TEXT", "__text", 0, words(&code), 2, TEXT);
        obj.symbol("_f", SECT | EXT, text, 0, 0);
        let g = obj.symbol("_g", EXT, 0, 0, 0);
        obj.relocate(text, rel(0, g, true, 2, true, macho::ARM64_RELOC_GOT_LOAD_PAGE21));
        obj.relocate(text, rel(4, g, false, 2, true, macho::ARM64_RELOC_GOT_LOAD_PAGEOFF12));
        obj.relocate(text, rel(8, g, true, 2, true, macho::ARM64_RELOC_TLVP_LOAD_PAGE21));
        obj.relocate(text, rel(12, g, false, 2, true, macho::ARM64_RELOC_TLVP_LOAD_PAGEOFF12));
        let object = obj.read(Arch::Aarch64).unwrap();
        let (g, _) = find(&object, "_g");
        assert_eq!(fixups(&object, 0), [
            fixup(0, Apply::A64GotPage21, g, 0),
            fixup(4, Apply::A64GotLo12, g, 0),
            fixup(8, Apply::TlvPage21, g, 0),
            fixup(12, Apply::TlvLo12, g, 0),
        ]);
    }

    #[test]
    fn subtractor_and_unsigned_are_one_delta64() {
        for (cpu, arch, subtractor, unsigned) in [
            (macho::CPU_TYPE_ARM64, Arch::Aarch64, macho::ARM64_RELOC_SUBTRACTOR, macho::ARM64_RELOC_UNSIGNED),
            (macho::CPU_TYPE_X86_64, Arch::X86_64, macho::X86_64_RELOC_SUBTRACTOR, macho::X86_64_RELOC_UNSIGNED),
        ] {
            let mut obj = Obj::new(cpu);
            let mut bytes = 5u64.to_le_bytes().to_vec();
            bytes.extend_from_slice(&(-3i32).to_le_bytes());
            bytes.extend_from_slice(&[0; 4]);
            let data = obj.section("__DATA", "__data", 0, bytes, 3, 0);
            obj.symbol("_x", SECT | EXT, data, 0, 0);
            let y = obj.symbol("_y", EXT, 0, 0, 0);
            let z = obj.symbol("_z", EXT, 0, 0, 0);
            obj.relocate(data, rel(0, y, false, 3, true, subtractor));
            obj.relocate(data, rel(0, z, false, 3, true, unsigned));
            obj.relocate(data, rel(8, z, false, 2, true, subtractor));
            obj.relocate(data, rel(8, y, false, 2, true, unsigned));
            let object = obj.read(arch).unwrap();
            let (y, _) = find(&object, "_y");
            let (z, _) = find(&object, "_z");
            assert_eq!(fixups(&object, 0), [
                NativeFixup { at: 0, apply: Apply::Delta64, symbol: z, addend: 5, minus: Some(y) },
                NativeFixup { at: 8, apply: Apply::Delta32, symbol: y, addend: -3, minus: Some(z) },
            ]);

            let mut bad = Obj::new(cpu);
            let data = bad.section("__DATA", "__data", 0, vec![0; 8], 3, 0);
            let y = bad.symbol("_y", EXT, 0, 0, 0);
            bad.relocate(data, rel(0, y, false, 3, true, subtractor));
            bad.relocate(data, rel(4, y, false, 3, true, unsigned));
            let error = bad.read(arch).unwrap_err();
            assert!(error.contains("SUBTRACTOR followed by"), "{error}");
        }
    }

    #[test]
    fn local_symbol_plus_addend_targets_the_atom_it_lands_in() {
        // `adrp x0, ltmp0@PAGE + 0x14`: the addend lands in `_b`'s atom, which is what is kept.
        let mut obj = Obj::new(macho::CPU_TYPE_ARM64);
        let text = obj.section("__TEXT", "__text", 0, words(&[0x9000_0000, NOP, NOP, NOP, NOP, NOP]), 2, TEXT);
        obj.symbol("ltmp0", SECT, text, 0, 0);
        obj.symbol("_a", SECT | EXT, text, 0, 0);
        obj.symbol("_b", SECT | EXT, text, 0, 16);
        obj.relocate(text, rel(0, 20, false, 2, false, macho::ARM64_RELOC_ADDEND));
        obj.relocate(text, rel(0, 0, true, 2, true, macho::ARM64_RELOC_PAGE21));
        let object = obj.read(Arch::Aarch64).unwrap();
        let [fixup] = fixups(&object, 0)[..] else { panic!("{:?}", fixups(&object, 0)) };
        assert_eq!((fixup.apply, fixup.addend), (Apply::A64Page21, 4));
        let symbol = &object.symbols[fixup.symbol as usize];
        assert_eq!(symbol.kind, SymbolKind::Section);
        assert_eq!(symbol.def, SymbolDef::Section { section: 1, value: 0, size: 8 });
    }

    #[test]
    fn non_extern_relocations_target_the_right_atom() {
        let mut obj = Obj::new(macho::CPU_TYPE_X86_64);
        let mut code = vec![0x48, 0x8d, 0x05];
        code.extend_from_slice(&(32i32 - 7).to_le_bytes()); // lea rax, [rip + data]
        code.resize(32, 0x90);
        let text = obj.section("__TEXT", "__text", 0, code, 4, TEXT);
        let data = obj.section("__DATA", "__data", 32, 20u64.to_le_bytes().to_vec(), 3, 0);
        obj.symbol("_f", SECT | EXT, text, 0, 0);
        obj.symbol("_g", SECT | EXT, text, 0, 16);
        obj.relocate(text, rel(3, u32::from(data), true, 2, false, macho::X86_64_RELOC_SIGNED));
        obj.relocate(data, rel(0, u32::from(text), false, 3, false, macho::X86_64_RELOC_UNSIGNED));
        let object = obj.read(Arch::X86_64).unwrap();
        assert_eq!(object.sections.len(), 3);

        let [pointer] = fixups(&object, 2)[..] else { panic!("{:?}", fixups(&object, 2)) };
        assert_eq!((pointer.at, pointer.apply, pointer.addend), (0, Apply::Abs64, 4));
        let target = &object.symbols[pointer.symbol as usize];
        assert_eq!(target.def, SymbolDef::Section { section: 1, value: 0, size: 16 });
        assert_eq!((target.kind, target.binding, object.name(target.name)), (SymbolKind::Section, Binding::Local, ""));

        let [lea] = fixups(&object, 0)[..] else { panic!("{:?}", fixups(&object, 0)) };
        assert_eq!((lea.at, lea.apply, lea.addend), (3, Apply::Rel32, -4));
        let target = &object.symbols[lea.symbol as usize];
        assert_eq!(target.def, SymbolDef::Section { section: 2, value: 0, size: 8 });
    }

    #[test]
    fn x86_64_pc_relative_addends() {
        let mut obj = Obj::new(macho::CPU_TYPE_X86_64);
        let mut code = vec![0xc7, 0x05, 8, 0, 0, 0, 1, 0, 0, 0]; // movl $1, _x+8(%rip)
        code.extend_from_slice(&[0xe8, 0, 0, 0, 0]); // call _x
        code.extend_from_slice(&[0x48, 0x8b, 0x05, 0, 0, 0, 0]); // movq _x@GOTPCREL(%rip), %rax
        code.extend_from_slice(&[0x48, 0x8b, 0x3d, 0, 0, 0, 0]); // movq _x@TLVP(%rip), %rdi
        let text = obj.section("__TEXT", "__text", 0, code, 4, TEXT);
        obj.symbol("_f", SECT | EXT, text, 0, 0);
        let x = obj.symbol("_x", EXT, 0, 0, 0);
        obj.relocate(text, rel(2, x, true, 2, true, macho::X86_64_RELOC_SIGNED_4));
        obj.relocate(text, rel(11, x, true, 2, true, macho::X86_64_RELOC_BRANCH));
        obj.relocate(text, rel(18, x, true, 2, true, macho::X86_64_RELOC_GOT_LOAD));
        obj.relocate(text, rel(25, x, true, 2, true, macho::X86_64_RELOC_TLV));
        let object = obj.read(Arch::X86_64).unwrap();
        let (x, _) = find(&object, "_x");
        assert_eq!(fixups(&object, 0), [
            fixup(2, Apply::Rel32, x, 0),
            fixup(11, Apply::X86Branch32, x, -4),
            fixup(18, Apply::X86GotPcRel32, x, -4),
            fixup(25, Apply::TlvRel32, x, -4),
        ]);
    }

    #[test]
    fn thread_local_descriptors() {
        let mut obj = Obj::new(macho::CPU_TYPE_ARM64);
        let vars = obj.section("__DATA", "__thread_vars", 0, vec![0; 24], 3, macho::S_THREAD_LOCAL_VARIABLES);
        let tdata = obj.section("__DATA", "__thread_data", 24, vec![7; 8], 3, macho::S_THREAD_LOCAL_REGULAR);
        obj.symbol("_v", SECT | EXT, vars, 0, 0);
        let init = obj.symbol("_v$tlv$init", SECT, tdata, 0, 24);
        let bootstrap = obj.symbol("__tlv_bootstrap", EXT, 0, 0, 0);
        obj.relocate(vars, rel(0, bootstrap, false, 3, true, macho::ARM64_RELOC_UNSIGNED));
        obj.relocate(vars, rel(16, init, false, 3, true, macho::ARM64_RELOC_UNSIGNED));
        let object = obj.read(Arch::Aarch64).unwrap();
        assert_eq!(find(&object, "_v$tlv$init").1.kind, SymbolKind::Tls);
        let (bootstrap, _) = find(&object, "__tlv_bootstrap");
        let fixups = fixups(&object, 0);
        assert_eq!(fixups.len(), 2);
        assert_eq!(fixups[0], fixup(0, Apply::Abs64, bootstrap, 0));
        assert_eq!((fixups[1].at, fixups[1].apply, fixups[1].addend), (16, Apply::DtpOff64, 0));
        assert_eq!(object.symbols[fixups[1].symbol as usize].def, SymbolDef::Section { section: 1, value: 0, size: 8 });
    }

    #[test]
    fn symbol_kinds_bindings_and_commons() {
        let mut obj = Obj::new(macho::CPU_TYPE_ARM64);
        let text = obj.section("__TEXT", "__text", 0, words(&[NOP; 4]), 2, TEXT);
        obj.symbol("_common", EXT, 0, 3 << 8, 16);
        obj.symbol("_weak", SECT | EXT, text, macho::N_WEAK_DEF, 0);
        obj.symbol("_hidden", SECT | EXT | macho::N_PEXT, text, 0, 4);
        obj.symbol("_made_local", SECT | macho::N_PEXT, text, 0, 8);
        obj.symbol("_weak_ref", EXT, 0, macho::N_WEAK_REF, 0);
        obj.symbol("_abs", macho::N_ABS | EXT, 0, 0, 0x1234);
        obj.symbol("_f", 0x24, text, 0, 0); // N_FUN: debugging, left out
        let object = obj.read(Arch::Aarch64).unwrap();
        assert_eq!(object.symbols.len(), 6);

        let common = find(&object, "_common").1;
        assert_eq!(common.def, SymbolDef::Common { size: 16, align: 8 });
        assert_eq!((common.binding, common.kind), (Binding::Global, SymbolKind::Data));
        let weak = find(&object, "_weak").1;
        assert_eq!((weak.binding, weak.hidden), (Binding::Weak, false));
        let hidden = find(&object, "_hidden").1;
        assert_eq!((hidden.binding, hidden.hidden), (Binding::Global, true));
        let made_local = find(&object, "_made_local").1;
        assert_eq!((made_local.binding, made_local.hidden), (Binding::Local, true));
        let weak_ref = find(&object, "_weak_ref").1;
        assert_eq!((weak_ref.def, weak_ref.binding), (SymbolDef::Undefined, Binding::Weak));
        assert_eq!(find(&object, "_abs").1.def, SymbolDef::Absolute(0x1234));
    }

    #[test]
    fn no_dead_strip_retains_every_atom() {
        let mut obj = Obj::new(macho::CPU_TYPE_ARM64);
        let text = obj.section("__TEXT", "__text", 0, words(&[NOP; 2]), 2, TEXT);
        let kept = obj.section("__DATA", "__kept", 8, vec![0; 16], 3, macho::S_ATTR_NO_DEAD_STRIP);
        obj.symbol("_f", SECT | EXT, text, 0, 0);
        obj.symbol("_k0", SECT | EXT, kept, 0, 8);
        obj.symbol("_k1", SECT | EXT, kept, 0, 16);
        let object = obj.read(Arch::Aarch64).unwrap();
        let retain: Vec<bool> = object.sections.iter().map(|it| it.retain).collect();
        assert_eq!(retain, [false, true, true]);
    }

    #[test]
    fn refused_inputs() {
        let mut obj = Obj::new(macho::CPU_TYPE_ARM64);
        let data = obj.section("__DATA", "__data", 0, vec![0; 8], 3, 0);
        let s = obj.symbol("_s", EXT, 0, 0, 0);
        obj.relocate(data, rel(0, s, false, 3, true, macho::ARM64_RELOC_AUTHENTICATED_POINTER));
        let error = obj.read(Arch::Aarch64).unwrap_err();
        assert!(error.contains("ARM64_RELOC_AUTHENTICATED_POINTER") && error.contains("__DATA,__data at 0x0"), "{error}");

        let mut obj = Obj::new(macho::CPU_TYPE_ARM64);
        let data = obj.section("__DATA", "__data", 0, vec![0; 8], 3, 0);
        let s = obj.symbol("_s", EXT, 0, 0, 0);
        obj.relocate(data, rel(0, 4, false, 2, false, macho::ARM64_RELOC_ADDEND));
        obj.relocate(data, rel(0, s, false, 3, true, macho::ARM64_RELOC_UNSIGNED));
        let error = obj.read(Arch::Aarch64).unwrap_err();
        assert!(error.contains("ARM64_RELOC_ADDEND before ARM64_RELOC_UNSIGNED"), "{error}");

        let mut obj = Obj::new(macho::CPU_TYPE_ARM64);
        let text = obj.section("__TEXT", "__text", 0, words(&[NOP]), 2, TEXT);
        let s = obj.symbol("_s", EXT, 0, 0, 0);
        obj.relocate(text, rel(0, s, false, 2, true, macho::ARM64_RELOC_PAGEOFF12));
        let error = obj.read(Arch::Aarch64).unwrap_err();
        assert!(error.contains("PAGEOFF12 on instruction 0xd503201f"), "{error}");

        let obj = Obj::new(macho::CPU_TYPE_X86_64);
        assert!(obj.read(Arch::Aarch64).unwrap_err().contains("CPU type"));
        let mut thin = obj.bytes();
        thin[..4].copy_from_slice(&macho::MH_MAGIC.to_le_bytes());
        assert!(read("test.o", &thin, Arch::X86_64).unwrap_err().contains("32-bit"));
        let mut fat = obj.bytes();
        fat[..4].copy_from_slice(&macho::FAT_MAGIC.to_be_bytes());
        assert!(read("test.o", &fat, Arch::X86_64).unwrap_err().contains("universal"));
        let mut dylib = obj.bytes();
        dylib[12..16].copy_from_slice(&macho::MH_DYLIB.to_le_bytes());
        assert!(read("test.o", &dylib, Arch::X86_64).unwrap_err().contains("MH_OBJECT"));

        let mut obj = Obj::new(macho::CPU_TYPE_X86_64);
        obj.symbol("_i", macho::N_INDR | EXT, 0, 0, 0);
        assert!(obj.read(Arch::X86_64).unwrap_err().contains("N_INDR"));
    }

    #[test]
    fn linker_options_are_libraries() {
        let mut obj = Obj::new(macho::CPU_TYPE_ARM64);
        obj.options = vec![vec!["-lz"], vec!["-framework", "CoreFoundation"]];
        let object = obj.read(Arch::Aarch64).unwrap();
        assert_eq!(object.libraries, ["-lz", "-framework CoreFoundation"]);
    }

    #[test]
    fn eh_frame_pc_begin_without_a_relocation_is_made_explicit() {
        let mut obj = Obj::new(macho::CPU_TYPE_X86_64);
        let text = obj.section("__TEXT", "__text", 0, vec![0x90; 32], 4, TEXT);
        obj.symbol("_f", SECT | EXT, text, 0, 0);
        obj.symbol("_g", SECT | EXT, text, 0, 16);
        // CIE: "zR", FDE pointers pc-relative 8-byte (0x10), as clang writes them for x86-64.
        let mut eh = Vec::new();
        w32(&mut eh, 16);
        w32(&mut eh, 0);
        eh.extend_from_slice(&[1, b'z', b'R', 0, 1, 0x78, 0x10, 1, 0x10, 0, 0, 0]);
        // FDE at 20, its CIE pointer at 24 and pc_begin at 28 (address 32 + 28): `_g`.
        w32(&mut eh, 24);
        w32(&mut eh, 24);
        w64(&mut eh, (16i64 - 60) as u64);
        w64(&mut eh, 16);
        eh.extend_from_slice(&[0, 0, 0, 0]);
        let frame = obj.section("__TEXT", "__eh_frame", 32, eh, 3, EH_FRAME);
        assert_eq!(frame, 2);
        let object = obj.read(Arch::X86_64).unwrap();
        let eh = &object.sections[2];
        assert_eq!(eh.what, SectionKind::EhFrame);
        let [pc_begin] = object.section_fixups(eh)[..] else { panic!("{:?}", object.section_fixups(eh)) };
        assert_eq!((pc_begin.at, pc_begin.apply, pc_begin.addend), (28, Apply::Rel64, 0));
        let target = &object.symbols[pc_begin.symbol as usize];
        assert_eq!(target.def, SymbolDef::Section { section: 1, value: 0, size: 16 });
    }

    #[test]
    fn eh_frame_fdes_find_their_cie_and_section_by_search() {
        let mut obj = Obj::new(macho::CPU_TYPE_X86_64);
        let text = obj.section("__TEXT", "__text", 0, vec![0x90; 32], 4, TEXT);
        let data = obj.section("__DATA", "__data", 32, vec![0; 16], 3, 0);
        obj.symbol("_f", SECT | EXT, text, 0, 0);
        obj.symbol("_g", SECT | EXT, text, 0, 16);
        obj.symbol("_h", SECT | EXT, data, 0, 32);
        let cie = |eh: &mut Vec<u8>| {
            w32(eh, 16);
            w32(eh, 0);
            eh.extend_from_slice(&[1, b'z', b'R', 0, 1, 0x78, 0x10, 1, 0x10, 0, 0, 0]);
        };
        // An FDE whose CIE pointer (at `at + 4`) leads back to `cie`, `pc_begin` at `at + 8`.
        let fde = |eh: &mut Vec<u8>, cie: usize, target: i64| {
            let at = eh.len();
            w32(eh, 24);
            w32(eh, (at + 4 - cie) as u32);
            w64(eh, (target - (48 + at as i64 + 8)) as u64);
            w64(eh, 16);
            eh.extend_from_slice(&[0, 0, 0, 0]);
        };
        let mut eh = Vec::new();
        cie(&mut eh);
        fde(&mut eh, 0, 16);
        cie(&mut eh);
        fde(&mut eh, 48, 32);
        fde(&mut eh, 0, 0);
        obj.section("__TEXT", "__eh_frame", 48, eh, 3, EH_FRAME);
        let object = obj.read(Arch::X86_64).unwrap();
        let eh = &object.sections[3];
        let got: Vec<(u64, Apply, i64, SymbolDef)> = (object.section_fixups(eh).iter())
            .map(|it| (it.at, it.apply, it.addend, object.symbols[it.symbol as usize].def))
            .collect();
        assert_eq!(got, [
            (28, Apply::Rel64, 0, SymbolDef::Section { section: 1, value: 0, size: 16 }),
            (76, Apply::Rel64, 0, SymbolDef::Section { section: 2, value: 0, size: 16 }),
            (104, Apply::Rel64, 0, SymbolDef::Section { section: 0, value: 0, size: 16 }),
        ]);
    }

    /// xorshift64: the differential test's seeded inputs.
    struct Rng(u64);

    impl Rng {
        fn below(&mut self, n: u64) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0 % n
        }
    }

    /// An atom as the reader had it before it read in place: its section's name, its alignment,
    /// a copy of its bytes (`Data::Bytes`) or `Data::Zero`, its own fixups.
    type Atom = (String, u64, Data, Vec<NativeFixup>);

    /// A symbol with its name spelled out: name, definition, binding, kind, hidden.
    type Seen = (String, SymbolDef, Binding, SymbolKind, bool);

    fn seen(object: &Object) -> Vec<Seen> {
        (object.symbols.iter())
            .map(|it| (object.name(it.name).to_owned(), it.def, it.binding, it.kind, it.hidden))
            .collect()
    }

    /// The reader's rules for atoms, symbol sizes, section symbols and fixups, written the plain
    /// way the reader had them before its atoms became one sorted column: a `Vec` of starts and
    /// of bounds per Mach-O section, linear scans, fixups pushed into each atom and sorted there,
    /// each atom's bytes copied out of its section.
    fn model(obj: &Obj) -> (Vec<Atom>, Vec<Seen>) {
        let n = obj.sections.len();
        let splits = |s: usize| obj.subsections && obj.sections[s].flags != macho::S_CSTRING_LITERALS;
        let mut starts: Vec<Vec<u64>> = vec![vec![0]; n];
        let mut bounds: Vec<Vec<u64>> = vec![Vec::new(); n];
        for sym in &obj.symbols {
            if sym.n_type & macho::N_TYPE != SECT
                || matches!(sym.name.as_bytes()[0], b'L' | b'l')
                || sym.n_desc & macho::N_ALT_ENTRY != 0
            {
                continue;
            }
            let s = usize::from(sym.n_sect) - 1;
            let offset = sym.n_value - obj.sections[s].addr;
            bounds[s].push(offset);
            if splits(s) {
                starts[s].push(offset);
            }
        }
        let mut first = Vec::new();
        let mut atoms: Vec<Atom> = Vec::new();
        for s in 0..n {
            starts[s].sort_unstable();
            starts[s].dedup();
            bounds[s].sort_unstable();
            bounds[s].dedup();
            first.push(atoms.len());
            let sect = &obj.sections[s];
            for (j, &start) in starts[s].iter().enumerate() {
                let end = starts[s].get(j + 1).copied().unwrap_or(sect.size);
                let data = if sect.zerofill {
                    Data::Zero(end - start)
                } else {
                    Data::Bytes(sect.data[start as usize..end as usize].to_vec())
                };
                let align = 1u64 << sect.align;
                let align = if start == 0 { align } else { align.min(1 << start.trailing_zeros()) };
                atoms.push((format!("{},{}", sect.segment, sect.name), align, data, Vec::new()));
            }
        }
        let atom = |s: usize, offset: u64| starts[s].iter().rposition(|&it| it <= offset).unwrap();

        let mut symbols = Vec::new();
        for sym in &obj.symbols {
            let (def, kind) = if sym.n_type & macho::N_TYPE == SECT {
                let s = usize::from(sym.n_sect) - 1;
                let offset = sym.n_value - obj.sections[s].addr;
                let j = atom(s, offset);
                let end = starts[s].get(j + 1).copied().unwrap_or(obj.sections[s].size);
                let next = bounds[s].iter().copied().find(|&it| it > offset).unwrap_or(end);
                let def = SymbolDef::Section {
                    section: (first[s] + j) as u32,
                    value: offset - starts[s][j],
                    size: next.min(end) - offset,
                };
                let kind = if obj.sections[s].flags & TEXT != 0 { SymbolKind::Function } else { SymbolKind::Data };
                (def, kind)
            } else {
                (SymbolDef::Undefined, SymbolKind::Other)
            };
            symbols.push((
                crate::macho::core_name(sym.name.as_bytes()).unwrap(),
                def,
                if sym.n_type & EXT != 0 { Binding::Global } else { Binding::Local },
                kind,
                false,
            ));
        }

        let mut section_symbols: Vec<Option<u32>> = vec![None; atoms.len()];
        for s in 0..n {
            let bytes = &obj.sections[s].data;
            for &[word0, word1] in &obj.sections[s].relocations {
                let at = u64::from(word0);
                let field = |width: usize| {
                    let mut word = [0u8; 8];
                    word[..width].copy_from_slice(&bytes[at as usize..at as usize + width]);
                    u64::from_le_bytes(word)
                };
                let symbolnum = word1 & 0x00ff_ffff;
                let (apply, symbol, addend) = if (word1 >> 28) as u8 == macho::ARM64_RELOC_BRANCH26 {
                    let call = field(4) & 0x8000_0000 != 0;
                    (if call { Apply::A64Call26 } else { Apply::A64Jump26 }, symbolnum, 0)
                } else if word1 >> 27 & 1 != 0 {
                    (Apply::Abs64, symbolnum, field(8) as i64)
                } else {
                    let t = symbolnum as usize - 1;
                    let offset = field(8) - obj.sections[t].addr;
                    let j = atom(t, offset);
                    let at_atom = first[t] + j;
                    let symbol = *section_symbols[at_atom].get_or_insert_with(|| {
                        symbols.push((
                            String::new(),
                            SymbolDef::Section { section: at_atom as u32, value: 0, size: atoms[at_atom].2.len() },
                            Binding::Local,
                            SymbolKind::Section,
                            false,
                        ));
                        symbols.len() as u32 - 1
                    });
                    (Apply::Abs64, symbol, (offset - starts[t][j]) as i64)
                };
                let j = atom(s, at);
                let fixup = NativeFixup { at: at - starts[s][j], apply, symbol, addend, minus: None };
                atoms[first[s] + j].3.push(fixup);
            }
        }
        for atom in &mut atoms {
            atom.3.sort_by_key(|it| it.at);
        }
        (atoms, symbols)
    }

    /// A seeded object: text, data, cstring and zerofill sections; symbols, local labels and
    /// alternate entries at random places; `BRANCH26` and extern and non-extern `UNSIGNED`
    /// relocations in a shuffled table.
    fn seeded(seed: u64) -> Obj {
        let mut rng = Rng(0x9e37_79b9_7f4a_7c15 ^ (seed + 1).wrapping_mul(0x2545_f491_4f6c_dd1d));
        const KINDS: [(&str, &str, u32); 4] = [
            ("__TEXT", "__text", TEXT),
            ("__DATA", "__data", 0),
            ("__TEXT", "__cstring", macho::S_CSTRING_LITERALS),
            ("__DATA", "__bss", macho::S_ZEROFILL),
        ];
        let mut obj = Obj::new(macho::CPU_TYPE_ARM64);
        obj.subsections = rng.below(4) != 0;
        let mut layout = Vec::new();
        let mut addr = 0;
        for _ in 0..1 + rng.below(4) {
            let kind = rng.below(4) as usize;
            let size = 8 * (1 + rng.below(8));
            addr = u64::next_multiple_of(addr, 16);
            layout.push((kind, addr, size, rng.below(5) as u32));
            addr += size;
        }
        let leak = |name: String| -> &'static str { Box::leak(name.into_boxed_str()) };
        let undefined: Vec<u32> = (0..1 + rng.below(3))
            .map(|i| obj.symbol(leak(format!("_u{i}")), EXT, 0, 0, 0))
            .collect();
        for i in 0..rng.below(14) {
            let s = rng.below(layout.len() as u64) as usize;
            let (kind, addr, size, _) = layout[s];
            let step = if kind == 0 { 4 } else { 8 };
            let offset = step * rng.below(size / step + 1);
            let (name, n_type, n_desc) = match rng.below(6) {
                0 => (format!("_s{i}"), SECT | EXT, 0),
                1 => (format!("_s{i}"), SECT, 0),
                2 => (format!("ltmp{i}"), SECT, 0),
                3 => (format!("L{i}"), SECT, 0),
                4 => (format!("l{i}"), SECT, 0),
                _ => (format!("_alt{i}"), SECT | EXT, macho::N_ALT_ENTRY),
            };
            obj.symbol(leak(name), n_type, s as u8 + 1, n_desc, addr + offset);
        }
        for (s, &(kind, addr, size, align)) in layout.iter().enumerate() {
            let (segment, name, flags) = KINDS[kind];
            if flags == macho::S_ZEROFILL {
                obj.zerofill(name, addr, size, align);
                continue;
            }
            let mut data: Vec<u8> = (0..size).map(|_| rng.below(256) as u8).collect();
            let mut relocations = Vec::new();
            let step = if kind == 0 { 4 } else { 8 };
            for at in (0..size).step_by(step as usize) {
                if rng.below(3) != 0 {
                    continue;
                }
                let target = undefined[rng.below(undefined.len() as u64) as usize];
                if kind == 0 {
                    relocations.push(rel(at as u32, target, true, 2, true, macho::ARM64_RELOC_BRANCH26));
                } else if rng.below(2) == 0 {
                    relocations.push(rel(at as u32, target, false, 3, true, macho::ARM64_RELOC_UNSIGNED));
                } else {
                    let t = rng.below(layout.len() as u64) as usize;
                    let (_, t_addr, t_size, _) = layout[t];
                    let pointer = t_addr + rng.below(t_size + 1);
                    data[at as usize..at as usize + 8].copy_from_slice(&pointer.to_le_bytes());
                    relocations.push(rel(at as u32, t as u32 + 1, false, 3, false, macho::ARM64_RELOC_UNSIGNED));
                }
            }
            for i in (1..relocations.len()).rev() {
                relocations.swap(i, rng.below(i as u64 + 1) as usize);
            }
            let ordinal = obj.section(segment, name, addr, data, align, flags);
            assert_eq!(usize::from(ordinal), s + 1);
            for relocation in relocations {
                obj.relocate(ordinal, relocation);
            }
        }
        obj
    }

    #[test]
    fn seeded_objects_read_as_the_per_section_model_reads_them() {
        for seed in 0..256 {
            let obj = seeded(seed);
            let input = obj.bytes();
            let object = read("test.o", &input, Arch::Aarch64).unwrap_or_else(|e| panic!("seed {seed}: {e}"));
            let (atoms, symbols) = model(&obj);
            assert_eq!(object.sections.len(), atoms.len(), "seed {seed}");
            for (i, (got, (name, align, data, fixups))) in object.sections.iter().zip(&atoms).enumerate() {
                assert_eq!(
                    (
                        object.name(got.name),
                        got.align,
                        got.data.len(),
                        got.data.bytes(&input),
                        object.section_fixups(got),
                        got.retain,
                    ),
                    (name.as_str(), *align, data.len(), data.bytes(&[]), &fixups[..], false),
                    "seed {seed}, atom {i}"
                );
            }
            assert_eq!(seen(&object), symbols, "seed {seed}");
            // The arena holds each Mach-O section's name once and each symbol's, nothing else.
            let named = (obj.sections.iter()).map(|it| it.segment.len() + 1 + it.name.len()).sum::<usize>()
                + symbols.iter().map(|it| it.0.len()).sum::<usize>();
            assert_eq!(object.names.bytes(), named, "seed {seed}");
            // Every fixup is in exactly one atom's run, in atom order.
            let mut next = 0;
            for section in &object.sections {
                if !section.fixups.is_empty() {
                    assert_eq!(section.fixups.start, next, "seed {seed}");
                    next = section.fixups.end;
                }
            }
            assert_eq!(next as usize, object.fixups.len(), "seed {seed}");
        }
    }

    #[test]
    fn every_input_range_holds_the_bytes_the_old_copy_held() {
        for seed in 0..256 {
            let obj = seeded(seed);
            let bytes = obj.bytes();
            let (atoms, _) = model(&obj);
            let aligned = read("test.o", &bytes, Arch::Aarch64).unwrap_or_else(|e| panic!("seed {seed}: {e}"));
            // From unaligned slices too, read in place: the ranges are of the slice given.
            for shift in [0, 1, 3, 7] {
                let mut shifted = vec![0u8; shift];
                shifted.extend_from_slice(&bytes);
                let input = &shifted[shift..];
                let object = read("test.o", input, Arch::Aarch64).unwrap_or_else(|e| panic!("seed {seed}: {e}"));
                assert_eq!(object.sections.len(), atoms.len(), "seed {seed}");
                for (i, (got, atom)) in object.sections.iter().zip(&atoms).enumerate() {
                    match (&got.data, &atom.2) {
                        (Data::Input(range), Data::Bytes(copy)) => {
                            let held = usize::try_from(range.start)
                                .ok()
                                .zip(usize::try_from(range.end).ok())
                                .and_then(|(start, end)| input.get(start..end));
                            assert_eq!(held, Some(&copy[..]), "seed {seed}, shift {shift}, atom {i}");
                            assert_eq!(got.data, aligned.sections[i].data, "seed {seed}, shift {shift}, atom {i}");
                        }
                        (Data::Zero(got), Data::Zero(want)) => {
                            assert_eq!(got, want, "seed {seed}, shift {shift}, atom {i}");
                        }
                        (got, want) => panic!("seed {seed}, shift {shift}, atom {i}: {got:?}, the old copy {want:?}"),
                    }
                }
            }
        }
    }
}
