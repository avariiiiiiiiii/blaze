//! The job's unit graph as flat columns, built once when the job is lowered, so no unit's work
//! walks the graph through strings or sets: each unit's dependencies as a CSR list sorted as
//! cargo's `build_map` visits them, its package as a `u32`, its role as a flag byte, the build
//! script runs it is linked with (cargo's `to_link`) as a second CSR list, and the units built
//! for build scripts and proc macros as a bitset.
//!
//! Cost: 4 bytes per edge, 4 per link edge, 9 per unit, `N/8` bytes per bitset. Every walk
//! (`closure`, `linked_in`) is one pass with a bitset of `N/64` words and no other allocation.
//! Building it is `O(N + E)` plus cargo's `to_link` once per unit with one shared memo.

use std::hash::{Hash, Hasher};

use blaze_build_script::apply::{GraphUnit, to_link};
use blaze_units::{
    query::Units,
    schema::{Mode, Unit},
};
use rustc_hash::{FxHashMap, FxHasher};

/// A unit's role, one bit each.
pub(crate) mod flag {
    /// A build script's compile or a proc macro: built for the host (cargo's `for_host`).
    pub(crate) const FOR_HOST: u8 = 1;
    /// A library linked into what depends on it (cargo's `is_linkable`).
    pub(crate) const LINKABLE: u8 = 2;
    /// A build script's run.
    pub(crate) const RUN: u8 = 4;
    /// A crate of the standard library.
    pub(crate) const STD: u8 = 8;
    /// A test (`--test`).
    pub(crate) const TEST: u8 = 16;
}

/// A set of units, one bit each.
#[derive(Clone, Default)]
pub(crate) struct Bits(Vec<u64>);

impl Bits {
    pub(crate) fn new(len: usize) -> Bits {
        Bits(vec![0; len.div_ceil(64)])
    }

    /// Sets `at`; whether it was clear.
    #[inline]
    pub(crate) fn insert(&mut self, at: u32) -> bool {
        let (word, bit) = (at as usize / 64, 1u64 << (at % 64));
        let clear = self.0[word] & bit == 0;
        self.0[word] |= bit;
        clear
    }

    #[inline]
    pub(crate) fn contains(&self, at: u32) -> bool {
        self.0.get(at as usize / 64).is_some_and(|word| word >> (at % 64) & 1 == 1)
    }
}

pub(crate) struct Graph {
    /// Unit `i`'s dependencies are `deps[dep_start[i]..dep_start[i + 1]]`, sorted by their
    /// packages' ids, as cargo's `build_map` visits them.
    dep_start: Vec<u32>,
    deps: Vec<u32>,
    /// The runs of build scripts unit `i` is linked with, in cargo's order:
    /// `links[link_start[i]..link_start[i + 1]]`.
    link_start: Vec<u32>,
    links: Vec<u32>,
    /// Each unit's package, by index into `Units::packages`; `u32::MAX` for one not listed.
    package: Vec<u32>,
    flags: Vec<u8>,
    /// The units compiled for build scripts and proc macros: all a script's compile or a proc
    /// macro depends on, and themselves.
    build_time: Bits,
    /// Units by [`target_hash`] of what they build, each hash's units in order: how an artifact
    /// finds its unit with no string built.
    by_target: FxHashMap<u64, Vec<u32>>,
}

/// What a unit builds, hashed: its package, target name and kinds, and mode.
pub(crate) fn target_hash(pkg_id: &str, name: &str, kinds: &[String], mode: Mode) -> u64 {
    let mut hasher = FxHasher::default();
    pkg_id.hash(&mut hasher);
    name.hash(&mut hasher);
    kinds.hash(&mut hasher);
    mode.hash(&mut hasher);
    hasher.finish()
}

fn flags_of(unit: &Unit) -> u8 {
    let target = &unit.target;
    let mut flags = 0;
    if target.is_custom_build() || target.is_proc_macro() {
        flags |= flag::FOR_HOST;
    }
    let linkable_type = target.crate_types.iter().any(|it| matches!(it.as_str(), "lib" | "rlib" | "dylib" | "proc-macro"));
    if target.is_lib() && linkable_type {
        flags |= flag::LINKABLE;
    }
    if unit.mode == Mode::RunCustomBuild {
        flags |= flag::RUN;
    }
    if unit.is_std {
        flags |= flag::STD;
    }
    if unit.mode == Mode::Test {
        flags |= flag::TEST;
    }
    flags
}

