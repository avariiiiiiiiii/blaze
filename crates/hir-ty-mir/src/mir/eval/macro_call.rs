//! A call of a function from outside any body: string slices in, a `String` out. This is how a
//! const macro is expanded at stage 0 (docs/future/const-macros.md): its input tokens are given
//! as text and its output is read back as text.

use hir_def::FunctionId;

use super::{
    Address, Evaluator, Heap, Interval, IntervalOrOwned, MirEvalError, Result, word_bytes,
};
use crate::db::HirDatabase;

/// Calls the function `func`, which takes `args.len()` `&str`s and returns a `String`, and
/// gives the string it returned. `func` is not generic.
pub fn call_with_strs<'db>(
    db: &'db dyn HirDatabase,
    func: FunctionId,
    args: &[&str],
) -> Result<'db, String> {
    let body =
        db.runtime_mir_body(func.into()).map_err(|e| MirEvalError::MirLowerError(func, e))?;
    let mut evaluator = Evaluator::new(db, func.into(), false, None)?;
    let mut slices = Vec::with_capacity(args.len());
    for arg in args {
        let data = evaluator.heap_allocate(arg.len().max(1), 1)?;
        evaluator.write_memory(data, arg.as_bytes())?;
        let mut slice = data.to_bytes().to_vec();
        slice.extend_from_slice(&word_bytes(arg.len()));
        slices.push(IntervalOrOwned::Owned(slice));
    }
    let result = evaluator.interpret_mir(body, slices.into_iter(), None)?;
    read_string(&evaluator, result)
}

/// The text of a `String` (or anything laid out as one: a pointer to its bytes, its capacity
/// and its length, in whichever order) that the evaluator holds at `value`.
fn read_string<'db>(evaluator: &Evaluator<'db>, value: Interval) -> Result<'db, String> {
    let ptr_size = evaluator.ptr_size();
    if value.size != 3 * ptr_size {
        return Err(MirEvalError::NotSupported(format!(
            "a const macro returns `String`; its function returned {} bytes",
            value.size
        )));
    }
    let bytes = value.get(evaluator)?;
    let mut words = Vec::with_capacity(3);
    for word in bytes.chunks(ptr_size) {
        words.push(from_bytes!(usize, word));
    }
    let Some(addr) =
        words.iter().map(|&it| Address::from_usize(it)).find(|it| matches!(it, Heap(_)))
    else {
        // An empty `String` holds a dangling pointer, not one into the heap.
        return Ok(String::new());
    };
    // The capacity is at least the length.
    let len = words
        .iter()
        .copied()
        .filter(|&it| !matches!(Address::from_usize(it), Heap(_)))
        .min()
        .unwrap_or(0);
    let text = evaluator.read_memory(addr, len)?;
    String::from_utf8(text.to_vec()).map_err(|_| {
        MirEvalError::UndefinedBehavior(
            "a const macro returned a `String` that is not UTF-8".into(),
        )
    })
}
