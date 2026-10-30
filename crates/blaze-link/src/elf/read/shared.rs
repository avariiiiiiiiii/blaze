//! Shared objects (`ET_DYN`): what an image may import from them.

use object::elf;
use object::read::elf::{Dyn as _, FileHeader as _, SectionHeader as _, Sym as _};

use super::{ENDIAN, Header};
use crate::model::{Names, Shared, SharedSymbol};

/// Reads the shared object `data`, whose header [`super::read`] checked.
pub(super) fn read(name: &str, data: &[u8]) -> Result<Shared, String> {
    let error = |error: object::read::Error| format!("{name}: {error}");
    let full = super::full(name);
    let header = Header::parse(data).map_err(error)?;
    let sections = header.sections(ENDIAN, data).map_err(error)?;
    let dynsym = sections.symbols(ENDIAN, data, elf::SHT_DYNSYM).map_err(error)?;
    let table = sections.versions(ENDIAN, data).map_err(error)?;

    let mut soname = None;
    if let Some((entries, strings)) = sections.dynamic(ENDIAN, data).map_err(error)? {
        let strings = sections.strings(ENDIAN, data, strings).map_err(error)?;
        for entry in entries {
            if entry.d_tag(ENDIAN) != u64::from(elf::DT_SONAME) {
                continue;
            }
            let bytes = u32::try_from(entry.d_val(ENDIAN))
                .ok()
                .and_then(|offset| strings.get(offset).ok())
                .ok_or_else(|| format!("{name}: DT_SONAME is past the dynamic string table"))?;
            soname = Some(super::utf8(name, "DT_SONAME", bytes)?.to_owned());
        }
    }

    // The names are `.dynstr`'s: it sizes the arena.
    let dynstr = sections.section(dynsym.string_section()).map_or(0, |it| it.sh_size(ENDIAN));
    let mut names = Names::with_capacity(dynstr.min(data.len() as u64) as usize);
    let mut symbols = Vec::with_capacity(dynsym.len());
    let mut versions: Vec<String> = Vec::new();
    // By ELF version index (at most 0x7fff): its slot in `versions` once seen, `None` inside
    // for the local and global indexes, which name no version.
    let mut slots: Vec<Option<Option<u16>>> = Vec::new();
    for (index, symbol) in dynsym.enumerate() {
        if index.0 == 0 || symbol.st_shndx(ENDIAN) == elf::SHN_UNDEF {
            continue;
        }
        let weak = match symbol.st_bind() {
            elf::STB_GLOBAL | elf::STB_GNU_UNIQUE => false,
            elf::STB_WEAK => true,
            _ => continue,
        };
        let symbol_name =
            super::utf8(name, "the symbol name", dynsym.symbol_name(ENDIAN, symbol).map_err(error)?)?;
        let version = match &table {
            None => None,
            Some(table) => {
                let version = table.version_index(ENDIAN, index);
                if version.is_hidden() {
                    continue;
                }
                let at = usize::from(version.index());
                if slots.len() <= at {
                    slots.resize(at + 1, None);
                }
                match slots[at] {
                    Some(slot) => slot,
                    None => {
                        let found = table.version(version).map_err(|error| {
                            format!("{name}: symbol {symbol_name}: {error}")
                        })?;
                        let slot = match found {
                            None => None,
                            Some(found) => {
                                let found = super::utf8(name, "the version name", found.name())?;
                                // Two indexes may name one version (a definition and a need).
                                Some(match versions.iter().position(|it| it == found) {
                                    Some(slot) => slot as u16,
                                    None => {
                                        let slot = u16::try_from(versions.len()).map_err(|_| {
                                            format!("{name}: more than 65,536 versions")
                                        })?;
                                        versions.push(found.to_owned());
                                        slot
                                    }
                                })
                            }
                        };
                        slots[at] = Some(slot);
                        slot
                    }
                }
            }
        };
        symbols.push(SharedSymbol {
            name: names.push(symbol_name).map_err(full)?,
            version,
            kind: super::kind(symbol.st_type()),
            weak,
        });
    }

    Ok(Shared {
        name: soname.unwrap_or_else(|| super::file_name(name)),
        symbols,
        names,
        versions,
        more: Vec::new(),
        inputs: Vec::new(),
    })
}
