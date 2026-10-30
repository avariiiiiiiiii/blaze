//! What a debugger is told of a function beyond its line rows ([`blaze_codegen::Line`]): its
//! named locals and where they are while it runs. Kept in the store under
//! [`blaze_codegen::CompiledFunction::side`], in postcard, versioned by [`VERSION`].

use std::borrow::Cow;

use blaze_ids::ContentId;
use intern::Symbol;
use serde_derive::{Deserialize, Serialize};

use crate::debuginfo::{LineRow, SlotPlaces, VariablePlace};

/// The encoding's version: a change to what is below is a new one.
pub(crate) const VERSION: u32 = 2;

const SIDE_DOMAIN: &str = "blaze v2 side 2";

/// A named local of a function, as its IR was built: in a stack slot, or at an address the
/// slot holds. 64 bytes; its name is the body's interned one, a pointer to it.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub(crate) struct Variable {
    pub(crate) name: Symbol,
    pub(crate) param: bool,
    /// Its type by content ([`super::Hashes::ty`]).
    pub(crate) ty: ContentId,
    pub(crate) slot: u32,
    /// The slot holds an address, the local is this far from it.
    pub(crate) pointer: Option<u32>,
    /// The scope its name is known in, among [`Scopes::parents`]: `None` is all of the
    /// function, as a parameter's is.
    pub(crate) scope: Option<u32>,
}

#[cfg(target_pointer_width = "64")]
const _: () = assert!(size_of::<Variable>() == 64);

/// The scopes of a function's body its named locals are bound in, as the body says them:
/// which code each is of is known once the code is, from its line rows.
#[derive(Clone, Default, PartialEq, Eq, Hash, Debug)]
pub(crate) struct Scopes {
    /// The scope each is inside of, which comes before it.
    pub(crate) parents: Vec<Option<u32>>,
    /// By the statement a line row is of (`LineRow::span`): which scope its code is of.
    pub(crate) of_span: Vec<SpanScope>,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) enum SpanScope {
    /// Of the scope of the code before it: code of no expression, or of a pattern.
    Before,
    /// Of none of the scopes.
    Outside,
    /// Of this scope and those it is inside of.
    In(u32),
}

/// The table as written, borrowing what it tells of, and as read, borrowing the names from
/// the stored bytes.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub(crate) struct Side<'a> {
    pub(crate) version: u32,
    /// The DWARF expression of what [`Place::Frame`] and `DW_OP_fbreg` count from.
    #[serde(borrow)]
    pub(crate) frame_base: Cow<'a, [u8]>,
    pub(crate) scopes: Vec<Scope>,
    #[serde(borrow)]
    pub(crate) variables: Vec<Told<'a>>,
}

/// A part of the function its names are looked up in: a block, what follows a `let`.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub(crate) struct Scope {
    pub(crate) parent: Option<u32>,
    /// The code of the scope and of the scopes inside it, from the first offset up to the
    /// second, in the order of the offsets.
    pub(crate) ranges: Vec<(u32, u32)>,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub(crate) struct Told<'a> {
    pub(crate) name: &'a str,
    pub(crate) param: bool,
    pub(crate) ty: ContentId,
    pub(crate) place: Place,
    pub(crate) scope: Option<u32>,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub(crate) enum Place {
    /// This far from the frame base, all through the function.
    Frame(i64),
    /// What a DWARF expression says, from the first code offset up to the second.
    Ranges(Vec<(u32, u32, Vec<u8>)>),
}

/// The side table of a function whose variables Cranelift put where `slots` says, with its
/// content ID; `None` for one with nothing to tell. The code of each scope is from `lines`,
/// the function's line rows, and `len`, where its code ends.
pub(crate) fn encode(
    variables: &[Variable],
    scopes: &Scopes,
    slots: &SlotPlaces,
    lines: &[LineRow],
    len: u32,
) -> Result<Option<(Vec<u8>, ContentId)>, String> {
    let mut told = Vec::with_capacity(variables.len());
    told.extend(variables.iter().filter_map(|variable| {
        let place = slots.of(variable.slot)?;
        let place = match variable.pointer {
            Some(offset) => place.pointee(offset),
            None => place.clone(),
        };
        let place = match place {
            VariablePlace::Frame(offset) => Place::Frame(offset),
            VariablePlace::Ranges(ranges) => Place::Ranges(ranges),
        };
        Some(Told {
            name: variable.name.as_str(),
            param: variable.param,
            ty: variable.ty,
            place,
            scope: variable.scope,
        })
    }));
    if told.is_empty() {
        return Ok(None);
    }
    let scopes = ranges(scopes, lines, len)?;
    let frame_base = Cow::Borrowed(&slots.frame_base[..]);
    let side = Side { version: VERSION, frame_base, scopes, variables: told };
    let bytes = postcard::to_allocvec(&side).map_err(|e| e.to_string())?;
    let id = ContentId::of(SIDE_DOMAIN, &bytes);
    Ok(Some((bytes, id)))
}

