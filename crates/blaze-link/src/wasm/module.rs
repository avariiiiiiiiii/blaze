//! Writes the module of a placed image, as wasm-ld lays one out: types, imports, functions,
//! the table, memory, globals, exports, the element segment, code, data, the custom sections,
//! and the `name` section.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use blaze_codegen::wasm::{Code, Import, Signature};

use super::leb::{self, PADDED, padded_uleb};
use super::reader::{MAGIC, Reader};
use super::{FUNCTIONS, HEAP, PAGE, STACK_POINTER, Wasm, heap_base, in_memory};
use crate::model::{Class, Placed, SymbolKind};
use crate::output::OutputKind;

mod id {
    pub(super) const TYPE: u8 = 1;
    pub(super) const IMPORT: u8 = 2;
    pub(super) const FUNCTION: u8 = 3;
    pub(super) const TABLE: u8 = 4;
    pub(super) const MEMORY: u8 = 5;
    pub(super) const GLOBAL: u8 = 6;
    pub(super) const EXPORT: u8 = 7;
    pub(super) const ELEMENT: u8 = 9;
    pub(super) const CODE: u8 = 10;
    pub(super) const DATA: u8 = 11;
}

const FUNCREF: u8 = 0x70;
const I32: u8 = 0x7f;
const EXPORT_FUNCTION: u8 = 0;
const EXPORT_MEMORY: u8 = 2;
const EXPORT_GLOBAL: u8 = 3;

/// A function the module defines.
enum Body<'a> {
    Piece { code: Code, body: &'a [u8], name: Option<&'a str> },
    /// `__wasm_call_ctors`, which calls these.
    Ctors(Vec<u32>),
}

/// The functions `__wasm_call_ctors` calls, in order: the slots in the init array, each a
/// function's index + 1.
pub(super) fn init_functions(placed: &Placed<'_>) -> Result<Vec<u32>, String> {
    let mut functions = Vec::new();
    for (index, _) in placed.regions.iter().enumerate().filter(|(_, it)| it.class == Class::InitArray)
    {
        let bytes = placed.bytes(index);
        if bytes.len() % 4 != 0 {
            return Err(format!("an init array of {} bytes, not of 4-byte slots", bytes.len()));
        }
        for slot in bytes.chunks_exact(4) {
            let slot = u32::from_le_bytes([slot[0], slot[1], slot[2], slot[3]]);
            let function = slot
                .checked_sub(1)
                .ok_or("an init function that is an undefined weak function")?;
            functions.push(function);
        }
    }
    Ok(functions)
}

fn ctors_body(functions: &[u32]) -> Vec<u8> {
    let mut body = vec![0];
    for &function in functions {
        body.push(0x10);
        leb::uleb(&mut body, function.into());
    }
    body.push(0x0b);
    body
}

/// The functions the module defines, in index order after its imports, and the index of each
/// by its address in the code space: the core numbers them so, the imports first, then the
/// code by address.
fn functions<'a>(
    placed: &'a Placed<'a>,
    imports: u32,
) -> Result<(Vec<Body<'a>>, HashMap<u64, u32>), String> {
    let mut units: BTreeMap<u64, Body<'a>> = BTreeMap::new();
    let mut put = |addr: u64, body: Body<'a>| match units.insert(addr, body) {
        None => Ok(()),
        Some(_) => Err(format!("two functions at {addr} in the code space")),
    };
    let mut taken = 0;
    for (index, region) in placed.regions.iter().enumerate() {
        match &region.class {
            Class::Text | Class::Patch => {
                let bytes = placed.bytes(index);
                let mut covered = vec![false; bytes.len()];
                let end = region.addr + region.size;
                for function in placed.functions.iter().filter(|it| (region.addr..end).contains(&it.addr)) {
                    let name = function.name.as_deref();
                    let at = (function.addr - region.addr) as usize;
                    let piece = usize::try_from(function.size)
                        .ok()
                        .and_then(|size| bytes.get(at..at.checked_add(size)?))
                        .ok_or_else(|| {
                            format!("the function at {} reaches past its region's bytes", function.addr)
                        })?;
                    covered[at..at + piece.len()].fill(true);
                    let (code, start) = Code::decode(piece)
                        .map_err(|e| format!("the function {}: {e}", name.unwrap_or("?")))?;
                    put(function.addr, Body::Piece { code, body: &piece[start..], name })?;
                    taken += 1;
                }
                if let Some(at) = (0..bytes.len()).find(|&at| !covered[at] && bytes[at] != 0) {
                    return Err(format!(
                        "code at {} of the {:?} region that is no function of the image's",
                        region.addr + at as u64,
                        region.class
                    ));
                }
            }
            Class::Synthetic(name) if *name == FUNCTIONS => {
                put(region.addr, Body::Ctors(init_functions(placed)?))?;
            }
            _ => {}
        }
    }
    if taken != placed.functions.len() {
        return Err(format!(
            "{} of the image's {} functions are in no region of code",
            placed.functions.len() - taken,
            placed.functions.len()
        ));
    }
    let mut index = HashMap::with_capacity(units.len());
    let mut defined = Vec::with_capacity(units.len());
    for (at, (addr, body)) in units.into_iter().enumerate() {
        let function = u32::try_from(at)
            .ok()
            .and_then(|at| at.checked_add(imports))
            .ok_or("more than 2^32 functions")?;
        index.insert(addr, function);
        defined.push(body);
    }
    Ok((defined, index))
}

