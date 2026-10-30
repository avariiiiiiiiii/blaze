//! Implementation of the cursors -- API for convenient access to syntax trees.
//!
//! Functional programmers will recognize that this module implements a zipper
//! for a purely functional (green) tree.
//!
//! A cursor node (`SyntaxNode`) points to a `GreenNode` and a parent
//! `SyntaxNode`. This allows cursor to provide iteration over both ancestors
//! and descendants, as well as a cheep access to absolute offset of the node in
//! file.
//!
//! A cursor can also point into a [`FlatTree`]: then it is the tree's root and an
//! element's number, and moving is arithmetic on the tree's arrays, with nothing
//! allocated. A green tree of a flat one's part is made only when asked for.
//!
//! A flat tree's lazy nodes are transparent: moving down into one moves into its part
//! (parsed then, once), whose root stands for the lazy node itself, and moving up or
//! sideways out of a part's children moves back into the file's tree.

// Implementation notes:
//
// The implementation is utterly and horribly unsafe. This whole module is an
// unsafety boundary. It is believed that the API here is, in principle, sound,
// but the implementation might have bugs.
//
// The core type is `NodeData` -- a heap-allocated reference counted object,
// which points to a green node or a green token, and to the parent `NodeData`.
// Publicly-exposed `SyntaxNode` and `SyntaxToken` own a reference to
// `NodeData`.
//
// `NodeData`s are transient, and are created and destroyed during tree
// traversals. In general, only currently referenced nodes and their ancestors
// are alive at any given moment.
//
// More specifically, `NodeData`'s ref count is equal to the number of
// outstanding `SyntaxNode` and `SyntaxToken` plus the number of children with
// non-zero ref counts. For example, if the user has only a single `SyntaxNode`
// pointing somewhere in the middle of the tree, then all `NodeData` on the path
// from that point towards the root have ref count equal to one.
//
// A root `NodeData` owns its green node and is responsible for freeing it.
//
// A `FlatRoot` is counted by every cursor into its tree, and owns the tree and
// the green trees made of its parts, which live as long as it does: a `&GreenNodeData`
// handed out for a flat node points into one of them. Two roots of one tree have equal
// cursors, as two roots of one green node do.
//
// A cursor into a lazy node's part points at a `FlatRoot` of the part, which the file's
// `FlatRoot` makes the first time a cursor enters the part and owns: such a root has no
// count of its own, and a cursor into it counts the file's root, so it lives as long as any
// cursor into the file. A cursor never points at a part's root element: that element is the
// lazy node, so a cursor at it points at the lazy node in the file's tree (`flat_ref`), and
// equal elements are equal cursors. Parts have no lazy nodes of their own (`FlatTree::part`
// checks), so a part's root is always a file root's.

use std::{
    cell::{Cell, RefCell},
    fmt,
    hash::{Hash, Hasher},
    iter,
    mem::ManuallyDrop,
    ptr, slice,
    sync::Arc,
};

use countme::Count;
use rustc_hash::FxHashMap;

use crate::{
    flat::FlatTree,
    green::{GreenChild, GreenElementRef, GreenNodeData, GreenTokenData, SyntaxKind},
    Direction, GreenNode, GreenToken, NodeCache, NodeOrToken, SyntaxText, TextRange, TextSize,
    TokenAtOffset, WalkEvent,
};

enum Green {
    Node { ptr: ptr::NonNull<GreenNodeData> },
    Token { ptr: ptr::NonNull<GreenTokenData> },
}

struct _SyntaxElement;

struct NodeData {
    _c: Count<_SyntaxElement>,

    rc: Cell<u32>,
    parent: Option<ptr::NonNull<NodeData>>,
    index: u32,
    green: Green,
    offset: TextSize,
}

struct FlatRoot {
    /// The cursors into this tree and its parts; unused in a part's root.
    rc: Cell<u32>,
    tree: Arc<FlatTree>,
    greens: RefCell<FxHashMap<u32, NodeOrToken<GreenNode, GreenToken>>>,
    /// For a part's root: the file's root, which owns it, and the lazy node it is the part of.
    file: Option<(ptr::NonNull<FlatRoot>, u32)>,
    /// For a file's root: the roots of the parts cursors entered, by their lazy node.
    parts: RefCell<FxHashMap<u32, ptr::NonNull<FlatRoot>>>,
}

impl Drop for FlatRoot {
    fn drop(&mut self) {
        for (_, part) in self.parts.get_mut().drain() {
            // SAFETY: a part's root is made by `part_root` from a box and freed only here.
            drop(unsafe { Box::from_raw(part.as_ptr()) })
        }
    }
}

/// What a cursor points at: a `NodeData`, or an element of a flat tree.
enum Ref {
    Green(ptr::NonNull<NodeData>),
    Flat(ptr::NonNull<FlatRoot>, u32),
}

impl Clone for Ref {
    #[inline]
    fn clone(&self) -> Self {
        match *self {
            Ref::Green(ptr) => {
                data(&ptr).inc_rc();
                Ref::Green(ptr)
            }
            Ref::Flat(root, at) => {
                flat(&owner(root)).inc_rc();
                Ref::Flat(root, at)
            }
        }
    }
}

impl Drop for Ref {
    #[inline]
    fn drop(&mut self) {
        match *self {
            Ref::Green(ptr) => {
                if data(&ptr).dec_rc() {
                    unsafe { free(ptr) }
                }
            }
            Ref::Flat(root, _) => {
                let owner = owner(root);
                if flat(&owner).dec_rc() {
                    drop(unsafe { Box::from_raw(owner.as_ptr()) })
                }
            }
        }
    }
}

#[inline]
fn data(ptr: &ptr::NonNull<NodeData>) -> &NodeData {
    unsafe { ptr.as_ref() }
}

#[inline]
fn flat(root: &ptr::NonNull<FlatRoot>) -> &FlatRoot {
    unsafe { root.as_ref() }
}

/// The root that owns `root` and counts the cursors into it: the file's for a part's.
#[inline]
fn owner(root: ptr::NonNull<FlatRoot>) -> ptr::NonNull<FlatRoot> {
    match flat(&root).file {
        Some((file, _)) => file,
        None => root,
    }
}

