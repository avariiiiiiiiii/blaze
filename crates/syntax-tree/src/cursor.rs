//! The cursors: [`SyntaxNode`] and [`SyntaxToken`], an element of a [`FlatTree`] each.
//!
//! A cursor is the tree's root and an element's number, and moving is arithmetic on the
//! tree's arrays, with nothing allocated. Trees are immutable: an edit makes a new tree
//! ([`SyntaxNode::splice_children`], [`FlatTree::replace_subtree`]).
//!
//! A flat tree's lazy nodes are transparent: moving down into one moves into its part
//! (parsed then, once), whose root stands for the lazy node itself, and moving up or
//! sideways out of a part's children moves back into the file's tree.

// Implementation notes:
//
// A `FlatRoot` is counted by every cursor into its tree, and owns the tree. Cursors are not
// `Send`: the count is a `Cell`. Two roots of one tree have equal cursors: a cursor's identity
// is its tree's address and its number.
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
    ops::Range,
    ptr,
    sync::Arc,
};

use rustc_hash::FxHashMap;

use crate::{
    flat::FlatTree, Direction, FlatTreeBuilder, NodeOrToken, SyntaxKind, SyntaxText, TextRange,
    TextSize, TokenAtOffset, WalkEvent,
};

struct FlatRoot {
    /// The cursors into this tree and its parts; unused in a part's root.
    rc: Cell<u32>,
    tree: Arc<FlatTree>,
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

/// What a cursor points at: element `at` of the tree of `root`.
struct Ref {
    root: ptr::NonNull<FlatRoot>,
    at: u32,
}

impl Clone for Ref {
    #[inline]
    fn clone(&self) -> Self {
        flat(&owner(self.root)).inc_rc();
        Ref { root: self.root, at: self.at }
    }
}

impl Drop for Ref {
    #[inline]
    fn drop(&mut self) {
        let owner = owner(self.root);
        if flat(&owner).dec_rc() {
            // SAFETY: a file's root is made by `new_flat_root` from a box, and this was the
            // last cursor counting it.
            drop(unsafe { Box::from_raw(owner.as_ptr()) })
        }
    }
}

#[inline]
fn flat(root: &ptr::NonNull<FlatRoot>) -> &FlatRoot {
    // SAFETY: a root lives while a cursor counts it (its own, or its file's for a part's),
    // and the reference is borrowed from that cursor's `Ref`.
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
            file: Some((this, at)),
            parts: RefCell::default(),
        };
        // SAFETY: `Box::into_raw` is never null.
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
}

