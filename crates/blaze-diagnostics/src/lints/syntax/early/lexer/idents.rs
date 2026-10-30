//! `non_ascii_idents`, `uncommon_codepoints`, `confusable_idents` and
//! `mixed_script_confusables`: rustc's `NonAsciiIdents::check_crate` over the symbol gallery
//! (every identifier the lexer saw and every one a proc macro made, each where it was first
//! seen), with the Unicode tables rustc uses (`unicode-security` at rustc's version). Each is
//! emitted at the crate root: only the crate's attributes set their level.
//!
//! Follows rustc's `compiler/rustc_lint/src/non_ascii_idents.rs` and the diagnostics of
//! `compiler/rustc_lint/src/lints.rs` (`IdentifierNonAsciiChar`,
//! `IdentifierUncommonCodepoints`, `ConfusableIdentifierPair`, `MixedScriptConfusables`), with
//! `compiler/rustc_span/src/symbol.rs` (`Symbol::to_ident_string`) and
//! `compiler/rustc_error_messages/src/lib.rs` (a list of characters written as "a, b, and c")
//! (MIT or Apache-2.0).

use std::collections::BTreeMap;

use blaze_ids::radix;
use hir::InFile;
use rustc_hash::FxHashMap;
use span::Edition;
use syntax::{SyntaxKind, SyntaxNodePtr};
use unicode_security::{
    GeneralSecurityProfile, confusable_detection::skeleton, general_security_profile::IdentifierType,
    is_potential_mixed_script_confusable_char, mixed_script::AugmentedScriptSet,
};

use super::{Order, Symbol};
use crate::{
    cx::Cx,
    lints::syntax::{Emitted, Pass, source_fact},
    lower::Phase,
    model::Diag,
};

/// The lints of `check_crate` over `symbols`, sorted by where each was first seen, at the
/// crate root `krate`.
pub(super) fn lints(cx: &Cx<'_, '_>, symbols: Vec<Symbol<'_>>, krate: InFile<SyntaxNodePtr>) -> Vec<Emitted> {
    diags(cx, &symbols)
        .into_iter()
        .map(|mut diag| {
            source_fact(cx, &mut diag, krate);
            Emitted::new(diag, Phase::Resolve, Pass::Early, krate)
        })
        .collect()
}

/// The diagnostics of [`lints`], in the order rustc emits them.
fn diags(cx: &Cx<'_, '_>, symbols: &[Symbol<'_>]) -> Vec<Diag> {
    let mut out = Vec::new();
    let mut has_non_ascii = false;
    for symbol in symbols {
        let name: &str = &symbol.name;
        if name.is_ascii() {
            continue;
        }
        has_non_ascii = true;
        let span = symbol.place.span(cx);
        out.push(Diag::lint("non_ascii_idents", "identifier contains non-ASCII characters", span.clone()));
        if name.chars().all(GeneralSecurityProfile::identifier_allowed) {
            continue;
        }
        for (codepoints, kind) in uncommon(name) {
            let mut diag = Diag::lint("uncommon_codepoints", uncommon_message(&codepoints, kind), span.clone());
            diag.note(uncommon_note(codepoints.len(), kind));
            out.push(diag);
        }
    }
    if !has_non_ascii {
        return out;
    }
    for (existing, symbol) in confusables(symbols) {
        let (existing, symbol) = (&symbols[existing], &symbols[symbol]);
        let existing_name = ident_string(&existing.name, cx.edition);
        let span = symbol.place.span(cx);
        let mut diag = Diag::lint(
            "confusable_idents",
            format!(
                "found both `{existing_name}` and `{}` as identifiers, which look alike",
                ident_string(&symbol.name, cx.edition)
            ),
            span.clone(),
        );
        diag.span_label(existing.place.span(cx), "other identifier used here");
        diag.span_label(span, format!("this identifier can be confused with `{existing_name}`"));
        out.push(diag);
    }
    for (symbol, set, chars) in mixed_script_confusables(symbols) {
        let includes: Vec<String> = chars.iter().map(|&c| format!("'{c}' (U+{:04X})", c as u32)).collect();
        let mut diag = Diag::lint(
            "mixed_script_confusables",
            format!("the usage of Script Group `{set}` in this crate consists solely of mixed script confusables"),
            symbols[symbol].place.span(cx),
        );
        diag.note(format!("the usage includes {}", includes.join(", ")));
        diag.note("please recheck to make sure their usages are indeed what you want");
        out.push(diag);
    }
    out
}

/// Which kind of uncommon codepoint a lint of `uncommon_codepoints` is of: one of the
/// identifier types rustc names, or `Restricted` for the rest that are not allowed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Uncommon {
    Exclusion,
    Technical,
    LimitedUse,
    NotNfkc,
    Restricted,
}

impl Uncommon {
    fn name(self) -> &'static str {
        match self {
            Uncommon::Exclusion => "Exclusion",
            Uncommon::Technical => "Technical",
            Uncommon::LimitedUse => "Limited_Use",
            Uncommon::NotNfkc => "Not_NFKC",
            Uncommon::Restricted => "Restricted",
        }
    }
}