impl FlatRoot {
    /// The root of the part of lazy node `at` of this file's tree, made the first time.
    fn part_root(&self, this: ptr::NonNull<FlatRoot>, at: u32) -> ptr::NonNull<FlatRoot> {
        debug_assert!(self.file.is_none(), "a part has no lazy nodes");
        if let Some(&part) = self.parts.borrow().get(&at) {
            return part;
        }
        let tree = self.tree.part(at).clone();
        let part = FlatRoot {
            rc: Cell::new(0),
            tree,
            greens: RefCell::default(),
            file: Some((this, at)),
            parts: RefCell::default(),
        };
        let part = unsafe { ptr::NonNull::new_unchecked(Box::into_raw(Box::new(part))) };
        self.parts.borrow_mut().insert(at, part);
        part
    }

    #[inline]
    fn inc_rc(&self) {
        let rc = match self.rc.get().checked_add(1) {
            Some(it) => it,
            None => std::process::abort(),
        };
        self.rc.set(rc)
    }

    #[inline]
    fn dec_rc(&self) -> bool {
        let rc = self.rc.get() - 1;
        self.rc.set(rc);
        rc == 0
    }

    /// The green tree of element `at`, made the first time it is asked for and kept as
    /// long as the root.
    fn green(&self, at: u32) -> GreenElementRef<'_> {
        let mut greens = self.greens.borrow_mut();
        let green = greens.entry(at).or_insert_with(|| {
            let tree = &self.tree;
            if tree.is_token(at) {
                NodeOrToken::Token(GreenToken::new(tree.kind(at), tree.token_text(at)))
            } else {
                NodeOrToken::Node(tree.green(at, &mut NodeCache::default()))
            }
        });
        let green: GreenElementRef<'_> = match green {
            NodeOrToken::Node(it) => NodeOrToken::Node(&**it),
            NodeOrToken::Token(it) => NodeOrToken::Token(&**it),
        };
        // SAFETY: a green tree is on the heap and stays in `greens`, which only grows, for as
        // long as `self` lives; moving the map's entries does not move what they point to.
        unsafe { std::mem::transmute::<GreenElementRef<'_>, GreenElementRef<'_>>(green) }
    }
}

/// A cursor at element `at` of the flat tree `root`: at the lazy node for a part's root.
#[inline]
fn flat_ref(root: ptr::NonNull<FlatRoot>, at: u32) -> Ref {
    let (root, at) = match flat(&root).file {
        Some(lazy) if at == 0 => lazy,
        _ => (root, at),
    };
    flat(&owner(root)).inc_rc();
    Ref::Flat(root, at)
}

#[inline]
fn flat_element(root: ptr::NonNull<FlatRoot>, at: u32) -> SyntaxElement {
    let token = flat(&root).tree.is_token(at);
    let element = flat_ref(root, at);
    match token {
        true => NodeOrToken::Token(SyntaxToken(element)),
        false => NodeOrToken::Node(SyntaxNode(element)),
    }
}

/// Where the children of element `at` of `root` are: a lazy node's in its part, under the
/// part's root.
#[inline]
fn inner(root: ptr::NonNull<FlatRoot>, at: u32) -> (ptr::NonNull<FlatRoot>, u32) {
    match flat(&root).tree.is_lazy(at) {
        true => (flat(&root).part_root(root, at), 0),
        false => (root, at),
    }
}

/// The parent of element `at` of `root`: the lazy node for a child of a part's root.
#[inline]
fn up(root: ptr::NonNull<FlatRoot>, at: u32) -> Option<(ptr::NonNull<FlatRoot>, u32)> {
    let parent = flat(&root).tree.parent(at)?;
    Some(match flat(&root).file {
        Some(lazy) if parent == 0 => lazy,
        _ => (root, parent),
    })
}

/// [`flat`] for a root that is not borrowed from a cursor.
///
/// SAFETY: the caller holds a cursor into `root`'s file, which keeps `root` alive (a part's
/// root lives as long as its file's), for as long as the reference is used.
#[inline]
unsafe fn flat_unbound<'a>(root: ptr::NonNull<FlatRoot>) -> &'a FlatRoot {
    unsafe { &*root.as_ptr() }
}

/// What stepping down by `child_or_token_at_range` from element `at` of `root` finds first
/// of `kind` and `range`, a range that is not empty. The arrays' search sees a lazy node as
/// a node without children, so when it finds nothing and the range is inside a lazy node,
/// the search goes on in its part: what is above the lazy node was searched already.
fn flat_node_at_range(
    root: ptr::NonNull<FlatRoot>,
    at: u32,
    range: TextRange,
    kind: SyntaxKind,
) -> Option<SyntaxNode> {
    let (root, at) = inner(root, at);
    let tree = &flat(&root).tree;
    if let Some(found) = tree.node_at_range(at, range, kind) {
        return Some(SyntaxNode(flat_ref(root, found)));
    }
    let lazy = tree
        .last_starting(at, range.start())
        .filter(|&it| tree.is_lazy(it) && tree.range(it).contains_range(range))?;
    let (part, at) = inner(root, lazy);
    flat(&part).tree.node_at_range(at, range, kind).map(|it| SyntaxNode(flat_ref(part, it)))
}

pub type SyntaxElement = NodeOrToken<SyntaxNode, SyntaxToken>;

#[derive(Clone)]
pub struct SyntaxNode(Ref);

#[derive(Clone)]
pub struct SyntaxToken(Ref);

#[inline(never)]
unsafe fn free(mut data: ptr::NonNull<NodeData>) {
    loop {
        debug_assert_eq!(data.as_ref().rc.get(), 0);
        let node = Box::from_raw(data.as_ptr());
        match node.parent {
            Some(parent) => {
                debug_assert!(parent.as_ref().rc.get() > 0);
                if parent.as_ref().dec_rc() {
                    data = parent;
                } else {
                    break;
                }
            }
            None => {
                if let Green::Node { ptr } = &node.green {
                    let _ = GreenNode::from_raw(*ptr);
                } else {
                    unreachable!("a token cannot be a root");
                }
                break;
            }
        }
    }
}

impl NodeData {
    #[inline]
    fn new(
        parent: Option<SyntaxNode>,
        index: u32,
        offset: TextSize,
        green: Green,
    ) -> ptr::NonNull<NodeData> {
        let parent = ManuallyDrop::new(parent);
        let res = NodeData {
            _c: Count::new(),
            rc: Cell::new(1),
            parent: parent.as_ref().map(|it| match it.0 {
                Ref::Green(ptr) => ptr,
                Ref::Flat(..) => unreachable!("a green node's parent is green"),
            }),
            index,
            green,
            offset,
        };
        unsafe { ptr::NonNull::new_unchecked(Box::into_raw(Box::new(res))) }
    }

