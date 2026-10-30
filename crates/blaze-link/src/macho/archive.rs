//! Static archives as ld64 takes them: members found through the archive's symbol index.

use object::read::archive::{ArchiveFile, ArchiveKind};

use crate::model::{Archive, Member};

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

    let mut members = Vec::new();
    // Where each member's data starts, in archive order: what the index's offsets lead to.
    let mut starts = Vec::new();
    for member in file.members() {
        let member = member.map_err(|e| format!("{name}: {e}"))?;
        let (offset, size) = member.file_range();
        let range = usize::try_from(offset)
            .ok()
            .and_then(|start| Some(start..start.checked_add(usize::try_from(size).ok()?)?))
            .filter(|it| it.end <= bytes.len())
            .ok_or_else(|| format!("{name}: a member past the end of the archive"))?;
        let member_name = std::str::from_utf8(member.name())
            .map_err(|_| format!("{name}: a member whose name isn't UTF-8: {:?}", member.name()))?;
        starts.push(offset);
        members.push(Member { name: member_name.to_owned(), bytes: range });
    }

    let symbols = file
        .symbols()
        .map_err(|e| format!("{name}: {e}"))?
        .ok_or_else(|| format!("{name}: an archive without a symbol index (run `ranlib` on it)"))?;
    let mut index = Vec::new();
    for symbol in symbols {
        let symbol = symbol.map_err(|e| format!("{name}: {e}"))?;
        let shown = || String::from_utf8_lossy(symbol.name()).into_owned();
        let member = file.member(symbol.offset()).map_err(|e| {
            format!("{name}: the index puts {} at offset {}: {e}", shown(), symbol.offset().0)
        })?;
        let start = member.file_range().0;
        let found = starts.binary_search(&start).map_err(|_| {
            format!("{name}: the index puts {} at offset {}, which is no member", shown(), symbol.offset().0)
        })?;
        let core = super::core_name(symbol.name()).map_err(|e| format!("{name}: {e}"))?;
        index.push((core, found as u32));
    }
    Ok(Archive { members, index })
}

#[cfg(test)]
mod tests {
    use super::read;

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

    #[test]
    fn bsd_archive_with_long_names_and_its_index() {
        let bytes = bsd(
            &[("a_rather_long_member_name.o", &b"first"[..]), ("b.o", &b"second!"[..])],
            &[("_one", 0), ("_two", 1), ("_three", 1)],
        );
        let archive = read("libx.a", &bytes).unwrap();
        let names: Vec<&str> = archive.members.iter().map(|it| it.name.as_str()).collect();
        assert_eq!(names, ["a_rather_long_member_name.o", "b.o"]);
        assert_eq!(&bytes[archive.members[0].bytes.clone()], b"first");
        assert_eq!(&bytes[archive.members[1].bytes.clone()], b"second!");
        assert_eq!(archive.index, [(core("_one"), 0), (core("_two"), 1), (core("_three"), 1)]);
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
        let names: Vec<&str> = archive.members.iter().map(|it| it.name.as_str()).collect();
        assert_eq!(names, ["a.o", "b.o"]);
        assert_eq!(&out[archive.members[1].bytes.clone()], b"bbb");
        assert_eq!(archive.index, [(core("_f"), 0), (core("_g"), 1)]);
    }

    #[test]
    fn an_archive_without_an_index_is_refused() {
        let mut out = b"!<arch>\n".to_vec();
        bsd_member(&mut out, "a.o", b"data");
        let error = read("libnoindex.a", &out).unwrap_err();
        assert!(error.contains("libnoindex.a") && error.contains("symbol index"), "{error}");
    }
}
