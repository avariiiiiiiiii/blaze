//! Items named by their path in one file, as [`blaze_api::Target`] names them: names joined by
//! `::` (`m::S`, `E::V`, `S::field`, `S::0` for a tuple field), an impl as `<Type>` or
//! `<Type as Trait>` (whitespace ignored) with `@n` for the `n`th of several alike in the file,
//! and a step that would be ambiguous led by its keyword (`fn S`, `mod m`).
//!
//! The steps after an impl look in every impl that matches it: a type's methods may be spread
//! over several `impl S` blocks.

use syntax::{AstNode, SyntaxKind, SyntaxNode, ast};

/// What one step of a path names.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Fn,
    Struct,
    Enum,
    Union,
    Trait,
    Mod,
    Const,
    Static,
    Type,
    Macro,
    Impl,
    Variant,
    Field,
}

impl Kind {
    const ALL: [Kind; 13] = [
        Kind::Fn,
        Kind::Struct,
        Kind::Enum,
        Kind::Union,
        Kind::Trait,
        Kind::Mod,
        Kind::Const,
        Kind::Static,
        Kind::Type,
        Kind::Macro,
        Kind::Impl,
        Kind::Variant,
        Kind::Field,
    ];

    pub fn keyword(self) -> &'static str {
        match self {
            Kind::Fn => "fn",
            Kind::Struct => "struct",
            Kind::Enum => "enum",
            Kind::Union => "union",
            Kind::Trait => "trait",
            Kind::Mod => "mod",
            Kind::Const => "const",
            Kind::Static => "static",
            Kind::Type => "type",
            Kind::Macro => "macro",
            Kind::Impl => "impl",
            Kind::Variant => "variant",
            Kind::Field => "field",
        }
    }

    /// The kind of a node a path can name, `None` for any other.
    pub fn of(kind: SyntaxKind) -> Option<Kind> {
        Some(match kind {
            SyntaxKind::FN => Kind::Fn,
            SyntaxKind::STRUCT => Kind::Struct,
            SyntaxKind::ENUM => Kind::Enum,
            SyntaxKind::UNION => Kind::Union,
            SyntaxKind::TRAIT => Kind::Trait,
            SyntaxKind::MODULE => Kind::Mod,
            SyntaxKind::CONST => Kind::Const,
            SyntaxKind::STATIC => Kind::Static,
            SyntaxKind::TYPE_ALIAS => Kind::Type,
            SyntaxKind::MACRO_RULES | SyntaxKind::MACRO_DEF => Kind::Macro,
            SyntaxKind::IMPL => Kind::Impl,
            SyntaxKind::VARIANT => Kind::Variant,
            SyntaxKind::RECORD_FIELD | SyntaxKind::TUPLE_FIELD => Kind::Field,
            _ => return None,
        })
    }

    fn by_keyword(keyword: &str) -> Option<Kind> {
        Kind::ALL.into_iter().find(|kind| kind.keyword() == keyword)
    }
}

/// One step of a path.
#[derive(Clone, PartialEq, Eq, Debug)]
enum Step<'a> {
    Named { kind: Option<Kind>, name: &'a str },
    /// The impls whose type, and trait if any, read `ty` and `of` with whitespace removed; the
    /// `nth` of them when given.
    Impl { ty: String, of: Option<String>, nth: Option<usize> },
}

/// `path` cut at each `::` outside angle brackets (an `->`'s `>` closes none).
fn split(path: &str) -> Result<Vec<&str>, String> {
    let bytes = path.as_bytes();
    let mut parts = Vec::with_capacity(4);
    let mut depth = 0u32;
    let mut start = 0;
    let mut at = 0;
    while at < bytes.len() {
        match bytes[at] {
            b'<' => depth += 1,
            b'>' if at > 0 && bytes[at - 1] == b'-' => {}
            b'>' => {
                depth = depth
                    .checked_sub(1)
                    .ok_or_else(|| format!("`{path}` closes an angle bracket it never opened"))?;
            }
            b':' if depth == 0 && bytes.get(at + 1) == Some(&b':') => {
                parts.push(&path[start..at]);
                at += 2;
                start = at;
                continue;
            }
            _ => {}
        }
        at += 1;
    }
    if depth != 0 {
        return Err(format!("`{path}` leaves an angle bracket open"));
    }
    parts.push(&path[start..]);
    Ok(parts)
}

fn squeeze(text: &str) -> String {
    text.chars().filter(|it| !it.is_whitespace()).collect()
}

fn strip_raw(name: &str) -> &str {
    name.strip_prefix("r#").unwrap_or(name)
}