    #[inline]
    fn inc_rc(&self) {
        let rc = match self.rc.get().checked_add(1) {
            Some(it) => it,
            None => std::process::abort(),
        };
        self.rc.set(rc)
    }

    #[inline]
    fn dec_rc(&self) -> bool {
        let rc = self.rc.get() - 1;
        self.rc.set(rc);
        rc == 0
    }

    #[inline]
    fn key(&self) -> (ptr::NonNull<()>, TextSize) {
        let ptr = match &self.green {
            Green::Node { ptr } => ptr.cast(),
            Green::Token { ptr } => ptr.cast(),
        };
        (ptr, self.offset())
    }

    #[inline]
    fn parent_node(&self) -> Option<SyntaxNode> {
        let parent = self.parent()?;
        debug_assert!(matches!(parent.green, Green::Node { .. }));
        parent.inc_rc();
        Some(SyntaxNode(Ref::Green(ptr::NonNull::from(parent))))
    }

    #[inline]
    fn parent(&self) -> Option<&NodeData> {
        self.parent.map(|it| unsafe { &*it.as_ptr() })
    }

    #[inline]
    fn green(&self) -> GreenElementRef<'_> {
        match &self.green {
            Green::Node { ptr } => GreenElementRef::Node(unsafe { &*ptr.as_ptr() }),
            Green::Token { ptr } => GreenElementRef::Token(unsafe { ptr.as_ref() }),
        }
    }
    #[inline]
    fn green_siblings(&self) -> slice::Iter<'_, GreenChild> {
        match &self.parent().map(|it| &it.green) {
            Some(Green::Node { ptr }) => unsafe { &*ptr.as_ptr() }.children().raw,
            Some(Green::Token { .. }) => {
                debug_assert!(false);
                [].iter()
            }
            None => [].iter(),
        }
    }
    #[inline]
    fn index(&self) -> u32 {
        self.index
    }

    #[inline]
    fn offset(&self) -> TextSize {
        self.offset
    }

    #[inline]
    fn text_range(&self) -> TextRange {
        let offset = self.offset();
        let len = self.green().text_len();
        TextRange::at(offset, len)
    }

    #[inline]
    fn kind(&self) -> SyntaxKind {
        self.green().kind()
    }

    fn next_sibling(&self) -> Option<SyntaxNode> {
        let mut siblings = self.green_siblings().enumerate();
        let index = self.index() as usize;

        siblings.nth(index);
        siblings.find_map(|(index, child)| {
            child.as_ref().into_node().and_then(|green| {
                let parent = self.parent_node()?;
                let offset = parent.offset() + child.rel_offset();
                Some(SyntaxNode::new_child(green, parent, index as u32, offset))
            })
        })
    }
    fn prev_sibling(&self) -> Option<SyntaxNode> {
        let mut rev_siblings = self.green_siblings().enumerate().rev();
        let index = rev_siblings.len().checked_sub(self.index() as usize + 1)?;

        rev_siblings.nth(index);
        rev_siblings.find_map(|(index, child)| {
            child.as_ref().into_node().and_then(|green| {
                let parent = self.parent_node()?;
                let offset = parent.offset() + child.rel_offset();
                Some(SyntaxNode::new_child(green, parent, index as u32, offset))
            })
        })
    }

    fn next_sibling_or_token(&self) -> Option<SyntaxElement> {
        let mut siblings = self.green_siblings().enumerate();
        let index = self.index() as usize + 1;

        siblings.nth(index).and_then(|(index, child)| {
            let parent = self.parent_node()?;
            let offset = parent.offset() + child.rel_offset();
            Some(SyntaxElement::new(child.as_ref(), parent, index as u32, offset))
        })
    }
    fn prev_sibling_or_token(&self) -> Option<SyntaxElement> {
        let mut siblings = self.green_siblings().enumerate();
        let index = self.index().checked_sub(1)? as usize;

        siblings.nth(index).and_then(|(index, child)| {
            let parent = self.parent_node()?;
            let offset = parent.offset() + child.rel_offset();
            Some(SyntaxElement::new(child.as_ref(), parent, index as u32, offset))
        })
    }
}

impl Ref {
    #[inline]
    fn kind(&self) -> SyntaxKind {
        match self {
            Ref::Green(ptr) => data(ptr).kind(),
            Ref::Flat(root, at) => flat(root).tree.kind(*at),
        }
    }

    #[inline]
    fn text_range(&self) -> TextRange {
        match self {
            Ref::Green(ptr) => data(ptr).text_range(),
            Ref::Flat(root, at) => flat(root).tree.range(*at),
        }
    }

    #[inline]
    fn index(&self) -> usize {
        match self {
            Ref::Green(ptr) => data(ptr).index() as usize,
            Ref::Flat(root, at) => flat(root).tree.index(*at),
        }
    }

    #[inline]
    fn green(&self) -> GreenElementRef<'_> {
        match self {
            Ref::Green(ptr) => data(ptr).green(),
            Ref::Flat(root, at) => flat(root).green(*at),
        }
    }

    #[inline]
    fn parent(&self) -> Option<SyntaxNode> {
        match self {
            Ref::Green(ptr) => data(ptr).parent_node(),
            Ref::Flat(root, at) => {
                up(*root, *at).map(|(root, it)| SyntaxNode(flat_ref(root, it)))
            }
        }
    }

    #[inline]
    fn next_sibling_or_token(&self) -> Option<SyntaxElement> {
        match self {
            Ref::Green(ptr) => data(ptr).next_sibling_or_token(),
            Ref::Flat(root, at) => {
                flat(root).tree.next_sibling_or_token(*at).map(|it| flat_element(*root, it))
            }
        }
    }

    #[inline]
    fn prev_sibling_or_token(&self) -> Option<SyntaxElement> {
        match self {
            Ref::Green(ptr) => data(ptr).prev_sibling_or_token(),
            Ref::Flat(root, at) => {
                flat(root).tree.prev_sibling_or_token(*at).map(|it| flat_element(*root, it))
            }
        }
    }

    #[inline]
    fn key(&self) -> (usize, u32) {
        match self {
            Ref::Green(ptr) => {
                let (green, offset) = data(ptr).key();
                (green.as_ptr() as usize, offset.into())
            }
            Ref::Flat(root, at) => (Arc::as_ptr(&flat(root).tree) as usize, *at),
        }
    }

    /// The element of a green tree equal to the flat one this points into that is where
    /// this element is.
    fn green_twin(&self) -> SyntaxElement {
        let Ref::Flat(root, _) = self else { unreachable!("a green element is its own twin") };
        let mut path = Vec::new();
        let mut it = self.clone();
        while let Some(parent) = it.parent() {
            path.push(it.index());
            it = parent.0;
        }
        let top = owner(*root);
        let green = flat(&top).green(0).into_node().expect("the root is a node").to_owned();
        let mut element: SyntaxElement = SyntaxNode::new_root(green).into();
        for index in path.into_iter().rev() {
            let node = element.into_node().expect("a token has no children");
            element = node.children_with_tokens().nth(index).expect("the twin has the child");
        }
        element
    }
}

