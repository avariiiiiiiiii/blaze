//! `ar` archives of ELF objects: GNU, GNU 64-bit and BSD.

use std::collections::hash_map::Entry;

use blaze_codegen::Arch;
use object::read::archive::{ArchiveFile, ArchiveKind};
use rayon::prelude::*;
use rustc_hash::FxHashMap;

use crate::model::{Archive, Member, Name, Names};

pub(super) fn read(arch: Arch, name: &str, bytes: &[u8]) -> Result<Archive, String> {
    let error = |error: object::read::Error| format!("{name}: {error}");
    let limit = bytes.len();
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
    let mut names = Names::default();
    let mut members: Vec<Member> = Vec::new();
    for member in file.members() {
        let member = member.map_err(error)?;
        if matches!(member.name(), b"/" | b"//" | b"/SYM64/")
            || member.name().starts_with(b"__.SYMDEF")
        {
            continue;
        }
        let member_name = String::from_utf8_lossy(member.name());
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
        members.push(Member { name: push(&mut names, &member_name, name, limit)?, bytes: range });
    }

    let mut index = Vec::new();
    match file.symbols().map_err(error)? {
        Some(symbols) => {
            index.reserve(symbols.size_hint().0);
            // A BSD index names each string by its offset, so many entries may name one: each
            // string is pushed once, keyed by where it is in the input. A GNU index lists every
            // name after the one before.
            let shared = !matches!(file.kind(), ArchiveKind::Gnu | ArchiveKind::Gnu64);
            let mut pushed: FxHashMap<(usize, usize), Name> = FxHashMap::default();
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
                let interned = if shared {
                    match pushed.entry((symbol_name.as_ptr().addr(), symbol_name.len())) {
                        Entry::Occupied(it) => *it.get(),
                        Entry::Vacant(it) => *it.insert(push(&mut names, symbol_name, name, limit)?),
                    }
                } else {
                    push(&mut names, symbol_name, name, limit)?
                };
                index.push((interned, member));
            }
        }
        None => {
            // What lld does without a symbol table: read every member, in parallel, and keep
            // member order.
            let defined: Vec<Result<Vec<&str>, String>> = members
                .par_iter()
                .map(|member| {
                    let member_name = format!("{name}({})", names.at(member.name));
                    super::defined(arch, &member_name, &bytes[member.bytes.clone()])
                })
                .collect();
            // Names from members' `.strtab`s, which may share tails: not held to `limit`.
            let full = super::full(name);
            for (number, found) in defined.into_iter().enumerate() {
                for symbol in found? {
                    index.push((names.push(symbol).map_err(full)?, number as u32));
                }
            }
        }
    }
    Ok(Archive { members, index, names })
}

/// Pushes `text` into `names`, refusing an arena bigger than the archive (`limit` bytes): a
/// real archive holds every name it lists, so only names that overlap in the input (suffixes
/// of one string in a BSD index, a GNU long name several members point at) could make it so.
fn push(names: &mut Names, text: &str, name: &str, limit: usize) -> Result<Name, String> {
    let pushed = names.push(text).map_err(super::full(name))?;
    if names.bytes() > limit {
        return Err(format!("{name}: its names come to more bytes than the archive holds"));
    }
    Ok(pushed)
}