fn step(part: &str, path: &str) -> Result<Step<'_>, String> {
    let part = part.trim();
    if part.is_empty() {
        return Err(format!("`{path}` has an empty step"));
    }
    if part.starts_with('<') {
        let (body, nth) = match part.rsplit_once(">@") {
            Some((body, nth)) => {
                let nth = nth
                    .parse::<usize>()
                    .map_err(|_| format!("`{part}` in `{path}`: `@` takes a number"))?;
                (&body[1..], Some(nth))
            }
            None => {
                let body = part
                    .strip_suffix('>')
                    .ok_or_else(|| format!("`{part}` in `{path}` opens an impl it never closes"))?;
                (&body[1..], None)
            }
        };
        let (ty, of) = match top_level_as(body) {
            Some(at) => (&body[..at], Some(squeeze(&body[at + 4..]))),
            None => (body, None),
        };
        let ty = squeeze(ty);
        if ty.is_empty() || of.as_deref() == Some("") {
            return Err(format!("`{part}` in `{path}` names no type"));
        }
        return Ok(Step::Impl { ty, of, nth });
    }
    match part.split_once(char::is_whitespace) {
        Some((keyword, name)) => {
            let kind = Kind::by_keyword(keyword).ok_or_else(|| {
                format!("`{part}` in `{path}`: `{keyword}` is no kind of item (fn, struct, mod, …)")
            })?;
            Ok(Step::Named { kind: Some(kind), name: name.trim() })
        }
        None => Ok(Step::Named { kind: None, name: part }),
    }
}

/// Where ` as ` splits `<Type as Trait>`'s inside, outside any nested angle brackets.
fn top_level_as(body: &str) -> Option<usize> {
    let bytes = body.as_bytes();
    let mut depth = 0u32;
    for at in 0..bytes.len() {
        match bytes[at] {
            b'<' => depth += 1,
            b'>' if at > 0 && bytes[at - 1] == b'-' => {}
            b'>' => depth = depth.saturating_sub(1),
            b' ' if depth == 0 && body[at..].starts_with(" as ") => return Some(at),
            _ => {}
        }
    }
    None
}

/// The nodes a container holds that a path can name: a file's or module's items (and those of
/// its extern blocks), an impl's or trait's, a struct's, union's or variant's fields, an enum's
/// variants.
pub fn members(container: &SyntaxNode) -> Vec<SyntaxNode> {
    let mut out = Vec::new();
    let list = match container.kind() {
        SyntaxKind::SOURCE_FILE => Some(container.clone()),
        SyntaxKind::MODULE => container.child_where(|it| it == SyntaxKind::ITEM_LIST),
        SyntaxKind::IMPL | SyntaxKind::TRAIT => {
            container.child_where(|it| it == SyntaxKind::ASSOC_ITEM_LIST)
        }
        SyntaxKind::STRUCT | SyntaxKind::UNION | SyntaxKind::VARIANT => container.child_where(|it| {
            it == SyntaxKind::RECORD_FIELD_LIST || it == SyntaxKind::TUPLE_FIELD_LIST
        }),
        SyntaxKind::ENUM => container.child_where(|it| it == SyntaxKind::VARIANT_LIST),
        _ => None,
    };
    let Some(list) = list else { return out };
    for child in list.children() {
        if child.kind() == SyntaxKind::EXTERN_BLOCK {
            if let Some(items) = child.child_where(|it| it == SyntaxKind::EXTERN_ITEM_LIST) {
                out.extend(items.children().filter(|it| Kind::of(it.kind()).is_some()));
            }
        } else if Kind::of(child.kind()).is_some() {
            out.push(child);
        }
    }
    out
}

/// The name a member goes by: its name, or a tuple field's index.
pub fn name_of(member: &SyntaxNode) -> Option<String> {
    if member.kind() == SyntaxKind::TUPLE_FIELD {
        let list = member.parent()?;
        let at = list
            .children()
            .filter(|it| it.kind() == SyntaxKind::TUPLE_FIELD)
            .position(|it| it == *member)?;
        return Some(at.to_string());
    }
    let name = member.child_where(|it| it == SyntaxKind::NAME)?;
    Some(ast::Name::cast(name)?.text().to_owned())
}

/// An impl's type and trait, whitespace removed.
fn impl_of(member: &SyntaxNode) -> Option<(String, Option<String>)> {
    let imp = ast::Impl::cast(member.clone())?;
    let ty = squeeze(&imp.self_ty()?.syntax().text().to_string());
    let of = imp.trait_().map(|it| squeeze(&it.syntax().text().to_string()));
    Some((ty, of))
}

