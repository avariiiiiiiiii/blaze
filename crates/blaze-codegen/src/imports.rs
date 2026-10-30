//! What each release compile is given, decided from the summaries of the whole build before
//! any compile runs (docs/v2-codegen.md, "Release"; #899, #176).
//!
//! [`decide`] is a function of the summaries alone, in key order, so a warm build gives every
//! function the [`Imports`] a cold one did without building any IR, and two builds of one
//! program give the same.
//!
//! Every fact of the whole program that is not an import ([`Imports::constants`],
//! [`Imports::dead`]) is about what a function's callers do, so it holds only of a function
//! whose every caller has a summary: one with `external == false` whose address no summary
//! takes. The driver marks a function external when something without a summary refers to
//! it ([`called_from_outside`]): a static's value, a unit of another backend.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use crate::{
    InstanceKey,
    summary::{Arg, Attrs, Hint, Imports, Summary},
};

/// How much a compile imports, as ThinLTO's options say it.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Limits {
    /// The most instructions, as LLVM counts them after its pre-link pipeline, a body with no
    /// `#[inline]` may have to be imported by a direct call.
    pub plain: u32,
    /// The same for an `#[inline]` body. `#[inline(always)]` has all of [`Limits::budget`];
    /// `#[inline(never)]` none.
    pub inline: u32,
    /// The share of its room an import leaves to each import of what it calls, as
    /// `-import-instr-evolution-factor`.
    pub evolution: f64,
    /// How many calls deep imports go.
    pub depth: u32,
    /// The most instructions one compile imports in all.
    pub budget: u32,
}

/// Bun on 16 cores against cargo's release build (c8a.24xlarge, 2026-10-23): rooms of 100
/// and 300 built in 118.0 s and ran 1.244x, 200 and 600 161.5 s and 1.242x, 40 and 100
/// 93.6 s and 1.311x. Its hot paths go through chains of small wrappers: 3 deep it ran
/// 1.785x, 12 deep as 8, and an evolution of 0.9 imported more for no faster a run.
impl Default for Limits {
    fn default() -> Limits {
        Limits { plain: 100, inline: 300, evolution: 0.7, depth: 8, budget: 40_000 }
    }
}

impl Limits {
    /// The room of a body with `hint` at the first call, `None` for one never imported.
    fn room(&self, hint: Hint) -> Option<u32> {
        match hint {
            Hint::None => Some(self.plain),
            Hint::Inline => Some(self.inline),
            Hint::Always => Some(self.budget),
            Hint::Never => None,
        }
    }
}

/// A summary that does not agree with itself or with another.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ImportError {
    /// The summary under `at` is of another function.
    Key { at: InstanceKey, says: InstanceKey },
    /// A call of `caller` passes more arguments than `callee` has parameters.
    Args { caller: InstanceKey, callee: InstanceKey, args: u32, params: u32 },
    /// A function's summary names a parameter it does not have as unused.
    Unused { function: InstanceKey, param: u32, params: u32 },
}

impl std::fmt::Display for ImportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ImportError::Key { at, says } => {
                write!(f, "the summary kept under {} is of {}", at.0, says.0)
            }
            ImportError::Args { caller, callee, args, params } => write!(
                f,
                "{} calls {} with {args} arguments, and its summary has {params} parameters",
                caller.0, callee.0
            ),
            ImportError::Unused { function, param, params } => write!(
                f,
                "the summary of {} says parameter {param} is unused, of {params}",
                function.0
            ),
        }
    }
}

impl std::error::Error for ImportError {}

/// Marks as external every function of `outside`: what something without a summary refers
/// to (a static's value, a vtable built as data, a unit of another backend), whose callers
/// the summaries don't show.
pub fn called_from_outside(
    summaries: &mut BTreeMap<InstanceKey, Summary>,
    outside: impl IntoIterator<Item = InstanceKey>,
) {
    for key in outside {
        if let Some(summary) = summaries.get_mut(&key) {
            summary.external = true;
        }
    }
}

