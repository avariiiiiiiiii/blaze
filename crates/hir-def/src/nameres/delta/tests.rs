//! Tests of the maintained name resolution (docs/v2/plan/design/dbsp.md §12), and the helpers
//! they share.
//!
//! A maintained def map is checked against two references in the same database, through the
//! canonical projection of `facts/tests.rs` (`project`, `compare`): the map a fresh maintainer
//! makes of the same inputs (`delta::cold`, the cold run of §3), with nothing left out, and
//! `facts::collect_crate`'s, which may differ only where §6 says ([`DIFFERENCES`]). One database
//! keeps both sides' interned ids (macro calls, macro files, other crates' modules) the same, so
//! the projection compares them exactly; across two databases it could not.

mod alloc;
mod cost;
mod random;
mod trace;

use std::fmt::Write as _;

use base_db::{Crate, Db, Maintained, all_crates};
use blaze_dbsp::Work;
use blaze_mem::Tag;
use hir_expand::{MacroCallKind, name::Name};
use intern::Symbol;
use span::FileId;
use test_fixture::ChangeFixture;
use test_utils::{FixtureWithProjectMeta, MiniCore};

use crate::{
    ModuleId,
    nameres::{
        DefMap, DefMapPair, ModulesSlot, ScopeSlot, crate_local_def_map,
        diagnostics::DefDiagnosticKind,
        facts::tests::{
            Exempt, compare, differential::ORDER_DEPENDENT, extra_proc_macros,
            new_crate_local_def_map, project,
        },
        frozen,
    },
    test_db::TestDB,
};

/// The crate's def map as a fresh maintainer makes it: maintained from an empty state, without
/// the database's state or published inputs. A query of its own, so the tracked structs it
/// makes live in one.
#[salsa::tracked(returns(ref))]
fn cold_crate_local_def_map(db: &Db, krate: Crate) -> DefMapPair<'_> {
    blaze_mem::frame::task(Tag::Nameres, |frame| {
        let (mut def_map, local) = super::cold(db, frame, krate);
        def_map.hand_over_scopes(db);
        DefMapPair::new(db, def_map, local)
    })
}

pub(super) fn name(text: &str) -> Name {
    Name::new_symbol_root(Symbol::intern(text))
}

/// A database of a fixture, which maintained every crate as it was loaded, with the fixture's
/// files by path in the fixture's order.
pub(super) struct Loaded {
    pub(super) db: TestDB,
    pub(super) files: Vec<(String, FileId)>,
}

impl Loaded {
    pub(super) fn new(fixture: &str) -> Loaded {
        let mut db = TestDB::for_tests();
        let change =
            ChangeFixture::parse_with_proc_macros(fixture, MiniCore::RAW_SOURCE, extra_proc_macros());
        let paths = FixtureWithProjectMeta::parse(fixture).fixture.into_iter().map(|it| it.path);
        let files = paths.zip(change.files.iter().map(|it| it.file_id())).collect();
        change.change.apply(&mut db);
        Loaded { db, files }
    }

    pub(super) fn file(&self, path: &str) -> FileId {
        self.files
            .iter()
            .find(|(it, _)| it == path)
            .unwrap_or_else(|| panic!("no file `{path}` in the fixture: {:?}", self.files))
            .1
    }

    pub(super) fn text(&self, file: FileId) -> String {
        self.db.file_text(file).text(&self.db).as_str().to_owned()
    }

    /// The crate whose root is the fixture's first file.
    pub(super) fn krate(&self) -> Crate {
        let root = self.files[0].1;
        *all_crates(&self.db)
            .iter()
            .find(|krate| krate.data(&self.db).root_file_id == root)
            .unwrap_or_else(|| panic!("no crate has `{}` for its root", self.files[0].0))
    }
}

/// The module at `path` (`crate::a::b` as `["a", "b"]`) of `krate`'s maintained map.
pub(super) fn module_at(db: &Db, krate: Crate, path: &[&str]) -> ModuleId {
    let map = crate_local_def_map(db, krate).def_map(db);
    let mut at = map.root;
    for segment in path {
        at = *map.module(db, at).children(db).get(&name(segment)).unwrap_or_else(|| {
            panic!("no module `{segment}` of `crate::{}`", path.join("::"))
        });
    }
    at
}

