//! The callers of a generic function, which rustc copied its code into: found from the bodies of
//! the crates that can see it, each expression of them that names it resolved by inference, as
//! rust-analyzer's search for its references finds them.
//!
//! The search reads what rust-analyzer's does first, in the same order: the text of each file
//! of those crates for the name as a word, then the text of each body in a file that has it.
//! Only a body that says the name is lowered, and only one with an expression of that name is
//! inferred and has its source map read.

#![forbid(unsafe_code)]

use base_db::{Db, FileId};
use hir::{AsAssocItem as _, HirFileId, InFile, Name};
use hir_def::{
    DefWithBodyId, FunctionId, HasModule as _, ItemContainerId, Lookup as _,
    expr_store::{Body, BodySourceMap},
    hir::{Expr, ExprId, ExprOrPatId},
    nameres::demand::{crate_body_owners, walk_bodies},
    resolver::ValueNs,
    signatures::FunctionSignature,
    src::HasSource as _,
};
use hir_ty::InferenceResult;
use rustc_hash::{FxHashMap, FxHashSet};
use syntax::{
    SyntaxKind,
    view::{self as ast, AstNode as _},
};

/// How many generic functions [`callers_of_generic`] finds the callers of, and about how many
/// callers it finds, which are compiled before the edit.
const CALLERS_SEARCHED: usize = 16;
const CALLERS_COMPILED: usize = 512;

/// The functions written elsewhere that call the generic functions among `functions`, or call
/// a generic one that does, as far as [`CALLERS_SEARCHED`] and [`CALLERS_COMPILED`] go: rustc
/// copied those into them. A method of a trait's impl is called through the trait's method as
/// well. A call outside a function's body is left out, as are the callers past the bound; the
/// copies they have are then ones no patch is known to reach.
pub(crate) fn callers_of_generic(db: &Db, functions: &[FunctionId]) -> Vec<FunctionId> {
    let of_trait = |func: FunctionId| matches!(func.lookup(db).container, ItemContainerId::TraitId(_));
    // alloc: once per edit of a file a program homes, sized by the functions written in it
    let mut seen: FxHashSet<FunctionId> = functions.iter().copied().collect();
    let mut queue: Vec<FunctionId> = functions
        .iter()
        .copied()
        .filter(|&it| !of_trait(it) && hir_ty_codegen::copied_by_rustc(db, it))
        // alloc: once per edit of a file a program homes
        .collect();
    // alloc: the callers, which the caller keeps; and one buffer reused for each search
    let (mut callers, mut found, mut searched) = (Vec::new(), Vec::new(), 0);
    while let Some(func) = queue.pop() {
        searched += 1;
        if searched > CALLERS_SEARCHED || callers.len() > CALLERS_COMPILED {
            break;
        }
        found.clear();
        direct_callers(db, func, &mut found);
        for &caller in &found {
            if !seen.insert(caller) {
                continue;
            }
            callers.push(caller);
            if hir_ty_codegen::copied_by_rustc(db, caller) {
                queue.push(caller);
            }
        }
    }
    callers
}

/// Pushes to `out` the function around each expression of a body that names `func`, once per
/// body: a path or a method call whose name is written in a file, or passed to a macro there,
/// and that resolves to `func`, or, when `func` is a trait's function or implements one, to
/// any function implementing the same function of the trait. That is what rust-analyzer's
/// search for the references of `func` and of the trait's function finds, each reference's
/// function being the innermost `fn` around it. A body in no function (a `const` or `static`
/// outside one, a discriminant) has no caller.
fn direct_callers(db: &Db, func: FunctionId, out: &mut Vec<FunctionId>) {
    let target = in_trait(db, func);
    let name = FunctionSignature::of(db, func).name.as_str();
    // alloc: once per search, by the files of the crates that see the function
    let mut files: FxHashMap<FileId, bool> = FxHashMap::default();
    // alloc: once per search, by the bodies declared in the bodies that say the name
    let mut enclosing: FxHashMap<DefWithBodyId, Option<FunctionId>> = FxHashMap::default();
    for &krate in target.krate(db).transitive_rev_deps(db) {
        walk_bodies(
            db,
            crate_body_owners(db, krate),
            |owner| says(db, &mut files, owner, name),
            |owner, blocks| {
                let caller = match owner {
                    DefWithBodyId::FunctionId(it) => Some(it),
                    _ => enclosing.get(&owner).copied().flatten(),
                };
                enclosing.extend(blocks.owners.iter().map(|&it| (it, caller)));
                if let Some(caller) = caller
                    && names(db, owner, target, name)
                {
                    out.push(caller);
                }
            },
        );
    }
}