impl SyntaxNode {
    pub fn new_root(green: GreenNode) -> SyntaxNode {
        let green = GreenNode::into_raw(green);
        let green = Green::Node { ptr: green };
        SyntaxNode(Ref::Green(NodeData::new(None, 0, 0.into(), green)))
    }

    /// The root of a flat tree.
    pub fn new_flat_root(tree: Arc<FlatTree>) -> SyntaxNode {
        assert!(!tree.is_empty() && !tree.is_token(0), "a tree's root is a node");
        let root = FlatRoot {
            rc: Cell::new(1),
            tree,
            greens: RefCell::default(),
            file: None,
            parts: RefCell::default(),
        };
        let root = unsafe { ptr::NonNull::new_unchecked(Box::into_raw(Box::new(root))) };
        SyntaxNode(Ref::Flat(root, 0))
    }

    /// Whether this is a lazy node of a flat tree whose part is not parsed yet. Never parses.
    #[inline]
    pub fn is_lazy(&self) -> bool {
        match &self.0 {
            Ref::Flat(root, at) => {
                let tree = &flat(root).tree;
                tree.is_lazy(*at) && !tree.is_parsed(*at)
            }
            Ref::Green(_) => false,
        }
    }

    /// The flat tree this node is in, if it is in one: a lazy node's part for a node inside
    /// it.
    pub fn flat_tree(&self) -> Option<&Arc<FlatTree>> {
        match &self.0 {
            Ref::Green(_) => None,
            Ref::Flat(root, _) => Some(&flat(root).tree),
        }
    }

    /// The flat tree this node is in and the node's number in it, if it is in one: a lazy
    /// node's part for a node inside it, the file's tree for the lazy node itself.
    #[inline]
    pub fn flat(&self) -> Option<(&Arc<FlatTree>, u32)> {
        match &self.0 {
            Ref::Green(_) => None,
            Ref::Flat(root, at) => Some((&flat(root).tree, *at)),
        }
    }

    /// Element `at` of the flat tree this node is in.
    #[inline]
    pub fn flat_element(&self, at: u32) -> SyntaxElement {
        match self.0 {
            Ref::Flat(root, _) => flat_element(root, at),
            Ref::Green(_) => panic!("a green node is in no flat tree"),
        }
    }

    /// This node in a flat tree, for a walk that reads the arrays: itself when it is in one,
    /// else the same node of a flat tree made of the green tree it is in.
    pub fn to_flat(&self) -> SyntaxNode {
        if let Ref::Flat(..) = self.0 {
            return self.clone();
        }
        let mut builder = crate::FlatTreeBuilder::new();
        let (mut elements, mut at) = (0, 0);
        for event in self.tree_top().preorder_with_tokens() {
            match event {
                WalkEvent::Enter(NodeOrToken::Node(node)) => {
                    if node == *self {
                        at = elements;
                    }
                    builder.start_node(node.kind());
                    elements += 1;
                }
                WalkEvent::Enter(NodeOrToken::Token(token)) => {
                    builder.token(token.kind(), token.text());
                    elements += 1;
                }
                WalkEvent::Leave(NodeOrToken::Node(_)) => builder.finish_node(),
                WalkEvent::Leave(NodeOrToken::Token(_)) => {}
            }
        }
        let root = SyntaxNode::new_flat_root(Arc::new(builder.finish()));
        root.flat_element(at).into_node().expect("a node's twin is a node")
    }

    /// The first child node whose kind `pred` accepts.
    #[inline]
    pub fn child_where(&self, mut pred: impl FnMut(SyntaxKind) -> bool) -> Option<SyntaxNode> {
        match self.0 {
            Ref::Flat(root, at) => {
                let (root, at) = inner(root, at);
                let tree = &flat(&root).tree;
                let child = tree.children(at).find(|&it| pred(tree.kind(it)))?;
                Some(SyntaxNode(flat_ref(root, child)))
            }
            Ref::Green(_) => self.children().find(|it| pred(it.kind())),
        }
    }

    /// The nearest of this node and its ancestors whose kind `pred` accepts.
    #[inline]
    pub fn ancestor_where(&self, mut pred: impl FnMut(SyntaxKind) -> bool) -> Option<SyntaxNode> {
        match self.0 {
            Ref::Flat(root, at) => {
                let (mut root, mut it) = (root, at);
                while !pred(flat(&root).tree.kind(it)) {
                    (root, it) = up(root, it)?;
                }
                Some(SyntaxNode(flat_ref(root, it)))
            }
            Ref::Green(_) => self.ancestors().find(|it| pred(it.kind())),
        }
    }

    /// The first child token of `kind`.
    #[inline]
    pub fn child_token(&self, kind: SyntaxKind) -> Option<SyntaxToken> {
        match self.0 {
            Ref::Flat(root, at) => {
                let (root, at) = inner(root, at);
                let tree = &flat(&root).tree;
                let token = tree
                    .children_with_tokens(at)
                    .find(|&it| tree.is_token(it) && tree.kind(it) == kind)?;
                Some(SyntaxToken(flat_ref(root, token)))
            }
            Ref::Green(_) => self
                .children_with_tokens()
                .filter_map(NodeOrToken::into_token)
                .find(|it| it.kind() == kind),
        }
    }

    /// The child nodes, each step taking the next whose kind a predicate accepts.
    #[inline]
    pub fn children_where(&self) -> SyntaxNodeChildrenWhere {
        SyntaxNodeChildrenWhere(match self.0 {
            Ref::Flat(root, at) => {
                let (root, at) = inner(root, at);
                let end = flat(&root).tree.end(at);
                ChildrenWhere::Flat { _parent: self.clone(), root, next: at + 1, end }
            }
            Ref::Green(_) => ChildrenWhere::Green(self.children()),
        })
    }

