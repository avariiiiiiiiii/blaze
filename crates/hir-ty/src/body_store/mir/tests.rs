//! Built MIR through the item store: a body's MIR and its closure's, built and flushed in one
//! database, are read in a fresh one whose ids differ with no MIR built, equal to the same MIR
//! built there; both databases encode it to the same bytes; a lowering error is never stored; a
//! corrupt answer is an error naming the body; a hit allocates only what decoding makes.
//!
//! hir-ty's test binary links no MIR lowering, so the MIR is built by hand, inside a query as
//! the built-MIR queries run it, so its reads are captured.

use std::sync::Arc;

use base_db::{Db, all_crates};
use blaze_alloc_count::measure;
use blaze_format::Aligned;
use hir_def::{
    AdtId, DefWithBodyId, ExpressionStoreOwnerId, FunctionId, ModuleDefId, StructId, VariantId,
    deps::ImageStore,
    expr_store::ExpressionStore,
    hir::{ClosureKind, Expr},
    nameres::{crate_def_map, demand::crate_body_owners},
    signatures::FunctionSignature,
};
use hir_expand::name::Name;
use la_arena::{Arena, ArenaMap, Idx, RawIdx};
use rustc_hash::FxHashMap;
use rustc_type_ir::inherent::Ty as _;
use salsa::Setter;

use super::{RAN, built, decode, encode, lookup_mir, place};
use crate::{
    CallableDefId, InferBodyId,
    body_store::{
        BodyFacts, Flushed, Lookup, Mode, Step,
        record::{self, AnswerKey, ShapeRef},
        tests::{BUILD, Defs, FIXTURE, Memory, Session, db_of, function, second, session},
    },
    db::{InternedClosure, InternedClosureId},
    mir::{
        AggregateKind, BasicBlock, Inlined, Local, MirBody, MirLowerError, MirOwner, MirPhase,
        MirSpan, Operand, OperandKind, Place, Projection, ProjectionElem, Rvalue, Statement,
        StatementKind, Terminator, TerminatorKind,
    },
    next_solver::{DbInterner, Ty, default_types, goal_store::solver_version},
};

/// What a test asks the store for: the MIR of `closures` or of its closure, or a lowering that
/// fails.
#[salsa::input]
struct Ask {
    #[returns(copy)]
    def: DefWithBodyId,
    #[returns(copy)]
    closure: bool,
    #[returns(copy)]
    fails: bool,
}

fn owner_of<'db>(db: &'db Db, ask: Ask) -> MirOwner<'db> {
    let def = ask.def(db);
    match ask.closure(db) {
        false => MirOwner::Body(InferBodyId::DefWithBodyId(def)),
        true => MirOwner::Closure(closure_of(db, def)),
    }
}

/// What a built-MIR query does with the store: `providers::compute` at `MirStage::Built`.
#[salsa::tracked(returns(ref))]
fn built_mir<'db>(db: &'db Db, ask: Ask) -> Result<MirBody<'db>, MirLowerError<'db>> {
    let owner = owner_of(db, ask);
    built(db, owner, || match ask.fails(db) {
        true => Err(MirLowerError::Loop),
        false => Ok(mir_of(db, owner)),
    })
}

fn ran() -> u32 {
    RAN.with(|ran| ran.get())
}

fn reset_ran() {
    RAN.with(|ran| ran.set(0));
}

fn idx<T>(raw: u32) -> Idx<T> {
    Idx::from_raw(RawIdx::from_u32(raw))
}

/// The function `name` of any crate of the fixture.
fn any_function(db: &Db, name: &str) -> FunctionId {
    for &krate in all_crates(db).iter() {
        for def in crate_body_owners(db, krate) {
            if let DefWithBodyId::FunctionId(it) = def
                && FunctionSignature::of(db, it).name == Name::new_root(name)
            {
                return it;
            }
        }
    }
    panic!("no function `{name}`")
}