/// The function of the trait that `func` implements, or `func`: rust-analyzer's
/// `convert_to_def_in_trait`, by which a search for a trait's function finds the calls of each
/// function implementing it.
fn in_trait(db: &Db, func: FunctionId) -> FunctionId {
    let it = hir::Function::from(func);
    let of_trait = it.as_assoc_item(db).and_then(|item| item.implemented_trait(db));
    of_trait
        .and_then(|trait_| trait_.function(db, it.name(db)))
        .and_then(|it| FunctionId::try_from(it).ok())
        .unwrap_or(func)
}

/// Whether the text of the body of `owner` has `name` in it as a word, the text of its file
/// read first (and once, kept in `files`): no body of a file without the name is parsed.
fn says(db: &Db, files: &mut FxHashMap<FileId, bool>, owner: DefWithBodyId, name: &str) -> bool {
    let file = hir_ty_codegen::body_file(db, owner);
    let in_file = *files.entry(file).or_insert_with(|| has_word(db.file_text(file).text(db), name));
    in_file && body_text(db, owner).is_some_and(|text| has_word(text, name))
}

/// The text of the body of `owner` as it is written, or as a macro wrote it.
fn body_text(db: &Db, owner: DefWithBodyId) -> Option<&str> {
    let body = match owner {
        DefWithBodyId::FunctionId(it) => it.lookup(db).source_view(db).value.body()?.syntax(),
        DefWithBodyId::ConstId(it) => it.lookup(db).source_view(db).value.body()?.syntax(),
        DefWithBodyId::StaticId(it) => it.lookup(db).source_view(db).value.body()?.syntax(),
        DefWithBodyId::VariantId(it) => it.lookup(db).source_view(db).value.const_arg()?.syntax(),
    };
    Some(body.text())
}

/// Whether `text` has `name` in it, not as a part of a longer identifier: where
/// rust-analyzer's search keeps a match of the name's text.
fn has_word(text: &str, name: &str) -> bool {
    let bytes = text.as_bytes();
    memchr::memmem::find_iter(bytes, name.as_bytes()).any(|at| {
        let before = at.checked_sub(1).map(|it| bytes[it]);
        let after = bytes.get(at + name.len()).copied();
        !before.is_some_and(|it| it.is_ascii_alphabetic() || it == b'_')
            && !after.is_some_and(|it| it.is_ascii_alphanumeric() || it == b'_')
    })
}

/// Whether the body of `owner` has a path or a method call named `name` that resolves to
/// `target` (to a function implementing it, for a trait's function) and whose name is written
/// ([`written`]).
fn names(db: &Db, owner: DefWithBodyId, target: FunctionId, name: &str) -> bool {
    let named = |it: &Name| it.as_str() == name;
    let mut candidates = Body::of(db, owner)
        .exprs()
        .filter(|(_, expr)| match expr {
            Expr::Path(path) => path.mod_path().and_then(|it| it.segments().last()).is_some_and(named),
            Expr::MethodCall { method_name, .. } => named(*method_name),
            _ => false,
        })
        .map(|(id, _)| id)
        .peekable();
    if candidates.peek().is_none() {
        return false;
    }
    let infer = InferenceResult::of(db, owner);
    // An expression of the body is inferred with it, or with the one anonymous constant of it
    // (an array's length, a constant argument) it is in.
    let inferred = |expr: ExprId| {
        let anonymous = infer.defined_anon_consts().iter().map(|&it| InferenceResult::of(db, it));
        std::iter::once(infer).chain(anonymous).find_map(|it| resolution(it, expr))
    };
    // A source map is lowered again where it is asked for: once, for a body that names the
    // function.
    let mut source_map: Option<&BodySourceMap> = None;
    candidates.any(|expr| {
        inferred(expr).is_some_and(|it| it == target || in_trait(db, it) == target) && {
            let source_map = *source_map.get_or_insert_with(|| &Body::with_source_map(db, owner).1);
            written(db, source_map, expr, name)
        }
    })
}

/// The function a path expression or a method call resolves to.
fn resolution(infer: &InferenceResult<'_>, expr: ExprId) -> Option<FunctionId> {
    match infer.value_resolution(ExprOrPatId::ExprId(expr)) {
        Some(ValueNs::FunctionId(it)) => Some(it),
        Some(_) => None,
        None => infer.method_resolution(expr).map(|(it, _)| it),
    }
}

