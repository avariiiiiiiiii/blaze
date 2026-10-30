//! A static's value as v2 keeps it ([`Static`]): its bytes with what they point at by place,
//! its own read-only data by content.

use base_db::Db;
use blaze_codegen::{
    Arch, Init, RelocKind, Relocation, Static, backend::FrontRef, template::Refs, wasm::Import,
};
use blaze_ids::ContentId;
use blaze_items::{ItemId, ItemKey};
use hir_def::{HasModule, StaticId, attrs::AttrFlags, signatures::StaticSignature};

use super::{
    HashSource, Hashes, Places,
    places::{Placer, Ref, fill_type_ids, place_requests, resolve, weak_symbol},
};
use crate::{
    ConstBlob, InstanceDef, compiled_static,
    fragment::{CompiledStatic, FuncTarget, Requests},
    home::Homes,
    is_thread_local, link_name,
    wasm::WasmSignature,
    wasm_import_module,
};

const VALUE_DOMAIN: &str = "blaze v2 static value 2";

/// A static's value, its references by place.
pub(crate) struct Value {
    /// The content ID of everything below: what the static's bytes are made of.
    pub(crate) value: ContentId,
    pub(crate) refs: Vec<Ref>,
    pub(crate) mutable: bool,
    pub(crate) thread_local: bool,
    pub(crate) section: Option<String>,
    /// On wasm, what the externs it points to are imported as ([`Static::imports`]).
    imports: Vec<(String, Import)>,
    blob: ConstBlob,
    requests: Requests,
    places: Places,
}

/// What the static `id` holds, evaluated.
pub(crate) fn value(
    db: &Db,
    homes: &mut Homes,
    hashes: &HashSource,
    id: StaticId,
) -> Result<Value, String> {
    let thread_local = is_thread_local(db, id);
    let section = AttrFlags::link_section(db, id.into()).map(|it| it.as_str().to_owned());
    let mut placer = Placer::open();
    let (blob, mut requests, mutable) = match weak_symbol(db, id)? {
        // What code reads of it is the address of the symbol.
        Some(symbol) => {
            placer.place(FrontRef::Weak(symbol))?;
            let blob = ConstBlob {
                bytes: vec![0; 8],
                relocs: Vec::new(),
                fn_relocs: Vec::new(),
                align: 8,
                type_ids: Vec::new(),
            };
            (blob, Requests::default(), false)
        }
        None => {
            let CompiledStatic { requests, blob, writable } =
                compiled_static(db, id).as_ref().map_err(|e| e.to_string())?;
            let requests = Requests { funcs: requests.funcs.clone(), datas: requests.datas.clone() };
            let mut blob = blob.clone();
            fill_type_ids(db, hashes, Some(id.krate(db)), &mut blob)?;
            (blob, requests, *writable)
        }
    };
    let places = place_requests(db, homes, hashes, Some(id.krate(db)), &mut placer, &mut requests)?;
    let imports = wasm_imports(db, id, &requests)?;
    let mut bytes = super::shape::value(&blob, &requests, &places)?;
    Ok(Value {
        value: value_id(&mut bytes, mutable, thread_local, &imports, section.as_deref()),
        refs: placer.into_refs(),
        mutable,
        thread_local,
        section,
        imports,
        blob,
        requests,
        places,
    })
}

/// The content ID of a static's value: `bytes`, its shape ([`super::shape::value`]), and the
/// rest of what the static is made of.
fn value_id(
    bytes: &mut Vec<u8>,
    mutable: bool,
    thread_local: bool,
    imports: &[(String, Import)],
    section: Option<&str>,
) -> ContentId {
    bytes.push(u8::from(mutable));
    bytes.push(u8::from(thread_local));
    bytes.extend_from_slice(&(imports.len() as u32).to_le_bytes());
    for (name, import) in imports {
        for text in [name, &import.module, &import.field] {
            bytes.extend_from_slice(&(text.len() as u32).to_le_bytes());
            bytes.extend_from_slice(text.as_bytes());
        }
        import.ty.encode(bytes);
    }
    if let Some(section) = section {
        bytes.extend_from_slice(section.as_bytes());
    }
    ContentId::of(VALUE_DOMAIN, bytes)
}

fn wasm_imports(db: &Db, id: StaticId, requests: &Requests) -> Result<Vec<(String, Import)>, String> {
    // alloc: an error's name of the static
    imports_of(db, requests, || format!("the static `{}`", StaticSignature::of(db, id).name.as_str()))
}

