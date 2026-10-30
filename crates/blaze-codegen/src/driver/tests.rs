//! The build over a front end and backend made up here: what is compiled, kept, shared and
//! renamed, without a database.

use std::{
    collections::{BTreeMap, HashMap},
    sync::{
        Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

use blaze_ids::{ContentId, Hasher};
use blaze_items::{CodeHash, ItemKey};

use super::*;
use crate::{
    Arch, Backend, Init, RelocKind,
    plan::{Mode, OptLevel, Override, Profile},
    settings::{DebugInfo, Panic, Relocation, Sanitize, Toolchain},
    summary::{Attrs, Call, Hint},
};

#[derive(Clone)]
struct Function {
    body: u8,
    calls: Vec<u32>,
    statics: Vec<u32>,
    origin: Origin,
    symbol: Option<String>,
    /// The places among its references it builds in.
    built_in: Vec<u32>,
}

#[derive(Default)]
struct Program {
    functions: HashMap<u32, Function>,
    /// Each static's value byte and section.
    statics: HashMap<u32, (u8, Option<String>)>,
    /// The functions of each level the walk reached, in the order it told them.
    reached: Mutex<Vec<Vec<u32>>>,
    stop: AtomicBool,
}

impl Program {
    fn function(&mut self, id: u32, body: u8, calls: &[u32]) -> &mut Function {
        let function = Function {
            body,
            calls: calls.to_vec(),
            statics: Vec::new(),
            origin: Origin::Member("app".to_owned()),
            symbol: None,
            built_in: Vec::new(),
        };
        self.functions.insert(id, function);
        self.functions.get_mut(&id).unwrap()
    }
}

fn item(id: u32) -> ItemKey {
    ItemKey(ContentId::of("blaze v2 test item 1", &id.to_le_bytes()))
}

fn static_item(id: u32) -> ItemKey {
    ItemKey(ContentId::of("blaze v2 test static 1", &id.to_le_bytes()))
}

impl Frontend for Program {
    type Instance = u32;
    type Static = u32;

    fn describe(&self, id: &u32, _: &Origin) -> Result<Described<u32, u32>, CodegenError> {
        let function = &self.functions[id];
        let refs = function
            .calls
            .iter()
            .map(|it| FrontRef::Function(*it))
            .chain(function.statics.iter().map(|it| FrontRef::Static(*it)))
            .chain([FrontRef::Extern("memcpy".to_owned())])
            .collect();
        // The code takes callees by their interface, here their number: not their bodies.
        let mut code = Hasher::new("blaze v2 test code 1");
        code.update(&id.to_le_bytes()).update(&[function.body]);
        for callee in &function.calls {
            code.update(&callee.to_le_bytes());
        }
        Ok(Described {
            item: item(*id),
            code: CodeHash(code.finish()),
            args: ContentId::default(),
            origin: function.origin.clone(),
            overflow_checks: None,
            address: match function.symbol {
                Some(_) => Address::Significant,
                None => Address::Free,
            },
            symbol: function.symbol.clone(),
            refs,
            built_in: function.built_in.clone(),
            name: format!("f{id}"),
        })
    }

    fn describe_static(&self, id: &u32) -> Result<DescribedStatic<u32, u32>, CodegenError> {
        let (value, section) = &self.statics[id];
        Ok(DescribedStatic {
            item: static_item(*id),
            instance: ContentId::default(),
            value: ContentId::of("blaze v2 test value 1", &[*value]),
            mutable: false,
            thread_local: false,
            section: section.clone(),
            origin: Origin::Member("app".to_owned()),
            symbol: None,
            refs: Vec::new(),
        })
    }

    fn reached(&self, functions: &[(u32, Origin)], _: &[u32]) {
        self.reached.lock().unwrap().push(functions.iter().map(|it| it.0).collect());
    }

    fn stopped(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }
}

#[derive(Default)]
struct Machine {
    prepared: AtomicUsize,
    emitted: AtomicUsize,
    summarized: AtomicUsize,
    statics: AtomicUsize,
}

/// A function's body byte, and how many places it has.
struct Prepared {
    body: u8,
    refs: usize,
}

fn bcmp() -> Target {
    Target::Extern("bcmp".to_owned())
}

fn code(prepared: &Prepared) -> Vec<u8> {
    let mut code = vec![prepared.body];
    code.extend(std::iter::repeat_n([0xe8, 0, 0, 0, 0], prepared.refs).flatten());
    code
}

impl Prepare for Machine {
    type Frontend = Program;
    type Prepared = Prepared;
    type Export = u8;

    fn prepare(
        &self,
        program: &Program,
        unit: &Unit,
        _: &Spec,
        id: &u32,
        refs: &crate::template::Refs,
    ) -> Result<(Shape, Prepared), CodegenError> {
        self.prepared.fetch_add(1, Ordering::Relaxed);
        let prepared = Prepared { body: program.functions[id].body, refs: refs.len() };
        Ok((Shape::of(unit, &code(&prepared)), prepared))
    }

    fn summarize(
        &self,
        unit: &Unit,
        _: &Spec,
        prepared: &mut Prepared,
        refs: &crate::template::Refs,
    ) -> Result<(Summary, u8), CodegenError> {
        self.summarized.fetch_add(1, Ordering::Relaxed);
        let calls = refs
            .iter()
            .filter_map(|it| match it {
                Target::Function(key) | Target::Direct(key) => {
                    Some(Call { callee: *key, count: 1, args: Vec::new() })
                }
                _ => None,
            })
            .collect();
        let summary = Summary {
            key: unit.instance.key(),
            size: 4,
            hint: Hint::None,
            calls,
            addressed: Vec::new(),
            params: 0,
            unused: Vec::new(),
            attrs: Attrs::default(),
            body: Attrs::default(),
            external: true,
        };
        Ok((summary, prepared.body))
    }

    fn emit(
        &self,
        unit: &Unit,
        _: &Spec,
        prepared: Prepared,
        refs: &crate::template::Refs,
        imports: Option<(&Imports, &[Import<'_, u8>])>,
    ) -> Result<Emitted, CodegenError> {
        self.emitted.fetch_add(1, Ordering::Relaxed);
        let mut relocs: Vec<crate::Relocation> = (0..prepared.refs)
            .map(|at| crate::Relocation {
                at: 2 + 5 * at as u32,
                kind: RelocKind::PcRel32,
                target: refs[at].clone(),
                addend: -4,
            })
            .collect();
        // A release compile calls `bcmp` of its own accord, as LLVM does for a `memcmp`.
        let mut extra = Vec::new();
        if imports.is_some() {
            relocs.push(crate::Relocation { at: 0, kind: RelocKind::PcRel32, target: bcmp(), addend: -4 });
            extra.push("bcmp".to_owned());
        }
        let mut code = code(&prepared);
        // What it built in shows in its code, as an import's body would.
        for import in imports.map_or(&[][..], |it| it.1) {
            code.push(*import.export);
        }
        let function = CompiledFunction {
            key: unit.instance.key(),
            arch: unit.arch,
            align: 16,
            code,
            relocs: relocs.into(),
            data: Vec::new(),
            inlined: Vec::new(),
            side: None,
            unwind: None,
            lines: Vec::new(),
            coverage: None,
        };
        Ok(Emitted { function, side: None, extra })
    }

    fn masked(&self, _: &Unit, _: &Prepared, _: &crate::template::Refs) -> Option<Masked> {
        None
    }

    fn merge(
        &self,
        group: &Group,
        _: Vec<(&Unit, &Spec, Prepared, &crate::template::Refs)>,
    ) -> Result<fold::Merged, CodegenError> {
        Err(error(group.key(), item(0), "nothing folds here"))
    }

    fn compile_static(
        &self,
        program: &Program,
        _: &Spec,
        id: &u32,
        _: &crate::template::Refs,
    ) -> Result<Static, CodegenError> {
        self.statics.fetch_add(1, Ordering::Relaxed);
        let (value, section) = &program.statics[id];
        Ok(Static {
            item: static_item(*id),
            instance: ContentId::default(),
            align: 8,
            init: Init::Bytes { bytes: vec![*value; 8], relocs: crate::Relocs::new() },
            data: Vec::new(),
            mutable: false,
            thread_local: false,
            section: section.clone(),
        })
    }
}

#[derive(Default)]
struct Store(BTreeMap<Key, Vec<u8>>);

impl Lookup for Store {
    fn read(&self, key: &Key) -> Result<Option<&[u8]>, String> {
        Ok(self.0.get(key).map(Vec::as_slice))
    }
}

impl Store {
    fn keep(&mut self, puts: &[Put]) {
        for put in puts {
            self.0.insert(put.key, put.bytes.clone());
        }
    }
}

fn policy(level: OptLevel, mode: Mode) -> Policy {
    Policy {
        toolchain: Toolchain {
            id: ContentId([1; 32]),
            target: "x86_64-unknown-linux-gnu".to_owned(),
            arch: Arch::X86_64,
            cpu: "x86-64-v3".to_owned(),
            features: Vec::new(),
            cranelift: "0.136".to_owned(),
            llvm: Some("23.1".to_owned()),
            frame_pointers: false,
            relocation: Relocation::Pic,
        },
        profile: Profile {
            opt_level: level,
            overflow_checks: true,
            debug_assertions: true,
            debuginfo: DebugInfo::Full,
            panic: Panic::Unwind,
            packages: BTreeMap::new(),
            build_override: Override::default(),
        },
        mode,
        fold: false,
        sanitize: Sanitize::default(),
    }
}

fn member() -> Origin {
    Origin::Member("app".to_owned())
}

/// `main` (1) calls 2 and 3; 2 calls 4, 3 calls 5; 4 and 5 differ; 2 and 3 are one shape.
fn program() -> Program {
    let mut program = Program::default();
    program.function(1, 0x11, &[2, 3]).symbol = Some("main".to_owned());
    program.function(2, 0x22, &[4]);
    program.function(3, 0x22, &[5]);
    program.function(4, 0x44, &[]);
    program.function(5, 0x55, &[]);
    program
}

fn build(
    program: &Program,
    machine: &Machine,
    policy: &Policy,
    memo: &Memo,
    store: &Store,
) -> Result<Built, Vec<CodegenError>> {
    Build::new(machine, program, policy, memo).run(&[(1, member())], store)
}

#[test]
fn a_cold_build_compiles_once_per_shape_and_a_warm_one_compiles_nothing() {
    let (program, policy) = (program(), policy(OptLevel::O0, Mode::Output));
    let (machine, memo, mut store) = (Machine::default(), Memo::new(), Store::default());
    let cold = build(&program, &machine, &policy, &memo, &store).unwrap();
    assert_eq!(cold.stats.instances, 5);
    // 2 and 3 are one shape up to their callees: one compile, one memo hit.
    assert_eq!(machine.emitted.load(Ordering::Relaxed), 4);
    assert_eq!(cold.stats.memo, 1);
    assert_eq!(cold.functions.len(), 5);
    assert_eq!(cold.roots.len(), 1);
    assert_eq!(cold.symbols, vec![(cold.roots[0], "main".to_owned())]);

    store.keep(&cold.puts);
    let warm_machine = Machine::default();
    let warm = build(&program, &warm_machine, &policy, &Memo::new(), &store).unwrap();
    assert_eq!(warm_machine.prepared.load(Ordering::Relaxed), 0);
    assert_eq!(warm_machine.emitted.load(Ordering::Relaxed), 0);
    assert_eq!(warm.stats.stored, 5);
    assert_eq!(warm.functions, cold.functions);
}

#[test]
fn the_front_end_hears_each_level_before_it_and_a_stop_ends_the_build() {
    let (program, policy) = (program(), policy(OptLevel::O0, Mode::Output));
    let (machine, memo, store) = (Machine::default(), Memo::new(), Store::default());
    build(&program, &machine, &policy, &memo, &store).unwrap();
    assert_eq!(*program.reached.lock().unwrap(), [vec![1], vec![2, 3], vec![4, 5]]);

    program.stop.store(true, Ordering::Relaxed);
    let stopped = Machine::default();
    let errors = build(&program, &stopped, &policy, &Memo::new(), &store).unwrap_err();
    assert!(errors.iter().all(|it| it.what.starts_with("stopped")), "{errors:?}");
    assert_eq!(stopped.prepared.load(Ordering::Relaxed), 0);
}

#[test]
fn a_callee_edit_renames_its_callers_and_compiles_only_it() {
    let policy = policy(OptLevel::O0, Mode::Output);
    let mut program = program();
    let memo = Memo::new();
    let mut store = Store::default();
    let before = build(&program, &Machine::default(), &policy, &memo, &store).unwrap();
    store.keep(&before.puts);

    program.functions.get_mut(&4).unwrap().body = 0x45;
    let machine = Machine::default();
    let after = build(&program, &machine, &policy, &Memo::new(), &store).unwrap();
    assert_eq!(machine.emitted.load(Ordering::Relaxed), 1);
    let key_of = |built: &Built, body: u8| {
        built.functions.iter().find(|it| it.code[0] == body).map(|it| it.key).unwrap()
    };
    let (old, new) = (key_of(&before, 0x44), key_of(&after, 0x45));
    assert_ne!(old, new);
    // 2 is the same template; its call now names the new 4.
    let caller = after.functions.iter().find(|it| it.relocs.targets().contains(&Target::Direct(new)));
    assert!(caller.is_some_and(|it| it.code[0] == 0x22));
    assert!(after.functions.iter().all(|it| !it.relocs.targets().contains(&Target::Direct(old))));
}

#[test]
fn equal_code_is_one_function_unless_its_address_is_significant() {
    let policy = policy(OptLevel::O0, Mode::Output);
    let mut program = Program::default();
    program.function(1, 0x11, &[2, 3]).symbol = Some("main".to_owned());
    program.function(2, 0x77, &[]);
    program.function(3, 0x77, &[]);
    let built = build(&program, &Machine::default(), &policy, &Memo::new(), &Store::default()).unwrap();
    assert_eq!(built.deduped.merged.len(), 1);
    assert_eq!(built.functions.len(), 2);

    program.functions.get_mut(&3).unwrap().symbol = Some("exported".to_owned());
    let built = build(&program, &Machine::default(), &policy, &Memo::new(), &Store::default()).unwrap();
    assert!(built.deduped.merged.is_empty());
    assert_eq!(built.functions.len(), 3);
}

#[test]
fn a_root_reaches_what_it_calls_and_an_edit_elsewhere_changes_nothing_it_reaches() {
    let policy = policy(OptLevel::O0, Mode::Output);
    let mut program = program();
    let built = build(&program, &Machine::default(), &policy, &Memo::new(), &Store::default()).unwrap();
    let key = |built: &Built, name: &str| built.debug_names.iter().find(|it| it.1 == name).unwrap().0;
    assert_eq!(built.inputs.len(), 5);
    assert!(built.inputs.is_sorted_by_key(|it| it.0));
    assert_eq!(built.debug_names.len(), 5);
    let all = built.reached(&built.roots[0]);
    let two = built.reached(&key(&built, "f2"));
    // 2's code and 4's, and the arguments both have.
    assert_eq!(two.len(), 3);
    assert!(two.iter().all(|it| all.contains(it)) && two.len() < all.len());
    assert!(all.is_sorted());

    program.functions.get_mut(&5).unwrap().body = 0x56;
    let after = build(&program, &Machine::default(), &policy, &Memo::new(), &Store::default()).unwrap();
    assert_eq!(after.reached(&key(&after, "f2")), two);
    assert_ne!(after.reached(&after.roots[0]), all);
}

#[test]
fn a_static_root_is_built_though_no_code_names_it() {
    let policy = policy(OptLevel::O0, Mode::Output);
    let mut program = program();
    program.statics.insert(7, (0xaa, None));
    let (machine, memo, store) = (Machine::default(), Memo::new(), Store::default());
    let without = build(&program, &machine, &policy, &memo, &store).unwrap();
    assert!(without.statics.is_empty());
    let with = Build::new(&machine, &program, &policy, &memo).run_with_statics(&[(1, member())], &[7], &store).unwrap();
    assert_eq!(with.statics.len(), 1);
    assert_eq!(with.statics[0].item, static_item(7));
    assert_eq!(with.functions.len(), without.functions.len());
}

#[test]
fn statics_are_kept_by_which_they_are() {
    let policy = policy(OptLevel::O0, Mode::Output);
    let mut program = program();
    program.statics.insert(7, (0xaa, None));
    program.statics.insert(8, (0xaa, None));
    program.functions.get_mut(&4).unwrap().statics = vec![7, 8];
    let machine = Machine::default();
    let mut store = Store::default();
    let built = build(&program, &machine, &policy, &Memo::new(), &store).unwrap();
    // Equal values, two statics, each compiled: a cold build reads the store before either.
    assert_eq!(built.statics.len(), 2);
    assert_eq!(machine.statics.load(Ordering::Relaxed), 2);
    let caller = built.functions.iter().find(|it| it.code[0] == 0x44).unwrap();
    let statics = caller.relocs.iter().filter(|it| matches!(it.target, Target::Static { .. })).count();
    assert_eq!(statics, 2);
    store.keep(&built.puts);
    let warm = Machine::default();
    let again = build(&program, &warm, &policy, &Memo::new(), &store).unwrap();
    assert_eq!(warm.statics.load(Ordering::Relaxed), 0);
    assert_eq!(again.statics, built.statics);
}

#[test]
fn a_static_is_kept_with_its_section() {
    let policy = policy(OptLevel::O0, Mode::Output);
    let mut program = program();
    program.statics.insert(7, (0xaa, None));
    program.statics.insert(8, (0xaa, Some(".init_array".to_owned())));
    program.functions.get_mut(&4).unwrap().statics = vec![7, 8];
    let mut store = Store::default();
    let built = build(&program, &Machine::default(), &policy, &Memo::new(), &store).unwrap();
    store.keep(&built.puts);
    let warm = Machine::default();
    let again = build(&program, &warm, &policy, &Memo::new(), &store).unwrap();
    assert_eq!(warm.statics.load(Ordering::Relaxed), 0);
    let sections: Vec<_> = again.statics.iter().map(|it| it.section.clone()).collect();
    let item_order = static_item(7) < static_item(8);
    let want = if item_order { vec![None, Some(".init_array".to_owned())] } else { vec![Some(".init_array".to_owned()), None] };
    assert_eq!(sections, want);
}

#[test]
fn a_release_build_summarizes_decides_and_keeps_by_imports() {
    let (program, policy) = (program(), policy(OptLevel::O3, Mode::Output));
    let (machine, mut store) = (Machine::default(), Store::default());
    let cold = build(&program, &machine, &policy, &Memo::new(), &store).unwrap();
    assert_eq!(machine.summarized.load(Ordering::Relaxed), 5);
    assert_eq!(cold.functions.len(), 5);
    assert!(cold.functions.iter().all(|it| it.arch == Arch::X86_64));
    // What the backend called of its own accord is kept by name, though no reference lists it.
    assert!(cold.functions.iter().all(|it| it.relocs.targets().contains(&bcmp())));
    store.keep(&cold.puts);

    let warm = Machine::default();
    let again = build(&program, &warm, &policy, &Memo::new(), &store).unwrap();
    assert_eq!(warm.prepared.load(Ordering::Relaxed), 0);
    assert_eq!(warm.summarized.load(Ordering::Relaxed), 0);
    assert_eq!(again.functions, cold.functions);
}

#[test]
fn a_live_build_starts_at_cranelift_and_tiers_up() {
    let mut policy = policy(OptLevel::O0, Mode::Live);
    policy.profile.packages.insert(
        "*".to_owned(),
        Override { opt_level: Some(OptLevel::O3), ..Override::default() },
    );
    let mut program = program();
    program.functions.get_mut(&4).unwrap().origin = Origin::Dependency("memchr".to_owned());
    let machine = Machine::default();
    let memo = Memo::new();
    let mut build = Build::new(&machine, &program, &policy, &memo);
    let built = build.run(&[(1, member())], &Store::default()).unwrap();
    assert_eq!(built.tiered.len(), 1);
    // Members and the tiered function are reached through their slots.
    let tiered = built.tiered[0];
    assert!(built.functions.iter().any(|it| it.relocs.targets().contains(&Target::Function(tiered))));
    assert_eq!(machine.summarized.load(Ordering::Relaxed), 0);
    let up = build.tier_up(&Store::default()).unwrap();
    assert_eq!(up.replaced.len(), 1);
    assert_eq!(up.replaced[0].0, tiered);
    assert_ne!(up.replaced[0].1.key, tiered);
}

#[test]
fn a_tiered_callee_may_be_built_in_but_a_patchable_one_may_not() {
    let mut policy = policy(OptLevel::O0, Mode::Live);
    policy.profile.packages.insert(
        "*".to_owned(),
        Override { opt_level: Some(OptLevel::O3), ..Override::default() },
    );
    let mut program = program();
    program.functions.get_mut(&4).unwrap().origin = Origin::Dependency("memchr".to_owned());
    program.functions.get_mut(&2).unwrap().built_in = vec![0];
    let built = Build::new(&Machine::default(), &program, &policy, &Memo::new())
        .run(&[(1, member())], &Store::default())
        .unwrap();
    assert_eq!(built.tiered.len(), 1);

    program.functions.get_mut(&1).unwrap().built_in = vec![0];
    let errors = Build::new(&Machine::default(), &program, &policy, &Memo::new())
        .run(&[(1, member())], &Store::default())
        .unwrap_err();
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].item, item(1));
    assert!(errors[0].what.contains("which is patchable"), "{}", errors[0].what);
}

#[test]
fn a_store_entry_that_does_not_read_is_an_error_not_a_compile() {
    let (program, policy) = (program(), policy(OptLevel::O0, Mode::Output));
    let mut store = Store::default();
    let cold = build(&program, &Machine::default(), &policy, &Memo::new(), &store).unwrap();
    store.keep(&cold.puts);
    for bytes in store.0.values_mut() {
        bytes.truncate(3);
    }
    let machine = Machine::default();
    let errors = build(&program, &machine, &policy, &Memo::new(), &store).unwrap_err();
    assert_eq!(errors.len(), 5);
    assert_eq!(machine.emitted.load(Ordering::Relaxed), 0);
}

/// A read of the store hands its bytes to the decoder in place: a warm hit allocates nothing
/// the decoder does not.
#[test]
fn a_store_hit_is_read_in_place() {
    let key = keys::template(&InstanceKey(ContentId([1; 32])));
    let mut store = Store::default();
    store.0.insert(key, vec![7; 4096]);
    let unit = Unit {
        instance: Instance {
            code: CodeHash(ContentId::default()),
            args: ContentId::default(),
            settings: crate::Settings(ContentId::default()),
        },
        backend: Backend::Cranelift { speed: false },
        arch: Arch::X86_64,
        item: item(0),
    };
    let mut out = Out::default();
    let sum = |bytes: &[u8]| Ok::<u64, String>(bytes.iter().map(|&it| u64::from(it)).sum());
    assert!(matches!(out.read(&store, &key, &unit, sum), Read::Hit(28672)));
    let read = blaze_alloc_count::assert_at_most(0, "a warm store hit", || out.read(&store, &key, &unit, sum));
    assert!(matches!(read, Read::Hit(28672)));
    let missing = keys::template(&InstanceKey(ContentId([2; 32])));
    assert!(matches!(out.read(&store, &missing, &unit, sum), Read::Miss));
    assert!(out.errors.is_empty());
}

/// A stored template's function, read in place, is the one decoding it and instantiating
/// gives, and bytes that are no template are the outer error.
#[test]
fn a_stored_function_is_the_decoded_templates() {
    let (program, policy) = (program(), policy(OptLevel::O0, Mode::Output));
    let mut store = Store::default();
    let cold = build(&program, &Machine::default(), &policy, &Memo::new(), &store).unwrap();
    store.keep(&cold.puts);
    let key = InstanceKey(ContentId([1; 32]));
    let mut templates = 0;
    for bytes in store.0.values() {
        let Ok(template) = blob::decode(bytes) else { continue };
        templates += 1;
        for count in [template.refs, template.refs + 1] {
            let refs: Vec<Target> =
                (0..count).map(|n| Target::Function(InstanceKey(ContentId([n as u8 + 2; 32])))).collect();
            let made = stored_function(bytes, key, &refs).expect("a template");
            assert_eq!(made, template.instantiate(key, &refs));
        }
    }
    assert_eq!(templates, cold.functions.len());
    assert!(stored_function(&[0; 3], key, &[]).is_err());
}

#[test]
fn a_level_without_its_backend_names_the_function() {
    let mut policy = policy(OptLevel::O3, Mode::Output);
    policy.toolchain.llvm = None;
    let errors = build(&program(), &Machine::default(), &policy, &Memo::new(), &Store::default()).unwrap_err();
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].item, item(1));
}