fn matches(member: &SyntaxNode, step: &Step<'_>) -> bool {
    match step {
        Step::Named { kind, name } => {
            kind.is_none_or(|kind| Kind::of(member.kind()) == Some(kind))
                && member.kind() != SyntaxKind::IMPL
                && name_of(member).is_some_and(|it| strip_raw(&it) == strip_raw(name))
        }
        Step::Impl { ty, of, .. } => {
            impl_of(member).is_some_and(|(its_ty, its_of)| its_ty == *ty && its_of == *of)
        }
    }
}

fn describe(node: &SyntaxNode) -> String {
    let kind = Kind::of(node.kind()).map_or("item", Kind::keyword);
    format!("{kind} at byte {}", u32::from(node.text_range().start()))
}

/// The one node `path` names in the file whose tree is `root`.
pub fn find(root: &SyntaxNode, path: &str) -> Result<SyntaxNode, String> {
    let mut frontier = vec![root.clone()];
    let parts = split(path)?;
    for (at, part) in parts.iter().enumerate() {
        let step = step(part, path)?;
        let mut found: Vec<SyntaxNode> = frontier
            .iter()
            .flat_map(members)
            .filter(|member| matches(member, &step))
            .collect();
        if let Step::Impl { nth: Some(nth), .. } = step {
            let count = found.len();
            found = vec![found.into_iter().nth(nth).ok_or_else(|| {
                format!("`{path}` asks for impl {nth} of `{part}`, and the file has {count}")
            })?];
        }
        let last = at + 1 == parts.len();
        match (found.len(), &step) {
            (0, _) => {
                let within = parts[..at].join("::");
                let within = if within.is_empty() { "the file".to_owned() } else { format!("`{within}`") };
                return Err(format!("no `{part}` in {within}"));
            }
            (1, _) => {}
            (_, Step::Impl { .. }) if !last => {}
            _ => {
                let all: Vec<String> = found.iter().map(describe).collect();
                return Err(format!(
                    "`{}` names {} items: {}; lead the step with its kind (`fn {part}`) or, for \
                     an impl, add `@n`",
                    parts[..=at].join("::"),
                    found.len(),
                    all.join(", ")
                ));
            }
        }
        frontier = found;
    }
    Ok(frontier.into_iter().next().expect("a path's last step found one node"))
}

/// The path [`find`] takes back to `node`, a member of a chain of containers up to the file.
pub fn of(node: &SyntaxNode) -> String {
    let mut steps: Vec<String> = Vec::new();
    let mut at = Some(node.clone());
    while let Some(node) = at {
        let parent = container_of(&node);
        if Kind::of(node.kind()).is_some() {
            let siblings = parent.as_ref().map(members).unwrap_or_default();
            steps.push(step_of(&node, &siblings));
        }
        at = parent;
    }
    steps.reverse();
    steps.join("::")
}

/// The node whose [`members`] `node` is one of.
fn container_of(node: &SyntaxNode) -> Option<SyntaxNode> {
    node.ancestors().skip(1).find(|it| {
        matches!(
            it.kind(),
            SyntaxKind::SOURCE_FILE
                | SyntaxKind::MODULE
                | SyntaxKind::IMPL
                | SyntaxKind::TRAIT
                | SyntaxKind::STRUCT
                | SyntaxKind::UNION
                | SyntaxKind::VARIANT
                | SyntaxKind::ENUM
        )
    })
}

fn step_of(node: &SyntaxNode, siblings: &[SyntaxNode]) -> String {
    if let Some((ty, of)) = impl_of(node) {
        let text = match &of {
            Some(of) => format!("<{ty} as {of}>"),
            None => format!("<{ty}>"),
        };
        let alike: Vec<&SyntaxNode> = siblings
            .iter()
            .filter(|it| impl_of(it).is_some_and(|(t, o)| t == ty && o == of))
            .collect();
        if alike.len() > 1 {
            let nth = alike.iter().position(|it| *it == node).expect("an impl is its own sibling");
            return format!("{text}@{nth}");
        }
        return text;
    }
    let name = name_of(node).unwrap_or_default();
    let same = siblings
        .iter()
        .filter(|it| it.kind() != SyntaxKind::IMPL && name_of(it).as_deref() == Some(&*name))
        .count();
    match Kind::of(node.kind()) {
        Some(kind) if same > 1 => format!("{} {name}", kind.keyword()),
        _ => name,
    }
}

