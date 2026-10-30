//! fn-cache's opt-in: only a crate that depends on `blaze` takes part.

use base_db::{Db, all_crates};
use test_fixture::ChangeFixture;

use crate::fn_cache::uses_fn_cache;

const FIXTURE: &str = r#"
//- /blaze.rs crate:blaze
//- /opted.rs crate:opted deps:blaze
//- /plain.rs crate:plain
//- /blaze_like.rs crate:blaze_like deps:plain
"#;

#[test]
fn only_a_crate_that_depends_on_blaze_uses_the_cache() {
    let mut db = Db::for_tests();
    ChangeFixture::parse(FIXTURE).change.apply(&mut db);
    let mut opted: Vec<String> = all_crates(&db)
        .iter()
        .filter(|&&krate| uses_fn_cache(&db, krate))
        .filter_map(|krate| Some(krate.extra_data(&db).display_name.as_ref()?.to_string()))
        .collect();
    opted.sort();
    assert_eq!(opted, ["opted"]);
}