fn struct_named(db: &Db, name: &str) -> StructId {
    for &krate in all_crates(db).iter() {
        for (_, module) in crate_def_map(db, krate).modules(db) {
            for (found, per_ns) in module.scope(db).entries() {
                if found.as_str() == name
                    && let Some(ModuleDefId::AdtId(AdtId::StructId(it))) = per_ns.take_types()
                {
                    return it;
                }
            }
        }
    }
    panic!("no struct `{name}`")
}

fn closure_of(db: &Db, def: DefWithBodyId) -> InternedClosureId<'_> {
    let store = ExpressionStore::of(db, ExpressionStoreOwnerId::Body(def));
    let expr = store
        .exprs()
        .find_map(|(id, expr)| matches!(expr, Expr::Closure { .. }).then_some(id))
        .expect("`closures` has a closure");
    InternedClosureId::new(
        db,
        InternedClosure { owner: InferBodyId::DefWithBodyId(def), expr, kind: ClosureKind::Closure },
    )
}

/// The MIR this test builds of `owner`, in `db`'s ids: a call into another crate, a function
/// item, an ADT, the closure, a drop with its unwind path. The closure's is another body.
fn mir_of<'db>(db: &'db Db, owner: MirOwner<'db>) -> MirBody<'db> {
    let interner = DbInterner::new_no_crate(db);
    let types = &default_types().types;
    let empty = default_types().empty.generic_args;
    let (def, closure) = match owner {
        MirOwner::Body(InferBodyId::DefWithBodyId(def)) => (def, false),
        MirOwner::Closure(it) => match it.loc(db).owner {
            InferBodyId::DefWithBodyId(def) => (def, true),
            InferBodyId::AnonConstId(_) => panic!("the test's closure is of a function"),
        },
        _ => panic!("the test builds a body's MIR or its closure's"),
    };
    let wrap = struct_named(db, "Wrap");
    let o1 = any_function(db, "o1");
    let seven = any_function(db, "seven");
    let wrap_ty = Ty::new_adt(interner, AdtId::StructId(wrap), empty);

    let mut locals = Arena::new();
    let ret = locals.alloc(Local { ty: types.u32.store() });
    let n = locals.alloc(Local { ty: types.u32.store() });
    let w = locals.alloc(Local { ty: wrap_ty.store() });
    let place = |local| Place::<'db> { local, projection: Projection::new_from_slice(&[]) }.store();
    let copy = |local| Operand { kind: OperandKind::Copy(place(local)), span: Some(MirSpan::ExprId(idx(0))) };

    let mut statements = vec![
        Statement { kind: StatementKind::StorageLive(w), span: MirSpan::Unknown },
        Statement {
            kind: StatementKind::Assign(place(n), Rvalue::Use(Operand::from_bytes(Box::new([7, 0, 0, 0]), types.u32))),
            span: MirSpan::ExprId(idx(1)),
        },
        Statement {
            kind: StatementKind::Assign(
                place(w),
                Rvalue::Aggregate(AggregateKind::Adt(VariantId::StructId(wrap), empty.store()), Box::new([copy(n)])),
            ),
            span: MirSpan::PatId(idx(0)),
        },
        Statement {
            kind: StatementKind::Assign(
                place(ret),
                Rvalue::Use(Operand {
                    kind: OperandKind::Move(
                        Place::<'db> {
                            local: w,
                            projection: Projection::new_from_slice(&[ProjectionElem::Field(crate::mir::FieldIndex(0))]),
                        }
                        .store(),
                    ),
                    span: None,
                }),
            ),
            span: MirSpan::ExprEnd(idx(1)),
        },
    ];
    if closure {
        statements.push(Statement { kind: StatementKind::Nop, span: MirSpan::Unknown });
    }
    let mut basic_blocks = Arena::new();
    let start = basic_blocks.alloc(BasicBlock {
        statements,
        terminator: Some(Terminator {
            span: MirSpan::ExprId(idx(2)),
            kind: TerminatorKind::Call {
                func: Operand::from_fn(db, if closure { seven } else { o1 }, empty),
                args: Box::new([]),
                destination: place(ret),
                target: Some(idx(1)),
                cleanup: Some(idx(2)),
                from_hir_call: true,
                intrinsic: None,
            },
        }),
        is_cleanup: false,
    });
    basic_blocks.alloc(BasicBlock {
        statements: Vec::new(),
        terminator: Some(Terminator { span: MirSpan::Unknown, kind: TerminatorKind::Return }),
        is_cleanup: false,
    });
    basic_blocks.alloc(BasicBlock {
        statements: Vec::new(),
        terminator: Some(Terminator {
            span: MirSpan::Unknown,
            kind: TerminatorKind::Drop { place: place(w), target: idx(3), unwind: None },
        }),
        is_cleanup: true,
    });
    basic_blocks.alloc(BasicBlock {
        statements: Vec::new(),
        terminator: Some(Terminator { span: MirSpan::Unknown, kind: TerminatorKind::UnwindResume }),
        is_cleanup: true,
    });
    let mut binding_locals = ArenaMap::default();
    binding_locals.insert(idx(0), n);
    MirBody {
        basic_blocks,
        locals,
        start_block: start,
        owner: InferBodyId::DefWithBodyId(def),
        binding_locals,
        upvar_locals: FxHashMap::default(),
        param_locals: vec![n],
        promoted_locals: Vec::new(),
        closures: if closure { Vec::new() } else { vec![closure_of(db, def)] },
        coroutine: None,
        unreached: Vec::new(),
        scope_ends: vec![w],
        inherits_overflow_checks: false,
        phase: MirPhase::Built,
        upvars_in_place: Vec::new(),
        panic_unwinds: Vec::new(),
        promoted_constants: Vec::new(),
        unreachable_after_calls: Vec::new(),
        guard_refs: Vec::new(),
        capture_copies: Vec::new(),
        inlined: Inlined::default(),
    }
}

