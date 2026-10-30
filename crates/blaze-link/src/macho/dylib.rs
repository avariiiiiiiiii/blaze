//! Thin Mach-O dynamic libraries: the install name and the exports, from the export trie (or,
//! without one, the symbol table's external definitions).

use blaze_codegen::Arch;
use object::Endianness;
use object::macho;
use object::read::macho::{MachHeader as _, Nlist as _, Section as _, Segment as _, SymbolTable};
use rustc_hash::FxHashSet;

use crate::model::{Shared, SharedSymbol, SymbolKind};

type Header = macho::MachHeader64<Endianness>;

/// dyld's flag for an export whose address is a resolver dyld runs once at load (newer than
/// `object`'s constants).
const EXPORT_SYMBOL_FLAGS_STATIC_RESOLVER: u64 = 0x20;

/// A thin Mach-O dynamic library (`MH_DYLIB`, or an `MH_DYLIB_STUB`) for `arch`: its install
/// name (`LC_ID_DYLIB`) and what it exports.
pub(super) fn read(name: &str, bytes: &[u8], arch: Arch) -> Result<Shared, String> {
    read_dylib(bytes, arch).map_err(|what| format!("{name}: {what}"))
}

fn err(error: object::read::Error) -> String {
    error.to_string()
}

#[derive(Clone, Copy)]
struct Section {
    addr: u64,
    size: u64,
    flags: u32,
}

impl Section {
    fn contains(&self, addr: u64) -> bool {
        addr >= self.addr && addr - self.addr < self.size
    }

    fn code(&self) -> bool {
        self.flags & (macho::S_ATTR_PURE_INSTRUCTIONS | macho::S_ATTR_SOME_INSTRUCTIONS) != 0
    }
}

