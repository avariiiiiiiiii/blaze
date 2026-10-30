//! The canonical bytes of a prepared function ([`Shape`]): its IR as Cranelift's own `Hash` of
//! the function's stencil writes it (what Cranelift's incremental cache keys on), then each
//! name of the IR by its place among the references or the number of its own data, then that
//! data, the positions and the named locals. Nothing in it is a number of the run or of the
//! order requests were made in, so two instances whose IR is the same but for which places
//! they name are one shape (`Vec<A>::drop` and `Vec<B>::drop` of a pointer-sized `A` and `B`).
//!
//! Cost: one pass over the IR and its data into one buffer sized up front, with no clone of
//! the function, no printing and no `String`; about 4 to 8 bytes per value and instruction.

use std::hash::{Hash as _, Hasher};

use blaze_codegen::backend::Shape;
use cranelift_codegen::ir::{self, UserExternalName};

use super::{
    Built, DataPlace, Function, Places,
    side::{Scopes, SpanScope, Variable},
};
use crate::{
    ConstBlob, DataIdentity,
    fragment::{self, DataRequest, Hint, RelocName, Requests},
};

const VERSION: u8 = 4;

/// Where data is in a shape, as the names of its IR have it.
mod namespace {
    pub(super) const PLACE: u32 = 0;
    pub(super) const STATIC: u32 = 2;
    pub(super) const POSITION: u32 = 3;
    pub(super) const OWN: u32 = 4;
}

/// The parts of a shape, each once, in this order.
mod part {
    pub(super) const IR: u8 = 1;
    pub(super) const NAMES: u8 = 2;
    pub(super) const NAMED: u8 = 3;
    pub(super) const ASM: u8 = 4;
    pub(super) const DATA: u8 = 5;
    pub(super) const SPANS: u8 = 6;
    pub(super) const VARIABLES: u8 = 7;
    pub(super) const HINT: u8 = 8;
    pub(super) const VALUE: u8 = 9;
    pub(super) const SCOPES: u8 = 10;
}

pub(crate) fn of(function: &Function) -> Result<Shape, String> {
    let places = &function.places;
    let requests = function.requests();
    let data = Data::of(places, requests)?;
    let mut out = Out::with_capacity(estimate(function));
    out.u8(VERSION);
    out.u32(places.refs);
    match &function.code {
        Built::Ir(pending) => {
            let func = &pending.func;
            out.u8(part::IR);
            func.stencil.hash(&mut out);
            out.u8(part::NAMES);
            let names = func.params.user_named_funcs();
            out.u32(names.len() as u32);
            for (_, name) in names.iter() {
                let canonical = match name.namespace {
                    0 => UserExternalName::new(namespace::PLACE, place(places, name.index)?),
                    1 => data.name(name.index)?,
                    fragment::INLINED => return Err("a reference of a body built in".to_owned()),
                    other => return Err(format!("a name in the unknown namespace {other}")),
                };
                out.name(canonical);
            }
            out.u8(part::NAMED);
            out.u32(pending.named.len() as u32);
            for slot in &pending.named {
                out.u32(slot.as_u32());
            }
            pending.wants.pointees.hash(&mut out);
        }
        Built::Assembled(code, _) => {
            out.u8(part::ASM);
            out.bytes(&code.bytes);
            out.u64(code.align);
            out.u32(code.relocs.len() as u32);
            for reloc in code.relocs.iter() {
                out.u32(reloc.offset);
                out.u8(reloc_kind(reloc.kind));
                out.u64(reloc.addend as u64);
                match reloc.name {
                    RelocName::Func(index) => {
                        out.name(UserExternalName::new(namespace::PLACE, place(places, index)?));
                    }
                    RelocName::Data(index) => out.name(data.name(index)?),
                    other => return Err(format!("assembly naming {other:?}")),
                }
            }
            let typed = code.relocs.iter().filter_map(|it| Some((it.offset, it.elf_type?)));
            out.u32(typed.clone().count() as u32);
            for (at, kind) in typed {
                out.u32(at);
                out.u32(kind);
            }
        }
    }
    data.write(&mut out, requests, places)?;
    out.u8(part::SPANS);
    out.u32(function.spans.len() as u32);
    for span in &function.spans {
        out.raw(&span.anchor.0);
        out.u32(span.index);
    }
    out.u8(part::VARIABLES);
    variables(&mut out, &function.variables);
    out.u8(part::SCOPES);
    scopes(&mut out, &function.scopes);
    out.u8(part::HINT);
    out.u8(match function.hint {
        Hint::None => 0,
        Hint::Inline => 1,
        Hint::Always => 2,
        Hint::Never => 3,
    });
    Ok(Shape(out.0))
}

