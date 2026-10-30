//! rustc's edit distances, and its choice of a similar name to suggest for one it doesn't know
//! (a lint, a label, a variable).
//!
//! Follows rustc's `compiler/rustc_span/src/edit_distance.rs` (MIT or Apache-2.0), with the same
//! results. The distance is Hyyrö's bit-parallel one (2003, as rapidfuzz's `osa_hyrroe2003`):
//! the shorter name, after rustc's common prefix and suffix are trimmed, is one `u64` of
//! columns, so a pair costs one word step per character of the longer, where rustc's rows cost
//! one cell per pair of characters. ASCII names (all lint names and nearly all identifiers) are
//! compared as bytes and allocate nothing; a shorter name of more than 64 characters takes
//! rustc's rows.

use std::{cmp, mem};

/// The restricted Damerau-Levenshtein distance between `a` and `b`, if it is at most `limit`.
pub fn edit_distance(a: &str, b: &str, limit: usize) -> Option<usize> {
    if a.is_ascii() && b.is_ascii() {
        return distance(a.as_bytes(), b.as_bytes(), limit);
    }
    // alloc: a name that isn't ASCII, on an error's path.
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    distance(&a, &b, limit)
}

/// A character of a name: a byte of an ASCII one, else a `char`.
trait Symbol: Copy + Eq {
    /// Where each symbol is in a pattern of at most 64: bit `i` of its mask is position `i`.
    type Peq;
    fn peq(pattern: &[Self]) -> Self::Peq;
    fn mask(peq: &Self::Peq, c: Self) -> u64;
}

impl Symbol for u8 {
    /// 2 KiB, filled in one pass over the pattern; a lookup is one load.
    type Peq = [u64; 256];

    fn peq(pattern: &[u8]) -> [u64; 256] {
        let mut peq = [0; 256];
        for (i, &c) in pattern.iter().enumerate() {
            peq[usize::from(c)] |= 1 << i;
        }
        peq
    }

    fn mask(peq: &[u64; 256], c: u8) -> u64 {
        peq[usize::from(c)]
    }
}

impl Symbol for char {
    /// The pattern's distinct characters and their masks; a lookup scans at most 64.
    type Peq = Vec<(char, u64)>;

    fn peq(pattern: &[char]) -> Vec<(char, u64)> {
        let mut peq: Vec<(char, u64)> = Vec::with_capacity(pattern.len());
        for (i, &c) in pattern.iter().enumerate() {
            match peq.iter_mut().find(|(it, _)| *it == c) {
                Some((_, mask)) => *mask |= 1 << i,
                None => peq.push((c, 1 << i)),
            }
        }
        peq
    }

    fn mask(peq: &Vec<(char, u64)>, c: char) -> u64 {
        peq.iter().find(|(it, _)| *it == c).map_or(0, |&(_, mask)| mask)
    }
}

fn distance<T: Symbol>(a: &[T], b: &[T], limit: usize) -> Option<usize> {
    let (mut a, mut b) = (a, b);
    if a.len() < b.len() {
        mem::swap(&mut a, &mut b);
    }
    let min_dist = a.len() - b.len();
    if min_dist > limit {
        return None;
    }
    let prefix = a.iter().zip(b).take_while(|(x, y)| x == y).count();
    (a, b) = (&a[prefix..], &b[prefix..]);
    let suffix = a.iter().rev().zip(b.iter().rev()).take_while(|(x, y)| x == y).count();
    (a, b) = (&a[..a.len() - suffix], &b[..b.len() - suffix]);
    if b.is_empty() {
        return Some(min_dist);
    }
    let distance = match b.len() <= 64 {
        true => bit_parallel(a, b, limit)?,
        false => rows(a, b),
    };
    (distance <= limit).then_some(distance)
}

