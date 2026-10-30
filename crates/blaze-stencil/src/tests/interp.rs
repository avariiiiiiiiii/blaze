//! `cranelift-interpreter` as the reference for what IR means, and the one rule the tests
//! compare results by.
//!
//! Against Cranelift's own machine code, stencil code must agree bit for bit, NaN payloads
//! included (stencil.md, "Semantics, op by op": the same hardware instructions). Against the
//! interpreter, which computes with the host's Rust float operations, a NaN only has to be a
//! NaN: Cranelift's semantics leave NaN bits nondeterministic, which is why its own
//! `cranelift-fuzzgen` canonicalizes NaNs before comparing with the interpreter.
//!
//! The interpreter runs everywhere; it needs no code mapped.

use cranelift_codegen::{data_value::DataValue, ir};
use cranelift_interpreter::{
    environment::FunctionStore,
    interpreter::{Interpreter, InterpreterState},
    step::{ControlFlow, CraneliftTrap},
};

/// Instructions the interpreter may step before a run is called endless.
const FUEL: u64 = 10_000_000;

/// How a run of the reference ended.
#[derive(Clone, PartialEq, Debug)]
pub enum Run {
    Returned(Vec<DataValue>),
    /// A trap with its code, or `None` for one with no code (`unreachable`, a bad signature).
    Trapped(Option<ir::TrapCode>),
}

/// Runs `funcs[entry]` on `args` in the interpreter. Each function is found by its name
/// (`func.name`, which is how a `call` in another one names it: `u0:3` or `%name`). An error
/// is what the interpreter can't do (an operation it lacks, its fuel run out).
pub fn run(funcs: &[&ir::Function], entry: usize, args: &[DataValue]) -> Result<Run, String> {
    let mut store = FunctionStore::default();
    for func in funcs {
        store.add(func.name.to_string(), func);
    }
    let state = InterpreterState::default().with_function_store(store);
    let mut interpreter = Interpreter::new(state).with_fuel(Some(FUEL));
    match interpreter.call_by_name(&funcs[entry].name.to_string(), args) {
        Ok(ControlFlow::Return(values)) => Ok(Run::Returned(values.to_vec())),
        Ok(ControlFlow::Trap(CraneliftTrap::User(code))) => Ok(Run::Trapped(Some(code))),
        Ok(ControlFlow::Trap(other)) => match format!("{other:?}") {
            it if it.contains("Unreachable") => Ok(Run::Trapped(None)),
            it => Err(format!("the interpreter stopped with {it}")),
        },
        Ok(_) => Err("the interpreter ended the call with no return".to_owned()),
        Err(e) => Err(format!("the interpreter: {e}")),
    }
}

/// How a call of machine code ended (`exec`'s calls in a forked child).
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Outcome<T> {
    Returned(T),
    /// The child ended by this signal: a trap (`SIGILL` for `ud2`/`udf`, `SIGFPE` for x86's
    /// divide, `SIGSEGV`/`SIGBUS` for a bad access, `SIGTRAP` for `brk`/`int3`).
    Trapped(i32),
}

impl<T> Outcome<T> {
    pub fn trapped(&self) -> bool {
        matches!(self, Outcome::Trapped(_))
    }
}

/// How strictly NaNs compare.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Nans {
    /// Bit for bit: stencil code against Cranelift's.
    Exact,
    /// Any NaN equals any NaN, lane by lane: either against the interpreter.
    Any,
}

/// Whether `a` and `b`, values of `ty`, are the same result.
pub fn same(ty: ir::Type, a: &DataValue, b: &DataValue, nans: Nans) -> bool {
    let (a, b) = (bytes(a), bytes(b));
    let width = ty.bytes() as usize;
    if a[..width] == b[..width] {
        return true;
    }
    if nans == Nans::Exact {
        return false;
    }
    let lane = ty.lane_type();
    if !lane.is_float() {
        return false;
    }
    let lane_bytes = lane.bytes() as usize;
    a[..width].chunks_exact(lane_bytes).zip(b[..width].chunks_exact(lane_bytes)).all(|(a, b)| {
        a == b || (is_nan(lane, a) && is_nan(lane, b))
    })
}

/// Whether every value of `a` and `b` (of `tys`) is the same result.
pub fn same_all(tys: &[ir::Type], a: &[DataValue], b: &[DataValue], nans: Nans) -> bool {
    a.len() == tys.len()
        && b.len() == tys.len()
        && tys.iter().zip(a.iter().zip(b)).all(|(ty, (a, b))| same(*ty, a, b, nans))
}