    fn new_child(
        green: &GreenNodeData,
        parent: SyntaxNode,
        index: u32,
        offset: TextSize,
    ) -> SyntaxNode {
        let green = Green::Node { ptr: green.into() };
        SyntaxNode(Ref::Green(NodeData::new(Some(parent), index, offset, green)))
    }

    pub fn clone_subtree(&self) -> SyntaxNode {
        match &self.0 {
            Ref::Green(_) => SyntaxNode::new_root(self.green().to_owned()),
            Ref::Flat(root, at) => {
                let tree = &flat(root).tree;
                let whole = flat(root).file.is_none() && tree.range(0).start() == TextSize::from(0);
                match *at == 0 && whole {
                    true => SyntaxNode(flat_ref(*root, 0)),
                    false => SyntaxNode::new_flat_root(Arc::new(tree.subtree(*at))),
                }
            }
        }
    }

    pub fn replace_with(&self, replacement: GreenNode) -> GreenNode {
        assert_eq!(self.kind(), replacement.kind());
        if let Ref::Flat(..) = self.0 {
            return self.0.green_twin().into_node().unwrap().replace_with(replacement);
        }
        match &self.parent() {
            None => replacement,
            Some(parent) => {
                let new_parent = parent.green_ref().replace_child(self.index(), replacement.into());
                parent.replace_with(new_parent)
            }
        }
    }

    #[inline]
    pub fn kind(&self) -> SyntaxKind {
        self.0.kind()
    }

    #[inline]
    fn offset(&self) -> TextSize {
        self.text_range().start()
    }

    #[inline]
    pub fn text_range(&self) -> TextRange {
        self.0.text_range()
    }

    #[inline]
    pub fn index(&self) -> usize {
        self.0.index()
    }

    #[inline]
    pub fn text(&self) -> SyntaxText {
        SyntaxText::new(self.clone())
    }

    #[inline]
    pub fn green(&self) -> &GreenNodeData {
        self.green_ref()
    }

    /// The text of the node's first child, when that is a token.
    #[inline]
    pub fn first_child_token_text(&self) -> Option<&str> {
        match &self.0 {
            Ref::Green(_) => Some(self.green_ref().children().next()?.into_token()?.text()),
            Ref::Flat(root, at) => {
                let (root, at) = inner(*root, *at);
                // SAFETY: `self` is a cursor into the file `root` is the tree or a part of.
                let tree = &unsafe { flat_unbound(root) }.tree;
                let first = tree.first_child_or_token(at).filter(|&it| tree.is_token(it))?;
                Some(tree.token_text(first))
            }
        }
    }
    #[inline]
    fn green_ref(&self) -> &GreenNodeData {
        self.0.green().into_node().unwrap()
    }

    #[inline]
    pub fn parent(&self) -> Option<SyntaxNode> {
        self.0.parent()
    }

    #[inline]
    pub fn ancestors(&self) -> impl Iterator<Item = SyntaxNode> {
        iter::successors(Some(self.clone()), SyntaxNode::parent)
    }

    #[inline]
    pub fn tree_top(&self) -> SyntaxNode {
        match &self.0 {
            Ref::Green(_) => self.ancestors().last().unwrap(),
            Ref::Flat(root, _) => SyntaxNode(flat_ref(owner(*root), 0)),
        }
    }

    #[inline]
    pub fn children(&self) -> SyntaxNodeChildren {
        SyntaxNodeChildren::new(self.clone())
    }

    #[inline]
    pub fn children_with_tokens(&self) -> SyntaxElementChildren {
        SyntaxElementChildren::new(self.clone())
    }

    pub fn first_child(&self) -> Option<SyntaxNode> {
        if let Ref::Flat(root, at) = self.0 {
            let (root, at) = inner(root, at);
            return flat(&root).tree.first_child(at).map(|it| SyntaxNode(flat_ref(root, it)));
        }
        self.green_ref().children().raw.enumerate().find_map(|(index, child)| {
            child.as_ref().into_node().map(|green| {
                SyntaxNode::new_child(
                    green,
                    self.clone(),
                    index as u32,
                    self.offset() + child.rel_offset(),
                )
            })
        })
    }
    pub fn last_child(&self) -> Option<SyntaxNode> {
        if let Ref::Flat(root, at) = self.0 {
            let (root, at) = inner(root, at);
            let tree = &flat(&root).tree;
            let mut child = tree.last_child_or_token(at);
            while let Some(it) = child {
                if !tree.is_token(it) {
                    return Some(SyntaxNode(flat_ref(root, it)));
                }
                child = tree.prev_sibling_or_token(it);
            }
            return None;
        }
        self.green_ref().children().raw.enumerate().rev().find_map(|(index, child)| {
            child.as_ref().into_node().map(|green| {
                SyntaxNode::new_child(
                    green,
                    self.clone(),
                    index as u32,
                    self.offset() + child.rel_offset(),
                )
            })
        })
    }

    pub fn first_child_or_token(&self) -> Option<SyntaxElement> {
        if let Ref::Flat(root, at) = self.0 {
            let (root, at) = inner(root, at);
            return flat(&root).tree.first_child_or_token(at).map(|it| flat_element(root, it));
        }
        self.green_ref().children().raw.next().map(|child| {
            SyntaxElement::new(child.as_ref(), self.clone(), 0, self.offset() + child.rel_offset())
        })
    }
    pub fn last_child_or_token(&self) -> Option<SyntaxElement> {
        if let Ref::Flat(root, at) = self.0 {
            let (root, at) = inner(root, at);
            return flat(&root).tree.last_child_or_token(at).map(|it| flat_element(root, it));
        }
        self.green_ref().children().raw.enumerate().next_back().map(|(index, child)| {
            SyntaxElement::new(
                child.as_ref(),
                self.clone(),
                index as u32,
                self.offset() + child.rel_offset(),
            )
        })
    }

    pub fn next_sibling(&self) -> Option<SyntaxNode> {
        if let Ref::Flat(root, at) = self.0 {
            return flat(&root).tree.next_sibling(at).map(|it| SyntaxNode(flat_ref(root, it)));
        }
        data(self.green_ptr()).next_sibling()
    }
    pub fn prev_sibling(&self) -> Option<SyntaxNode> {
        if let Ref::Flat(root, at) = self.0 {
            let tree = &flat(&root).tree;
            let mut sibling = tree.prev_sibling_or_token(at);
            while let Some(it) = sibling {
                if !tree.is_token(it) {
                    return Some(SyntaxNode(flat_ref(root, it)));
                }
                sibling = tree.prev_sibling_or_token(it);
            }
            return None;
        }
        data(self.green_ptr()).prev_sibling()
    }