/// The codepoints of `name` that `uncommon_codepoints` names, one group for each lint it
/// gives, in rustc's order: those of the four identifier types it names, then the rest that
/// the general security profile does not allow.
fn uncommon(name: &str) -> Vec<(Vec<char>, Uncommon)> {
    let mut chars: Vec<(char, Option<IdentifierType>)> =
        name.chars().map(|c| (c, GeneralSecurityProfile::identifier_type(c))).collect();
    let mut groups = Vec::new();
    for (ty, kind) in [
        (IdentifierType::Exclusion, Uncommon::Exclusion),
        (IdentifierType::Technical, Uncommon::Technical),
        (IdentifierType::Limited_Use, Uncommon::LimitedUse),
        (IdentifierType::Not_NFKC, Uncommon::NotNfkc),
    ] {
        let codepoints = extract(&mut chars, |&(_, it)| it == Some(ty));
        if !codepoints.is_empty() {
            groups.push((codepoints, kind));
        }
    }
    let rest = extract(&mut chars, |&(c, _)| !GeneralSecurityProfile::identifier_allowed(c));
    if !rest.is_empty() {
        groups.push((rest, Uncommon::Restricted));
    }
    groups
}

/// Takes the characters of `chars` that `take` picks out, in order (`Vec::extract_if`).
fn extract(chars: &mut Vec<(char, Option<IdentifierType>)>, take: impl Fn(&(char, Option<IdentifierType>)) -> bool) -> Vec<char> {
    let mut taken = Vec::new();
    chars.retain(|it| match take(it) {
        true => {
            taken.push(it.0);
            false
        }
        false => true,
    });
    taken
}

fn uncommon_message(codepoints: &[char], kind: Uncommon) -> String {
    let n = codepoints.len();
    let what = match (n, kind) {
        (1, Uncommon::Exclusion) => "a character from an archaic script".to_owned(),
        (1, Uncommon::Technical) => "a character that is for non-linguistic, specialized usage".to_owned(),
        (1, Uncommon::LimitedUse) => "a character from a script in limited use".to_owned(),
        (1, Uncommon::NotNfkc) => "a non normalized (NFKC) character".to_owned(),
        (1, Uncommon::Restricted) => "an uncommon character".to_owned(),
        (_, Uncommon::Exclusion) => format!("{n} characters from archaic scripts"),
        (_, Uncommon::Technical) => format!("{n} characters that are for non-linguistic, specialized usage"),
        (_, Uncommon::LimitedUse) => format!("{n} characters from scripts in limited use"),
        (_, Uncommon::NotNfkc) => format!("{n} non normalized (NFKC) characters"),
        (_, Uncommon::Restricted) => "uncommon characters".to_owned(),
    };
    let listed: Vec<String> = codepoints.iter().map(|c| format!("{c:?}")).collect();
    format!("identifier contains {what}: {}", and_list(&listed))
}

fn uncommon_note(n: usize, kind: Uncommon) -> String {
    let these = match n {
        1 => "this character is",
        _ => "these characters are",
    };
    match kind {
        Uncommon::Restricted => format!("{these} included in the Unicode general security profile"),
        _ => format!("{these} included in the {} Unicode general security profile", kind.name()),
    }
}

/// A list as rustc's diagnostics write one (`StrListSepByAnd`, in English): "a", "a and b",
/// "a, b, and c".
fn and_list(items: &[String]) -> String {
    match items {
        [] => String::new(),
        [one] => one.clone(),
        [a, b] => format!("{a} and {b}"),
        [init @ .., last] => format!("{}, and {last}", init.join(", ")),
    }
}

/// A symbol as rustc's diagnostics write one (`Symbol::to_ident_string`): with `r#` when it is
/// a keyword of `edition` that can be written raw.
fn ident_string(name: &str, edition: Edition) -> String {
    let raw = SyntaxKind::from_keyword(name, edition).is_some()
        && !matches!(name, "_" | "crate" | "self" | "Self" | "super");
    match raw {
        true => format!("r#{name}"),
        false => name.to_owned(),
    }
}

