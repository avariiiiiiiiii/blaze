//! rustc's `find_best_match_for_name` (`rustc_span/src/edit_distance.rs`), without the
//! substring score, which match checking does not ask for.

use std::{cmp, mem};

use hir_expand::name::Name;

/// The restricted Damerau-Levenshtein distance of `a` and `b`, if it is at most `limit`.
pub(crate) fn edit_distance(a: &str, b: &str, limit: usize) -> Option<usize> {
    // alloc: a typo search, made only for an unreachable pattern after a catch-all binding.
    let a: Vec<char> = a.chars().collect();
    // alloc: as above.
    let b: Vec<char> = b.chars().collect();
    let (mut a, mut b) = (&a[..], &b[..]);
    if a.len() < b.len() {
        mem::swap(&mut a, &mut b);
    }
    let min_dist = a.len() - b.len();
    if min_dist > limit {
        return None;
    }
    while let (Some((b_char, b_rest)), Some((a_char, a_rest))) = (b.split_first(), a.split_first())
        && a_char == b_char
    {
        a = a_rest;
        b = b_rest;
    }
    while let (Some((b_char, b_rest)), Some((a_char, a_rest))) = (b.split_last(), a.split_last())
        && a_char == b_char
    {
        a = a_rest;
        b = b_rest;
    }
    if b.is_empty() {
        return Some(min_dist);
    }

    // alloc: three rows of the distance table, as above.
    let mut prev_prev = vec![usize::MAX; b.len() + 1];
    // alloc: as above.
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    // alloc: as above.
    let mut current = vec![0; b.len() + 1];
    for i in 1..=a.len() {
        current[0] = i;
        let a_idx = i - 1;
        for j in 1..=b.len() {
            let b_idx = j - 1;
            let substitution_cost = usize::from(a[a_idx] != b[b_idx]);
            current[j] = cmp::min(
                prev[j] + 1,
                cmp::min(current[j - 1] + 1, prev[j - 1] + substitution_cost),
            );
            if i > 1 && j > 1 && a[a_idx] == b[b_idx - 1] && a[a_idx - 1] == b[b_idx] {
                current[j] = cmp::min(current[j], prev_prev[j - 2] + 1);
            }
        }
        [prev_prev, prev, current] = [prev, current, prev_prev];
    }
    let distance = prev[b.len()];
    (distance <= limit).then_some(distance)
}

/// The candidate `lookup` most likely is a typo of: one equal ignoring case, else the closest
/// within a third of its length, else one of the same `_`-separated words in another order.
pub(crate) fn find_best_match_for_name<'n>(candidates: &'n [Name], lookup: &Name) -> Option<&'n Name> {
    let lookup = lookup.as_str();
    // alloc: as above.
    let lookup_uppercase = lookup.to_uppercase();
    if let Some(c) = candidates.iter().find(|c| c.as_str().to_uppercase() == lookup_uppercase) {
        return Some(c);
    }

    let mut dist = cmp::max(lookup.chars().count(), 3) / 3;
    let mut best = None;
    for c in candidates {
        match edit_distance(lookup, c.as_str(), dist) {
            Some(0) => return Some(c),
            Some(d) => {
                dist = d - 1;
                best = Some(c);
            }
            None => {}
        }
    }
    if best.is_some() {
        return best;
    }

    let lookup_words = sort_by_words(lookup);
    candidates.iter().fold(None, |result, candidate| {
        if sort_by_words(candidate.as_str()) == lookup_words { Some(candidate) } else { result }
    })
}

fn sort_by_words(name: &str) -> Vec<&str> {
    // alloc: as above.
    let mut words: Vec<&str> = name.split('_').collect();
    words.sort_unstable();
    words
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distances_are_rustcs() {
        assert_eq!(edit_distance("FOO", "FOO", 1), Some(0));
        assert_eq!(edit_distance("FOO", "FO0", 1), Some(1));
        assert_eq!(edit_distance("ab", "ba", 1), Some(1));
        assert_eq!(edit_distance("kitten", "sitting", 3), Some(3));
        assert_eq!(edit_distance("kitten", "sitting", 2), None);
        assert_eq!(edit_distance("a", "abcde", 2), None);
    }

    #[test]
    fn best_match_prefers_case_then_distance_then_words() {
        let candidates = [Name::new_root("MAGIC_TEST"), Name::new_root("FOO_BAR")];
        let pick = |s: &str| {
            find_best_match_for_name(&candidates, &Name::new_root(s)).map(|n| n.as_str().to_owned())
        };
        assert_eq!(pick("magic_test").as_deref(), Some("MAGIC_TEST"));
        assert_eq!(pick("MAGIC_TES").as_deref(), Some("MAGIC_TEST"));
        assert_eq!(pick("BAR_FOO").as_deref(), Some("FOO_BAR"));
        assert_eq!(pick("ZZZ"), None);
    }
}
