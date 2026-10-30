//! `ar` archives of ELF objects: GNU, GNU 64-bit and BSD.

use std::collections::BTreeMap;

use blaze_codegen::Arch;
use object::read::archive::{ArchiveFile, ArchiveKind};

use crate::model::{Archive, Member};

pub(super) fn read(arch: Arch, name: &str, bytes: &[u8]) -> Result<Archive, String> {
    let error = |error: object::read::Error| format!("{name}: {error}");
    let file = ArchiveFile::parse(bytes).map_err(error)?;
    if file.is_thin() {
        return Err(format!(
            "{name}: a thin archive, whose members are files outside it: blaze links only \
             archives that hold their members"
        ));
    }
    match file.kind() {
        ArchiveKind::Unknown
        | ArchiveKind::Gnu
        | ArchiveKind::Gnu64
        | ArchiveKind::Bsd
        | ArchiveKind::Bsd64 => {}
        kind => return Err(format!("{name}: a {kind:?} archive, not an archive of ELF objects")),
    }

    let mut members = Vec::new();
    // Each member's index by the offset of its bytes, which is what its header's offset in the
    // symbol table comes to.
    let mut by_start = BTreeMap::new();
    for member in file.members() {
        let member = member.map_err(error)?;
        if matches!(member.name(), b"/" | b"//" | b"/SYM64/")
            || member.name().starts_with(b"__.SYMDEF")
        {
            continue;
        }
        let member_name = String::from_utf8_lossy(member.name()).into_owned();
        let (offset, size) = member.file_range();
        let range = usize::try_from(offset)
            .ok()
            .zip(usize::try_from(size).ok())
            .and_then(|(start, size)| Some(start..start.checked_add(size)?))
            .filter(|range| range.end <= bytes.len())
            .ok_or_else(|| format!("{name}: member {member_name} runs past the archive's end"))?;
        by_start.insert(range.start, members.len() as u32);
        members.push(Member { name: member_name, bytes: range });
    }

    let mut index = Vec::new();
    match file.symbols().map_err(error)? {
        Some(symbols) => {
            for symbol in symbols {
                let symbol = symbol.map_err(error)?;
                let symbol_name = super::utf8(name, "the archive symbol", symbol.name())?;
                let member = file.member(symbol.offset()).map_err(|error| {
                    format!("{name}: archive symbol {symbol_name}: {error}")
                })?;
                let start = usize::try_from(member.file_range().0).ok();
                let Some(&member) = start.and_then(|start| by_start.get(&start)) else {
                    return Err(format!(
                        "{name}: archive symbol {symbol_name} names a member at offset {}, \
                         which is not a member file",
                        symbol.offset().0
                    ));
                };
                index.push((symbol_name, member));
            }
        }
        None => {
            for (number, member) in members.iter().enumerate() {
                let member_name = format!("{name}({})", member.name);
                for symbol in super::defined(arch, &member_name, &bytes[member.bytes.clone()])? {
                    index.push((symbol, number as u32));
                }
            }
        }
    }
    Ok(Archive { members, index })
}