/// A cursor at element `at` of the flat tree `root`: at the lazy node for a part's root.
#[inline]
fn flat_ref(root: ptr::NonNull<FlatRoot>, at: u32) -> Ref {
    let (root, at) = match flat(&root).file {
        Some(lazy) if at == 0 => lazy,
        _ => (root, at),
    };
    flat(&owner(root)).inc_rc();
    Ref { root, at }
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
/// # Safety
///
/// The caller holds a cursor into `root`'s file, which keeps `root` alive (a part's root lives
/// as long as its file's), for as long as `'a`.
#[inline]
unsafe fn flat_unbound<'a>(root: ptr::NonNull<FlatRoot>) -> &'a FlatRoot {
    // SAFETY: the caller's precondition keeps `root` alive for `'a`: a `FlatRoot` made from a
    // box by `new_flat_root` or `part_root`, freed only when the last cursor into its file
    // drops, and only ever read through shared references.
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

/// Gives `builder` `element` and its subtree, the parts of its lazy nodes in it.
pub(crate) fn copy_element(builder: &mut FlatTreeBuilder, element: &SyntaxElement) {
    match element {
        NodeOrToken::Token(token) => builder.token(token.kind(), token.text()),
        NodeOrToken::Node(node) => {
            for event in node.preorder_with_tokens() {
                match event {
                    WalkEvent::Enter(NodeOrToken::Node(it)) => builder.start_node(it.kind()),
                    WalkEvent::Enter(NodeOrToken::Token(it)) => builder.token(it.kind(), it.text()),
                    WalkEvent::Leave(NodeOrToken::Node(_)) => builder.finish_node(),
                    WalkEvent::Leave(NodeOrToken::Token(_)) => {}
                }
            }
        }
    }
}

pub type SyntaxElement = NodeOrToken<SyntaxNode, SyntaxToken>;

#[derive(Clone)]
pub struct SyntaxNode(Ref);

#[derive(Clone)]
pub struct SyntaxToken(Ref);

impl Ref {
    #[inline]
    fn tree(&self) -> &FlatTree {
        &flat(&self.root).tree
    }

    #[inline]
    fn kind(&self) -> SyntaxKind {
        self.tree().kind(self.at)
    }

    #[inline]
    fn text_range(&self) -> TextRange {
        self.tree().range(self.at)
    }

    #[inline]
    fn index(&self) -> usize {
        self.tree().index(self.at)
    }

    #[inline]
    fn parent(&self) -> Option<SyntaxNode> {
        up(self.root, self.at).map(|(root, it)| SyntaxNode(flat_ref(root, it)))
    }

    #[inline]
    fn next_sibling_or_token(&self) -> Option<SyntaxElement> {
        self.tree().next_sibling_or_token(self.at).map(|it| flat_element(self.root, it))
    }

    #[inline]
    fn prev_sibling_or_token(&self) -> Option<SyntaxElement> {
        self.tree().prev_sibling_or_token(self.at).map(|it| flat_element(self.root, it))
    }

    #[inline]
    fn key(&self) -> (usize, u32) {
        (Arc::as_ptr(&flat(&self.root).tree) as usize, self.at)
    }
}

impl SyntaxNode {
    /// The root of a flat tree.
    pub fn new_flat_root(tree: Arc<FlatTree>) -> SyntaxNode {
        assert!(!tree.is_empty() && !tree.is_token(0), "a tree's root is a node");
        let root = FlatRoot { rc: Cell::new(1), tree, file: None, parts: RefCell::default() };
        // SAFETY: `Box::into_raw` is never null.
        let root = unsafe { ptr::NonNull::new_unchecked(Box::into_raw(Box::new(root))) };
        SyntaxNode(Ref { root, at: 0 })
    }

    /// Whether this is a lazy node whose part is not parsed yet. Never parses.
    #[inline]
    pub fn is_lazy(&self) -> bool {
        let tree = self.0.tree();
        tree.is_lazy(self.0.at) && !tree.is_parsed(self.0.at)
    }

    /// The flat tree this node is in: a lazy node's part for a node inside it. Always `Some`.
    pub fn flat_tree(&self) -> Option<&Arc<FlatTree>> {
        Some(&flat(&self.0.root).tree)
    }

    /// The flat tree this node is in and the node's number in it: a lazy node's part for a
    /// node inside it, the file's tree for the lazy node itself. Always `Some`.
    #[inline]
    pub fn flat(&self) -> Option<(&Arc<FlatTree>, u32)> {
        Some((&flat(&self.0.root).tree, self.0.at))
    }

    /// Element `at` of the flat tree this node is in.
    #[inline]
    pub fn flat_element(&self, at: u32) -> SyntaxElement {
        flat_element(self.0.root, at)
    }

    /// This node, for a walk that reads the arrays: every node is in a flat tree.
    #[inline]
    pub fn to_flat(&self) -> SyntaxNode {
        self.clone()
    }

    /// The first child node whose kind `pred` accepts.
    #[inline]
    pub fn child_where(&self, mut pred: impl FnMut(SyntaxKind) -> bool) -> Option<SyntaxNode> {
        let (root, at) = inner(self.0.root, self.0.at);
        let tree = &flat(&root).tree;
        let child = tree.children(at).find(|&it| pred(tree.kind(it)))?;
        Some(SyntaxNode(flat_ref(root, child)))
    }

    /// The nearest of this node and its ancestors whose kind `pred` accepts.
    #[inline]
    pub fn ancestor_where(&self, mut pred: impl FnMut(SyntaxKind) -> bool) -> Option<SyntaxNode> {
        let (mut root, mut it) = (self.0.root, self.0.at);
        while !pred(flat(&root).tree.kind(it)) {
            (root, it) = up(root, it)?;
        }
        Some(SyntaxNode(flat_ref(root, it)))
    }

    /// The first child token of `kind`.
    #[inline]
    pub fn child_token(&self, kind: SyntaxKind) -> Option<SyntaxToken> {
        let (root, at) = inner(self.0.root, self.0.at);
        let tree = &flat(&root).tree;
        let token =
            tree.children_with_tokens(at).find(|&it| tree.is_token(it) && tree.kind(it) == kind)?;
        Some(SyntaxToken(flat_ref(root, token)))
    }

    /// The child nodes, each step taking the next whose kind a predicate accepts.
    #[inline]
    pub fn children_where(&self) -> SyntaxNodeChildrenWhere {
        let (root, at) = inner(self.0.root, self.0.at);
        let end = flat(&root).tree.end(at);
        SyntaxNodeChildrenWhere { _parent: self.clone(), root, next: at + 1, end }
    }

    /// An independent copy of the subtree rooted at this node: a tree of its own whose text
    /// starts at 0, its lazy nodes lazy.
    pub fn clone_subtree(&self) -> SyntaxNode {
        let (root, at) = (self.0.root, self.0.at);
        let tree = &flat(&root).tree;
        let whole = flat(&root).file.is_none() && tree.range(0).start() == TextSize::from(0);
        match at == 0 && whole {
            true => SyntaxNode(flat_ref(root, 0)),
            false => SyntaxNode::new_flat_root(Arc::new(tree.subtree(at))),
        }
    }

    /// The root of a new tree, the one this node is in but with this node's children (and
    /// tokens) at `range` replaced by copies of `with`. Only the text and structure carry
    /// over: the new tree's text is its own, its lazy nodes parsed into it, so its ranges
    /// start at 0 whatever this tree's did.
    pub fn splice_children(&self, range: Range<usize>, with: &[SyntaxElement]) -> SyntaxNode {
        let top = self.tree_top();
        let mut builder = FlatTreeBuilder::with_capacity(usize::from(top.text_range().len()));
        let mut walk = top.preorder_with_tokens();
        while let Some(event) = walk.next() {
            match event {
                WalkEvent::Enter(NodeOrToken::Node(node)) if node == *self => {
                    builder.start_node(node.kind());
                    let children: Vec<SyntaxElement> = node.children_with_tokens().collect();
                    assert!(
                        range.start <= range.end && range.end <= children.len(),
                        "children {range:?} of a {:?} of {} children",
                        node.kind(),
                        children.len(),
                    );
                    let (before, after) = (&children[..range.start], &children[range.end..]);
                    for it in before.iter().chain(with).chain(after) {
                        copy_element(&mut builder, it);
                    }
                    // The walk's next event is now leaving `node`, which finishes it.
                    walk.skip_subtree();
                }
                WalkEvent::Enter(NodeOrToken::Node(it)) => builder.start_node(it.kind()),
                WalkEvent::Enter(NodeOrToken::Token(it)) => builder.token(it.kind(), it.text()),
                WalkEvent::Leave(NodeOrToken::Node(_)) => builder.finish_node(),
                WalkEvent::Leave(NodeOrToken::Token(_)) => {}
            }
        }
        SyntaxNode::new_flat_root(Arc::new(builder.finish()))
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
    pub fn text(&self) -> SyntaxText {
        SyntaxText::new(self.clone())
    }

    /// The text of the node's first child, when that is a token.
    #[inline]
    pub fn first_child_token_text(&self) -> Option<&str> {
        let (root, at) = inner(self.0.root, self.0.at);
        // SAFETY: `self` is a cursor into the file `root` is the tree or a part of.
        let tree = &unsafe { flat_unbound(root) }.tree;
        let first = tree.first_child_or_token(at).filter(|&it| tree.is_token(it))?;
        Some(tree.token_text(first))
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
        SyntaxNode(flat_ref(owner(self.0.root), 0))
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
        let (root, at) = inner(self.0.root, self.0.at);
        flat(&root).tree.first_child(at).map(|it| SyntaxNode(flat_ref(root, it)))
    }

    pub fn last_child(&self) -> Option<SyntaxNode> {
        let (root, at) = inner(self.0.root, self.0.at);
        let tree = &flat(&root).tree;
        let mut child = tree.last_child_or_token(at);
        while let Some(it) = child {
            if !tree.is_token(it) {
                return Some(SyntaxNode(flat_ref(root, it)));
            }
            child = tree.prev_sibling_or_token(it);
        }
        None
    }

    pub fn first_child_or_token(&self) -> Option<SyntaxElement> {
        let (root, at) = inner(self.0.root, self.0.at);
        flat(&root).tree.first_child_or_token(at).map(|it| flat_element(root, it))
    }

    pub fn last_child_or_token(&self) -> Option<SyntaxElement> {
        let (root, at) = inner(self.0.root, self.0.at);
        flat(&root).tree.last_child_or_token(at).map(|it| flat_element(root, it))
    }

    pub fn next_sibling(&self) -> Option<SyntaxNode> {
        let root = self.0.root;
        flat(&root).tree.next_sibling(self.0.at).map(|it| SyntaxNode(flat_ref(root, it)))
    }

    pub fn prev_sibling(&self) -> Option<SyntaxNode> {
        let root = self.0.root;
        let tree = &flat(&root).tree;
        let mut sibling = tree.prev_sibling_or_token(self.0.at);
        while let Some(it) = sibling {
            if !tree.is_token(it) {
                return Some(SyntaxNode(flat_ref(root, it)));
            }
            sibling = tree.prev_sibling_or_token(it);
        }
        None
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
    /// and `range`, when the range is not empty; `None` when it is, for the caller to step.
    pub(crate) fn flat_node_at_range(
        &self,
        range: TextRange,
        kind: SyntaxKind,
    ) -> Option<Option<SyntaxNode>> {
        (!range.is_empty()).then(|| flat_node_at_range(self.0.root, self.0.at, range, kind))
    }

    pub fn child_or_token_at_range(&self, range: TextRange) -> Option<SyntaxElement> {
        let (root, at) = inner(self.0.root, self.0.at);
        flat(&root).tree.child_at_range(at, range).map(|it| flat_element(root, it))
    }
}

impl SyntaxToken {
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
        self.0.tree().token_text(self.0.at)
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
        SyntaxNode(flat_ref(owner(self.0.root), 0))
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
/// [`SyntaxNode::children_where`], read off the arrays: only those taken are made cursors.
#[derive(Clone, Debug)]
pub struct SyntaxNodeChildrenWhere {
    /// The node whose children these are (in its part when it is a lazy node), keeping
    /// `root` alive.
    _parent: SyntaxNode,
    root: ptr::NonNull<FlatRoot>,
    next: u32,
    end: u32,
}

impl SyntaxNodeChildrenWhere {
    /// The next child whose kind `pred` accepts.
    #[inline]
    pub fn next_where(&mut self, mut pred: impl FnMut(SyntaxKind) -> bool) -> Option<SyntaxNode> {
        let root = self.root;
        let tree = &flat(&root).tree;
        while self.next < self.end {
            let it = self.next;
            self.next = tree.end(it);
            if !tree.is_token(it) && pred(tree.kind(it)) {
                return Some(SyntaxNode(flat_ref(root, it)));
            }
        }
        None
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
