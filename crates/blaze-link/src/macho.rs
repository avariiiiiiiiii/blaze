//! The Mach-O format of the linker: macOS executables (`MH_EXECUTE`, position-independent)
//! and dynamic libraries (`MH_DYLIB`) on AArch64 and x86-64, laid out as ld64 lays them.
//!
//! Segments, in address order: `__PAGEZERO` (an executable's first 4 GiB, unmapped),
//! `__TEXT` (the headers, code, read-only data, `__unwind_info`, `__eh_frame`: read and
//! run), `__DATA_CONST` (what the loader fixes up and then makes read-only: the GOT,
//! initializers, constant data with pointers, the C objects' `__compact_unwind`), `__DATA`
//! (the jump table's cells, data, thread-locals, zeroed data), `__DWARF` when the image carries debug
//! info, and `__LINKEDIT` (what dyld reads: the rebase and bind opcodes, the export trie, the
//! symbol table, the function starts and, last, the code signature, without which macOS on
//! arm64 kills the program). Addresses the loader fixes up are told it by opcodes
//! (`LC_DYLD_INFO_ONLY`), not in chains.
//!
//! Every region but a zeroed one is in the file at its address less the base, so a region's
//! offset never moves while its address doesn't. dyld finds the thread-local variable
//! descriptors by their section's type (`S_THREAD_LOCAL_VARIABLES`): they are all in
//! [`Class::ThreadVars`], C objects' too.
//!
//! Names in Mach-O carry a leading underscore and the core's don't ([`push_core_name`],
//! [`spelled`]); a Mach-O name without one (`section$start$...`) is the core's with a
//! leading `\x01`, as LLVM writes a name taken as it is.

mod archive;
mod dylib;
mod input;
mod linkedit;
mod sha256;
mod sign;
mod tbd;
#[cfg(test)]
mod tests;
mod unwind;

use std::cmp::Ordering;
use std::sync::Mutex;

use blaze_codegen::Arch;
use blaze_ids::radix;
use object::macho;

use crate::model::{
    Class, Counts, Data, File, Holds, Name, NativeFile, Names, Placed, Plan, Prot, Region,
    SectionKind, Special, Writer,
};
use crate::output::Platform;
use crate::{Format, LinkError, NativeKind, Output, OutputKind};

/// Where an executable's image starts: after `__PAGEZERO`, which catches 32-bit pointers.
const BASE: u64 = 0x1_0000_0000;
/// The room for the header and the load commands, which grow with the libraries and with an
/// incremental link's sections.
const HEADERS: u64 = 0x4000;
const HEADER: usize = 32;
const SEGMENT: usize = 72;
const SECTION: usize = 80;
const NLIST: usize = 16;
const UNWIND_INFO: &str = "__unwind_info";

fn error(what: impl Into<String>) -> LinkError {
    LinkError { function: None, what: what.into() }
}

pub(crate) fn writer(output: &Output) -> Result<Box<dyn Writer>, LinkError> {
    let (cpu, subtype, page) = match output.arch {
        Arch::Aarch64 => (macho::CPU_TYPE_ARM64, macho::CPU_SUBTYPE_ARM64_ALL, 0x4000),
        Arch::X86_64 => (macho::CPU_TYPE_X86_64, macho::CPU_SUBTYPE_X86_64_ALL, 0x1000),
        Arch::Wasm32 => return Err(error("a Mach-O image for wasm32, which has no Mach-O")),
    };
    let Some(platform) = output.platform else {
        return Err(error(
            "a Mach-O image without a platform (`Output::platform`): `LC_BUILD_VERSION` says \
             which OS and SDK it is for, and dyld refuses an image without it",
        ));
    };
    let cache = output.incremental.then(Mutex::default);
    Ok(Box::new(MachO { output: output.clone(), cpu, subtype, page, platform, cache }))
}

struct MachO {
    output: Output,
    cpu: u32,
    subtype: u32,
    page: u64,
    platform: Platform,
    /// The last signature's pages, for an image linked again in place. Locked only by
    /// [`Writer::file`], which the core calls once per link of the image, so never contended
    /// and never held across a call out of the writer.
    cache: Option<Mutex<sign::Cache>>,
}

/// [`push_core_name`] as a `String`: the readers' tests compare against it.
#[cfg(test)]
pub(super) fn core_name(name: &[u8]) -> Result<String, String> {
    let name = std::str::from_utf8(name)
        .map_err(|_| format!("the symbol `{}` is not UTF-8", String::from_utf8_lossy(name)))?;
    Ok(match name.strip_prefix('_') {
        Some(rest) => rest.to_owned(),
        None => format!("\x01{name}"),
    })
}

/// The name the core knows a Mach-O symbol by, added to a file's `names`: without its leading
/// underscore, or with a leading `\x01` when it has none. No `String` of its own: a name
/// without an underscore (clang's kept `ltmp0`, `l_` labels) gains its `\x01` on the stack.
pub(super) fn push_core_name(names: &mut Names, name: &[u8]) -> Result<Name, String> {
    let text = std::str::from_utf8(name)
        .map_err(|_| format!("the symbol `{}` is not UTF-8", String::from_utf8_lossy(name)))?;
    if let Some(rest) = text.strip_prefix('_') {
        return names.push(rest);
    }
    let mut buffer = [0u8; 256];
    match buffer.get_mut(1..=text.len()) {
        Some(tail) => {
            tail.copy_from_slice(text.as_bytes());
            buffer[0] = 1;
            let marked = std::str::from_utf8(&buffer[..=text.len()])
                .expect("`\\x01` and a `str` are UTF-8");
            names.push(marked)
        }
        None => names.push(&format!("\x01{text}")),
    }
}

/// The Mach-O name of a symbol the core names `name`.
#[cfg(test)]
pub(super) fn macho_name(name: &str) -> String {
    let (prefix, rest) = spelled(name);
    let mut out = String::with_capacity(prefix.len() + rest.len());
    out.push_str(prefix);
    out.push_str(rest);
    out
}

/// [`macho_name`] in its two parts, built without allocating: the underscore it gains (or
/// none), and the rest.
fn spelled(name: &str) -> (&'static str, &str) {
    match name.strip_prefix('\x01') {
        Some(rest) => ("", rest),
        None => ("_", name),
    }
}

/// Two core names in the order of their Mach-O names' bytes, as the symbol table and the
/// trie sort them.
fn macho_order(a: &str, b: &str) -> Ordering {
    let ((a_prefix, a), (b_prefix, b)) = (spelled(a), spelled(b));
    if a_prefix == b_prefix {
        return a.as_bytes().cmp(b.as_bytes());
    }
    (a_prefix.bytes().chain(a.bytes())).cmp(b_prefix.bytes().chain(b.bytes()))
}

