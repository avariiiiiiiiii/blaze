//! Shared objects (`ET_DYN`): what an image may import from them.

use object::elf;
use object::read::elf::{Dyn as _, FileHeader as _, Sym as _};

use super::{ENDIAN, Header};
use crate::model::{Shared, SharedSymbol};

/// Reads the shared object `data`, whose header [`super::read`] checked.
pub(super) fn read(name: &str, data: &[u8]) -> Result<Shared, String> {
    let error = |error: object::read::Error| format!("{name}: {error}");
    let header = Header::parse(data).map_err(error)?;
    let sections = header.sections(ENDIAN, data).map_err(error)?;
    let dynsym = sections.symbols(ENDIAN, data, elf::SHT_DYNSYM).map_err(error)?;
    let versions = sections.versions(ENDIAN, data).map_err(error)?;

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
            soname = Some(super::utf8(name, "DT_SONAME", bytes)?);
        }
    }

    let mut symbols = Vec::new();
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
        let version = match &versions {
            None => None,
            Some(versions) => {
                let version = versions.version_index(ENDIAN, index);
                if version.is_hidden() {
                    continue;
                }
                match versions.version(version).map_err(|error| {
                    format!("{name}: symbol {symbol_name}: {error}")
                })? {
                    None => None,
                    Some(version) => Some(super::utf8(name, "the version name", version.name())?),
                }
            }
        };
        symbols.push(SharedSymbol {
            name: symbol_name,
            version,
            kind: super::kind(symbol.st_type()),
            weak,
        });
    }

    Ok(Shared {
        name: soname.unwrap_or_else(|| super::file_name(name)),
        symbols,
        more: Vec::new(),
        inputs: Vec::new(),
    })
}
