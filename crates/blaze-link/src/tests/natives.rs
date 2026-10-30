//! The engine's checks of what native inputs name on the line: a GNU linker script's `-lNAME`,
//! and the `/DEFAULTLIB`s of the COFF objects and archive members a PE link takes.

use std::sync::Arc;

use blaze_codegen::{Arch, Target};
use object::write::{Object as Builder, Symbol as Def, SymbolSection};
use object::{Architecture, BinaryFormat, Endianness, SectionKind, SymbolFlags, SymbolKind, SymbolScope};

use super::{CALLER, Change, Image, Native, NativeKind, function, id, key, name, output, rel32, returns};
use crate::{Debug, Format, LinkError, Output, OutputKind};

/// An object of `format` defining `defines` (each returning 1), with `directives` as its
/// `.drectve` when there are any.
fn object(format: BinaryFormat, defines: &[&str], directives: &str) -> Vec<u8> {
    let mut b = Builder::new(format, Architecture::X86_64, Endianness::Little);
    let text = b.add_section(Vec::new(), b".text".to_vec(), SectionKind::Text);
    for it in defines {
        let at = b.append_section_data(text, &returns(1), 16);
        b.add_symbol(Def {
            name: it.as_bytes().to_vec(),
            value: at,
            size: 6,
            kind: SymbolKind::Text,
            scope: SymbolScope::Linkage,
            weak: false,
            section: SymbolSection::Section(text),
            flags: SymbolFlags::None,
        });
    }
    if !directives.is_empty() {
        let drectve = b.add_section(Vec::new(), b".drectve".to_vec(), SectionKind::Linker);
        b.append_section_data(drectve, directives.as_bytes(), 1);
    }
    b.write().unwrap()
}

/// A GNU-format archive of `members` (name, bytes, the symbols it defines), with its symbol
/// index: what both ELF and COFF archives may be.
fn archive(members: &[(&str, &[u8], &[&str])]) -> Vec<u8> {
    let header = |name: &str, size: usize| format!("{name:<16}{:<12}{:<6}{:<6}{:<8}{size:<10}`\n", 0, 0, 0, 644);
    let names: Vec<&str> = members.iter().flat_map(|it| it.2.iter().copied()).collect();
    let index_len = 4 + 4 * names.len() + names.iter().map(|it| it.len() + 1).sum::<usize>();
    let mut offsets = Vec::new();
    let mut at = 8 + 60 + index_len.next_multiple_of(2);
    for (_, bytes, defines) in members {
        offsets.extend(defines.iter().map(|_| at as u32));
        at += 60 + bytes.len().next_multiple_of(2);
    }
    let mut out = b"!<arch>\n".to_vec();
    out.extend_from_slice(header("/", index_len).as_bytes());
    out.extend_from_slice(&(names.len() as u32).to_be_bytes());
    for offset in offsets {
        out.extend_from_slice(&offset.to_be_bytes());
    }
    for it in &names {
        out.extend_from_slice(it.as_bytes());
        out.push(0);
    }
    if out.len() % 2 != 0 {
        out.push(b'\n');
    }
    for (member, bytes, _) in members {
        out.extend_from_slice(header(&format!("{member}/"), bytes.len()).as_bytes());
        out.extend_from_slice(bytes);
        if out.len() % 2 != 0 {
            out.push(b'\n');
        }
    }
    out
}

/// An x86-64 ELF shared object with no sections: a library that exports nothing.
fn empty_shared() -> Vec<u8> {
    let mut out = vec![0u8; 64];
    out[..4].copy_from_slice(b"\x7fELF");
    out[4] = 2; // ELFCLASS64
    out[5] = 1; // ELFDATA2LSB
    out[6] = 1; // EV_CURRENT
    out[16..18].copy_from_slice(&3u16.to_le_bytes()); // ET_DYN
    out[18..20].copy_from_slice(&62u16.to_le_bytes()); // EM_X86_64
    out[20..24].copy_from_slice(&1u32.to_le_bytes());
    out[52..54].copy_from_slice(&64u16.to_le_bytes()); // e_ehsize
    out[54..56].copy_from_slice(&56u16.to_le_bytes()); // e_phentsize
    out[58..60].copy_from_slice(&64u16.to_le_bytes()); // e_shentsize
    out
}

const SHARED: NativeKind = NativeKind::Shared { as_needed: true };
const LAZY: NativeKind = NativeKind::Archive { whole: false };

