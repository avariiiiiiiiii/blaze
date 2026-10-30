//! The goal store's part: a fact read back from the parts the store keeps, a crate's key and
//! its lang items, and the fact a read of an item's query stands for.

use blaze_alloc_count::assert_at_most;
use blaze_ids::ContentId;
use blaze_query::QueryKind;
use hir_def::{AdtId, AssocItemId, ImplId, TraitId, lang_item::lang_items};
use hir_ty::{
    method_resolution::{ImplPair, RelevantImplsKey, SelfTyKey, relevant_impls},
    next_solver::{
        AnyImplId, SimplifiedType as HirSimplifiedType,
        goal_store::{FactObject, FactRef, GoalFacts},
    },
};
use salsa::plumbing::AsId;
use test_fixture::WithFixture;

use super::{TestDB, edit, item, items, single, table::every_kind};
use crate::{
    Fact, ImplEntry, ImplIndexKey, ItemId, blanket_impls,
    goals::{Goals, fact_of_read},
    implements, impls,
    index::impl_self_ty,
    item_key, key, kinds, specializes,
    table::{fact_of_ref, fact_ref},
    value,
};

fn id(n: u8) -> ContentId {
    ContentId([n; 32])
}

#[test]
fn a_fact_reads_back_from_its_parts() {
    let facts = every_kind();
    let mut kinds: Vec<u16> = facts.iter().map(|it| it.kind().0).collect();
    kinds.sort_unstable();
    kinds.dedup();
    assert_eq!(kinds.len(), 17, "one fact of each kind");
    for fact in facts {
        let parts = fact_ref(&fact);
        assert_eq!(parts.kind, fact.kind());
        assert_eq!(fact_of_ref(parts), Some(fact), "{parts:?}");
    }
}

#[test]
fn parts_that_are_no_fact_read_back_as_none() {
    let (db, _) = single("");
    let subject = id(1);
    let parts = |kind: QueryKind, object: FactObject| FactRef { kind, subject, object };
    for refused in [
        parts(QueryKind(kinds::SIGNATURE.0 - 1), FactObject::None),
        parts(QueryKind(kinds::SIGNATURE.0 + 0x100), FactObject::None),
        parts(kinds::HASHES, FactObject::None),
        parts(kinds::SIGNATURE, FactObject::Id(id(2))),
        parts(kinds::SIGNATURE, FactObject::Field(0)),
        parts(kinds::FIELD, FactObject::None),
        parts(kinds::FIELD, FactObject::Id(id(2))),
        parts(kinds::IMPLEMENTS, FactObject::None),
        parts(kinds::METHOD, FactObject::Field(1)),
        parts(kinds::TARGET, FactObject::Id(id(2))),
    ] {
        assert_eq!(fact_of_ref(refused), None, "{refused:?}");
        assert_eq!(Goals.value(&db, refused), None, "{refused:?}");
    }
}

const LANG_ON_A: &str = r#"
#![feature(lang_items)]
#[lang = "sized"]
pub trait A {}
pub trait B {}
"#;

#[test]
fn the_crate_key_changes_when_a_lang_item_moves() {
    let (mut db, file) = single(LANG_ON_A);
    let krate = db.test_crate();
    let trait_of = |db: &TestDB, path: &str| match item(db, path) {
        ItemId::Trait(it) => it,
        other => panic!("`{path}` is {other:?}"),
    };
    assert_eq!(lang_items(&db, krate).Sized, Some(trait_of(&db, "A")));
    let before = Goals.crate_key(&db, Some(krate));
    let identity = key::crate_key(&db, krate);
    assert_ne!(before, Goals.crate_key(&db, None));
    assert_eq!(Goals.crate_key(&db, None), Goals.crate_key(&single("").0, None));

    edit(&mut db, file, &format!("{LANG_ON_A}pub fn f() {{}}\n"));
    assert_eq!(Goals.crate_key(&db, Some(krate)), before, "an edit of no lang item keeps it");

    edit(
        &mut db,
        file,
        r#"
#![feature(lang_items)]
pub trait A {}
#[lang = "sized"]
pub trait B {}
"#,
    );
    assert_eq!(lang_items(&db, krate).Sized, Some(trait_of(&db, "B")));
    assert_eq!(key::crate_key(&db, krate), identity, "the crate is the same");
    assert_ne!(Goals.crate_key(&db, Some(krate)), before, "its lang item moved");
}

