//! Static archives as ld64 takes them: members found through the archive's symbol index.

#![forbid(unsafe_code)]

use object::read::archive::{ArchiveFile, ArchiveKind};

use crate::model::{Archive, Member, Names};

/// A BSD `ar` archive (as Apple's `libtool`/`ar` write it, `#1/<len>` long names) or a GNU one,
/// with its `__.SYMDEF`/`__.SYMDEF SORTED`/`__.SYMDEF_64` index (a GNU `/` index too).
/// `Member::bytes` is the member's data, as a range of `bytes`.
pub(super) fn read(name: &str, bytes: &[u8]) -> Result<Archive, String> {
    let file = ArchiveFile::parse(bytes).map_err(|e| format!("{name}: {e}"))?;
    if file.is_thin() {
        return Err(format!("{name}: a thin archive, whose members are other files"));
    }
    match file.kind() {
        ArchiveKind::Bsd | ArchiveKind::Bsd64 | ArchiveKind::Gnu | ArchiveKind::Gnu64 => {}
        ArchiveKind::Unknown => {
            return Err(format!("{name}: an archive without a symbol index (run `ranlib` on it)"));
        }
        other => return Err(format!("{name}: a {other:?} archive, not a BSD or GNU one")),
    }

    let symbols = file
        .symbols()
        .map_err(|e| format!("{name}: {e}"))?
        .ok_or_else(|| format!("{name}: an archive without a symbol index (run `ranlib` on it)"))?;
    // A BSD `__.SYMDEF` names each entry's string by offset, so many entries may name one
    // string: each distinct string is one `Name`, or the arena grows with the entries times
    // the string's length. Keyed by where the string is in `bytes`; SipHash, since the
    // offsets are the file's.
    let mut by_offset: std::collections::HashMap<usize, Option<Name>> =
        std::collections::HashMap::with_capacity(symbols.size_hint().0);
    // Sized from the index's distinct names, read in place once: a core name is at most one
    // byte longer than its Mach-O one. The members' names, short and few, grow it at most once.
    let mut index_bytes = 0;
    for symbol in symbols.clone() {
        let raw = symbol.map_err(|e| format!("{name}: {e}"))?.name();
        if by_offset.insert(raw.as_ptr() as usize - bytes.as_ptr() as usize, None).is_none() {
            index_bytes += raw.len() + 1;
        }
    }
    let mut names = Names::with_capacity(index_bytes);

    let mut members = Vec::new();
    // Where each member's header is, in archive order (so sorted): what the index's offsets
    // name, found by binary search without parsing the member again. 8 bytes a member.
    let mut heads: Vec<u64> = Vec::new();
    for member in file.members() {
        let member = member.map_err(|e| format!("{name}: {e}"))?;
        let header = member
            .header()
            .ok_or_else(|| format!("{name}: a member without an `ar` header"))?;
        heads.push((std::ptr::from_ref(header) as usize - bytes.as_ptr() as usize) as u64);
        let (offset, size) = member.file_range();
        let range = usize::try_from(offset)
            .ok()
            .and_then(|start| Some(start..start.checked_add(usize::try_from(size).ok()?)?))
            .filter(|it| it.end <= bytes.len())
            .ok_or_else(|| format!("{name}: a member past the end of the archive"))?;
        let member_name = std::str::from_utf8(member.name())
            .map_err(|_| format!("{name}: a member whose name isn't UTF-8: {:?}", member.name()))?;
        let member_name = names.push(member_name).map_err(|e| format!("{name}: {e}"))?;
        members.push(Member { name: member_name, bytes: range });
    }

    let mut index = Vec::with_capacity(symbols.size_hint().0);
    for symbol in symbols {
        let symbol = symbol.map_err(|e| format!("{name}: {e}"))?;
        let found = heads.binary_search(&symbol.offset().0).map_err(|_| {
            let shown = String::from_utf8_lossy(symbol.name());
            format!("{name}: the index puts {shown} at offset {}, which is no member", symbol.offset().0)
        })?;
        let raw = symbol.name();
        let slot = by_offset.entry(raw.as_ptr() as usize - bytes.as_ptr() as usize).or_default();
        let core = match *slot {
            Some(core) => core,
            None => *slot.insert(super::push_core_name(&mut names, raw).map_err(|e| format!("{name}: {e}"))?),
        };
        index.push((core, found as u32));
    }
    Ok(Archive { members, index, names })
}