    #[inline]
    fn green_ptr(&self) -> &ptr::NonNull<NodeData> {
        match &self.0 {
            Ref::Green(ptr) => ptr,
            Ref::Flat(..) => unreachable!("a flat node has no NodeData"),
        }
    }

    pub fn next_sibling_or_token(&self) -> Option<SyntaxElement> {
        self.0.next_sibling_or_token()
    }
    pub fn prev_sibling_or_token(&self) -> Option<SyntaxElement> {
        self.0.prev_sibling_or_token()
    }

    pub fn first_token(&self) -> Option<SyntaxToken> {
        self.first_child_or_token()?.first_token()
    }
    pub fn last_token(&self) -> Option<SyntaxToken> {
        self.last_child_or_token()?.last_token()
    }

    #[inline]
    pub fn siblings(&self, direction: Direction) -> impl Iterator<Item = SyntaxNode> {
        iter::successors(Some(self.clone()), move |node| match direction {
            Direction::Next => node.next_sibling(),
            Direction::Prev => node.prev_sibling(),
        })
    }

    #[inline]
    pub fn siblings_with_tokens(
        &self,
        direction: Direction,
    ) -> impl Iterator<Item = SyntaxElement> {
        let me: SyntaxElement = self.clone().into();
        iter::successors(Some(me), move |el| match direction {
            Direction::Next => el.next_sibling_or_token(),
            Direction::Prev => el.prev_sibling_or_token(),
        })
    }

    #[inline]
    pub fn descendants(&self) -> impl Iterator<Item = SyntaxNode> {
        self.preorder().filter_map(|event| match event {
            WalkEvent::Enter(node) => Some(node),
            WalkEvent::Leave(_) => None,
        })
    }

    #[inline]
    pub fn descendants_with_tokens(&self) -> impl Iterator<Item = SyntaxElement> {
        self.preorder_with_tokens().filter_map(|event| match event {
            WalkEvent::Enter(it) => Some(it),
            WalkEvent::Leave(_) => None,
        })
    }

    #[inline]
    pub fn preorder(&self) -> Preorder {
        Preorder::new(self.clone())
    }

    #[inline]
    pub fn preorder_with_tokens(&self) -> PreorderWithTokens {
        PreorderWithTokens::new(self.clone())
    }

    pub fn token_at_offset(&self, offset: TextSize) -> TokenAtOffset<SyntaxToken> {
        // TODO: this could be faster if we first drill-down to node, and only
        // then switch to token search. We should also replace explicit
        // recursion with a loop.
        let range = self.text_range();
        assert!(
            range.start() <= offset && offset <= range.end(),
            "Bad offset: range {:?} offset {:?}",
            range,
            offset
        );
        if range.is_empty() {
            return TokenAtOffset::None;
        }

        let mut children = self.children_with_tokens().filter(|child| {
            let child_range = child.text_range();
            !child_range.is_empty()
                && (child_range.start() <= offset && offset <= child_range.end())
        });

        let left = children.next().unwrap();
        let right = children.next();
        assert!(children.next().is_none());

        if let Some(right) = right {
            match (left.token_at_offset(offset), right.token_at_offset(offset)) {
                (TokenAtOffset::Single(left), TokenAtOffset::Single(right)) => {
                    TokenAtOffset::Between(left, right)
                }
                _ => unreachable!(),
            }
        } else {
            left.token_at_offset(offset)
        }
    }

    pub fn covering_element(&self, range: TextRange) -> SyntaxElement {
        let mut res: SyntaxElement = self.clone().into();
        loop {
            assert!(
                res.text_range().contains_range(range),
                "Bad range: node range {:?}, range {:?}",
                res.text_range(),
                range,
            );
            res = match &res {
                NodeOrToken::Token(_) => return res,
                NodeOrToken::Node(node) => match node.child_or_token_at_range(range) {
                    Some(it) => it,
                    None => return res,
                },
            };
        }
    }

    /// What stepping down by `child_or_token_at_range` from this node finds first of `kind`
    /// and `range`, when this is a flat tree's node and the range is not empty; `None` when
    /// it is not, for the caller to step.
    pub(crate) fn flat_node_at_range(
        &self,
        range: TextRange,
        kind: SyntaxKind,
    ) -> Option<Option<SyntaxNode>> {
        match self.0 {
            Ref::Flat(root, at) if !range.is_empty() => {
                Some(flat_node_at_range(root, at, range, kind))
            }
            _ => None,
        }
    }

    pub fn child_or_token_at_range(&self, range: TextRange) -> Option<SyntaxElement> {
        if let Ref::Flat(root, at) = self.0 {
            let (root, at) = inner(root, at);
            return flat(&root).tree.child_at_range(at, range).map(|it| flat_element(root, it));
        }
        let rel_range = range - self.offset();
        self.green_ref().child_at_range(rel_range).map(|(index, rel_offset, green)| {
            SyntaxElement::new(green, self.clone(), index as u32, self.offset() + rel_offset)
        })
    }
}

impl SyntaxToken {
    fn new(
        green: &GreenTokenData,
        parent: SyntaxNode,
        index: u32,
        offset: TextSize,
    ) -> SyntaxToken {
        let green = Green::Token { ptr: green.into() };
        SyntaxToken(Ref::Green(NodeData::new(Some(parent), index, offset, green)))
    }

    pub fn replace_with(&self, replacement: GreenToken) -> GreenNode {
        assert_eq!(self.kind(), replacement.kind());
        if let Ref::Flat(..) = self.0 {
            return self.0.green_twin().into_token().unwrap().replace_with(replacement);
        }
        let parent = self.parent().unwrap();
        let me = self.index();

        let new_parent = parent.green_ref().replace_child(me, replacement.into());
        parent.replace_with(new_parent)
    }

    #[inline]
    pub fn kind(&self) -> SyntaxKind {
        self.0.kind()
    }

    #[inline]
    pub fn text_range(&self) -> TextRange {
        self.0.text_range()
    }

    #[inline]
    pub fn index(&self) -> usize {
        self.0.index()
    }

    #[inline]
    pub fn text(&self) -> &str {
        if let Ref::Flat(root, at) = &self.0 {
            return flat(root).tree.token_text(*at);
        }
        match self.0.green().as_token() {
            Some(it) => it.text(),
            None => {
                debug_assert!(
                    false,
                    "corrupted tree: a node thinks it is a token: {:?}",
                    self.0.green().as_node().unwrap().to_string()
                );
                ""
            }
        }
    }

