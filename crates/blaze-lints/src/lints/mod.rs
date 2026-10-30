//! The lints, each a port of clippy's (its file says which), and what runs them on a node.

mod bool_assert_comparison;
mod byte_char_slices;
mod clone_on_copy;
mod collapsible_if;
mod crate_in_macro_def;
mod empty_enums;
mod format;
mod get_first;
mod io_other_error;
mod map_clone;
mod map_unwrap_or;
mod match_like_matches_macro;
mod needless_bool;
mod needless_borrow;
mod needless_borrows_for_generic_args;
mod needless_return;
mod new_without_default;
mod nonstandard_macro_braces;
mod redundant_static_lifetimes;
mod single_char_add_str;
mod unnecessary_map_or;
mod upper_case_acronyms;
mod useless_borrows_in_formatting;
mod useless_format;
mod useless_vec;
mod utils;

use syntax::{AstNode, SyntaxNode, ast};

use crate::{
    Level,
    check::Cx,
    level::{Levels, Lint},
    registry::{self, Info, LintIndex},
};

/// When rustc runs a lint's pass, which orders what it emits.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Pass {
    /// Over the crate root's file before macros expand, and again after, as an early pass.
    PreExpansion,
    /// Over the expanded crate, before the late passes.
    Early,
    /// Over the crate's HIR.
    Late,
    /// After the late pass, from what it gathered (clippy's `check_crate_post`), in the order of
    /// where each is.
    AfterLate,
}

macro_rules! implemented {
    ($( $lint:ident = $name:literal, $pass:ident; )*) => {
        /// A lint implemented here, by its index among them; a set of them is an [`On`].
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
        #[repr(u8)]
        pub(crate) enum Implemented {
            $( $lint, )*
        }

        impl Implemented {
            /// Every lint implemented, in the order of their indices.
            pub(crate) const ALL: &[Implemented] = &[$( Implemented::$lint, )*];

            /// Its name, without `clippy::`.
            pub(crate) const fn name(self) -> &'static str {
                match self {
                    $( Implemented::$lint => $name, )*
                }
            }

            /// The pass clippy runs it in.
            const fn pass(self) -> Pass {
                match self {
                    $( Implemented::$lint => Pass::$pass, )*
                }
            }
        }
    };
}

implemented! {
    BoolAssertComparison = "bool_assert_comparison", Late;
    ByteCharSlices = "byte_char_slices", Late;
    CloneOnCopy = "clone_on_copy", Late;
    CollapsibleIf = "collapsible_if", Late;
    CrateInMacroDef = "crate_in_macro_def", Early;
    EmptyEnums = "empty_enums", Late;
    GetFirst = "get_first", Late;
    IoOtherError = "io_other_error", Late;
    MapClone = "map_clone", Late;
    MapUnwrapOr = "map_unwrap_or", Late;
    MatchLikeMatchesMacro = "match_like_matches_macro", Late;
    NeedlessBool = "needless_bool", Late;
    NeedlessBorrow = "needless_borrow", Late;
    NeedlessBorrowsForGenericArgs = "needless_borrows_for_generic_args", Late;
    NeedlessReturn = "needless_return", Late;
    NewWithoutDefault = "new_without_default", Late;
    NonstandardMacroBraces = "nonstandard_macro_braces", PreExpansion;
    RedundantStaticLifetimes = "redundant_static_lifetimes", Early;
    SingleCharAddStr = "single_char_add_str", Late;
    UnnecessaryMapOr = "unnecessary_map_or", Late;
    UpperCaseAcronyms = "upper_case_acronyms", Late;
    UselessBorrowsInFormatting = "useless_borrows_in_formatting", Late;
    UselessFormat = "useless_format", Late;
    UselessVec = "useless_vec", AfterLate;
}

/// A set of [`Implemented`] lints: bit `i` for the lint of index `i`.
pub(crate) type On = u32;

const _: () = assert!(Implemented::ALL.len() <= On::BITS as usize);

/// Each implemented lint's index in clippy's table, found when blaze compiles: a lint not of
/// clippy's is a compile error here.
const REGISTRY_INDEX: [LintIndex; Implemented::ALL.len()] = {
    let mut indices = [match registry::lint_index_const(Implemented::ALL[0].name()) {
        Some(it) => it,
        None => panic!("an implemented lint is not clippy's"),
    }; Implemented::ALL.len()];
    let mut i = 1;
    while i < Implemented::ALL.len() {
        indices[i] = match registry::lint_index_const(Implemented::ALL[i].name()) {
            Some(it) => it,
            None => panic!("an implemented lint is not clippy's"),
        };
        i += 1;
    }
    indices
};