const ITEMS: &str = r#"
pub struct S<T>(T);
pub enum E { V(u8) }
pub trait Tr { type X; }
impl<T> Tr for S<T> { type X = u8; }
pub fn f() -> u8 { 0 }
"#;

struct Fixture {
    s: AdtId,
    e: AdtId,
    tr: TraitId,
    imp: ImplId,
    f: hir_def::FunctionId,
}

fn fixture(db: &TestDB) -> Fixture {
    let adt = |path: &str| match item(db, path) {
        ItemId::Adt(it) => it,
        other => panic!("`{path}` is {other:?}"),
    };
    let ItemId::Trait(tr) = item(db, "Tr") else { panic!("`Tr` is no trait") };
    let ItemId::Function(f) = item(db, "f") else { panic!("`f` is no fn") };
    let imp = items(db)
        .into_iter()
        .find_map(|it| match it {
            ItemId::Impl(it) => Some(it),
            _ => None,
        })
        .expect("the fixture has an impl");
    Fixture { s: adt("S"), e: adt("E"), tr, imp, f }
}

#[test]
fn a_read_of_an_items_query_stands_for_its_signature() {
    let (db, _) = single(ITEMS);
    let Fixture { s, e, tr, imp, f } = fixture(&db);
    let signature = |id: ItemId| Some(Fact::Signature(item_key(&db, id)));
    let AdtId::StructId(struct_) = s else { panic!("`S` is no struct") };
    let AdtId::EnumId(enum_) = e else { panic!("`E` is no enum") };
    let variant = enum_.enum_variants(&db).variants.values().next().expect("`E` has a variant").0;
    let alias = tr
        .trait_items(&db)
        .items
        .iter()
        .find_map(|&(_, it)| match it {
            AssocItemId::TypeAliasId(it) => Some(it),
            _ => None,
        })
        .expect("`Tr` has a type");

    let read = |name: &str, key: salsa::Id| fact_of_read(&db, name, key);
    for name in [
        "ImplSignature::kept",
        "ImplItems::of",
        "impl_trait_of_impl",
        "impl_has_unconstrained_params",
        "generic_predicates_of",
    ] {
        assert_eq!(read(name, imp.as_id()), signature(ItemId::Impl(imp)), "{name}");
    }
    for name in ["StructSignature::kept", "field_types_of_variant", "variances_of_query"] {
        assert_eq!(read(name, struct_.as_id()), signature(ItemId::Adt(s)), "{name}");
    }
    assert_eq!(read("representability", enum_.as_id()), signature(ItemId::Adt(e)));
    let ctor = "fn_sig_for_enum_variant_constructor";
    for name in [ctor, "ctor_variances_query", "AttrFlags::query"] {
        assert_eq!(read(name, variant.as_id()), signature(ItemId::Adt(e)), "{name}: its enum");
    }
    for name in ["TraitSignature::kept", "supertraits_info", "AttrFlags::query"] {
        assert_eq!(read(name, tr.as_id()), signature(ItemId::Trait(tr)), "{name}");
    }
    assert_eq!(read("ItemBounds::of", alias.as_id()), signature(ItemId::Trait(tr)), "its trait");
    assert_eq!(read("fn_sig_for_fn", f.as_id()), signature(ItemId::Function(f)));

    let krate = db.test_crate();
    let target = read("target_data_layout_query", krate.as_id());
    assert_eq!(target, Some(Fact::Target(key::crate_key(&db, krate))));
    assert_eq!(target, Some(crate::target(&db, krate)));

    // A key whose page holds another struct, a query keyed by a crate's batch or several
    // arguments, and a name no query has stand for nothing.
    assert_eq!(read("ImplSignature::kept", tr.as_id()), None);
    assert_eq!(read("fn_sig_for_fn", struct_.as_id()), None);
    assert_eq!(read("crate_impl_traits", krate.as_id()), None);
    assert_eq!(read("TraitImpls::for_crate", krate.as_id()), None);
    assert_eq!(read("no such query", imp.as_id()), None);
}