/// The canonical bytes of a static's value, as [`of`] has a function's data.
pub(crate) fn value(blob: &ConstBlob, requests: &Requests, places: &Places) -> Result<Vec<u8>, String> {
    let data = Data::of(places, requests)?;
    let mut out = Out::with_capacity(64 + 2 * blob.bytes.len());
    out.u8(VERSION);
    out.u32(places.refs);
    data.write(&mut out, requests, places)?;
    out.u8(part::VALUE);
    data.blob(&mut out, blob, places)?;
    Ok(out.0)
}

/// A shape's size, give or take: the stencil's `Hash` writes a few words per value, instruction
/// and block.
fn estimate(function: &Function) -> usize {
    let data: usize = function
        .requests()
        .datas
        .iter()
        .map(|it| match it {
            DataRequest::Blob { blob, .. } | DataRequest::Vtable { blob, .. } => {
                blob.bytes.len() + 16 * (blob.relocs.len() + blob.fn_relocs.len())
            }
            _ => 40,
        })
        .sum();
    let code = match &function.code {
        Built::Ir(pending) => {
            let dfg = &pending.func.dfg;
            24 * dfg.num_insts() + 8 * dfg.num_values() + 16 * dfg.num_blocks()
        }
        Built::Assembled(code, _) => code.bytes.len() + 24 * code.relocs.len(),
    };
    let scopes = 5 * function.scopes.parents.len() + 5 * function.scopes.of_span.len();
    256 + code + data + 36 * function.spans.len() + 53 * function.variables.len() + scopes
}

/// The shape being written. As a [`Hasher`] it keeps every byte it is given: the stencil's
/// derived `Hash` is a serialization of it, so equal bytes are equal IR, with no hash between.
struct Out(Vec<u8>);

impl Out {
    fn with_capacity(capacity: usize) -> Out {
        Out(Vec::with_capacity(capacity))
    }

    fn u8(&mut self, it: u8) {
        self.0.push(it);
    }

    fn u32(&mut self, it: u32) {
        self.0.extend_from_slice(&it.to_le_bytes());
    }

    fn u64(&mut self, it: u64) {
        self.0.extend_from_slice(&it.to_le_bytes());
    }

    fn raw(&mut self, bytes: &[u8]) {
        self.0.extend_from_slice(bytes);
    }

    fn bytes(&mut self, bytes: &[u8]) {
        self.u64(bytes.len() as u64);
        self.raw(bytes);
    }

    fn name(&mut self, name: UserExternalName) {
        self.u32(name.namespace);
        self.u32(name.index);
    }
}

impl Hasher for Out {
    fn finish(&self) -> u64 {
        self.0.len() as u64
    }

    fn write(&mut self, bytes: &[u8]) {
        self.0.extend_from_slice(bytes);
    }
}

fn place(places: &Places, index: u32) -> Result<u32, String> {
    places.funcs.get(index as usize).copied().ok_or_else(|| format!("function {index} has no place"))
}

/// What each data request is in a shape: a place, or the number of the object among the
/// function's own data and positions in the order they are first asked for.
struct Data {
    names: Vec<UserExternalName>,
}

