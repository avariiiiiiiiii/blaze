//! What the IR builder's requests are as references: one function, static or outside symbol
//! for each, the same whether the IR builder or the walk of the MIR ([`super::refs`]) asks.
//! Equal references are one place, so both go through here.

use std::hash::BuildHasher as _;

use blaze_codegen::{Target, backend::FrontRef, template::Refs};
use cranelift_codegen::ir;
use hashbrown::HashTable;
use rustc_hash::FxBuildHasher;
use hir_def::{
    FunctionId, ItemContainerId, Lookup, StaticId,
    attrs::AttrFlags,
    signatures::{FunctionSignature, StaticSignature},
};
use hir_ty::{
    db::HirDatabase,
    next_solver::{DbInterner, GenericArgs},
    traits::StoredParamEnvAndCrate,
};

use super::{
    DataPlace, Hashes, HirInstance, HirStatic, Kind, Places,
    convert::{DataTarget, Resolve},
};
use crate::{
    ConstBlob, DataIdentity, Instance, InstanceDef, erase_regions,
    fragment::{DataRequest, FuncRequest, FuncTarget, Requests, TextPart},
    home::Homes,
    link_name, static_link_name,
};

pub(crate) type Ref = FrontRef<HirInstance, HirStatic>;

/// The symbol of the routine the unwinder asks about a frame with landing pads.
pub(crate) const PERSONALITY: &str = crate::unwind::PERSONALITY;

/// The reference a function request of the IR builder is.
pub(crate) fn func_ref(
    db: &dyn HirDatabase,
    homes: &mut Homes,
    target: &FuncTarget,
) -> Result<Ref, String> {
    match target {
        FuncTarget::Symbol(symbol) => Ok(FrontRef::Extern(symbol.clone())),
        FuncTarget::Path(path) => {
            Err(format!("a call of rustc's `{path}`, which a build without rustc has none of"))
        }
        FuncTarget::Instance(instance, env) => instance_ref(db, homes, instance, env),
        FuncTarget::Asm(text) => Ok(FrontRef::Function(HirInstance(Kind::Asm(text.clone())))),
    }
}

/// The reference a call of `def` with `args` in `env` is: the instance, compiled in the crate
/// every asker agrees on, or the symbol of a function that has no body of Rust.
pub(crate) fn call_ref<'db>(
    db: &'db dyn HirDatabase,
    homes: &mut Homes,
    def: InstanceDef,
    args: GenericArgs<'db>,
    env: &StoredParamEnvAndCrate,
) -> Result<Ref, String> {
    let interner = DbInterner::new_with(db, env.krate);
    let instance = Instance { def, args: erase_regions(interner, args).store(), checked: false };
    instance_ref(db, homes, &instance, env)
}

/// As [`call_ref`], of an instance as the IR builder holds it.
pub(crate) fn instance_ref(
    db: &dyn HirDatabase,
    homes: &mut Homes,
    instance: &Instance,
    env: &StoredParamEnvAndCrate,
) -> Result<Ref, String> {
    let instance = Instance { checked: false, ..instance.clone() };
    if let InstanceDef::Fn(func) = instance.def
        && !FunctionSignature::of(db, func).has_body()
    {
        return bodyless(db, func).map(FrontRef::Extern);
    }
    let env = homes.home(db, &instance, env);
    Ok(FrontRef::Function(HirInstance(Kind::Code(instance, env))))
}

/// The symbol a function without a body of Rust is: an item of an `extern` block.
fn bodyless(db: &dyn HirDatabase, func: FunctionId) -> Result<String, String> {
    let name = FunctionSignature::of(db, func).name.as_str().to_owned();
    match func.lookup(db).container {
        ItemContainerId::ExternBlockId(_) => {
            let Some(symbol) = link_name(db, func) else {
                return Err(format!("`{name}` of an `extern` block has no link name"));
            };
            if symbol.starts_with("llvm.") {
                return Err(format!("a call of LLVM's intrinsic `{symbol}` the IR builder left"));
            }
            Ok(symbol)
        }
        ItemContainerId::TraitId(_) => {
            Err(format!("a call of the trait method `{name}`, which no impl was found for"))
        }
        _ if FunctionSignature::is_intrinsic(db, func) => {
            Err(format!("a call of the intrinsic `{name}`, which has no fallback body"))
        }
        _ => Err(format!("a call of `{name}`, which has no body")),
    }
}

