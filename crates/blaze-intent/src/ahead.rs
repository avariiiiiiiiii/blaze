//! What a [`blaze_api::Request::WillTest`] builds ahead: the test binaries holding a test its
//! filters select, and the functions those tests reached when they last ran, the ones most of
//! them reached first. The daemon builds those units at low priority and, with lazy codegen,
//! compiles those functions first; it runs nothing.
//!
//! Selection is libtest's: a test is selected when its name contains any of the filters, and
//! every test when there are none.

use blaze_items::CodeHash;
use rustc_hash::FxHashMap;

/// A test as its binary's last build and run left it.
pub struct Test<'a> {
    pub name: &'a str,
    /// The code hashes of what it reached when it last ran.
    pub reached: &'a [CodeHash],
}

/// A test binary's last build: the caller's number for its unit, and its tests.
pub struct Binary<'a> {
    pub unit: u32,
    pub tests: &'a [Test<'a>],
}

/// What to build ahead.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Plan {
    /// The units of the binaries holding a selected test, in the order given.
    pub units: Vec<u32>,
    /// The functions the selected tests reached, those more of them reached first, then by
    /// hash.
    pub hot: Vec<CodeHash>,
    /// The filters that select no test of any binary: tests the last builds don't have, which
    /// building ahead can't serve.
    pub unmatched: Vec<String>,
}

/// The plan for `filters` over the binaries' last builds.
pub fn plan(binaries: &[Binary<'_>], filters: &[String]) -> Plan {
    let mut plan = Plan::default();
    let mut matched = vec![false; filters.len()];
    let mut count: FxHashMap<CodeHash, u32> = FxHashMap::default();
    for binary in binaries {
        let mut selected = false;
        for test in binary.tests {
            let mut hit = filters.is_empty();
            for (at, filter) in filters.iter().enumerate() {
                if test.name.contains(filter.as_str()) {
                    matched[at] = true;
                    hit = true;
                }
            }
            if !hit {
                continue;
            }
            selected = true;
            count.reserve(test.reached.len());
            for hash in test.reached {
                *count.entry(*hash).or_insert(0) += 1;
            }
        }
        if selected {
            plan.units.push(binary.unit);
        }
    }
    let mut hot: Vec<(u32, CodeHash)> = count.into_iter().map(|(hash, n)| (n, hash)).collect();
    hot.sort_unstable_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    plan.hot = hot.into_iter().map(|(_, hash)| hash).collect();
    plan.unmatched = filters.iter().zip(&matched).filter(|(_, hit)| !**hit).map(|(it, _)| it.clone()).collect();
    plan
}

#[cfg(test)]
mod tests {
    use blaze_ids::ContentId;

    use super::*;

    fn hash(n: u8) -> CodeHash {
        CodeHash(ContentId::of("blaze-intent test", &[n]))
    }

    #[test]
    fn selects_binaries_and_orders_what_more_tests_reached_first() {
        let (a, b, c) = (hash(1), hash(2), hash(3));
        let first = [
            Test { name: "parse::empty", reached: &[a, b] },
            Test { name: "parse::long", reached: &[b] },
            Test { name: "eval::adds", reached: &[c] },
        ];
        let second = [Test { name: "io::reads", reached: &[c] }];
        let binaries = [Binary { unit: 7, tests: &first }, Binary { unit: 9, tests: &second }];
        let plan = plan(&binaries, &["parse".to_owned(), "nothing".to_owned()]);
        assert_eq!(plan.units, [7]);
        assert_eq!(plan.hot, [b, a]);
        assert_eq!(plan.unmatched, ["nothing"]);
    }

    #[test]
    fn no_filter_selects_every_test_as_libtest_does() {
        let (a, b) = (hash(1), hash(2));
        let first = [Test { name: "t", reached: &[a] }];
        let second = [Test { name: "u", reached: &[a, b] }];
        let binaries = [Binary { unit: 0, tests: &first }, Binary { unit: 1, tests: &second }];
        let plan = plan(&binaries, &[]);
        assert_eq!((plan.units, plan.hot, plan.unmatched), (vec![0, 1], vec![a, b], Vec::<String>::new()));
    }
}
