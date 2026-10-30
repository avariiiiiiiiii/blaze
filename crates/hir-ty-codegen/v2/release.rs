//! Release units (docs/v2-codegen.md, "Release"; #899, #176): LLVM per function, with what
//! cross-function optimization needs carried by summaries.
//!
//! [`summarize`] lowers an LLVM unit's IR and runs ThinLTO's pre-link pipeline on it once:
//! the module is its [`Export`], what importers link in, and what LLVM then says of it is its
//! [`Summary`]. [`blaze_codegen::imports::decide`] gives each unit its [`Imports`] from all
//! of them, and [`emit`] compiles the unit from the same pre-linked module with the exports
//! it imports linked in and the whole program's facts applied, through the post-link
//! pipeline of its level. One function per compile: imports are shared through the
//! summaries, never by compiling functions together (v1's units, `RA_JIT_UNITS`).

#[cfg(feature = "llvm")]
use std::sync::{Arc, OnceLock};

use blaze_codegen::{
    CodegenError, Unit,
    backend::{Emitted, Import},
    settings::Spec,
    summary::{Imports, Summary},
    template::Refs,
};
#[cfg(feature = "llvm")]
use blaze_codegen::{
    Backend, InstanceKey, Target,
    settings::DebugInfo,
    summary::{self, Arg, Call},
};
#[cfg(feature = "llvm")]
use blaze_items::CodeHash;
#[cfg(feature = "llvm")]
use cranelift_module::{DataId, FuncId};

use super::{Function, failed};
#[cfg(feature = "llvm")]
use super::{Built, DataPlace, Places};
#[cfg(feature = "llvm")]
use crate::{
    ConstBlob,
    backend::{Optimization, Passes, Selection},
    fragment::{DataRequest, Hint, Requests, Wants},
    llvm::release::{self as llvm, Given, Prelinked},
};

/// What [`summarize`] leaves of an LLVM unit for its own [`emit`]: its pre-linked module,
/// so that LLVM's pre-link pipeline runs once per function.
#[derive(Default)]
pub(crate) struct Kept {
    #[cfg(feature = "llvm")]
    own: OnceLock<Arc<Vec<u8>>>,
}

/// A release function as its importers link it, held for one build.
#[cfg(feature = "llvm")]
pub(crate) struct Export {
    /// Its pre-linked module without a line table, with its requests and signature; `None`
    /// for one nothing can import (assembly, data that points at data after it), whose
    /// summary says [`summary::Hint::Never`].
    module: Option<Arc<crate::imports::Export>>,
    /// What each of its requests is among its own references.
    places: Places,
    /// Its code hash: what a compile that imports it built in.
    code: CodeHash,
}

#[cfg(not(feature = "llvm"))]
pub(crate) struct Export(std::convert::Infallible);