/// The reference a static is: an item of an `extern` block by its symbol, any other by
/// which static it is. An `extern_weak` one is a static of blaze's that holds the address of
/// its symbol, 0 when nothing defines it ([`weak_symbol`]).
pub(crate) fn static_ref(db: &dyn HirDatabase, id: StaticId) -> Result<Ref, String> {
    if weak_symbol(db, id)?.is_some() {
        return Ok(FrontRef::Static(HirStatic(id)));
    }
    match static_link_name(db, id) {
        Some(symbol) => Ok(FrontRef::Extern(symbol)),
        None => Ok(FrontRef::Static(HirStatic(id))),
    }
}

/// The symbol of an `extern_weak` static of an `extern` block.
pub(crate) fn weak_symbol(db: &dyn HirDatabase, id: StaticId) -> Result<Option<String>, String> {
    if !AttrFlags::linkage(db, id.into()).is_some_and(|it| it.as_str() == "extern_weak") {
        return Ok(None);
    }
    match static_link_name(db, id) {
        Some(symbol) => Ok(Some(symbol)),
        None => Err(format!(
            "`{}` is `extern_weak` but of no `extern` block",
            StaticSignature::of(db, id).name.as_str()
        )),
    }
}

/// The reference a routine Cranelift calls itself is: the C library's.
pub(crate) fn libcall_ref(libcall: ir::LibCall) -> Ref {
    FrontRef::Extern(libcall_symbol(libcall))
}

pub(crate) fn libcall_symbol(libcall: ir::LibCall) -> String {
    cranelift_module::default_libcall_names()(libcall)
}

/// The reference of the personality routine of a function with landing pads.
pub(crate) fn personality_ref() -> Ref {
    FrontRef::Extern(PERSONALITY.to_owned())
}
/// The places of a list of references: a function's, which the walk made and nothing is
/// added to, or a static's, made as its requests come.
pub(crate) struct Placer {
    refs: Vec<Ref>,
    /// Each place, found by its reference's hash and checked against `refs`, so no reference is
    /// cloned into a key: 4 bytes and a control byte a place, one probe a lookup.
    index: HashTable<u32>,
    open: bool,
}

fn hash(it: &Ref) -> u64 {
    FxBuildHasher.hash_one(it)
}

impl Placer {
    pub(crate) fn listed(refs: Vec<Ref>) -> Placer {
        let mut index = HashTable::with_capacity(refs.len());
        for (place, it) in refs.iter().enumerate() {
            let hashed = hash(it);
            if index.find(hashed, |&at: &u32| refs[at as usize] == *it).is_none() {
                index.insert_unique(hashed, place as u32, |&at| hash(&refs[at as usize]));
            }
        }
        Placer { refs, index, open: false }
    }

    pub(crate) fn open() -> Placer {
        Placer { refs: Vec::new(), index: HashTable::new(), open: true }
    }

    pub(crate) fn place(&mut self, it: Ref) -> Result<u32, String> {
        let hashed = hash(&it);
        let refs = &self.refs;
        if let Some(&place) = self.index.find(hashed, |&at| refs[at as usize] == it) {
            return Ok(place);
        }
        if !self.open {
            return Err(format!("the IR asks for {it:?}, which its references do not list"));
        }
        let place = self.refs.len() as u32;
        self.refs.push(it);
        let refs = &self.refs;
        self.index.insert_unique(hashed, place, |&at| hash(&refs[at as usize]));
        Ok(place)
    }

    pub(crate) fn refs(&self) -> &[Ref] {
        &self.refs
    }

    pub(crate) fn into_refs(self) -> Vec<Ref> {
        self.refs
    }
}

