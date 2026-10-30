//! Folding functions that differ only in constants (#1292), after Relax's `merge_similar`.
//!
//! Monomorphic copies of one generic body are often the same IR except for a few constants:
//! element sizes, alignments, the addresses of the functions and statics they name. A backend
//! gives each prepared function with those constants masked ([`Masked`]); [`groups`] puts
//! together the functions whose masked IR and settings are equal, and a group whose members
//! differ in few enough constants, and save more than their thunks cost, becomes one body
//! that takes the differing constants as trailing parameters, with each member a thunk that
//! calls it with its own ([`Merged`]).
//!
//! Every member becomes a thunk: callers name members by their keys and are never rewritten,
//! so a member's address, key and signature stay what they were.
//!
//! Dev builds only (Cranelift units with [`crate::settings::Spec::fold`]): a varying function
//! becomes an indirect call in the body.

use std::collections::BTreeMap;

use blaze_ids::{ContentId, Hasher};
use serde::{Deserialize, Serialize};

use crate::{InstanceKey, Settings, Target, backend::Emitted, content::target_id, template::Refs};

/// A constant of a function's IR that can become a parameter.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum Constant {
    /// An integer immediate of `bits` bits, zero-extended.
    Int { bits: u8, value: u128 },
    /// A float immediate's bits.
    F32(u32),
    F64(u64),
    /// The address of the `n`th of the function's references (a function or a static): whether
    /// two members' are equal is whether their targets are, not their places.
    Place(u32),
}

/// A prepared function's IR with its constants masked: what [`groups`] compares.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct Masked {
    /// The IR's canonical bytes, each constant written as its index among `constants`.
    pub shape: Vec<u8>,
    /// Its constants, in the order the shape names them.
    pub constants: Vec<Constant>,
    /// Its number of instructions.
    pub size: u32,
}

/// Functions that fold into one body.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Group {
    /// The merged body's shape: the masked IR, every constant equal for all members with its
    /// value (a place by its target), and which parameter each varying one reads.
    pub shape: ContentId,
    pub settings: Settings,
    /// The indices among [`Masked::constants`] whose values differ between members, in order.
    pub varying: Vec<u32>,
    /// Sorted by key.
    pub members: Vec<InstanceKey>,
    /// `constants[i]` are member `i`'s values of `varying`, in its order.
    pub constants: Vec<Vec<Constant>>,
}

pub const FOLDED_DOMAIN: &str = "blaze v2 folded 1";
const MASKED_DOMAIN: &str = "blaze v2 masked 1";
const FOLD_SHAPE_DOMAIN: &str = "blaze v2 fold shape 1";

impl Group {
    /// The merged body's key: from what the body is, so one group anywhere is one body.
    pub fn key(&self) -> InstanceKey {
        let mut hasher = Hasher::new(FOLDED_DOMAIN);
        hasher.id(&self.shape).id(&self.settings.0);
        hasher.update(&(self.varying.len() as u64).to_le_bytes());
        for at in &self.varying {
            hasher.update(&at.to_le_bytes());
        }
        InstanceKey(hasher.finish())
    }

    /// The parameter of the merged body each of `varying` reads, and how many parameters
    /// there are: varying constants whose values are equal for every member are one.
    pub fn parameters(&self) -> (Vec<u32>, u32) {
        let columns: Vec<Vec<Constant>> = (0..self.varying.len())
            .map(|at| self.constants.iter().map(|it| it[at]).collect())
            .collect();
        parameters(&columns)
    }

    /// The references of member `member`'s thunk: the member's own, then the merged body,
    /// [`Target::Direct`] of [`Group::key`], which the thunk calls.
    pub fn thunk_refs(&self, own: &Refs) -> Vec<Target> {
        let mut refs = own.to_vec();
        refs.push(Target::Direct(self.key()));
        refs
    }
}