fn ask(s: &Session, closure: bool) -> Ask {
    Ask::new(&s.db, function(&s.db, "closures"), closure, false)
}

/// `owner`'s MIR in stable form: its answer's bytes, every id by its key.
fn stable<'db>(s: &'db Session, owner: MirOwner<'db>) -> Vec<u8> {
    let def = place(&s.db, owner).expect("the test's owners are a body's").0;
    let mut out = Vec::new();
    encode(&s.db, &*s.facts, def, &mir_of(&s.db, owner), &mut out).expect("the test's MIR encodes");
    out
}

/// The first database's MIR of `closures` and of its closure, built and flushed into a new
/// store, and each in stable form.
fn stored() -> (Arc<Memory>, Defs, [Vec<u8>; 2]) {
    let memory = Arc::new(Memory::default());
    let defs = Defs::default();
    let first = session(db_of(FIXTURE), &memory, &defs, None, Mode::Serve);
    let mut forms = [Vec::new(), Vec::new()];
    for closure in [false, true] {
        let ask = ask(&first, closure);
        let built = built_mir(&first.db, ask).as_ref().expect("the test's MIR builds");
        assert_eq!(*built, mir_of(&first.db, owner_of(&first.db, ask)));
        forms[usize::from(closure)] = stable(&first, owner_of(&first.db, ask));
    }
    let flushed = first.installed.flush().unwrap();
    assert_eq!(flushed.written, 2, "the body's MIR and its closure's, each a record: {flushed:?}");
    assert_eq!(memory.len(), 4, "a shape and an answer each");
    (memory, defs, forms)
}

