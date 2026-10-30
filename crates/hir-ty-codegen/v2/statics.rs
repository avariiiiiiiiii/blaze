//! A static's value as v2 keeps it ([`Static`]): its bytes with what they point at by place,
//! its own read-only data by content.

use blaze_codegen::{Arch, Init, Reloc, RelocKind, Static, backend::FrontRef, template::Refs};
use blaze_ids::ContentId;
use blaze_items::{ItemId, ItemKey};
use hir_def::{HasModule, StaticId, attrs::AttrFlags};
use hir_ty::db::HirDatabase;

use super::{
    Hashes, Places,
    places::{Placer, Ref, fill_type_ids, place_requests, resolve, weak_symbol},
};
use crate::{
    ConstBlob, compiled_static,
    fragment::{CompiledStatic, Requests},
    home::Homes,
    is_thread_local,
};

const VALUE_DOMAIN: &str = "blaze v2 static value 1";

/// A static's value, its references by place.
pub(crate) struct Value {
    /// The content ID of everything below: what the static's bytes are made of.
    pub(crate) value: ContentId,
    pub(crate) refs: Vec<Ref>,
    pub(crate) mutable: bool,
    pub(crate) thread_local: bool,
    pub(crate) section: Option<String>,
    blob: ConstBlob,
    requests: Requests,
    places: Places,
}

/// What the static `id` holds, evaluated.
pub(crate) fn value(
    db: &dyn HirDatabase,
    homes: &mut Homes,
    hashes: &dyn Hashes,
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
    let mut bytes = super::shape::value(&blob, &requests, &places)?;
    bytes.push(u8::from(mutable));
    bytes.push(u8::from(thread_local));
    if let Some(section) = &section {
        bytes.extend_from_slice(section.as_bytes());
    }
    Ok(Value {
        value: ContentId::of(VALUE_DOMAIN, &bytes),
        refs: placer.into_refs(),
        mutable,
        thread_local,
        section,
        blob,
        requests,
        places,
    })
}

/// The static `id` with its references at `refs`.
pub(crate) fn compile(
    db: &dyn HirDatabase,
    homes: &mut Homes,
    hashes: &dyn Hashes,
    arch: Arch,
    id: StaticId,
    refs: &Refs,
) -> Result<Static, String> {
    let item: ItemKey = hashes.item(db, ItemId::Static(id))?;
    let value = value(db, homes, hashes, id)?;
    if value.refs.len() != refs.len() {
        return Err(format!(
            "{} references given for a value of {}",
            refs.len(),
            value.refs.len()
        ));
    }
    let (data, mut relocs) = resolve(&value.places, refs, |resolve| {
        super::convert::value(arch, &value.blob, &value.requests, resolve)
    })?;
    if weak_symbol(db, id)?.is_some() {
        relocs.push(Reloc { at: 0, kind: RelocKind::Abs64, target: refs[0].clone(), addend: 0 });
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
    })
}
