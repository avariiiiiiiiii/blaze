//! The canonical bytes of a prepared function ([`Shape`]): its IR as Cranelift prints it, with
//! each function, static and symbol by its place among the references and each object of its
//! own data by content, then its data, positions and named locals. Nothing in it is a number
//! of the run or of the order requests were made in, so two instances whose IR is the same
//! but for which places they name are one shape (`Vec<A>::drop` and `Vec<B>::drop` of a
//! pointer-sized `A` and `B`).

use std::fmt::Write as _;

use blaze_codegen::backend::Shape;
use cranelift_codegen::ir::{self, UserExternalName, UserFuncName};

use super::{Built, DataPlace, Function, Places, side::Variable};
use crate::{
    ConstBlob, DataIdentity,
    fragment::{self, DataRequest, Requests},
};

const VERSION: u8 = 2;

/// Where data is in a shape, as the names of its IR have it.
mod namespace {
    pub(super) const PLACE: u32 = 0;
    pub(super) const STATIC: u32 = 2;
    pub(super) const POSITION: u32 = 3;
    pub(super) const OWN: u32 = 4;
}

pub(crate) fn of(function: &Function) -> Result<Shape, String> {
    let mut out = Out(vec![VERSION]);
    let places = &function.places;
    out.part(b"refs", &places.refs.to_le_bytes());
    let requests = function.requests();
    let data = Data::of(places, requests)?;
    match &function.code {
        Built::Ir(pending) => {
            out.part(b"ir", ir(&pending.func, places, &data)?.as_bytes());
            let mut more = String::new();
            for slot in &pending.named {
                write!(more, "{slot} ").expect("a String");
            }
            write!(more, "| {:?}", pending.wants.pointees).expect("a String");
            out.part(b"named", more.as_bytes());
        }
        Built::Assembled(code, _) => {
            out.part(b"asm", &code.bytes);
            out.part(b"align", &code.align.to_le_bytes());
            let mut relocs = String::new();
            for reloc in &code.relocs {
                let name = match &reloc.name {
                    cranelift_module::ModuleRelocTarget::User { namespace: 0, index } => {
                        format!("f{}", place(places, *index)?)
                    }
                    cranelift_module::ModuleRelocTarget::User { namespace: 1, index } => {
                        format!("d{:?}", data.name(*index)?)
                    }
                    other => format!("{other:?}"),
                };
                writeln!(relocs, "{} {:?} {name} {}", reloc.offset, reloc.kind, reloc.addend)
                    .expect("a String");
            }
            out.part(b"relocs", relocs.as_bytes());
            let elf: Vec<u8> = code
                .elf_types
                .iter()
                .flat_map(|&(at, kind)| [at, kind])
                .flat_map(u32::to_le_bytes)
                .collect();
            out.part(b"elf", &elf);
        }
    }
    data.write(&mut out, requests, places)?;
    let mut spans = Vec::with_capacity(function.spans.len() * 36);
    for span in &function.spans {
        spans.extend_from_slice(&span.anchor.0);
        spans.extend_from_slice(&span.index.to_le_bytes());
    }
    out.part(b"spans", &spans);
    out.part(b"variables", &variables(&function.variables));
    out.part(b"hint", format!("{:?}", function.hint).as_bytes());
    Ok(Shape(out.0))
}

/// The canonical bytes of a static's value, as [`of`] has a function's data.
pub(crate) fn value(blob: &ConstBlob, requests: &Requests, places: &Places) -> Result<Vec<u8>, String> {
    let mut out = Out(vec![VERSION]);
    out.part(b"refs", &places.refs.to_le_bytes());
    let data = Data::of(places, requests)?;
    data.write(&mut out, requests, places)?;
    let mut text = String::new();
    data.blob(&mut text, blob, places)?;
    out.part(b"value", text.as_bytes());
    Ok(out.0)
}

struct Out(Vec<u8>);

impl Out {
    fn part(&mut self, name: &[u8], bytes: &[u8]) {
        self.0.extend_from_slice(&(name.len() as u32).to_le_bytes());
        self.0.extend_from_slice(name);
        self.0.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
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
        let mut text = String::new();
        for (index, place) in places.datas.iter().enumerate() {
            match place {
                DataPlace::Place(_) => {}
                DataPlace::Position(span) => {
                    writeln!(text, "position {} {}", span.anchor, span.index).expect("a String");
                }
                DataPlace::Text(it) => writeln!(text, "text {it:?}").expect("a String"),
                DataPlace::Nowhere => writeln!(text, "nowhere").expect("a String"),
                DataPlace::Own => {
                    let blob = match &requests.datas[index] {
                        DataRequest::Blob { identity: DataIdentity::Writable, .. } => {
                            return Err(format!("data {index} is written to"));
                        }
                        DataRequest::Blob { blob, .. } | DataRequest::Vtable { blob, .. } => blob,
                        _ => return Err(format!("data {index} is its own but has no blob")),
                    };
                    self.blob(&mut text, blob, places)?;
                }
            }
        }
        out.part(b"data", text.as_bytes());
        Ok(())
    }

    fn blob(&self, text: &mut String, blob: &ConstBlob, places: &Places) -> Result<(), String> {
        if !blob.type_ids.is_empty() {
            return Err("a constant with a `TypeId` not filled in".to_owned());
        }
        write!(text, "blob {} {:02x?}", blob.align, blob.bytes).expect("a String");
        for (at, data, addend) in &blob.relocs {
            let name = self.name(data.as_u32())?;
            write!(text, " {at}:{}:{}{addend:+}", name.namespace, name.index).expect("a String");
        }
        for (at, func) in &blob.fn_relocs {
            write!(text, " {at}:f{}", place(places, func.as_u32())?).expect("a String");
        }
        text.push('\n');
        Ok(())
    }
}

/// The IR as Cranelift prints it, every name by its place.
fn ir(func: &ir::Function, places: &Places, data: &Data) -> Result<String, String> {
    let mut func = func.clone();
    func.name = UserFuncName::user(0, 0);
    let names: Vec<_> =
        func.params.user_named_funcs().iter().map(|(at, name)| (at, name.clone())).collect();
    for (at, name) in names {
        let canonical = match name.namespace {
            0 => UserExternalName::new(namespace::PLACE, place(places, name.index)?),
            1 => data.name(name.index)?,
            fragment::INLINED => return Err("a reference of a body built in".to_owned()),
            other => return Err(format!("a name in the unknown namespace {other}")),
        };
        func.params.reset_user_func_name(at, canonical);
    }
    Ok(func.display().to_string())
}

fn variables(variables: &[Variable]) -> Vec<u8> {
    let mut bytes = Vec::new();
    for variable in variables {
        bytes.extend_from_slice(&(variable.name.len() as u32).to_le_bytes());
        bytes.extend_from_slice(variable.name.as_bytes());
        bytes.push(u8::from(variable.param));
        bytes.extend_from_slice(&variable.ty.0);
        bytes.extend_from_slice(&variable.slot.to_le_bytes());
        match variable.pointer {
            Some(offset) => {
                bytes.push(1);
                bytes.extend_from_slice(&offset.to_le_bytes());
            }
            None => bytes.push(0),
        }
    }
    bytes
}