const SPECIALIZING: &str = r#"
#![feature(specialization)]
pub trait Tr { fn f(); }
pub struct S;
pub struct Other;
impl<T> Tr for T { default fn f() {} }
impl Tr for S { fn f() {} }
"#;

#[test]
fn a_read_of_the_impls_of_a_trait_stands_for_its_implements_fact() {
    let (db, _) = single(SPECIALIZING);
    let krate = db.test_crate();
    let ItemId::Trait(tr) = item(&db, "Tr") else { panic!("`Tr` is no trait") };
    let adt = |path: &str| match item(&db, path) {
        ItemId::Adt(it) => HirSimplifiedType::Adt(it.into()),
        other => panic!("`{path}` is {other:?}"),
    };
    let read = |name: &str, key: salsa::Id| fact_of_read(&db, name, key);
    let trait_key = item_key(&db, ItemId::Trait(tr));
    let sorted_keys = |impls: &[AnyImplId]| {
        let mut keys: Vec<_> = impls
            .iter()
            .map(|&it| match it {
                AnyImplId::ImplId(it) => item_key(&db, ItemId::Impl(it)),
                AnyImplId::BuiltinDeriveImplId(it) => key::builtin_derive_key(&db, it),
            })
            .collect();
        keys.sort_unstable();
        keys
    };
    let entry_keys = |entry: ImplEntry| match entry {
        ImplEntry::Impls(it) => it.iter().map(|&(key, _)| key).collect::<Vec<_>>(),
        ImplEntry::TraitAbsent => panic!("`Tr` is there"),
    };
    let blanket = entry_keys(blanket_impls(&db, trait_key).unwrap());
    assert_eq!(blanket.len(), 1);

    for (self_ty, own) in [(adt("S"), 1), (adt("Other"), 0)] {
        let at = RelevantImplsKey::new(&db, krate, tr, Some(SelfTyKey(self_ty)));
        let fact = implements(&db, tr, &self_ty);
        assert_eq!(read("relevant_impls", at.as_id()), Some(fact), "{self_ty:?}");
        // The impls the read gives are those the fact's value hashes.
        let Fact::Implements { self_ty: hashed, .. } = fact else { unreachable!() };
        let index = ImplIndexKey { trait_: trait_key, self_ty: hashed };
        let mut expected = entry_keys(impls(&db, index).unwrap());
        assert_eq!(expected.len(), own, "{self_ty:?}");
        expected.extend_from_slice(&blanket);
        expected.sort_unstable();
        assert_eq!(sorted_keys(relevant_impls(&db, at).all()), expected, "{self_ty:?}");
    }

    let none = RelevantImplsKey::new(&db, krate, tr, None);
    let only = Fact::Implements { trait_: trait_key, self_ty: key::unsimplified() };
    assert_eq!(read("relevant_impls", none.as_id()), Some(only));
    let unsimplified = ImplIndexKey { trait_: trait_key, self_ty: key::unsimplified() };
    assert_eq!(entry_keys(impls(&db, unsimplified).unwrap()), Vec::new(), "no impl is for it");
    assert_eq!(sorted_keys(relevant_impls(&db, none).blanket()), blanket);
    assert_ne!(value(&db, &only).ok(), value(&db, &implements(&db, tr, &adt("S"))).ok());

    // A key whose page holds another struct stands for nothing.
    assert_eq!(read("relevant_impls", tr.as_id()), None);
    assert_eq!(read("relevant_impls", krate.as_id()), None);
}