/// What every function of `summaries` is compiled with: the functions it imports, its
/// parameters every caller passes one constant for, what it may assume of its callees, and
/// which of their parameters it need not compute.
///
/// A callee without a summary (not an LLVM unit of this build) is imported by nothing and
/// assumed nothing of.
pub fn decide(
    summaries: &BTreeMap<InstanceKey, Summary>,
    limits: &Limits,
) -> Result<BTreeMap<InstanceKey, Imports>, ImportError> {
    check(summaries)?;
    let addressed: BTreeSet<InstanceKey> =
        summaries.values().flat_map(|it| it.addressed.iter().copied()).collect();
    let closed = |key: &InstanceKey| {
        summaries.get(key).is_some_and(|it| !it.external && !addressed.contains(key))
    };
    let constants = constants(summaries, &closed);
    let attrs = attrs(summaries);
    let mut decided = BTreeMap::new();
    for (key, summary) in summaries {
        let imported = imported(summaries, summary, limits);
        let mut callees = BTreeMap::new();
        let mut dead = BTreeMap::new();
        let callers = std::iter::once(summary).chain(imported.iter().map(|it| &summaries[it]));
        for caller in callers {
            for call in &caller.calls {
                let Some(callee) = summaries.get(&call.callee) else { continue };
                let assumed = attrs[&call.callee];
                if assumed != Attrs::default() {
                    callees.insert(call.callee, assumed);
                }
                if closed(&call.callee) && !callee.unused.is_empty() {
                    let mut unused = callee.unused.clone();
                    unused.sort_unstable();
                    unused.dedup();
                    dead.insert(call.callee, unused);
                }
            }
        }
        let imports = Imports {
            imported,
            constants: constants.get(key).cloned().unwrap_or_default(),
            callees: callees.into_iter().collect(),
            dead: dead.into_iter().collect(),
        };
        decided.insert(*key, imports);
    }
    Ok(decided)
}

fn check(summaries: &BTreeMap<InstanceKey, Summary>) -> Result<(), ImportError> {
    for (at, summary) in summaries {
        if summary.key != *at {
            return Err(ImportError::Key { at: *at, says: summary.key });
        }
        if let Some(&param) = summary.unused.iter().find(|it| **it >= summary.params) {
            return Err(ImportError::Unused { function: *at, param, params: summary.params });
        }
        for call in &summary.calls {
            let Some(callee) = summaries.get(&call.callee) else { continue };
            let args = u32::try_from(call.args.len()).unwrap_or(u32::MAX);
            if args > callee.params {
                return Err(ImportError::Args {
                    caller: *at,
                    callee: call.callee,
                    args,
                    params: callee.params,
                });
            }
        }
    }
    Ok(())
}

/// What `importer` imports, as ThinLTO's `computeImportForFunction` chooses: each call's
/// callee if its body fits the room its hint gives, shrunk by [`Limits::evolution`] at each
/// call deeper, and then what that callee calls, breadth first, each callee once, until
/// [`Limits::depth`] or [`Limits::budget`]. Sorted.
fn imported(
    summaries: &BTreeMap<InstanceKey, Summary>,
    importer: &Summary,
    limits: &Limits,
) -> Vec<InstanceKey> {
    let mut seen = BTreeSet::from([importer.key]);
    let mut taken = BTreeSet::new();
    let mut spent: u64 = 0;
    let mut next: VecDeque<(InstanceKey, f64, u32)> =
        importer.calls.iter().map(|it| (it.callee, 1.0, 0)).collect();
    while let Some((key, share, depth)) = next.pop_front() {
        // Breadth first, a callee is first met where its room is the largest it gets: one
        // that does not fit there fits nowhere deeper.
        if !seen.insert(key) {
            continue;
        }
        let Some(callee) = summaries.get(&key) else { continue };
        let Some(room) = limits.room(callee.hint) else { continue };
        if f64::from(callee.size) > f64::from(room) * share
            || spent + u64::from(callee.size) > u64::from(limits.budget)
        {
            continue;
        }
        spent += u64::from(callee.size);
        taken.insert(key);
        if depth + 1 < limits.depth {
            let share = share * limits.evolution;
            next.extend(callee.calls.iter().map(|it| (it.callee, share, depth + 1)));
        }
    }
    taken.into_iter().collect()
}

