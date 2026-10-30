//! A compiled function with what it refers to by place: what the store keeps and the back-end
//! memo shares (#1291).
//!
//! A [`CompiledFunction`]'s relocations name other functions by [`InstanceKey`], and an
//! instance key holds the callee's code hash: a callee's body edit gives it a new key while
//! the caller's own key stays, as the caller's code hash takes the callee by its interface.
//! So what is kept under the caller's key can't name its callees. A [`Template`] names each
//! function, static and outside symbol it refers to by its place in the instance's
//! references ([`Refs`]: the list the front end makes of an instance, in the order of its
//! MIR, without compiling it), and its own data by its place among its data. A build fills
//! the places with what they are in that build ([`Template::instantiate`]): a callee edit
//! renames relocations and compiles nothing.
//!
//! Two instances whose templates are equal compile once: that is the memo up to callee
//! renaming. A template keeps constants, layouts and positions, so the sharing is exact.
//!
//! [`Target::Function`] and [`Target::Direct`] go in one kind of instruction (a direct call
//! or address), the first through the callee's jump-table stub: which one a reference is
//! changes no byte of the template, only the place's target.

use blaze_format::SpanRef;
use blaze_ids::{ContentId, Hasher};
use blaze_items::{CodeHash, ItemKey};
use serde::{Deserialize, Serialize};

use crate::{
    Arch, CompiledFunction, Data, Init, InstanceKey, Line, Reloc, RelocKind, Static, Target, Unwind,
    content,
};

/// What an instance refers to, in the order its front end lists them: every function, static
/// and outside symbol, each once. Never a [`Target::Data`] or [`Target::Position`].
pub type Refs = [Target];

/// What a relocation of a template points at.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Ref {
    /// The `n`th of the instance's [`Refs`].
    Place(u32),
    /// The `n`th of the template's own data.
    Data(u32),
    Position(SpanRef),
    /// The `n`th of the template's own [`Template::externs`].
    Extern(u32),
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct TemplateReloc {
    pub at: u32,
    pub kind: RelocKind,
    pub target: Ref,
    pub addend: i64,
}

/// A data object of a template: it points only at data before it, so each one's content ID
/// can be made in order.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Hash, Debug)]
pub struct TemplateData {
    pub align: u32,
    pub bytes: Vec<u8>,
    pub relocs: Vec<TemplateReloc>,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Hash, Debug)]
pub struct Template {
    pub arch: Arch,
    pub align: u32,
    pub code: Vec<u8>,
    pub relocs: Vec<TemplateReloc>,
    pub data: Vec<TemplateData>,
    pub inlined: Vec<CodeHash>,
    pub side: Option<ContentId>,
    pub unwind: Option<TemplateUnwind>,
    pub lines: Vec<Line>,
    /// How many references it was made against: what [`Template::instantiate`] is given.
    pub refs: u32,
    /// Outside symbols its backend calls of its own accord, which no front end lists
    /// ([`crate::backend::Emitted::extra`]): a function of its code and settings, so kept with
    /// it and never a place.
    pub externs: Vec<String>,
}

/// A function's [`Unwind`], its personality routine or handler by place and its
/// language-specific data area by its index among the template's data.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Hash, Debug)]
pub enum TemplateUnwind {
    Dwarf {
        code_align: u32,
        data_align: i32,
        return_address: u16,
        cie: Vec<u8>,
        fde: Vec<u8>,
        personality: Option<u32>,
        lsda: Option<u32>,
    },
    Windows { info: Vec<u8>, handler: Option<u32>, lsda: Option<u32> },
}

/// A static's value with what it refers to by place, as a function's [`Template`]. Never
/// merged with another by content: it is the static [`Target::Static`] names.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Hash, Debug)]
pub struct StaticTemplate {
    pub align: u32,
    pub init: TemplateInit,
    pub data: Vec<TemplateData>,
    pub mutable: bool,
    pub thread_local: bool,
    pub section: Option<String>,
    pub refs: u32,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Hash, Debug)]