/// For each lint of clippy's table, by its index, the index of the implemented lint it is,
/// or `u8::MAX`.
static BY_REGISTRY: [u8; registry::LINT_COUNT] = {
    let mut by_registry = [u8::MAX; registry::LINT_COUNT];
    let mut i = 0;
    while i < Implemented::ALL.len() {
        by_registry[REGISTRY_INDEX[i].get()] = i as u8;
        i += 1;
    }
    by_registry
};

impl Implemented {
    /// What clippy declares of it.
    #[inline]
    pub(crate) fn info(self) -> &'static Info {
        REGISTRY_INDEX[self as usize].info()
    }

    /// The implemented lint `lint` of clippy's is, if it is one.
    #[inline]
    pub(crate) fn of(lint: LintIndex) -> Option<Implemented> {
        Implemented::ALL.get(usize::from(BY_REGISTRY[lint.get()])).copied()
    }

    #[inline]
    pub(crate) const fn bit(self) -> On {
        1 << self as u32
    }
}

/// Which of rustc's passes over a crate emits `lint`, in their order, for code in the root
/// file or not.
pub(crate) fn phase(lint: Implemented, in_root_file: bool) -> u8 {
    match lint.pass() {
        Pass::PreExpansion if in_root_file => 0,
        Pass::PreExpansion | Pass::Early => 1,
        Pass::AfterLate => 3,
        Pass::Late => 2,
    }
}

/// Every lint implemented, by name.
pub fn names() -> impl Iterator<Item = &'static str> {
    Implemented::ALL.iter().map(|it| it.name())
}

/// The lints that may be at a level other than `allow` somewhere in a crate whose files have
/// `texts`: those the command line or their group does not allow, and those the crate names
/// in an attribute.
pub(crate) fn on(levels: &Levels, texts: &[&str]) -> On {
    let mut on = 0;
    // Whether some file names `clippy::all`, and each group: asked once each, not per lint.
    let mut all: Option<bool> = None;
    let mut groups: [Option<bool>; registry::Group::ALL.len()] = [None; registry::Group::ALL.len()];
    let named = |name: &str| texts.iter().any(|it| it.contains(name));
    for &lint in Implemented::ALL {
        let info = lint.info();
        if levels.of(Lint::Clippy(info), &[]).level != Level::Allow
            || named(info.name)
            || *groups[info.group as usize].get_or_insert_with(|| named(info.group.name()))
            || *all.get_or_insert_with(|| named("clippy::all"))
        {
            on |= lint.bit();
        }
    }
    on
}

/// Whether the walk goes into what derives expand to: only lints that run there need it.
pub(crate) fn in_derives(cx: &Cx<'_, '_, '_>) -> bool {
    cx.on(Implemented::EmptyEnums)
}

/// Whether the walk goes into what macro calls expand to, for the code handed to them: only
/// lints that run on code a macro can be handed need it.
pub(crate) fn in_calls(cx: &Cx<'_, '_, '_>) -> bool {
    cx.on_any(!Implemented::EmptyEnums.bit())
}