/// `crate::a::b` for a module of a crate's map.
pub(super) fn module_path(db: &Db, module: ModuleId) -> String {
    let mut names = Vec::new();
    let mut at = module;
    while let Some(parent) = at.containing_module_inside_def_map(db) {
        names.push(at.name(db).map_or_else(|| "?".to_owned(), |it| it.as_str().to_owned()));
        at = parent;
    }
    names.push("crate".to_owned());
    names.reverse();
    names.join("::")
}

/// The maintained map of `krate` without ids: each module's path with the names in its scope
/// and the namespaces each is in, sorted.
pub(super) fn summary(db: &Db, krate: Crate) -> Vec<(String, Vec<String>)> {
    let map = crate_local_def_map(db, krate).def_map(db);
    let mut out: Vec<(String, Vec<String>)> = map
        .modules(db)
        .map(|(id, data)| {
            let mut names: Vec<String> = data
                .scope(db)
                .entries()
                .map(|(name, per_ns)| {
                    format!(
                        "{} types {} values {} macros {}",
                        name.as_str(),
                        per_ns.take_types().is_some(),
                        per_ns.take_values().is_some(),
                        per_ns.take_macros().is_some()
                    )
                })
                .collect();
            names.sort();
            (module_path(db, id), names)
        })
        .collect();
    out.sort();
    out
}

/// Panics naming `what` unless `def_map` is what the maintainer published: its modules a
/// `CrateResolution`, every scope a `ScopeInput`.
pub(super) fn assert_published(db: &Db, def_map: &DefMap, what: &str) {
    assert!(
        matches!(def_map.modules, ModulesSlot::Published(_)),
        "{what}: the modules of crate {:?} are not published: {:?}",
        def_map.krate,
        def_map.modules
    );
    for (id, data) in def_map.modules(db) {
        assert!(
            matches!(data.scope, ScopeSlot::Input(_)),
            "{what}: the scope of {id:?} is not a published input: {:?}",
            data.scope
        );
    }
}

// ---------------------------------------------------------------------------------------------
// §6: where the maintained map may differ from `facts`'. §6 names them against `facts/` and the
// old collector; `facts/` now has rustc's rule for 2 (glob ties by position) and 3 (an import
// through a glob waits for its module's expansions), so there the maintained map equals it.

/// One of design §6's differences from `facts/`.
pub(super) struct Difference {
    /// Its number in §6's list.
    pub(super) number: u8,
    pub(super) name: &'static str,
    /// What rustc (the pinned nightly) says of the program; empty where rustc accepts it and
    /// the difference is one the projection does not see.
    pub(super) rustc: &'static str,
    pub(super) fixture: &'static str,
    /// What the comparison with `facts` leaves out (only 4's: attributes reseeded at once). The
    /// comparison with a fresh maintainer leaves out nothing.
    pub(super) exempt: Exempt,
}

pub(super) const DIFFERENCES: &[Difference] = &[
    Difference {
        number: 1,
        name: "position_order_against_component_order",
        // Accepted: the columns are built in another order, and the projection sorts them.
        rustc: "",
        fixture: r#"
mod late { pub use crate::early::*; pub struct L; }
mod early { pub struct E; pub fn e() {} }
pub use late::*;
pub use early::E as Again;
mod user { pub use crate::late::L; pub use crate::*; }
"#,
        exempt: Exempt::NONE,
    },
    Difference {
        number: 2,
        name: ORDER_DEPENDENT[0].name,
        rustc: ORDER_DEPENDENT[0].rustc,
        fixture: ORDER_DEPENDENT[0].fixture,
        exempt: Exempt::NONE,
    },
    Difference {
        number: 2,
        name: ORDER_DEPENDENT[1].name,
        rustc: ORDER_DEPENDENT[1].rustc,
        fixture: ORDER_DEPENDENT[1].fixture,
        exempt: Exempt::NONE,
    },
    Difference {
        number: 3,
        name: ORDER_DEPENDENT[2].name,
        // Accepted: rustc's glob binding of `foo::T` is undetermined while `foo` has unexpanded
        // macros, so `use foo::T` waits for the expansion and binds its `T`
        // ([`an_import_through_a_glob_waits_for_its_modules_expansions`]).
        rustc: "",
        fixture: ORDER_DEPENDENT[2].fixture,
        exempt: Exempt::NONE,
    },
    Difference {
        number: 4,
        name: ORDER_DEPENDENT[3].name,
        rustc: ORDER_DEPENDENT[3].rustc,
        fixture: ORDER_DEPENDENT[3].fixture,
        exempt: ORDER_DEPENDENT[3].exempt,
    },
];

