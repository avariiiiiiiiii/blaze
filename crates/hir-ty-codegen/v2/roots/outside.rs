//! Which tests reach the outside world through what neither the hashes of their code nor the
//! trace of their run say ([`super::RootTest::outside`]): a symbol that starts a process or
//! talks over the network (v1's classifier, [`crate::fncache::outside`]), or assembly. Files,
//! directories and the environment a test reads are in its run's trace, and a run that read
//! the network is not kept, so those are not this.
//!
//! What a test reaches is everything its function's references reach, through functions and
//! statics, so one walk of the graph of the program answers every test: the strongly
//! connected components (Tarjan's), each outside if a member or a component it reaches is.

use base_db::Db;
use rustc_hash::FxHashMap;

use super::{
    super::{HirFrontend, HirInstance, HirStatic, Kind, places::Ref, refs, statics},
    RootTest,
};
use crate::{
    fncache::{self, Kind as Reads},
    home::Homes,
};

#[derive(Clone, PartialEq, Eq, Hash)]
enum Node {
    Function(HirInstance),
    Static(HirStatic),
}

struct Visit {
    /// Its place in the order of the walk, which is its place in [`Graph::visits`].
    index: u32,
    low: u32,
    on_stack: bool,
    /// Whether it or what it reaches is outside: final once its component is done.
    outside: bool,
    names: Vec<Node>,
}

struct Graph<'a, 'db> {
    frontend: &'a HirFrontend<'db>,
    places: FxHashMap<Node, u32>,
    visits: Vec<Visit>,
    /// Tarjan's stack: the nodes of the components not yet done.
    stack: Vec<u32>,
}

/// For each of `tests`, whether what its function at its place in `roots` reaches is outside.
pub(super) fn of(
    frontend: &HirFrontend<'_>,
    db: &Db,
    homes: &mut Homes,
    roots: &[HirInstance],
    tests: &[RootTest],
) -> Result<Vec<bool>, String> {
    let mut graph = Graph {
        frontend,
        // alloc: once a harness, sized by the program it reaches
        places: FxHashMap::default(),
        // alloc: as above
        visits: Vec::new(),
        // alloc: as above
        stack: Vec::new(),
    };
    tests
        .iter()
        .map(|test| {
            // clone: once a test
            let at = graph.visit(db, homes, Node::Function(roots[test.root].clone()))?;
            Ok(graph.visits[at as usize].outside)
        })
        // alloc: once a harness, a flag a test
        .collect()
}

impl Graph<'_, '_> {
    /// The place of `node`, once all it reaches is visited.
    fn visit(&mut self, db: &Db, homes: &mut Homes, node: Node) -> Result<u32, String> {
        if let Some(&at) = self.places.get(&node) {
            return Ok(at);
        }
        let root = self.open(db, homes, node)?;
        // Each frame: a node, and how many of what it names are walked.
        // alloc: once a test not yet reached, as deep as its walk
        let mut frames: Vec<(u32, usize)> = vec![(root, 0)];
        while let Some(frame) = frames.last_mut() {
            let (at, next) = (frame.0, frame.1);
            if let Some(named) = self.visits[at as usize].names.get(next) {
                frame.1 += 1;
                let Some(&to) = self.places.get(named) else {
                    let named = named.clone(); // clone: once a node of the program
                    let to = self.open(db, homes, named)?;
                    frames.push((to, 0));
                    continue;
                };
                let target = &self.visits[to as usize];
                let (on_stack, index, outside) = (target.on_stack, target.index, target.outside);
                let visit = &mut self.visits[at as usize];
                if on_stack {
                    visit.low = visit.low.min(index);
                }
                // One of the same component gives its own by the end of it, and one done
                // gives what its component has.
                visit.outside |= outside;
                continue;
            }
            frames.pop();
            let visit = &mut self.visits[at as usize];
            // alloc: empty, which frees the names walked
            visit.names = Vec::new();
            let (low, outside) = (visit.low, visit.outside);
            if low == at {
                self.close(at);
            }
            if let Some(&(parent, _)) = frames.last() {
                let outside = outside || self.visits[at as usize].outside;
                let visit = &mut self.visits[parent as usize];
                visit.low = visit.low.min(low);
                visit.outside |= outside;
            }
        }
        Ok(root)
    }

    /// Ends the component whose first node is `at`: each of its nodes is outside if one is.
    fn close(&mut self, at: u32) {
        let from = (self.stack.iter().rposition(|&it| it == at))
            .expect("a component's root is on the stack");
        let outside = self.stack[from..].iter().any(|&it| self.visits[it as usize].outside);
        for member in self.stack.drain(from..) {
            let visit = &mut self.visits[member as usize];
            (visit.on_stack, visit.outside) = (false, outside);
        }
    }

    /// Places `node` and reads what it names.
    fn open(&mut self, db: &Db, homes: &mut Homes, node: Node) -> Result<u32, String> {
        let at = u32::try_from(self.visits.len())
            // alloc: the error's message
            .map_err(|_| "a program of more than 2^32 functions and statics".to_owned())?;
        let (outside, names) = self.read(db, homes, &node)?;
        self.places.insert(node, at);
        self.visits.push(Visit { index: at, low: at, on_stack: true, outside, names });
        self.stack.push(at);
        Ok(at)
    }

    /// Whether `node` is outside itself, and the functions and statics it names.
    fn read(
        &self,
        db: &Db,
        homes: &mut Homes,
        node: &Node,
    ) -> Result<(bool, Vec<Node>), String> {
        let refs: Vec<Ref> = match node {
            Node::Function(instance) => match &instance.0 {
                // alloc: empty, which allocates nothing
                Kind::Asm(_) => return Ok((true, Vec::new())),
                // alloc: once a node of the program; the walk without MIR inlining reaches
                // what the one with does, through the callees it inlines
                _ => refs::listed(db, instance, false)?.into_owned(),
            },
            Node::Static(value) => statics::value(db, homes, &self.frontend.hashes, value.0)?.refs,
        };
        let mut outside = false;
        let mut names = Vec::with_capacity(refs.len());
        for it in refs {
            match it {
                Ref::Function(it) => names.push(Node::Function(it)),
                Ref::Static(it) => names.push(Node::Static(it)),
                Ref::Extern(name) | Ref::Weak(name) => {
                    let reads = fncache::outside(&name).map(|it| it.0);
                    outside |= matches!(reads, Some(Reads::Net | Reads::Process));
                }
            }
        }
        Ok((outside, names))
    }
}