/// Hyyrö's bit-parallel restricted Damerau-Levenshtein distance between `text` and `pattern`
/// (1 to 64 symbols): the column of the DP matrix as vertical deltas `vp`/`vn`, its last cell
/// as `score`. `None` once the score can no longer come down to `limit`.
fn bit_parallel<T: Symbol>(text: &[T], pattern: &[T], limit: usize) -> Option<usize> {
    let peq = T::peq(pattern);
    let last = 1u64 << (pattern.len() - 1);
    let (mut vp, mut vn, mut d0, mut pm_prev) = (!0u64, 0u64, 0u64, 0u64);
    let mut score = pattern.len();
    for (j, &c) in text.iter().enumerate() {
        let pm = T::mask(&peq, c);
        let transposed = ((!d0 & pm) << 1) & pm_prev;
        d0 = (((pm & vp).wrapping_add(vp)) ^ vp) | pm | vn | transposed;
        let hp = vn | !(d0 | vp);
        let hn = d0 & vp;
        score += usize::from(hp & last != 0);
        score -= usize::from(hn & last != 0);
        let hp = (hp << 1) | 1;
        vp = (hn << 1) | !(d0 | hp);
        vn = hp & d0;
        pm_prev = pm;
        let left = text.len() - j - 1;
        if score.saturating_sub(left) > limit {
            return None;
        }
    }
    Some(score)
}

/// rustc's rows, for a pattern longer than a word.
fn rows<T: Symbol>(a: &[T], b: &[T]) -> usize {
    // alloc: three rows, only for a name of more than 64 characters.
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
    prev[b.len()]
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

/// `a.to_uppercase() == b.to_uppercase()`, without building either.
fn eq_uppercase(a: &str, b: &str) -> bool {
    match a.is_ascii() && b.is_ascii() {
        true => a.eq_ignore_ascii_case(b),
        false => a.chars().flat_map(char::to_uppercase).eq(b.chars().flat_map(char::to_uppercase)),
    }
}

fn find_best_match<'a>(
    use_substring_score: bool,
    candidates: &[&'a str],
    lookup: &str,
    dist: Option<usize>,
) -> Option<&'a str> {
    if let Some(found) = candidates.iter().find(|it| eq_uppercase(it, lookup)) {
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
    // The same words sorted: only a candidate of the same length can be one, and it reuses one
    // buffer.
    let mut sorted: Vec<&str> = lookup.split('_').collect();
    sorted.sort_unstable();
    let mut words: Vec<&str> = Vec::with_capacity(sorted.len());
    candidates.iter().fold(None, |found, it| {
        if it.len() != lookup.len() {
            return found;
        }
        words.clear();
        words.extend(it.split('_'));
        words.sort_unstable();
        if words == sorted { Some(*it) } else { found }
    })
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
        assert_eq!(edit_distance("ca", "abc", usize::MAX), Some(3));
        assert_eq!(edit_distance("héllo", "hlélo", usize::MAX), Some(1));
    }

    /// rustc's algorithm as it is, the reference the bit-parallel one is held to.
    fn rustc(a: &str, b: &str, limit: usize) -> Option<usize> {
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
            (a, b) = (a_rest, b_rest);
        }
        while let Some(((b_char, b_rest), (a_char, a_rest))) = b.split_last().zip(a.split_last())
            && a_char == b_char
        {
            (a, b) = (a_rest, b_rest);
        }
        if b.is_empty() {
            return Some(min_dist);
        }
        let distance = rows(a, b);
        (distance <= limit).then_some(distance)
    }

    #[test]
    fn bit_parallel_gives_rustc_s_distances() {
        // xorshift64: names over a small alphabet, so that matches and transpositions are common,
        // of lengths around and past one word.
        let mut state = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let alphabets: [&[char]; 2] = [&['a', 'b', 'c', '_'], &['a', 'é', 'b', '_']];
        for round in 0..20_000 {
            let alphabet = alphabets[round % 2];
            let max = if round % 10 == 0 { 90 } else { 12 };
            let name = |next: &mut dyn FnMut() -> u64| -> String {
                let len = (next() % max) as usize;
                (0..len).map(|_| alphabet[(next() % alphabet.len() as u64) as usize]).collect()
            };
            let a = name(&mut next);
            let b = name(&mut next);
            let limit = match next() % 3 {
                0 => usize::MAX,
                _ => (next() % 8) as usize,
            };
            assert_eq!(edit_distance(&a, &b, limit), rustc(&a, &b, limit), "{a:?} {b:?} {limit}");
        }
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
        assert_eq!(find_best_match_for_name(&["straße"], "STRASSE", None), Some("straße"));
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