fn read_dylib(bytes: &[u8], arch: Arch) -> Result<Shared, String> {
    let magic = bytes
        .get(..4)
        .map(|it| u32::from_be_bytes([it[0], it[1], it[2], it[3]]))
        .ok_or("too short for a Mach-O header")?;
    match magic {
        macho::MH_MAGIC_64 | macho::MH_CIGAM_64 => {}
        macho::FAT_MAGIC | macho::FAT_CIGAM | macho::FAT_MAGIC_64 | macho::FAT_CIGAM_64 => {
            return Err("a universal (fat) file: only a thin slice is read here".to_owned());
        }
        macho::MH_MAGIC | macho::MH_CIGAM => return Err("a 32-bit Mach-O".to_owned()),
        _ => return Err(format!("not a Mach-O file (magic {magic:#010x})")),
    }
    let header = Header::parse(bytes, 0).map_err(err)?;
    let endian = header.endian().map_err(err)?;
    let want = match arch {
        Arch::Aarch64 => macho::CPU_TYPE_ARM64,
        Arch::X86_64 => macho::CPU_TYPE_X86_64,
        Arch::Wasm32 => return Err("a Mach-O library in a wasm32 link".to_owned()),
    };
    let cpu = header.cputype(endian);
    if cpu != want {
        return Err(format!("a Mach-O of CPU type {cpu:#x}, not {arch:?} ({want:#x})"));
    }
    let filetype = header.filetype(endian);
    if filetype != macho::MH_DYLIB && filetype != macho::MH_DYLIB_STUB {
        return Err(format!("not a dynamic library (Mach-O file type {filetype})"));
    }

    let mut install_name = None;
    let mut inputs = Vec::new();
    let mut trie: Option<(&'static str, u32, u32)> = None;
    let mut symtab = None;
    let mut sections = Vec::new();
    let mut base = None;
    let mut commands = header.load_commands(endian, bytes, 0).map_err(err)?;
    while let Some(command) = commands.next().map_err(err)? {
        match command.cmd() {
            macho::LC_ID_DYLIB | macho::LC_REEXPORT_DYLIB => {
                let dylib: &macho::DylibCommand<Endianness> = command.data().map_err(err)?;
                let path = command.string(endian, dylib.dylib.name).map_err(err)?;
                let path = std::str::from_utf8(path)
                    .map_err(|_| "a dylib path that is not UTF-8".to_owned())?
                    .to_owned();
                if command.cmd() == macho::LC_ID_DYLIB {
                    if install_name.replace(path).is_some() {
                        return Err("two LC_ID_DYLIB commands".to_owned());
                    }
                } else if !inputs.contains(&path) {
                    inputs.push(path);
                }
            }
            macho::LC_DYLD_INFO | macho::LC_DYLD_INFO_ONLY => {
                let info: &macho::DyldInfoCommand<Endianness> = command.data().map_err(err)?;
                let size = info.export_size.get(endian);
                if size != 0 {
                    set_trie(&mut trie, "LC_DYLD_INFO", info.export_off.get(endian), size)?;
                }
            }
            macho::LC_DYLD_EXPORTS_TRIE => {
                let data: &macho::LinkeditDataCommand<Endianness> = command.data().map_err(err)?;
                let size = data.datasize.get(endian);
                if size != 0 {
                    set_trie(&mut trie, "LC_DYLD_EXPORTS_TRIE", data.dataoff.get(endian), size)?;
                }
            }
            macho::LC_SYMTAB => symtab = command.symtab().map_err(err)?,
            macho::LC_SEGMENT_64 => {
                if let Some((segment, data)) = command.segment_64().map_err(err)? {
                    if segment.fileoff(endian) == 0 && segment.filesize(endian) != 0 {
                        base.get_or_insert(segment.vmaddr(endian));
                    }
                    for section in segment.sections(endian, data).map_err(err)? {
                        sections.push(Section {
                            addr: section.addr(endian),
                            size: section.size(endian),
                            flags: section.flags(endian),
                        });
                    }
                }
            }
            _ => {}
        }
    }
    let name = install_name.ok_or("a dynamic library without LC_ID_DYLIB")?;

    let symbols = match trie {
        Some((command, offset, size)) => {
            let data = usize::try_from(offset)
                .ok()
                .zip(usize::try_from(size).ok())
                .and_then(|(offset, size)| bytes.get(offset..offset.checked_add(size)?))
                .ok_or_else(|| {
                    format!("{command}'s export trie ({size} bytes at {offset:#x}) is outside the file")
                })?;
            let base = base.ok_or("an export trie, but no segment maps the Mach-O header")?;
            trie_exports(data, base, &sections).map_err(|what| format!("{command}'s export trie: {what}"))?
        }
        None => {
            let symtab = symtab.ok_or("neither an export trie nor a symbol table")?;
            let table: SymbolTable<'_, Header, &[u8]> = symtab.symbols(endian, bytes).map_err(err)?;
            symtab_exports(&table, endian, &sections)?
        }
    };
    Ok(Shared { name, symbols, more: Vec::new(), inputs })
}

fn set_trie(
    trie: &mut Option<(&'static str, u32, u32)>,
    command: &'static str,
    offset: u32,
    size: u32,
) -> Result<(), String> {
    if let Some((other, ..)) = trie.replace((command, offset, size)) {
        return Err(format!("two export tries ({other} and {command})"));
    }
    Ok(())
}

fn uleb(data: &[u8], at: &mut usize) -> Result<u64, String> {
    let start = *at;
    let mut value = 0u64;
    let mut shift = 0u32;
    loop {
        let byte = *data.get(*at).ok_or_else(|| format!("a ULEB128 at {start:#x} runs past the end"))?;
        *at += 1;
        if shift > 63 || (shift == 63 && byte & 0x7e != 0) {
            return Err(format!("a ULEB128 at {start:#x} is too long"));
        }
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
        shift += 7;
    }
}

fn cstring<'a>(data: &'a [u8], at: &mut usize) -> Result<&'a [u8], String> {
    let rest = data.get(*at..).unwrap_or_default();
    let len = rest
        .iter()
        .position(|&it| it == 0)
        .ok_or_else(|| format!("a string at {:#x} runs past the end", *at))?;
    *at += len + 1;
    Ok(&rest[..len])
}