    #[inline]
    pub fn green(&self) -> &GreenTokenData {
        self.0.green().into_token().unwrap()
    }

    #[inline]
    pub fn parent(&self) -> Option<SyntaxNode> {
        self.0.parent()
    }

    #[inline]
    pub fn ancestors(&self) -> impl Iterator<Item = SyntaxNode> {
        std::iter::successors(self.parent(), SyntaxNode::parent)
    }

    #[inline]
    pub fn tree_top(&self) -> SyntaxNode {
        self.ancestors().last().unwrap()
    }

    pub fn next_sibling_or_token(&self) -> Option<SyntaxElement> {
        self.0.next_sibling_or_token()
    }
    pub fn prev_sibling_or_token(&self) -> Option<SyntaxElement> {
        self.0.prev_sibling_or_token()
    }

    #[inline]
    pub fn siblings_with_tokens(
        &self,
        direction: Direction,
    ) -> impl Iterator<Item = SyntaxElement> {
        let me: SyntaxElement = self.clone().into();
        iter::successors(Some(me), move |el| match direction {
            Direction::Next => el.next_sibling_or_token(),
            Direction::Prev => el.prev_sibling_or_token(),
        })
    }

    pub fn next_token(&self) -> Option<SyntaxToken> {
        match self.next_sibling_or_token() {
            Some(element) => element.first_token(),
            None => self
                .ancestors()
                .find_map(|it| it.next_sibling_or_token())
                .and_then(|element| element.first_token()),
        }
    }
    pub fn prev_token(&self) -> Option<SyntaxToken> {
        match self.prev_sibling_or_token() {
            Some(element) => element.last_token(),
            None => self
                .ancestors()
                .find_map(|it| it.prev_sibling_or_token())
                .and_then(|element| element.last_token()),
        }
    }
}

impl SyntaxElement {
    fn new(
        element: GreenElementRef<'_>,
        parent: SyntaxNode,
        index: u32,
        offset: TextSize,
    ) -> SyntaxElement {
        match element {
            NodeOrToken::Node(node) => {
                SyntaxNode::new_child(node, parent, index as u32, offset).into()
            }
            NodeOrToken::Token(token) => {
                SyntaxToken::new(token, parent, index as u32, offset).into()
            }
        }
    }

    #[inline]
    pub fn text_range(&self) -> TextRange {
        match self {
            NodeOrToken::Node(it) => it.text_range(),
            NodeOrToken::Token(it) => it.text_range(),
        }
    }

    #[inline]
    pub fn index(&self) -> usize {
        match self {
            NodeOrToken::Node(it) => it.index(),
            NodeOrToken::Token(it) => it.index(),
        }
    }

    #[inline]
    pub fn kind(&self) -> SyntaxKind {
        match self {
            NodeOrToken::Node(it) => it.kind(),
            NodeOrToken::Token(it) => it.kind(),
        }
    }

    #[inline]
    pub fn parent(&self) -> Option<SyntaxNode> {
        match self {
            NodeOrToken::Node(it) => it.parent(),
            NodeOrToken::Token(it) => it.parent(),
        }
    }

    #[inline]
    pub fn ancestors(&self) -> impl Iterator<Item = SyntaxNode> {
        let first = match self {
            NodeOrToken::Node(it) => Some(it.clone()),
            NodeOrToken::Token(it) => it.parent(),
        };
        iter::successors(first, SyntaxNode::parent)
    }

    #[inline]
    pub fn tree_top(&self) -> SyntaxNode {
        match self {
            NodeOrToken::Node(it) => it.tree_top(),
            NodeOrToken::Token(it) => it.tree_top(),
        }
    }

    pub fn first_token(&self) -> Option<SyntaxToken> {
        match self {
            NodeOrToken::Node(it) => it.first_token(),
            NodeOrToken::Token(it) => Some(it.clone()),
        }
    }
    pub fn last_token(&self) -> Option<SyntaxToken> {
        match self {
            NodeOrToken::Node(it) => it.last_token(),
            NodeOrToken::Token(it) => Some(it.clone()),
        }
    }

    pub fn next_sibling_or_token(&self) -> Option<SyntaxElement> {
        match self {
            NodeOrToken::Node(it) => it.next_sibling_or_token(),
            NodeOrToken::Token(it) => it.next_sibling_or_token(),
        }
    }
    pub fn prev_sibling_or_token(&self) -> Option<SyntaxElement> {
        match self {
            NodeOrToken::Node(it) => it.prev_sibling_or_token(),
            NodeOrToken::Token(it) => it.prev_sibling_or_token(),
        }
    }

    fn token_at_offset(&self, offset: TextSize) -> TokenAtOffset<SyntaxToken> {
        assert!(self.text_range().start() <= offset && offset <= self.text_range().end());
        match self {
            NodeOrToken::Token(token) => TokenAtOffset::Single(token.clone()),
            NodeOrToken::Node(node) => node.token_at_offset(offset),
        }
    }
}

// region: impls

// Identity semantics for hash & eq
impl PartialEq for SyntaxNode {
    #[inline]
    fn eq(&self, other: &SyntaxNode) -> bool {
        self.0.key() == other.0.key()
    }
}

impl Eq for SyntaxNode {}

impl Hash for SyntaxNode {
    #[inline]
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.key().hash(state);
    }
}

impl fmt::Debug for SyntaxNode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SyntaxNode")
            .field("kind", &self.kind())
            .field("text_range", &self.text_range())
            .finish()
    }
}

impl fmt::Display for SyntaxNode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.preorder_with_tokens()
            .filter_map(|event| match event {
                WalkEvent::Enter(NodeOrToken::Token(token)) => Some(token),
                _ => None,
            })
            .try_for_each(|it| fmt::Display::fmt(&it, f))
    }
}

// Identity semantics for hash & eq
impl PartialEq for SyntaxToken {
    #[inline]
    fn eq(&self, other: &SyntaxToken) -> bool {
        self.0.key() == other.0.key()
    }
}

impl Eq for SyntaxToken {}

impl Hash for SyntaxToken {
    #[inline]
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.key().hash(state);
    }
}

impl fmt::Debug for SyntaxToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SyntaxToken")
            .field("kind", &self.kind())
            .field("text_range", &self.text_range())
            .finish()
    }
}

impl fmt::Display for SyntaxToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self.text(), f)
    }
}