/// The class the C objects' `__compact_unwind` sections are placed in: relocated as data,
/// where each entry's personality field is a cell the loader fixes up, which `__unwind_info`
/// refers to the personality by.
pub(super) fn compact_unwind() -> Class {
    Class::Named { name: "__LD,__compact_unwind".to_owned(), write: true, exec: false }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum Segment {
    Text,
    DataConst,
    Data,
}

impl Segment {
    fn of(name: &str) -> Option<Segment> {
        match name {
            "__TEXT" => Some(Segment::Text),
            "__DATA_CONST" | "__LD" => Some(Segment::DataConst),
            "__DATA" => Some(Segment::Data),
            _ => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Segment::Text => "__TEXT",
            Segment::DataConst => "__DATA_CONST",
            Segment::Data => "__DATA",
        }
    }

    fn prot(self) -> Prot {
        match self {
            Segment::Text => Prot::RX,
            // Written only by the loader, before the program runs.
            Segment::DataConst => Prot::R,
            Segment::Data => Prot::RW,
        }
    }
}

/// The segment of a class and its place in the segment.
fn place(class: &Class) -> (Segment, u8) {
    match class {
        Class::Text => (Segment::Text, 0),
        Class::Patch => (Segment::Text, 1),
        Class::Stubs => (Segment::Text, 2),
        Class::Plt => (Segment::Text, 3),
        Class::Veneers => (Segment::Text, 4),
        Class::Named { name, .. } => match Segment::of(split(name).0) {
            Some(Segment::Text) => (Segment::Text, 5),
            Some(Segment::DataConst) => (Segment::DataConst, 4),
            Some(Segment::Data) | None => (Segment::Data, 3),
        },
        Class::Rodata => (Segment::Text, 6),
        Class::Synthetic(_) => (Segment::Text, 7),
        Class::EhFrame => (Segment::Text, 8),
        Class::Got => (Segment::DataConst, 0),
        Class::InitArray => (Segment::DataConst, 1),
        Class::FiniArray => (Segment::DataConst, 2),
        Class::RelRo => (Segment::DataConst, 3),
        Class::Table => (Segment::Data, 0),
        Class::Data => (Segment::Data, 1),
        Class::PatchData => (Segment::Data, 2),
        Class::ThreadVars => (Segment::Data, 4),
        Class::Tdata => (Segment::Data, 5),
        Class::Tbss => (Segment::Data, 6),
        Class::Bss => (Segment::Data, 7),
    }
}

fn split(name: &str) -> (&str, &str) {
    name.split_once(',').unwrap_or((name, ""))
}

fn zeroed(class: &Class) -> bool {
    matches!(class, Class::Tbss | Class::Bss)
}

/// The segment, name and flags of the section a region is in the file.
fn section_of(class: &Class) -> (Segment, String, u32) {
    let code = macho::S_REGULAR | macho::S_ATTR_PURE_INSTRUCTIONS | macho::S_ATTR_SOME_INSTRUCTIONS;
    let (segment, _) = place(class);
    let (name, flags) = match class {
        Class::Text => ("__text", code),
        Class::Patch => ("__blaze_patch", code),
        Class::Stubs => ("__blaze_stubs", code),
        Class::Plt => ("__stubs", code),
        Class::Veneers => ("__blaze_veneers", code),
        Class::Rodata => ("__const", macho::S_REGULAR),
        Class::Synthetic(name) => (*name, macho::S_REGULAR),
        Class::EhFrame => (
            "__eh_frame",
            macho::S_COALESCED
                | macho::S_ATTR_NO_TOC
                | macho::S_ATTR_STRIP_STATIC_SYMS
                | macho::S_ATTR_LIVE_SUPPORT,
        ),
        Class::InitArray => ("__mod_init_func", macho::S_MOD_INIT_FUNC_POINTERS),
        Class::FiniArray => ("__mod_term_func", macho::S_MOD_TERM_FUNC_POINTERS),
        Class::Got => ("__got", macho::S_NON_LAZY_SYMBOL_POINTERS),
        Class::RelRo => ("__const", macho::S_REGULAR),
        Class::Table => ("__blaze_table", macho::S_REGULAR),
        Class::Data => ("__data", macho::S_REGULAR),
        Class::PatchData => ("__blaze_pdata", macho::S_REGULAR),
        Class::ThreadVars => ("__thread_vars", macho::S_THREAD_LOCAL_VARIABLES),
        Class::Tdata => ("__thread_data", macho::S_THREAD_LOCAL_REGULAR),
        Class::Tbss => ("__thread_bss", macho::S_THREAD_LOCAL_ZEROFILL),
        Class::Bss => ("__bss", macho::S_ZEROFILL),
        Class::Named { name, .. } => {
            let (seg, sect) = split(name);
            let flags = match sect {
                "__thread_init" => macho::S_THREAD_LOCAL_INIT_FUNCTION_POINTERS,
                "__cstring" => macho::S_CSTRING_LITERALS,
                _ => macho::S_REGULAR,
            };
            let sect = match (seg, sect) {
                ("__LD", "__compact_unwind") => "__blaze_unwind",
                _ => sect,
            };
            return (segment, sect.to_owned(), flags);
        }
    };
    (segment, name.to_owned(), flags)
}

/// Where a Mach-O section goes, by its segment, name and flags (`S_*` and `S_ATTR_*`): the
/// class and rank of what is loaded, or what else it is. The same name always gives the same
/// class, so `section$start$SEG$SECT` finds what an object or a `#[link_section]` put there.
pub(super) fn classify(segment: &str, section: &str, flags: u32) -> Result<SectionKind, String> {
    let alloc = |class| Ok(SectionKind::Alloc(class, u32::MAX));
    let named = |segment: &str, section: &str| Class::Named {
        name: format!("{segment},{section}"),
        write: segment != "__TEXT",
        exec: false,
    };
    // Before the debug check: LLVM marks `__compact_unwind` `S_ATTR_DEBUG`.
    match (segment, section) {
        ("__LD", "__compact_unwind") => return Ok(SectionKind::Unwind(compact_unwind())),
        ("__TEXT", "__eh_frame") => return Ok(SectionKind::EhFrame),
        _ => {}
    }
    if segment == "__DWARF" || segment == "__LLVM" || flags & macho::S_ATTR_DEBUG != 0 {
        return Ok(SectionKind::Dropped);
    }
    if Segment::of(segment).is_none() || segment == "__LD" {
        return Err(format!(
            "the section `{segment},{section}` is in a segment blaze's Mach-O writer doesn't lay \
             out (it has `__TEXT`, `__DATA_CONST` and `__DATA`)"
        ));
    }
    let text = segment == "__TEXT";
    match flags & macho::SECTION_TYPE {
        macho::S_MOD_INIT_FUNC_POINTERS => return alloc(Class::InitArray),
        macho::S_MOD_TERM_FUNC_POINTERS => return alloc(Class::FiniArray),
        macho::S_THREAD_LOCAL_REGULAR => return alloc(Class::Tdata),
        macho::S_THREAD_LOCAL_ZEROFILL => return alloc(Class::Tbss),
        macho::S_THREAD_LOCAL_VARIABLES => return alloc(Class::ThreadVars),
        macho::S_THREAD_LOCAL_INIT_FUNCTION_POINTERS => {
            return alloc(named("__DATA", "__thread_init"));
        }
        macho::S_ZEROFILL | macho::S_GB_ZEROFILL if !text => return alloc(Class::Bss),
        macho::S_CSTRING_LITERALS
        | macho::S_4BYTE_LITERALS
        | macho::S_8BYTE_LITERALS
        | macho::S_16BYTE_LITERALS
            if text =>
        {
            return alloc(Class::Rodata);
        }
        macho::S_NON_LAZY_SYMBOL_POINTERS
        | macho::S_LAZY_SYMBOL_POINTERS
        | macho::S_LAZY_DYLIB_SYMBOL_POINTERS
        | macho::S_SYMBOL_STUBS => {
            return Err(format!(
                "the section `{segment},{section}` is of symbol stubs or pointers, which only a \
                 linker makes"
            ));
        }
        macho::S_DTRACE_DOF => {
            return Err(format!("the section `{segment},{section}` holds DTrace probes (DOF)"));
        }
        _ => {}
    }
    match (segment, section) {
        ("__TEXT", "__text" | "__textcoal_nt" | "__StaticInit") => alloc(Class::Text),
        (
            "__TEXT",
            "__const" | "__cstring" | "__literal4" | "__literal8" | "__literal16" | "__ustring"
            | "__gcc_except_tab" | "__const_coal",
        ) => alloc(Class::Rodata),
        (_, "__mod_init_func") => alloc(Class::InitArray),
        (_, "__mod_term_func") => alloc(Class::FiniArray),
        ("__DATA", "__thread_data") => alloc(Class::Tdata),
        ("__DATA", "__thread_bss") => alloc(Class::Tbss),
        ("__DATA", "__thread_vars") => alloc(Class::ThreadVars),
        ("__DATA", "__thread_init") => alloc(named("__DATA", "__thread_init")),
        ("__DATA", "__data" | "__datacoal_nt") => alloc(Class::Data),
        ("__DATA" | "__DATA_CONST", "__const" | "__const_coal") => alloc(Class::RelRo),
        ("__DATA", "__bss" | "__common") => alloc(Class::Bss),
        _ => alloc(named(segment, section)),
    }
}

/// `S_*` of a section type as an assembler's `.section` and rustc's `#[link_section]` name it.
fn section_type(name: &str) -> Option<u32> {
    Some(match name {
        "regular" => macho::S_REGULAR,
        "zerofill" => macho::S_ZEROFILL,
        "cstring_literals" => macho::S_CSTRING_LITERALS,
        "4byte_literals" => macho::S_4BYTE_LITERALS,
        "8byte_literals" => macho::S_8BYTE_LITERALS,
        "16byte_literals" => macho::S_16BYTE_LITERALS,
        "literal_pointers" => macho::S_LITERAL_POINTERS,
        "non_lazy_symbol_pointers" => macho::S_NON_LAZY_SYMBOL_POINTERS,
        "lazy_symbol_pointers" => macho::S_LAZY_SYMBOL_POINTERS,
        "symbol_stubs" => macho::S_SYMBOL_STUBS,
        "mod_init_funcs" => macho::S_MOD_INIT_FUNC_POINTERS,
        "mod_term_funcs" => macho::S_MOD_TERM_FUNC_POINTERS,
        "coalesced" => macho::S_COALESCED,
        "interposing" => macho::S_INTERPOSING,
        "thread_local_regular" => macho::S_THREAD_LOCAL_REGULAR,
        "thread_local_zerofill" => macho::S_THREAD_LOCAL_ZEROFILL,
        "thread_local_variables" => macho::S_THREAD_LOCAL_VARIABLES,
        "thread_local_variable_pointers" => macho::S_THREAD_LOCAL_VARIABLE_POINTERS,
        "thread_local_init_function_pointers" => macho::S_THREAD_LOCAL_INIT_FUNCTION_POINTERS,
        _ => return None,
    })
}

fn section_attribute(name: &str) -> Option<u32> {
    Some(match name {
        "pure_instructions" => macho::S_ATTR_PURE_INSTRUCTIONS,
        "no_toc" => macho::S_ATTR_NO_TOC,
        "strip_static_syms" => macho::S_ATTR_STRIP_STATIC_SYMS,
        "no_dead_strip" => macho::S_ATTR_NO_DEAD_STRIP,
        "live_support" => macho::S_ATTR_LIVE_SUPPORT,
        "self_modifying_code" => macho::S_ATTR_SELF_MODIFYING_CODE,
        "debug" => macho::S_ATTR_DEBUG,
        _ => return None,
    })
}

impl MachO {
    fn shared(&self) -> bool {
        matches!(self.output.kind, OutputKind::Shared { .. })
    }

    fn base(&self) -> u64 {
        if self.shared() { 0 } else { BASE }
    }

    /// The class of the section `segment,section`, when one is loaded.
    fn class_by_name(segment: &str, section: &str) -> Option<Class> {
        match classify(segment, section, macho::S_REGULAR).ok()? {
            SectionKind::Alloc(class, _) | SectionKind::Unwind(class) => Some(class),
            SectionKind::EhFrame => Some(Class::EhFrame),
            SectionKind::Dropped => None,
        }
    }

    /// The architecture's slice of a universal file and where it starts, or the file.
    fn thin<'a>(&self, name: &str, bytes: &'a [u8]) -> Result<(usize, &'a [u8]), String> {
        use object::read::macho::{FatArch, MachOFatFile32, MachOFatFile64};
        let magic = bytes.get(..4).map(|it| u32::from_be_bytes([it[0], it[1], it[2], it[3]]));
        let slices: Vec<(u32, u64, u64)> = match magic {
            Some(macho::FAT_MAGIC) => MachOFatFile32::parse(bytes)
                .map_err(|it| format!("{name}: {it}"))?
                .arches()
                .iter()
                .map(|it| (it.cputype(), it.offset().into(), it.size().into()))
                .collect(),
            Some(macho::FAT_MAGIC_64) => MachOFatFile64::parse(bytes)
                .map_err(|it| format!("{name}: {it}"))?
                .arches()
                .iter()
                .map(|it| (it.cputype(), it.offset(), it.size()))
                .collect(),
            _ => return Ok((0, bytes)),
        };
        let Some(&(_, offset, size)) = slices.iter().find(|it| it.0 == self.cpu) else {
            let has: Vec<String> = slices.iter().map(|it| format!("{:#x}", it.0)).collect();
            return Err(format!(
                "{name} is a universal file without a slice for CPU type {:#x} (it has {})",
                self.cpu,
                has.join(", ")
            ));
        };
        let (start, end) = (offset as usize, offset.saturating_add(size) as usize);
        let slice = bytes
            .get(start..end)
            .ok_or_else(|| format!("{name}: a universal file's slice past its end"))?;
        Ok((start, slice))
    }
}