/// The place of each of `requests`, whose `TypeId`s it fills in.
pub(crate) fn place_requests(
    db: &dyn HirDatabase,
    homes: &mut Homes,
    hashes: &dyn Hashes,
    placer: &mut Placer,
    requests: &mut Requests,
) -> Result<Places, String> {
    let mut funcs = Vec::with_capacity(requests.funcs.len());
    for request in &requests.funcs {
        funcs.push(placer.place(func_ref(db, homes, &request.target)?)?);
    }
    let mut datas = Vec::with_capacity(requests.datas.len());
    for request in &mut requests.datas {
        let place = match request {
            DataRequest::Blob { identity: DataIdentity::Writable, .. } => {
                return Err("data the code writes to, which is no constant (`promoted_slot`)"
                    .to_owned());
            }
            DataRequest::Blob { blob, .. } | DataRequest::Vtable { blob, .. } => {
                fill_type_ids(db, hashes, blob)?;
                DataPlace::Own
            }
            DataRequest::Static(id) => DataPlace::Place(placer.place(static_ref(db, *id)?)?),
            DataRequest::Location(Some((owner, span))) => {
                DataPlace::Position(hashes.span(db, *owner, *span)?)
            }
            DataRequest::Location(None) => DataPlace::Nowhere,
            DataRequest::Declaration(func) => DataPlace::Position(hashes.declaration(db, *func)?),
            DataRequest::FilePath(file) => DataPlace::Text(hashes.file_name(db, *file)?),
            DataRequest::Text(parts) => {
                let mut text = String::new();
                for part in parts.iter() {
                    match part {
                        TextPart::Text(it) => text.push_str(it),
                        TextPart::FilePath(file) => text.push_str(&hashes.file_name(db, *file)?),
                    }
                }
                DataPlace::Text(text)
            }
        };
        datas.push(place);
    }
    Ok(Places { funcs, datas, refs: placer.refs().len() as u32 })
}

/// Writes each `TypeId` of a constant where const eval left the place of it: the first 16
/// bytes of its type's content ID, one value for a type in every crate of the build.
pub(crate) fn fill_type_ids(
    db: &dyn HirDatabase,
    hashes: &dyn Hashes,
    blob: &mut ConstBlob,
) -> Result<(), String> {
    for (offset, ty) in std::mem::take(&mut blob.type_ids) {
        let id = hashes.ty(db, ty.as_ref())?;
        let at = offset as usize;
        let Some(bytes) = blob.bytes.get_mut(at..at + 16) else {
            return Err(format!("a `TypeId` at {at}, past the end of its constant"));
        };
        bytes.copy_from_slice(&id.0[..16]);
    }
    Ok(())
}

/// Resolves a function's requests against the targets of its places.
pub(crate) fn resolve<T>(
    places: &Places,
    refs: &Refs,
    work: impl FnOnce(&Resolve<'_>) -> T,
) -> T {
    let target = |place: u32| {
        refs.get(place as usize)
            .cloned()
            .ok_or_else(|| format!("place {place} of {} references", refs.len()))
    };
    let func = |index: u32, _: &FuncRequest| match places.funcs.get(index as usize) {
        Some(&place) => target(place),
        None => Err(format!("function {index} has no place")),
    };
    let data = |index: u32, _: &DataRequest| match places.datas.get(index as usize) {
        Some(DataPlace::Place(place)) => target(*place).map(DataTarget::Target),
        Some(DataPlace::Position(span)) => Ok(DataTarget::Target(Target::Position(*span))),
        Some(DataPlace::Own) => Ok(DataTarget::Own),
        Some(DataPlace::Text(text)) => Ok(DataTarget::Text(text.clone())),
        Some(DataPlace::Nowhere) => Ok(DataTarget::Nowhere),
        None => Err(format!("data {index} has no place")),
    };
    let symbol = |name: &str| {
        refs.iter()
            .find(|it| matches!(it, Target::Extern(it) if it == name))
            .cloned()
            .ok_or_else(|| format!("the code calls `{name}`, which its references do not list"))
    };
    let body = || match refs.len().checked_sub(1) {
        Some(last) if last == places.refs as usize => Ok(refs[last].clone()),
        _ => Err("a call of a fold's body from what is no thunk of it".to_owned()),
    };
    work(&Resolve { func: &func, data: &data, symbol: &symbol, body: &body })
}