/// Stages `_start` (calling `calls`, when given) with `natives`, each `(file, bytes, kind)` in
/// line order.
fn stage(output: Output, entry: &str, calls: Option<&str>, natives: &[(&str, &Arc<[u8]>, NativeKind)]) -> Result<(), LinkError> {
    let relocs = calls.map(|it| vec![rel32(1, Target::Extern(it.into()))]).unwrap_or_default();
    let code = if calls.is_some() { CALLER } else { returns(0) };
    let start = function(1, &code, relocs);
    let mut image = Image::new(name(super::TO), output).unwrap();
    let change = Change {
        added: vec![&start],
        names: vec![(entry.into(), Target::Direct(key(1)))],
        debug_names: vec![(key(1), "start".into())],
        natives: natives
            .iter()
            .enumerate()
            .map(|(at, (file, bytes, kind))| Native { id: id(100 + at as u8), name: file, bytes, kind: *kind, order: at as u32 })
            .collect(),
        ..Change::default()
    };
    image.stage(change).map(drop)
}

fn elf(natives: &[(&str, &Arc<[u8]>, NativeKind)]) -> Result<(), LinkError> {
    stage(output(false), "_start", None, natives)
}

const DEBIAN_GCC_S: &[u8] = b"/* GNU ld script\n   Use the shared library, but some functions are only in\n   the static library.  */\nGROUP ( libgcc_s.so.1 -lgcc )\n";

#[test]
fn debians_libgcc_s_script_finds_libgcc_on_the_line() {
    let script: Arc<[u8]> = DEBIAN_GCC_S.into();
    let so: Arc<[u8]> = empty_shared().into();
    let member = object(BinaryFormat::Elf, &["__popcountdi2"], "");
    let libgcc: Arc<[u8]> = archive(&[("_popcountsi2.o", &member, &["__popcountdi2"])]).into();
    let gcc_s = ("/usr/lib/gcc/x86_64-linux-gnu/12/libgcc_s.so", &script, SHARED);
    let so1 = ("/lib/x86_64-linux-gnu/libgcc_s.so.1", &so, SHARED);
    elf(&[gcc_s, so1, ("/usr/lib/gcc/x86_64-linux-gnu/12/libgcc.a", &libgcc, LAZY)]).unwrap();
    elf(&[gcc_s, so1, ("/usr/lib/gcc/x86_64-linux-gnu/12/libgcc.so", &so, SHARED)]).expect("a libgcc.so is -lgcc too");

    let error = elf(&[gcc_s, so1]).unwrap_err();
    for said in ["-lgcc", "libgcc_s.so", "libgcc.so or libgcc.a"] {
        assert!(error.what.contains(said), "{}", error.what);
    }
    let error = elf(&[gcc_s, so1, ("/usr/lib/gcc/x86_64-linux-gnu/12/libgcc_eh.a", &libgcc, LAZY)]).unwrap_err();
    assert!(error.what.contains("-lgcc"), "libgcc_eh.a is not -lgcc: {}", error.what);
}

#[test]
fn a_scripts_verbatim_library_is_the_file_it_names() {
    let script: Arc<[u8]> = b"INPUT ( -l:libfoo.so.2 )".as_slice().into();
    let so: Arc<[u8]> = empty_shared().into();
    elf(&[("/usr/lib/libfoo.so", &script, SHARED), ("/opt/lib/libfoo.so.2", &so, SHARED)]).unwrap();
    let error = elf(&[("/usr/lib/libfoo.so", &script, SHARED), ("/opt/lib/libfoo.so.3", &so, SHARED)]).unwrap_err();
    assert!(error.what.contains("-l:libfoo.so.2") && error.what.contains("has no libfoo.so.2"), "{}", error.what);
}