impl Writer for MachO {
    fn format(&self) -> Format {
        Format::MachO
    }

    fn arch(&self) -> Arch {
        self.output.arch
    }

    fn plan(&self) -> Plan {
        Plan {
            base: self.base(),
            headers: HEADERS,
            page: self.page,
            indexed_code: false,
            got_by_library: false,
            tls_descriptors: true,
        }
    }

    fn order(&self, classes: &mut Vec<Class>) {
        classes.sort_by(|a, b| place(a).cmp(&place(b)).then_with(|| a.cmp(b)));
    }

    fn prot(&self, class: &Class) -> Prot {
        place(class).0.prot()
    }

    fn section(
        &self,
        name: &str,
        write: bool,
        exec: bool,
        tls: bool,
    ) -> Result<(Class, u32), String> {
        let _ = (write, exec);
        let mut parts = name.split(',').map(str::trim);
        let segment = parts.next().unwrap_or_default();
        let shape = || {
            format!(
                "`{name}` is not a Mach-O section: `SEGMENT,section[,type[,attributes]]`, each \
                 name of 1 to 16 bytes"
            )
        };
        let Some(section) = parts.next() else { return Err(shape()) };
        if [segment, section].iter().any(|it| it.is_empty() || it.len() > 16) {
            return Err(shape());
        }
        let mut flags = match parts.next() {
            None => macho::S_REGULAR,
            Some(kind) => {
                section_type(kind).ok_or_else(|| format!("`{name}`: no section type `{kind}`"))?
            }
        };
        if let Some(attributes) = parts.next() {
            for attribute in attributes.split('+').map(str::trim) {
                flags |= section_attribute(attribute)
                    .ok_or_else(|| format!("`{name}`: no section attribute `{attribute}`"))?;
            }
        }
        // A stub size, after the attributes, only `symbol_stubs` has, which is refused below.
        if tls {
            let zero = matches!(flags & macho::SECTION_TYPE, macho::S_THREAD_LOCAL_ZEROFILL)
                || section == "__thread_bss";
            return Ok((if zero { Class::Tbss } else { Class::Tdata }, u32::MAX));
        }
        match classify(segment, section, flags)? {
            SectionKind::Alloc(class, rank) => Ok((class, rank)),
            _ => Err(format!("`{name}` is not a section a static can be placed in")),
        }
    }

