//! Keywords recognized from an identifier's bytes: a length check, then a perfect hash built at
//! compile time (`blaze_phf`), one compare of the one keyword the bytes can be, and its edition.
//! No string `match`, which compiles to a compare chain per length.
//!
//! Cost: about 0.5 KB a table; a lookup of an identifier outside the keywords' lengths is two
//! compares, else one hash of at most 15 bytes, two loads and one compare.

use crate::{
    Edition,
    SyntaxKind::{self, *},
};

/// The strict and reserved keywords, with the edition each is a keyword from.
const STRICT: &[(&str, SyntaxKind, Edition)] = &[
    ("Self", SELF_TYPE_KW, Edition::Edition2015),
    ("abstract", ABSTRACT_KW, Edition::Edition2015),
    ("as", AS_KW, Edition::Edition2015),
    ("become", BECOME_KW, Edition::Edition2015),
    ("box", BOX_KW, Edition::Edition2015),
    ("break", BREAK_KW, Edition::Edition2015),
    ("const", CONST_KW, Edition::Edition2015),
    ("continue", CONTINUE_KW, Edition::Edition2015),
    ("crate", CRATE_KW, Edition::Edition2015),
    ("do", DO_KW, Edition::Edition2015),
    ("else", ELSE_KW, Edition::Edition2015),
    ("enum", ENUM_KW, Edition::Edition2015),
    ("extern", EXTERN_KW, Edition::Edition2015),
    ("false", FALSE_KW, Edition::Edition2015),
    ("final", FINAL_KW, Edition::Edition2015),
    ("fn", FN_KW, Edition::Edition2015),
    ("for", FOR_KW, Edition::Edition2015),
    ("if", IF_KW, Edition::Edition2015),
    ("impl", IMPL_KW, Edition::Edition2015),
    ("in", IN_KW, Edition::Edition2015),
    ("let", LET_KW, Edition::Edition2015),
    ("loop", LOOP_KW, Edition::Edition2015),
    ("macro", MACRO_KW, Edition::Edition2015),
    ("match", MATCH_KW, Edition::Edition2015),
    ("mod", MOD_KW, Edition::Edition2015),
    ("move", MOVE_KW, Edition::Edition2015),
    ("mut", MUT_KW, Edition::Edition2015),
    ("override", OVERRIDE_KW, Edition::Edition2015),
    ("priv", PRIV_KW, Edition::Edition2015),
    ("pub", PUB_KW, Edition::Edition2015),
    ("ref", REF_KW, Edition::Edition2015),
    ("return", RETURN_KW, Edition::Edition2015),
    ("self", SELF_KW, Edition::Edition2015),
    ("static", STATIC_KW, Edition::Edition2015),
    ("struct", STRUCT_KW, Edition::Edition2015),
    ("super", SUPER_KW, Edition::Edition2015),
    ("trait", TRAIT_KW, Edition::Edition2015),
    ("true", TRUE_KW, Edition::Edition2015),
    ("type", TYPE_KW, Edition::Edition2015),
    ("typeof", TYPEOF_KW, Edition::Edition2015),
    ("unsafe", UNSAFE_KW, Edition::Edition2015),
    ("unsized", UNSIZED_KW, Edition::Edition2015),
    ("use", USE_KW, Edition::Edition2015),
    ("virtual", VIRTUAL_KW, Edition::Edition2015),
    ("where", WHERE_KW, Edition::Edition2015),
    ("while", WHILE_KW, Edition::Edition2015),
    ("yield", YIELD_KW, Edition::Edition2015),
    ("async", ASYNC_KW, Edition::Edition2018),
    ("await", AWAIT_KW, Edition::Edition2018),
    ("dyn", DYN_KW, Edition::Edition2018),
    ("gen", GEN_KW, Edition::Edition2024),
    ("try", TRY_KW, Edition::Edition2018),
];

/// The contextual keywords, with the edition each stops being one at, if it does.
const CONTEXTUAL: &[(&str, SyntaxKind, Option<Edition>)] = &[
    ("asm", ASM_KW, None),
    ("att_syntax", ATT_SYNTAX_KW, None),
    ("auto", AUTO_KW, None),
    ("bikeshed", BIKESHED_KW, None),
    ("builtin", BUILTIN_KW, None),
    ("cfg", CFG_KW, None),
    ("cfg_attr", CFG_ATTR_KW, None),
    ("clobber_abi", CLOBBER_ABI_KW, None),
    ("default", DEFAULT_KW, None),
    ("deref", DEREF_KW, None),
    ("dyn", DYN_KW, Some(Edition::Edition2018)),
    ("format_args", FORMAT_ARGS_KW, None),
    ("global_asm", GLOBAL_ASM_KW, None),
    ("include_bytes", INCLUDE_BYTES_KW, None),
    ("inlateout", INLATEOUT_KW, None),
    ("inout", INOUT_KW, None),
    ("is", IS_KW, None),
    ("label", LABEL_KW, None),
    ("lateout", LATEOUT_KW, None),
    ("macro_rules", MACRO_RULES_KW, None),
    ("may_unwind", MAY_UNWIND_KW, None),
    ("naked_asm", NAKED_ASM_KW, None),
    ("nomem", NOMEM_KW, None),
    ("noreturn", NORETURN_KW, None),
    ("nostack", NOSTACK_KW, None),
    ("null", NULL_KW, None),
    ("offset_of", OFFSET_OF_KW, None),
    ("options", OPTIONS_KW, None),
    ("out", OUT_KW, None),
    ("pattern_type", PATTERN_TYPE_KW, None),
    ("pin", PIN_KW, None),
    ("preserves_flags", PRESERVES_FLAGS_KW, None),
    ("pure", PURE_KW, None),
    ("raw", RAW_KW, None),
    ("readonly", READONLY_KW, None),
    ("safe", SAFE_KW, None),
    ("sym", SYM_KW, None),
    ("union", UNION_KW, None),
    ("yeet", YEET_KW, None),
];