#[test]
fn a_read_of_specialization_stands_for_its_fact() {
    let (db, _) = single(SPECIALIZING);
    let krate = db.test_crate();
    let all: Vec<ImplId> = items(&db)
        .into_iter()
        .filter_map(|it| match it {
            ItemId::Impl(it) => Some(it),
            _ => None,
        })
        .collect();
    let blanket = *all.iter().find(|&&it| impl_self_ty(&db, it).is_none()).expect("a blanket");
    let for_s = *all.iter().find(|&&it| impl_self_ty(&db, it).is_some()).expect("`impl Tr for S`");
    let read = |name: &str, key: salsa::Id| fact_of_read(&db, name, key);

    assert_eq!(read("may_specialize", for_s.as_id()), Some(specializes(&db, for_s)));
    let pair = ImplPair::new(&db, for_s, blanket);
    let whole_crate = Fact::Crate(key::crate_key(&db, krate));
    assert_eq!(read("specializes_query", pair.as_id()), Some(whole_crate));

    assert_eq!(read("specializes_query", for_s.as_id()), None, "an impl is no pair");
    assert_eq!(read("may_specialize", pair.as_id()), None, "a pair is no impl");
    let ItemId::Trait(tr) = item(&db, "Tr") else { panic!("`Tr` is no trait") };
    let at = RelevantImplsKey::new(&db, krate, tr, None);
    assert_eq!(read("specializes_query", at.as_id()), None);
}

const LOADS: &str = r#"
//- /dep.rs crate:dep
pub struct D;
//- /main.rs crate:main deps:dep
pub struct M;
"#;

#[test]
fn the_crate_key_changes_when_the_crate_loads_another() {
    let (mut db, files) = TestDB::with_many_files(LOADS);
    let main_file = files[1].file_id(&db);
    let main = *base_db::all_crates(&db)
        .iter()
        .find(|it| it.extra_data(&db).display_name.as_ref().is_some_and(|it| it.to_string() == "main"))
        .expect("the crate `main`");
    let before = Goals.crate_key(&db, Some(main));
    edit(&mut db, main_file, "pub struct M;\npub fn f() {}\n");
    assert_eq!(Goals.crate_key(&db, Some(main)), before, "it loads the same crates");
    edit(&mut db, main_file, "pub struct M;\npub use dep::D;\n");
    assert_ne!(Goals.crate_key(&db, Some(main)), before, "it loads `dep` now");
}

#[test]
fn a_facts_value_is_the_one_its_parts_read_back_to() {
    let (db, _) = single(ITEMS);
    let Fixture { imp, .. } = fixture(&db);
    let krate = db.test_crate();
    for fact in [Fact::Signature(item_key(&db, ItemId::Impl(imp))), crate::target(&db, krate)] {
        assert_eq!(Goals.value(&db, fact_ref(&fact)), value(&db, &fact).ok(), "{fact:?}");
        assert!(Goals.value(&db, fact_ref(&fact)).is_some(), "{fact:?}");
    }
    let elsewhere = Fact::Target(id(9));
    assert_ne!(value(&db, &elsewhere).ok(), value(&db, &crate::target(&db, krate)).ok());
}

#[test]
fn a_warm_read_and_value_allocate_nothing() {
    let (db, _) = single(ITEMS);
    let Fixture { imp, .. } = fixture(&db);
    let fact = fact_ref(&Fact::Signature(item_key(&db, ItemId::Impl(imp))));
    let read = || fact_of_read(&db, "ImplSignature::kept", imp.as_id());
    let warm = (read(), Goals.value(&db, fact));
    assert_at_most(0, "a read's fact and a fact's value, warm", || {
        std::hint::black_box((read(), Goals.value(&db, fact)))
    });
    assert!(warm.1.is_some());
}