/// Runs the lints of `node`'s kind on it.
pub(crate) fn check_node(cx: &mut Cx<'_, '_, '_>, node: &SyntaxNode) {
    if cx.expansion.is_some() {
        if let Some(it) = ast::Enum::cast(node.clone()) {
            empty_enums::check(cx, &it);
        }
        return;
    }
    if let Some(it) = ast::Item::cast(node.clone()) {
        upper_case_acronyms::check(cx, &it);
    }
    if let Some(it) = ast::Enum::cast(node.clone()) {
        empty_enums::check(cx, &it);
    } else if let Some(it) = ast::Impl::cast(node.clone()) {
        new_without_default::check(cx, &it);
    } else if let Some(it) = ast::Fn::cast(node.clone()) {
        needless_return::check_fn(cx, &it);
    } else if let Some(it) = ast::Const::cast(node.clone()) {
        redundant_static_lifetimes::check_const(cx, &it);
    } else if let Some(it) = ast::Static::cast(node.clone()) {
        redundant_static_lifetimes::check_static(cx, &it);
    } else if let Some(it) = ast::MacroCall::cast(node.clone()) {
        nonstandard_macro_braces::check_call(cx, &it);
        bool_assert_comparison::check(cx, &it);
        useless_borrows_in_formatting::check(cx, &it);
        useless_format::check(cx, &it);
        useless_vec::check(cx, &it);
    } else if let Some(it) = ast::MacroRules::cast(node.clone()) {
        nonstandard_macro_braces::check_rules(cx, &it);
        crate_in_macro_def::check(cx, &it);
    } else if let Some(it) = ast::StmtList::cast(node.clone()) {
        needless_bool::check_block(cx, &it);
    } else if let Some(it) = ast::IdentPat::cast(node.clone()) {
        needless_borrow::check_ref_pat(cx, &it);
    }
    if let Some(it) = ast::Expr::cast(node.clone()) {
        if let ast::Expr::IfExpr(it) = &it {
            needless_bool::check_if(cx, it);
            collapsible_if::check(cx, it);
            match_like_matches_macro::check_if_let(cx, it);
        }
        if let ast::Expr::MatchExpr(it) = &it {
            match_like_matches_macro::check_match(cx, it);
        }
        io_other_error::check(cx, &it);
        map_clone::check(cx, &it);
        map_unwrap_or::check(cx, &it);
        needless_borrow::check(cx, &it);
        needless_borrows_for_generic_args::check(cx, &it);
        unnecessary_map_or::check(cx, &it);
        byte_char_slices::check(cx, &it);
        clone_on_copy::check(cx, &it);
        get_first::check(cx, &it);
        single_char_add_str::check(cx, &it);
        if let ast::Expr::ClosureExpr(it) = &it {
            needless_return::check_closure(cx, it);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn implemented_lints_are_clippys() {
        assert_eq!(Implemented::ALL.len(), 24);
        for (at, &lint) in Implemented::ALL.iter().enumerate() {
            assert_eq!(lint as usize, at);
            let info = registry::lint(lint.name()).expect("a lint of clippy's");
            assert!(std::ptr::eq(lint.info(), info));
            assert_eq!(Implemented::of(info.index()), Some(lint));
            assert_eq!(lint.bit(), 1 << at);
        }
        let others = registry::lints().iter().filter(|it| Implemented::ALL.iter().all(|l| l.name() != it.name));
        for info in others {
            assert_eq!(Implemented::of(info.index()), None, "{}", info.name);
        }
        assert!(names().is_sorted());
    }

    #[test]
    fn phases_by_pass() {
        assert_eq!(phase(Implemented::NonstandardMacroBraces, true), 0);
        assert_eq!(phase(Implemented::NonstandardMacroBraces, false), 1);
        assert_eq!(phase(Implemented::CrateInMacroDef, true), 1);
        assert_eq!(phase(Implemented::RedundantStaticLifetimes, false), 1);
        assert_eq!(phase(Implemented::NeedlessBool, true), 2);
        assert_eq!(phase(Implemented::UselessVec, false), 3);
    }

    /// `on` as it was: a set of names, asked of each lint with each text scanned.
    fn on_by_names(levels: &Levels, texts: &[&str]) -> Vec<&'static str> {
        let mut on = Vec::new();
        for &lint in Implemented::ALL {
            let (name, info) = (lint.name(), lint.info());
            let named = |text: &str| {
                text.contains(name) || text.contains(info.group.name()) || text.contains("clippy::all")
            };
            if levels.of(Lint::Clippy(info), &[]).level != Level::Allow || texts.iter().any(|it| named(it)) {
                on.push(name);
            }
        }
        on
    }

    #[test]
    fn on_is_the_set_of_names_it_was() {
        let flags = |args: &str| {
            let args: Vec<String> = args.split_whitespace().map(str::to_owned).collect();
            Levels::new(&crate::level::CommandLine::parse(&args).unwrap())
        };
        let cases: [(&str, &[&str]); 6] = [
            ("", &[]),
            ("-A clippy::all", &[]),
            ("-A clippy::all", &["#![warn(clippy::needless_bool)]"]),
            ("-A clippy::all", &["fn f() {}", "#[deny(clippy::pedantic)] mod m {}"]),
            ("-W clippy::pedantic -A clippy::style", &["clippy::all"]),
            ("-A warnings", &["#[expect(clippy::map_unwrap_or)]"]),
        ];
        for (args, texts) in cases {
            let levels = flags(args);
            let on = on(&levels, texts);
            let by_set: Vec<&str> =
                Implemented::ALL.iter().filter(|it| on & it.bit() != 0).map(|it| it.name()).collect();
            assert_eq!(by_set, on_by_names(&levels, texts), "{args:?} {texts:?}");
        }
    }
}