/// [`encode`] of a function's `variables` and `scopes`, where its `code` put them.
pub(crate) fn of(
    variables: &[Variable],
    scopes: &Scopes,
    code: &crate::fragment::Code,
) -> Result<Option<(Vec<u8>, ContentId)>, String> {
    let len = u32::try_from(code.bytes.len()).map_err(|e| e.to_string())?;
    encode(variables, scopes, &code.slots, &code.lines, len)
}

/// The code of each scope: that of each line row whose statement is of it or of a scope
/// inside it, up to the next row's offset or `len`.
fn ranges(scopes: &Scopes, lines: &[LineRow], len: u32) -> Result<Vec<Scope>, String> {
    let mut out: Vec<Scope> =
        scopes.parents.iter().map(|&parent| Scope { parent, ranges: Vec::new() }).collect();
    if out.is_empty() {
        return Ok(out);
    }
    let mut scope = None;
    let ends = lines.iter().skip(1).map(|row| row.offset).chain([len]);
    for (row, to) in lines.iter().zip(ends) {
        match scopes.of_span.get(row.span as usize) {
            Some(SpanScope::Before) => {}
            Some(SpanScope::Outside) => scope = None,
            Some(&SpanScope::In(it)) => scope = Some(it),
            None => return Err(format!("a line row of statement {}, which has no scope", row.span)),
        }
        let from = row.offset;
        if from >= to {
            continue;
        }
        let mut at = scope;
        // Each scope's parent comes before it, so the chain ends within as many steps.
        for _ in 0..out.len() {
            let Some(index) = at else { break };
            let Some(Scope { parent, ranges }) = out.get_mut(index as usize) else {
                return Err(format!("scope {index} of {}", scopes.parents.len()));
            };
            match ranges.last_mut() {
                Some(last) if last.1 == from => last.1 = to,
                _ => ranges.push((from, to)),
            }
            at = *parent;
        }
    }
    Ok(out)
}

/// The table in `bytes`, its names borrowed from them.
pub(crate) fn decode(bytes: &[u8]) -> Result<Side<'_>, String> {
    let side: Side<'_> = postcard::from_bytes(bytes).map_err(|e| e.to_string())?;
    if side.version != VERSION {
        return Err(format!("a side table of version {}, not {VERSION}", side.version));
    }
    Ok(side)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_side_table_reads_back() {
        let slots = SlotPlaces {
            frame_base: vec![0x56],
            slots: vec![(0, VariablePlace::Frame(-16)), (2, VariablePlace::Frame(8))],
        };
        let ty = ContentId::of("test", b"u32");
        let variable = |name: &str, slot, pointer| Variable {
            name: Symbol::intern(name),
            param: false,
            ty,
            slot,
            pointer,
            scope: None,
        };
        let variables = [variable("a", 0, None), variable("gone", 1, None), variable("b", 2, Some(4))];
        let encoded = || encode(&variables, &Scopes::default(), &slots, &[], 16);
        let (bytes, id) = encoded().expect("encoded").expect("variables");
        let side = decode(&bytes).expect("decoded");
        let names: Vec<_> = side.variables.iter().map(|it| it.name).collect();
        assert_eq!(names, ["a", "b"]);
        assert_eq!(side.variables[0].place, Place::Frame(-16));
        assert!(matches!(side.variables[1].place, Place::Ranges(_)));
        assert_eq!(encoded().expect("encoded").expect("variables").1, id);
    }

    #[test]
    fn nothing_to_tell_is_no_table() {
        assert_eq!(encode(&[], &Scopes::default(), &SlotPlaces::default(), &[], 0), Ok(None));
    }

    /// A scope's code is that of its rows and of its inner scopes', joined where they meet;
    /// a row of no expression stays in the scope before it.
    #[test]
    fn a_scope_is_the_code_of_its_rows() {
        let scopes = Scopes {
            parents: vec![None, Some(0)],
            of_span: vec![SpanScope::Outside, SpanScope::In(0), SpanScope::In(1), SpanScope::Before],
        };
        let row = |offset, span| LineRow { offset, span };
        let lines = [row(0, 0), row(4, 1), row(8, 2), row(12, 3), row(16, 0)];
        let ranges = ranges(&scopes, &lines, 20).expect("the ranges");
        assert_eq!(ranges[0], Scope { parent: None, ranges: vec![(4, 16)] });
        assert_eq!(ranges[1], Scope { parent: Some(0), ranges: vec![(8, 16)] });
        let unknown = [row(0, 9)];
        assert!(super::ranges(&scopes, &unknown, 4).is_err());
    }
}