pub enum TemplateInit {
    Bytes { bytes: Vec<u8>, relocs: Vec<TemplateReloc> },
    Zeroed { size: u64 },
}

/// Why a function and its references don't make a template, or a template and references
/// don't make a function.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum TemplateError {
    /// A relocation points at a function, static or symbol that is not among the references.
    NotReferenced(Target),
    /// A relocation points at data that is not the function's own.
    ForeignData(ContentId),
    /// A data object points at itself or at data after it.
    DataOrder { data: u32, points_at: u32 },
    /// A reference is data or a position.
    NotAReference(Target),
    /// Instantiated with another number of references than it was made against.
    Refs { made: u32, given: u32 },
    /// A place or data index past the end.
    Place(u32),
    /// An index past the end of the template's own outside symbols.
    Extern(u32),
    /// A symbol a backend says it named of its own accord is not an outside symbol's name.
    NotAnExtern(String),
}

impl std::fmt::Display for TemplateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TemplateError::NotReferenced(target) => {
                write!(f, "a relocation points at {target:?}, which the instance's references do not list")
            }
            TemplateError::ForeignData(id) => {
                write!(f, "a relocation points at data {id} that is not the function's own")
            }
            TemplateError::DataOrder { data, points_at } => {
                write!(f, "data object {data} points at data object {points_at}, not one before it")
            }
            TemplateError::NotAReference(target) => {
                write!(f, "{target:?} is listed as a reference, but is data or a position")
            }
            TemplateError::Refs { made, given } => {
                write!(f, "a template made against {made} references is given {given}")
            }
            TemplateError::Place(at) => write!(f, "place {at} is past the end"),
            TemplateError::Extern(at) => write!(f, "outside symbol {at} is past the end of the template's own"),
            TemplateError::NotAnExtern(name) => {
                write!(f, "the backend named {name:?} of its own accord, which is empty or listed twice")
            }
        }
    }
}

pub const TEMPLATE_DOMAIN: &str = "blaze v2 template 1";

fn check_refs(refs: &Refs) -> Result<(), TemplateError> {
    match refs.iter().find(|it| matches!(it, Target::Data(_) | Target::Position(_))) {
        Some(it) => Err(TemplateError::NotAReference(it.clone())),
        None => Ok(()),
    }
}

/// The first place of `target` among `refs`.
fn place(refs: &Refs, target: &Target) -> Result<u32, TemplateError> {
    let found = refs.iter().position(|it| it == target);
    found.map(|it| it as u32).ok_or_else(|| TemplateError::NotReferenced(target.clone()))
}

/// `target` by its place among `refs`, else, for an outside symbol, among `externs`.
fn reference(refs: &Refs, externs: &[String], target: &Target) -> Result<Ref, TemplateError> {
    match place(refs, target) {
        Ok(at) => Ok(Ref::Place(at)),
        Err(missing) => match target {
            Target::Extern(name) => {
                externs.iter().position(|it| it == name).map(|at| Ref::Extern(at as u32)).ok_or(missing)
            }
            _ => Err(missing),
        },
    }
}

/// The outside symbols of `extra` that `refs` does not list, each once, in `extra`'s order.
fn own_externs(refs: &Refs, extra: &[String]) -> Result<Vec<String>, TemplateError> {
    let mut externs: Vec<String> = Vec::with_capacity(extra.len());
    for name in extra {
        if name.is_empty() || externs.contains(name) {
            return Err(TemplateError::NotAnExtern(name.clone()));
        }
        if !refs.iter().any(|it| matches!(it, Target::Extern(listed) if listed == name)) {
            externs.push(name.clone());
        }
    }
    Ok(externs)
}

