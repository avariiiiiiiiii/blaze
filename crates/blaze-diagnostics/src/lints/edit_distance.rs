//! rustc's edit distances, and its choice of a similar name to suggest for one it doesn't know
//! (a lint, a label, a variable).
//!
//! Follows rustc's `compiler/rustc_span/src/edit_distance.rs` (MIT or Apache-2.0).

use std::{cmp, mem};

/// The restricted Damerau-Levenshtein distance between `a` and `b`, if it is at most `limit`.
pub fn edit_distance(a: &str, b: &str, limit: usize) -> Option<usize> {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let (mut a, mut b) = (&a[..], &b[..]);
    if a.len() < b.len() {
        mem::swap(&mut a, &mut b);
    }
    let min_dist = a.len() - b.len();
    if min_dist > limit {
        return None;
    }
    while let Some(((b_char, b_rest), (a_char, a_rest))) = b.split_first().zip(a.split_first())
        && a_char == b_char
    {
        a = a_rest;
        b = b_rest;
    }
    while let Some(((b_char, b_rest), (a_char, a_rest))) = b.split_last().zip(a.split_last())
        && a_char == b_char
    {
        a = a_rest;
        b = b_rest;
    }
    if b.is_empty() {
        return Some(min_dist);
    }
    let mut prev_prev = vec![usize::MAX; b.len() + 1];
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut current = vec![0; b.len() + 1];
    for i in 1..=a.len() {
        current[0] = i;
        let a_idx = i - 1;
        for j in 1..=b.len() {
            let b_idx = j - 1;
            let substitution_cost = usize::from(a[a_idx] != b[b_idx]);
            current[j] = cmp::min(prev[j] + 1, cmp::min(current[j - 1] + 1, prev[j - 1] + substitution_cost));
            if i > 1 && j > 1 && a[a_idx] == b[b_idx - 1] && a[a_idx - 1] == b[b_idx] {
                current[j] = cmp::min(current[j], prev_prev[j - 2] + 1);
            }
        }
        [prev_prev, prev, current] = [prev, current, prev_prev];
    }
    let distance = prev[b.len()];
    (distance <= limit).then_some(distance)
}

/// rustc's `edit_distance_with_substrings`: a substring scores as near.
pub fn edit_distance_with_substrings(a: &str, b: &str, limit: usize) -> Option<usize> {
    let n = a.chars().count();
    let m = b.chars().count();
    let big_len_diff = (n * 2) < m || (m * 2) < n;
    let len_diff = m.abs_diff(n);
    let distance = edit_distance(a, b, limit.saturating_add(len_diff))?;
    let score = distance - len_diff;
    let score = if score == 0 && len_diff > 0 && !big_len_diff {
        1
    } else if !big_len_diff {
        score + len_diff.div_ceil(2)
    } else {
        score + len_diff
    };
    (score <= limit).then_some(score)
}

/// The candidate rustc suggests for `lookup`: one equal to it but for case, else the nearest
/// within `dist` (a third of its length by default), else one of the same words in another
/// order.
pub fn find_best_match_for_name<'a>(candidates: &[&'a str], lookup: &str, dist: Option<usize>) -> Option<&'a str> {
    find_best_match(false, candidates, lookup, dist)
}

/// [`find_best_match_for_name`], a substring scoring as near.
pub fn find_best_match_for_name_with_substrings<'a>(
    candidates: &[&'a str],
    lookup: &str,
    dist: Option<usize>,
) -> Option<&'a str> {
    find_best_match(true, candidates, lookup, dist)
}

/// rustc's `find_best_match_for_names`: the best match of any of `lookups`, the one nearest
/// its lookup.
pub fn find_best_match_for_names<'a>(candidates: &[&'a str], lookups: &[&str], dist: Option<usize>) -> Option<&'a str> {
    lookups
        .iter()
        .filter_map(|lookup| Some((*lookup, find_best_match(false, candidates, lookup, dist)?)))
        .min_by_key(|(lookup, found)| edit_distance(lookup, found, usize::MAX))
        .map(|(_, found)| found)
}

fn find_best_match<'a>(
    use_substring_score: bool,
    candidates: &[&'a str],
    lookup: &str,
    dist: Option<usize>,
) -> Option<&'a str> {
    let lookup_uppercase = lookup.to_uppercase();
    if let Some(found) = candidates.iter().find(|it| it.to_uppercase() == lookup_uppercase) {
        return Some(found);
    }
    let lookup_len = lookup.chars().count();
    let mut dist = dist.unwrap_or_else(|| cmp::max(lookup_len, 3) / 3);
    let mut best = None;
    let mut next_candidates = Vec::new();
    for candidate in candidates {
        let distance = match use_substring_score {
            true => edit_distance_with_substrings(lookup, candidate, dist),
            false => edit_distance(lookup, candidate, dist),
        };
        match distance {
            Some(0) => return Some(candidate),
            Some(d) => {
                if use_substring_score {
                    if d < dist {
                        dist = d;
                        next_candidates.clear();
                    }
                    next_candidates.push(*candidate);
                } else {
                    dist = d - 1;
                }
                best = Some(*candidate);
            }
            None => {}
        }
    }
    if next_candidates.len() > 1 {
        best = find_best_match(false, &next_candidates, lookup, Some(lookup.len()));
    }
    if best.is_some() {
        return best;
    }
    let sorted = sort_by_words(lookup);
    candidates.iter().fold(None, |found, it| if sort_by_words(it) == sorted { Some(*it) } else { found })
}

fn sort_by_words(name: &str) -> Vec<&str> {
    let mut words: Vec<&str> = name.split('_').collect();
    words.sort_unstable();
    words
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distances_as_rustc_counts_them() {
        assert_eq!(edit_distance("kitten", "sitting", usize::MAX), Some(3));
        assert_eq!(edit_distance("ab", "ba", usize::MAX), Some(1));
        assert_eq!(edit_distance("abc", "xyz", 2), None);
        assert_eq!(edit_distance("", "abc", usize::MAX), Some(3));
    }

    #[test]
    fn the_name_rustc_suggests() {
        let names = ["unused_variables", "unused_imports", "dead_code"];
        assert_eq!(find_best_match_for_name(&names, "unused_variable", None), Some("unused_variables"));
        assert_eq!(find_best_match_for_name(&names, "DEAD_CODE", None), Some("dead_code"));
        assert_eq!(find_best_match_for_name(&names, "code_dead", None), Some("dead_code"));
        assert_eq!(find_best_match_for_name(&names, "nothing_like_it", None), None);
        let labels = ["'outer"];
        assert_eq!(find_best_match_for_name(&labels, "'outr", None), Some("'outer"));
    }

    #[test]
    fn substrings_score_near() {
        assert_eq!(edit_distance_with_substrings("cap", "capture", usize::MAX), Some(4));
        let names = ["force_capture", "capture"];
        assert_eq!(
            find_best_match_for_name_with_substrings(&names, "forced_capture", None),
            Some("force_capture")
        );
    }
}