impl Data {
    fn of(places: &Places, requests: &Requests) -> Result<Data, String> {
        if places.datas.len() != requests.datas.len() {
            return Err(format!(
                "{} data requests and {} places",
                requests.datas.len(),
                places.datas.len()
            ));
        }
        let (mut own, mut positions) = (0, 0);
        let names = places
            .datas
            .iter()
            .map(|place| match place {
                DataPlace::Place(place) => UserExternalName::new(namespace::STATIC, *place),
                DataPlace::Position(_) => {
                    positions += 1;
                    UserExternalName::new(namespace::POSITION, positions - 1)
                }
                DataPlace::Own | DataPlace::Text(_) | DataPlace::Nowhere => {
                    own += 1;
                    UserExternalName::new(namespace::OWN, own - 1)
                }
            })
            .collect();
        Ok(Data { names })
    }

    fn name(&self, index: u32) -> Result<UserExternalName, String> {
        self.names.get(index as usize).cloned().ok_or_else(|| format!("data {index} has no place"))
    }

    /// The content of the function's own data and its positions, in their order.
    fn write(&self, out: &mut Out, requests: &Requests, places: &Places) -> Result<(), String> {
        out.u8(part::DATA);
        out.u32(places.datas.len() as u32);
        for (index, place) in places.datas.iter().enumerate() {
            match place {
                DataPlace::Place(_) => out.u8(0),
                DataPlace::Position(span) => {
                    out.u8(1);
                    out.raw(&span.anchor.0);
                    out.u32(span.index);
                }
                DataPlace::Text(it) => {
                    out.u8(2);
                    out.bytes(it.as_bytes());
                }
                DataPlace::Nowhere => out.u8(3),
                DataPlace::Own => {
                    let blob = match &requests.datas[index] {
                        DataRequest::Blob { identity: DataIdentity::Writable, .. } => {
                            return Err(format!("data {index} is written to"));
                        }
                        DataRequest::Blob { blob, .. } | DataRequest::Vtable { blob, .. } => blob,
                        _ => return Err(format!("data {index} is its own but has no blob")),
                    };
                    out.u8(4);
                    self.blob(out, blob, places)?;
                }
            }
        }
        Ok(())
    }

    fn blob(&self, out: &mut Out, blob: &ConstBlob, places: &Places) -> Result<(), String> {
        if !blob.type_ids.is_empty() {
            return Err("a constant with a `TypeId` not filled in".to_owned());
        }
        out.u64(blob.align);
        out.bytes(&blob.bytes);
        out.u32(blob.relocs.len() as u32);
        for &(at, data, addend) in &blob.relocs {
            out.u32(at);
            out.name(self.name(data.as_u32())?);
            out.u64(addend as u64);
        }
        out.u32(blob.fn_relocs.len() as u32);
        for &(at, func) in &blob.fn_relocs {
            out.u32(at);
            out.u32(place(places, func.as_u32())?);
        }
        Ok(())
    }
}

/// A relocation kind of an assembly function by number: Cranelift's `Reloc` has no `Hash`.
fn reloc_kind(kind: cranelift_codegen::binemit::Reloc) -> u8 {
    // A fieldless enum: its discriminant is the kind, stable within one Cranelift, which the
    // unit's settings name.
    kind as u8
}

fn variables(out: &mut Out, variables: &[Variable]) {
    out.u32(variables.len() as u32);
    for variable in variables {
        out.bytes(variable.name.as_str().as_bytes());
        out.u8(u8::from(variable.param));
        out.raw(&variable.ty.0);
        out.u32(variable.slot);
        option(out, variable.pointer);
        option(out, variable.scope);
    }
}

fn scopes(out: &mut Out, scopes: &Scopes) {
    out.u32(scopes.parents.len() as u32);
    for &parent in &scopes.parents {
        option(out, parent);
    }
    out.u32(scopes.of_span.len() as u32);
    for &of in &scopes.of_span {
        match of {
            SpanScope::Before => out.u8(0),
            SpanScope::Outside => out.u8(1),
            SpanScope::In(scope) => {
                out.u8(2);
                out.u32(scope);
            }
        }
    }
}

fn option(out: &mut Out, it: Option<u32>) {
    match it {
        Some(it) => {
            out.u8(1);
            out.u32(it);
        }
        None => out.u8(0),
    }
}