fn template_relocs(
    relocs: &[Reloc],
    refs: &Refs,
    externs: &[String],
    data: &[Data],
    before: Option<u32>,
) -> Result<Vec<TemplateReloc>, TemplateError> {
    relocs
        .iter()
        .map(|reloc| {
            let target = match &reloc.target {
                Target::Data(id) => {
                    let Some(at) = data.iter().position(|it| it.id == *id) else {
                        return Err(TemplateError::ForeignData(*id));
                    };
                    let at = at as u32;
                    if let Some(data) = before
                        && at >= data
                    {
                        return Err(TemplateError::DataOrder { data, points_at: at });
                    }
                    Ref::Data(at)
                }
                Target::Position(span) => Ref::Position(*span),
                other => reference(refs, externs, other)?,
            };
            Ok(TemplateReloc { at: reloc.at, kind: reloc.kind, target, addend: reloc.addend })
        })
        .collect()
}

fn template_data(data: &[Data], refs: &Refs, externs: &[String]) -> Result<Vec<TemplateData>, TemplateError> {
    data.iter()
        .enumerate()
        .map(|(at, it)| {
            Ok(TemplateData {
                align: it.align,
                bytes: it.bytes.clone(),
                relocs: template_relocs(&it.relocs, refs, externs, data, Some(at as u32))?,
            })
        })
        .collect()
}

/// The data of a template with `refs` in its places, each with its content ID.
fn instantiate_data(data: &[TemplateData], refs: &Refs, externs: &[String]) -> Result<Vec<Data>, TemplateError> {
    let mut made: Vec<Data> = Vec::with_capacity(data.len());
    for (at, it) in data.iter().enumerate() {
        let relocs = instantiate_relocs(&it.relocs, refs, externs, &made, Some(at as u32))?;
        made.push(Data::new(it.align, it.bytes.clone(), relocs));
    }
    Ok(made)
}

fn instantiate_relocs(
    relocs: &[TemplateReloc],
    refs: &Refs,
    externs: &[String],
    data: &[Data],
    before: Option<u32>,
) -> Result<Vec<Reloc>, TemplateError> {
    relocs
        .iter()
        .map(|reloc| {
            let target = match reloc.target {
                Ref::Place(at) => refs.get(at as usize).cloned().ok_or(TemplateError::Place(at))?,
                Ref::Extern(at) => {
                    Target::Extern(externs.get(at as usize).cloned().ok_or(TemplateError::Extern(at))?)
                }
                Ref::Data(at) => {
                    if let Some(data) = before
                        && at >= data
                    {
                        return Err(TemplateError::DataOrder { data, points_at: at });
                    }
                    Target::Data(data.get(at as usize).ok_or(TemplateError::Place(at))?.id)
                }
                Ref::Position(span) => Target::Position(span),
            };
            Ok(Reloc { at: reloc.at, kind: reloc.kind, target, addend: reloc.addend })
        })
        .collect()
}

fn data_index(data: &[Data], id: ContentId) -> Result<u32, TemplateError> {
    match data.iter().position(|it| it.id == id) {
        Some(at) => Ok(at as u32),
        None => Err(TemplateError::ForeignData(id)),
    }
}

fn template_unwind(unwind: &Unwind, refs: &Refs, data: &[Data]) -> Result<TemplateUnwind, TemplateError> {
    let routine = |it: &Option<Target>| it.as_ref().map(|it| place(refs, it)).transpose();
    let lsda = |it: &Option<ContentId>| it.map(|it| data_index(data, it)).transpose();
    Ok(match unwind {
        Unwind::Dwarf { code_align, data_align, return_address, cie, fde, personality, lsda: area } => {
            TemplateUnwind::Dwarf {
                code_align: *code_align,
                data_align: *data_align,
                return_address: *return_address,
                cie: cie.clone(),
                fde: fde.clone(),
                personality: routine(personality)?,
                lsda: lsda(area)?,
            }
        }
        Unwind::Windows { info, handler, lsda: area } => {
            TemplateUnwind::Windows { info: info.clone(), handler: routine(handler)?, lsda: lsda(area)? }
        }
    })
}