/// What the comparison of fixture `name` with `facts` leaves out: its §6 difference's.
pub(super) fn facts_exempt(name: &str) -> Exempt {
    DIFFERENCES.iter().find(|it| it.name == name).map_or(Exempt::NONE, |it| it.exempt)
}

/// Whether `def_map` has an attribute left unresolved: §6's 4, the one difference a program an
/// edit script made may show.
fn unresolved_attribute(db: &Db, def_map: &DefMap) -> bool {
    def_map.records(db).diagnostics.iter().any(|it| {
        matches!(&it.kind, DefDiagnosticKind::UnresolvedMacroCall { ast: MacroCallKind::Attr { .. }, .. })
    })
}

/// Compares every maintained crate of `db` with a fresh maintainer's map (nothing left out)
/// and with `facts`' (leaving out `exempt`, and with `rejected` also 4's difference where either
/// map has an [`unresolved_attribute`]: for a program an edit script made, where no fixture
/// says), appending each difference to `out` under `what`. A crate served by a frozen image is
/// not maintained and is skipped.
pub(super) fn check_db(db: &Db, what: &str, exempt: &Exempt, rejected: bool, out: &mut String) {
    for &krate in all_crates(db).iter() {
        let pair = crate_local_def_map(db, krate);
        let maintained = pair.def_map(db);
        if !matches!(maintained.modules, ModulesSlot::Published(_)) {
            assert!(
                frozen::served(db, krate).is_some(),
                "{what}: crate {krate:?} is neither maintained nor frozen"
            );
            continue;
        }
        assert_published(db, maintained, what);
        let display = &krate.extra_data(db).display_name;

        let cold = cold_crate_local_def_map(db, krate);
        let diff = compare(
            &project(db, cold.def_map(db), Some(cold.local(db)), &Exempt::NONE),
            &project(db, maintained, Some(pair.local(db)), &Exempt::NONE),
        );
        if !diff.is_empty() {
            _ = writeln!(
                out,
                "== {what}: crate {display:?}: a fresh maintainer's map (old) and the maintained \
                 one (new) differ\n{diff}"
            );
        }

        let facts = new_crate_local_def_map(db, krate);
        let mut exempt = *exempt;
        if rejected {
            exempt.unresolved_imports |=
                unresolved_attribute(db, maintained) || unresolved_attribute(db, facts.def_map(db));
        }
        let diff = compare(
            &project(db, facts.def_map(db), Some(facts.local(db)), &exempt),
            &project(db, maintained, Some(pair.local(db)), &exempt),
        );
        if !diff.is_empty() {
            _ = writeln!(
                out,
                "== {what}: crate {display:?}: `facts::collect_crate`'s map (old) and the \
                 maintained one (new) differ\n{diff}"
            );
        }
    }
}

/// The `nameres` maintainer's records of `krate` kept since the last take, one per write.
pub(super) fn take_work(db: &mut Db, krate: Crate) -> Vec<Work> {
    let mut records: Vec<Maintained> = Vec::new();
    db.take_maintained(&mut records);
    let mut out = Vec::new();
    for Maintained { write, maintainer, krate: of, work } in records {
        if maintainer != "nameres" || of != krate {
            continue;
        }
        assert_eq!(write as usize, out.len(), "one `nameres` record of {krate:?} per write");
        out.push(work);
    }
    out
}

/// The `Work` of the one write `f` makes to `db`, for `krate`.
pub(super) fn work_of(db: &mut Db, krate: Crate, f: impl FnOnce(&mut Db)) -> Work {
    db.keep_maintained(true);
    let mut stale = Vec::new();
    db.take_maintained(&mut stale);
    f(db);
    let work = take_work(db, krate);
    db.keep_maintained(false);
    assert_eq!(work.len(), 1, "the edit is one write: {work:?}");
    work[0]
}

