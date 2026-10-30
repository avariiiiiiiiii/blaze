//! What Cranelift reuses from one function to the next, kept per thread. A [`Context`] keeps the
//! capacity of its control flow graph, dominator tree, loop analysis and register allocator
//! through [`Context::clear`], and a [`FunctionBuilderContext`] that of its SSA tables once its
//! builder finalizes: Cranelift's own advice for throughput, and what wasmtime's compiler keeps
//! one of per thread. A function then allocates only what grows past the largest one the
//! thread built before.
//!
//! Builds nest (the body of a callee built in is built while its caller is), so each is a small
//! stack of spares rather than one.

#![forbid(unsafe_code)]

use std::cell::RefCell;

use cranelift_codegen::{Context, ir};
use cranelift_frontend::FunctionBuilderContext;

/// The most of each kind a thread keeps: deeper than this a nested build makes its own.
const KEPT: usize = 4;

/// A context whose function had more instructions than this is let go rather than kept: what
/// it grew to would be held by the thread for good.
const LARGEST: usize = super::SINGLE_PASS;

thread_local! {
    static CONTEXTS: RefCell<Vec<Context>> = const { RefCell::new(Vec::new()) };
    static BUILDERS: RefCell<Vec<FunctionBuilderContext>> = const { RefCell::new(Vec::new()) };
}

/// A cleared context: one the thread kept, or a new one.
pub(super) fn context() -> Context {
    CONTEXTS.with_borrow_mut(Vec::pop).unwrap_or_else(Context::new)
}

/// Keeps `context` for the thread's next function, cleared.
pub(super) fn keep_context(mut context: Context) {
    if context.func.dfg.num_insts() > LARGEST {
        return;
    }
    context.clear();
    CONTEXTS.with_borrow_mut(|kept| {
        if kept.len() < KEPT {
            kept.push(context);
        }
    });
}

/// A builder context with nothing in it: one the thread kept, or a new one.
pub(super) fn builder() -> FunctionBuilderContext {
    BUILDERS.with_borrow_mut(Vec::pop).unwrap_or_default()
}

/// Keeps `builder` for the thread's next function. It must be one a `FunctionBuilder` was
/// finalized with, which leaves it empty: one whose build failed half way is dropped instead.
pub(super) fn keep_builder(builder: FunctionBuilderContext) {
    BUILDERS.with_borrow_mut(|kept| {
        if kept.len() < KEPT {
            kept.push(builder);
        }
    });
}

/// Runs `work` on one of the thread's contexts holding `func`, then keeps the context: what v2
/// compiles a prepared function's IR with (`v2/backend.rs`), so that its register allocator,
/// dominator tree and control flow graph reuse what the thread's last compile grew them to.
pub(crate) fn with_context<T>(func: ir::Function, work: impl FnOnce(&mut Context) -> T) -> T {
    let mut context = context();
    context.func = func;
    let out = work(&mut context);
    keep_context(context);
    out
}

#[cfg(test)]
mod tests {
    use cranelift_codegen::ir::{self, InstBuilder as _, types};
    use cranelift_codegen::isa::CallConv;
    use cranelift_frontend::FunctionBuilder;

    use super::*;

    fn build(builder_ctx: &mut FunctionBuilderContext, func: &mut ir::Function, add: i64) {
        let mut builder = FunctionBuilder::new(func, builder_ctx);
        let block = builder.create_block();
        builder.append_block_params_for_function_params(block);
        builder.switch_to_block(block);
        let var = builder.declare_var(types::I64);
        let param = builder.block_params(block)[0];
        builder.def_var(var, param);
        let value = builder.use_var(var);
        let value = builder.ins().iadd_imm(value, add);
        builder.ins().return_(&[value]);
        builder.seal_all_blocks();
        builder.finalize(crate::target().unwrap().frontend_config());
    }

    fn signature() -> ir::Signature {
        let mut sig = ir::Signature::new(CallConv::SystemV);
        sig.params.push(ir::AbiParam::new(types::I64));
        sig.returns.push(ir::AbiParam::new(types::I64));
        sig
    }

    /// A function built with a kept builder context and in a kept context prints the same as
    /// one built fresh: nothing of the function before is left in either.
    #[test]
    fn a_kept_context_builds_what_a_fresh_one_does() {
        let fresh = {
            let mut func = ir::Function::with_name_signature(Default::default(), signature());
            build(&mut FunctionBuilderContext::new(), &mut func, 7);
            func.display().to_string()
        };
        for add in [3, 9] {
            let mut context = context();
            context.func.signature = signature();
            let mut builder_ctx = builder();
            build(&mut builder_ctx, &mut context.func, add);
            keep_builder(builder_ctx);
            keep_context(context);
        }
        let mut context = context();
        context.func.signature = signature();
        let mut builder_ctx = builder();
        build(&mut builder_ctx, &mut context.func, 7);
        assert_eq!(context.func.display().to_string(), fresh);
    }

    #[test]
    fn a_thread_keeps_at_most_a_few() {
        for _ in 0..2 * KEPT {
            keep_context(Context::new());
            keep_builder(FunctionBuilderContext::new());
        }
        assert_eq!(CONTEXTS.with_borrow(Vec::len), KEPT);
        assert_eq!(BUILDERS.with_borrow(Vec::len), KEPT);
    }

    #[test]
    fn with_context_holds_the_function_and_keeps_the_context() {
        CONTEXTS.with_borrow_mut(Vec::clear);
        let func = ir::Function::with_name_signature(Default::default(), signature());
        let params = with_context(func, |context| context.func.signature.params.len());
        assert_eq!(params, 1);
        assert_eq!(CONTEXTS.with_borrow(Vec::len), 1);
        let kept = context();
        assert_eq!(kept.func.signature.params.len(), 0);
    }
}