/// Where `owner`'s MIR answer is in `memory`, from its shape in slot 0: its store key and its
/// answer key.
fn answer_of<'db>(memory: &Memory, s: &'db Session, owner: MirOwner<'db>) -> (blaze_ids::ContentId, blaze_ids::ContentId) {
    let (def, schema) = place(&s.db, owner).unwrap();
    let identity = s.facts.identity(&s.db, def).expect("a module's function has an identity");
    let base = record::base_key(&identity, Step::Mir, &schema, &solver_version(&BUILD));
    let shape_key = record::shape_key(&base, 0);
    let shape = memory.get(&record::shape_at(Step::Mir, &shape_key)).unwrap().expect("a shape in slot 0");
    let shape = ShapeRef::open(shape.bytes(), &base).unwrap();
    let mut key = AnswerKey::new(&shape_key);
    for fact in shape.facts() {
        key.value(&s.facts.value(&s.db, fact).unwrap());
    }
    let answer_key = key.finish();
    (record::answer_at(Step::Mir, &answer_key), answer_key)
}

#[test]
fn a_body_and_its_closure_are_two_records_under_the_body() {
    let db = db_of(FIXTURE);
    let def = function(&db, "closures");
    let body = MirOwner::Body(InferBodyId::DefWithBodyId(def));
    let closure = MirOwner::Closure(closure_of(&db, def));
    let (body_def, body_schema) = place(&db, body).unwrap();
    let (closure_def, closure_schema) = place(&db, closure).unwrap();
    assert_eq!((body_def, closure_def), (def, def), "a closure's MIR is filed under its body");
    assert_ne!(body_schema, closure_schema, "and apart from the body's own");
}

#[test]
fn stored_mir_is_read_in_a_fresh_database_without_building_it() {
    let (memory, defs, forms) = stored();
    let fresh = session(second(FIXTURE), &memory, &defs, None, Mode::Serve);
    assert_ne!(function(&db_of(FIXTURE), "closures"), function(&fresh.db, "closures"), "the two databases' ids differ");
    reset_ran();
    for closure in [false, true] {
        let ask = ask(&fresh, closure);
        let owner = owner_of(&fresh.db, ask);
        let read = built_mir(&fresh.db, ask).as_ref().expect("a stored MIR is `Ok`");
        assert_eq!(ran(), 0, "{owner:?} is read, not built");
        assert_eq!(*read, mir_of(&fresh.db, owner), "{owner:?} reads back as the MIR built here");
        assert_eq!(stable(&fresh, owner), forms[usize::from(closure)], "{owner:?}: both databases write the same bytes");
    }
    assert_eq!(fresh.installed.flush().unwrap(), Flushed::default(), "a hit offers nothing");
}

#[test]
fn a_lowering_error_is_never_stored() {
    let memory = Arc::new(Memory::default());
    let defs = Defs::default();
    let mut first = session(db_of(FIXTURE), &memory, &defs, None, Mode::Serve);
    let failing = Ask::new(&first.db, function(&first.db, "closures"), false, true);
    assert!(built_mir(&first.db, failing).is_err());
    assert_eq!(first.installed.flush().unwrap(), Flushed::default(), "an error is never offered");
    assert_eq!(memory.len(), 0);

    failing.set_fails(&mut first.db).to(false);
    reset_ran();
    assert!(built_mir(&first.db, failing).is_ok());
    assert_eq!(ran(), 1, "the body is built once it lowers");
    assert_eq!(first.installed.flush().unwrap().written, 1);
}

#[test]
fn a_corrupt_answer_is_an_error_naming_the_body() {
    let (memory, defs, _) = stored();
    let probe = session(db_of(FIXTURE), &Arc::new(Memory::default()), &defs, None, Mode::Serve);
    let (at, answer_key) = answer_of(&memory, &probe, owner_of(&probe.db, ask(&probe, false)));
    drop(probe);

    let mut garbage = vec![0u8; 256];
    garbage[..16].copy_from_slice(b"not a MIR answer");
    memory.set(at, &record::write_answer(&answer_key, &garbage));
    let fresh = session(second(FIXTURE), &memory, &defs, None, Mode::Serve);
    let ask = ask(&fresh, false);
    let def = ask.def(&fresh.db);
    reset_ran();
    let read = built_mir(&fresh.db, ask).as_ref().expect("built again, it lowers");
    assert_eq!(ran(), 1, "the body is built, which gives the same MIR");
    assert_eq!(*read, mir_of(&fresh.db, owner_of(&fresh.db, ask)));
    let error = fresh.installed.flush().expect_err("an answer that does not decode");
    assert!(error.contains(&format!("{def:?}")) && error.contains("MIR"), "{error}");
}