/// The summary and export of an LLVM unit, from its prepared IR.
#[cfg(feature = "llvm")]
pub(crate) fn summarize(
    unit: &Unit,
    spec: &Spec,
    function: &Function,
    refs: &Refs,
) -> Result<(Summary, Export), CodegenError> {
    let optimization = optimization(unit, spec)?;
    let places = &function.places;
    let pending = match &function.code {
        Built::Ir(pending) => pending,
        Built::Assembled(code, requests) => {
            let params = u32::try_from(code.signature.params.len())
                .map_err(|_| failed(unit, "a function of more than 2^32 parameters"))?;
            let all = (0..requests.funcs.len() as u32).collect::<Vec<_>>();
            let summary = Summary {
                key: unit.instance.key(),
                size: 0,
                hint: summary::Hint::Never,
                calls: Vec::new(),
                // Its code calls what it calls with whatever it likes.
                addressed: keys(unit, places, refs, all.iter().copied())?,
                params,
                unused: Vec::new(),
                attrs: Default::default(),
                body: Default::default(),
                external: true,
            };
            let export = Export { module: None, places: places.clone(), code: unit.instance.code };
            return Ok((summary, export));
        }
    };
    let Prelinked { own, export, size, facts } = prelinked(unit, spec, function, pending, refs, optimization)?;
    let own = Arc::new(own);
    // A second summary of the same unit made the same module.
    _ = function.release.own.set(own);
    let requests = &pending.wants.requests;
    let mut calls: Vec<Call> = Vec::with_capacity(facts.calls.len());
    for called in facts.calls {
        let Some(callee) = key(unit, places, refs, called.request)? else {
            return Err(failed(unit, "LLVM calls a request that is not a function of the build"));
        };
        match calls.iter_mut().find(|it| it.callee == callee) {
            Some(call) => {
                call.count += called.count;
                let len = call.args.len().min(called.args.len());
                call.args.truncate(len);
                for (known, arg) in call.args.iter_mut().zip(called.args) {
                    if *known != arg {
                        *known = Arg::Unknown;
                    }
                }
            }
            None => calls.push(Call { callee, count: called.count, args: called.args }),
        }
    }
    // What data points at is called from where no summary says: a vtable's methods.
    let in_data = requests.datas.iter().flat_map(|it| match it {
        DataRequest::Blob { blob, .. } | DataRequest::Vtable { blob, .. } => {
            blob.fn_relocs.iter().map(|it| it.1.as_u32()).collect()
        }
        _ => Vec::new(),
    });
    let addressed = keys(unit, places, refs, facts.addressed.iter().copied().chain(in_data))?;
    let importable = requests.ordered();
    let hint = match function.hint {
        _ if !importable => summary::Hint::Never,
        Hint::None => summary::Hint::None,
        Hint::Inline => summary::Hint::Inline,
        Hint::Always => summary::Hint::Always,
        Hint::Never => summary::Hint::Never,
    };
    let summary = Summary {
        key: unit.instance.key(),
        size: u32::try_from(size).unwrap_or(u32::MAX),
        hint,
        calls,
        addressed,
        params: facts.params,
        unused: facts.unused,
        attrs: facts.attrs,
        body: facts.body,
        // Until the build says what code without a summary reaches
        // (`blaze_codegen::imports::close`).
        external: true,
    };
    let module = importable.then(|| {
        Arc::new(crate::imports::Export {
            bitcode: export,
            requests: copied(requests),
            signature: pending.func.signature.clone(),
            size,
            hint: function.hint,
        })
    });
    Ok((summary, Export { module, places: places.clone(), code: unit.instance.code }))
}

#[cfg(not(feature = "llvm"))]
pub(crate) fn summarize(
    unit: &Unit,
    _: &Spec,
    _: &Function,
    _: &Refs,
) -> Result<(Summary, Export), CodegenError> {
    Err(failed(unit, "blaze was built without `--features llvm`"))
}