/// Whether `node` is a top item: one a path names directly under a file, module, impl or trait,
/// not one inside a body.
pub fn is_top(node: &SyntaxNode) -> bool {
    Kind::of(node.kind()).is_some()
        && node.parent().is_some_and(|parent| {
            matches!(
                parent.kind(),
                SyntaxKind::SOURCE_FILE
                    | SyntaxKind::ITEM_LIST
                    | SyntaxKind::ASSOC_ITEM_LIST
                    | SyntaxKind::EXTERN_ITEM_LIST
            )
        })
}

/// The top item `node` is in: itself when it is one.
pub fn top_of(node: &SyntaxNode) -> Option<SyntaxNode> {
    node.ancestors().find(is_top)
}

/// The module, impl or trait a top item is in, `None` for one directly in the file.
pub fn scope_of(item: &SyntaxNode) -> Option<SyntaxNode> {
    item.ancestors().skip(1).find(|it| {
        matches!(it.kind(), SyntaxKind::MODULE | SyntaxKind::IMPL | SyntaxKind::TRAIT)
    })
}

#[cfg(test)]
mod tests {
    use syntax::{Edition, SourceFile};

    use super::*;

    const FILE: &str = "
mod m {
    pub struct S { pub x: u32, y: (u8, u16) }
    pub struct T(u8, u16);
    impl S { pub fn new() -> S { make() } }
    impl S { fn other(&self) {} }
    impl std::fmt::Display for S { fn fmt(&self) {} }
    pub enum E { A, B { z: u8 } }
}
struct S;
fn S() {}
impl<T> From<Vec<T>> for m::T { fn from(_: Vec<T>) -> m::T { make() } }
extern \"C\" { fn ext(); }
";

    fn root() -> SyntaxNode {
        SourceFile::parse(FILE, Edition::CURRENT).syntax_node()
    }

    #[track_caller]
    fn round_trip(path: &str, kind: Kind) {
        let node = find(&root(), path).unwrap_or_else(|error| panic!("{path}: {error}"));
        assert_eq!(Kind::of(node.kind()), Some(kind), "{path}");
        assert_eq!(of(&node), path);
    }

    #[test]
    fn every_kind_of_step_finds_its_node_and_reads_back_the_same() {
        round_trip("m::S", Kind::Struct);
        round_trip("m::S::x", Kind::Field);
        round_trip("m::S::y", Kind::Field);
        round_trip("m::T::1", Kind::Field);
        round_trip("m::<S>@0::new", Kind::Fn);
        round_trip("m::<S>@1::other", Kind::Fn);
        round_trip("m::<S as std::fmt::Display>::fmt", Kind::Fn);
        round_trip("m::E::B::z", Kind::Field);
        round_trip("struct S", Kind::Struct);
        round_trip("fn S", Kind::Fn);
        round_trip("<m::T as From<Vec<T>>>::from", Kind::Fn);
        round_trip("ext", Kind::Fn);
    }

    #[test]
    fn steps_after_an_impl_look_in_every_impl_alike() {
        let node = find(&root(), "m::<S>::other").unwrap();
        assert_eq!(of(&node), "m::<S>@1::other");
        let spaced = find(&root(), "< m::T  as From< Vec<T> > >::from").unwrap();
        assert_eq!(of(&spaced), "<m::T as From<Vec<T>>>::from");
    }

    #[test]
    fn an_ambiguous_or_missing_step_is_an_error_naming_it() {
        let error = find(&root(), "S").unwrap_err();
        assert!(error.contains("`S` names 2 items"), "{error}");
        let error = find(&root(), "m::<S>").unwrap_err();
        assert!(error.contains("names 2 items"), "{error}");
        let error = find(&root(), "m::nope").unwrap_err();
        assert_eq!(error, "no `nope` in `m`");
        let error = find(&root(), "m::<S>@5::new").unwrap_err();
        assert!(error.contains("asks for impl 5"), "{error}");
        assert!(find(&root(), "m::<S").is_err());
        assert!(find(&root(), "m::").is_err());
        assert!(find(&root(), "bogus S").is_err());
    }

    #[test]
    fn top_items_and_their_scopes() {
        let new = find(&root(), "m::<S>::new").unwrap();
        assert!(is_top(&new));
        assert_eq!(scope_of(&new).map(|it| it.kind()), Some(SyntaxKind::IMPL));
        let field = find(&root(), "m::S::x").unwrap();
        assert!(!is_top(&field));
        assert_eq!(top_of(&field).map(|it| of(&it)).as_deref(), Some("m::S"));
    }
}