/// Whether the name of `expr`, a path's last segment or a method call's method, is a token
/// written in a file, or one passed to a macro there that the macro kept: what
/// rust-analyzer's search, which finds the name in the text of a file and follows it into the
/// macros it is passed to, reaches. A name a macro writes itself, or makes up (a derive's), is
/// not, nor is an expression nothing is written for (a desugaring).
fn written(db: &Db, source_map: &BodySourceMap, expr: ExprId, name: &str) -> bool {
    let Ok(source) = source_map.expr_syntax(expr) else { return false };
    let node = ast::to_node(source.value.syntax_node_ptr(), source.file_id.root(db));
    let name_ref = match node.kind() {
        SyntaxKind::PATH_EXPR => {
            ast::PathExpr::cast(node).and_then(|it| it.path()?.segment()?.name_ref())
        }
        SyntaxKind::METHOD_CALL_EXPR => ast::MethodCallExpr::cast(node).and_then(|it| it.name_ref()),
        _ => None,
    };
    let Some(token) = name_ref.and_then(|it| it.ident_token()) else { return false };
    let is_name = |text: &str| text.strip_prefix("r#").unwrap_or(text) == name;
    if !is_name(token.text()) {
        return false;
    }
    match source.file_id {
        HirFileId::FileId(_) => true,
        // Mapped up only when its context is the file's own, so a token of the macro's own
        // text, whose context is the macro's, maps up to nothing.
        HirFileId::MacroFile(_) => {
            InFile::new(source.file_id, token).original_file_range_opt(db).is_some_and(|it| {
                let text = db.file_text(it.file_id.file_id(db)).text(db);
                text.get(std::ops::Range::<usize>::from(it.range)).is_some_and(is_name)
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_fixture::WithFixture as _;

    /// A crate that writes a generic function, an inherent generic method and a trait's
    /// generic method implemented twice, calls of each in every place a call can be written
    /// (by path, by pointer, in a closure, in a nested function, in a constant of a body, at
    /// the top level, through a macro, through an import, in a crate that uses it), and calls
    /// of a function of the same name that is another.
    const FIXTURE: &str = r#"
//- /lib.rs crate:lib
pub const fn g<T: Copy>(t: T) -> T { t }
pub struct S;
impl S { pub fn m<T>(&self, t: T) -> T { t } }
pub trait Tr { fn t<T>(&self, x: T) -> T; }
impl Tr for S { fn t<T>(&self, x: T) -> T { x } }
pub struct R;
impl Tr for R { fn t<T>(&self, x: T) -> T { x } }
macro_rules! pass { ($e:expr) => { $e } }
macro_rules! write_own { () => { g(5u8) } }
pub fn by_path() -> u8 { g(1) }
pub fn by_pointer() -> u8 { let p = g::<u8>; p(1) }
pub fn in_closure() -> u8 { let c = || g(2); c() }
pub fn in_block_const() -> u8 { const C: u8 = g(3); C }
pub fn in_nested() -> u8 { fn nested() -> u8 { g(4) } nested() }
pub const TOP: u8 = g(4);
pub fn through_macro() -> u8 { pass!(g(6)) }
pub fn macro_writes() -> u8 { write_own!() }
pub fn generic_caller<T: Copy>(t: T) -> T { g(t) }
pub fn calls_generic_caller() -> u8 { generic_caller(15) }
pub fn inherent() -> u8 { S.m(7) }
pub fn trait_s() -> u8 { S.t(8) }
pub fn trait_r() -> u8 { R.t(9) }
pub fn ufcs() -> u8 { <S as Tr>::t(&S, 10) }
pub fn generic_bound<X: Tr>(x: X) -> u8 { x.t(11) }
mod imports {
    use crate::g;
    pub fn imported() -> u8 { g(12) }
}
mod other {
    pub fn g() {}
    pub fn calls_other() { g() }
}
//- /main.rs crate:app deps:lib
fn main() { lib::g(13); }
fn uses_method() { lib::S.m(14); }
"#;

    fn name_of(db: &Db, func: FunctionId) -> &str {
        FunctionSignature::of(db, func).name.as_str()
    }

    /// The function of `krate` named `name`, in a body or not.
    fn function(db: &Db, krate: base_db::Crate, name: &str) -> FunctionId {
        let bodies = hir_def::nameres::demand::every_body(db, krate);
        let found = bodies.into_iter().find_map(|it| match it {
            DefWithBodyId::FunctionId(it) if name_of(db, it) == name => Some(it),
            _ => None,
        });
        found.unwrap_or_else(|| panic!("no function {name}"))
    }

    fn names_of(db: &Db, functions: impl IntoIterator<Item = FunctionId>) -> FxHashSet<&str> {
        functions.into_iter().map(|it| name_of(db, it)).collect()
    }

    /// rust-analyzer's search for the references of `func` and of the trait's function it
    /// implements, each taken to the innermost `fn` around it: what [`direct_callers`] was
    /// before it read the bodies, kept to compare with while `ide_db` is in the tree.
    fn by_references(db: &Db, func: FunctionId) -> FxHashSet<FunctionId> {
        let sema = hir::Semantics::new(db);
        let named = |func: FunctionId| {
            let func = hir::Function::from(func);
            let trait_ = func.as_assoc_item(db).and_then(|it| it.implemented_trait(db));
            std::iter::once(func).chain(trait_.and_then(|it| it.function(db, func.name(db))))
        };
        let usages =
            named(func).flat_map(|it| ide_db::defs::Definition::Function(it).usages(&sema).all());
        let mut callers = FxHashSet::default();
        for reference in usages.flat_map(|(_, references)| references) {
            if reference.category.contains(ide_db::search::ReferenceCategory::IMPORT) {
                continue;
            }
            let ide_db::search::FileReferenceNode::NameRef(name) = reference.name else { continue };
            let name = syntax::AstNode::syntax(&name);
            let caller = sema
                .ancestors_with_macros(name.clone())
                .find_map(<syntax::ast::Fn as syntax::AstNode>::cast);
            let caller = caller.and_then(|it| sema.to_def(&it));
            callers.extend(caller.and_then(|it| FunctionId::try_from(it).ok()));
        }
        callers
    }

    fn direct(db: &Db, func: FunctionId) -> FxHashSet<FunctionId> {
        let mut found = Vec::new();
        direct_callers(db, func, &mut found);
        found.into_iter().collect()
    }

    /// The callers the bodies give are those rust-analyzer's search of the references gives,
    /// for a free function, an inherent method and a trait's method (through either impl),
    /// and are the ones written out here.
    #[test]
    fn the_callers_in_the_bodies_are_those_the_references_give() {
        let (db, files) = ide_db::RootDatabase::with_many_files(FIXTURE);
        hir::attach_db(&db, || {
            let lib = ide_db::base_db::relevant_crates(&db, files[0].file_id(&db))[0];
            let expected: [(&str, &[&str]); 4] = [
                ("g", &[
                    "by_path",
                    "by_pointer",
                    "in_closure",
                    "in_block_const",
                    "nested",
                    "through_macro",
                    "generic_caller",
                    "imported",
                    "main",
                ]),
                ("m", &["inherent", "uses_method"]),
                ("t", &["trait_s", "trait_r", "ufcs", "generic_bound"]),
                ("generic_caller", &["calls_generic_caller"]),
            ];
            for (func, callers) in expected {
                let func = function(&db, lib, func);
                let found = direct(&db, func);
                let named = name_of(&db, func);
                assert_eq!(found, by_references(&db, func), "{named}");
                assert_eq!(names_of(&db, found), callers.iter().copied().collect(), "{named}");
            }
        });
    }

    /// Either impl's method of a trait has the callers of the trait's method: those of the
    /// other impl's too.
    #[test]
    fn each_impl_of_a_trait_method_has_the_callers_of_the_trait_method() {
        let (db, files) = ide_db::RootDatabase::with_many_files(FIXTURE);
        hir::attach_db(&db, || {
            let lib = ide_db::base_db::relevant_crates(&db, files[0].file_id(&db))[0];
            let bodies = hir_def::nameres::demand::every_body(&db, lib);
            let impls: Vec<FunctionId> = bodies
                .into_iter()
                .filter_map(|it| match it {
                    DefWithBodyId::FunctionId(it) if name_of(&db, it) == "t" => Some(it),
                    _ => None,
                })
                .collect();
            assert_eq!(impls.len(), 2);
            assert_eq!(in_trait(&db, impls[0]), in_trait(&db, impls[1]));
            assert_ne!(in_trait(&db, impls[0]), impls[0]);
            assert_eq!(direct(&db, impls[0]), direct(&db, impls[1]));
        });
    }

    /// The callers of a generic function are found in turn for those of them that are generic,
    /// and none is found twice.
    #[test]
    fn the_callers_of_generic_callers_are_found_too() {
        let (db, files) = ide_db::RootDatabase::with_many_files(FIXTURE);
        hir::attach_db(&db, || {
            let lib = ide_db::base_db::relevant_crates(&db, files[0].file_id(&db))[0];
            let g = function(&db, lib, "g");
            let callers = callers_of_generic(&db, &[g]);
            let unique: FxHashSet<FunctionId> = callers.iter().copied().collect();
            assert_eq!(unique.len(), callers.len());
            let expected = [
                "by_path",
                "by_pointer",
                "in_closure",
                "in_block_const",
                "nested",
                "through_macro",
                "generic_caller",
                "imported",
                "main",
                "calls_generic_caller",
            ];
            assert_eq!(names_of(&db, callers), expected.into_iter().collect());
        });
    }

    #[test]
    fn a_name_is_matched_as_a_whole_word() {
        assert!(has_word("g(1)", "g"));
        assert!(has_word("r#g(1)", "g"));
        assert!(has_word("a::g::<u8>", "g"));
        assert!(!has_word("gg(1)", "g"));
        assert!(!has_word("ag(1)", "g"));
        assert!(!has_word("_g(1)", "g"));
        assert!(!has_word("g1(1)", "g"));
        assert!(has_word("1g", "g"));
        assert!(!has_word("", "g"));
    }
}