/// For each column of values, the parameter it is: the first column equal to it.
fn parameters(columns: &[Vec<Constant>]) -> (Vec<u32>, u32) {
    let mut params: Vec<&Vec<Constant>> = Vec::new();
    let mut reads = Vec::with_capacity(columns.len());
    for column in columns {
        let param = match params.iter().position(|&it| it == column) {
            Some(param) => param,
            None => {
                params.push(column);
                params.len() - 1
            }
        };
        reads.push(param as u32);
    }
    (reads, params.len() as u32)
}

/// What [`groups`] makes a group of.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Limits {
    /// The most parameters a merged body takes.
    pub max_params: u32,
    /// The fewest members of a group.
    pub min_members: u32,
    /// What a thunk costs, in instructions, besides one per constant it passes: Relax's
    /// 6 bytes (a call and a return) at its 3 bytes an instruction.
    pub thunk_size: u32,
}

impl Default for Limits {
    fn default() -> Limits {
        Limits { max_params: 16, min_members: 2, thunk_size: 2 }
    }
}

/// A group compiled: one body and a thunk per member.
///
/// `body.function.key` is [`Group::key`]; its references are the first member's (it names
/// only places whose targets are equal for every member, the varying ones being parameters).
/// Thunk `i`'s key is `group.members[i]`; its references are [`Group::thunk_refs`] of member
/// `i`'s: its own, then [`Target::Direct`] of the body. The driver makes their templates
/// against exactly those.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Merged {
    pub body: Emitted,
    pub thunks: Vec<Emitted>,
}

/// A constant as members are compared by: a place by its target.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
enum Cell {
    Value(Constant),
    Target(ContentId),
}

fn cell(key: InstanceKey, constant: Constant, refs: &Refs) -> Cell {
    match constant {
        Constant::Place(at) => {
            let Some(target) = refs.get(at as usize) else {
                panic!("{key:?}'s masked IR names place {at} of its {} references", refs.len())
            };
            Cell::Target(target_id(target))
        }
        other => Cell::Value(other),
    }
}

fn cell_into(hasher: &mut Hasher, cell: &Cell) {
    match *cell {
        Cell::Value(Constant::Int { bits, value }) => {
            hasher.update(&[0, bits]).update(&value.to_le_bytes())
        }
        Cell::Value(Constant::F32(bits)) => hasher.update(&[1]).update(&bits.to_le_bytes()),
        Cell::Value(Constant::F64(bits)) => hasher.update(&[2]).update(&bits.to_le_bytes()),
        Cell::Value(Constant::Place(at)) => hasher.update(&[3]).update(&at.to_le_bytes()),
        Cell::Target(id) => hasher.update(&[4]).id(&id),
    };
}

/// What a constant is, without its value: members whose constants differ in it never group.
fn kind(constant: &Constant) -> [u8; 2] {
    match *constant {
        Constant::Int { bits, .. } => [0, bits],
        Constant::F32(_) => [1, 0],
        Constant::F64(_) => [2, 0],
        Constant::Place(_) => [3, 0],
    }
}

struct Candidate<'a> {
    key: InstanceKey,
    masked: &'a Masked,
    cells: Vec<Cell>,
}

/// What a set of members would fold as.
struct Plan {
    varying: Vec<u32>,
    reads: Vec<u32>,
    params: u32,
}