    fn special(&self, name: &str) -> Option<Special> {
        match name {
            "_mh_execute_header" if !self.shared() => return Some(Special::ImageBase),
            "_mh_dylib_header" if self.shared() => return Some(Special::ImageBase),
            "__dso_handle" => return Some(Special::ImageBase),
            "__blaze_live" => return Some(Special::LiveDescriptor),
            _ => {}
        }
        let name = name.strip_prefix('\x01').unwrap_or(name);
        for (prefix, end) in [("section$start$", false), ("section$end$", true)] {
            let Some((segment, section)) = name.strip_prefix(prefix).and_then(|it| it.split_once('$'))
            else {
                continue;
            };
            return match Self::class_by_name(segment, section)? {
                Class::Named { name, .. } => Some(Special::Section { name, end }),
                class if end => Some(Special::Stop(class)),
                class => Some(Special::Start(class)),
            };
        }
        let at: fn(&[Region]) -> u64 = match (name, self.page) {
            ("segment$start$__TEXT", _) => return Some(Special::ImageBase),
            ("segment$end$__TEXT", 0x4000) => segment_bound::<0, true, 0x4000>,
            ("segment$end$__TEXT", _) => segment_bound::<0, true, 0x1000>,
            ("segment$start$__DATA_CONST", 0x4000) => segment_bound::<1, false, 0x4000>,
            ("segment$start$__DATA_CONST", _) => segment_bound::<1, false, 0x1000>,
            ("segment$end$__DATA_CONST", 0x4000) => segment_bound::<1, true, 0x4000>,
            ("segment$end$__DATA_CONST", _) => segment_bound::<1, true, 0x1000>,
            ("segment$start$__DATA", 0x4000) => segment_bound::<2, false, 0x4000>,
            ("segment$start$__DATA", _) => segment_bound::<2, false, 0x1000>,
            ("segment$end$__DATA", 0x4000) => segment_bound::<2, true, 0x4000>,
            ("segment$end$__DATA", _) => segment_bound::<2, true, 0x1000>,
            _ => return None,
        };
        Some(Special::Placed(at))
    }

    fn read(&self, name: &str, bytes: &[u8], kind: &NativeKind) -> Result<NativeFile, String> {
        let (at, bytes) = self.thin(name, bytes)?;
        let magic = bytes.get(..4).map(|it| u32::from_le_bytes([it[0], it[1], it[2], it[3]]));
        match kind {
            NativeKind::Object => {
                let mut object = input::read(name, bytes, self.output.arch)?;
                // The reader's ranges are of the slice it read: of this file, past a universal
                // header.
                if at != 0 {
                    for section in &mut object.sections {
                        if let Data::Input(range) = &mut section.data {
                            *range = range.start + at as u64..range.end + at as u64;
                        }
                    }
                }
                Ok(NativeFile::Object(object))
            }
            NativeKind::Archive { .. } => {
                let mut archive = archive::read(name, bytes)?;
                for member in &mut archive.members {
                    member.bytes = member.bytes.start + at..member.bytes.end + at;
                }
                Ok(NativeFile::Archive(archive))
            }
            NativeKind::Shared { .. } if magic == Some(macho::MH_MAGIC_64) => {
                dylib::read(name, bytes, self.output.arch).map(NativeFile::Shared)
            }
            NativeKind::Shared { .. } => {
                let text = std::str::from_utf8(bytes).map_err(|_| {
                    format!("{name} is neither a Mach-O dynamic library nor a `.tbd` stub")
                })?;
                tbd::read(name, text, self.output.arch, self.platform.platform)
                    .map(NativeFile::Shared)
            }
        }
    }

    fn synthetic(&self, counts: &Counts) -> Vec<(Class, u64)> {
        vec![(Class::Synthetic(UNWIND_INFO), unwind::bound(counts.functions, counts.fdes, counts.unwind))]
    }

    fn apply(
        &self,
        r_type: u32,
        bytes: &mut [u8],
        p: u64,
        s: u64,
        addend: i64,
    ) -> Result<(), String> {
        let _ = (bytes, s, addend);
        Err(format!(
            "a Mach-O relocation of type {r_type} at {p:#x}: every Mach-O relocation is read into \
             one the core applies"
        ))
    }

    fn synthesize(&self, placed: &Placed<'_>) -> Result<Vec<(Class, Vec<u8>)>, LinkError> {
        Ok(vec![(Class::Synthetic(UNWIND_INFO), unwind::build(placed, self.output.arch)?)])
    }

    fn offset(&self, placed: &Placed<'_>, region: &Region) -> Option<u64> {
        (!zeroed(&region.class)).then(|| region.addr - placed.plan.base)
    }

    fn file(&self, placed: &Placed<'_>) -> Result<File, LinkError> {
        self.image(placed)
    }
}

// The readers view an input in place wherever it starts (an archive's member starts where
// its header and name end, at any byte): the workspace builds `object` with `unaligned`,
// which makes every structure it reads byte-aligned.
const _: () = {
    use std::mem::align_of;
    type E = object::LittleEndian;
    assert!(align_of::<macho::FatHeader>() == 1 && align_of::<macho::FatArch64>() == 1);
    assert!(align_of::<macho::MachHeader64<E>>() == 1 && align_of::<macho::LoadCommand<E>>() == 1);
    assert!(align_of::<macho::SegmentCommand64<E>>() == 1 && align_of::<macho::Section64<E>>() == 1);
    assert!(align_of::<macho::SymtabCommand<E>>() == 1 && align_of::<macho::Nlist64<E>>() == 1);
    assert!(align_of::<macho::Relocation<E>>() == 1 && align_of::<macho::DylibCommand<E>>() == 1);
    assert!(align_of::<macho::DyldInfoCommand<E>>() == 1);
    assert!(align_of::<macho::LinkeditDataCommand<E>>() == 1);
};

/// A segment as its load command says it.
struct SegmentOut {
    name: &'static str,
    vmaddr: u64,
    vmsize: u64,
    fileoff: u64,
    filesize: u64,
    prot: u32,
    flags: u32,
    sections: Vec<SectionOut>,
}

struct SectionOut {
    name: String,
    segment: &'static str,
    addr: u64,
    size: u64,
    offset: u32,
    align: u32,
    flags: u32,
    /// `__got`: its first entry in the indirect symbol table.
    indirect: u32,
}

/// A little-endian writer of load commands and tables.
#[derive(Default)]
struct Out(Vec<u8>);