impl From<SyntaxNode> for SyntaxElement {
    #[inline]
    fn from(node: SyntaxNode) -> SyntaxElement {
        NodeOrToken::Node(node)
    }
}

impl From<SyntaxToken> for SyntaxElement {
    #[inline]
    fn from(token: SyntaxToken) -> SyntaxElement {
        NodeOrToken::Token(token)
    }
}

// endregion

// region: iterators

#[derive(Clone, Debug)]
pub struct SyntaxNodeChildren {
    next: Option<SyntaxNode>,
}

impl SyntaxNodeChildren {
    fn new(parent: SyntaxNode) -> SyntaxNodeChildren {
        SyntaxNodeChildren { next: parent.first_child() }
    }
}

impl Iterator for SyntaxNodeChildren {
    type Item = SyntaxNode;
    fn next(&mut self) -> Option<SyntaxNode> {
        self.next.take().map(|next| {
            self.next = next.next_sibling();
            next
        })
    }
}

/// The child nodes of a node whose kinds a predicate accepts, from
/// [`SyntaxNode::children_where`]: a flat node's are read off the arrays, and only those
/// taken are made cursors.
#[derive(Clone, Debug)]
pub struct SyntaxNodeChildrenWhere(ChildrenWhere);

#[derive(Clone, Debug)]
enum ChildrenWhere {
    /// The children from `next` to `end` of `root`'s tree, `_parent` (whose children they
    /// are, in its part when it is a lazy node) keeping `root` alive.
    Flat { _parent: SyntaxNode, root: ptr::NonNull<FlatRoot>, next: u32, end: u32 },
    Green(SyntaxNodeChildren),
}

impl SyntaxNodeChildrenWhere {
    /// The next child whose kind `pred` accepts.
    #[inline]
    pub fn next_where(&mut self, mut pred: impl FnMut(SyntaxKind) -> bool) -> Option<SyntaxNode> {
        match &mut self.0 {
            ChildrenWhere::Flat { root, next, end, .. } => {
                let root = *root;
                let tree = &flat(&root).tree;
                while *next < *end {
                    let it = *next;
                    *next = tree.end(it);
                    if !tree.is_token(it) && pred(tree.kind(it)) {
                        return Some(SyntaxNode(flat_ref(root, it)));
                    }
                }
                None
            }
            ChildrenWhere::Green(children) => children.find(|it| pred(it.kind())),
        }
    }
}

#[derive(Clone, Debug)]
pub struct SyntaxElementChildren {
    next: Option<SyntaxElement>,
}

impl SyntaxElementChildren {
    fn new(parent: SyntaxNode) -> SyntaxElementChildren {
        SyntaxElementChildren { next: parent.first_child_or_token() }
    }
}

impl Iterator for SyntaxElementChildren {
    type Item = SyntaxElement;
    fn next(&mut self) -> Option<SyntaxElement> {
        self.next.take().map(|next| {
            self.next = next.next_sibling_or_token();
            next
        })
    }
}

#[derive(Debug, Clone)]
pub struct Preorder {
    start: SyntaxNode,
    next: Option<WalkEvent<SyntaxNode>>,
    skip_subtree: bool,
}

impl Preorder {
    fn new(start: SyntaxNode) -> Preorder {
        let next = Some(WalkEvent::Enter(start.clone()));
        Preorder { start, next, skip_subtree: false }
    }

    pub fn skip_subtree(&mut self) {
        self.skip_subtree = true;
    }

    #[cold]
    fn do_skip(&mut self) {
        self.next = self.next.take().map(|next| match next {
            WalkEvent::Enter(first_child) => WalkEvent::Leave(first_child.parent().unwrap()),
            WalkEvent::Leave(parent) => WalkEvent::Leave(parent),
        })
    }
}

impl Iterator for Preorder {
    type Item = WalkEvent<SyntaxNode>;

    fn next(&mut self) -> Option<WalkEvent<SyntaxNode>> {
        if self.skip_subtree {
            self.do_skip();
            self.skip_subtree = false;
        }
        let next = self.next.take();
        self.next = next.as_ref().and_then(|next| {
            Some(match next {
                WalkEvent::Enter(node) => match node.first_child() {
                    Some(child) => WalkEvent::Enter(child),
                    None => WalkEvent::Leave(node.clone()),
                },
                WalkEvent::Leave(node) => {
                    if node == &self.start {
                        return None;
                    }
                    match node.next_sibling() {
                        Some(sibling) => WalkEvent::Enter(sibling),
                        None => WalkEvent::Leave(node.parent()?),
                    }
                }
            })
        });
        next
    }
}

#[derive(Debug, Clone)]
pub struct PreorderWithTokens {
    start: SyntaxElement,
    next: Option<WalkEvent<SyntaxElement>>,
    skip_subtree: bool,
}

impl PreorderWithTokens {
    fn new(start: SyntaxNode) -> PreorderWithTokens {
        let next = Some(WalkEvent::Enter(start.clone().into()));
        PreorderWithTokens { start: start.into(), next, skip_subtree: false }
    }

    pub fn skip_subtree(&mut self) {
        self.skip_subtree = true;
    }

    #[cold]
    fn do_skip(&mut self) {
        self.next = self.next.take().map(|next| match next {
            WalkEvent::Enter(first_child) => WalkEvent::Leave(first_child.parent().unwrap().into()),
            WalkEvent::Leave(parent) => WalkEvent::Leave(parent),
        })
    }
}

impl Iterator for PreorderWithTokens {
    type Item = WalkEvent<SyntaxElement>;

    fn next(&mut self) -> Option<WalkEvent<SyntaxElement>> {
        if self.skip_subtree {
            self.do_skip();
            self.skip_subtree = false;
        }
        let next = self.next.take();
        self.next = next.as_ref().and_then(|next| {
            Some(match next {
                WalkEvent::Enter(el) => match el {
                    NodeOrToken::Node(node) => match node.first_child_or_token() {
                        Some(child) => WalkEvent::Enter(child),
                        None => WalkEvent::Leave(node.clone().into()),
                    },
                    NodeOrToken::Token(token) => WalkEvent::Leave(token.clone().into()),
                },
                WalkEvent::Leave(el) if el == &self.start => return None,
                WalkEvent::Leave(el) => match el.next_sibling_or_token() {
                    Some(sibling) => WalkEvent::Enter(sibling),
                    None => WalkEvent::Leave(el.parent()?.into()),
                },
            })
        });
        next
    }
}
// endregion