impl Graph {
    pub(crate) fn new(units: &Units) -> Graph {
        let all = &units.graph.units;
        let count = all.len();
        let packages: FxHashMap<&str, u32> =
            units.packages.iter().enumerate().map(|(at, it)| (it.id.as_str(), at as u32)).collect();
        let package: Vec<u32> =
            all.iter().map(|it| packages.get(it.pkg_id.as_str()).copied().unwrap_or(u32::MAX)).collect();
        let flags: Vec<u8> = all.iter().map(flags_of).collect();
        let edges = all.iter().map(|it| it.dependencies.len()).sum();
        let mut dep_start = Vec::with_capacity(count + 1);
        let mut deps = Vec::with_capacity(edges);
        dep_start.push(0);
        for unit in all {
            let from = deps.len();
            deps.extend(unit.dependencies.iter().map(|it| it.index as u32).filter(|it| (*it as usize) < count));
            deps[from..].sort_by(|a, b| all[*a as usize].pkg_id.cmp(&all[*b as usize].pkg_id));
            dep_start.push(deps.len() as u32);
        }
        let mut graph = Graph {
            dep_start,
            deps,
            link_start: Vec::with_capacity(count + 1),
            links: Vec::new(),
            package,
            flags,
            build_time: Bits::new(count),
            by_target: FxHashMap::default(),
        };
        graph.by_target.reserve(count);
        for (at, unit) in all.iter().enumerate() {
            let hash = target_hash(&unit.pkg_id, &unit.target.name, &unit.target.kind, unit.mode);
            graph.by_target.entry(hash).or_default().push(at as u32);
        }

        let mut memo = FxHashMap::default();
        memo.reserve(count);
        let node = |at: &u32| graph.graph_unit(all, *at);
        let mut link_start = Vec::with_capacity(count + 1);
        let mut links = Vec::new();
        link_start.push(0);
        for at in 0..count as u32 {
            links.extend(to_link(at, &node, &mut memo).to_link);
            link_start.push(links.len() as u32);
        }
        graph.link_start = link_start;
        graph.links = links;

        let mut build_time = Bits::new(count);
        let mut pending: Vec<u32> = (0..count as u32)
            .filter(|at| graph.flags[*at as usize] & flag::FOR_HOST != 0 && graph.flags[*at as usize] & flag::RUN == 0)
            .collect();
        for at in &pending {
            build_time.insert(*at);
        }
        while let Some(at) = pending.pop() {
            for &dep in graph.deps(at) {
                if build_time.insert(dep) {
                    pending.push(dep);
                }
            }
        }
        graph.build_time = build_time;
        graph
    }

    fn graph_unit(&self, all: &[Unit], at: u32) -> GraphUnit<u32, u32> {
        let deps = self.deps(at).to_vec();
        let custom_build = all[at as usize].target.is_custom_build();
        let own_runs = match custom_build {
            true => Vec::new(),
            false => deps
                .iter()
                .copied()
                .filter(|dep| self.has(*dep, flag::RUN) && self.package[*dep as usize] == self.package[at as usize])
                .collect(),
        };
        GraphUnit { deps, own_runs, for_host: self.has(at, flag::FOR_HOST), linkable: self.has(at, flag::LINKABLE) }
    }

    pub(crate) fn len(&self) -> usize {
        self.flags.len()
    }

    #[inline]
    pub(crate) fn deps(&self, at: u32) -> &[u32] {
        let at = at as usize;
        &self.deps[self.dep_start[at] as usize..self.dep_start[at + 1] as usize]
    }

    /// The build script runs `at` is linked with, in cargo's order.
    #[inline]
    pub(crate) fn links(&self, at: u32) -> &[u32] {
        let at = at as usize;
        &self.links[self.link_start[at] as usize..self.link_start[at + 1] as usize]
    }

    #[inline]
    pub(crate) fn has(&self, at: u32, flag: u8) -> bool {
        self.flags.get(at as usize).is_some_and(|it| it & flag != 0)
    }

    #[inline]
    pub(crate) fn package(&self, at: u32) -> u32 {
        self.package[at as usize]
    }

    #[inline]
    pub(crate) fn build_time(&self, at: u32) -> bool {
        self.build_time.contains(at)
    }

    /// The unit that builds `kinds` target `name` of `pkg_id` in `mode`.
    pub(crate) fn find(&self, all: &[Unit], pkg_id: &str, name: &str, kinds: &[String], mode: Mode) -> Option<u32> {
        let found = self.by_target.get(&target_hash(pkg_id, name, kinds, mode))?;
        found.iter().copied().find(|at| {
            let unit = &all[*at as usize];
            unit.pkg_id == pkg_id && unit.target.name == name && unit.target.kind == kinds && unit.mode == mode
        })
    }

    /// Every unit `unit` depends on, itself first, each once, each before its dependencies
    /// (what depends on a unit comes before it).
    pub(crate) fn closure(&self, unit: u32) -> Vec<u32> {
        let mut seen = Bits::new(self.len());
        let mut post = Vec::with_capacity(self.len().min(256));
        let mut stack = vec![(unit, false)];
        while let Some((at, expanded)) = stack.pop() {
            if expanded {
                post.push(at);
                continue;
            }
            if !seen.insert(at) {
                continue;
            }
            stack.push((at, true));
            for &dep in self.deps(at).iter().rev() {
                if !seen.contains(dep) {
                    stack.push((dep, false));
                }
            }
        }
        post.reverse();
        post
    }

    /// The crates linked into `unit`'s image, in [`Graph::closure`]'s order: its own, and each
    /// library reached through libraries, not through a build script or proc macro.
    pub(crate) fn linked_in(&self, unit: u32) -> Vec<u32> {
        let mut inside = Bits::new(self.len());
        inside.insert(unit);
        let mut pending = vec![unit];
        while let Some(at) = pending.pop() {
            for &dep in self.deps(at) {
                if self.has(dep, flag::LINKABLE) && !self.has(dep, flag::FOR_HOST) && inside.insert(dep) {
                    pending.push(dep);
                }
            }
        }
        let mut order = self.closure(unit);
        order.retain(|at| inside.contains(*at) && !self.has(*at, flag::RUN));
        order
    }
}
