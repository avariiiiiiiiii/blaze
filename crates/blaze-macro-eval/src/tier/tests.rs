use super::*;

fn code(n: u8) -> ContentId {
    ContentId([n; 32])
}

#[test]
fn a_blaze_without_the_compiled_tier_evaluates_everything() {
    assert!(!Policy::DEFAULT.compiled);
    for runs in [0, 1, 2, 1_000, u64::MAX] {
        assert_eq!(choose(code(1), runs), Tier::Evaluator, "{runs}");
    }
}

#[test]
fn with_it_the_first_expansion_is_evaluated_and_the_rest_compiled() {
    let policy = Policy { compiled: true, ..Policy::DEFAULT };
    assert_eq!(policy.choose(code(1), 0), Tier::Evaluator);
    assert_eq!(policy.choose(code(1), 1), Tier::Compiled);
    assert_eq!(policy.choose(code(2), u64::MAX), Tier::Compiled);
}

#[test]
fn the_policy_is_a_threshold_on_runs_alone() {
    let policy = Policy { warm: 4, compiled: true };
    for runs in 0..10 {
        let tier = policy.choose(code(3), runs);
        assert_eq!(tier == Tier::Compiled, runs >= 4, "{runs}");
        assert_eq!(tier, policy.choose(code(4), runs), "the same for every code");
    }
    assert_eq!(Policy { warm: 0, compiled: true }.choose(code(4), 0), Tier::Compiled);
}
