
## Workspace: blaze-alloc-count and clippy's allocation lints

- What:
  - add `crates/blaze-alloc-count` as a workspace dependency
    (`blaze-alloc-count = { path = "crates/blaze-alloc-count" }`);
  - in `[workspace.lints.clippy]`, set to `"deny"`: `redundant_clone`, `clone_on_ref_ptr`,
    `implicit_clone`, `unnecessary_to_owned`, `inefficient_to_string`, `format_collect`,
    `needless_pass_by_value`, `large_types_passed_by_value`, `box_collection`, `vec_box`,
    `rc_buffer`, `large_enum_variant`.
- Where: root `Cargo.toml`.
- Why: `docs/v2/plan/goals/principles.md`, "Allocation, `dyn` and clones are controlled
  mechanically".

> applied by requests: `blaze-alloc-count = { path = \"crates/blaze-alloc-count\" }` in `[workspace.dependencies]` (new to the lock), and the twelve lints `\"deny\"` in `[workspace.lints.clippy]` under their own heading citing principles.md; `rc_buffer` moved there from the warn list.

## tools/v2/hot_lint.py crashes when given `crates` (from db-ty-port-infer-relate)

- What: `{"root": ".", "crates": ["hir-ty"]}` fails with `typed_input: crates has a type it does
  not read: list[str]`. `python/typed-input` does not read `list[str]` fields, so either it
  learns lists or `Input.crates` becomes a type it does read (a `tuple[str, ...]`, if supported).
- Where: `tools/v2/hot_lint.py` (`Input`) or `python/typed-input/src/typed_input/__init__.py`.
- Why: the brief's command doesn't run. Without `crates` it works (all hot crates).

> forwarded to the coordinator: `tools/` is its, not requests' (listed in requests' report).

## tools/v2/hot_lint.py doesn't see salsa (from speed-salsa)