const fn names<const N: usize, T: Copy, U: Copy>(table: &[(&'static str, T, U)]) -> [&'static str; N] {
    assert!(table.len() == N);
    let mut names = [""; N];
    let mut i = 0;
    while i < N {
        names[i] = table[i].0;
        i += 1;
    }
    names
}

/// The shortest and the longest of `names`.
const fn lengths(names: &[&str]) -> (usize, usize) {
    let (mut shortest, mut longest) = (usize::MAX, 0);
    let mut i = 0;
    while i < names.len() {
        let len = names[i].len();
        if len < shortest {
            shortest = len;
        }
        if len > longest {
            longest = len;
        }
        i += 1;
    }
    (shortest, longest)
}

const STRICT_NAMES: [&str; STRICT.len()] = names(STRICT);
const CONTEXTUAL_NAMES: [&str; CONTEXTUAL.len()] = names(CONTEXTUAL);
const STRICT_LENGTHS: (usize, usize) = lengths(&STRICT_NAMES);
const CONTEXTUAL_LENGTHS: (usize, usize) = lengths(&CONTEXTUAL_NAMES);

blaze_phf::phf_table!(static STRICT_TABLE = &STRICT_NAMES;);
blaze_phf::phf_table!(static CONTEXTUAL_TABLE = &CONTEXTUAL_NAMES;);

/// Every strict, reserved and contextual keyword's text, a text in both lists twice.
pub(super) fn texts() -> impl Iterator<Item = &'static str> {
    STRICT_NAMES.into_iter().chain(CONTEXTUAL_NAMES)
}

/// [`SyntaxKind::from_keyword`].
#[inline]
pub(super) fn strict(ident: &str, edition: Edition) -> Option<SyntaxKind> {
    let len = ident.len();
    if len < STRICT_LENGTHS.0 || len > STRICT_LENGTHS.1 {
        return None;
    }
    let (_, kind, since) = STRICT[STRICT_TABLE.get(ident)?];
    (since <= edition).then_some(kind)
}

/// [`SyntaxKind::from_contextual_keyword`].
#[inline]
pub(super) fn contextual(ident: &str, edition: Edition) -> Option<SyntaxKind> {
    let len = ident.len();
    if len < CONTEXTUAL_LENGTHS.0 || len > CONTEXTUAL_LENGTHS.1 {
        return None;
    }
    let (_, kind, until) = CONTEXTUAL[CONTEXTUAL_TABLE.get(ident)?];
    until.is_none_or(|until| edition < until).then_some(kind)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `match` the tables replace, as the grammar generator wrote it.
    fn strict_by_match(ident: &str, edition: Edition) -> Option<SyntaxKind> {
        let kw = match ident {
            "Self" => SELF_TYPE_KW,
            "abstract" => ABSTRACT_KW,
            "as" => AS_KW,
            "become" => BECOME_KW,
            "box" => BOX_KW,
            "break" => BREAK_KW,
            "const" => CONST_KW,
            "continue" => CONTINUE_KW,
            "crate" => CRATE_KW,
            "do" => DO_KW,
            "else" => ELSE_KW,
            "enum" => ENUM_KW,
            "extern" => EXTERN_KW,
            "false" => FALSE_KW,
            "final" => FINAL_KW,
            "fn" => FN_KW,
            "for" => FOR_KW,
            "if" => IF_KW,
            "impl" => IMPL_KW,
            "in" => IN_KW,
            "let" => LET_KW,
            "loop" => LOOP_KW,
            "macro" => MACRO_KW,
            "match" => MATCH_KW,
            "mod" => MOD_KW,
            "move" => MOVE_KW,
            "mut" => MUT_KW,
            "override" => OVERRIDE_KW,
            "priv" => PRIV_KW,
            "pub" => PUB_KW,
            "ref" => REF_KW,
            "return" => RETURN_KW,
            "self" => SELF_KW,
            "static" => STATIC_KW,
            "struct" => STRUCT_KW,
            "super" => SUPER_KW,
            "trait" => TRAIT_KW,
            "true" => TRUE_KW,
            "type" => TYPE_KW,
            "typeof" => TYPEOF_KW,
            "unsafe" => UNSAFE_KW,
            "unsized" => UNSIZED_KW,
            "use" => USE_KW,
            "virtual" => VIRTUAL_KW,
            "where" => WHERE_KW,
            "while" => WHILE_KW,
            "yield" => YIELD_KW,
            "async" if Edition::Edition2018 <= edition => ASYNC_KW,
            "await" if Edition::Edition2018 <= edition => AWAIT_KW,
            "dyn" if Edition::Edition2018 <= edition => DYN_KW,
            "gen" if Edition::Edition2024 <= edition => GEN_KW,
            "try" if Edition::Edition2018 <= edition => TRY_KW,
            _ => return None,
        };
        Some(kw)
    }

    fn contextual_by_match(ident: &str, edition: Edition) -> Option<SyntaxKind> {
        let kw = match ident {
            "asm" => ASM_KW,
            "att_syntax" => ATT_SYNTAX_KW,
            "auto" => AUTO_KW,
            "bikeshed" => BIKESHED_KW,
            "builtin" => BUILTIN_KW,
            "cfg" => CFG_KW,
            "cfg_attr" => CFG_ATTR_KW,
            "clobber_abi" => CLOBBER_ABI_KW,
            "default" => DEFAULT_KW,
            "deref" => DEREF_KW,
            "dyn" if edition < Edition::Edition2018 => DYN_KW,
            "format_args" => FORMAT_ARGS_KW,
            "global_asm" => GLOBAL_ASM_KW,
            "include_bytes" => INCLUDE_BYTES_KW,
            "inlateout" => INLATEOUT_KW,
            "inout" => INOUT_KW,
            "is" => IS_KW,
            "label" => LABEL_KW,
            "lateout" => LATEOUT_KW,
            "macro_rules" => MACRO_RULES_KW,
            "may_unwind" => MAY_UNWIND_KW,
            "naked_asm" => NAKED_ASM_KW,
            "nomem" => NOMEM_KW,
            "noreturn" => NORETURN_KW,
            "nostack" => NOSTACK_KW,
            "null" => NULL_KW,
            "offset_of" => OFFSET_OF_KW,
            "options" => OPTIONS_KW,
            "out" => OUT_KW,
            "pattern_type" => PATTERN_TYPE_KW,
            "pin" => PIN_KW,
            "preserves_flags" => PRESERVES_FLAGS_KW,
            "pure" => PURE_KW,
            "raw" => RAW_KW,
            "readonly" => READONLY_KW,
            "safe" => SAFE_KW,
            "sym" => SYM_KW,
            "union" => UNION_KW,
            "yeet" => YEET_KW,
            _ => return None,
        };
        Some(kw)
    }

    const EDITIONS: [Edition; 4] =
        [Edition::Edition2015, Edition::Edition2018, Edition::Edition2021, Edition::Edition2024];

    #[test]
    fn tables_give_what_the_match_gave() {
        let mut idents: Vec<String> = Vec::new();
        for name in STRICT_NAMES.iter().chain(&CONTEXTUAL_NAMES) {
            idents.push((*name).to_owned());
            idents.push(format!("{name}_"));
            idents.push(format!("r#{name}"));
            idents.push(name.to_uppercase());
            idents.push(name[1..].to_owned());
            idents.push(name[..name.len() - 1].to_owned());
        }
        idents.extend(["", "_", "a", "x", "self_", "Self_", "macro_rules!", "union_", "unions"].map(String::from));
        idents.push("a".repeat(100));
        for ident in &idents {
            for edition in EDITIONS {
                assert_eq!(strict(ident, edition), strict_by_match(ident, edition), "{ident:?} {edition:?}");
                assert_eq!(
                    contextual(ident, edition),
                    contextual_by_match(ident, edition),
                    "{ident:?} {edition:?}"
                );
                assert_eq!(SyntaxKind::from_keyword(ident, edition), strict_by_match(ident, edition));
                assert_eq!(
                    SyntaxKind::from_contextual_keyword(ident, edition),
                    contextual_by_match(ident, edition)
                );
            }
        }
    }

    #[test]
    fn edition_keywords() {
        assert_eq!(strict("async", Edition::Edition2015), None);
        assert_eq!(strict("async", Edition::Edition2018), Some(ASYNC_KW));
        assert_eq!(strict("gen", Edition::Edition2021), None);
        assert_eq!(strict("gen", Edition::Edition2024), Some(GEN_KW));
        assert_eq!(contextual("dyn", Edition::Edition2015), Some(DYN_KW));
        assert_eq!(contextual("dyn", Edition::Edition2018), None);
        assert_eq!(STRICT_LENGTHS, (2, 8));
        assert_eq!(CONTEXTUAL_LENGTHS, (2, 15));
    }
}