#[test]
fn the_differences_are_named_and_agree_on_everything_else() {
    let mut numbers: Vec<u8> = DIFFERENCES.iter().map(|it| it.number).collect();
    numbers.dedup();
    assert_eq!(numbers, [1, 2, 3, 4], "each of §6's differences has a fixture");
    let mut failures = String::new();
    for it in DIFFERENCES {
        assert!(
            matches!(it.number, 1 | 3) || it.rustc.starts_with("error"),
            "{}: say what rustc says of it",
            it.name
        );
        let loaded = Loaded::new(it.fixture);
        check_db(&loaded.db, it.name, &it.exempt, false, &mut failures);
    }
    assert!(failures.is_empty(), "{failures}");
}

/// Of two globs bringing one name, the maintained binding is the first by position, also when
/// it is the second to arrive (§6 "glob ties").
#[test]
fn a_glob_tie_follows_position_not_arrival() {
    let fixture = r#"
//- /lib.rs
mod a;
mod b;
use b::*;
pub use S as Exported;
//- /a.rs
pub struct S;
//- /b.rs
pub struct S;
"#;
    let mut loaded = Loaded::new(fixture);
    let krate = loaded.krate();
    let root = loaded.file("/lib.rs");
    // The module whose `S` the root's `S` is.
    let container = |db: &Db| {
        let map = crate_local_def_map(db, krate).def_map(db);
        let (def, _) = map.module(db, map.root).scope(db).type_(&name("S")).expect("`S` is in scope");
        let crate::ModuleDefId::AdtId(crate::AdtId::StructId(it)) = def else { panic!("`S` is {def:?}") };
        crate::Lookup::lookup(&it, db).container.name(db)
    };
    assert_eq!(container(&loaded.db), Some(name("b")), "one glob: `b`'s `S`");
    loaded.db.set_file_text(root, "mod a;\nmod b;\nuse a::*;\nuse b::*;\npub use S as Exported;\n");
    assert_eq!(container(&loaded.db), Some(name("a")), "`a`'s glob arrived second and comes first");
    let mut failures = String::new();
    check_db(&loaded.db, "glob tie", &Exempt::NONE, false, &mut failures);
    assert!(failures.is_empty(), "{failures}");
    loaded.db.set_file_text(root, "mod a;\nmod b;\nuse b::*;\npub use S as Exported;\n");
    assert_eq!(container(&loaded.db), Some(name("b")), "`a`'s glob gone again");
}

/// `use foo::T` where `foo` globs `x::T` and a macro in `foo` declares its own `T`: rustc binds
/// the expanded one, as its glob binding is undetermined while `foo` has unexpanded macros
/// (`rustc_resolve::ident`, `resolve_ident_in_module_globs_unadjusted`). A cold map and one
/// maintained through an edit that reruns only the root agree, with each other and with rustc
/// (§6's third difference).
#[test]
fn an_import_through_a_glob_waits_for_its_modules_expansions() {
    let mut loaded = Loaded::new(ORDER_DEPENDENT[2].fixture);
    let krate = loaded.krate();
    let root = loaded.file("/lib.rs");
    let container = |db: &Db| {
        let map = crate_local_def_map(db, krate).def_map(db);
        let (def, _) = map.module(db, map.root).scope(db).type_(&name("T")).expect("`T` is in scope");
        let crate::ModuleDefId::AdtId(crate::AdtId::StructId(it)) = def else { panic!("`T` is {def:?}") };
        crate::Lookup::lookup(&it, db).container.name(db)
    };
    assert_eq!(container(&loaded.db), Some(name("foo")), "loaded: `foo`'s expanded `T`");
    let text = format!("{}fn edited() {{}}\n", loaded.text(root));
    loaded.db.set_file_text(root, &text);
    assert_eq!(container(&loaded.db), Some(name("foo")), "the root rerun alone: `foo`'s expanded `T`");
    let mut failures = String::new();
    check_db(&loaded.db, "an import through a glob", &Exempt::NONE, false, &mut failures);
    assert!(failures.is_empty(), "{failures}");
}