- What: `HOT` names crates under `crates/` only, so `vendor/salsa`, `vendor/salsa-macros` and
  `vendor/salsa-macro-rules` (every query's hit path) are never linted. Let `crates` take a path
  relative to the root (or add a `vendor` list) and add the three to `HOT`. The lint also skips
  only a literal `#[cfg(test)]`, so `#[cfg(all(test, ...))]` modules are linted as code.
- Where: `tools/v2/hot_lint.py`.
- Why: speed-salsa linted them by calling `hot_lint.lint_file` on each file directly: 0 findings
  now (fixed, or justified on the line), including the proc macros' expansion-time code.
- Also, for `Cargo.lock` at the first build: `vendor/salsa` now depends on `blaze-mem` (memo
  columns are blaze-mem regions) and dev-depends on `blaze-alloc-count`, both by path; salsa's
  lock entry gains the two.

> forwarded to the coordinator: `tools/` is its; on requests' coordinator list (vendor salsa in `HOT`, `cfg(all(test, ..))` as test code).

## speed-hir-store-stmts: statements become columns, `Statement` a view (hir-ide)

From speed-hir-store-stmts (child of speed-hir-store), 2026-10-29. `crates/hir-ide/` is the
coordinator's (`docs/v2/plan/reference/crates.md`) and `crates/hir/` still builds on it, so it
must compile against this change. `hir-def` stores a block's statements as rows of two columns
(5 bytes a statement, a `let`'s ids in `extra`), where it was a 32-byte enum in a flat array.

**The rule.** `Statement` is now `Statement<'a>`, a `Copy` view decoded from the rows, with
today's variant and field names. `Expr::Block { statements, .. }` gives `statements:
Statements<'a>`, a `Copy` handle (`len`, `is_empty`, `get(i) -> Option<Statement<'a>>`, `first`,
`last`, `iter()` double-ended and exact size, `IntoIterator` for it and for `&` it), no longer
`&'a [Statement]`. A statement is a value: its id, `Option` and `bool` fields bind by value
(`*pat` -> `pat`, `Some(*e)` -> `Some(e)`, `match *stmt` -> `match stmt`, a `&Statement::..`
pattern drops the `&`); a parameter `&[Statement]` -> `Statements<'_>`. No slice operations.

Examples (real sites):
- `diagnostics/expr.rs` ~L321: `fn validate_block(&mut self, statements: &[Statement]) {`  ->  `fn validate_block(&mut self, statements: Statements<'_>) {`
- `diagnostics/unsafe_check.rs` ~L378: `&Statement::Let { pat, .. } => Some(pat),`  ->  `Statement::Let { pat, .. } => Some(pat),`
- `diagnostics/expr.rs` ~L500: `Statement::Expr { expr, .. } => Some(*expr),`  ->  `Statement::Expr { expr, .. } => Some(expr),`

Counts: 7 sites in 2 files of `crates/hir-ide/src/diagnostics/`. The rest of `hir-ide`
(`mir_pretty.rs`'s `StatementKind` is MIR's), `crates/hir/`, `crates/ide-db/` and
`crates/test-fixture/` have none.

> applied by requests: the seven sites as listed (`diagnostics/expr.rs`: `Statements` imported, `validate_block(statements: Statements<'_>)` matching `stmt` by value, both `Some(expr)`; `diagnostics/unsafe_check.rs`: both `Statement::Let { pat, .. }` without `&`). `hir-def`'s `Statements` is in the tree.

### crates/hir-ide/src/diagnostics/expr.rs  (5)
- ~L47  `    hir::{Expr, ExprId, MatchArm, Pat, PatId, RecordSpread, Statement},`  ->  `    hir::{Expr, ExprId, MatchArm, Pat, PatId, RecordSpread, Statement, Statements},`
- ~L321  `    fn validate_block(&mut self, statements: &[Statement]) {`  ->  `    fn validate_block(&mut self, statements: Statements<'_>) {`
- ~L325  `            match *stmt {`  ->  `            match stmt {`
- ~L500 and ~L538 (the only two)  `Statement::Expr { expr, .. } => Some(*expr),`  ->  `Statement::Expr { expr, .. } => Some(expr),`
- unchanged: `self.validate_block(statements);` (~L159), `match statements.last()? {` (~L499, ~L537).

> applied by requests: the seven sites as listed (`diagnostics/expr.rs`: `Statements` imported, `validate_block(statements: Statements<'_>)` matching `stmt` by value, both `Some(expr)`; `diagnostics/unsafe_check.rs`: both `Statement::Let { pat, .. }` without `&`). `hir-def`'s `Statements` is in the tree.

### crates/hir-ide/src/diagnostics/unsafe_check.rs  (2)
- ~L378 and ~L390 (the only two)  `&Statement::Let { pat, .. } => Some(pat),`  ->  `Statement::Let { pat, .. } => Some(pat),`

Status: open

> applied by requests: the seven sites as listed (`diagnostics/expr.rs`: `Statements` imported, `validate_block(statements: Statements<'_>)` matching `stmt` by value, both `Some(expr)`; `diagnostics/unsafe_check.rs`: both `Statement::Let { pat, .. }` without `&`). `hir-def`'s `Statements` is in the tree.

## frontend-image: `tools/v2/hot_lint.py` can't run with `crates`, and reads gated test modules as hot code

1. `{"root": ".", "crates": ["intern"]}` fails: `typed_input: crates has a type it does not read:
   list[str]`. `typed_input` reads `tuple[str, ...]`, so the fix is
   `crates: tuple[str, ...] = HOT`.
2. `in_test_module` only spots `#[cfg(test)]`. Modules gated `#[cfg(all(test, blaze_check))]` and
   `#[cfg(all(test, not(blaze_check)))]` (in `intern`'s `arena.rs`, `intern.rs` and
   `intern_slice.rs`) are reported: 28 findings in intern, all test code. Matching
   `#[cfg(` followed by `test` as a word (`r"#\[cfg\((all\()?test\b"`) fixes it.

Until then I ran `lint_file` over my crates directly: the only findings left are those 28.

> forwarded to the coordinator: `tools/` is its; on requests' coordinator list (with usercode-fast-tests 9 and safety F4).

## frontend-tree: `hot_lint.py` refuses its own `crates` field (2026-10-29)

`PYTHONPATH=python/typed-input/src python3 tools/v2/hot_lint.py '{"root": ".", "crates": ["syntax-tree"]}'`
fails with `TypeError: typed_input: crates has a type it does not read: list[str]`: `Input.crates`
is `list[str]`, and `python/typed-input/src/typed_input/__init__.py` `_check` reads no lists.
Either `_check` reads `list[T]` (check a JSON array, each item as `T`), or `crates` becomes
`tuple[str, ...]` if that is read. Without it a lane can only lint the whole hot set.

Status: open

> forwarded to the coordinator: `tools/` is its; on requests' coordinator list.


## dedup: one `stdx::edit_distance` (rustc's edit distance and `find_best_match_for_name`, cargo's `closest`)

From `dedup-scan` (lane dedup; report `/tmp/blaze-lanes/v2-dedup-scan.md`, notes
`docs/v2/plan/lanes/dedup/scan.md`). Six copies of rustc's restricted Damerau-Levenshtein
distance exist: `crates/blaze-manifest/src/edit_distance.rs`, `crates/blaze-resolve/src/edit_distance.rs`
(cargo's: the same algorithm on lowercased strings), `crates/blaze-diagnostics/src/lints/edit_distance.rs`,
`crates/blaze-diagnostics/src/lower/unresolved_field.rs` 474-617, `crates/hir-expand/src/builtin/fn_macro.rs`
1400-1448 and `crates/hir-ty-matchck/src/distance.rs`, plus three copies of `find_best_match_for_name`.
Diagnostics must suggest what rustc suggests, so one implementation of rustc's rules, in `stdx`
(the dedup table in `docs/v2/plan/lanes/dedup.md`: "edit distance and other small text helpers").
The callers' edits are in `v2/requests/diagnostics.md`, `v2/requests/db.md` (hir-expand),
`v2/requests/db-matchck.md` (hir-ty-matchck), each under `## dedup: stdx::edit_distance`; the cargo
crates' edits are below (they are the coordinator's too).

> forwarded to the coordinator (from requests, pass 16; on requests' coordinator list).

### 1. `crates/stdx/src/lib.rs`: add the module

Before:

```rust
pub mod anymap;
pub mod assert;
```

After:

```rust
pub mod anymap;
pub mod assert;
pub mod edit_distance;
```

> forwarded to the coordinator (from requests, pass 16; on requests' coordinator list).

### 2. `crates/stdx/Cargo.toml`: proptest for the differential test

Append (proptest is already a root workspace dependency):

```toml
[dev-dependencies]
proptest.workspace = true
```

> forwarded to the coordinator (from requests, pass 16; on requests' coordinator list).

### 3. New file `crates/stdx/src/edit_distance.rs`

```rust
//! rustc's edit distance and its choice of a similar name to suggest
//! (`compiler/rustc_span/src/edit_distance.rs`, MIT or Apache-2.0), and cargo's case-blind
//! variant ([`cargo`]). Diagnostics must suggest what rustc and cargo suggest, so every rule,
//! default and tie-break here is theirs.

use std::cmp;

/// rustc's restricted Damerau-Levenshtein distance (optimal string alignment) between `a` and
/// `b`, counted in code points, or `None` if it is over `limit`.
pub fn edit_distance(a: &str, b: &str, limit: usize) -> Option<usize> {
    if a.is_ascii() && b.is_ascii() {
        return distance(a.as_bytes(), b.as_bytes(), limit);
    }
    // alloc: code points of a non-ASCII name, on a suggestion's (error) path.
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    distance(&a, &b, limit)
}

/// The distance over code units: bytes when both sides are ASCII (where bytes are code points),
/// else `char`s.
fn distance<T: PartialEq>(a: &[T], b: &[T], limit: usize) -> Option<usize> {
    let (mut a, mut b) = if a.len() < b.len() { (b, a) } else { (a, b) };
    let min_dist = a.len() - b.len();
    if min_dist > limit {
        return None;
    }
    while let (Some((a_first, a_rest)), Some((b_first, b_rest))) = (a.split_first(), b.split_first())
        && a_first == b_first
    {
        a = a_rest;
        b = b_rest;
    }
    while let (Some((a_last, a_rest)), Some((b_last, b_rest))) = (a.split_last(), b.split_last())
        && a_last == b_last
    {
        a = a_rest;
        b = b_rest;
    }
    if b.is_empty() {
        return Some(min_dist);
    }
    let width = b.len() + 1;
    // alloc: the table's three rows, one buffer, on a suggestion's (error) path.
    let mut rows = vec![usize::MAX; 3 * width];
    let (mut prev_prev, mut prev, mut current) = (0, width, 2 * width);
    for (j, cell) in rows[prev..prev + width].iter_mut().enumerate() {
        *cell = j;
    }
    for i in 1..=a.len() {
        rows[current] = i;
        for j in 1..=b.len() {
            let substitution_cost = usize::from(a[i - 1] != b[j - 1]);
            let mut cell =
                cmp::min(rows[prev + j] + 1, cmp::min(rows[current + j - 1] + 1, rows[prev + j - 1] + substitution_cost));
            // `prev_prev` holds a real row whenever `i > 1`: row 0 after the first rotation.
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                cell = cmp::min(cell, rows[prev_prev + j - 2] + 1);
            }
            rows[current + j] = cell;
        }
        (prev_prev, prev, current) = (prev, current, prev_prev);
    }
    let distance = rows[prev + b.len()];
    (distance <= limit).then_some(distance)
}

/// rustc's `edit_distance_with_substrings`: [`edit_distance`] scoring a substring as near, so
/// that `foo` is closer to `foobar` than to `bar`. `limit + len_diff` saturates where rustc's
/// would overflow (only for a `limit` near `usize::MAX`, which rustc never passes).
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

/// rustc's `find_best_match_for_name`: the candidate equal to `lookup` but for case, else the
/// nearest within `dist` (a third of `lookup`'s length, at least one, by default; the first of
/// equally near ones), else one with the same `_`-separated words in another order (the last
/// such).
pub fn find_best_match_for_name<'c, S: AsRef<str>>(candidates: &'c [S], lookup: &str, dist: Option<usize>) -> Option<&'c S> {
    find_best_match_for_name_by(candidates, as_str::<S>, lookup, dist)
}

/// [`find_best_match_for_name`] over candidates whose name `name` gives.
pub fn find_best_match_for_name_by<'c, T>(
    candidates: &'c [T],
    name: impl Fn(&T) -> &str,
    lookup: &str,
    dist: Option<usize>,
) -> Option<&'c T> {
    // alloc: the candidates as references, on a suggestion's (error) path.
    let candidates: Vec<&T> = candidates.iter().collect();
    find_best_match(false, &candidates, &name, lookup, dist)
}

/// rustc's `find_best_match_for_name_with_substrings`: [`find_best_match_for_name`] scoring
/// by [`edit_distance_with_substrings`], the nearest ones then compared by [`edit_distance`].
pub fn find_best_match_for_name_with_substrings<'c, S: AsRef<str>>(
    candidates: &'c [S],
    lookup: &str,
    dist: Option<usize>,
) -> Option<&'c S> {
    // alloc: as in `find_best_match_for_name_by`.
    let candidates: Vec<&S> = candidates.iter().collect();
    find_best_match(true, &candidates, &as_str::<S>, lookup, dist)
}

/// rustc's `find_best_match_for_names`: the best match of any of `lookups`, the one nearest its
/// lookup (the first of equally near ones).
pub fn find_best_match_for_names<'c, S: AsRef<str>>(
    candidates: &'c [S],
    lookups: &[&str],
    dist: Option<usize>,
) -> Option<&'c S> {
    // alloc: as in `find_best_match_for_name_by`.
    let candidates: Vec<&S> = candidates.iter().collect();
    lookups
        .iter()
        .filter_map(|&lookup| Some((lookup, find_best_match(false, &candidates, &as_str::<S>, lookup, dist)?)))
        .min_by_key(|&(lookup, found)| edit_distance(lookup, as_str(found), usize::MAX))
        .map(|(_, found)| found)
}

fn find_best_match<'c, T: ?Sized, F: Fn(&T) -> &str>(
    use_substring_score: bool,
    candidates: &[&'c T],
    name: &F,
    lookup: &str,
    dist: Option<usize>,
) -> Option<&'c T> {
    if let Some(&found) = candidates.iter().find(|&&it| eq_uppercase(name(it), lookup)) {
        return Some(found);
    }
    let lookup_len = lookup.chars().count();
    let mut dist = dist.unwrap_or_else(|| cmp::max(lookup_len, 3) / 3);
    let mut best = None;
    // alloc: rustc's tie set for the substring score, on a suggestion's (error) path.
    let mut next_candidates: Vec<&'c T> = Vec::new();
    for &candidate in candidates {
        let found = match use_substring_score {
            true => edit_distance_with_substrings(lookup, name(candidate), dist),
            false => edit_distance(lookup, name(candidate), dist),
        };
        match found {
            Some(0) => return Some(candidate),
            Some(d) => {
                if use_substring_score {
                    if d < dist {
                        dist = d;
                        next_candidates.clear();
                    }
                    next_candidates.push(candidate);
                } else {
                    dist = d - 1;
                }
                best = Some(candidate);
            }
            None => {}
        }
    }
    if next_candidates.len() > 1 {
        best = find_best_match(false, &next_candidates, name, lookup, Some(lookup.len()));
    }
    if best.is_some() {
        return best;
    }
    let lookup_words = sort_by_words(lookup);
    candidates
        .iter()
        .fold(None, |found, &candidate| if sort_by_words(name(candidate)) == lookup_words { Some(candidate) } else { found })
}

/// `a.to_uppercase() == b.to_uppercase()` without the two strings: `str::to_uppercase` maps each
/// code point on its own, so comparing the mapped code points is the same test.
fn eq_uppercase(a: &str, b: &str) -> bool {
    if a.is_ascii() && b.is_ascii() {
        return a.eq_ignore_ascii_case(b);
    }
    a.chars().flat_map(char::to_uppercase).eq(b.chars().flat_map(char::to_uppercase))
}

fn as_str<S: AsRef<str>>(it: &S) -> &str {
    it.as_ref()
}

fn sort_by_words(name: &str) -> Vec<&str> {
    // alloc: rustc's word sort, on a suggestion's (error) path.
    let mut words: Vec<&str> = name.split('_').collect();
    words.sort_unstable();
    words
}

/// cargo's "a ... with a similar name exists" suggestions (`src/util/edit_distance.rs` of cargo
/// 1.99.0, 5f94df4789): rustc's distance on lowercased strings, the nearest within 3.
pub mod cargo {
    /// cargo's `edit_distance`: [`super::edit_distance`] of `a` and `b` lowercased.
    pub fn edit_distance(a: &str, b: &str, limit: usize) -> Option<usize> {
        if a.is_ascii() && b.is_ascii() && !a.bytes().chain(b.bytes()).any(|it| it.is_ascii_uppercase()) {
            return super::edit_distance(a, b, limit);
        }
        // alloc: cargo lowercases both sides, on an error message's path.
        super::edit_distance(&a.to_lowercase(), &b.to_lowercase(), limit)
    }

    /// cargo's `closest`: the element of `iter` nearest to `choice` within a distance of 3, the
    /// first of the nearest.
    pub fn closest<'a, T>(choice: &str, iter: impl Iterator<Item = T>, key: impl Fn(&T) -> &'a str) -> Option<T> {
        iter.filter_map(|e| Some((edit_distance(choice, key(&e), 3)?, e))).min_by_key(|t| t.0).map(|t| t.1)
    }

    /// cargo's `closest_msg`: `\n\nhelp: a {kind} with a similar name exists: `x`` or nothing.
    pub fn closest_msg<'a, T>(choice: &str, iter: impl Iterator<Item = T>, key: impl Fn(&T) -> &'a str, kind: &str) -> String {
        match closest(choice, iter, &key) {
            Some(e) => format!("\n\nhelp: a {kind} with a similar name exists: `{}`", key(&e)),
            None => String::new(),
        }
    }
}

#[cfg(test)]
mod tests;
```

> forwarded to the coordinator (from requests, pass 16; on requests' coordinator list).

### 4. New file `crates/stdx/src/edit_distance/tests.rs`

Every test the six copies had, plus a proptest differential against rustc's code verbatim
(the `blaze-diagnostics` copy, kept here as the spec once it is deleted).

```rust
use proptest::prelude::*;

use super::*;

/// rustc's `edit_distance` and `find_best_match_for_name_impl` as `rustc_span` writes them:
/// the reference the shared implementation is checked against.
mod rustc {
    use std::{cmp, mem};

    pub(super) fn edit_distance(a: &str, b: &str, limit: usize) -> Option<usize> {
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

    pub(super) fn edit_distance_with_substrings(a: &str, b: &str, limit: usize) -> Option<usize> {
        let n = a.chars().count();
        let m = b.chars().count();
        let big_len_diff = (n * 2) < m || (m * 2) < n;
        let len_diff = m.abs_diff(n);
        let distance = edit_distance(a, b, limit + len_diff)?;
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

    pub(super) fn find_best_match<'a>(
        use_substring_score: bool,
        candidates: &[&'a str],
        lookup: &str,
        dist: Option<usize>,
    ) -> Option<&'a str> {
        let lookup_uppercase = lookup.to_uppercase();
        if let Some(found) = candidates.iter().find(|it| it.to_uppercase() == lookup_uppercase) {
            return Some(*found);
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
                Some(0) => return Some(*candidate),
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
}

fn best<'a>(candidates: &[&'a str], lookup: &str, dist: Option<usize>) -> Option<&'a str> {
    find_best_match_for_name(candidates, lookup, dist).copied()
}

fn best_with_substrings<'a>(candidates: &[&'a str], lookup: &str, dist: Option<usize>) -> Option<&'a str> {
    find_best_match_for_name_with_substrings(candidates, lookup, dist).copied()
}

// From `blaze-diagnostics/src/lints/edit_distance.rs`.
#[test]
fn distances_as_rustc_counts_them() {
    assert_eq!(edit_distance("kitten", "sitting", usize::MAX), Some(3));
    assert_eq!(edit_distance("ab", "ba", usize::MAX), Some(1));
    assert_eq!(edit_distance("abc", "xyz", 2), None);
    assert_eq!(edit_distance("", "abc", usize::MAX), Some(3));
}

// From `hir-ty-matchck/src/distance.rs`.
#[test]
fn distances_are_rustcs() {
    assert_eq!(edit_distance("FOO", "FOO", 1), Some(0));
    assert_eq!(edit_distance("FOO", "FO0", 1), Some(1));
    assert_eq!(edit_distance("ab", "ba", 1), Some(1));
    assert_eq!(edit_distance("kitten", "sitting", 3), Some(3));
    assert_eq!(edit_distance("kitten", "sitting", 2), None);
    assert_eq!(edit_distance("a", "abcde", 2), None);
}

// From `blaze-diagnostics/src/lints/edit_distance.rs`.
#[test]
fn the_name_rustc_suggests() {
    let names = ["unused_variables", "unused_imports", "dead_code"];
    assert_eq!(best(&names, "unused_variable", None), Some("unused_variables"));
    assert_eq!(best(&names, "DEAD_CODE", None), Some("dead_code"));
    assert_eq!(best(&names, "code_dead", None), Some("dead_code"));
    assert_eq!(best(&names, "nothing_like_it", None), None);
    let labels = ["'outer"];
    assert_eq!(best(&labels, "'outr", None), Some("'outer"));
}

// From `hir-ty-matchck/src/distance.rs`, over names given by a function.
#[test]
fn best_match_prefers_case_then_distance_then_words() {
    struct Named(&'static str);
    let candidates = [Named("MAGIC_TEST"), Named("FOO_BAR")];
    let pick = |s: &str| find_best_match_for_name_by(&candidates, |it| it.0, s, None).map(|it| it.0);
    assert_eq!(pick("magic_test"), Some("MAGIC_TEST"));
    assert_eq!(pick("MAGIC_TES"), Some("MAGIC_TEST"));
    assert_eq!(pick("BAR_FOO"), Some("FOO_BAR"));
    assert_eq!(pick("ZZZ"), None);
}

// From `blaze-diagnostics/src/lints/edit_distance.rs`.
#[test]
fn substrings_score_near() {
    assert_eq!(edit_distance_with_substrings("cap", "capture", usize::MAX), Some(4));
    let names = ["force_capture", "capture"];
    assert_eq!(best_with_substrings(&names, "forced_capture", None), Some("force_capture"));
}

#[test]
fn owned_candidates() {
    let names = vec!["alpha".to_owned(), "beta".to_owned()];
    assert_eq!(find_best_match_for_name(&names, "alpah", None).map(String::as_str), Some("alpha"));
    let lookups = ["xx::betta", "betta"];
    assert_eq!(find_best_match_for_names(&names, &lookups, None).map(String::as_str), Some("beta"));
}

// From cargo's `test_edit_distance` (`blaze-resolve/src/edit_distance.rs`).
#[test]
fn cargo_distances() {
    let a = "\nMäry häd ä little lämb\n\nLittle lämb\n";
    let b = "\nMary häd ä little lämb\n\nLittle lämb\n";
    let c = "Mary häd ä little lämb\n\nLittle lämb\n";
    assert_eq!(cargo::edit_distance(a, b, usize::MAX), Some(1));
    assert_eq!(cargo::edit_distance(b, a, usize::MAX), Some(1));
    assert_eq!(cargo::edit_distance(a, c, usize::MAX), Some(2));
    assert_eq!(cargo::edit_distance(c, a, usize::MAX), Some(2));
    assert_eq!(cargo::edit_distance(b, c, usize::MAX), Some(1));
    assert_eq!(cargo::edit_distance(c, b, usize::MAX), Some(1));
    assert_eq!(cargo::edit_distance("ab", "ba", usize::MAX), Some(1));
    assert_eq!(cargo::edit_distance("CHECK", "check", 0), Some(0));
    assert_eq!(cargo::edit_distance("abcdefgh", "x", 3), None);
}

// From `blaze-resolve/src/edit_distance.rs`.
#[test]
fn cargo_suggestion() {
    let names = ["default", "std", "serde"];
    assert_eq!(
        cargo::closest_msg("serd", names.iter(), |n| n, "feature"),
        "\n\nhelp: a feature with a similar name exists: `serde`"
    );
    assert_eq!(cargo::closest_msg("tokio-full", names.iter(), |n| n, "feature"), "");
}

/// Names from a small alphabet (so that near-misses, transpositions and `_`-separated word
/// reorderings are common), with upper case and a non-ASCII letter to reach both paths.
fn name() -> impl Strategy<Value = String> {
    proptest::string::string_regex("[abAB_éΣ]{0,9}").unwrap()
}

proptest! {
    #[test]
    fn distance_is_rustcs(a in name(), b in name(), limit in prop_oneof![0usize..6, Just(usize::MAX)]) {
        prop_assert_eq!(edit_distance(&a, &b, limit), rustc::edit_distance(&a, &b, limit));
    }

    #[test]
    fn substring_score_is_rustcs(a in name(), b in name(), limit in 0usize..6) {
        prop_assert_eq!(edit_distance_with_substrings(&a, &b, limit), rustc::edit_distance_with_substrings(&a, &b, limit));
    }

    #[test]
    fn best_match_is_rustcs(
        candidates in proptest::collection::vec(name(), 0..8),
        lookup in name(),
        dist in proptest::option::of(0usize..4),
        use_substring_score in any::<bool>(),
    ) {
        let candidates: Vec<&str> = candidates.iter().map(String::as_str).collect();
        let ours = match use_substring_score {
            true => best_with_substrings(&candidates, &lookup, dist),
            false => best(&candidates, &lookup, dist),
        };
        prop_assert_eq!(ours, rustc::find_best_match(use_substring_score, &candidates, &lookup, dist));
    }

    #[test]
    fn cargo_distance_is_rustcs_lowercased(a in name(), b in name(), limit in 0usize..6) {
        prop_assert_eq!(
            cargo::edit_distance(&a, &b, limit),
            rustc::edit_distance(&a.to_lowercase(), &b.to_lowercase(), limit)
        );
    }
}
```

> forwarded to the coordinator (from requests, pass 16; on requests' coordinator list).

### 5. `crates/blaze-manifest` and `crates/blaze-resolve` (the coordinator's): onto `stdx`

Both files are cargo's `src/util/edit_distance.rs` verbatim, the same as each other but for doc
comments. (Lane cargo's request 1 moves these crates into `blaze-units`; this edit stands either
way and leaves the move nothing to merge.)

- `crates/blaze-manifest/Cargo.toml` and `crates/blaze-resolve/Cargo.toml`, `[dependencies]`: add
  `stdx.workspace = true`.
- Delete `crates/blaze-manifest/src/edit_distance.rs` and `crates/blaze-resolve/src/edit_distance.rs`
  (blaze-resolve's tests `distances` and `suggestion` are in stdx's tests above as `cargo_distances`
  and `cargo_suggestion`).
- `crates/blaze-manifest/src/lib.rs`: `mod edit_distance;` becomes
  `use stdx::edit_distance::cargo as edit_distance;` (every `crate::edit_distance::closest_msg`
  path in `config/mod.rs`, `manifest.rs`, `targets.rs`, `profiles.rs`, `dependency.rs` then resolves
  unchanged).
- `crates/blaze-resolve/src/lib.rs`: `mod edit_distance;` (line 23) becomes
  `use stdx::edit_distance::cargo as edit_distance;`; line 45
  `pub use edit_distance::{closest, closest_msg, edit_distance};` stays (so `blaze-cargo`'s
  `blaze_resolve::closest` and `crate::closest_msg` in `package_id_spec.rs`, and
  `crate::edit_distance::{closest, closest_msg, edit_distance}` in `resolver/{errors,members,dep_cache}.rs`
  and `summary.rs`, resolve unchanged).

Status: open

> forwarded to the coordinator (from requests, pass 16; on requests' coordinator list).

## dedup: delete `stdx::thread::Pool`, a thread pool nothing uses (from dedup-mem)

- What: delete `crates/stdx/src/thread/pool.rs`. In `crates/stdx/src/thread.rs` remove
  `mod pool;` (line 21) and `pub use pool::Pool;` (line 24). In `crates/stdx/Cargo.toml` remove
  `crossbeam-channel.workspace = true` and `crossbeam-utils = "0.8.21"` (pool.rs is their only
  user in stdx: `rg crossbeam crates/stdx/src` finds nothing else).
- Why: it is rust-analyzer's LSP thread pool and has no caller in the tree
  (`rg 'thread::Pool|stdx::thread' crates` finds only `Builder`, `ThreadIntent`, `JoinHandle`
  in `ide-db` and `vfs-notify`). The plane's pool, `blaze_graph::pool::Pool` (work stealing,
  topology-aware, LIFO slot), is blaze's one thread pool; a second is a copy
  (docs/v2/plan/lanes/dedup.md, "Thread pools").
- Check: `rg -n 'stdx::thread::Pool|thread::\{[^}]*Pool' crates` stays empty.

Status: open

> forwarded to the coordinator (from requests, pass 16; on requests' coordinator list).

## dedup: workspace dependency `blaze-sha256`

From `dedup-codec` (report `/tmp/blaze-lanes/v2-dedup-codec.md`). New crate
`crates/blaze-sha256` (no dependencies): the one SHA-256, with the SHA-NI and ARMv8 SHA-256
instructions, replacing the four copies in `crates/blaze-cargo/build.rs`,
`crates/blaze-engine/build.rs`, `crates/blaze-link/src/macho/sha256.rs` and
`crates/blaze-splice/src/sha256.rs`. The two build scripts take it as a build-dependency: a
crate of its own because `tools/nix/units.nix` cuts each crate to its own directory (so no
`include!` of a sibling's file), and with no dependencies because those build scripts avoid
`sha2` (its `cpufeatures` needs `libc` on Apple's aarch64, see `crates/blaze-engine/build.rs`).

Root `Cargo.toml`, `[workspace.dependencies]`, beside `blaze-radix`:

    blaze-sha256 = { path = "crates/blaze-sha256" }

(`members = ["crates/*", ...]` already takes the crate.) `Cargo.lock` and `tools/nix/units/*.json`
gain the package when next regenerated.

> applied by requests: `blaze-sha256 = { path = "crates/blaze-sha256" }` beside `blaze-radix` in the root `[workspace.dependencies]`. New to the lock (a path crate): on requests' lock additions list.

## dedup: memmap2 leaves the workspace

From `dedup-map` (report `/tmp/blaze-lanes/v2-dedup-map.md`). Every read-only file map is now
`blaze_core::fs::Mapped`. Once the requests in `v2/requests/codegen.md`, `proof.md` and `cargo.md`
headed "dedup: ... memmap2" are applied, no crate of ours names memmap2: delete the root
`Cargo.toml` `[workspace.dependencies]` line

    memmap2 = "0.9"

and regenerate `Cargo.lock` (`memmap2` stays only where `library/vendor/addr2line` asks for it
optionally, which is the vendored std's, not ours). `blaze-splice` and `blaze-store` gained
`blaze-core.workspace = true` (blaze-store under `cfg(not(target_family = "wasm"))`), so the lock
gains those edges.

Status: open

> held by requests: the root `memmap2` line goes once codegen.md, proof.md and cargo.md's "dedup: ... memmap2" requests have outcomes and `rg memmap2 crates --glob Cargo.toml` finds nothing; the lock is the first compile's.
> applied: no crate manifest or source names memmap2 any more (codegen's is applied, cargo's and proof's are in), so the root `memmap2 = "0.9"` line is deleted; the lock change is the first compile's (requests, pass 22).


## speed-hir-store-paths: paths as columns (2026-10-29)

From `speed-hir-store-paths-consumers` (child of `speed-hir-store-paths`). hir-def's paths and
generic arguments are columns now (`crates/hir-def/src/expr_store/path.rs`; the API is in
`/tmp/blaze-lanes/v2-speed-hir-store-paths.md`, "The API"): `Path<'a>`, `GenericArgs<'a>`,
`AssociatedTypeBinding<'a>`, `PathGenericArgs<'a>` and `Bindings<'a>` are `Copy` views decoded
from the store; `TypeBound` and `WherePredicate` are `Copy` records whose `for<..>` binders and
`use<..>` arguments are runs (`Names`, `UseArgs`) read with `store.names(..)` and
`store.use_args(..)`. Apply these with the paths lane's own edits to `hir.rs`/`expr_store*`
(one compile). Nothing here is built (common.md).

The consumer rule:

1. `&Path` parameters and fields become `Path<'_>` (by value; `Copy`). Where a function keeps
   `&Path<'_>`, callers pass `&path`. `&store[p]` / `&map[p]` for a `PathId` -> `store.path(p)`.
2. `Path::Normal(it) => &it.mod_path` -> `it.mod_path` (already a reference).
   `it.generic_args` / `it.type_anchor` field reads are unchanged.
3. `generic_args()` yields slots by value: `.into_iter().flatten().flatten()` still gives
   `GenericArgs<'_>`; `.iter().any(Option::is_some)` -> `.iter().any(|it| it.is_some())`;
   `generic_args()?.last()?.as_ref()?` -> `generic_args()?.last()??`; `&generic_args[i]` ->
   `generic_args.get(i)`.
4. `&GenericArgs` parameters -> `GenericArgs<'_>`; `Option<&GenericArgs>` -> `Option<GenericArgs<'_>>`
   (drop `.as_ref()`/`.as_deref()`; `*args` -> `args`). `args.bindings.iter()`, `for b in
   &args.bindings`, `.first()`, `.is_empty()`, `.len()` keep working; `bindings[i]` ->
   `bindings.get(i).unwrap()` (or restructure), `&*bindings`/`bindings.as_ref()` -> iterate.
5. `binding.args.as_ref()` -> `binding.args`; `&binding.bounds` / `binding.bounds.iter()`
   unchanged (`&'a [TypeBound]`).
6. `TypeBound::ForLifetime(lifetimes, path)`: `lifetimes` is `Names`; read with
   `store.names(lifetimes)`. `TypeBound::Use(args)`: `store.use_args(args)`. `bound.clone()` ->
   `*bound`; `&TypeBound::Path(p, m)` patterns keep working.
7. `WherePredicate::TypeBound { lifetimes: Some(l), .. }`: `l` is `Names`; `store.names(l)`.
8. `Path::from_known_path_with_no_generic(m)` -> `Path::from_known_path_with_no_generic(&m)`
   (bind the `ModPath` to a local if it is a temporary used past the statement).
9. `path.qualifier()` gives `Qualifier<'_>`: use `.path()` for a `Path<'_>` (bind the
   qualifier to a local first: `let q = path.qualifier()?; let q = q.path();`).

Applied the same way everywhere, so that callers in one crate agree with signatures in another:
every `&Path`, `Option<&GenericArgs>`, `&GenericArgs`, `&[Option<GenericArgs>]` parameter,
field and return becomes the by-value view, hir-def's `Resolver::resolve_path_in_*` and
`record_path_uses` and hir-ty's path lowering included. Bodies are by-value views
(`store.expr(id) -> Expr<'_>`), so `Expr::Path(p)` binds `p: Path<'_>` and most call sites
keep their text. Also `args.args` is `&[GenericArg]` now, so `for arg in &args.args` becomes
`for arg in args.args` (`&&[T]` is not `IntoIterator`).

The edits assume, from the paths lane: `Expr::Path`/`Pat::Path`/`RecordLit`/`Pat::Record`/
`Pat::TupleStruct` hold `Path<'a>`; `Expr::MethodCall { generic_args: Option<GenericArgs<'a>> }`;
`AsmOperand::Sym(PathIdx)` read with `store.path_at(idx)` (both `pub`); `Index<PathId> for
ExpressionStore` removed; `StoreVisitor::on_path`/`on_generic_args`/`on_type_bound` taking the
views by value; `ExprCollector::lower_path -> Option<PathIdx>`.

Line numbers are as of 20:50; several of these files are being edited, so find each edit by its
old text (where the same text is on several lines, the function named beside it picks).


### crates/blaze-doc/src/sig.rs

Apply in the same edit as `v2/requests/speed.md` "### from speed-hir-store-tools: blaze-doc onto
the type-ref view" (still pending: `match self.store.type_ref(ty)` makes `TypeRef::Path(path)`
bind `Path<'_>` by value, so line 90 and lines 1272-1277 need nothing more). If that block is
not applied, line 90 becomes `self.path(out, *path)` and line 1274 `..._fully(db, *path)?`.

- line 252: `self.path(out, &self.store[*path]);` -> `self.path(out, self.store.path(*path));`
- line 256: `for (at, name) in names.iter().enumerate() {` -> `for (at, name) in self.store.names(*names).iter().enumerate() {`
- line 263: `self.path(out, &self.store[*path]);` -> `self.path(out, self.store.path(*path));`
- line 268: `for (at, arg) in args.iter().enumerate() {` -> `for (at, arg) in self.store.use_args(*args).iter().enumerate() {`
- line 309: `pub(crate) fn path(&self, out: &mut Code, path: &Path) {` -> `pub(crate) fn path(&self, out: &mut Code, path: Path<'_>) {`
- line 312: `let def: Option<ModuleDef> = match *target {` -> `let def: Option<ModuleDef> = match target {`
- line 403: `fn segment(&self, out: &mut Code, ns: TypeNs, name: &str, args: Option<&GenericArgs>) {` -> `fn segment(&self, out: &mut Code, ns: TypeNs, name: &str, args: Option<GenericArgs<'_>>) {`
- line 460: `pub(crate) fn generic_args(&self, out: &mut Code, args: &GenericArgs) {` -> `pub(crate) fn generic_args(&self, out: &mut Code, args: GenericArgs<'_>) {`
- line 516: `if let Some(args) = &binding.args {` -> `if let Some(args) = binding.args {`
- line 664: `for (at, name) in lifetimes.iter().enumerate() {` -> `for (at, name) in self.store.names(*lifetimes).iter().enumerate() {`
  (`self.store` is the generics' store at every `where_clause` call: each `Sig` is built from the
  signature's own `store`)
- line 854: `Some(TypeBound::Path(path, _)) => data.store[*path]` -> `Some(TypeBound::Path(path, _)) => data.store.path(*path)`
- line 1438: `sig.path(&mut out, &data.store[trait_ref.path]);` -> `sig.path(&mut out, data.store.path(trait_ref.path));`

Unchanged and checked: `self.resolver.resolve_path_in_type_ns(db, path)` (line 336, `Path<'_>`
by the signature rule), the `args_and_bindings` reads (338-396, now `Option<GenericArgs<'_>>`
passed straight to `segment`/`generic_args`), `args.bindings.first()`/`is_empty()`/`for binding in
&args.bindings`, `self.bounds(out, &binding.bounds)` (`&&[TypeBound]` coerces), the
`WherePredicate::TypeBound { lifetimes: None, .. }` patterns, `lifetimes.is_none()`.

### crates/hir-ide/src/display.rs

- line 2661: `store[path].hir_fmt(f, owner, store)` -> `store.path(path).hir_fmt(f, owner, store)`
- line 2669: `lifetimes.iter().map(|it| it.display(f.db, edition)).format(", ")` -> `store.names(*lifetimes).iter().map(|it| it.display(f.db, edition)).format(", ")`
- line 2671: `store[*path].hir_fmt(f, owner, store)` -> `store.path(*path).hir_fmt(f, owner, store)`
- line 2665 (first line of the `TypeBound::Use(args)` arm body): `let edition = f.edition();` -> `let edition = f.edition();\n                let args = store.use_args(*args);`
  (as a block:)
  ```old
                write!(f, "use<")?;
                let edition = f.edition();
                let last = args.len().saturating_sub(1);
  ```
  ```new
                write!(f, "use<")?;
                let edition = f.edition();
                let args = store.use_args(*args);
                let last = args.len().saturating_sub(1);
  ```
- line 2693: `impl<'db> HirDisplayWithExpressionStore<'db> for Path {` -> `impl<'db> HirDisplayWithExpressionStore<'db> for Path<'_> {`
- line 2782: `if let Some(ret) = generic_args.bindings[0].type_ref` -> `if let Some(ret) = generic_args.bindings.get(0).unwrap().type_ref`

Unchanged and checked: line 2496 `TypeRef::Path(path) => path.hir_fmt(..)` (the view binds
`Path<'_>`, `hir_fmt` takes `&self`); `seg.args_and_bindings?` and `&generic_args.args[0]` (2731-2732:
`args` is `&'a [GenericArg]`, the reborrow outlives the local); `generic_args.args[0]`,
`&generic_args.args[..]`, `generic_args.bindings.iter()`, `match &binding.type_ref`,
`binding.bounds.iter()`; the `GenericArg` impl (2836).

### crates/hir-ide/src/diagnostics/unsafe_check.rs

- line 411: `fn mark_unsafe_path(&mut self, node: ExprOrPatId, path: &Path) {` -> `fn mark_unsafe_path(&mut self, node: ExprOrPatId, path: Path<'_>) {`

Callers (260 `Pat::Path(path)`, 285 `Expr::Path(path)`) match the by-value `self.body.pat(..)`/
`self.body.expr(..)` views, so `path: Path<'_>` passes as is; 413 passes it to
`resolve_path_in_value_ns(.., path, ..)` (by value under the signature rule).

### crates/hir-ide/src/diagnostics/match_check.rs

- line 278: `fn lower_path(&mut self, pat: PatId, _path: &Path) -> Pat<'db> {` -> `fn lower_path(&mut self, pat: PatId, _path: Path<'_>) -> Pat<'db> {`

(caller 138-139 `hir_def::hir::Pat::Path(path) => return self.lower_path(pat, path)` matches the by-value view.)

### hir-ide, no edit

- `diagnostics/expr.rs`: `Expr::Path(path)` at 302 and 457 bind `Path<'_>` from `self.body.expr(..)`
  and go to `resolve_path_in_value_ns_fully(db, path, ..)` (by value); `Expr::RecordLit`/`MethodCall`
  patterns don't bind the changed fields.
- `impl_db_macro.rs`, `mir_pretty/errors.rs`, `tests/{consteval,layout,mir/eval,next_solver_items}.rs`:
  next-solver `GenericArgs`/`.bindings()` and MIR names only. `tests/consteval/const_ast_digest.rs`:
  `const_ast` syntax views.

Status: open

> applied by requests (child requests-db-fixture), the `crates/hir-ide/` subsections only: `display.rs` all six (`store.path(path)`, `store.names(*lifetimes)`, `store.path(*path)`, `let args = store.use_args(*args);`, `for Path<'_>`, `bindings.get(0).unwrap()`), `diagnostics/unsafe_check.rs` `mark_unsafe_path(.., path: Path<'_>)`. `match_check.rs` skipped: deleted (db-matchck.md item 4). Its `resolve_path_in_value_ns(.., path, ..)` call and `expr.rs`'s rely on db.md's `resolver.rs` subsection (still `&Path` in the tree) in the same compile. `blaze-doc/src/sig.rs` not touched (not hir-ide). Nothing built.

## dedup: blaze-fmt's `Symbol::intern` onto `intern::Symbol`

From `dedup-intern` (report `/tmp/blaze-lanes/v2-dedup-intern.md`). `crates/blaze-fmt/src/span/symbol.rs`
keeps a second process-global text interner: one `parking_lot::Mutex<Option<HashSet<&'static str>>>`
every `Symbol::intern` locks (38 call sites), leaking a `Box<str>` per new text. `intern::Symbol`
is the shared one (sharded, lock-free when found) and hands back the same `&'static str` for
equal text, so blaze-fmt's `Symbol(&'static str)` keeps its type, its text equality and order.

1. `crates/blaze-fmt/Cargo.toml` `[dependencies]`: add `intern.workspace = true`; remove
   `parking_lot = "0.12.5"` (its one use is this interner).
2. `crates/blaze-fmt/src/span/symbol.rs`: delete `use std::collections::HashSet;`,
   `use parking_lot::Mutex;` and `static INTERNER: ...;`, and `Symbol::intern` becomes:
```rust
    pub fn intern(s: &str) -> Symbol {
        Symbol(intern::Symbol::intern(s).as_str())
    }
```

> forwarded to the coordinator (from requests, pass 16; on requests' coordinator list).

## dedup: `blaze-lints` uses `blaze-diagnostics`' diagnostic model, `Level`, `CommandLine` and emitter (from dedup-scan-diag)

From `dedup-scan-diag` (report `/tmp/blaze-lanes/v2-dedup-scan-diag.md`, notes
`docs/v2/plan/lanes/dedup/scan-diag.md`). The files are `lints-driver`'s (lane `lints`), which is
rewriting `diag.rs`, `level.rs`, `check.rs` and `serve.rs` now: apply this with or after that
rewrite, keeping its rules where its code has moved. Land it after `v2/requests/diagnostics.md`
`## dedup: one diagnostic model, `Level` and emitter, shared with `blaze-lints`` parts 1 and 2
(`Diag::is_fixable`, `emit::json_rendered`).

Why: `diag.rs` 13-235 is `blaze_diagnostics::model` 68-294 line for line; `emit.rs` is
`blaze_diagnostics::emit` (an older rustc's, without BOM and `\r\n`, recursive macro calls, the
def-site rule of macro labels; it writes the "originates in" note as a child, so the JSON has a
child rustc's does not; it writes cargo's key order even under `ansi`, where cargo passes rustc's
line as it is); `level::Level` is `blaze_diagnostics::lints::Level` (whose `of_cap` is rustc's
`Level::from_str`: no `force-warn`, which `blaze-lints`' took); `level::CommandLine::parse` is
`blaze_diagnostics::lints::CommandLine::parse` (whose errors are rustc's words). No cycle:
`blaze-diagnostics` depends on neither `blaze-lints` nor anything that does.

> forwarded to lints (holds `crates/blaze-lints/`) (from requests, pass 16; told by tmux).
> applied by lints, 2026-10-29 (see Status under part 8).

### 1. `crates/blaze-lints/Cargo.toml`

Add `blaze-diagnostics.workspace = true` to `[dependencies]`; remove the line
`annotate-snippets = "=0.12.16"` and the two comment lines above it (only `emit.rs` used it).

> forwarded to lints (holds `crates/blaze-lints/`) (from requests, pass 16; told by tmux).
> applied by lints, 2026-10-29 (see Status under part 8).

### 2. `crates/blaze-lints/src/diag.rs`: the whole file becomes

```rust
//! A lint's diagnostic as data: [`blaze_diagnostics::model`]'s, as every diagnostic blaze gives
//! is, and the naming of the spans a lint query makes. It crosses the daemon's socket as it is,
//! so the source it points into is read where it is written, by its path.

use syntax::TextRange;

pub use blaze_diagnostics::model::{
    Applicability, Child, Code, Diag, Expansion, MacroKind, MultiSpan, Span, SpanLabel, Sublevel,
    Suggestion, SuggestionStyle,
};

/// What a span made in a lint query has for its file, before the driver names it: the empty
/// name for the tree the query walks (its range moved into the file by the driver), or this
/// byte and the file's number for a span of another file.
const FILE_ID: char = '\0';

/// A span of the tree a lint query walks, which the driver moves into the file and names.
pub(crate) fn in_query(range: TextRange) -> Span {
    Span { file: String::new(), lo: range.start().into(), hi: range.end().into(), expansion: None }
}

/// A span of `file` as a lint query makes it, which the driver names.
pub(crate) fn of_file(file: span::FileId, range: TextRange) -> Span {
    let mut name = String::with_capacity(11);
    name.push(FILE_ID);
    name.push_str(&file.index().to_string());
    Span { file: name, lo: range.start().into(), hi: range.end().into(), expansion: None }
}

/// Names the files of every span of `diag` a lint query made ([`place`]). `false` when one of
/// them is of a file that is not named.
pub(crate) fn place_diag(
    diag: &mut Diag,
    offset: u32,
    name: &str,
    name_of: &dyn Fn(span::FileId) -> Option<String>,
) -> bool {
    let mut named = place_multi(&mut diag.span, offset, name, name_of);
    for child in &mut diag.children {
        named &= place_multi(&mut child.span, offset, name, name_of);
    }
    for suggestion in &mut diag.suggestions {
        for (span, _) in suggestion.substitutions.iter_mut().flatten() {
            named &= place(span, offset, name, name_of);
        }
    }
    named
}

fn place_multi(
    span: &mut MultiSpan,
    offset: u32,
    name: &str,
    name_of: &dyn Fn(span::FileId) -> Option<String>,
) -> bool {
    let mut named = true;
    for span in span.primary.iter_mut().chain(span.labels.iter_mut().map(|it| &mut it.0)) {
        named &= place(span, offset, name, name_of);
    }
    named
}

/// Gives a span a lint query made its file's name, moving it by `offset` when it is of the
/// tree the query walked, named `name`. `false` when a file is not named.
pub(crate) fn place(
    span: &mut Span,
    offset: u32,
    name: &str,
    name_of: &dyn Fn(span::FileId) -> Option<String>,
) -> bool {
    if span.file.is_empty() {
        span.lo += offset;
        span.hi += offset;
        name.clone_into(&mut span.file);
    } else if let Some(id) = span.file.strip_prefix(FILE_ID) {
        let Some(file) = id.parse().ok().map(span::FileId::from_raw).and_then(name_of) else {
            return false;
        };
        span.file = file;
    }
    match &mut span.expansion {
        Some(expansion) => place_expansion(expansion, offset, name, name_of),
        None => true,
    }
}

fn place_expansion(
    expansion: &mut Expansion,
    offset: u32,
    name: &str,
    name_of: &dyn Fn(span::FileId) -> Option<String>,
) -> bool {
    let def_site = expansion.def_site.as_mut().is_none_or(|it| place(it, offset, name, name_of));
    place(&mut expansion.call_site, offset, name, name_of) && def_site
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_span_of_the_query_is_moved_and_named_and_one_of_another_file_is_named() {
        let range = TextRange::new(2.into(), 5.into());
        let mut diag = Diag::lint("clippy::needless_bool", "x", in_query(range));
        diag.span_label(of_file(span::FileId::from_raw(7), range), "there");
        let name_of = |file: span::FileId| (file.index() == 7).then(|| "src/other.rs".to_owned());
        assert!(place_diag(&mut diag, 100, "src/lib.rs", &name_of));
        assert_eq!((diag.span.primary[0].file.as_str(), diag.span.primary[0].lo, diag.span.primary[0].hi), ("src/lib.rs", 102, 105));
        assert_eq!((diag.span.labels[0].0.file.as_str(), diag.span.labels[0].0.lo), ("src/other.rs", 2));
        let mut unnamed = Diag::lint("clippy::needless_bool", "x", of_file(span::FileId::from_raw(8), range));
        assert!(!place_diag(&mut unnamed, 0, "src/lib.rs", &name_of));
    }
}
```

Every `diagnostic.place(..)`, `span.place(..)`, `Span::in_query(..)` and `Span::of_file(..)` the
rewrite writes becomes `diag::place_diag(&mut diagnostic, ..)`, `diag::place(&mut span, ..)`,
`diag::in_query(..)`, `diag::of_file(..)`: methods can't be added to `blaze-diagnostics`' types
here, and this naming is the lint queries' own, not the model's. `Diag` derives `Eq` and `Hash`,
so the queries' values still backdate.

> forwarded to lints (holds `crates/blaze-lints/`) (from requests, pass 16; told by tmux).
> applied by lints, 2026-10-29 (see Status under part 8).

### 3. `crates/blaze-lints/src/emit.rs`: delete the file

`lib.rs`: delete `pub mod emit;`, and in its doc comment `[`diag`] is a diagnostic as data and
[`emit`] writes it as rustc does, in text and in JSON.` becomes `[`diag`] is a diagnostic as
data, written by [`blaze_diagnostics::emit`] as rustc writes it, in text and in JSON.` Nothing
outside the crate names `blaze_lints::emit` or `blaze_lints::diag`.

> forwarded to lints (holds `crates/blaze-lints/`) (from requests, pass 16; told by tmux).
> applied by lints, 2026-10-29 (see Status under part 8).

### 4. `crates/blaze-lints/src/level.rs`

- Delete `pub enum Level` and its `impl Level` (the doc comment `/// A lint level, in rustc's
  order: ...` to the end of `fn of_cap`'s `impl`) and put `pub use blaze_diagnostics::lints::Level;`
  after the `use` lines.
- Delete `pub struct CommandLine` and its `impl CommandLine` (`parse` and `extend`; nothing calls
  `extend`) and put `pub use blaze_diagnostics::lints::CommandLine;` beside the other `pub use`.
- `of_manifest`'s last line `CommandLine { opts: opts.collect(), cap: None }` becomes
  `CommandLine { opts: opts.collect(), ..CommandLine::default() }`.
- `diag::{Diagnostic, MultiSpan, Span, Sublevel}` becomes `diag::{Diag, MultiSpan, Span, Sublevel}`,
  and `pub fn explain(diagnostic: &mut Diagnostic, ...)` takes `diagnostic: &mut Diag`.
- In `the_notes_rustc_adds`, `let mut diagnostic = Diagnostic { lint: ..., level: spec.level, ... };`
  (the whole literal) becomes
  `let mut diagnostic = Diag::new(blaze_diagnostics::model::Level::Warning, None, String::new());`.
- Add to `mod tests` (the reference of `Level`'s names as this file had them, against the
  perfect hash `blaze-diagnostics` names them by; its `of_cap` differs on purpose):

```rust
    /// `Level`'s names as this crate wrote them, by string `match`.
    fn reference_of_attr(name: &str) -> Option<Level> {
        Some(match name {
            "allow" => Level::Allow,
            "expect" => Level::Expect,
            "warn" => Level::Warn,
            "deny" => Level::Deny,
            "forbid" => Level::Forbid,
            _ => return None,
        })
    }

    proptest::proptest! {
        #[test]
        fn levels_are_named_as_the_reference_named_them(
            name in "(allow|expect|warn|force-warn|deny|forbid|[a-z-]{0,12})",
        ) {
            proptest::prop_assert_eq!(Level::of_attr(&name), reference_of_attr(&name));
            let cap = reference_of_attr(&name).filter(|it| *it != Level::Expect);
            proptest::prop_assert_eq!(Level::of_cap(&name), cap);
        }
    }

    #[test]
    fn cap_lints_takes_rustcs_levels_only() {
        assert_eq!(Level::of_cap("force-warn"), None);
        assert_eq!(Level::of_cap("expect"), None);
        assert!(CommandLine::parse(&args("--cap-lints force-warn")).is_err());
        for level in [Level::Allow, Level::Expect, Level::Warn, Level::ForceWarn, Level::Deny, Level::Forbid] {
            let flag = match level {
                Level::Allow => "-A",
                Level::Expect | Level::Warn => "-W",
                Level::ForceWarn => "--force-warn",
                Level::Deny => "-D",
                Level::Forbid => "-F",
            };
            assert_eq!(level.to_cmd_flag(), flag);
            assert_eq!(Level::of_attr(level.as_str()), (level != Level::ForceWarn).then_some(level));
        }
    }
```

  and `proptest.workspace = true` to `Cargo.toml`'s `[dev-dependencies]`.

> forwarded to lints (holds `crates/blaze-lints/`) (from requests, pass 16; told by tmux).
> applied by lints, 2026-10-29 (see Status under part 8).

### 5. `crates/blaze-lints/src/check.rs`

- `diag::{Diagnostic, Expansion, MacroKind, MultiSpan, Span, Sublevel}` becomes
  `diag::{Code, Diag, Expansion, MacroKind, MultiSpan, Span, Sublevel}`; add
  `use blaze_diagnostics::{cx::{item_range, signature_range}, model};`.
- `Found::diagnostic`, `Pending::diagnostic` and every `-> &mut Diagnostic` are `Diag`.
- In `lint_placed`, the literal `diagnostic: Diagnostic { lint: format!("clippy::{}", info.name),
  level: Level::Warn, message: message.into(), span, children: Vec::new(), suggestions:
  Vec::new(), suggestions_disabled: false }` becomes (the rewrite's name for a `blaze::` lint
  goes where `format!("clippy::{}", info.name)` is):

```rust
            diagnostic: {
                let code = Code::Lint(format!("clippy::{}", info.name));
                let mut diag = Diag::new(model::Level::Warning, Some(code), message);
                diag.span = span;
                diag
            },
```

- In `finish`, `diagnostic.level = spec.level;` becomes `diagnostic.level = written_at(spec.level);`;
  `originates_in(&mut diagnostic);` is deleted (the emitter writes the note, and leaves it out of
  the JSON as rustc does); the condition
  `.any(|it| it.expansion.as_ref().is_some_and(|it| it.def_site.is_some()))` becomes
  `.any(|it| it.expansion.as_ref().is_some_and(|it| it.kind != MacroKind::Bang))` (rustc's
  `Span::in_external_macro`: in an attribute or derive macro's expansion always; the walk makes
  expansions of derives only. With part 3 below every derive has a def site, so the old test
  would no longer pick the proc macros out; this is the rule itself, and it also turns them off
  in `core`'s derives, as rustc does).
- The unfulfilled expectation's literal `let mut diagnostic = Diagnostic { lint:
  "unfulfilled_lint_expectations".to_owned(), level: spec.level, message: "this lint expectation
  is unfulfilled".to_owned(), span: MultiSpan::of(expectation.span), children: Vec::new(),
  suggestions: Vec::new(), suggestions_disabled: false };` becomes

```rust
            let code = Code::Lint("unfulfilled_lint_expectations".to_owned());
            let mut diagnostic = Diag::new(written_at(spec.level), Some(code), "this lint expectation is unfulfilled");
            diagnostic.span = MultiSpan::of(expectation.span);
```

- Delete `fn originates_in` (its doc comment `/// rustc's note on a diagnostic in a macro's
  expansion: which macro it came from.` to its `}`: `blaze_diagnostics::model::Diag::origin_note`,
  fixed to rustc's backtrace there). Add:

```rust
/// The level a diagnostic of a lint at `level` is written at.
fn written_at(level: Level) -> model::Level {
    match level.is_error() {
        true => model::Level::Error,
        false => model::Level::Warning,
    }
}
```

- `expansion_of`: every macro has its definition as its def site (rustc's `ExpnData::def_site`;
  `blaze_diagnostics::emit` labels only an expansion that has one, as rustc skips a dummy one, so
  without this `#[derive(Clone)]` loses its "in this derive macro expansion"). Before:

```rust
        let def_site = match loc.def.kind {
            MacroDefKind::ProcMacro(id, ..) => {
                let file = id.file_id.file_id()?;
                let name = (self.state.name_of)(file.file_id(db))?;
                let item = id.to_node(db);
                let range = signature_range(&item);
                Some(Span::new(&name, range))
            }
            _ => None,
        };
```

  after:

```rust
        let named = |file: HirFileId, range: TextRange| {
            let name = (self.state.name_of)(file.file_id()?.file_id(db))?;
            Some(Span::new(&name, range))
        };
        let def_site = match loc.def.kind {
            MacroDefKind::ProcMacro(id, ..) => Some(named(id.file_id, signature_range(&id.to_node(db)))?),
            MacroDefKind::Declarative(id, _)
            | MacroDefKind::BuiltIn(id, _)
            | MacroDefKind::BuiltInAttr(id, _)
            | MacroDefKind::BuiltInDerive(id, _)
            | MacroDefKind::BuiltInEager(id, _)
            | MacroDefKind::UnimplementedBuiltIn(id) => named(id.file_id, item_range(id.to_node(db).syntax())),
        };
```

- Delete this file's `fn item_range` and `fn signature_range` (`blaze_diagnostics::cx`'s are the
  same lines, `check.rs` 911-942 against `cx.rs` 297-327) and `fn derive_paths` (`cx`'s gives the
  same paths and ranges); `lints/empty_enums.rs`'s `crate::check::item_range` becomes
  `blaze_diagnostics::cx::item_range`. If the rewrite moves `item_range` onto `syntax::view`
  nodes (`docs/v2/plan/lanes/lints/cx.md`), that one is of another tree and stays, under its
  own name (`view_item_range`), and only the `SyntaxNode` ones go.

> forwarded to lints (holds `crates/blaze-lints/`) (from requests, pass 16; told by tmux).
> applied by lints, 2026-10-29 (see Status under part 8).

### 6. `crates/blaze-lints/src/serve.rs`

- The `use crate::{...}` drops `Level`, `diag::Diagnostic` and `emit::{self, Sources, Style}`;
  add `use blaze_diagnostics::{emit::{self, Sources, Style}, model::{Child, Diag}};`.
- `let style = Style { short: ask.short, ansi: ask.ansi, width: ask.width.unwrap_or(emit::WIDTH) };`
  becomes `let style = Style { short: ask.short, ansi: ask.ansi, width: ask.width.unwrap_or(emit::WIDTH), ui_testing: false };`.
- `fn written` becomes:

```rust
fn written<'a>(
    found: impl Iterator<Item = &'a Found>,
    sources: &mut Sources,
    style: Style,
) -> Vec<Message> {
    let mut once: FxHashSet<Child> = FxHashSet::default();
    let mut seen = FxHashSet::default();
    let mut messages = Vec::new();
    for Found { diagnostic, .. } in found {
        let mut diagnostic: Diag = diagnostic.clone();
        diagnostic.children.retain(|child| !child.once || once.insert(child.clone()));
        let rendered = emit::written(&diagnostic, sources, style);
        if !seen.insert(rendered.clone()) {
            continue;
        }
        let json = emit::json_rendered(&diagnostic, sources, rendered.clone());
        messages.push(Message {
            json: if style.ansi { json } else { cargo_order(&json) },
            rendered,
            error: diagnostic.is_error(),
            fixable: diagnostic.is_fixable(),
        });
    }
    messages
}

/// rustc's JSON line `line` as cargo writes it again when it takes the colours out of
/// `rendered` (`on_stderr_line_inner` under `MessageFormat::Json`): `rendered`, every other key
/// but `code` sorted at every depth, then `code`. With colours cargo passes rustc's line as it
/// is.
fn cargo_order(line: &str) -> String {
    #[derive(Deserialize, Serialize)]
    struct CompilerMessage {
        rendered: String,
        #[serde(flatten)]
        other: std::collections::BTreeMap<String, serde_json::Value>,
        code: Option<serde_json::Value>,
    }
    let mut message: CompilerMessage = serde_json::from_str(line)
        .unwrap_or_else(|error| panic!("blaze-lints: rustc's JSON of a diagnostic: {error}"));
    message.other.values_mut().for_each(sort_keys);
    message.code.iter_mut().for_each(sort_keys);
    serde_json::to_string(&message)
        .unwrap_or_else(|error| panic!("blaze-lints: a diagnostic as cargo writes it: {error}"))
}

/// Sorts the keys of every object in `value`, as serde_json's `Map` without `preserve_order`
/// has them, whatever features the build unifies.
fn sort_keys(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            let mut entries: Vec<(String, serde_json::Value)> = std::mem::take(map).into_iter().collect();
            entries.sort_unstable_by(|a, b| a.0.cmp(&b.0));
            for (key, mut it) in entries {
                sort_keys(&mut it);
                map.insert(key, it);
            }
        }
        serde_json::Value::Array(items) => items.iter_mut().for_each(sort_keys),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_diagnostic_is_written_as_cargo_writes_it_again() {
        let line = concat!(
            r#"{"$message_type":"diagnostic","message":"m","code":{"code":"clippy::x","explanation":null},"#,
            r#""level":"warning","spans":[{"file_name":"a.rs","byte_start":0,"expansion":null}],"children":[],"rendered":"r"}"#,
        );
        let expected = concat!(
            r#"{"rendered":"r","$message_type":"diagnostic","children":[],"level":"warning","message":"m","#,
            r#""spans":[{"byte_start":0,"expansion":null,"file_name":"a.rs"}],"code":{"code":"clippy::x","explanation":null}}"#,
        );
        assert_eq!(cargo_order(line), expected);
    }
}
```

  (`once` keys on the whole `Child`, as `blaze_diagnostics::check::finish` does, in place of its
  `format!("{:?}", child.span)` key; every child kept by it has `once` set, so the set is the
  same. `cargo_order` is cargo's rule from `/tmp/cargo-src/src_compiler_mod.rs`: `code` is a field
  of its own after the flattened map, so it comes last.)

> forwarded to lints (holds `crates/blaze-lints/`) (from requests, pass 16; told by tmux).
> applied by lints, 2026-10-29 (see Status under part 8).

### 7. `crates/blaze-lints/src/tests.rs`

`emit::{self, Sources, Style},` leaves the `use crate::{...}`; add
`use blaze_diagnostics::emit::{self, Sources, Style};`. The expectations stay as they are: none
has a macro expansion, and on what they have the two emitters write the same bytes
(`blaze_diagnostics::emit::reference`'s proptest).

> forwarded to lints (holds `crates/blaze-lints/`) (from requests, pass 16; told by tmux).
> applied by lints, 2026-10-29 (see Status under part 8).

### 8. `docs/v2/plan/lanes/lints/cx.md`

Each `-> &mut Diagnostic` becomes `-> &mut Diag`, and "`Diagnostic`, `MultiSpan`, `Span`,
`Applicability` and their builders" becomes "`Diag` (`blaze_diagnostics::model`'s, re-exported
by `crate::diag`), `MultiSpan`, `Span`, `Applicability` and their builders". The builders the
lint files call (`suggestion`, `multipart_suggestion`, `suggestion_with_style`, `help`, `note`,
`span_note`, `span_help`, `span_label`, `sub`) have the same signatures on `Diag` (`span_note`
and `span_help` take `impl Into<MultiSpan>`, which a `Span` is).

Status: applied by lane `lints`, 2026-10-29. Parts 1-4 and 6-8 as written. In `serve.rs`,
`blaze_diagnostics::emit::Sources` has no reading constructor, so `written` also takes a `read`
function and gives `Sources::add` each file `Sources::unread` names before it writes a build.
`check.rs` is on views, so its `item_range`, `signature_range` and `derive_paths` stay, with
no `SyntaxNode` copies to delete. `blaze_diagnostics::cx` isn't imported, `empty_enums` keeps
`crate::check::item_range`, and a declarative or built-in macro's def site is
`item_range(self.node_of(id)?)`, which is `None` (not the whole expansion) when its node isn't
found.

> forwarded to lints (holds `crates/blaze-lints/`) (from requests, pass 16; told by tmux).
> applied by lints, 2026-10-29 (see Status under part 8).

## dedup: the v1 cargo crates call blaze-units' copies (from dedup-scan-cargo, 2026-10-29)

The v1 half of `v2/requests/cargo.md` "dedup: one copy of each piece the load ported from v1's
cargo crates": `crates/blaze-units/src/load/` ported pieces of `blaze-metadata`, `blaze-registry`
and `blaze-resolve` it could not call (blaze-metadata depends on blaze-units). Once lane cargo's
half makes them public and ungated in `blaze-units`, the v1 copies go. These are the
coordinator's paths today; lane cargo's request 1 (the grant of the five cargo crates) would make
all of this lane cargo's, and it is fine for lane cargo to do both halves once granted. Items 4
and 5 do not wait on lane cargo. No dependency cycle: `blaze-units` (without `query`) depends on
`blaze-manifest`, `blaze-resolve`, `blaze-source` only.

> forwarded to the coordinator ("every other path"; lane cargo once request 1's grant lands) (from requests, pass 16; on requests' coordinator list).

### 1. `blaze-metadata/src/summary.rs` goes (lane cargo item 1)

`crates/blaze-metadata/src/lib.rs`:

- line 17 `mod summary;` goes; add `use blaze_units::read as summary;`.
- line 44 `pub use crate::summary::Read;` becomes `pub use blaze_units::Read;`;
  `graph.rs:11` `use crate::summary::Read;` becomes `use blaze_units::Read;`.
- the root manifest is read once and handed over as text. Add

  ```rust
  /// The text of the root manifest of the workspace at `root`.
  fn root_text(root: &Path) -> Result<String> {
      let path = root.join("Cargo.toml");
      std::fs::read_to_string(&path).with_context(|| path.display().to_string())
  }
  ```
  and at line 607 `let replace = summary::replace_table(&root)?;` becomes

  ```rust
  let root_text = root_text(&root)?;
  let root_table = summary::Root::new(&root, &root_text)?;
  let replace = summary::replace_table(&root_table)?;
  ```
  with line 662 `summary::patch_tables(&root)?` becoming `summary::patch_tables(&root_table)?`.
- `path_packages` (line 946): its first two lines become

  ```rust
  let root_text = root_text(root)?;
  let root_table = summary::Root::new(root, &root_text)?;
  let mut queue = summary::patch_paths(&root_table);
  queue.extend(summary::replace_paths(&summary::replace_table(&root_table)?));
  ```
  and its two `path_dependencies(...)` calls `summary::path_dependencies(...)`; delete
  `fn path_dependencies` (974-980).
- delete `fn manifest_source` (1218-1240) and `fn rust_versions` (1079-1101); line 1020 calls
  `summary::manifest_source(&source_text, &ctx.config)?`, line 694
  `summary::rust_versions(&ctx, behavior, &member_rust_versions, asked.rustc_version)?` (the union
  takes the release before a space, so a bare version reads as before).
- delete `crates/blaze-metadata/src/summary.rs`. Its tests not already in `blaze-units/src/read.rs`
  move there (lane cargo item 1 lists them).

> forwarded to the coordinator ("every other path"; lane cargo once request 1's grant lands) (from requests, pass 16; on requests' coordinator list).

### 2. `blaze-resolve/examples/metadata_resolve.rs` moves to blaze-units (lane cargo item 1)

Delete `crates/blaze-resolve/examples/metadata_resolve.rs` once lane cargo has written
`crates/blaze-units/examples/metadata_resolve.rs`; in `harness/resolve_graph.py` 84-85 the build's
`"-p", "blaze-resolve"` becomes `"-p", "blaze-units"` and the docstring's "The `metadata_resolve`
example of blaze-resolve" (line 11) names blaze-units; the binary path (line 86) is unchanged.

> forwarded to the coordinator ("every other path"; lane cargo once request 1's grant lands) (from requests, pass 16; on requests' coordinator list).

### 3. blaze-registry on `blaze_units::{index, sources}` (lane cargo items 2, 3)

`crates/blaze-registry/Cargo.toml` += `blaze-units.workspace = true`.

- `src/index.rs`: delete 19-398 but `CACHE_VERSION` (21) (that is, `INDEX_V_MAX`, `IndexEntry`,
  `Status`, `IndexLine`, `DepKind`, `IndexDependency`, `Artifact`, `RustVersion` and both impls,
  `IndexPackage`, `RegistryDependency`, `yes`, `rust_version`, `pubtime`, `is_pubtime`, the
  `IndexPackage*` probes, `parse_line`, `entry`, `dependency`, `artifact`, `dep_path`), and add
  `pub use blaze_units::index::{Artifact, INDEX_V_MAX, IndexDependency, IndexEntry, IndexLine, Status, dep_path, parse_line};`.
  `Summaries`, `Split` and `cache_round_trip` stay; the tests `paths`, `lines` and
  `crates_io_drops_its_own_registry` move to blaze-units (lane cargo item 2).
- `src/query.rs`: delete `index_summary`, `summary` and `dependency` (25-97) and
  `pub use blaze_units::index::index_summary;`; `describe` (275-292) goes, and
  `describe_source`/`is_replaced` (260-274) become `self.sources.map().describe(source)` and
  `self.sources.map().is_replaced(source)`. The `index::DepKind` match goes with `dependency`
  (blaze_units' entries carry blaze-resolve's `DepKind`).
- `src/download.rs`: `crate_url` (93-107) becomes `pub use blaze_units::index::crate_url;`.
- `src/sources.rs`: `Replacement`, `SourceMap` and its impl, `supports_checksums`,
  `requires_precise` (44-219) go; `pub use blaze_units::sources::SourceMap;`; the constructor
  becomes a free function with v1's reading of a `[source]` table:

  ```rust
  /// The map `config` makes, as cargo's `SourceConfigMap::new`.
  pub fn source_map(config: &SourceConfig) -> anyhow::Result<SourceMap> {
      let crates_io =
          if config.crates_io_git { SourceId::crates_io() } else { SourceId::crates_io_sparse() };
      let mut map = SourceMap::new(crates_io, config.registries.clone())?;
      for (name, def) in &config.sources {
          add_def(&mut map, name, def)?;
      }
      Ok(map)
  }
  ```
  where `add_def(map: &mut SourceMap, name, def)` is v1's `SourceMap::add_def` (91-142) with
  `self.crates_io` as `map.crates_io()` and the last line
  `map.add(name, id, def.replace_with.clone().map(|it| (it, def.definition.clone())))`.
  `Sources::new` (240) and the tests' `map` (349) and `crates_io_is_read_sparse_unless_told` (375)
  call `source_map(...)`. The test at 495 decides
  which order the doubly-defined-source error names its two sources in: see lane cargo item 3,
  "Which name comes first" (v1 and v2 disagree; cargo's is the one).
- `src/git.rs:658`: delete `normalize` and call `blaze_manifest::paths::normalize_path`.
- `src/lib.rs:34`: `SourceMap` is still exported (now blaze-units').

> forwarded to the coordinator ("every other path"; lane cargo once request 1's grant lands) (from requests, pass 16; on requests' coordinator list).

### 4. blaze-manifest: one feature map, one `CrateType`/`TargetKind`

Root `Cargo.toml` `[workspace.dependencies]` += `blaze-resolve = { path = "crates/blaze-resolve" }`
(today each user names the path); `crates/blaze-manifest/Cargo.toml` += `blaze-resolve.workspace = true`.

**Feature map.** `blaze-manifest/src/dependency.rs` 189-337 (`build_summary_features`) and
`blaze-resolve/src/summary.rs` 182-325 (`build_feature_map`) are cargo's `build_feature_map`
twice, one over `String`s and one over `InternedString`s. blaze-resolve's is right where they
differ: the manifest's copy finds the optional dependency no feature names by iterating an
`FxHashMap` (`dep_map.iter().find(...)`, 327), so which one its error names depends on hashing;
cargo and blaze-resolve take the first in `[dependencies]` order.

- `crates/blaze-resolve/src/summary.rs`: `build_feature_map` becomes public over what it reads of a
  dependency:

  ```rust
  /// What the feature map reads of a dependency.
  #[derive(Clone, Copy, Debug)]
  pub struct FeatureDep {
      /// The key of its `[dependencies]` entry.
      pub name_in_toml: InternedString,
      pub optional: bool,
  }

  /// cargo's `build_feature_map`: the declared features, plus the implicit feature of each
  /// optional dependency no `dep:` names, with cargo's checks and messages. `dependencies` is
  /// walked in its order, which decides which unused optional dependency an error names.
  pub fn build_feature_map<I>(
      features: &BTreeMap<InternedString, Vec<InternedString>>,
      dependencies: I,
  ) -> Result<FeatureMap>
  where
      I: Iterator<Item = FeatureDep> + Clone,
  ```
  with the three walks `for dep in dependencies.clone()`, `dep.name_in_toml`/`dep.optional` for
  `dep.name_in_toml()`/`dep.is_optional()`, and the last `dependencies.map(|d| d.name_in_toml).find(...)`.
  `Summary::new` calls
  `build_feature_map(features, dependencies.iter().map(|d| FeatureDep { name_in_toml: d.name_in_toml(), optional: d.is_optional() }))?`.
  `lib.rs:52` exports `FeatureDep` and `build_feature_map`.
- `crates/blaze-manifest/src/dependency.rs`: delete `FeatureValue` and its impls and `FeatureMap`
  (135-189), and `pub use blaze_resolve::{FeatureMap, FeatureValue};`. `build_summary_features`
  keeps its dev-dependency check and then is

  ```rust
  let features: BTreeMap<InternedString, Vec<InternedString>> = features
      .iter()
      .map(|(name, list)| (InternedString::new(name), list.iter().map(|it| InternedString::new(it)).collect()))
      .collect();
  let deps = dependencies
      .iter()
      .map(|dep| FeatureDep { name_in_toml: InternedString::new(dep.name_in_toml()), optional: dep.optional });
  blaze_resolve::build_feature_map(&features, deps)
  ```
  (`use blaze_resolve::{FeatureDep, InternedString};`; `closest_msg`, `FeatureName`, `FxHashSet`
  imports go if nothing else uses them). Feature names are then checked by blaze-resolve's port
  (`names.rs` `validate_feature_name`) instead of cargo-util-schemas' `FeatureName::new`: the same
  rule and messages, which `harness/resolve.py` holds to cargo.
- `Manifest.features` is then `BTreeMap<InternedString, Vec<FeatureValue>>`: `manifest.rs:1336`
  (`contains_key("default-features")`, through `Borrow<str>`) and `metadata.rs:104` compile as they
  are; `blaze-units/src/load/plan/flags.rs:99` `.map(String::as_str)` becomes
  `.map(|it| it.as_str())` (lane cargo's file; told in cargo.md).

**`CrateType`, `TargetKind`.** `blaze-units/src/load/plan/hash.rs` mirrors both for cargo's
`-C metadata` hash, with the methods cargo has on them; blaze-manifest's get those methods and
hash.rs uses blaze-manifest's (lane cargo item 4). In `crates/blaze-manifest/src/targets.rs`:

```rust
impl CrateType {
    // after as_str:
    pub fn can_lto(&self) -> bool {
        matches!(self, CrateType::Bin | CrateType::Staticlib | CrateType::Cdylib)
    }

    pub fn is_linkable(&self) -> bool {
        matches!(self, CrateType::Lib | CrateType::Rlib | CrateType::Dylib | CrateType::ProcMacro)
    }

    pub fn is_dynamic(&self) -> bool {
        matches!(self, CrateType::Dylib | CrateType::Cdylib | CrateType::ProcMacro)
    }

    pub fn requires_upstream_objects(&self) -> bool {
        !matches!(self, CrateType::Lib | CrateType::Rlib)
    }
}

impl TargetKind {
    // after rustc_crate_types:
    pub fn is_bin(&self) -> bool {
        matches!(self, TargetKind::Bin)
    }

    pub fn is_example(&self) -> bool {
        matches!(self, TargetKind::ExampleLib(_) | TargetKind::ExampleBin)
    }

    pub fn is_executable(&self) -> bool {
        matches!(self, TargetKind::Bin | TargetKind::ExampleBin)
    }

    pub fn is_custom_build(&self) -> bool {
        matches!(self, TargetKind::CustomBuild)
    }

    pub fn is_test_or_bench(&self) -> bool {
        matches!(self, TargetKind::Test | TargetKind::Bench)
    }

    fn lib_has(&self, crate_type: &CrateType) -> bool {
        matches!(self, TargetKind::Lib(types) if types.contains(crate_type))
    }

    pub fn is_dylib(&self) -> bool {
        self.lib_has(&CrateType::Dylib)
    }

    pub fn is_cdylib(&self) -> bool {
        self.lib_has(&CrateType::Cdylib)
    }

    /// cargo's `Target::for_host`: a build script, or a library that is a proc macro.
    pub fn for_host(&self) -> bool {
        self.is_custom_build() || self.lib_has(&CrateType::ProcMacro)
    }
}
```
Both enums already derive `Clone, Debug, PartialEq, Eq, Hash` with cargo's variants in cargo's
order, which is what the hash needs.

> held by requests: the root line `blaze-resolve = { path = "crates/blaze-resolve" }` is in (after `blaze-metadata`; already in the lock). The `blaze-manifest`/`blaze-resolve` edits are the coordinator's ("every other path", lane cargo once request 1's grant lands): on requests' coordinator list.
> forwarded to the coordinator ("every other path"; lane cargo once request 1's grant lands) (from requests, pass 16; on requests' coordinator list).

### 5. blaze-registry's `LimitErrorReader` public (lane cargo item 9)

`crates/blaze-registry/src/download.rs:347`: `struct LimitErrorReader<R> { inner: R, left: u64 }`
becomes `pub struct LimitErrorReader<R> { pub inner: R, pub left: u64 }`, so
`blaze-cargo/src/serve/fetch/archive.rs` drops its copy.

> forwarded to the coordinator ("every other path"; lane cargo once request 1's grant lands) (from requests, pass 16; on requests' coordinator list).

### 6. `harness/answers.py` writes the one `rustc.json` (lane cargo item 6)

`harness/answers.py:26`: `DATA = os.path.join(ROOT, "crates/blaze-units/src/stdlib", "rustc.json")`,
and line 10's text names that path; lane cargo deletes `crates/blaze-cargo/src/answers/rustc.json`.

> forwarded to the coordinator ("every other path"; lane cargo once request 1's grant lands) (from requests, pass 16; on requests' coordinator list).

### 7. Found, not requested here

- `crates/blaze-new/src/lib.rs:691` `normalize_path` drops a relative path's leading `..`, unlike
  cargo-util's (`blaze_manifest::paths::normalize_path`); blaze-new does not depend on
  blaze-manifest. If that dependency is fine for `cargo new`, it should call that one.
- blaze-manifest has its own `source::SourceId` beside blaze-source's (`load/source.rs`
  `source_id` converts one to the other); one `SourceId` is the larger dedup behind several of
  these, for after request 1.

Status: open

> forwarded to the coordinator ("every other path"; lane cargo once request 1's grant lands) (from requests, pass 16; on requests' coordinator list).

## dedup: test_utils::rust_files (from dedup-scan)

Seven test-only copies list the `.rs` files under a directory (callers in frontend-tree.md).
`crates/test-utils/src/lib.rs` gains:
```rust
/// Every `.rs` file under `dir`, recursively, sorted. Panics on a directory it can't read.
pub fn rust_files(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut dirs = vec![dir.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        let entries = std::fs::read_dir(&dir)
            .unwrap_or_else(|err| panic!("can't read {}: {err}", dir.display()));
        for entry in entries {
            let path = entry.unwrap_or_else(|err| panic!("can't read {}: {err}", dir.display())).path();
            if path.is_dir() {
                dirs.push(path);
            } else if path.extension().is_some_and(|it| it == "rs") {
                files.push(path);
            }
        }
    }
    files.sort();
    files
}
```
(with `std::path::{Path, PathBuf}` imported). test-utils depends only on stdx, paths, profile,
dissimilar, text-size and rustc-hash, so no caller makes a cycle.

> forwarded to the coordinator (from requests, pass 16; on requests' coordinator list).

## dedyn: crates/blaze-fmt: `&mut dyn Write` stays, with `// dyn:` (a CLI output writer, per file, not per token)

Checked against the second sweep's item 4: the formatter does not write through these. It
builds each file's text in a `String` (`rewrite` → `String`), and `source_file.rs::write_file`
hands the whole text to the session's emitter once per file; the emitters write it with one
`write!` (stdout, files) or a line per mismatch (diff, checkstyle, json, modified lines).
`print_docs` is `--print-config`'s output. These are CLI output writers, on the user's leave-alone
list. `Session<'b, T: Write>` is already generic; only the emitter's boundary is erased. Making
`Emitter` an enum of its seven modes to make the writer generic would move upstream rustfmt's
structure for one indirect call per file. So: no code change, a comment on each.

Add the line `// dyn: the session's output, written once per formatted file, never per token.`
directly above each of these (today's line numbers):

- `src/emitter.rs:34` (`fn emit_formatted_file(`), `:40` (`fn emit_header`), `:44` (`fn emit_footer`)
- `src/emitter/stdout.rs:20`, `src/emitter/files.rs:22`, `src/emitter/files_with_backup.rs:12`,
  `src/emitter/diff.rs:20`, `src/emitter/modified_lines.rs:12`, `src/emitter/json.rs:30` and `:37`,
  `src/emitter/checkstyle.rs:13`, `:19` and `:25` (each `output: &mut dyn Write` parameter line)
- `src/config/config_type.rs:388`: `// dyn: --print-config's output, written once.`

`emitter: Box<dyn Emitter + 'b>` (`lib.rs:441`, `create_emitter` `:516`) and
`source_file.rs:23` (`&mut dyn Emitter`): `// dyn: the emit mode, chosen once per session from
the config; called once per formatted file.` above each.

> forwarded to the coordinator: `crates/blaze-fmt` is the coordinator's own (crates.md "every other path"), not requests'; on requests' coordinator list (requests, pass 22).