/// What a placed image can't hold and still be a wasm module.
fn check(placed: &Placed<'_>) -> Result<(), String> {
    if let Some((name, _)) = placed.debug.first() {
        return Err(format!(
            "the debug section {name}: a wasm module's debug info is its name section, and \
             DWARF of function indices would be wrong"
        ));
    }
    if let Some(cell) = placed.got.first() {
        return Err(format!("a GOT cell at {:#x}: a wasm32 module has no GOT", cell.addr));
    }
    if let Some(bind) = placed.binds.first() {
        return Err(format!("a bind at {:#x}: a wasm module is never bound", bind.addr));
    }
    if let Some(addr) = placed.rebase.first() {
        return Err(format!("a rebase at {addr:#x}: a wasm32 module is never slid"));
    }
    for region in placed.regions {
        let unplaceable = match &region.class {
            Class::Plt
            | Class::Stubs
            | Class::Veneers
            | Class::EhFrame
            | Class::Table
            | Class::Got
            | Class::FiniArray
            | Class::ThreadVars => true,
            Class::Named { exec, .. } => *exec,
            Class::Synthetic(name) => *name != FUNCTIONS && *name != HEAP,
            _ => false,
        };
        if unplaceable && region.size > 0 {
            return Err(format!(
                "a {:?} region of {} at {:#x}, which a wasm module has no place for",
                region.class, region.size, region.addr
            ));
        }
    }
    Ok(())
}

#[derive(Default)]
struct Types {
    list: Vec<Signature>,
    index: HashMap<Signature, u32>,
}

impl Types {
    fn of(&mut self, signature: &Signature) -> u32 {
        if let Some(&index) = self.index.get(signature) {
            return index;
        }
        let index = self.list.len() as u32;
        self.list.push(signature.clone());
        self.index.insert(signature.clone(), index);
        index
    }
}

/// What each import is imported as: what the core says, or what a code piece that calls it
/// says.
fn imports(placed: &Placed<'_>, defined: &[Body<'_>]) -> Result<Vec<Import>, String> {
    let mut said: HashMap<&str, &Import> = HashMap::new();
    for body in defined {
        if let Body::Piece { code, .. } = body {
            for (name, import) in &code.imports {
                said.entry(name.as_str()).or_insert(import);
            }
        }
    }
    placed
        .imports
        .iter()
        .map(|import| {
            if import.kind != SymbolKind::Function {
                return Err(format!(
                    "the import `{}` is {:?}: a wasm32 module imports only functions",
                    import.name, import.kind
                ));
            }
            match (&import.wasm, said.get(import.name.as_str())) {
                (Some(wasm), _) => Ok(wasm.clone()),
                (None, Some(&wasm)) => Ok(wasm.clone()),
                (None, None) => Err(format!(
                    "the import `{}`: nothing that refers to it says its module, field and type",
                    import.name
                )),
            }
        })
        .collect()
}

/// The type of function `index`: of an import, or of a code piece.
fn signature<'s>(index: u32, imports: &'s [Import], defined: &'s [Body<'_>]) -> Option<&'s Signature> {
    match (index as usize).checked_sub(imports.len()) {
        None => imports.get(index as usize).map(|it| &it.ty),
        Some(at) => match defined.get(at)? {
            Body::Piece { code, .. } => Some(&code.ty),
            Body::Ctors(_) => None,
        },
    }
}

/// A piece's body with its type indices and the stack pointer's index filled in.
fn fill(code: &Code, body: &[u8], types: &mut Types) -> Result<Vec<u8>, String> {
    let mut body = body.to_vec();
    let len = body.len();
    let mut write = |at: u32, value: u32| -> Result<(), String> {
        let place = (at as usize)
            .checked_add(PADDED)
            .and_then(|end| body.get_mut(at as usize..end))
            .ok_or_else(|| format!("a fixup at {at} of a body of {len} bytes"))?;
        place.copy_from_slice(&padded_uleb(value));
        Ok(())
    };
    for &(at, which) in &code.type_fixups {
        let ty = code.types.get(which as usize).ok_or_else(|| format!("no type {which}"))?;
        write(at, types.of(ty))?;
    }
    for &at in &code.stack_pointer {
        write(at, STACK_POINTER)?;
    }
    Ok(body)
}

/// The producers sections of the objects, one after the other, as one: each field's values
/// by the first version of each name, the fields in the tool conventions' order.
fn producers(payloads: &[u8]) -> Result<Vec<u8>, String> {
    let mut fields: Vec<(&str, Vec<(&str, &str)>)> = Vec::new();
    let mut r = Reader::new(payloads);
    while !r.done() {
        for _ in 0..r.count()? {
            let field = r.name()?;
            let at = match fields.iter().position(|(it, _)| *it == field) {
                Some(at) => at,
                None => {
                    fields.push((field, Vec::new()));
                    fields.len() - 1
                }
            };
            for _ in 0..r.count()? {
                let name = r.name()?;
                let version = r.name()?;
                let values = &mut fields[at].1;
                if !values.iter().any(|(it, _)| *it == name) {
                    values.push((name, version));
                }
            }
        }
    }
    const ORDER: [&str; 3] = ["language", "processed-by", "sdk"];
    fields.sort_by_key(|(field, _)| ORDER.iter().position(|it| it == field).unwrap_or(ORDER.len()));
    let mut out = Vec::new();
    leb::uleb(&mut out, fields.len() as u64);
    for (field, values) in fields {
        leb::name(&mut out, field);
        leb::uleb(&mut out, values.len() as u64);
        for (name, version) in values {
            leb::name(&mut out, name);
            leb::name(&mut out, version);
        }
    }
    Ok(out)
}