/// Whether `got` (of a backend's code) is what `reference` (the interpreter's run) says, for
/// a function returning `tys`: equal values with any NaN for a NaN, or a trap for a trap.
/// Trap codes are not compared: stencil code reports a trap by its signal only.
pub fn agrees(tys: &[ir::Type], reference: &Run, got: &Outcome<Vec<DataValue>>) -> Result<(), String> {
    match (reference, got) {
        (Run::Returned(want), Outcome::Returned(got)) => {
            if same_all(tys, want, got, Nans::Any) {
                Ok(())
            } else {
                Err(format!("the interpreter returns {want:?}, the code {got:?}"))
            }
        }
        (Run::Trapped(_), Outcome::Trapped(_)) => Ok(()),
        (Run::Returned(want), Outcome::Trapped(signal)) => {
            Err(format!("the interpreter returns {want:?}, the code traps (signal {signal})"))
        }
        (Run::Trapped(code), Outcome::Returned(got)) => {
            Err(format!("the interpreter traps ({code:?}), the code returns {got:?}"))
        }
    }
}

/// The 16 little-endian bytes of `value`, zero past its width.
pub fn bytes(value: &DataValue) -> [u8; 16] {
    let mut out = [0u8; 16];
    value.write_to_slice_le(&mut out);
    out
}

fn is_nan(lane: ir::Type, bytes: &[u8]) -> bool {
    let mut wide = [0u8; 16];
    wide[..bytes.len()].copy_from_slice(bytes);
    let bits = u128::from_le_bytes(wide);
    let (exp, man) = match lane.bits() {
        16 => (0x1f << 10, (1 << 10) - 1),
        32 => (0xff << 23, (1 << 23) - 1),
        64 => (0x7ff << 52, (1 << 52) - 1),
        128 => (0x7fff << 112, (1 << 112) - 1),
        _ => return false,
    };
    bits & exp == exp && bits & man != 0
}

#[test]
fn nans_compare_by_rule() {
    use cranelift_codegen::ir::{immediates::Ieee32, types};
    let quiet = DataValue::F32(Ieee32::with_bits(0x7fc0_0000));
    let payload = DataValue::F32(Ieee32::with_bits(0x7fc1_2345));
    let one = DataValue::F32(Ieee32::with_bits(0x3f80_0000));
    assert!(same(types::F32, &quiet, &payload, Nans::Any));
    assert!(!same(types::F32, &quiet, &payload, Nans::Exact));
    assert!(!same(types::F32, &quiet, &one, Nans::Any));
    let mut lanes = [0u8; 16];
    lanes[..4].copy_from_slice(&0x7fc0_0000u32.to_le_bytes());
    let mut other = lanes;
    other[..4].copy_from_slice(&0xffc0_0001u32.to_le_bytes());
    assert!(same(types::F32X4, &DataValue::V128(lanes), &DataValue::V128(other), Nans::Any));
    assert!(!same(types::I32X4, &DataValue::V128(lanes), &DataValue::V128(other), Nans::Any));
}

#[test]
fn the_interpreter_runs_an_add() {
    use cranelift_codegen::ir::{InstBuilder, types};
    use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
    let mut sig = ir::Signature::new(cranelift_codegen::isa::CallConv::SystemV);
    sig.params.push(ir::AbiParam::new(types::I32));
    sig.returns.push(ir::AbiParam::new(types::I32));
    let mut func = ir::Function::with_name_signature(ir::UserFuncName::user(0, 0), sig);
    let mut cx = FunctionBuilderContext::new();
    let mut b = FunctionBuilder::new(&mut func, &mut cx);
    let entry = b.create_block();
    b.append_block_params_for_function_params(entry);
    b.switch_to_block(entry);
    let x = b.block_params(entry)[0];
    let y = b.ins().iadd_imm(x, 1);
    b.ins().return_(&[y]);
    b.seal_all_blocks();
    let config = super::host::isa(&super::host::TARGETS[0]).frontend_config();
    b.finalize(config);
    assert_eq!(
        run(&[&func], 0, &[DataValue::I32(i32::MAX)]),
        Ok(Run::Returned(vec![DataValue::I32(i32::MIN)]))
    );
}