fn instantiate_unwind(unwind: &TemplateUnwind, refs: &Refs, data: &[Data]) -> Result<Unwind, TemplateError> {
    let routine = |it: Option<u32>| {
        it.map(|at| refs.get(at as usize).cloned().ok_or(TemplateError::Place(at))).transpose()
    };
    let lsda =
        |it: Option<u32>| it.map(|at| data.get(at as usize).map(|it| it.id).ok_or(TemplateError::Place(at))).transpose();
    Ok(match unwind {
        TemplateUnwind::Dwarf { code_align, data_align, return_address, cie, fde, personality, lsda: area } => {
            Unwind::Dwarf {
                code_align: *code_align,
                data_align: *data_align,
                return_address: *return_address,
                cie: cie.clone(),
                fde: fde.clone(),
                personality: routine(*personality)?,
                lsda: lsda(*area)?,
            }
        }
        TemplateUnwind::Windows { info, handler, lsda: area } => {
            Unwind::Windows { info: info.clone(), handler: routine(*handler)?, lsda: lsda(*area)? }
        }
    })
}

fn refs_len(refs: &Refs) -> u32 {
    u32::try_from(refs.len()).expect("an instance refers to fewer than 2^32 things")
}

impl Template {
    /// `function` with each of its references by its place in `refs`. `extra` are the outside
    /// symbols its backend named of its own accord ([`crate::backend::Emitted::extra`]): a
    /// relocation to one of them that `refs` lacks is kept by name in the template.
    pub fn of(function: &CompiledFunction, refs: &Refs, extra: &[String]) -> Result<Template, TemplateError> {
        check_refs(refs)?;
        let externs = own_externs(refs, extra)?;
        Ok(Template {
            arch: function.arch,
            align: function.align,
            code: function.code.clone(),
            relocs: template_relocs(&function.relocs, refs, &externs, &function.data, None)?,
            data: template_data(&function.data, refs, &externs)?,
            inlined: function.inlined.clone(),
            side: function.side,
            unwind: function.unwind.as_ref().map(|it| template_unwind(it, refs, &function.data)).transpose()?,
            lines: function.lines.clone(),
            refs: refs_len(refs),
            externs,
        })
    }

    /// The function of instance `key` whose references are `refs`.
    pub fn instantiate(
        &self,
        key: InstanceKey,
        refs: &Refs,
    ) -> Result<CompiledFunction, TemplateError> {
        check_refs(refs)?;
        if refs_len(refs) != self.refs {
            return Err(TemplateError::Refs { made: self.refs, given: refs_len(refs) });
        }
        let data = instantiate_data(&self.data, refs, &self.externs)?;
        Ok(CompiledFunction {
            key,
            arch: self.arch,
            align: self.align,
            code: self.code.clone(),
            relocs: instantiate_relocs(&self.relocs, refs, &self.externs, &data, None)?,
            unwind: self.unwind.as_ref().map(|it| instantiate_unwind(it, refs, &data)).transpose()?,
            data,
            inlined: self.inlined.clone(),
            side: self.side,
            lines: self.lines.clone(),
        })
    }

    /// The template's content ID: two templates with one are the same function up to what
    /// fills their places.
    pub fn id(&self) -> ContentId {
        let mut hasher = Hasher::new(TEMPLATE_DOMAIN);
        hasher.update(&[content::arch_tag(self.arch)]);
        hasher.update(&self.align.to_le_bytes()).part(&self.code);
        template_relocs_into(&mut hasher, &self.relocs);
        template_data_into(&mut hasher, &self.data);
        hasher.update(&(self.inlined.len() as u64).to_le_bytes());
        for code in &self.inlined {
            hasher.id(&code.0);
        }
        match &self.side {
            Some(side) => hasher.update(&[1]).id(side),
            None => hasher.update(&[0]),
        };
        let index = |hasher: &mut Hasher, it: &Option<u32>| match it {
            Some(at) => {
                hasher.update(&[1]).update(&at.to_le_bytes());
            }
            None => {
                hasher.update(&[0]);
            }
        };
        match &self.unwind {
            None => {
                hasher.update(&[0]);
            }
            Some(TemplateUnwind::Dwarf { code_align, data_align, return_address, cie, fde, personality, lsda }) => {
                hasher.update(&[1]).update(&code_align.to_le_bytes()).update(&data_align.to_le_bytes());
                hasher.update(&return_address.to_le_bytes()).part(cie).part(fde);
                index(&mut hasher, personality);
                index(&mut hasher, lsda);
            }
            Some(TemplateUnwind::Windows { info, handler, lsda }) => {
                hasher.update(&[2]).part(info);
                index(&mut hasher, handler);
                index(&mut hasher, lsda);
            }
        }
        content::lines_into(&mut hasher, &self.lines);
        hasher.update(&self.refs.to_le_bytes());
        hasher.update(&(self.externs.len() as u64).to_le_bytes());
        for name in &self.externs {
            hasher.part(name.as_bytes());
        }
        hasher.finish()
    }
}