impl Out {
    fn u8(&mut self, value: u8) {
        self.0.push(value);
    }
    fn u16(&mut self, value: u16) {
        self.0.extend_from_slice(&value.to_le_bytes());
    }
    fn u32(&mut self, value: u32) {
        self.0.extend_from_slice(&value.to_le_bytes());
    }
    fn u64(&mut self, value: u64) {
        self.0.extend_from_slice(&value.to_le_bytes());
    }
    fn name16(&mut self, name: &str) {
        let mut field = [0u8; 16];
        field[..name.len()].copy_from_slice(name.as_bytes());
        self.0.extend_from_slice(&field);
    }
    fn align(&mut self, to: usize) {
        self.0.resize(self.0.len().next_multiple_of(to), 0);
    }
}

fn page_up(value: u64, page: u64) -> u64 {
    value.next_multiple_of(page)
}

/// `segment$start$SEG` (`END` false) or `segment$end$SEG` of the segment `SEGMENT` in
/// [`Segment`] order: where its first region starts, or the page after its last region's room.
/// A segment with no regions is empty, both where it would start.
fn segment_bound<const SEGMENT: u8, const END: bool, const PAGE: u64>(regions: &[Region]) -> u64 {
    let order = |it: &&Region| place(&it.class).0 as u8;
    let end_of = |it: &Region| page_up(it.addr + it.capacity, PAGE);
    let mine = || regions.iter().filter(|it| order(it) == SEGMENT);
    match (mine().map(|it| it.addr).min(), mine().map(end_of).max()) {
        (Some(start), Some(end)) => if END { end } else { start },
        _ => (regions.iter().filter(|it| order(it) < SEGMENT).map(end_of).max())
            .unwrap_or(BASE + HEADERS),
    }
}

fn u32_of(value: u64, what: &str) -> Result<u32, LinkError> {
    u32::try_from(value).map_err(|_| error(format!("{what} ({value:#x}) doesn't fit 32 bits")))
}

/// `__debug_line` for `.debug_line`, at most 16 bytes, as Apple's tools name them.
fn dwarf_name(name: &str) -> String {
    let mut out = format!("__{}", name.trim_start_matches('.'));
    out.truncate(16);
    out
}

/// What the symbol table and the indirect symbol table say.
struct Symbols {
    nlist: Vec<u8>,
    strings: Vec<u8>,
    count: u32,
    locals: u32,
    defined: u32,
    undefined: u32,
    indirect: Vec<u8>,
    indirect_count: u32,
    /// Each import's Mach-O name in `strings` (offset, length), by import.
    import_names: Vec<(u32, u32)>,
    /// What the export trie holds: the defined symbols in name order, each its name in
    /// `strings`, its `EXPORT_SYMBOL_FLAGS_*` and its offset from the image's start.
    exports: Vec<((u32, u32), u64, u64)>,
}