/// What each `#[link(wasm_import_module)]` function `requests` name is imported as, by its
/// symbol (the `Target::Extern` it is a reference to): as rustc imports it, the field its
/// link name and the type the one a call of it is lowered to ([`WasmSignature::of`], as
/// `finish_wasm` declares an import). Empty but on a target whose objects are wasm's.
/// `owner` names what asks, in an error: a static, or a function whose `Code::imports` these
/// are.
pub(crate) fn imports_of(
    db: &Db,
    requests: &Requests,
    owner: impl Fn() -> String,
) -> Result<Vec<(String, Import)>, String> {
    // alloc: empty, allocates only for what points to a wasm import
    let mut imports: Vec<(String, Import)> = Vec::new();
    for request in &requests.funcs {
        let FuncTarget::Instance(instance, _) = &request.target else { continue };
        let InstanceDef::Fn(func) = instance.def else { continue };
        let Some(module) = wasm_import_module(db, func) else { continue };
        let Some(name) = link_name(db, func) else {
            // alloc: an error
            return Err(format!(
                "{} points to a function of the wasm module `{module}` with no link name",
                owner()
            ));
        };
        let ty = WasmSignature::of(&request.sig).map_err(|what| {
            // alloc: an error
            format!(
                "{} points to `{name}` of the wasm module `{module}`, whose signature has no \
                 wasm type: {what}",
                owner()
            )
        })?;
        // clone: the field is the symbol, and both are the import's to keep
        let import = Import { module, field: name.clone(), ty: ty.format() };
        match imports.iter().find(|it| it.0 == name) {
            Some((_, seen)) if *seen == import => {}
            Some((_, seen)) => {
                // alloc: an error
                return Err(format!(
                    "{} points to `{name}` as two imports: of `{}` with {:?}, and of `{}` with {:?}",
                    owner(),
                    seen.module,
                    seen.ty,
                    import.module,
                    import.ty
                ));
            }
            None => imports.push((name, import)),
        }
    }
    Ok(imports)
}

/// The static `id` with its references at `refs`.
pub(crate) fn compile(
    db: &Db,
    homes: &mut Homes,
    hashes: &HashSource,
    arch: Arch,
    id: StaticId,
    refs: &Refs,
) -> Result<Static, String> {
    let item: ItemKey = hashes.item(db, ItemId::Static(id))?;
    // `wasm_import_module` reads the selected target, so `arch` has to be its.
    let selected = crate::Target::selected()?;
    if (arch == Arch::Wasm32) != selected.is_wasm() {
        // alloc: an error
        return Err(format!(
            "the static `{}` compiled for {arch:?} while the selected target is {:?}",
            StaticSignature::of(db, id).name.as_str(),
            selected.arch
        ));
    }
    let value = value(db, homes, hashes, id)?;
    if value.refs.len() != refs.len() {
        return Err(format!(
            "{} references given for a value of {}",
            refs.len(),
            value.refs.len()
        ));
    }
    let (data, mut relocs) = resolve(&value.places, refs, |resolve| {
        super::convert::value(resolve.frame(), arch, &value.blob, &value.requests, resolve)
    })?;
    if weak_symbol(db, id)?.is_some() {
        relocs.push(Relocation { at: 0, kind: RelocKind::Abs64, target: refs[0].clone(), addend: 0 });
    }
    let align = u32::try_from(value.blob.align).map_err(|e| e.to_string())?;
    let init = if relocs.is_empty() && value.blob.bytes.iter().all(|&it| it == 0) {
        Init::Zeroed { size: value.blob.bytes.len() as u64 }
    } else {
        Init::Bytes { bytes: value.blob.bytes, relocs }
    };
    Ok(Static {
        item,
        instance: ContentId::default(),
        align,
        init,
        data,
        mutable: value.mutable,
        thread_local: value.thread_local,
        section: value.section,
        imports: value.imports,
    })
}

#[cfg(test)]
mod tests {
    use blaze_codegen::wasm::{Signature, ValType};

    use super::*;

    fn import(module: &str, field: &str, results: Vec<ValType>) -> Vec<(String, Import)> {
        let ty = Signature { params: vec![ValType::I32], results };
        vec![("f".to_owned(), Import { module: module.to_owned(), field: field.to_owned(), ty })]
    }

    fn id(imports: &[(String, Import)]) -> ContentId {
        value_id(&mut vec![1, 2, 3, 4, 0, 0, 0, 0], false, false, imports, Some(".data.x"))
    }

    #[test]
    fn statics_that_differ_only_in_an_import_are_two_values() {
        let base = import("env", "f", vec![]);
        assert_eq!(id(&base), id(&import("env", "f", vec![])));
        assert_ne!(id(&base), id(&[]));
        assert_ne!(id(&base), id(&import("other", "f", vec![])));
        assert_ne!(id(&base), id(&import("env", "g", vec![])));
        assert_ne!(id(&base), id(&import("env", "f", vec![ValType::I64])));
        // The texts are length-prefixed: moving a byte from the module to the field is another
        // value.
        assert_ne!(id(&import("en", "vf", vec![])), id(&import("env", "f", vec![])));
    }
}