/// Compiles an LLVM unit, with what it imports when it is a release unit. `refs` are its own
/// references, then those of what it imports ([`blaze_codegen::backend::release_refs`]).
#[cfg(feature = "llvm")]
pub(crate) fn emit(
    unit: &Unit,
    spec: &Spec,
    mut function: Function,
    refs: &Refs,
    imports: Option<(&Imports, &[Import<'_, Export>])>,
) -> Result<Emitted, CodegenError> {
    let optimization = optimization(unit, spec)?;
    let pending = match &function.code {
        Built::Ir(pending) => pending,
        Built::Assembled(..) => {
            if imports.is_some_and(|(it, _)| it.imported.iter().len() > 0) {
                return Err(failed(unit, "an assembly function imports nothing"));
            }
            return super::convert::emitted(unit, function, refs, Vec::new());
        }
    };
    let own = match function.release.own.get() {
        Some(own) => own.clone(),
        None => Arc::new(prelinked(unit, spec, &function, pending, refs, optimization)?.own),
    };
    let empty = Imports::default();
    let (decided, imported) = imports.unwrap_or((&empty, &[]));
    if decided.imported.len() != imported.len()
        || std::iter::zip(&decided.imported, imported).any(|(key, it)| *key != it.key)
    {
        return Err(failed(unit, "its imports are not those it was decided to have"));
    }
    let Function { code, places, .. } = &mut function;
    let Built::Ir(pending) = code else { unreachable!("matched above") };
    let mut linked = Vec::with_capacity(imported.len());
    let mut inlined = Vec::with_capacity(imported.len());
    for import in imported {
        let Some(module) = &import.export.module else {
            return Err(failed(unit, format!("imports {}, which has no LLVM export", import.key.0)));
        };
        let (funcs, datas) =
            merge(unit, &mut pending.wants, places, refs, &module.requests, &import.export.places, import.refs)?;
        let id = by_key(places, refs, import.key).into_iter().next().ok_or_else(|| {
            failed(unit, format!("imports {}, which nothing it builds calls", import.key.0))
        })?;
        linked.push(crate::imports::Import { export: module.clone(), id, funcs, datas });
        // A body LLVM did not build in can still have told it what the function returns or
        // that it writes nothing: every import is what the code is made of.
        inlined.push(import.export.code);
    }
    places.refs = u32::try_from(refs.len()).map_err(|_| failed(unit, "more than 2^32 references"))?;
    let mut given = Given { constants: decided.constants.clone(), ..Given::default() };
    for (callee, attrs) in &decided.callees {
        given.callees.extend(by_key(places, refs, *callee).into_iter().map(|it| (it, *attrs)));
    }
    for (callee, params) in &decided.dead {
        given.dead.extend(by_key(places, refs, *callee).into_iter().map(|it| (it, params.clone())));
    }
    let code = llvm::compile(&mut pending.wants, &own, &pending.func.signature, &linked, &given, optimization)
        .map_err(|why| failed(unit, format!("LLVM has no code for it: {why}")))?;
    let code = crate::backend::pad(code).map_err(|why| failed(unit, why))?;
    pending_code(&mut function, code);
    inlined.sort_unstable();
    inlined.dedup();
    super::convert::emitted(unit, function, refs, inlined)
}

#[cfg(not(feature = "llvm"))]
pub(crate) fn emit(
    unit: &Unit,
    _: &Spec,
    _: Function,
    _: &Refs,
    _: Option<(&Imports, &[Import<'_, Export>])>,
) -> Result<Emitted, CodegenError> {
    Err(failed(unit, "blaze was built without `--features llvm`"))
}

/// The optimization a unit's settings ask of LLVM.
#[cfg(feature = "llvm")]
fn optimization(unit: &Unit, spec: &Spec) -> Result<Optimization, CodegenError> {
    let Backend::Llvm { opt } = spec.backend else {
        return Err(failed(unit, "a unit Cranelift compiles is given to LLVM"));
    };
    if spec.toolchain.llvm.is_none() {
        return Err(failed(unit, "a unit asks for LLVM, and the toolchain has none"));
    }
    let passes = match opt {
        0 => Passes::None,
        1 => Passes::Less,
        2 => Passes::Default,
        3 => Passes::Aggressive,
        _ => return Err(failed(unit, format!("LLVM has no level {opt}"))),
    };
    let selection = if passes == Passes::None { Selection::Fast } else { Selection::Optimized };
    Ok(Optimization { passes, selection })
}

/// The function's module after the pre-link pipeline.
#[cfg(feature = "llvm")]
fn prelinked(
    unit: &Unit,
    spec: &Spec,
    function: &Function,
    pending: &crate::Pending,
    refs: &Refs,
    optimization: Optimization,
) -> Result<Prelinked, CodegenError> {
    let places = &function.places;
    let instances = |request: u32| {
        let target = places.funcs.get(request as usize).and_then(|&it| refs.get(it as usize));
        matches!(target, Some(Target::Function(_) | Target::Direct(_)))
    };
    let debug = spec.debuginfo != DebugInfo::None;
    llvm::prelink(&pending.wants, &pending.func, &pending.named, function.hint, debug, optimization, &instances)
        .map_err(|why| failed(unit, format!("LLVM has no code for it: {why}")))
}

/// What the function request `request` is in the build: the instance's key of a function
/// of it, `None` for an outside symbol.
#[cfg(feature = "llvm")]
fn key(unit: &Unit, places: &Places, refs: &Refs, request: u32) -> Result<Option<InstanceKey>, CodegenError> {
    let target = places
        .funcs
        .get(request as usize)
        .and_then(|&it| refs.get(it as usize))
        .ok_or_else(|| failed(unit, format!("function request {request} has no place among the references")))?;
    match target {
        Target::Function(key) | Target::Direct(key) => Ok(Some(*key)),
        Target::Extern(_) => Ok(None),
        other => Err(failed(unit, format!("function request {request} is {other:?}"))),
    }
}

/// The functions of the build `requests` are, sorted, each once.
#[cfg(feature = "llvm")]
fn keys(
    unit: &Unit,
    places: &Places,
    refs: &Refs,
    requests: impl Iterator<Item = u32>,
) -> Result<Vec<InstanceKey>, CodegenError> {
    let mut keys = Vec::new();
    for request in requests {
        keys.extend(key(unit, places, refs, request)?);
    }
    keys.sort_unstable();
    keys.dedup();
    Ok(keys)
}

/// The function requests that are the build's function `wanted`, in order.
#[cfg(feature = "llvm")]
fn by_key(places: &Places, refs: &Refs, wanted: InstanceKey) -> Vec<u32> {
    let is = |place: u32| match refs.get(place as usize) {
        Some(Target::Function(key) | Target::Direct(key)) => *key == wanted,
        _ => false,
    };
    (0..places.funcs.len() as u32).filter(|&at| is(places.funcs[at as usize])).collect()
}

#[cfg(feature = "llvm")]
fn copied(requests: &Requests) -> Requests {
    Requests { funcs: requests.funcs.clone(), datas: requests.datas.clone() }
}

/// Puts an import's `requests`, which are `theirs` among `their_refs`, among what the
/// compile asks for (`wants`, `places`, places into the compile's `refs`): what each of them
/// is in the compile, functions then data. A static, a vtable or a position asked for twice
/// is one object, as is equal read-only data; LLVM takes two globals for two objects.
#[cfg(feature = "llvm")]
fn merge(
    unit: &Unit,
    wants: &mut Wants,
    places: &mut Places,
    refs: &Refs,
    requests: &Requests,
    theirs: &Places,
    their_refs: &Refs,
) -> Result<(Vec<u32>, Vec<u32>), CodegenError> {
    let place_in = |target: &Target| {
        let found = refs.iter().position(|it| it == target).map(|it| it as u32);
        found.ok_or_else(|| {
            failed(unit, format!("an import refers to {target:?}, which the compile's references lack"))
        })
    };
    let theirs_of = |place: u32| {
        their_refs
            .get(place as usize)
            .ok_or_else(|| failed(unit, format!("an import's place {place} is past its references")))
    };
    let mut funcs = Vec::with_capacity(requests.funcs.len());
    for (at, request) in requests.funcs.iter().enumerate() {
        let theirs = *theirs.funcs.get(at).ok_or_else(|| failed(unit, "an import's request without its place"))?;
        let place = place_in(theirs_of(theirs)?)?;
        let before = wants.requests.funcs.len();
        let id = wants.func(request.target.clone(), &request.sig).as_u32();
        if id as usize == before {
            places.funcs.push(place);
        } else if places.funcs[id as usize] != place {
            return Err(failed(unit, "an import asks for a function the compile has as another"));
        }
        funcs.push(id);
    }
    let mut datas: Vec<u32> = Vec::with_capacity(requests.datas.len());
    for (at, request) in requests.datas.iter().enumerate() {
        let place = match theirs.datas.get(at) {
            Some(DataPlace::Place(theirs)) => DataPlace::Place(place_in(theirs_of(*theirs)?)?),
            Some(other) => other.clone(),
            None => return Err(failed(unit, "an import's data without its place")),
        };
        let moved = |blob: &ConstBlob| -> Result<ConstBlob, CodegenError> {
            let relocs = blob.relocs.iter().map(|&(offset, target, addend)| {
                let moved = datas.get(target.as_u32() as usize).copied();
                let moved = moved.ok_or_else(|| failed(unit, "an import's data points at data after it"))?;
                Ok((offset, DataId::from_u32(moved), addend))
            });
            let fn_relocs = blob.fn_relocs.iter().map(|&(offset, target)| {
                let moved = funcs.get(target.as_u32() as usize).copied();
                let moved = moved.ok_or_else(|| failed(unit, "an import's data points at no function"))?;
                Ok((offset, FuncId::from_u32(moved)))
            });
            Ok(ConstBlob {
                bytes: blob.bytes.clone(),
                relocs: relocs.collect::<Result<_, CodegenError>>()?,
                fn_relocs: fn_relocs.collect::<Result<_, CodegenError>>()?,
                align: blob.align,
                type_ids: blob.type_ids.clone(),
            })
        };
        let before = wants.requests.datas.len();
        let id = match request {
            DataRequest::Blob { blob, identity } => wants.define_data_as(moved(blob)?, *identity).as_u32(),
            DataRequest::Vtable { ty, trait_ref, blob } => {
                let found = wants.requests.datas.iter().position(|it| {
                    matches!(it, DataRequest::Vtable { ty: other, trait_ref: other_ref, .. }
                        if other == ty && other_ref == trait_ref)
                });
                match found {
                    Some(found) => found as u32,
                    None => {
                        let blob = moved(blob)?;
                        wants.requests.datas.push(DataRequest::Vtable { ty: ty.clone(), trait_ref: trait_ref.clone(), blob });
                        before as u32
                    }
                }
            }
            other => match wants.requests.datas.iter().position(|it| it == other) {
                Some(found) => found as u32,
                None => {
                    wants.requests.datas.push(other.clone());
                    before as u32
                }
            },
        };
        if id as usize == before {
            places.datas.push(place);
        } else if places.datas[id as usize] != place {
            return Err(failed(unit, "an import asks for data the compile has as another"));
        }
        datas.push(id);
    }
    Ok((funcs, datas))
}