/// Per function whose callers all have summaries, the parameters every call site of the
/// build passes one integer constant for, sorted.
fn constants(
    summaries: &BTreeMap<InstanceKey, Summary>,
    closed: &dyn Fn(&InstanceKey) -> bool,
) -> BTreeMap<InstanceKey, Vec<(u32, Arg)>> {
    // Per callee and parameter, what its call sites pass: `None` once two disagree or one
    // is unknown.
    let mut passed: BTreeMap<InstanceKey, Vec<Option<Arg>>> = BTreeMap::new();
    for caller in summaries.values() {
        for call in &caller.calls {
            if !closed(&call.callee) || call.count == 0 {
                continue;
            }
            let params = summaries[&call.callee].params as usize;
            let seen = passed.entry(call.callee).or_insert_with(|| {
                (0..params)
                    .map(|at| match call.args.get(at) {
                        Some(arg @ Arg::Int { .. }) => Some(*arg),
                        _ => None,
                    })
                    .collect()
            });
            for (at, known) in seen.iter_mut().enumerate() {
                if *known != call.args.get(at).copied() {
                    *known = None;
                }
            }
        }
    }
    passed
        .into_iter()
        .map(|(key, args)| {
            let known = args
                .into_iter()
                .enumerate()
                .filter_map(|(at, arg)| Some((at as u32, arg?)))
                .collect::<Vec<_>>();
            (key, known)
        })
        .filter(|(_, known)| !known.is_empty())
        .collect()
}

/// What each function may be assumed to be by its callers, its body's attributes met with
/// its callees' bottom up over the call graph's strongly connected components, as LLVM's
/// `function-attrs` infers them over an SCC: nounwind, readonly and readnone hold of a
/// component when they hold of every member's body and of every callee outside it;
/// willreturn and norecurse never hold of a member of a cycle. noreturn is the function's
/// own, as inferred alone.
fn attrs(summaries: &BTreeMap<InstanceKey, Summary>) -> BTreeMap<InstanceKey, Attrs> {
    let nodes: Vec<InstanceKey> = summaries.keys().copied().collect();
    let at: BTreeMap<InstanceKey, usize> =
        nodes.iter().enumerate().map(|(at, key)| (*key, at)).collect();
    let edges: Vec<Vec<usize>> = nodes
        .iter()
        .map(|key| {
            let mut to: Vec<usize> =
                summaries[key].calls.iter().filter_map(|it| at.get(&it.callee).copied()).collect();
            to.sort_unstable();
            to.dedup();
            to
        })
        .collect();
    let mut known: Vec<Option<Attrs>> = vec![None; nodes.len()];
    for component in components(&edges) {
        let cyclic = component.len() > 1 || edges[component[0]].contains(&component[0]);
        let mut met = Attrs {
            nounwind: true,
            readonly: true,
            readnone: true,
            norecurse: !cyclic,
            willreturn: !cyclic,
            noreturn: false,
        };
        for &member in &component {
            let summary = &summaries[&nodes[member]];
            let body = summary.body;
            met.nounwind &= body.nounwind;
            met.readnone &= body.readnone;
            met.readonly &= body.readonly || body.readnone;
            met.norecurse &= body.norecurse;
            met.willreturn &= body.willreturn;
            for call in &summary.calls {
                match at.get(&call.callee) {
                    Some(callee) if component.contains(callee) => {}
                    Some(callee) => {
                        let callee = known[*callee].expect("components come callees first");
                        met.nounwind &= callee.nounwind;
                        met.readnone &= callee.readnone;
                        met.readonly &= callee.readonly || callee.readnone;
                        met.norecurse &= callee.norecurse;
                        met.willreturn &= callee.willreturn;
                    }
                    None => {
                        met = Attrs::default();
                    }
                }
            }
        }
        for &member in &component {
            let alone = summaries[&nodes[member]].attrs;
            let readnone = met.readnone || alone.readnone;
            known[member] = Some(Attrs {
                nounwind: met.nounwind || alone.nounwind,
                readonly: met.readonly || alone.readonly || readnone,
                readnone,
                norecurse: met.norecurse || alone.norecurse,
                willreturn: met.willreturn || alone.willreturn,
                noreturn: alone.noreturn,
            });
        }
    }
    nodes.into_iter().zip(known).map(|(key, it)| (key, it.expect("every node is in a component"))).collect()
}