#[cfg(test)]
mod tests {
    use super::read;
    use crate::model::Archive;

    fn header(out: &mut Vec<u8>, name: &str, size: usize) {
        out.extend_from_slice(format!("{name:<16}{:<12}{:<6}{:<6}{:<8}{size:<10}`\n", 0, 0, 0, 644).as_bytes());
    }

    /// A BSD member: a `#1/<len>` name padded with NULs to 8 bytes, as Apple's `ar` writes.
    fn bsd_member(out: &mut Vec<u8>, name: &str, data: &[u8]) {
        let padded = (name.len() + 1).next_multiple_of(8);
        header(out, &format!("#1/{padded}"), padded + data.len());
        out.extend_from_slice(name.as_bytes());
        out.resize(out.len() + padded - name.len(), 0);
        out.extend_from_slice(data);
        if out.len() % 2 == 1 {
            out.push(b'\n');
        }
    }

    /// `members` with a `__.SYMDEF` naming `symbols` (name, member index).
    fn bsd(members: &[(&str, &[u8])], symbols: &[(&str, usize)]) -> Vec<u8> {
        let mut strings = Vec::new();
        let mut strx = Vec::new();
        for (symbol, _) in symbols {
            strx.push(strings.len() as u32);
            strings.extend_from_slice(symbol.as_bytes());
            strings.push(0);
        }
        strings.resize(strings.len().next_multiple_of(4), 0);
        let symdef_name = "__.SYMDEF SORTED";
        let symdef_padded = (symdef_name.len() + 1).next_multiple_of(8);
        let symdef_len = 4 + 8 * symbols.len() + 4 + strings.len();
        let mut at = 8 + 60 + symdef_padded + symdef_len;
        at += at % 2;
        let mut offsets = Vec::new();
        for (name, data) in members {
            offsets.push(at);
            at += 60 + (name.len() + 1).next_multiple_of(8) + data.len();
            at += at % 2;
        }
        let mut symdef = Vec::new();
        symdef.extend_from_slice(&(8 * symbols.len() as u32).to_le_bytes());
        for ((_, member), strx) in symbols.iter().zip(strx) {
            symdef.extend_from_slice(&strx.to_le_bytes());
            symdef.extend_from_slice(&(offsets[*member] as u32).to_le_bytes());
        }
        symdef.extend_from_slice(&(strings.len() as u32).to_le_bytes());
        symdef.extend_from_slice(&strings);

        let mut out = b"!<arch>\n".to_vec();
        bsd_member(&mut out, symdef_name, &symdef);
        for ((name, data), offset) in members.iter().zip(&offsets) {
            assert_eq!(out.len(), *offset);
            bsd_member(&mut out, name, data);
        }
        out
    }

    fn core(name: &str) -> String {
        crate::macho::core_name(name.as_bytes()).unwrap()
    }

    fn member_names(archive: &Archive) -> Vec<&str> {
        archive.members.iter().map(|it| archive.name(it.name)).collect()
    }

    fn index(archive: &Archive) -> Vec<(String, u32)> {
        archive.index.iter().map(|&(name, member)| (archive.name(name).to_owned(), member)).collect()
    }

    #[test]
    fn bsd_archive_with_long_names_and_its_index() {
        let bytes = bsd(
            &[("a_rather_long_member_name.o", &b"first"[..]), ("b.o", &b"second!"[..])],
            &[("_one", 0), ("_two", 1), ("_three", 1)],
        );
        let archive = read("libx.a", &bytes).unwrap();
        assert_eq!(member_names(&archive), ["a_rather_long_member_name.o", "b.o"]);
        assert_eq!(&bytes[archive.members[0].bytes.clone()], b"first");
        assert_eq!(&bytes[archive.members[1].bytes.clone()], b"second!");
        assert_eq!(index(&archive), [(core("_one"), 0), (core("_two"), 1), (core("_three"), 1)]);
    }

