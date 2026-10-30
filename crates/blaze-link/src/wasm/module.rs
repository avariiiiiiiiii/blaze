//! Writes the module of a placed image, as wasm-ld lays one out: types, imports, functions,
//! the table, memory, globals, exports, the element segment, code, data, the custom sections,
//! and the `name` section.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use blaze_codegen::wasm::{Code, Import, Signature};

use super::leb::{self, PADDED, padded_uleb};
use super::reader::{MAGIC, Reader};
use super::{FUNCTIONS, HEAP, PAGE, STACK_POINTER, Wasm, is_code, is_custom};
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
    /// An index the core left free (room to grow, a function removed): it traps.
    Unused,
}

/// The functions `__wasm_call_ctors` calls, in order: the slots in the init array.
fn init_functions(placed: &Placed<'_>) -> Result<Vec<u32>, String> {
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

/// The bytes of the synthetic region of functions: `__wasm_call_ctors` as a code piece.
pub(super) fn ctors_piece(placed: &Placed<'_>) -> Result<Vec<u8>, String> {
    Ok(Code::default().encode(&ctors_body(&init_functions(placed)?)).0)
}

/// The function index of an address in the index space of functions.
fn function_index(addr: u64, count: u64) -> Result<u32, String> {
    match addr.checked_sub(1) {
        Some(index) if index < count => Ok(index as u32),
        _ => Err(format!("the address {addr} is no function of the module's {count}")),
    }
}

/// Every function of the module from its first import, and how many are imported.
fn functions<'a>(placed: &'a Placed<'a>) -> Result<(u64, Vec<Body<'a>>), String> {
    let imports = placed.imports.len() as u64;
    if imports > 0 {
        let (_, plt) = placed.region(&Class::Plt).ok_or("imports and no region of them")?;
        if plt.addr != 1 || plt.size < imports {
            return Err(format!(
                "the {imports} imports at {}, {} of them: imports are the first functions, from \
                 address 1",
                plt.addr, plt.size
            ));
        }
    }
    let mut end = 1 + imports;
    for region in placed.regions.iter().filter(|it| is_code(&it.class)) {
        if region.size > 0 {
            end = end.max(region.addr + region.size);
        }
    }
    let count = end - 1;
    let mut defined: Vec<Option<Body<'a>>> = Vec::new();
    defined.resize_with((count - imports) as usize, || None);
    let mut put = |addr: u64, body: Body<'a>| -> Result<(), String> {
        let index = u64::from(function_index(addr, count)?);
        if index < imports {
            return Err(format!("a function at index {index}, among the {imports} imports"));
        }
        let slot = &mut defined[(index - imports) as usize];
        if slot.is_some() {
            return Err(format!("two functions at index {index}"));
        }
        *slot = Some(body);
        Ok(())
    };

    let mut by_addr = BTreeMap::new();
    for function in placed.functions {
        if by_addr.insert(function.addr, function).is_some() {
            return Err(format!("two functions at address {}", function.addr));
        }
    }
    let mut taken = 0;
    for (index, region) in placed.regions.iter().enumerate() {
        let range = region.addr..region.addr + region.size;
        match &region.class {
            Class::Text | Class::Patch => {
                let bytes = placed.bytes(index);
                let mut at = 0usize;
                for (&addr, &function) in by_addr.range(range) {
                    let piece = usize::try_from(function.size)
                        .ok()
                        .and_then(|size| bytes.get(at..at.checked_add(size)?))
                        .ok_or_else(|| {
                            format!("the function at {addr} reaches past its region's bytes")
                        })?;
                    at += piece.len();
                    let (code, start) = Code::decode(piece).map_err(|e| {
                        format!("the function {}: {e}", function.name.as_deref().unwrap_or("?"))
                    })?;
                    put(addr, Body::Piece { code, body: &piece[start..], name: function.name.as_deref() })?;
                    taken += 1;
                }
                if at != bytes.len() {
                    return Err(format!(
                        "the {:?} region at {} has {} bytes and its functions {at}",
                        region.class,
                        region.addr,
                        bytes.len()
                    ));
                }
            }
            Class::Synthetic(name) if *name == FUNCTIONS && region.size > 0 => {
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
    Ok((imports, defined.into_iter().map(|it| it.unwrap_or(Body::Unused)).collect()))
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
    if placed.tls.is_some() {
        return Err("thread-local data: wasm32 without shared memory has no TLS".to_owned());
    }
    for region in placed.regions {
        let unplaceable = match &region.class {
            Class::Stubs
            | Class::Veneers
            | Class::EhFrame
            | Class::Table
            | Class::Got
            | Class::FiniArray
            | Class::Tdata
            | Class::Tbss => true,
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
            Body::Ctors(_) | Body::Unused => None,
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
        Class::Data => ".data",
        Class::InitArray => ".init_array",
        Class::PatchData => ".data.patch",
        Class::Named { name, .. } => name,
        _ => ".data",
    }
}

/// The module of a placed image.
pub(super) fn write(wasm: &Wasm, placed: &Placed<'_>) -> Result<Vec<u8>, String> {
    check(placed)?;
    let (imported, defined) = functions(placed)?;
    let count = imported + defined.len() as u64;
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
            Body::Unused => (types.of(&nothing), vec![0x00, 0x00, 0x0b]),
        };
        leb::uleb(&mut function_section, ty.into());
        leb::uleb(&mut code_section, bytes.len() as u64);
        code_section.extend(bytes);
    }

    // Memory: the stack, the data, then the heap from `__heap_base`.
    let (_, heap) = placed
        .region(&Class::Synthetic(HEAP))
        .ok_or("no heap region: the writer's synthetic regions were not placed")?;
    if heap.addr > 1 << 32 {
        return Err(format!("data up to {:#x}, past 32-bit memory", heap.addr));
    }
    let pages = heap.addr.div_ceil(PAGE).max(1);
    let mut segments = Vec::new();
    for (index, region) in placed.regions.iter().enumerate() {
        if is_code(&region.class) || is_custom(&region.class) {
            continue;
        }
        if region.addr + region.size > heap.addr {
            return Err(format!(
                "the {:?} region at {:#x} reaches past the heap's start {:#x}",
                region.class, region.addr, heap.addr
            ));
        }
        let bytes = placed.bytes(index);
        if bytes.is_empty() || region.class == Class::Bss {
            continue;
        }
        if bytes.len() as u64 > region.size {
            return Err(format!("the {:?} region has more bytes than its size", region.class));
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
        add(&mut exports, entry, EXPORT_FUNCTION, function_index(addr, count)?)?;
    }
    for it in placed.exports {
        match it.kind {
            SymbolKind::Function => {
                add(&mut exports, &it.name, EXPORT_FUNCTION, function_index(it.addr, count)?)?
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
            add(&mut exports, name, EXPORT_FUNCTION, (imported as usize + at) as u32)?;
        }
    }

    // Custom sections, by name in address order.
    let mut customs: Vec<(&str, Vec<u8>)> = Vec::new();
    for (index, region) in placed.regions.iter().enumerate() {
        if let Class::Named { name, write: false, exec: false } = &region.class {
            let bytes = placed.bytes(index);
            match customs.iter_mut().find(|(it, _)| *it == name.as_str()) {
                Some((_, contents)) => contents.extend(bytes),
                None => customs.push((name.as_str(), bytes.to_vec())),
            }
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
        // One active segment of table 0 (flags 0) putting every function at its address: the
        // slot a function pointer names.
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
    let mut function_names = BTreeMap::new();
    for (index, import) in placed.imports.iter().enumerate() {
        function_names.insert(index as u32, import.name.as_str());
    }
    for (at, body) in defined.iter().enumerate() {
        let name = match body {
            Body::Piece { name, .. } => *name,
            Body::Ctors(_) => Some("__wasm_call_ctors"),
            Body::Unused => None,
        };
        if let Some(name) = name {
            function_names.insert((imported as usize + at) as u32, name);
        }
    }
    name_map(&mut names, 1, &function_names);
    let global_names = globals.iter().enumerate().map(|(at, it)| (at as u32, it.0)).collect();
    name_map(&mut names, 7, &global_names);
    let segment_names = segments.iter().enumerate().map(|(at, it)| (at as u32, it.0)).collect();
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
