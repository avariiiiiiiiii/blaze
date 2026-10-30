//! `ar` archives of ELF objects: GNU, GNU 64-bit and BSD.

use blaze_codegen::Arch;
use object::read::archive::{ArchiveFile, ArchiveKind};
use rayon::prelude::*;

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

    // In archive order, so by the start of their bytes: a symbol finds its member by binary
    // search on that start, which is what its header's offset in the symbol table comes to.
    let mut members: Vec<Member> = Vec::new();
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
        if members.last().is_some_and(|last| last.bytes.start >= range.start) {
            return Err(format!("{name}: member {member_name} does not follow the member before it"));
        }
        members.push(Member { name: member_name, bytes: range });
    }

    let mut index = Vec::new();
    match file.symbols().map_err(error)? {
        Some(symbols) => {
            // A member's symbols are listed together: its header is parsed once for the run.
            let mut last: Option<(u64, u32)> = None;
            for symbol in symbols {
                let symbol = symbol.map_err(error)?;
                let symbol_name = super::utf8(name, "the archive symbol", symbol.name())?;
                let offset = symbol.offset();
                let member = match last {
                    Some((at, member)) if at == offset.0 => member,
                    _ => {
                        let header = file.member(offset).map_err(|error| {
                            format!("{name}: archive symbol {symbol_name}: {error}")
                        })?;
                        let start = usize::try_from(header.file_range().0).ok();
                        let Some(member) = start
                            .and_then(|start| members.binary_search_by_key(&start, |it| it.bytes.start).ok())
                        else {
                            return Err(format!(
                                "{name}: archive symbol {symbol_name} names a member at offset {}, \
                                 which is not a member file",
                                offset.0
                            ));
                        };
                        last = Some((offset.0, member as u32));
                        member as u32
                    }
                };
                index.push((symbol_name, member));
            }
        }
        None => {
            // What lld does without a symbol table: read every member, in parallel, and keep
            // member order.
            let defined: Vec<Result<Vec<String>, String>> = members
                .par_iter()
                .map(|member| {
                    let member_name = format!("{name}({})", member.name);
                    super::defined(arch, &member_name, &bytes[member.bytes.clone()])
                })
                .collect();
            for (number, names) in defined.into_iter().enumerate() {
                index.extend(names?.into_iter().map(|symbol| (symbol, number as u32)));
            }
        }
    }
    Ok(Archive { members, index })
}
