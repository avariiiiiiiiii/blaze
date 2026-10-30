//! What a debugger is told of a function beyond its line rows ([`blaze_codegen::Line`]): its
//! named locals and where they are while it runs. Kept in the store under
//! [`blaze_codegen::CompiledFunction::side`], in postcard, versioned by [`VERSION`].

use blaze_ids::ContentId;
use serde_derive::{Deserialize, Serialize};

use crate::debuginfo::{SlotPlaces, VariablePlace};

/// The encoding's version: a change to what is below is a new one.
pub(crate) const VERSION: u32 = 1;

const SIDE_DOMAIN: &str = "blaze v2 side 1";

/// A named local of a function, as its IR was built: in a stack slot, or at an address the
/// slot holds.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub(crate) struct Variable {
    pub(crate) name: String,
    pub(crate) param: bool,
    /// Its type by content ([`super::Hashes::ty`]).
    pub(crate) ty: ContentId,
    pub(crate) slot: u32,
    /// The slot holds an address, the local is this far from it.
    pub(crate) pointer: Option<u32>,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub(crate) struct Side {
    pub(crate) version: u32,
    /// The DWARF expression of what [`Place::Frame`] and `DW_OP_fbreg` count from.
    pub(crate) frame_base: Vec<u8>,
    pub(crate) variables: Vec<Told>,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub(crate) struct Told {
    pub(crate) name: String,
    pub(crate) param: bool,
    pub(crate) ty: ContentId,
    pub(crate) place: Place,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub(crate) enum Place {
    /// This far from the frame base, all through the function.
    Frame(i64),
    /// What a DWARF expression says, from the first code offset up to the second.
    Ranges(Vec<(u32, u32, Vec<u8>)>),
}

/// The side table of a function whose variables Cranelift put where `slots` says, with its
/// content ID; `None` for one with nothing to tell.
pub(crate) fn encode(
    variables: &[Variable],
    slots: &SlotPlaces,
) -> Result<Option<(Vec<u8>, ContentId)>, String> {
    let told: Vec<Told> = variables
        .iter()
        .filter_map(|variable| {
            let place = slots.of(variable.slot)?;
            let place = match variable.pointer {
                Some(offset) => place.pointee(offset),
                None => place.clone(),
            };
            let place = match place {
                VariablePlace::Frame(offset) => Place::Frame(offset),
                VariablePlace::Ranges(ranges) => Place::Ranges(ranges),
            };
            Some(Told { name: variable.name.clone(), param: variable.param, ty: variable.ty, place })
        })
        .collect();
    if told.is_empty() {
        return Ok(None);
    }
    let side = Side { version: VERSION, frame_base: slots.frame_base.clone(), variables: told };
    let bytes = postcard::to_allocvec(&side).map_err(|e| e.to_string())?;
    let id = ContentId::of(SIDE_DOMAIN, &bytes);
    Ok(Some((bytes, id)))
}

pub(crate) fn decode(bytes: &[u8]) -> Result<Side, String> {
    let side: Side = postcard::from_bytes(bytes).map_err(|e| e.to_string())?;
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
            name: name.to_owned(),
            param: false,
            ty,
            slot,
            pointer,
        };
        let variables = [variable("a", 0, None), variable("gone", 1, None), variable("b", 2, Some(4))];
        let (bytes, id) = encode(&variables, &slots).expect("encoded").expect("variables");
        let side = decode(&bytes).expect("decoded");
        let names: Vec<_> = side.variables.iter().map(|it| it.name.as_str()).collect();
        assert_eq!(names, ["a", "b"]);
        assert_eq!(side.variables[0].place, Place::Frame(-16));
        assert!(matches!(side.variables[1].place, Place::Ranges(_)));
        assert_eq!(encode(&variables, &slots).expect("encoded").expect("variables").1, id);
    }

    #[test]
    fn nothing_to_tell_is_no_table() {
        assert_eq!(encode(&[], &SlotPlaces::default()), Ok(None));
    }
}