    #[test]
    fn gnu_archive_and_its_index() {
        let members: [(&str, &[u8]); 2] = [("a.o", &b"aaaa"[..]), ("b.o", &b"bbb"[..])];
        let names = b"_f\0_g\0";
        let index_len = 4 + 4 * 2 + names.len();
        let mut at = 8 + 60 + index_len;
        at += at % 2;
        let first = at;
        at += 60 + members[0].1.len();
        at += at % 2;
        let second = at;

        let mut out = b"!<arch>\n".to_vec();
        header(&mut out, "/", index_len);
        out.extend_from_slice(&2u32.to_be_bytes());
        out.extend_from_slice(&(first as u32).to_be_bytes());
        out.extend_from_slice(&(second as u32).to_be_bytes());
        out.extend_from_slice(names);
        if out.len() % 2 == 1 {
            out.push(b'\n');
        }
        for (name, data) in members {
            header(&mut out, &format!("{name}/"), data.len());
            out.extend_from_slice(data);
            if out.len() % 2 == 1 {
                out.push(b'\n');
            }
        }
        let archive = read("libx.a", &out).unwrap();
        assert_eq!(member_names(&archive), ["a.o", "b.o"]);
        assert_eq!(&out[archive.members[1].bytes.clone()], b"bbb");
        assert_eq!(index(&archive), [(core("_f"), 0), (core("_g"), 1)]);
    }

    #[test]
    fn seeded_indexes_name_the_members_they_were_written_for() {
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut below = |n: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed % n
        };
        for round in 0..64 {
            let count = 1 + below(20) as usize;
            let names: Vec<String> = (0..count)
                .map(|i| if below(2) == 0 { format!("m{i}.o") } else { format!("member_with_a_long_name_{round}_{i}.o") })
                .collect();
            let datas: Vec<Vec<u8>> = (0..count).map(|i| vec![i as u8; 1 + below(40) as usize]).collect();
            let symbols: Vec<(String, usize)> =
                (0..below(50)).map(|i| (format!("_s{i}"), below(count as u64) as usize)).collect();
            let members: Vec<(&str, &[u8])> =
                names.iter().zip(&datas).map(|(name, data)| (name.as_str(), data.as_slice())).collect();
            let index: Vec<(&str, usize)> = symbols.iter().map(|(name, at)| (name.as_str(), *at)).collect();
            let bytes = bsd(&members, &index);
            let archive = read("libseeded.a", &bytes).unwrap_or_else(|e| panic!("round {round}: {e}"));
            assert_eq!(member_names(&archive), names, "round {round}");
            for (member, data) in archive.members.iter().zip(&datas) {
                assert_eq!(&bytes[member.bytes.clone()], data.as_slice(), "round {round}");
            }
            let want: Vec<(String, u32)> = symbols.iter().map(|(name, at)| (core(name), *at as u32)).collect();
            assert_eq!(index(&archive), want, "round {round}");
            // Every name in the one arena: the members' and the index's, nothing else.
            let bytes_named: usize = names.iter().map(String::len).sum::<usize>()
                + want.iter().map(|(name, _)| name.len()).sum::<usize>();
            assert_eq!(archive.names.bytes(), bytes_named, "round {round}");
        }
    }

    #[test]
    fn an_index_entry_that_names_no_member_is_refused() {
        let mut bytes = bsd(&[("a.o", &b"aaaa"[..])], &[("_a", 0)]);
        // The `__.SYMDEF`'s one entry: its member offset, 8 bytes past the ranlib array's size.
        let at = 8 + 60 + ("__.SYMDEF SORTED".len() + 1).next_multiple_of(8) + 8;
        let offset = u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap());
        bytes[at..at + 4].copy_from_slice(&(offset + 2).to_le_bytes());
        let error = read("libx.a", &bytes).unwrap_err();
        assert!(error.contains("_a") && error.contains("no member"), "{error}");
    }

    #[test]
    fn an_archive_without_an_index_is_refused() {
        let mut out = b"!<arch>\n".to_vec();
        bsd_member(&mut out, "a.o", b"data");
        let error = read("libnoindex.a", &out).unwrap_err();
        assert!(error.contains("libnoindex.a") && error.contains("symbol index"), "{error}");
    }
}