/// The pairs `confusable_idents` gives, by index in `symbols`: (the symbol it is confused
/// with, the symbol it is given at). A symbol is confused with the first of the same skeleton
/// unless both are ASCII; a non-ASCII symbol then takes an ASCII one's place.
fn confusables(symbols: &[Symbol<'_>]) -> Vec<(usize, usize)> {
    let mut by_skeleton: FxHashMap<String, (usize, bool)> = FxHashMap::default();
    by_skeleton.reserve(symbols.len());
    let mut pairs = Vec::new();
    let mut buf = String::new();
    for (index, symbol) in symbols.iter().enumerate() {
        let is_ascii = symbol.name.is_ascii();
        buf.clear();
        buf.extend(skeleton(&symbol.name));
        match by_skeleton.get_mut(buf.as_str()) {
            Some((existing, existing_is_ascii)) => {
                if !*existing_is_ascii || !is_ascii {
                    pairs.push((*existing, index));
                }
                if *existing_is_ascii && !is_ascii {
                    *existing = index;
                    *existing_is_ascii = false;
                }
            }
            None => {
                by_skeleton.insert(buf.clone(), (index, is_ascii));
            }
        }
    }
    pairs
}

/// How a script set is used by the crate's identifiers (`ScriptSetUsage`).
enum Usage {
    /// Only by mixed script confusables: those characters, and the first symbol of them.
    Suspicious(Vec<char>, usize),
    Verified,
}

/// The lints `mixed_script_confusables` gives: the symbol it is given at (by index in
/// `symbols`), the script set, and its confusable characters, sorted, in the order of the
/// symbols' places.
fn mixed_script_confusables(symbols: &[Symbol<'_>]) -> Vec<(usize, AugmentedScriptSet, Vec<char>)> {
    // In the order first seen, as rustc's `FxIndexMap`; Latin is always verified.
    let mut states: Vec<(AugmentedScriptSet, Usage)> = vec![(AugmentedScriptSet::for_char('A'), Usage::Verified)];
    let mut has_suspicious = false;
    for (index, symbol) in symbols.iter().enumerate() {
        for ch in symbol.name.chars() {
            // ASCII is never confusable; what is not allowed is `uncommon_codepoints`'.
            if ch.is_ascii() || !GeneralSecurityProfile::identifier_allowed(ch) {
                continue;
            }
            let set = AugmentedScriptSet::for_char(ch);
            match states.iter_mut().find(|(it, _)| *it == set) {
                Some((_, state)) => {
                    if let Usage::Suspicious(chars, _) = state {
                        match is_potential_mixed_script_confusable_char(ch) {
                            true => chars.push(ch),
                            false => *state = Usage::Verified,
                        }
                    }
                }
                None => {
                    let state = match is_potential_mixed_script_confusable_char(ch) {
                        false => Usage::Verified,
                        true => {
                            has_suspicious = true;
                            Usage::Suspicious(vec![ch], index)
                        }
                    };
                    states.push((set, state));
                }
            }
        }
    }
    if !has_suspicious {
        return Vec::new();
    }
    let verified: Vec<AugmentedScriptSet> = states
        .iter()
        .filter_map(|(set, state)| matches!(state, Usage::Verified).then_some(*set))
        .collect();
    let mut reports: BTreeMap<(Order, Vec<char>), (usize, AugmentedScriptSet)> = BTreeMap::new();
    'sets: for (set, state) in states {
        let Usage::Suspicious(mut chars, symbol) = state else { continue };
        if set.is_all() {
            continue;
        }
        for existing in &verified {
            if existing.is_all() {
                continue;
            }
            let mut intersect = *existing;
            intersect.intersect_with(set);
            if !intersect.is_empty() && !intersect.is_all() {
                continue 'sets;
            }
        }
        radix::sort_copy_by_key(&mut chars, |&c| u32::from(c));
        chars.dedup();
        reports.insert((symbols[symbol].order, chars), (symbol, set));
    }
    reports.into_iter().map(|((_, chars), (symbol, set))| (symbol, set, chars)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uncommon_codepoints_are_grouped_as_rustc_groups_them() {
        assert_eq!(uncommon("µ"), [(vec!['µ'], Uncommon::NotNfkc)]);
        assert_eq!(uncommon("ㇻㇲㇳ"), [(vec!['ㇻ', 'ㇲ', 'ㇳ'], Uncommon::Restricted)]);
        assert_eq!(
            uncommon_message(&['ㇻ', 'ㇲ', 'ㇳ'], Uncommon::Restricted),
            "identifier contains uncommon characters: 'ㇻ', 'ㇲ', and 'ㇳ'"
        );
        assert_eq!(uncommon_message(&['ĳ'], Uncommon::NotNfkc), "identifier contains a non normalized (NFKC) character: 'ĳ'");
        assert_eq!(uncommon_note(1, Uncommon::NotNfkc), "this character is included in the Not_NFKC Unicode general security profile");
        assert_eq!(uncommon_note(3, Uncommon::Restricted), "these characters are included in the Unicode general security profile");
    }

    #[test]
    fn lists_are_written_as_rustc_writes_them() {
        let items = |n: usize| (0..n).map(|it| it.to_string()).collect::<Vec<_>>();
        assert_eq!(and_list(&items(1)), "0");
        assert_eq!(and_list(&items(2)), "0 and 1");
        assert_eq!(and_list(&items(3)), "0, 1, and 2");
    }

    #[test]
    fn keywords_are_written_raw() {
        assert_eq!(ident_string("match", Edition::Edition2021), "r#match");
        assert_eq!(ident_string("async", Edition::Edition2015), "async");
        assert_eq!(ident_string("async", Edition::Edition2018), "r#async");
        assert_eq!(ident_string("self", Edition::Edition2021), "self");
        assert_eq!(ident_string("ｓ", Edition::Edition2021), "ｓ");
    }
}