/// The strongly connected components of `edges`, each sorted, callees before callers
/// (Tarjan's order).
fn components(edges: &[Vec<usize>]) -> Vec<Vec<usize>> {
    const UNSEEN: usize = usize::MAX;
    let mut index = vec![UNSEEN; edges.len()];
    let mut low = vec![0; edges.len()];
    let mut on_stack = vec![false; edges.len()];
    let mut stack = Vec::new();
    let mut found = Vec::new();
    let mut next = 0;
    for root in 0..edges.len() {
        if index[root] != UNSEEN {
            continue;
        }
        index[root] = next;
        low[root] = next;
        next += 1;
        stack.push(root);
        on_stack[root] = true;
        let mut work: Vec<(usize, usize)> = vec![(root, 0)];
        while let Some(&(node, edge)) = work.last() {
            if let Some(&to) = edges[node].get(edge) {
                work.last_mut().expect("just read").1 += 1;
                if index[to] == UNSEEN {
                    index[to] = next;
                    low[to] = next;
                    next += 1;
                    stack.push(to);
                    on_stack[to] = true;
                    work.push((to, 0));
                } else if on_stack[to] {
                    low[node] = low[node].min(index[to]);
                }
                continue;
            }
            work.pop();
            if let Some(&(parent, _)) = work.last() {
                low[parent] = low[parent].min(low[node]);
            }
            if low[node] == index[node] {
                let mut component = Vec::new();
                loop {
                    let member = stack.pop().expect("the component's root is on the stack");
                    on_stack[member] = false;
                    component.push(member);
                    if member == node {
                        break;
                    }
                }
                component.sort_unstable();
                found.push(component);
            }
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use blaze_ids::ContentId;

    use super::*;
    use crate::summary::Call;

    fn key(n: u8) -> InstanceKey {
        InstanceKey(ContentId([n; 32]))
    }

    fn all() -> Attrs {
        Attrs {
            nounwind: true,
            readonly: true,
            readnone: true,
            norecurse: true,
            willreturn: true,
            noreturn: false,
        }
    }

    fn function(n: u8, size: u32, calls: &[u8]) -> Summary {
        Summary {
            key: key(n),
            size,
            hint: Hint::None,
            calls: calls
                .iter()
                .map(|it| Call { callee: key(*it), count: 1, args: Vec::new() })
                .collect(),
            addressed: Vec::new(),
            params: 0,
            unused: Vec::new(),
            attrs: Attrs::default(),
            body: Attrs::default(),
            external: false,
        }
    }

    fn build(summaries: Vec<Summary>) -> BTreeMap<InstanceKey, Summary> {
        summaries.into_iter().map(|it| (it.key, it)).collect()
    }

    fn decided(summaries: Vec<Summary>) -> BTreeMap<InstanceKey, Imports> {
        decide(&build(summaries), &Limits::default()).unwrap()
    }

    fn int(value: u128) -> Arg {
        Arg::Int { bits: 32, value }
    }

    #[test]
    fn imports_shrink_by_depth() {
        // 1 calls 2 (90), which calls 3 (80 > 70), which calls 4 (10).
        let imports = decided(vec![
            function(1, 5, &[2]),
            function(2, 90, &[3]),
            function(3, 80, &[4]),
            function(4, 10, &[]),
        ]);
        assert_eq!(imports[&key(1)].imported, vec![key(2)]);
        // From 2, 3 is one call deep: 80 fits 100.
        assert_eq!(imports[&key(2)].imported, vec![key(3), key(4)]);
    }

    #[test]
    fn a_large_body_is_not_imported_and_hints_change_the_room() {
        let mut inline = function(3, 250, &[]);
        inline.hint = Hint::Inline;
        let mut always = function(4, 5_000, &[]);
        always.hint = Hint::Always;
        let mut never = function(5, 1, &[]);
        never.hint = Hint::Never;
        let imports = decided(vec![
            function(1, 5, &[2, 3, 4, 5]),
            function(2, 101, &[]),
            inline,
            always,
            never,
        ]);
        assert_eq!(imports[&key(1)].imported, vec![key(3), key(4)]);
    }

    #[test]
    fn imports_stop_at_the_depth() {
        let limits = Limits { depth: 2, ..Limits::default() };
        let summaries =
            build(vec![function(1, 1, &[2]), function(2, 1, &[3]), function(3, 1, &[4]), function(4, 1, &[])]);
        let imports = decide(&summaries, &limits).unwrap();
        assert_eq!(imports[&key(1)].imported, vec![key(2), key(3)]);
    }

    #[test]
    fn a_callee_is_imported_once_and_never_the_importer() {
        // 1 calls 2 and 3, both of which call 4; 4 calls 1.
        let imports = decided(vec![
            function(1, 1, &[2, 3]),
            function(2, 1, &[4]),
            function(3, 1, &[4]),
            function(4, 1, &[1]),
        ]);
        assert_eq!(imports[&key(1)].imported, vec![key(2), key(3), key(4)]);
    }

    #[test]
    fn the_budget_is_spent_in_order() {
        let limits = Limits { budget: 150, ..Limits::default() };
        let summaries = build(vec![
            function(1, 1, &[4, 2, 3]),
            function(2, 60, &[]),
            function(3, 60, &[]),
            function(4, 60, &[]),
        ]);
        let imports = decide(&summaries, &limits).unwrap();
        // 4 and 2 are met first; 3 would make 180.
        assert_eq!(imports[&key(1)].imported, vec![key(2), key(4)]);
    }

    #[test]
    fn a_callee_without_a_summary_is_a_case() {
        let imports = decided(vec![function(1, 1, &[9])]);
        assert_eq!(imports[&key(1)], Imports::default());
    }

    #[test]
    fn constants_only_for_internal_functions_with_agreeing_call_sites() {
        let callee = |n: u8| {
            let mut it = function(n, 1_000, &[]);
            it.params = 3;
            it
        };
        let call = |callee: u8, args: Vec<Arg>| Call { callee: key(callee), count: 2, args };
        let (internal, mut external, addressed, disagreeing) =
            (callee(2), callee(3), callee(4), callee(5));
        external.external = true;
        let mut taker = function(6, 1_000, &[]);
        taker.addressed = vec![key(4)];
        let args = vec![int(7), Arg::Unknown, int(1)];
        let mut first = function(1, 1_000, &[]);
        first.calls = vec![
            call(2, args.clone()),
            call(3, args.clone()),
            call(4, args.clone()),
            call(5, args.clone()),
        ];
        let mut second = function(7, 1_000, &[]);
        // Fewer arguments than parameters: the last is unknown.
        second.calls = vec![
            call(2, vec![int(7), int(3)]),
            call(3, args.clone()),
            call(4, args.clone()),
            call(5, vec![int(8), Arg::Unknown, int(1)]),
        ];
        let imports = decided(vec![first, second, internal, external, addressed, disagreeing, taker]);
        assert_eq!(imports[&key(2)].constants, vec![(0, int(7))]);
        assert_eq!(imports[&key(3)].constants, vec![]);
        assert_eq!(imports[&key(4)].constants, vec![]);
        assert_eq!(imports[&key(5)].constants, vec![(2, int(1))]);
        // No callers, no constants.
        assert_eq!(imports[&key(1)].constants, vec![]);
    }

    #[test]
    fn what_is_called_from_outside_gets_no_constants() {
        let mut callee = function(2, 1_000, &[]);
        callee.params = 1;
        let mut caller = function(1, 1_000, &[]);
        caller.calls = vec![Call { callee: key(2), count: 1, args: vec![int(4)] }];
        let mut summaries = build(vec![caller, callee]);
        assert_eq!(decide(&summaries, &Limits::default()).unwrap()[&key(2)].constants, vec![(0, int(4))]);
        called_from_outside(&mut summaries, [key(2)]);
        assert_eq!(decide(&summaries, &Limits::default()).unwrap()[&key(2)].constants, vec![]);
    }

    #[test]
    fn attributes_are_met_through_a_cycle() {
        // 1 calls 2; 2 and 3 call each other; 3 calls 4. Every body is everything.
        let mut summaries =
            vec![function(1, 1_000, &[2]), function(2, 1_000, &[3]), function(3, 1_000, &[2, 4]), function(4, 1_000, &[])];
        for it in &mut summaries {
            it.body = all();
        }
        let imports = decided(summaries.clone());
        let cyclic = Attrs { norecurse: false, willreturn: false, ..all() };
        assert_eq!(imports[&key(1)].callees, vec![(key(2), cyclic)]);
        assert_eq!(imports[&key(2)].callees, vec![(key(3), cyclic)]);
        assert_eq!(imports[&key(3)].callees, vec![(key(2), cyclic), (key(4), all())]);
        // 4 may unwind: so may the cycle and its caller; 4 still reads nothing.
        summaries[3].body.nounwind = false;
        let imports = decided(summaries.clone());
        let unwinding = Attrs { nounwind: false, ..cyclic };
        assert_eq!(imports[&key(1)].callees, vec![(key(2), unwinding)]);
        assert_eq!(imports[&key(3)].callees[1], (key(4), Attrs { nounwind: false, ..all() }));
        // A callee without a summary makes its callers assume nothing of it or of themselves.
        summaries[3].body = all();
        summaries[3].calls.push(Call { callee: key(9), count: 1, args: Vec::new() });
        let imports = decided(summaries.clone());
        assert_eq!(imports[&key(1)].callees, vec![]);
        // What LLVM inferred alone still holds.
        summaries[1].attrs = Attrs { nounwind: true, noreturn: true, ..Attrs::default() };
        let imports = decided(summaries);
        assert_eq!(
            imports[&key(1)].callees,
            vec![(key(2), Attrs { nounwind: true, noreturn: true, ..Attrs::default() })]
        );
    }

    #[test]
    fn a_self_call_is_a_cycle_and_readnone_is_readonly() {
        let mut recursive = function(2, 1_000, &[2]);
        recursive.body = Attrs { readnone: true, ..all() };
        recursive.body.readonly = false;
        let imports = decided(vec![function(1, 1_000, &[2]), recursive]);
        assert_eq!(
            imports[&key(1)].callees,
            vec![(key(2), Attrs { norecurse: false, willreturn: false, ..all() })]
        );
    }

    #[test]
    fn dead_arguments_of_closed_callees() {
        let mut internal = function(2, 1_000, &[]);
        internal.params = 3;
        internal.unused = vec![2, 0];
        let mut external = internal.clone();
        external.key = key(3);
        external.external = true;
        let mut addressed = internal.clone();
        addressed.key = key(4);
        let mut caller = function(1, 1_000, &[2, 3, 4]);
        caller.addressed = vec![key(4)];
        let imports = decided(vec![caller, internal, external, addressed]);
        assert_eq!(imports[&key(1)].dead, vec![(key(2), vec![0, 2])]);
    }

    #[test]
    fn an_imported_body_brings_its_callees_facts() {
        // 1 imports 2, which calls 3: 1's compile has 3's declaration too.
        let mut leaf = function(3, 1_000, &[]);
        leaf.body = all();
        leaf.params = 1;
        leaf.unused = vec![0];
        let imports = decided(vec![function(1, 1, &[2]), function(2, 1, &[3]), leaf]);
        assert_eq!(imports[&key(1)].imported, vec![key(2)]);
        assert_eq!(imports[&key(1)].callees, vec![(key(3), all())]);
        assert_eq!(imports[&key(1)].dead, vec![(key(3), vec![0])]);
    }

    #[test]
    fn inconsistent_summaries_are_refused() {
        let mut caller = function(1, 1, &[2]);
        caller.calls[0].args = vec![Arg::Unknown, Arg::Unknown];
        let mut callee = function(2, 1, &[]);
        callee.params = 1;
        assert_eq!(
            decide(&build(vec![caller.clone(), callee.clone()]), &Limits::default()),
            Err(ImportError::Args { caller: key(1), callee: key(2), args: 2, params: 1 })
        );
        callee.unused = vec![1];
        caller.calls[0].args.pop();
        assert_eq!(
            decide(&build(vec![caller.clone(), callee]), &Limits::default()),
            Err(ImportError::Unused { function: key(2), param: 1, params: 1 })
        );
        let mut summaries = build(vec![caller]);
        summaries.insert(key(5), function(6, 1, &[]));
        assert_eq!(
            decide(&summaries, &Limits::default()),
            Err(ImportError::Key { at: key(5), says: key(6) })
        );
    }

    #[test]
    fn the_same_whatever_the_order_of_insertion() {
        let mut summaries = Vec::new();
        for n in 1..40u8 {
            let calls: Vec<u8> = [n.wrapping_mul(7) % 40 + 1, n.wrapping_mul(13) % 40 + 1, n % 5 + 1]
                .into_iter()
                .filter(|it| *it < 40)
                .collect();
            let mut it = function(n, u32::from(n) * 3, &calls);
            it.params = 2;
            it.unused = vec![u32::from(n % 2)];
            it.body = Attrs { nounwind: n % 3 != 0, readonly: n % 4 != 0, ..all() };
            for call in &mut it.calls {
                call.args = vec![int(u128::from(n % 2)), Arg::Unknown];
            }
            summaries.push(it);
        }
        let forward = decided(summaries.clone());
        summaries.reverse();
        let mut map = BTreeMap::new();
        for it in summaries.iter().cloned() {
            map.insert(it.key, it);
        }
        let backward = decide(&map, &Limits::default()).unwrap();
        assert_eq!(forward, backward);
        assert_eq!(forward.len(), 39);
        for (key, imports) in &forward {
            assert_eq!(imports.blob_key(*key), backward[key].blob_key(*key));
        }
    }

    #[test]
    fn components_come_callees_first() {
        // 0 -> 1 -> 2 -> 1, 2 -> 3.
        let edges = vec![vec![1], vec![2], vec![1, 3], vec![]];
        assert_eq!(components(&edges), vec![vec![3], vec![1, 2], vec![0]]);
    }
}
