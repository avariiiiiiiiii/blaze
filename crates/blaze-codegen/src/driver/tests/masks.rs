//! Masked IR kept across builds: a function unchanged since the build before is neither read
//! from the store nor hashed again.

use std::cell::Cell;

use super::*;

/// [`Machine`] whose functions can be folded: each body byte is its own form, too small for
/// a group to pay.
#[derive(Default)]
struct Folding(Machine);

impl Prepare for Folding {
    type Frontend = Program;
    type Prepared = Prepared;
    type Export = u8;

    fn prepare(
        &self,
        program: &Program,
        unit: &Unit,
        spec: &Spec,
        id: &u32,
        refs: &crate::template::Refs,
    ) -> Result<(Shape, Prepared), CodegenError> {
        self.0.prepare(program, unit, spec, id, refs)
    }

    fn summarize(
        &self,
        unit: &Unit,
        spec: &Spec,
        prepared: &mut Prepared,
        refs: &crate::template::Refs,
    ) -> Result<(Summary, u8), CodegenError> {
        self.0.summarize(unit, spec, prepared, refs)
    }

    fn emit(
        &self,
        unit: &Unit,
        spec: &Spec,
        prepared: Prepared,
        refs: &crate::template::Refs,
        imports: Option<(&Imports, &[Import<'_, u8>])>,
    ) -> Result<Emitted, CodegenError> {
        self.0.emit(unit, spec, prepared, refs, imports)
    }

    fn masked(&self, _: &Unit, prepared: &Prepared, _: &crate::template::Refs) -> Option<Masked> {
        let constants = vec![fold::Constant::Int { bits: 8, value: u128::from(prepared.body) }];
        Some(Masked { shape: vec![prepared.body], constants, size: 4 })
    }

    fn merge(
        &self,
        group: &Group,
        members: Vec<(&Unit, &Spec, Prepared, &crate::template::Refs)>,
    ) -> Result<fold::Merged, CodegenError> {
        self.0.merge(group, members)
    }

    fn compile_static(
        &self,
        program: &Program,
        spec: &Spec,
        id: &u32,
        refs: &crate::template::Refs,
    ) -> Result<Static, CodegenError> {
        self.0.compile_static(program, spec, id, refs)
    }
}

fn hashed() -> usize {
    fold::HASHED.with(Cell::get)
}

#[test]
fn an_unchanged_functions_masked_ir_is_hashed_once_over_two_builds() {
    let mut policy = policy(OptLevel::O0, Mode::Output);
    policy.fold = true;
    let mut program = program();
    let (machine, memo, masks) = (Folding::default(), Memo::new(), Masks::new());
    // The forms each build hashes: interning runs on the build's thread.
    let run = |program: &Program, store: &Store| {
        let before = hashed();
        let built = Build::new(&machine, program, &policy, &memo)
            .with_masks(&masks)
            .run(&[(1, member())], store)
            .unwrap();
        (built, hashed() - before)
    };

    let mut store = Store::default();
    let (cold, cold_hashed) = run(&program, &store);
    // Five functions, each hashed as it is first interned; 2 and 3 are one form.
    assert_eq!(cold_hashed, 5);
    assert_eq!(masks.forms(), 4);
    assert!(cold.functions.len() == 5 && cold.stats.folded == 0);
    let masked_puts = cold.puts.iter().filter(|it| it.kind == Kind::Query).count();
    assert_eq!(masked_puts, 5);

    store.keep(&cold.puts);
    let (warm, warm_hashed) = run(&program, &store);
    assert_eq!(warm_hashed, 0, "an unchanged function's masked IR is hashed again");
    assert_eq!(warm.functions, cold.functions);
    assert!(warm.puts.iter().all(|it| it.kind != Kind::Query), "kept masked IR is put again");

    // Nor read: with an empty store, the masks alone answer, and nothing is put for them.
    let (bare, bare_hashed) = run(&program, &Store::default());
    assert_eq!(bare_hashed, 0);
    assert_eq!(bare.functions, cold.functions);

    // An edit of 4 makes one new key: its masked IR alone is hashed.
    program.functions.get_mut(&4).unwrap().body = 0x45;
    let (_, edited_hashed) = run(&program, &store);
    assert_eq!(edited_hashed, 1);
    assert_eq!(masks.forms(), 5);
}

#[test]
fn builds_with_the_shared_masks_keep_across_builds() {
    let mut policy = policy(OptLevel::O0, Mode::Output);
    policy.fold = true;
    // A body no other test builds, so the process's masks have none of its keys yet.
    let mut program = Program::default();
    program.function(1, 0xc3, &[2]).symbol = Some("main".to_owned());
    program.function(2, 0xc4, &[]);
    let (machine, memo) = (Folding::default(), Memo::new());
    let before = hashed();
    Build::new(&machine, &program, &policy, &memo).run(&[(1, member())], &Store::default()).unwrap();
    let cold = hashed() - before;
    let before = hashed();
    Build::new(&machine, &program, &policy, &memo).run(&[(1, member())], &Store::default()).unwrap();
    assert_eq!((cold, hashed() - before), (2, 0));
}