impl StaticTemplate {
    pub fn of(value: &Static, refs: &Refs) -> Result<StaticTemplate, TemplateError> {
        check_refs(refs)?;
        let init = match &value.init {
            Init::Bytes { bytes, relocs } => {
                TemplateInit::Bytes { bytes: bytes.clone(), relocs: template_relocs(relocs, refs, &[], &value.data, None)? }
            }
            Init::Zeroed { size } => TemplateInit::Zeroed { size: *size },
        };
        Ok(StaticTemplate {
            align: value.align,
            init,
            data: template_data(&value.data, refs, &[])?,
            mutable: value.mutable,
            thread_local: value.thread_local,
            section: value.section.clone(),
            refs: refs_len(refs),
        })
    }

    pub fn instantiate(&self, item: ItemKey, instance: ContentId, refs: &Refs) -> Result<Static, TemplateError> {
        check_refs(refs)?;
        if refs_len(refs) != self.refs {
            return Err(TemplateError::Refs { made: self.refs, given: refs_len(refs) });
        }
        let data = instantiate_data(&self.data, refs, &[])?;
        let init = match &self.init {
            TemplateInit::Bytes { bytes, relocs } => {
                Init::Bytes { bytes: bytes.clone(), relocs: instantiate_relocs(relocs, refs, &[], &data, None)? }
            }
            TemplateInit::Zeroed { size } => Init::Zeroed { size: *size },
        };
        Ok(Static {
            item,
            instance,
            align: self.align,
            init,
            data,
            mutable: self.mutable,
            thread_local: self.thread_local,
            section: self.section.clone(),
        })
    }
}

fn template_relocs_into(hasher: &mut Hasher, relocs: &[TemplateReloc]) {
    hasher.update(&(relocs.len() as u64).to_le_bytes());
    for reloc in relocs {
        hasher.update(&reloc.at.to_le_bytes());
        hasher.update(&[content::kind_tag(reloc.kind)]);
        hasher.update(&reloc.addend.to_le_bytes());
        match reloc.target {
            Ref::Place(at) => hasher.update(&[0]).update(&at.to_le_bytes()),
            Ref::Data(at) => hasher.update(&[1]).update(&at.to_le_bytes()),
            Ref::Position(SpanRef { anchor, index }) => {
                hasher.update(&[2]).id(&anchor).update(&index.to_le_bytes())
            }
            Ref::Extern(at) => hasher.update(&[3]).update(&at.to_le_bytes()),
        };
    }
}