#[test]
fn verify_builds_every_hit_and_a_difference_is_an_error_naming_the_body() {
    let (memory, defs, _) = stored();
    let same = session(second(FIXTURE), &memory, &defs, None, Mode::Verify);
    reset_ran();
    for closure in [false, true] {
        let _ = built_mir(&same.db, ask(&same, closure));
    }
    assert_eq!(ran(), 2, "every hit is built as well");
    assert_eq!(same.installed.flush().unwrap(), Flushed::default(), "each stored MIR is the one built");
    drop(same);

    // The body's answer swapped for its closure's: it decodes, and differs.
    let probe = session(db_of(FIXTURE), &Arc::new(Memory::default()), &defs, None, Mode::Serve);
    let (at, body_key) = answer_of(&memory, &probe, owner_of(&probe.db, ask(&probe, false)));
    let closure_bytes = stable(&probe, owner_of(&probe.db, ask(&probe, true)));
    drop(probe);
    memory.set(at, &record::write_answer(&body_key, &closure_bytes));
    let wrong = session(second(FIXTURE), &memory, &defs, None, Mode::Verify);
    let ask = ask(&wrong, false);
    let def = ask.def(&wrong.db);
    let _ = built_mir(&wrong.db, ask);
    let error = wrong.installed.flush().expect_err("a stored MIR that differs from building it");
    assert!(error.contains(&format!("{def:?}")) && error.contains("differs"), "{error}");
}

#[test]
fn a_hit_allocates_only_the_decoded_mir() {
    let (memory, defs, _) = stored();
    let fresh = session(second(FIXTURE), &memory, &defs, None, Mode::Serve);
    for closure in [false, true] {
        let owner = owner_of(&fresh.db, ask(&fresh, closure));
        let (def, schema) = place(&fresh.db, owner).unwrap();
        // Warm: the identity, the facts' values and every type the MIR interns.
        assert!(matches!(lookup_mir(&fresh.db, def, owner, schema), Lookup::Hit { .. }));
        let (at, answer_key) = answer_of(&memory, &fresh, owner);
        let answer = memory.get(&at).unwrap().unwrap();
        let body = Aligned::copy_from(record::open_answer(answer.bytes(), &answer_key).unwrap());
        let (decoded, decoding) = measure(|| decode(&fresh.db, &*fresh.facts, def, &body));
        drop(decoded.unwrap());
        let (found, hit) = measure(|| lookup_mir(&fresh.db, def, owner, schema));
        assert!(matches!(found, Lookup::Hit { .. }));
        drop(found);
        assert!(
            hit.allocations <= decoding.allocations,
            "{owner:?}: a hit made {} allocations, decoding its answer {}",
            hit.allocations,
            decoding.allocations
        );
    }
}

#[test]
fn no_store_builds_as_without_one() {
    let db = second(FIXTURE);
    let def = function(&db, "closures");
    let owner = MirOwner::Body(InferBodyId::DefWithBodyId(def));
    let (_, schema) = place(&db, owner).unwrap();
    assert!(matches!(lookup_mir(&db, def, owner, schema), Lookup::Off));
    let ask = Ask::new(&db, def, false, false);
    assert_eq!(*built_mir(&db, ask).as_ref().unwrap(), mir_of(&db, owner_of(&db, ask)));
}