#[test]
fn glibcs_libc_script_is_checked_as_before() {
    let script: Arc<[u8]> = b"/* GNU ld script */\nOUTPUT_FORMAT(elf64-x86-64)\nGROUP ( /lib/x86_64-linux-gnu/libc.so.6 /usr/lib/x86_64-linux-gnu/libc_nonshared.a  AS_NEEDED ( /lib64/ld-linux-x86-64.so.2 ) )\n"
        .as_slice()
        .into();
    let so: Arc<[u8]> = empty_shared().into();
    let member = object(BinaryFormat::Elf, &["atexit"], "");
    let nonshared: Arc<[u8]> = archive(&[("atexit.oS", &member, &["atexit"])]).into();
    let script_at = "/usr/lib/x86_64-linux-gnu/libc.so";
    let all = [
        (script_at, &script, SHARED),
        ("/lib/x86_64-linux-gnu/libc.so.6", &so, SHARED),
        ("/usr/lib/x86_64-linux-gnu/libc_nonshared.a", &nonshared, LAZY),
        ("/lib64/ld-linux-x86-64.so.2", &so, SHARED),
    ];
    elf(&all).unwrap();
    let error = elf(&all[..3]).unwrap_err();
    assert!(error.what.contains("names /lib64/ld-linux-x86-64.so.2, which the link line does not have"), "{}", error.what);
}

fn windows() -> Output {
    Output {
        format: Format::Pe,
        arch: Arch::X86_64,
        kind: OutputKind::Executable { entry: "mainCRTStartup".into() },
        interpreter: None,
        platform: None,
        incremental: false,
        debug: Debug::None,
        live: None,
        tags: 1 << 32,
        windows_subsystem: false,
        dev_run: None,
        exec_log: false,
        triple: "x86_64-pc-windows-msvc".into(),
        stack: Some(1 << 20),
    }
}

/// `mainCRTStartup` calling `crt_helper`, which `msvcrt.lib`'s `crt.obj` defines and whose
/// `.drectve` is `directives`, with `more` after it on the line.
fn pe(directives: &str, more: &[(&str, &Arc<[u8]>, NativeKind)]) -> Result<(), LinkError> {
    let member = object(BinaryFormat::Coff, &["crt_helper"], directives);
    let unused = object(BinaryFormat::Coff, &["crt_unused"], "/DEFAULTLIB:never_taken");
    let msvcrt: Arc<[u8]> = archive(&[("crt.obj", &member, &["crt_helper"]), ("unused.obj", &unused, &["crt_unused"])]).into();
    let mut natives = vec![("C:\\VC\\lib\\x64\\msvcrt.lib", &msvcrt, LAZY)];
    natives.extend_from_slice(more);
    stage(windows(), "mainCRTStartup", Some("crt_helper"), &natives)
}

#[test]
fn a_members_default_library_is_taken_from_the_line() {
    let empty: Arc<[u8]> = b"!<arch>\n".as_slice().into();
    let vcruntime = ("C:\\VC\\lib\\x64\\VCRUNTIME.lib", &empty, LAZY);
    let ucrt = ("C:\\Kits\\ucrt\\x64\\ucrt.lib", &empty, LAZY);
    pe(" /DEFAULTLIB:vcruntime /DEFAULTLIB:\"ucrt.lib\"", &[vcruntime, ucrt]).unwrap();

    let error = pe(" /DEFAULTLIB:vcruntime /DEFAULTLIB:\"ucrt.lib\"", &[vcruntime]).unwrap_err();
    for said in ["ucrt.lib", "(/DEFAULTLIB)", "msvcrt.lib(crt.obj)"] {
        assert!(error.what.contains(said), "{}", error.what);
    }
    assert!(!error.what.contains("never_taken"), "a member not taken asks for nothing: {}", error.what);
}

#[test]
fn nodefaultlib_takes_a_default_library_out() {
    let empty: Arc<[u8]> = b"!<arch>\n".as_slice().into();
    let vcruntime = ("C:\\VC\\lib\\x64\\vcruntime.lib", &empty, LAZY);
    pe(" /DEFAULTLIB:vcruntime /DEFAULTLIB:libcmt /NODEFAULTLIB:LIBCMT.lib", &[vcruntime]).unwrap();
    pe(" /DEFAULTLIB:vcruntime /DEFAULTLIB:libcmt /NODEFAULTLIB", &[]).unwrap();

    let other: Arc<[u8]> = object(BinaryFormat::Coff, &["other_fn"], "/NODEFAULTLIB:libcmt").into();
    pe(" /DEFAULTLIB:vcruntime /DEFAULTLIB:libcmt", &[vcruntime, ("other.obj", &other, NativeKind::Object)])
        .expect("another loaded object's /NODEFAULTLIB takes it out of the whole link");
    let error = pe(" /DEFAULTLIB:vcruntime /DEFAULTLIB:libcmt", &[vcruntime]).unwrap_err();
    assert!(error.what.contains("libcmt.lib") && error.what.contains("msvcrt.lib(crt.obj)"), "{}", error.what);
}