fn template_data_into(hasher: &mut Hasher, data: &[TemplateData]) {
    hasher.update(&(data.len() as u64).to_le_bytes());
    for it in data {
        hasher.update(&it.align.to_le_bytes()).part(&it.bytes);
        template_relocs_into(hasher, &it.relocs);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(n: u8) -> InstanceKey {
        InstanceKey(ContentId([n; 32]))
    }

    fn call(at: u32, to: Target) -> Reloc {
        Reloc { at, kind: RelocKind::PcRel32, target: to, addend: -4 }
    }

    /// `Vec<A>::reserve` calling `Vec<A>::try_grow_to` and reading a constant that points at
    /// a static.
    fn reserve(own: InstanceKey, grow: InstanceKey, item: ItemKey) -> (CompiledFunction, Vec<Target>) {
        let refs = vec![
            Target::Function(grow),
            Target::Static { item, instance: ContentId([9; 32]) },
            Target::Extern("memcpy".to_owned()),
        ];
        let inner = Data::new(8, vec![1, 2, 3, 4, 5, 6, 7, 8], vec![]);
        let outer = Data::new(
            8,
            vec![0; 16],
            vec![
                Reloc { at: 0, kind: RelocKind::Abs64, target: Target::Data(inner.id), addend: 0 },
                Reloc { at: 8, kind: RelocKind::Abs64, target: refs[0].clone(), addend: 0 },
            ],
        );
        let function = CompiledFunction {
            key: own,
            arch: Arch::X86_64,
            align: 16,
            code: vec![0xe8, 0, 0, 0, 0, 0xe8, 0, 0, 0, 0, 0xc3],
            relocs: vec![
                call(1, refs[0].clone()),
                call(6, refs[2].clone()),
                Reloc { at: 6, kind: RelocKind::Abs64, target: Target::Data(outer.id), addend: 0 },
                Reloc { at: 1, kind: RelocKind::Abs64, target: refs[1].clone(), addend: 8 },
            ],
            data: vec![inner, outer],
            inlined: Vec::new(),
            side: None,
            unwind: None,
            lines: vec![Line { at: 0, span: SpanRef { anchor: ContentId([5; 32]), index: 2 } }],
        };
        (function, refs)
    }

    #[test]
    fn unwinding_names_its_personality_by_place_and_its_lsda_by_index() {
        let (mut function, mut refs) = reserve(key(1), key(2), ItemKey(ContentId([3; 32])));
        refs.push(Target::Extern("rust_eh_personality".to_owned()));
        let lsda = function.data[0].id;
        function.unwind = Some(Unwind::Dwarf {
            code_align: 1,
            data_align: -8,
            return_address: 16,
            cie: vec![0x0c, 7, 8],
            fde: vec![0x0e, 16],
            personality: Some(refs[3].clone()),
            lsda: Some(lsda),
        });
        let template = Template::of(&function, &refs, &[]).unwrap();
        assert!(matches!(
            template.unwind,
            Some(TemplateUnwind::Dwarf { personality: Some(3), lsda: Some(0), .. })
        ));
        assert_eq!(template.instantiate(function.key, &refs).unwrap(), function);

        let mut foreign = function.clone();
        foreign.unwind = Some(Unwind::Windows { info: vec![1], handler: None, lsda: Some(ContentId([8; 32])) });
        assert_eq!(Template::of(&foreign, &refs, &[]).unwrap_err(), TemplateError::ForeignData(ContentId([8; 32])));
        let mut unlisted = function;
        unlisted.unwind = Some(Unwind::Windows {
            info: vec![1],
            handler: Some(Target::Extern("__CxxFrameHandler3".to_owned())),
            lsda: None,
        });
        assert!(matches!(Template::of(&unlisted, &refs, &[]), Err(TemplateError::NotReferenced(_))));
    }

    #[test]
    fn a_template_gives_back_its_function() {
        let (function, refs) = reserve(key(1), key(2), ItemKey(ContentId([3; 32])));
        let template = Template::of(&function, &refs, &[]).unwrap();
        assert_eq!(template.instantiate(function.key, &refs).unwrap(), function);
    }

    #[test]
    fn a_symbol_the_backend_named_is_kept_by_name() {
        let (mut function, refs) = reserve(key(1), key(2), ItemKey(ContentId([3; 32])));
        let bcmp = Target::Extern("bcmp".to_owned());
        function.relocs.push(call(1, bcmp.clone()));
        assert_eq!(Template::of(&function, &refs, &[]).unwrap_err(), TemplateError::NotReferenced(bcmp));
        // `memcpy` is listed already, so it stays a place.
        let extra = ["memcpy".to_owned(), "bcmp".to_owned()];
        let template = Template::of(&function, &refs, &extra).unwrap();
        assert_eq!(template.externs, ["bcmp".to_owned()]);
        assert_eq!(template.relocs.last().unwrap().target, Ref::Extern(0));
        assert_eq!(template.instantiate(function.key, &refs).unwrap(), function);
        assert_ne!(template.id(), Template { externs: vec!["memcmp".to_owned()], ..template.clone() }.id());
        let twice = ["bcmp".to_owned(), "bcmp".to_owned()];
        assert_eq!(
            Template::of(&function, &refs, &twice).unwrap_err(),
            TemplateError::NotAnExtern("bcmp".to_owned())
        );
    }

    #[test]
    fn siblings_up_to_callee_renaming_share_one_template() {
        let (a, a_refs) = reserve(key(1), key(2), ItemKey(ContentId([3; 32])));
        let (b, b_refs) = reserve(key(4), key(5), ItemKey(ContentId([6; 32])));
        let (ta, tb) = (Template::of(&a, &a_refs, &[]).unwrap(), Template::of(&b, &b_refs, &[]).unwrap());
        assert_eq!(ta, tb);
        assert_eq!(ta.id(), tb.id());
        // `b`'s data points at its callee, so its ID is made again from the new place.
        assert_eq!(ta.instantiate(b.key, &b_refs).unwrap(), b);
    }

    #[test]
    fn a_constant_is_not_a_place() {
        let (a, refs) = reserve(key(1), key(2), ItemKey(ContentId([3; 32])));
        let mut b = a.clone();
        b.code[10] = 0xcc;
        assert_ne!(Template::of(&a, &refs, &[]).unwrap().id(), Template::of(&b, &refs, &[]).unwrap().id());
    }

    #[test]
    fn what_is_not_referenced_is_refused() {
        let (function, refs) = reserve(key(1), key(2), ItemKey(ContentId([3; 32])));
        assert_eq!(
            Template::of(&function, &refs[..1], &[]).unwrap_err(),
            TemplateError::NotReferenced(refs[2].clone())
        );
        let template = Template::of(&function, &refs, &[]).unwrap();
        assert_eq!(
            template.instantiate(function.key, &refs[..2]).unwrap_err(),
            TemplateError::Refs { made: 3, given: 2 }
        );
    }

    #[test]
    fn data_out_of_order_is_refused() {
        let (mut function, refs) = reserve(key(1), key(2), ItemKey(ContentId([3; 32])));
        function.data.reverse();
        assert!(matches!(
            Template::of(&function, &refs, &[]),
            Err(TemplateError::DataOrder { data: 0, points_at: 1 })
        ));
    }

    #[test]
    fn a_static_keeps_what_it_is() {
        let item = ItemKey(ContentId([3; 32]));
        let refs = vec![Target::Function(key(2))];
        let text = Data::new(1, b"hi".to_vec(), Vec::new());
        let value = Static {
            item,
            instance: ContentId([4; 32]),
            align: 8,
            init: Init::Bytes {
                bytes: vec![0; 24],
                relocs: vec![
                    Reloc { at: 0, kind: RelocKind::Abs64, target: refs[0].clone(), addend: 0 },
                    Reloc { at: 8, kind: RelocKind::Abs64, target: Target::Data(text.id), addend: 0 },
                ],
            },
            data: vec![text],
            mutable: true,
            thread_local: false,
            section: Some(".init_array".to_owned()),
        };
        let template = StaticTemplate::of(&value, &refs).unwrap();
        assert_eq!(template.instantiate(item, value.instance, &refs).unwrap(), value);

        let zeroed = Static { init: Init::Zeroed { size: 1 << 20 }, data: Vec::new(), ..value };
        let template = StaticTemplate::of(&zeroed, &refs).unwrap();
        assert_eq!(template.instantiate(item, zeroed.instance, &refs).unwrap(), zeroed);
    }
}