fn plan(bucket: &[Candidate<'_>], members: &[usize]) -> Plan {
    let first = &bucket[members[0]];
    let varying: Vec<u32> = (0..first.cells.len())
        .filter(|&at| members.iter().any(|&it| bucket[it].cells[at] != first.cells[at]))
        .map(|at| at as u32)
        .collect();
    let columns: Vec<Vec<Constant>> = varying
        .iter()
        .map(|&at| members.iter().map(|&it| bucket[it].masked.constants[at as usize]).collect())
        .collect();
    let (reads, params) = parameters(&columns);
    Plan { varying, reads, params }
}

/// Relax's test: the bodies folded away save more than the thunks cost, and the body grows
/// by an eighth for the parameters it keeps in registers.
fn profitable(size: u32, members: usize, params: u32, limits: &Limits) -> bool {
    let (size, members) = (u64::from(size), members as u64);
    let saved = size * (members - 1);
    let thunks = members * (u64::from(limits.thunk_size) + u64::from(params));
    let body = if params == 0 { 0 } else { size / 8 };
    saved > thunks + body
}

/// The functions among `candidates` that fold, each group with its members sorted by key;
/// the same groups in the same order whatever the order of `candidates`.
///
/// Members of one group have equal settings and masked IR, and equal values of every
/// constant but `varying` (a place by its target: places whose targets are equal for every
/// member are not varying). A group's members differ in at least one constant (functions
/// equal in all are code dedupe's and the memo's) and in at most [`Limits::max_params`]
/// parameters, and pass [`profitable`]. Functions of one masked IR whose constants differ in
/// too many are split, each taking the first group, in key order, it fits.
pub fn groups(
    candidates: &[(InstanceKey, Settings, &Masked, &Refs)],
    limits: &Limits,
) -> Vec<Group> {
    let mut buckets: BTreeMap<(Settings, ContentId), Vec<Candidate<'_>>> = BTreeMap::new();
    for &(key, settings, masked, refs) in candidates {
        let mut hasher = Hasher::new(MASKED_DOMAIN);
        hasher.part(&masked.shape).update(&masked.size.to_le_bytes());
        hasher.update(&(masked.constants.len() as u64).to_le_bytes());
        for constant in &masked.constants {
            hasher.update(&kind(constant));
        }
        let cells = masked.constants.iter().map(|&it| cell(key, it, refs)).collect();
        buckets.entry((settings, hasher.finish())).or_default().push(Candidate {
            key,
            masked,
            cells,
        });
    }
    let mut groups = Vec::new();
    for ((settings, masked_id), mut bucket) in buckets {
        bucket.sort_by_key(|it| it.key);
        bucket.dedup_by_key(|it| it.key);
        if bucket.len() < limits.min_members as usize {
            continue;
        }
        let mut parts: Vec<Vec<usize>> = Vec::new();
        for member in 0..bucket.len() {
            let fits = parts.iter().position(|part| {
                let mut with = part.clone();
                with.push(member);
                plan(&bucket, &with).params <= limits.max_params
            });
            match fits {
                Some(at) => parts[at].push(member),
                None => parts.push(vec![member]),
            }
        }
        for part in parts {
            if part.len() < limits.min_members as usize {
                continue;
            }
            let Plan { varying, reads, params } = plan(&bucket, &part);
            let size = bucket[part[0]].masked.size;
            if params == 0 || !profitable(size, part.len(), params, limits) {
                continue;
            }
            let mut hasher = Hasher::new(FOLD_SHAPE_DOMAIN);
            hasher.id(&masked_id);
            let mut reads_of = varying.iter().zip(&reads).peekable();
            for (at, it) in bucket[part[0]].cells.iter().enumerate() {
                if let Some((_, param)) = reads_of.next_if(|(v, _)| **v as usize == at) {
                    hasher.update(&[5]).update(&param.to_le_bytes());
                } else {
                    cell_into(&mut hasher, it);
                }
            }
            let constants = part
                .iter()
                .map(|&it| {
                    let constants = &bucket[it].masked.constants;
                    varying.iter().map(|&at| constants[at as usize]).collect()
                })
                .collect();
            groups.push(Group {
                shape: hasher.finish(),
                settings,
                varying,
                members: part.iter().map(|&it| bucket[it].key).collect(),
                constants,
            });
        }
    }
    groups.sort_by_key(|it| it.members[0]);
    groups
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(n: u8) -> InstanceKey {
        InstanceKey(ContentId([n; 32]))
    }

    fn settings(n: u8) -> Settings {
        Settings(ContentId([n; 32]))
    }

    fn int(value: u128) -> Constant {
        Constant::Int { bits: 64, value }
    }

    /// `Vec<T>::grow` with `T`'s size and alignment, calling place 0 and taking place 1's
    /// address.
    fn grow(size: u128, align: u128) -> Masked {
        Masked {
            shape: b"block0: v0 = iconst #0; v1 = iconst #1; call #2; v2 = func_addr #3".to_vec(),
            constants: vec![int(size), int(align), Constant::Place(0), Constant::Place(1)],
            size: 400,
        }
    }

    fn refs(call: u8, addr: u8) -> Vec<Target> {
        vec![Target::Function(key(call)), Target::Direct(key(addr))]
    }

    fn of<'a>(
        items: &'a [(InstanceKey, Masked, Vec<Target>)],
    ) -> Vec<(InstanceKey, Settings, &'a Masked, &'a Refs)> {
        items.iter().map(|(k, m, r)| (*k, settings(1), m, r.as_slice())).collect()
    }

    #[test]
    fn element_sizes_fold() {
        let items = [
            (key(2), grow(8, 8), refs(9, 10)),
            (key(1), grow(4, 4), refs(9, 10)),
            (key(3), grow(16, 8), refs(9, 10)),
        ];
        let groups = groups(&of(&items), &Limits::default());
        assert_eq!(groups.len(), 1);
        let group = &groups[0];
        assert_eq!(group.members, vec![key(1), key(2), key(3)]);
        assert_eq!(group.varying, vec![0, 1]);
        let want = vec![vec![int(4), int(4)], vec![int(8), int(8)], vec![int(16), int(8)]];
        assert_eq!(group.constants, want);
        assert_eq!(group.parameters(), (vec![0, 1], 2));
        assert_ne!(group.key(), key(1));
    }

    #[test]
    fn other_control_flow_does_not_fold() {
        let mut other = grow(4, 4);
        other.shape = b"block0: v0 = iconst #0; brif v0, block1, block2; ...".to_vec();
        let items = [(key(1), grow(8, 8), refs(9, 10)), (key(2), other, refs(9, 10))];
        assert!(groups(&of(&items), &Limits::default()).is_empty());
    }

    #[test]
    fn other_settings_do_not_fold() {
        let (a, b) = (grow(8, 8), grow(4, 4));
        let (ra, rb) = (refs(9, 10), refs(9, 10));
        let candidates = [(key(1), settings(1), &a, &ra[..]), (key(2), settings(2), &b, &rb[..])];
        assert!(groups(&candidates, &Limits::default()).is_empty());
    }

    #[test]
    fn a_place_varies_only_when_its_targets_do() {
        // The same targets at other places.
        let mut swapped = grow(4, 8);
        swapped.constants[2] = Constant::Place(1);
        swapped.constants[3] = Constant::Place(0);
        let swapped_refs = vec![Target::Direct(key(10)), Target::Function(key(9))];
        let items = [(key(1), grow(8, 8), refs(9, 10)), (key(2), swapped, swapped_refs)];
        let groups_same = groups(&of(&items), &Limits::default());
        assert_eq!(groups_same[0].varying, vec![0]);

        let items = [(key(1), grow(8, 8), refs(9, 10)), (key(2), grow(4, 8), refs(11, 10))];
        let groups_other = groups(&of(&items), &Limits::default());
        assert_eq!(groups_other[0].varying, vec![0, 2]);
        assert_eq!(groups_other[0].constants[1], vec![int(4), Constant::Place(0)]);
        assert_ne!(groups_same[0].key(), groups_other[0].key());
    }

    #[test]
    fn equal_constants_alone_are_not_folded() {
        let items = [(key(1), grow(8, 8), refs(9, 10)), (key(2), grow(8, 8), refs(9, 10))];
        assert!(groups(&of(&items), &Limits::default()).is_empty());
    }

    #[test]
    fn a_fixed_target_is_in_the_key() {
        let a = [(key(1), grow(8, 8), refs(9, 10)), (key(2), grow(4, 8), refs(9, 10))];
        let b = [(key(1), grow(8, 8), refs(12, 10)), (key(2), grow(4, 8), refs(12, 10))];
        let (a, b) = (groups(&of(&a), &Limits::default()), groups(&of(&b), &Limits::default()));
        assert_eq!(a[0].varying, b[0].varying);
        assert_ne!(a[0].key(), b[0].key());
    }

    fn many(n: usize, base: u128) -> Masked {
        Masked {
            shape: b"iconsts".to_vec(),
            constants: (0..n).map(|it| int(base + it as u128)).collect(),
            size: 10_000,
        }
    }

    #[test]
    fn the_parameter_cap_splits() {
        let limits = Limits::default();
        let items = [(key(1), many(17, 0), vec![]), (key(2), many(17, 100), vec![])];
        assert!(groups(&of(&items), &limits).is_empty());
        let items = [(key(1), many(16, 0), vec![]), (key(2), many(16, 100), vec![])];
        assert_eq!(groups(&of(&items), &limits)[0].parameters().1, 16);
        // Twenty constants that are one value per member are one parameter.
        let same = |value| Masked { constants: vec![int(value); 20], ..many(20, 0) };
        let items = [(key(1), same(1), vec![]), (key(2), same(2), vec![])];
        let group = &groups(&of(&items), &limits)[0];
        assert_eq!(group.varying.len(), 20);
        assert_eq!(group.parameters(), (vec![0; 20], 1));
        // Members far apart in their constants split into the groups they fit.
        let near = |value| {
            let mut it = many(17, 0);
            it.constants[0] = int(value);
            it
        };
        let items = [
            (key(1), near(1), vec![]),
            (key(2), many(17, 100), vec![]),
            (key(3), near(3), vec![]),
            (key(4), many(17, 200), vec![]),
        ];
        let groups = groups(&of(&items), &limits);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].members, vec![key(1), key(3)]);
    }

    #[test]
    fn small_bodies_are_not_worth_thunks() {
        let small = |size| Masked { size, ..grow(8, 8) };
        let small_b = |size| Masked { size, ..grow(4, 4) };
        let items = [(key(1), small(4), refs(9, 10)), (key(2), small_b(4), refs(9, 10))];
        assert!(groups(&of(&items), &Limits::default()).is_empty());
        let items = [(key(1), small(40), refs(9, 10)), (key(2), small_b(40), refs(9, 10))];
        assert_eq!(groups(&of(&items), &Limits::default()).len(), 1);
        assert!(!profitable(8, 2, 2, &Limits::default()));
        assert!(profitable(16, 2, 2, &Limits::default()));
    }

    #[test]
    fn the_input_order_changes_nothing() {
        let items: Vec<(InstanceKey, Masked, Vec<Target>)> = (0..12u8)
            .map(|n| {
                let size = u128::from(n);
                let masked = if n % 3 == 0 { many(17, size * 50) } else { grow(size, 8) };
                (key(n), masked, refs(9, 10 + n % 2))
            })
            .collect();
        let want = groups(&of(&items), &Limits::default());
        assert!(!want.is_empty());
        let mut order: Vec<usize> = (0..items.len()).collect();
        for round in 0..8 {
            order.rotate_left(5);
            order.swap(round % 12, (round * 7 + 3) % 12);
            let candidates = of(&items);
            let permuted: Vec<_> = order.iter().map(|&it| candidates[it]).collect();
            assert_eq!(groups(&permuted, &Limits::default()), want);
        }
    }

    #[test]
    fn thunks_call_the_body_last() {
        let items = [(key(1), grow(8, 8), refs(9, 10)), (key(2), grow(4, 8), refs(9, 10))];
        let group = &groups(&of(&items), &Limits::default())[0];
        let refs = group.thunk_refs(&items[0].2);
        assert_eq!(refs.len(), 3);
        assert_eq!(refs[2], Target::Direct(group.key()));
    }
}