/// Every terminal node of the trie, in the order of a walk that takes edges as written.
fn trie_exports(data: &[u8], base: u64, sections: &[Section]) -> Result<Vec<SharedSymbol>, String> {
    let classify = |offset: u64| -> SymbolKind {
        let addr = base.wrapping_add(offset);
        match sections.iter().find(|it| it.contains(addr)) {
            Some(section) if section.code() => SymbolKind::Function,
            _ => SymbolKind::Data,
        }
    };
    let mut out = Vec::new();
    let mut visited = FxHashSet::default();
    let mut stack = vec![(0usize, Vec::new())];
    while let Some((node, prefix)) = stack.pop() {
        if !visited.insert(node) {
            return Err(format!("node {node:#x} is reached twice (a cycle)"));
        }
        let mut at = node;
        let terminal = uleb(data, &mut at)?;
        if terminal != 0 {
            let end = usize::try_from(terminal)
                .ok()
                .and_then(|it| at.checked_add(it))
                .filter(|&it| it <= data.len())
                .ok_or_else(|| format!("node {node:#x}'s export info runs past the end"))?;
            let info = &data[at..end];
            let (kind, weak) = export(info, &classify)
                .map_err(|what| format!("node {node:#x}: {what}"))?;
            let name = super::core_name(&prefix)?;
            out.push(SharedSymbol { name, version: None, kind, weak });
            at = end;
        }
        let count = *data.get(at).ok_or_else(|| format!("node {node:#x} runs past the end"))?;
        at += 1;
        let mut children = Vec::with_capacity(usize::from(count));
        for _ in 0..count {
            let label = cstring(data, &mut at)?;
            let child = uleb(data, &mut at)?;
            let child = usize::try_from(child)
                .ok()
                .filter(|&it| it < data.len())
                .ok_or_else(|| format!("node {node:#x} has a child at {child:#x}, out of range"))?;
            let mut name = prefix.clone();
            name.extend_from_slice(label);
            children.push((child, name));
        }
        stack.extend(children.into_iter().rev());
    }
    Ok(out)
}

/// What a terminal node's export info says: the symbol's kind and whether it is weak.
fn export(info: &[u8], classify: &dyn Fn(u64) -> SymbolKind) -> Result<(SymbolKind, bool), String> {
    let known = u64::from(
        macho::EXPORT_SYMBOL_FLAGS_KIND_MASK
            | macho::EXPORT_SYMBOL_FLAGS_WEAK_DEFINITION
            | macho::EXPORT_SYMBOL_FLAGS_REEXPORT
            | macho::EXPORT_SYMBOL_FLAGS_STUB_AND_RESOLVER,
    ) | EXPORT_SYMBOL_FLAGS_STATIC_RESOLVER;
    let mut at = 0;
    let flags = uleb(info, &mut at)?;
    if flags & !known != 0 {
        return Err(format!("unknown export flags {:#x}", flags & !known));
    }
    let weak = flags & u64::from(macho::EXPORT_SYMBOL_FLAGS_WEAK_DEFINITION) != 0;
    let kind = (flags & u64::from(macho::EXPORT_SYMBOL_FLAGS_KIND_MASK)) as u32;
    let tls = kind == macho::EXPORT_SYMBOL_FLAGS_KIND_THREAD_LOCAL;
    if kind > macho::EXPORT_SYMBOL_FLAGS_KIND_ABSOLUTE {
        return Err(format!("unknown export kind {kind}"));
    }
    let kind = if flags & u64::from(macho::EXPORT_SYMBOL_FLAGS_REEXPORT) != 0 {
        uleb(info, &mut at)?;
        cstring(info, &mut at)?;
        if tls { SymbolKind::Tls } else { SymbolKind::Other }
    } else if flags & u64::from(macho::EXPORT_SYMBOL_FLAGS_STUB_AND_RESOLVER) != 0 {
        let stub = uleb(info, &mut at)?;
        uleb(info, &mut at)?;
        classify(stub)
    } else {
        let address = uleb(info, &mut at)?;
        if tls {
            SymbolKind::Tls
        } else if kind == macho::EXPORT_SYMBOL_FLAGS_KIND_ABSOLUTE {
            SymbolKind::Data
        } else if flags & EXPORT_SYMBOL_FLAGS_STATIC_RESOLVER != 0 {
            SymbolKind::Function
        } else {
            classify(address)
        }
    };
    Ok((kind, weak))
}