/// The target_features sections of the objects as one: every feature one uses, an error when
/// another disallows it.
fn target_features(payloads: &[u8]) -> Result<Vec<u8>, String> {
    let mut used = BTreeSet::new();
    let mut disallowed = BTreeSet::new();
    let mut r = Reader::new(payloads);
    while !r.done() {
        for _ in 0..r.count()? {
            let prefix = r.u8()?;
            let feature = r.name()?;
            match prefix {
                b'+' | b'=' => used.insert(feature),
                b'-' => disallowed.insert(feature),
                other => return Err(format!("the target feature prefix {other:#04x}")),
            };
        }
    }
    if let Some(feature) = used.intersection(&disallowed).next() {
        return Err(format!("the target feature `{feature}` is used by one object and disallowed by another"));
    }
    let mut out = Vec::new();
    leb::uleb(&mut out, used.len() as u64);
    for feature in used {
        out.push(b'+');
        leb::name(&mut out, feature);
    }
    Ok(out)
}

/// A name map of the `name` section: an index and a name each, by index.
fn name_map(out: &mut Vec<u8>, id: u8, names: &BTreeMap<u32, &str>) {
    if names.is_empty() {
        return;
    }
    let mut contents = Vec::new();
    leb::uleb(&mut contents, names.len() as u64);
    for (&index, name) in names {
        leb::uleb(&mut contents, index.into());
        leb::name(&mut contents, name);
    }
    out.push(id);
    leb::uleb(out, contents.len() as u64);
    out.extend(contents);
}

fn segment_name(class: &Class) -> &str {
    match class {
        Class::Rodata => ".rodata",
        Class::RelRo => ".data.rel.ro",
        Class::Tdata => ".tdata",
        Class::PatchData => ".data.patch",
        Class::Named { name, .. } => name,
        _ => ".data",
    }
}