impl MachO {
    fn image(&self, placed: &Placed<'_>) -> Result<File, LinkError> {
        let base = placed.plan.base;
        let page = self.page;
        let shared = self.shared();

        // The segments of the address space, and the sections of the regions in them.
        let mut segments: Vec<SegmentOut> = Vec::new();
        if !shared {
            segments.push(SegmentOut {
                name: "__PAGEZERO",
                vmaddr: 0,
                vmsize: base,
                fileoff: 0,
                filesize: 0,
                prot: 0,
                flags: 0,
                sections: Vec::new(),
            });
        }
        let mut has_tlv = false;
        for segment in [Segment::Text, Segment::DataConst, Segment::Data] {
            let regions: Vec<(usize, &Region)> = (placed.regions.iter().enumerate())
                .filter(|(_, it)| place(&it.class).0 == segment)
                .collect();
            if regions.is_empty() && segment != Segment::Text {
                continue;
            }
            let vmaddr = match (segment, regions.first()) {
                (Segment::Text, _) => base,
                (_, Some((_, first))) => first.addr,
                (_, None) => unreachable!("only `__TEXT` is kept without regions"),
            };
            if vmaddr % page != 0 {
                return Err(error(format!(
                    "the segment {} starts at {vmaddr:#x}, which is not at a page",
                    segment.name()
                )));
            }
            let start = match segment {
                Segment::Text => base + placed.plan.headers,
                _ => vmaddr,
            };
            let (mut vmend, mut filend) = (start, start);
            let mut zero_seen = None;
            let mut sections = Vec::new();
            for &(index, region) in &regions {
                let end = region.addr + region.capacity;
                if region.addr < start {
                    return Err(error(format!(
                        "the region of {:?} at {:#x} is before its segment {} (at {start:#x})",
                        region.class,
                        region.addr,
                        segment.name()
                    )));
                }
                if let Some(last) = segments.last().filter(|_| segment != Segment::Text)
                    && region.addr < last.vmaddr + last.vmsize
                {
                    return Err(error(format!(
                        "the region of {:?} at {:#x} overlaps the segment {}",
                        region.class, region.addr, last.name
                    )));
                }
                if region.size > region.capacity
                    || placed.bytes(index).len() as u64 > region.capacity
                {
                    return Err(error(format!(
                        "the region of {:?} at {:#x} holds more than its capacity",
                        region.class, region.addr
                    )));
                }
                vmend = vmend.max(end);
                if zeroed(&region.class) {
                    zero_seen.get_or_insert(region.addr);
                    if !placed.bytes(index).is_empty() {
                        return Err(error(format!(
                            "the zeroed region of {:?} at {:#x} has bytes",
                            region.class, region.addr
                        )));
                    }
                } else {
                    if let Some(zero) = zero_seen {
                        return Err(error(format!(
                            "the region of {:?} at {:#x} is behind zeroed data at {zero:#x}: \
                             the file has no bytes there",
                            region.class, region.addr
                        )));
                    }
                    filend = filend.max(end);
                }
                let offset = match zeroed(&region.class) {
                    true => 0,
                    false => u32_of(region.addr - base, "a section's file offset")?,
                };
                let align = region.align.max(1).trailing_zeros();
                let (_, name, flags) = section_of(&region.class);
                has_tlv |= flags == macho::S_THREAD_LOCAL_VARIABLES && region.size != 0;
                sections.push(SectionOut {
                    name,
                    segment: segment.name(),
                    addr: region.addr,
                    size: region.size,
                    offset,
                    align,
                    flags,
                    indirect: 0,
                });
            }
            let vmend = page_up(vmend, page);
            let filesize = (page_up(filend, page) - vmaddr).min(vmend - vmaddr);
            let prot = match segment.prot() {
                Prot { write: true, .. } => macho::VM_PROT_READ | macho::VM_PROT_WRITE,
                Prot { exec: true, .. } => macho::VM_PROT_READ | macho::VM_PROT_EXECUTE,
                // `__DATA_CONST` is written by the loader, which makes it read-only after.
                _ => macho::VM_PROT_READ | macho::VM_PROT_WRITE,
            };
            segments.push(SegmentOut {
                name: segment.name(),
                vmaddr,
                vmsize: vmend - vmaddr,
                fileoff: vmaddr - base,
                filesize,
                prot,
                flags: if segment == Segment::DataConst { macho::SG_READ_ONLY } else { 0 },
                sections,
            });
        }
        let last = segments.last().expect("`__TEXT` is always there");
        let mut vm_next = last.vmaddr + last.vmsize;
        let mut file_next = (segments.iter())
            .map(|it| page_up(it.fileoff + it.filesize, page))
            .max()
            .unwrap_or(0);

        // The debug info, in a segment no one runs.
        let mut dwarf = Vec::new();
        let debug: Vec<&(&'static str, Vec<u8>)> =
            placed.debug.iter().filter(|it| !it.1.is_empty()).collect();
        if !debug.is_empty() {
            let (vmaddr, fileoff) = (vm_next, file_next);
            let mut sections = Vec::new();
            for (name, bytes) in debug {
                sections.push(SectionOut {
                    name: dwarf_name(name),
                    segment: "__DWARF",
                    addr: vmaddr + dwarf.len() as u64,
                    size: bytes.len() as u64,
                    offset: u32_of(fileoff + dwarf.len() as u64, "a debug section's offset")?,
                    align: 0,
                    flags: macho::S_REGULAR | macho::S_ATTR_DEBUG,
                    indirect: 0,
                });
                dwarf.extend_from_slice(bytes);
            }
            let filesize = dwarf.len() as u64;
            segments.push(SegmentOut {
                name: "__DWARF",
                vmaddr,
                vmsize: page_up(filesize, page),
                fileoff,
                filesize,
                prot: macho::VM_PROT_READ,
                flags: 0,
                sections,
            });
            vm_next = page_up(vmaddr + filesize, page);
            file_next = page_up(fileoff + filesize, page);
        }

        // Which libraries are loaded, and each one's ordinal.
        let mut used = vec![(0usize, 0usize); placed.libraries.len()];
        for import in placed.imports {
            if let Some(library) = import.library {
                let it = used.get_mut(library as usize).ok_or_else(|| {
                    error(format!("the import {} is of no library ({library})", import.name))
                })?;
                it.0 += 1;
                it.1 += usize::from(import.weak);
            }
        }
        let mut ordinals = vec![0u32; placed.libraries.len()];
        let mut loaded = Vec::new();
        for (nth, library) in placed.libraries.iter().enumerate() {
            if library.as_needed && used[nth].0 == 0 {
                continue;
            }
            loaded.push((library, used[nth].0 != 0 && used[nth].0 == used[nth].1));
            ordinals[nth] = loaded.len() as u32;
        }
        if loaded.len() > 253 {
            return Err(error(format!(
                "{} libraries: a two-level namespace image names at most 253",
                loaded.len()
            )));
        }
        let ordinal = |import: u32| -> Result<i64, LinkError> {
            let import = placed
                .imports
                .get(import as usize)
                .ok_or_else(|| error(format!("a bind of no import ({import})")))?;
            Ok(match import.library {
                Some(library) => i64::from(ordinals[library as usize]),
                None => i64::from(macho::BIND_SPECIAL_DYLIB_FLAT_LOOKUP),
            })
        };

        // What the loader fixes up.
        let spans: Vec<linkedit::Span> = (segments.iter())
            .filter(|it| it.name != "__DWARF")
            .map(|it| linkedit::Span {
                name: it.name,
                start: it.vmaddr,
                end: it.vmaddr + it.vmsize,
                writable: it.prot & macho::VM_PROT_WRITE != 0,
            })
            .collect();
        let rebase = linkedit::rebases(placed.rebase, &spans).map_err(error)?;

        // The symbol table first: its strings are the names the binds and the trie use.
        let mut sections: Vec<&mut SectionOut> =
            segments.iter_mut().flat_map(|it| it.sections.iter_mut()).collect();
        let symbols = self.symbols(placed, &mut sections, &ordinals)?;
        let name_at = |(at, len): (u32, u32)| &symbols.strings[at as usize..(at + len) as usize];

        let mut binds = Vec::with_capacity(placed.binds.len() + placed.got.len());
        for bind in placed.binds {
            let import = placed.imports.get(bind.import as usize);
            binds.push(linkedit::Bind {
                addr: bind.addr,
                ordinal: ordinal(bind.import)?,
                name: name_at(symbols.import_names[bind.import as usize]),
                weak: import.is_some_and(|it| it.weak),
                addend: bind.addend,
            });
        }
        for cell in placed.got {
            match &cell.holds {
                Holds::Local(_) => {}
                &Holds::Import(import) => binds.push(linkedit::Bind {
                    addr: cell.addr,
                    ordinal: ordinal(import)?,
                    name: name_at(symbols.import_names[import as usize]),
                    weak: placed.imports[import as usize].weak,
                    addend: 0,
                }),
                other => {
                    return Err(error(format!(
                        "a GOT cell at {:#x} holding {other:?}, which is ELF's and not Mach-O's",
                        cell.addr
                    )));
                }
            }
        }
        let bind = linkedit::binds(&mut binds, &spans).map_err(error)?;

        // What the image gives other images: what the symbol table defines, in its order.
        let mut exports: Vec<(&[u8], u64, u64)> = (symbols.exports.iter())
            .map(|&(name, flags, offset)| (name_at(name), flags, offset))
            .collect();
        let trie = linkedit::trie(&mut exports).map_err(error)?;

        let starts: Vec<u64> = placed.functions.iter().map(|it| it.addr).collect();
        let function_starts = linkedit::function_starts(base, &starts);

        // `__LINKEDIT`, laid out as ld64 does: the signature last, at 16 bytes.
        let linkedit_at = file_next;
        let tables = [&rebase, &bind, &trie, &function_starts, &symbols.nlist, &symbols.indirect];
        let mut linkedit = Out(Vec::with_capacity(
            tables.iter().map(|it| it.len()).sum::<usize>() + symbols.strings.len() + 64,
        ));
        let table = |bytes: &[u8], linkedit: &mut Out| -> Result<(u32, u32), LinkError> {
            if bytes.is_empty() {
                return Ok((0, 0));
            }
            let at = u32_of(linkedit_at + linkedit.0.len() as u64, "`__LINKEDIT`'s offsets")?;
            linkedit.0.extend_from_slice(bytes);
            linkedit.align(8);
            Ok((at, bytes.len() as u32))
        };
        let rebase_at = table(&rebase, &mut linkedit)?;
        let bind_at = table(&bind, &mut linkedit)?;
        let trie_at = table(&trie, &mut linkedit)?;
        let starts_at = table(&function_starts, &mut linkedit)?;
        let in_code_at = (u32_of(linkedit_at + linkedit.0.len() as u64, "`__LINKEDIT`")?, 0);
        let nlist_at = table(&symbols.nlist, &mut linkedit)?;
        let indirect_at = table(&symbols.indirect, &mut linkedit)?;
        let strings_at = table(&symbols.strings, &mut linkedit)?;
        linkedit.align(16);
        let code_limit = linkedit_at + linkedit.0.len() as u64;
        let identifier = placed.name;
        if identifier.is_empty() || identifier.contains(['/', '\0']) {
            return Err(error(format!(
                "`{identifier}` is not a file name, which the code signature's identifier is"
            )));
        }
        let signature_len = sign::len(identifier, code_limit);
        let linkedit_size = linkedit.0.len() as u64 + signature_len;
        segments.push(SegmentOut {
            name: "__LINKEDIT",
            vmaddr: vm_next,
            vmsize: page_up(linkedit_size, page),
            fileoff: linkedit_at,
            filesize: linkedit_size,
            prot: macho::VM_PROT_READ,
            flags: 0,
            sections: Vec::new(),
        });

        // The load commands.
        let mut commands = Out::default();
        let mut count = 0u32;
        for segment in &segments {
            count += 1;
            commands.u32(macho::LC_SEGMENT_64);
            commands.u32((SEGMENT + SECTION * segment.sections.len()) as u32);
            commands.name16(segment.name);
            commands.u64(segment.vmaddr);
            commands.u64(segment.vmsize);
            commands.u64(segment.fileoff);
            commands.u64(segment.filesize);
            commands.u32(segment.prot);
            commands.u32(segment.prot);
            commands.u32(segment.sections.len() as u32);
            commands.u32(segment.flags);
            for section in &segment.sections {
                commands.name16(&section.name);
                commands.name16(section.segment);
                commands.u64(section.addr);
                commands.u64(section.size);
                commands.u32(section.offset);
                commands.u32(section.align);
                commands.u32(0);
                commands.u32(0);
                commands.u32(section.flags);
                commands.u32(section.indirect);
                commands.u32(0);
                commands.u32(0);
            }
        }
        count += 1;
        commands.u32(macho::LC_DYLD_INFO_ONLY);
        commands.u32(48);
        for (at, len) in [rebase_at, bind_at, (0, 0), (0, 0), trie_at] {
            commands.u32(at);
            commands.u32(len);
        }
        count += 1;
        commands.u32(macho::LC_SYMTAB);
        commands.u32(24);
        commands.u32(nlist_at.0);
        commands.u32(symbols.count);
        commands.u32(strings_at.0);
        commands.u32(strings_at.1);
        count += 1;
        commands.u32(macho::LC_DYSYMTAB);
        commands.u32(80);
        for field in [
            0,
            symbols.locals,
            symbols.locals,
            symbols.defined,
            symbols.locals + symbols.defined,
            symbols.undefined,
            0,
            0,
            0,
            0,
            0,
            0,
            indirect_at.0,
            symbols.indirect_count,
            0,
            0,
            0,
            0,
        ] {
            commands.u32(field);
        }
        match &self.output.kind {
            OutputKind::Executable { .. } => {
                count += 1;
                path_command(&mut commands, macho::LC_LOAD_DYLINKER, 12, "/usr/lib/dyld", &[]);
            }
            OutputKind::Shared { name, .. } => {
                count += 1;
                path_command(&mut commands, macho::LC_ID_DYLIB, 24, name, &[1, 0, 0]);
            }
        }
        count += 1;
        commands.u32(macho::LC_UUID);
        commands.u32(24);
        commands.0.extend_from_slice(&placed.id);
        count += 1;
        commands.u32(macho::LC_BUILD_VERSION);
        commands.u32(24);
        commands.u32(self.platform.platform);
        commands.u32(self.platform.minimum);
        commands.u32(self.platform.sdk);
        commands.u32(0);
        count += 1;
        commands.u32(macho::LC_SOURCE_VERSION);
        commands.u32(16);
        commands.u64(0);
        if !shared {
            let entry = placed.entry.ok_or_else(|| error("an executable without an entry"))?;
            let text = &segments[1];
            if !(text.vmaddr..text.vmaddr + text.vmsize).contains(&entry) {
                return Err(error(format!("the entry {entry:#x} is not in `__TEXT`")));
            }
            count += 1;
            commands.u32(macho::LC_MAIN);
            commands.u32(24);
            commands.u64(entry - base);
            commands.u64(self.output.stack.unwrap_or(0));
        }
        for (library, weak) in &loaded {
            count += 1;
            let command = if *weak { macho::LC_LOAD_WEAK_DYLIB } else { macho::LC_LOAD_DYLIB };
            path_command(&mut commands, command, 24, &library.name, &[2, 0, 0]);
        }
        for (command, (at, len)) in [
            (macho::LC_FUNCTION_STARTS, starts_at),
            (macho::LC_DATA_IN_CODE, in_code_at),
        ] {
            count += 1;
            commands.u32(command);
            commands.u32(16);
            commands.u32(at);
            commands.u32(len);
        }
        count += 1;
        commands.u32(macho::LC_CODE_SIGNATURE);
        commands.u32(16);
        commands.u32(u32_of(code_limit, "the code signature's offset")?);
        commands.u32(u32_of(signature_len, "the code signature's length")?);

        if (HEADER + commands.0.len()) as u64 > placed.plan.headers {
            return Err(error(format!(
                "the load commands take {} bytes, more than the {} the image keeps for them",
                HEADER + commands.0.len(),
                placed.plan.headers
            )));
        }
        let mut flags = macho::MH_NOUNDEFS | macho::MH_DYLDLINK | macho::MH_TWOLEVEL;
        flags |= match shared {
            true => macho::MH_NO_REEXPORTED_DYLIBS,
            false => macho::MH_PIE,
        };
        if has_tlv {
            flags |= macho::MH_HAS_TLV_DESCRIPTORS;
        }
        let mut headers = Out::default();
        headers.u32(macho::MH_MAGIC_64);
        headers.u32(self.cpu);
        headers.u32(self.subtype);
        headers.u32(if shared { macho::MH_DYLIB } else { macho::MH_EXECUTE });
        headers.u32(count);
        headers.u32(commands.0.len() as u32);
        headers.u32(flags);
        headers.u32(0);
        headers.0.extend_from_slice(&commands.0);

        // The file as the loader reads it, hashed where its bytes are.
        let dwarf_at = segments.iter().find(|it| it.name == "__DWARF").map(|it| it.fileoff);
        let mut pieces: Vec<(u64, &[u8])> = Vec::with_capacity(placed.regions.len() + 3);
        pieces.push((0, &headers.0[..]));
        for (index, region) in placed.regions.iter().enumerate() {
            if let Some(at) = self.offset(placed, region) {
                pieces.push((at, placed.bytes(index)));
            }
        }
        if let Some(at) = dwarf_at {
            pieces.push((at, &dwarf[..]));
        }
        pieces.push((linkedit_at, &linkedit.0[..]));
        let file = sign::View::new(code_limit, pieces).map_err(error)?;
        let text = &segments[usize::from(!shared)];
        let mut signature = Vec::with_capacity(signature_len as usize);
        {
            let mut cache = self.cache.as_ref().map(|it| match it.lock() {
                Ok(cache) => cache,
                // A panic while signing may have left a page without its hash: start over.
                Err(poisoned) => {
                    let mut cache = poisoned.into_inner();
                    *cache = sign::Cache::default();
                    cache
                }
            });
            let (at, len) = (text.fileoff, text.filesize);
            sign::signature(&file, identifier, at, len, !shared, cache.as_deref_mut(), &mut signature);
        }
        if signature.len() as u64 != signature_len {
            return Err(error(format!(
                "a code signature of {} bytes where {signature_len} were kept for it",
                signature.len()
            )));
        }

        // The signature in pieces of a page of the file, so an incremental link, which
        // changes few of its hashes, writes few of them.
        let mut parts = Vec::with_capacity(3 + signature.len().div_ceil(sign::PAGE));
        parts.push((0, headers.0));
        if let Some(at) = dwarf_at {
            parts.push((at, dwarf));
        }
        parts.push((linkedit_at, linkedit.0));
        let mut at = code_limit;
        for piece in signature.chunks(sign::PAGE) {
            parts.push((at, piece.to_vec()));
            at += piece.len() as u64;
        }
        Ok(File { len: linkedit_at + linkedit_size, parts })
    }

    /// The symbol table: the functions' names, then what the image exports, then what it
    /// imports, each with its library's ordinal; and the indirect symbols of the `__got`s.
    fn symbols(
        &self,
        placed: &Placed<'_>,
        sections: &mut [&mut SectionOut],
        ordinals: &[u32],
    ) -> Result<Symbols, LinkError> {
        let base = placed.plan.base;
        let mut ranges: Vec<(u64, u64, u8)> = Vec::new();
        for (nth, section) in sections.iter().enumerate() {
            if section.segment != "__DWARF" {
                let ordinal = u8::try_from(nth + 1)
                    .map_err(|_| error("more than 255 sections: symbols can't name them"))?;
                ranges.push((section.addr, section.addr + section.size, ordinal));
            }
        }
        radix::sort_copy_by_key(&mut ranges, |&(start, end, _)| radix::pack_64(start, end));
        let section_at = |addr: u64, what: &str| -> Result<u8, LinkError> {
            let at = ranges.partition_point(|it| it.0 <= addr);
            (ranges[..at].iter().rev())
                .find(|it| addr <= it.1)
                .map(|it| it.2)
                .ok_or_else(|| error(format!("the symbol {what} at {addr:#x} is in no section")))
        };

        let exported: rustc_hash::FxHashSet<&str> =
            placed.exports.iter().map(|it| it.name.as_str()).collect();
        let mut locals: Vec<(u64, &str)> = (placed.functions.iter())
            .filter_map(|it| Some((it.addr, it.name.as_deref()?)))
            .filter(|it| !exported.contains(it.1))
            .collect();
        if !locals.is_sorted() {
            locals.sort_unstable();
        }

        // The defined: what the image exports and, in an executable, its header.
        let mut defined: Vec<(&str, u64, u16, u64)> = Vec::with_capacity(placed.exports.len() + 1);
        for export in placed.exports {
            let flags = match export.kind {
                crate::model::SymbolKind::Tls => {
                    u64::from(macho::EXPORT_SYMBOL_FLAGS_KIND_THREAD_LOCAL)
                }
                _ => u64::from(macho::EXPORT_SYMBOL_FLAGS_KIND_REGULAR),
            };
            if export.addr < base {
                return Err(error(format!(
                    "the export {} at {:#x} is before the image",
                    export.name, export.addr
                )));
            }
            defined.push((export.name.as_str(), export.addr, 0, flags));
        }
        if !self.shared() {
            let flags = u64::from(macho::EXPORT_SYMBOL_FLAGS_KIND_REGULAR);
            defined.push(("_mh_execute_header", base, macho::REFERENCED_DYNAMICALLY, flags));
        }
        defined.sort_unstable_by(|a, b| {
            macho_order(a.0, b.0).then_with(|| (a.1, a.2, a.3).cmp(&(b.1, b.2, b.3)))
        });

        let mut undefined: Vec<(&str, u32)> =
            (placed.imports.iter().enumerate()).map(|(nth, it)| (it.name.as_str(), nth as u32)).collect();
        undefined.sort_unstable_by(|a, b| macho_order(a.0, b.0).then(a.1.cmp(&b.1)));

        // Every name once, as Mach-O spells it: the binds and the trie take theirs from here.
        let count = locals.len() + defined.len() + undefined.len();
        let names = (locals.iter().map(|it| it.1))
            .chain(defined.iter().map(|it| it.0))
            .chain(undefined.iter().map(|it| it.0))
            .map(|it| it.len() + 2)
            .sum::<usize>();
        let mut strings = Out(Vec::with_capacity(names + 16));
        strings.0.extend_from_slice(b" \0");
        let mut nlist = Out(Vec::with_capacity(count * NLIST));
        let mut entry = |name: &str, kind: u8, sect: u8, desc: u16, value: u64| -> (u32, u32) {
            let at = strings.0.len() as u32;
            let (prefix, rest) = spelled(name);
            nlist.u32(at);
            strings.0.extend_from_slice(prefix.as_bytes());
            strings.0.extend_from_slice(rest.as_bytes());
            strings.u8(0);
            nlist.u8(kind);
            nlist.u8(sect);
            nlist.u16(desc);
            nlist.u64(value);
            (at, (prefix.len() + rest.len()) as u32)
        };

        for &(addr, name) in &locals {
            entry(name, macho::N_SECT, section_at(addr, name)?, 0, addr);
        }
        let mut exports = Vec::with_capacity(defined.len());
        for &(name, addr, desc, flags) in &defined {
            let sect = section_at(addr, name)?;
            let at = entry(name, macho::N_SECT | macho::N_EXT, sect, desc, addr);
            exports.push((at, flags, addr - base));
        }
        let first_undefined = (locals.len() + defined.len()) as u32;
        let mut index_of = vec![0u32; placed.imports.len()];
        let mut import_names = vec![(0u32, 0u32); placed.imports.len()];
        for (nth, &(name, index)) in undefined.iter().enumerate() {
            let import = &placed.imports[index as usize];
            let library = match import.library {
                Some(library) => ordinals[library as usize] as u16,
                None => u16::from(macho::DYNAMIC_LOOKUP_ORDINAL),
            };
            let weak = if import.weak { macho::N_WEAK_REF } else { 0 };
            import_names[index as usize] = entry(name, macho::N_UNDF | macho::N_EXT, 0, library << 8 | weak, 0);
            index_of[index as usize] = first_undefined + nth as u32;
        }
        strings.align(8);

        // The GOT's cells by address, walked alongside each `__got` section's cells.
        let mut got: Vec<(u64, &Holds)> = placed.got.iter().map(|it| (it.addr, &it.holds)).collect();
        if !got.is_sorted_by_key(|it| it.0) {
            radix::sort_by_key(&mut got, |it| it.0);
        }
        let mut indirect = Out::default();
        let mut indirect_count = 0u32;
        for section in sections.iter_mut() {
            if section.flags != macho::S_NON_LAZY_SYMBOL_POINTERS {
                continue;
            }
            if section.size % 8 != 0 {
                return Err(error(format!(
                    "the `__got` at {:#x} is {} bytes, not of whole cells",
                    section.addr, section.size
                )));
            }
            section.indirect = indirect_count;
            indirect.0.reserve(section.size as usize / 2);
            let mut next = got.partition_point(|it| it.0 < section.addr);
            for nth in 0..section.size / 8 {
                let addr = section.addr + nth * 8;
                while got.get(next).is_some_and(|it| it.0 < addr) {
                    next += 1;
                }
                let cell = got.get(next).filter(|it| it.0 == addr).map(|it| it.1);
                indirect.u32(match cell {
                    Some(&Holds::Import(import)) => index_of[import as usize],
                    Some(Holds::Local(_)) => macho::INDIRECT_SYMBOL_LOCAL,
                    _ => macho::INDIRECT_SYMBOL_ABS,
                });
                indirect_count += 1;
            }
        }

        Ok(Symbols {
            count: count as u32,
            locals: locals.len() as u32,
            defined: defined.len() as u32,
            undefined: undefined.len() as u32,
            nlist: nlist.0,
            strings: strings.0,
            indirect: indirect.0,
            indirect_count,
            import_names,
            exports,
        })
    }
}

/// A load command of a path at `at`, after `fields`: `LC_LOAD_DYLINKER`, `LC_*_DYLIB`.
fn path_command(out: &mut Out, command: u32, at: u32, path: &str, fields: &[u32]) {
    let size = (at as usize + path.len() + 1).next_multiple_of(8);
    out.u32(command);
    out.u32(size as u32);
    out.u32(at);
    for field in fields {
        out.u32(*field);
    }
    let start = out.0.len();
    out.0.extend_from_slice(path.as_bytes());
    out.0.resize(start + size - at as usize, 0);
}