/// The symbol table's external definitions, for a library without an export trie.
fn symtab_exports(
    table: &SymbolTable<'_, Header, &[u8]>,
    endian: Endianness,
    sections: &[Section],
) -> Result<Vec<SharedSymbol>, String> {
    let mut out = Vec::new();
    for nlist in table.iter() {
        let n_type = nlist.n_type();
        if n_type & macho::N_STAB != 0 || n_type & macho::N_EXT == 0 || n_type & macho::N_PEXT != 0 {
            continue;
        }
        let kind = match n_type & macho::N_TYPE {
            macho::N_UNDF | macho::N_PBUD => continue,
            macho::N_SECT => {
                let index = nlist.n_sect();
                let section = usize::from(index)
                    .checked_sub(1)
                    .and_then(|it| sections.get(it))
                    .ok_or_else(|| format!("a symbol in section {index}, which does not exist"))?;
                if section.flags & macho::SECTION_TYPE == macho::S_THREAD_LOCAL_VARIABLES {
                    SymbolKind::Tls
                } else if section.code() {
                    SymbolKind::Function
                } else {
                    SymbolKind::Data
                }
            }
            macho::N_ABS => SymbolKind::Data,
            macho::N_INDR => SymbolKind::Other,
            other => return Err(format!("a symbol of unknown type {other:#x}")),
        };
        let name = nlist.name(endian, table.strings()).map_err(err)?;
        out.push(SharedSymbol {
            name: super::core_name(name)?,
            version: None,
            kind,
            weak: nlist.n_desc(endian) & macho::N_WEAK_DEF != 0,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u32le(out: &mut Vec<u8>, value: u32) {
        out.extend_from_slice(&value.to_le_bytes());
    }

    fn u64le(out: &mut Vec<u8>, value: u64) {
        out.extend_from_slice(&value.to_le_bytes());
    }

    fn name16(out: &mut Vec<u8>, name: &str) {
        let mut field = [0u8; 16];
        field[..name.len()].copy_from_slice(name.as_bytes());
        out.extend_from_slice(&field);
    }

    fn segment(name: &str, vmaddr: u64, fileoff: u64, filesize: u64, sections: &[(&str, u64, u64, u32)]) -> Vec<u8> {
        let mut out = Vec::new();
        u32le(&mut out, macho::LC_SEGMENT_64);
        u32le(&mut out, 72 + 80 * sections.len() as u32);
        name16(&mut out, name);
        u64le(&mut out, vmaddr);
        u64le(&mut out, 0x4000);
        u64le(&mut out, fileoff);
        u64le(&mut out, filesize);
        u32le(&mut out, 7);
        u32le(&mut out, 5);
        u32le(&mut out, sections.len() as u32);
        u32le(&mut out, 0);
        for &(sect, addr, size, flags) in sections {
            name16(&mut out, sect);
            name16(&mut out, name);
            u64le(&mut out, addr);
            u64le(&mut out, size);
            for value in [0, 4, 0, 0, flags, 0, 0, 0] {
                u32le(&mut out, value);
            }
        }
        out
    }

    fn dylib_command(cmd: u32, path: &str) -> Vec<u8> {
        let size = (24 + path.len() + 1).next_multiple_of(8);
        let mut out = Vec::new();
        u32le(&mut out, cmd);
        u32le(&mut out, size as u32);
        for value in [24, 2, 0x10000, 0x10000] {
            u32le(&mut out, value);
        }
        out.extend_from_slice(path.as_bytes());
        out.resize(size, 0);
        out
    }

    fn dyld_info(export_off: u32, export_size: u32) -> Vec<u8> {
        let mut out = Vec::new();
        u32le(&mut out, macho::LC_DYLD_INFO_ONLY);
        u32le(&mut out, 48);
        for _ in 0..8 {
            u32le(&mut out, 0);
        }
        u32le(&mut out, export_off);
        u32le(&mut out, export_size);
        out
    }

    fn symtab_command(symoff: u32, nsyms: u32, stroff: u32, strsize: u32) -> Vec<u8> {
        let mut out = Vec::new();
        for value in [macho::LC_SYMTAB, 24, symoff, nsyms, stroff, strsize] {
            u32le(&mut out, value);
        }
        out
    }

    /// A dylib of `cpu` whose commands `commands(tail)` make, given where `tail` (the
    /// linkedit data) starts in the file.
    fn file(cpu: u32, commands: impl Fn(u32) -> Vec<Vec<u8>>, tail: &[u8]) -> Vec<u8> {
        let sizeofcmds: usize = commands(0).iter().map(Vec::len).sum();
        let offset = (32 + sizeofcmds).next_multiple_of(8);
        let commands = commands(offset as u32);
        let mut out = Vec::new();
        u32le(&mut out, macho::MH_MAGIC_64);
        u32le(&mut out, cpu);
        u32le(&mut out, 0);
        u32le(&mut out, macho::MH_DYLIB);
        u32le(&mut out, commands.len() as u32);
        u32le(&mut out, sizeofcmds as u32);
        u32le(&mut out, 0);
        u32le(&mut out, 0);
        for command in commands {
            out.extend_from_slice(&command);
        }
        out.resize(offset, 0);
        out.extend_from_slice(tail);
        out
    }

    fn segments(len: u64) -> Vec<Vec<u8>> {
        let code = macho::S_ATTR_PURE_INSTRUCTIONS | macho::S_ATTR_SOME_INSTRUCTIONS;
        vec![
            segment("__TEXT", 0, 0, len, &[("__text", 0x1000, 0x100, code)]),
            segment(
                "__DATA",
                0x4000,
                0x4000,
                0,
                &[("__data", 0x4000, 0x100, 0), ("__thread_vars", 0x4100, 0x30, macho::S_THREAD_LOCAL_VARIABLES)],
            ),
        ]
    }

    fn with_trie(cpu: u32, trie: &[u8]) -> Vec<u8> {
        let size = trie.len() as u32;
        file(
            cpu,
            |tail| {
                let mut commands = segments(0x1000);
                commands.push(dylib_command(macho::LC_ID_DYLIB, "/usr/lib/libfoo.dylib"));
                commands.push(dylib_command(macho::LC_REEXPORT_DYLIB, "/usr/lib/libbar.dylib"));
                commands.push(dyld_info(tail, size));
                commands
            },
            trie,
        )
    }

    fn encode_uleb(out: &mut Vec<u8>, mut value: u64) {
        loop {
            let byte = (value & 0x7f) as u8;
            value >>= 7;
            if value == 0 {
                out.push(byte);
                return;
            }
            out.push(byte | 0x80);
        }
    }

    fn uleb_len(value: u64) -> usize {
        let mut out = Vec::new();
        encode_uleb(&mut out, value);
        out.len()
    }

    struct Node {
        terminal: Vec<u8>,
        children: Vec<(&'static str, Node)>,
    }

    fn leaf(terminal: Vec<u8>) -> Node {
        Node { terminal, children: Vec::new() }
    }

    fn flatten<'a>(node: &'a Node, out: &mut Vec<(&'a Node, Vec<(&'static str, usize)>)>) -> usize {
        let index = out.len();
        out.push((node, Vec::new()));
        for &(label, ref child) in &node.children {
            let child = flatten(child, out);
            out[index].1.push((label, child));
        }
        index
    }

    /// Lays a trie out as ld64 does: offsets grown until every child offset's ULEB fits.
    fn build_trie(root: &Node) -> Vec<u8> {
        let mut nodes = Vec::new();
        flatten(root, &mut nodes);
        let mut offsets = vec![0u64; nodes.len()];
        loop {
            let mut next = Vec::with_capacity(nodes.len());
            let mut at = 0u64;
            for (node, children) in &nodes {
                next.push(at);
                let len = node.terminal.len() as u64;
                at += uleb_len(len) as u64 + len + 1;
                for (label, child) in children {
                    at += label.len() as u64 + 1 + uleb_len(offsets[*child]) as u64;
                }
            }
            if next == offsets {
                break;
            }
            offsets = next;
        }
        let mut out = Vec::new();
        for (index, (node, children)) in nodes.iter().enumerate() {
            assert_eq!(out.len() as u64, offsets[index]);
            encode_uleb(&mut out, node.terminal.len() as u64);
            out.extend_from_slice(&node.terminal);
            out.push(children.len() as u8);
            for (label, child) in children {
                out.extend_from_slice(label.as_bytes());
                out.push(0);
                encode_uleb(&mut out, offsets[*child]);
            }
        }
        out
    }

    fn info(flags: u32, values: &[u64]) -> Vec<u8> {
        let mut out = Vec::new();
        encode_uleb(&mut out, u64::from(flags));
        for &value in values {
            encode_uleb(&mut out, value);
        }
        out
    }

    fn reexport(flags: u32, ordinal: u64, import: &str) -> Vec<u8> {
        let mut out = info(flags | macho::EXPORT_SYMBOL_FLAGS_REEXPORT, &[ordinal]);
        out.extend_from_slice(import.as_bytes());
        out.push(0);
        out
    }

    fn sym(name: &str, kind: SymbolKind, weak: bool) -> SharedSymbol {
        SharedSymbol { name: name.to_owned(), version: None, kind, weak }
    }

    fn sample_trie() -> Vec<u8> {
        let foo = Node {
            terminal: info(0, &[0x1010]),
            children: vec![("bar", leaf(info(macho::EXPORT_SYMBOL_FLAGS_WEAK_DEFINITION, &[0x4010])))],
        };
        let fo = Node {
            terminal: Vec::new(),
            children: vec![
                ("o", foo),
                ("b", leaf(reexport(0, 1, "_bob"))),
                ("_tls", leaf(info(macho::EXPORT_SYMBOL_FLAGS_KIND_THREAD_LOCAL, &[0x4100]))),
            ],
        };
        let underscore = Node {
            terminal: Vec::new(),
            children: vec![
                ("fo", fo),
                ("abs", leaf(info(macho::EXPORT_SYMBOL_FLAGS_KIND_ABSOLUTE, &[0x1234]))),
                ("res", leaf(info(macho::EXPORT_SYMBOL_FLAGS_STUB_AND_RESOLVER, &[0x1020, 0x1030]))),
                ("same", leaf(reexport(0, 1, ""))),
            ],
        };
        build_trie(&Node { terminal: Vec::new(), children: vec![("_", underscore)] })
    }

    #[test]
    fn trie_dylib() {
        let bytes = with_trie(macho::CPU_TYPE_ARM64, &sample_trie());
        let shared = read("libfoo.dylib", &bytes, Arch::Aarch64).unwrap();
        assert_eq!(shared.name, "/usr/lib/libfoo.dylib");
        assert_eq!(shared.inputs, vec!["/usr/lib/libbar.dylib".to_owned()]);
        assert!(shared.more.is_empty());
        assert_eq!(
            shared.symbols,
            vec![
                sym("foo", SymbolKind::Function, false),
                sym("foobar", SymbolKind::Data, true),
                sym("fob", SymbolKind::Other, false),
                sym("fo_tls", SymbolKind::Tls, false),
                sym("abs", SymbolKind::Data, false),
                sym("res", SymbolKind::Function, false),
                sym("same", SymbolKind::Other, false),
            ]
        );
    }

    #[test]
    fn exports_trie_command() {
        let trie = sample_trie();
        let size = trie.len() as u32;
        let bytes = file(
            macho::CPU_TYPE_X86_64,
            |tail| {
                let mut commands = segments(0x1000);
                commands.push(dylib_command(macho::LC_ID_DYLIB, "/usr/lib/libfoo.dylib"));
                let mut trie = Vec::new();
                for value in [macho::LC_DYLD_EXPORTS_TRIE, 16, tail, size] {
                    u32le(&mut trie, value);
                }
                commands.push(trie);
                commands
            },
            &trie,
        );
        let shared = read("libfoo.dylib", &bytes, Arch::X86_64).unwrap();
        assert_eq!(shared.symbols.len(), 7);
        assert!(shared.inputs.is_empty());
    }

    #[test]
    fn wrong_cpu() {
        let bytes = with_trie(macho::CPU_TYPE_ARM64, &sample_trie());
        let error = read("libfoo.dylib", &bytes, Arch::X86_64).unwrap_err();
        assert!(error.starts_with("libfoo.dylib: ") && error.contains("CPU type"), "{error}");
    }

    #[test]
    fn malformed_tries() {
        let cycle = [0x00, 0x01, b'a', 0x00, 0x00];
        let out_of_range = [0x00, 0x01, b'a', 0x00, 0x40];
        let mut too_long = vec![0x80; 11];
        too_long.extend_from_slice(&[0x00, 0x00]);
        let runs_past = [0x05, 0x00];
        for (trie, what) in [
            (&cycle[..], "reached twice"),
            (&out_of_range[..], "out of range"),
            (&too_long[..], "too long"),
            (&runs_past[..], "runs past the end"),
        ] {
            let bytes = with_trie(macho::CPU_TYPE_ARM64, trie);
            let error = read("bad.dylib", &bytes, Arch::Aarch64).unwrap_err();
            assert!(error.starts_with("bad.dylib: ") && error.contains(what), "{what}: {error}");
        }
    }

    #[test]
    fn symtab_dylib() {
        let symbols: [(&str, u8, u8, u16, u64); 7] = [
            ("_f", macho::N_SECT | macho::N_EXT, 1, 0, 0x1000),
            ("_d", macho::N_SECT | macho::N_EXT, 2, macho::N_WEAK_DEF, 0x4000),
            ("_t", macho::N_SECT | macho::N_EXT, 3, 0, 0x4100),
            ("_local", macho::N_SECT, 1, 0, 0x1004),
            ("_pext", macho::N_SECT | macho::N_EXT | macho::N_PEXT, 1, 0, 0x1008),
            ("_undef", macho::N_UNDF | macho::N_EXT, 0, 0, 0),
            ("_abs", macho::N_ABS | macho::N_EXT, 0, 0, 0x42),
        ];
        let mut strings = vec![0u8];
        let mut table = Vec::new();
        for (name, n_type, n_sect, n_desc, value) in symbols {
            u32le(&mut table, strings.len() as u32);
            table.extend_from_slice(&[n_type, n_sect]);
            table.extend_from_slice(&n_desc.to_le_bytes());
            u64le(&mut table, value);
            strings.extend_from_slice(name.as_bytes());
            strings.push(0);
        }
        let nsyms = symbols.len() as u32;
        let strsize = strings.len() as u32;
        let table_len = table.len() as u32;
        let mut tail = table;
        tail.extend_from_slice(&strings);
        let bytes = file(
            macho::CPU_TYPE_ARM64,
            |at| {
                let mut commands = segments(0x1000);
                commands.push(dylib_command(macho::LC_ID_DYLIB, "/usr/lib/libsym.dylib"));
                commands.push(symtab_command(at, nsyms, at + table_len, strsize));
                commands
            },
            &tail,
        );
        let shared = read("libsym.dylib", &bytes, Arch::Aarch64).unwrap();
        assert_eq!(shared.name, "/usr/lib/libsym.dylib");
        assert_eq!(
            shared.symbols,
            vec![
                sym("f", SymbolKind::Function, false),
                sym("d", SymbolKind::Data, true),
                sym("t", SymbolKind::Tls, false),
                sym("abs", SymbolKind::Data, false),
            ]
        );
    }

    #[test]
    fn uleb_limits() {
        let mut max = Vec::new();
        encode_uleb(&mut max, u64::MAX);
        let mut at = 0;
        assert_eq!(uleb(&max, &mut at).unwrap(), u64::MAX);
        assert_eq!(at, max.len());
        let over = [0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x02];
        assert!(uleb(&over, &mut 0).unwrap_err().contains("too long"));
    }
}