/// The module of a placed image.
pub(super) fn write(wasm: &Wasm, placed: &Placed<'_>) -> Result<Vec<u8>, String> {
    check(placed)?;
    let imported = u32::try_from(placed.imports.len()).map_err(|_| "more than 2^32 imports")?;
    let (defined, index_of) = functions(placed, imported)?;
    let count = u64::from(imported) + defined.len() as u64;
    let index_at = |addr: u64, what: &str| {
        index_of.get(&addr).copied().ok_or_else(|| {
            format!("{what} at {addr} of the code space, where no function of the module starts")
        })
    };
    let imports = imports(placed, &defined)?;

    let mut types = Types::default();
    let mut import_section = Vec::new();
    leb::uleb(&mut import_section, imports.len() as u64);
    for import in &imports {
        leb::name(&mut import_section, &import.module);
        leb::name(&mut import_section, &import.field);
        import_section.push(0);
        leb::uleb(&mut import_section, types.of(&import.ty).into());
    }
    let nothing = Signature::default();
    let mut function_section = Vec::new();
    let mut code_section = Vec::new();
    leb::uleb(&mut function_section, defined.len() as u64);
    leb::uleb(&mut code_section, defined.len() as u64);
    for body in &defined {
        let (ty, bytes) = match body {
            Body::Piece { code, body, .. } => {
                let ty = types.of(&code.ty);
                (ty, fill(code, body, &mut types)?)
            }
            Body::Ctors(functions) => {
                for &function in functions {
                    match signature(function, &imports, &defined) {
                        Some(ty) if *ty == nothing => {}
                        _ => {
                            return Err(format!(
                                "the init function {function} takes or returns values (it \
                                 must be `fn()`), or is no function of the program's"
                            ));
                        }
                    }
                }
                (types.of(&nothing), ctors_body(functions))
            }
        };
        leb::uleb(&mut function_section, ty.into());
        leb::uleb(&mut code_section, bytes.len() as u64);
        code_section.extend(bytes);
    }

    // Memory: the stack, the data, then the heap from `__heap_base`.
    if placed.region(&Class::Synthetic(HEAP)).is_none() {
        return Err("no heap region: the writer's synthetic regions were not placed".to_owned());
    }
    let heap = heap_base(placed.regions);
    if heap > 1 << 32 {
        return Err(format!("data up to {heap:#x}, past 32-bit memory"));
    }
    let pages = heap.div_ceil(PAGE).max(1);
    let mut segments = Vec::new();
    for (index, region) in placed.regions.iter().enumerate() {
        if !in_memory(&region.class) {
            continue;
        }
        if region.addr < wasm.stack {
            return Err(format!(
                "the {:?} region at {:#x}, in the stack below {:#x}",
                region.class, region.addr, wasm.stack
            ));
        }
        if region.addr + region.size > heap {
            return Err(format!(
                "the {:?} region at {:#x} reaches past the heap's start {heap:#x}",
                region.class, region.addr
            ));
        }
        let bytes = placed.bytes(index);
        if bytes.len() as u64 > region.size {
            return Err(format!("the {:?} region has more bytes than its size", region.class));
        }
        if bytes.iter().all(|&it| it == 0) {
            continue;
        }
        segments.push((segment_name(&region.class), region.addr, bytes));
    }

    // Globals and exports.
    let mut globals: Vec<(&str, bool, u32)> = vec![("__stack_pointer", true, wasm.stack as u32)];
    let mut exports: Vec<(&str, u8, u32)> = vec![("memory", EXPORT_MEMORY, 0)];
    fn add<'n>(
        exports: &mut Vec<(&'n str, u8, u32)>,
        name: &'n str,
        kind: u8,
        index: u32,
    ) -> Result<(), String> {
        match exports.iter().find(|(it, ..)| *it == name) {
            Some(&(_, k, i)) if (k, i) == (kind, index) => Ok(()),
            Some(_) => Err(format!("two different things exported as `{name}`")),
            None => {
                exports.push((name, kind, index));
                Ok(())
            }
        }
    }
    if let OutputKind::Executable { entry } = &wasm.kind {
        let addr = placed.entry.ok_or_else(|| format!("the entry `{entry}` is not defined"))?;
        let function = index_at(addr, &format!("the entry `{entry}`"))?;
        add(&mut exports, entry, EXPORT_FUNCTION, function)?;
    }
    for it in placed.exports {
        match it.kind {
            SymbolKind::Function => {
                let function = index_at(it.addr, &format!("the export `{}`", it.name))?;
                add(&mut exports, &it.name, EXPORT_FUNCTION, function)?
            }
            SymbolKind::Data | SymbolKind::Other => {
                if exports.iter().any(|(name, ..)| *name == it.name) {
                    return Err(format!("`{}` exported twice", it.name));
                }
                let value = u32::try_from(it.addr)
                    .map_err(|_| format!("`{}` at {:#x}, past 32-bit memory", it.name, it.addr))?;
                add(&mut exports, &it.name, EXPORT_GLOBAL, globals.len() as u32)?;
                globals.push((it.name.as_str(), false, value));
            }
            SymbolKind::Tls | SymbolKind::Section => {
                return Err(format!("`{}` is exported and is {:?}", it.name, it.kind));
            }
        }
    }
    for (at, body) in defined.iter().enumerate() {
        if let Body::Piece { code: Code { export: Some(name), .. }, .. } = body {
            add(&mut exports, name, EXPORT_FUNCTION, imported + at as u32)?;
        }
    }

    // Custom sections, by name in address order.
    let mut customs: Vec<(&str, Vec<u8>)> = Vec::new();
    let mut by_addr: Vec<(u64, usize)> = placed
        .regions
        .iter()
        .enumerate()
        .filter(|(_, it)| matches!(it.class, Class::Named { write: false, exec: false, .. }))
        .map(|(index, it)| (it.addr, index))
        .collect();
    by_addr.sort_unstable();
    for (_, index) in by_addr {
        let Class::Named { name, .. } = &placed.regions[index].class else { continue };
        let bytes = placed.bytes(index);
        match customs.iter_mut().find(|(it, _)| *it == name.as_str()) {
            Some((_, contents)) => contents.extend(bytes),
            None => customs.push((name.as_str(), bytes.to_vec())),
        }
    }
    let mut last = Vec::new();
    customs.retain(|(name, contents)| match *name {
        "producers" | "target_features" => {
            last.push((*name, contents.clone()));
            false
        }
        _ => true,
    });
    last.sort_by_key(|(name, _)| *name == "target_features");

    // The module.
    let mut out = MAGIC.to_vec();
    let mut type_section = Vec::new();
    leb::uleb(&mut type_section, types.list.len() as u64);
    for ty in &types.list {
        ty.encode(&mut type_section);
    }
    leb::section(&mut out, id::TYPE, &type_section);
    if !imports.is_empty() {
        leb::section(&mut out, id::IMPORT, &import_section);
    }
    leb::section(&mut out, id::FUNCTION, &function_section);
    let mut table = vec![1, FUNCREF, 1];
    leb::uleb(&mut table, count + 1);
    leb::uleb(&mut table, count + 1);
    leb::section(&mut out, id::TABLE, &table);
    let mut memory = vec![1, 0];
    leb::uleb(&mut memory, pages);
    leb::section(&mut out, id::MEMORY, &memory);
    let mut global_section = Vec::new();
    leb::uleb(&mut global_section, globals.len() as u64);
    for &(_, mutable, value) in &globals {
        global_section.extend([I32, u8::from(mutable)]);
        leb::i32_const(&mut global_section, value as i32);
    }
    leb::section(&mut out, id::GLOBAL, &global_section);
    let mut export_section = Vec::new();
    leb::uleb(&mut export_section, exports.len() as u64);
    for &(name, kind, index) in &exports {
        leb::name(&mut export_section, name);
        export_section.push(kind);
        leb::uleb(&mut export_section, index.into());
    }
    leb::section(&mut out, id::EXPORT, &export_section);
    if count > 0 {
        // One active segment of table 0 (flags 0) putting every function at its slot, its
        // index + 1: what a function pointer is.
        let mut element = vec![1, 0];
        leb::i32_const(&mut element, 1);
        leb::uleb(&mut element, count);
        for index in 0..count {
            leb::uleb(&mut element, index);
        }
        leb::section(&mut out, id::ELEMENT, &element);
    }
    leb::section(&mut out, id::CODE, &code_section);
    if !segments.is_empty() {
        let mut data = Vec::new();
        leb::uleb(&mut data, segments.len() as u64);
        for &(_, addr, bytes) in &segments {
            data.push(0);
            leb::i32_const(&mut data, addr as u32 as i32);
            leb::uleb(&mut data, bytes.len() as u64);
            data.extend(bytes);
        }
        leb::section(&mut out, id::DATA, &data);
    }
    for (name, contents) in &customs {
        leb::custom(&mut out, name, contents);
    }

    let mut names = Vec::new();
    let mut function_names: BTreeMap<u32, &str> = BTreeMap::new();
    for (index, import) in placed.imports.iter().enumerate() {
        function_names.insert(index as u32, import.name.as_str());
    }
    for (at, body) in defined.iter().enumerate() {
        let name = match body {
            Body::Piece { name, .. } => *name,
            Body::Ctors(_) => Some("__wasm_call_ctors"),
        };
        if let Some(name) = name {
            function_names.insert(imported + at as u32, name);
        }
    }
    name_map(&mut names, 1, &function_names);
    let global_names: BTreeMap<u32, &str> =
        globals.iter().enumerate().map(|(at, it)| (at as u32, it.0)).collect();
    name_map(&mut names, 7, &global_names);
    let segment_names: BTreeMap<u32, &str> =
        segments.iter().enumerate().map(|(at, it)| (at as u32, it.0)).collect();
    name_map(&mut names, 9, &segment_names);
    leb::custom(&mut out, "name", &names);

    for (name, contents) in last {
        let merged = match name {
            "producers" => producers(&contents),
            _ => target_features(&contents),
        };
        leb::custom(&mut out, name, &merged.map_err(|e| format!("the {name} sections: {e}"))?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use blaze_codegen::wasm::ValType;

    use super::*;
    use crate::model::{self, Export, Function, GotEntry, Holds, Plan, Prot, Region, Writer};
    use crate::output::{Debug, Format, Output};
    use crate::wasm::leb::padded_sleb;
    use crate::wasm::reader;
    use crate::wasm::{STACK, custom};

    const DATA: u64 = STACK;

    fn sig(params: &[ValType], results: &[ValType]) -> Signature {
        Signature { params: params.to_vec(), results: results.to_vec() }
    }

    fn fd_write() -> Import {
        Import {
            module: "wasi_snapshot_preview1".to_owned(),
            field: "fd_write".to_owned(),
            ty: sig(&[ValType::I32; 4], &[ValType::I32]),
        }
    }

    /// `_start`'s body: the stack pointer at 2, a call of `__wasm_call_ctors` (index 1, as the
    /// core wrote it), the address of the data and `init`'s slot (3), and an indirect call
    /// whose type is at 27.
    fn start_body() -> Vec<u8> {
        let mut body = vec![0x00, 0x23];
        body.extend(padded_uleb(0));
        body.extend([0x1a, 0x10]);
        body.extend(padded_uleb(1));
        body.push(0x41);
        body.extend(padded_sleb(DATA as i32));
        body.push(0x41);
        body.extend(padded_sleb(3));
        body.push(0x11);
        body.extend(padded_uleb(0));
        body.extend([0x00, 0x0b]);
        assert_eq!(body.len(), 34);
        body
    }

    fn piece(code: &Code, body: &[u8]) -> Vec<u8> {
        code.encode(body).0
    }

    /// A placed image as the core hands it to the writer.
    struct Image {
        output: Output,
        plan: Plan,
        regions: Vec<Region>,
        contents: Vec<Vec<u8>>,
        imports: Vec<model::Import>,
        exports: Vec<Export>,
        functions: Vec<Function>,
        entry: Option<u64>,
        got: Vec<GotEntry>,
        debug: Vec<(&'static str, Vec<u8>)>,
    }

    impl Image {
        fn with<R>(&self, then: impl FnOnce(&Placed<'_>) -> R) -> R {
            let contents: Vec<&[u8]> = self.contents.iter().map(Vec::as_slice).collect();
            then(&Placed {
                output: &self.output,
                name: "app.wasm",
                plan: &self.plan,
                regions: &self.regions,
                contents: &contents,
                libraries: &[],
                imports: &self.imports,
                got: &self.got,
                rebase: &[],
                binds: &[],
                exports: &self.exports,
                functions: &self.functions,
                entry: self.entry,
                init: None,
                fini: None,
                tls: None,
                id: [0; 16],
                debug: &self.debug,
            })
        }

        fn writer(&self) -> Box<dyn Writer> {
            super::super::writer(&self.output).unwrap()
        }

        /// The module, written as the core asks for it: synthesized, then the file.
        fn module(&mut self) -> Result<Vec<u8>, String> {
            let writer = self.writer();
            let synthetic = self.with(|placed| writer.synthesize(placed)).map_err(|e| e.what)?;
            for (class, bytes) in synthetic {
                let Some(at) = self.regions.iter().position(|it| it.class == class) else {
                    return Err(format!("the writer filled {class:?}, which has no region"));
                };
                assert!(bytes.len() as u64 <= self.regions[at].capacity, "{class:?} outgrew its room");
                self.regions[at].size = bytes.len() as u64;
                self.contents[at] = bytes;
            }
            self.with(|placed| {
                for region in placed.regions {
                    assert_eq!(writer.offset(placed, region), None, "{:?}", region.class);
                }
            });
            let file = self.with(|placed| writer.file(placed)).map_err(|e| e.what)?;
            assert_eq!(file.parts.len(), 1);
            assert_eq!(file.parts[0].0, 0);
            assert_eq!(file.len, file.parts[0].1.len() as u64);
            Ok(file.parts.into_iter().next().unwrap().1)
        }
    }

    fn region(writer: &dyn Writer, class: Class, addr: u64, size: u64) -> Region {
        let prot = writer.prot(&class);
        Region { class, addr, capacity: size, size, prot, align: 1 }
    }

    /// A command: `fd_write` imported; `__wasm_call_ctors`, `init` (an init function) and
    /// `_start` in the code space from 0; `.rodata`, `.data` (`init`'s slot) and `.bss` after
    /// the stack; the heap; the init array and two custom sections after it.
    fn command() -> Image {
        let output = Output {
            format: Format::Wasm,
            arch: blaze_codegen::Arch::Wasm32,
            kind: OutputKind::Executable { entry: "_start".to_owned() },
            interpreter: None,
            platform: None,
            incremental: false,
            debug: Debug::None,
            live: None,
            tags: 0,
            stack: None,
        };
        let writer = super::super::writer(&output).unwrap();
        let init = piece(&Code { ty: sig(&[], &[]), ..Code::default() }, &[0x00, 0x0b]);
        let start = piece(
            &Code {
                ty: sig(&[], &[]),
                types: vec![sig(&[ValType::I32], &[])],
                type_fixups: vec![(27, 0)],
                stack_pointer: vec![2],
                imports: vec![("fd_write".to_owned(), fd_write())],
                export: Some("_start".to_owned()),
            },
            &start_body(),
        );
        let (init_at, start_at) = (1, 1 + init.len() as u64);
        let text = [init.clone(), start.clone()].concat();
        let mut producers = Vec::new();
        for (field, name, version) in [("processed-by", "clang", "19"), ("language", "C11", "")] {
            leb::uleb(&mut producers, 1);
            leb::name(&mut producers, field);
            leb::uleb(&mut producers, 1);
            leb::name(&mut producers, name);
            leb::name(&mut producers, version);
        }
        let regions = vec![
            region(&*writer, Class::Synthetic(FUNCTIONS), 0, 1),
            region(&*writer, Class::Text, 1, text.len() as u64),
            region(&*writer, Class::Rodata, DATA, 5),
            region(&*writer, Class::Data, DATA + 8, 4),
            region(&*writer, Class::Bss, DATA + 12, 4),
            region(&*writer, Class::Synthetic(HEAP), DATA + 16, 0),
            region(&*writer, Class::InitArray, DATA + 16, 4),
            region(&*writer, custom("my_section"), DATA + 20, 3),
            region(&*writer, custom("producers"), DATA + 23, producers.len() as u64),
        ];
        let contents = vec![
            vec![0],
            text,
            b"hello".to_vec(),
            vec![3, 0, 0, 0],
            Vec::new(),
            Vec::new(),
            vec![3, 0, 0, 0],
            vec![1, 2, 3],
            producers,
        ];
        let function = |addr, piece: &Vec<u8>, name: &str| Function {
            addr,
            size: piece.len() as u64,
            fde: None,
            windows: None,
            name: Some(name.to_owned()),
        };
        Image {
            plan: writer.plan(),
            output,
            regions,
            contents,
            imports: vec![model::Import {
                name: "fd_write".to_owned(),
                library: None,
                version: None,
                kind: SymbolKind::Function,
                weak: false,
                wasm: None,
            }],
            exports: Vec::new(),
            functions: vec![function(init_at, &init, "init"), function(start_at, &start, "_start")],
            entry: Some(start_at),
            got: Vec::new(),
            debug: Vec::new(),
        }
    }

    /// The module's sections: each id, a custom one's name, and its contents.
    fn sections(module: &[u8]) -> Vec<(u8, String, Vec<u8>)> {
        reader::sections(module)
            .unwrap()
            .into_iter()
            .map(|section| {
                let mut r = Reader::new(section.contents);
                match section.id {
                    0 => {
                        let name = r.name().unwrap().to_owned();
                        (0, name, r.rest().to_vec())
                    }
                    id => (id, String::new(), section.contents.to_vec()),
                }
            })
            .collect()
    }

    fn find<'s>(sections: &'s [(u8, String, Vec<u8>)], id: u8, name: &str) -> &'s [u8] {
        let found = sections.iter().find(|it| it.0 == id && it.1 == name);
        &found.unwrap_or_else(|| panic!("no section {id} `{name}`")).2
    }

    fn names(r: &mut Reader<'_>) -> Vec<(u32, String)> {
        (0..r.count().unwrap()).map(|_| (r.u32().unwrap(), r.name().unwrap().to_owned())).collect()
    }

    #[test]
    fn a_command() {
        let module = command().module().unwrap();
        let file = ::object::File::parse(&*module).expect("object reads the module");
        assert_eq!(::object::Object::format(&file), ::object::BinaryFormat::Wasm);

        let sections = sections(&module);
        let ids: Vec<u8> = sections.iter().map(|it| it.0).collect();
        assert_eq!(ids, [1, 2, 3, 4, 5, 6, 7, 9, 10, 11, 0, 0, 0]);
        let customs: Vec<&str> = sections.iter().filter(|it| it.0 == 0).map(|it| it.1.as_str()).collect();
        assert_eq!(customs, ["my_section", "name", "producers"]);

        // Types, deduplicated: the import's, `fn()`, then the indirect call's.
        let types = find(&sections, 1, "");
        assert_eq!(types[0], 3);
        let mut at = 1;
        let decoded: Vec<Signature> =
            (0..3).map(|_| Signature::decode(types, &mut at).unwrap()).collect();
        assert_eq!(at, types.len());
        assert_eq!(decoded, [fd_write().ty, sig(&[], &[]), sig(&[ValType::I32], &[])]);

        let mut r = Reader::new(find(&sections, 2, ""));
        assert_eq!(r.count().unwrap(), 1);
        assert_eq!((r.name().unwrap(), r.name().unwrap()), ("wasi_snapshot_preview1", "fd_write"));
        assert_eq!((r.u8().unwrap(), r.u32().unwrap()), (0, 0));
        assert!(r.done());

        // `__wasm_call_ctors`, `init`, `_start`: all `fn()`.
        assert_eq!(find(&sections, 3, ""), [3, 1, 1, 1]);
        // A table of the 4 functions at slots 1 to 4, slot 0 null.
        assert_eq!(find(&sections, 4, ""), [1, FUNCREF, 1, 5, 5]);
        // Memory to the heap's start: 0x10_0010 is 17 pages.
        assert_eq!(find(&sections, 5, ""), [1, 0, 17]);
        let mut global = vec![1, I32, 1];
        leb::i32_const(&mut global, STACK as i32);
        assert_eq!(find(&sections, 6, ""), global);

        let mut r = Reader::new(find(&sections, 7, ""));
        assert_eq!(r.count().unwrap(), 2);
        assert_eq!((r.name().unwrap(), r.u8().unwrap(), r.u32().unwrap()), ("memory", EXPORT_MEMORY, 0));
        assert_eq!((r.name().unwrap(), r.u8().unwrap(), r.u32().unwrap()), ("_start", EXPORT_FUNCTION, 3));
        assert!(r.done());

        let mut element = vec![1, 0];
        leb::i32_const(&mut element, 1);
        element.extend([4, 0, 1, 2, 3]);
        assert_eq!(find(&sections, 9, ""), element);

        let mut r = Reader::new(find(&sections, 10, ""));
        assert_eq!(r.count().unwrap(), 3);
        let mut bodies = Vec::new();
        for _ in 0..3 {
            let len = r.u32().unwrap() as usize;
            bodies.push(r.bytes(len).unwrap().to_vec());
        }
        assert!(r.done());
        // `__wasm_call_ctors` calls `init` (slot 3 in the init array: index 2).
        assert_eq!(bodies[0], [0x00, 0x10, 2, 0x0b]);
        assert_eq!(bodies[1], [0x00, 0x0b]);
        let mut start = start_body();
        start[2..7].copy_from_slice(&padded_uleb(STACK_POINTER));
        start[27..32].copy_from_slice(&padded_uleb(2));
        assert_eq!(bodies[2], start);

        // `.rodata` and `.data` after the stack; `.bss` and the init array are not written.
        let mut r = Reader::new(find(&sections, 11, ""));
        assert_eq!(r.count().unwrap(), 2);
        for (addr, bytes) in [(DATA, &b"hello"[..]), (DATA + 8, &[3, 0, 0, 0][..])] {
            assert_eq!(r.u32().unwrap(), 0);
            assert_eq!(r.i32_const().unwrap(), addr as i32);
            let len = r.u32().unwrap() as usize;
            assert_eq!(r.bytes(len).unwrap(), bytes);
        }
        assert!(r.done());
        assert_eq!(find(&sections, 0, "my_section"), [1, 2, 3]);

        let mut r = Reader::new(find(&sections, 0, "name"));
        let mut subsections = BTreeMap::new();
        while !r.done() {
            let id = r.u8().unwrap();
            let len = r.u32().unwrap() as usize;
            subsections.insert(id, r.bytes(len).unwrap());
        }
        let owned = |list: &[(u32, &str)]| -> Vec<(u32, String)> {
            list.iter().map(|&(at, name)| (at, name.to_owned())).collect()
        };
        assert_eq!(
            names(&mut Reader::new(subsections[&1])),
            owned(&[(0, "fd_write"), (1, "__wasm_call_ctors"), (2, "init"), (3, "_start")])
        );
        assert_eq!(names(&mut Reader::new(subsections[&7])), owned(&[(0, "__stack_pointer")]));
        assert_eq!(names(&mut Reader::new(subsections[&9])), owned(&[(0, ".rodata"), (1, ".data")]));
        assert_eq!(subsections.keys().copied().collect::<Vec<_>>(), [1, 7, 9]);

        // The producers sections as one, `language` first.
        let mut r = Reader::new(find(&sections, 0, "producers"));
        assert_eq!(r.count().unwrap(), 2);
        assert_eq!(r.name().unwrap(), "language");
        assert_eq!(r.count().unwrap(), 1);
        assert_eq!((r.name().unwrap(), r.name().unwrap()), ("C11", ""));
        assert_eq!(r.name().unwrap(), "processed-by");
        assert_eq!(r.count().unwrap(), 1);
        assert_eq!((r.name().unwrap(), r.name().unwrap()), ("clang", "19"));
        assert!(r.done());
    }

    #[test]
    fn the_same_image_is_the_same_module() {
        assert_eq!(command().module().unwrap(), command().module().unwrap());
    }

    #[test]
    fn a_reactor_exports_functions_and_data() {
        let mut image = command();
        image.output.kind = OutputKind::Shared {
            name: "lib.wasm".to_owned(),
            exports: vec!["init".to_owned(), "__heap_base".to_owned()],
        };
        image.entry = None;
        image.imports[0].wasm = Some(fd_write());
        let init = image.functions[0].addr;
        image.exports = vec![
            Export { name: "init".to_owned(), addr: init, size: 4, kind: SymbolKind::Function },
            Export { name: "__heap_base".to_owned(), addr: DATA + 16, size: 0, kind: SymbolKind::Other },
        ];
        let module = image.module().unwrap();
        let sections = sections(&module);
        let mut r = Reader::new(find(&sections, 7, ""));
        let mut exports = Vec::new();
        for _ in 0..r.count().unwrap() {
            exports.push((r.name().unwrap().to_owned(), r.u8().unwrap(), r.u32().unwrap()));
        }
        // `_start` is exported by its object's `export_name`, as a reactor's `_initialize` is.
        assert_eq!(
            exports,
            [
                ("memory".to_owned(), EXPORT_MEMORY, 0),
                ("init".to_owned(), EXPORT_FUNCTION, 2),
                ("__heap_base".to_owned(), EXPORT_GLOBAL, 1),
                ("_start".to_owned(), EXPORT_FUNCTION, 3),
            ]
        );
        let mut global = vec![2, I32, 1];
        leb::i32_const(&mut global, STACK as i32);
        global.extend([I32, 0]);
        leb::i32_const(&mut global, (DATA + 16) as i32);
        assert_eq!(find(&sections, 6, ""), global);
    }

    #[test]
    fn functions_are_numbered_as_the_core_numbers_them() {
        // Two imports, then the code by its address, whatever order the core lists it in.
        let mut image = command();
        let mut second = image.imports[0].clone();
        second.name = "proc_exit".to_owned();
        second.wasm = Some(Import {
            module: "wasi_snapshot_preview1".to_owned(),
            field: "proc_exit".to_owned(),
            ty: sig(&[ValType::I32], &[]),
        });
        image.imports.push(second);
        image.functions.reverse();
        image.contents[3] = vec![4, 0, 0, 0];
        image.contents[6] = vec![4, 0, 0, 0];
        let module = image.module().unwrap();
        let sections = sections(&module);
        let mut r = Reader::new(find(&sections, 7, ""));
        assert_eq!(r.count().unwrap(), 2);
        assert_eq!((r.name().unwrap(), r.u8().unwrap(), r.u32().unwrap()), ("memory", EXPORT_MEMORY, 0));
        assert_eq!((r.name().unwrap(), r.u8().unwrap(), r.u32().unwrap()), ("_start", EXPORT_FUNCTION, 4));
        let mut r = Reader::new(find(&sections, 10, ""));
        r.count().unwrap();
        let len = r.u32().unwrap() as usize;
        assert_eq!(r.bytes(len).unwrap(), [0x00, 0x10, 3, 0x0b]);
        assert_eq!(find(&sections, 4, ""), [1, FUNCREF, 1, 6, 6]);
    }

    #[test]
    fn what_a_module_cannot_hold() {
        let refused = |change: fn(&mut Image), says: &str| {
            let mut image = command();
            change(&mut image);
            let what = image.module().unwrap_err();
            assert!(what.contains(says), "`{what}` does not say `{says}`");
        };
        refused(|it| it.debug.push((".debug_line", vec![1])), ".debug_line");
        refused(|it| it.got.push(GotEntry { addr: DATA, holds: Holds::Local(DATA) }), "GOT");
        refused(|it| it.imports[0].kind = SymbolKind::Data, "only functions");
        refused(|it| it.entry = None, "`_start` is not defined");
        refused(|it| it.entry = Some(2), "no function of the module starts");
        refused(
            |it| {
                it.functions.remove(0);
            },
            "no function of the image's",
        );
        refused(|it| it.contents[6] = vec![0, 0, 0, 0], "undefined weak");
        refused(|it| it.contents[6] = vec![9, 0, 0, 0], "init function 8");
        refused(|it| it.contents[6] = vec![1, 0, 0, 0], "init function 0");
        refused(|it| it.regions[2].addr = 16, "in the stack");
        refused(
            |it| {
                let fini = Region {
                    class: Class::FiniArray,
                    addr: DATA,
                    capacity: 4,
                    size: 4,
                    prot: Prot::RW,
                    align: 4,
                };
                it.regions.push(fini);
                it.contents.push(vec![0; 4]);
            },
            "FiniArray",
        );
        refused(
            |it| {
                let at = it.regions.iter().position(|r| r.class == Class::Synthetic(HEAP)).unwrap();
                it.regions.remove(at);
                it.contents.remove(at);
            },
            "heap",
        );
    }

    #[test]
    fn imports_need_a_module() {
        // Neither the core nor any code piece says what `fd_write` is imported as.
        let mut image = command();
        let code = Code { ty: sig(&[], &[]), export: Some("_start".to_owned()), ..Code::default() };
        let start = piece(&code, &[0x00, 0x0b]);
        let init = image.contents[1][..image.functions[0].size as usize].to_vec();
        image.functions[1].size = start.len() as u64;
        image.contents[1] = [init, start].concat();
        image.regions[1].size = image.contents[1].len() as u64;
        image.regions[1].capacity = image.regions[1].size;
        let what = image.module().unwrap_err();
        assert!(what.contains("`fd_write`"), "{what}");
        image.imports[0].wasm = Some(fd_write());
        image.module().unwrap();
    }

    #[test]
    fn merged_target_features() {
        let mut payload = Vec::new();
        for (prefix, feature) in [(b'+', "mutable-globals"), (b'+', "sign-ext")] {
            leb::uleb(&mut payload, 1);
            payload.push(prefix);
            leb::name(&mut payload, feature);
        }
        let merged = target_features(&payload).unwrap();
        let mut r = Reader::new(&merged);
        assert_eq!(r.count().unwrap(), 2);
        assert_eq!((r.u8().unwrap(), r.name().unwrap()), (b'+', "mutable-globals"));
        assert_eq!((r.u8().unwrap(), r.name().unwrap()), (b'+', "sign-ext"));
        leb::uleb(&mut payload, 1);
        payload.push(b'-');
        leb::name(&mut payload, "sign-ext");
        assert!(target_features(&payload).unwrap_err().contains("sign-ext"));
    }
}
