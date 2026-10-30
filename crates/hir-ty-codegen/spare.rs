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

use std::{cell::RefCell, sync::Arc};

use cranelift_codegen::{Context, ir};
use cranelift_frontend::FunctionBuilderContext;

use crate::{Layout, debuginfo::LineRow, fragment::Relocs};

/// The most of each kind a thread keeps: deeper than this a nested build makes its own.
const KEPT: usize = 4;

/// A context whose function had more instructions than this is let go rather than kept: what
/// it grew to would be held by the thread for good.
const LARGEST: usize = super::SINGLE_PASS;

/// A kept list of relocations or line rows with room for more than this is let go rather than
/// kept, as a context of more than [`LARGEST`] instructions is.
const LARGEST_ROWS: usize = 1 << 16;

/// The lists of layouts a build asked for and of variants' layouts it made (`KeptLayouts`).
type LayoutLists = (Vec<Arc<Layout>>, Vec<Box<Layout>>);

thread_local! {
    // alloc: none, an empty shelf
    static CONTEXTS: RefCell<Vec<Context>> = const { RefCell::new(Vec::new()) };
    // alloc: none, an empty shelf
    static BUILDERS: RefCell<Vec<FunctionBuilderContext>> = const { RefCell::new(Vec::new()) };
    // alloc: none, an empty shelf
    static RELOCS: RefCell<Vec<Relocs>> = const { RefCell::new(Vec::new()) };
    // alloc: none, an empty shelf
    static LINES: RefCell<Vec<Vec<LineRow>>> = const { RefCell::new(Vec::new()) };
    // alloc: none, an empty shelf
    static LAYOUT_LISTS: RefCell<Vec<LayoutLists>> = const { RefCell::new(Vec::new()) };
    // alloc: none, an empty shelf
    static COPIES: RefCell<Vec<Vec<(intern::Symbol, u64)>>> = const { RefCell::new(Vec::new()) };
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

/// An empty list of relocations: one the thread kept, or a new one. v2's emit writes a
/// function's into it ([`super::emit_lent`]) and its conversion gives it back.
pub(super) fn relocs() -> Relocs {
    RELOCS.with_borrow_mut(Vec::pop).unwrap_or_default()
}

/// Keeps `relocs` for the thread's next function, emptied.
pub(super) fn keep_relocs(mut relocs: Relocs) {
    if relocs.capacity() > LARGEST_ROWS {
        return;
    }
    relocs.clear();
    RELOCS.with_borrow_mut(|kept| {
        if kept.len() < KEPT {
            kept.push(relocs);
        }
    });
}

/// An empty list of line rows: one the thread kept, or a new one, as [`relocs`].
pub(super) fn lines() -> Vec<LineRow> {
    LINES.with_borrow_mut(Vec::pop).unwrap_or_default()
}

/// Keeps `lines` for the thread's next function, emptied.
pub(super) fn keep_lines(mut lines: Vec<LineRow>) {
    if lines.capacity() > LARGEST_ROWS {
        return;
    }
    lines.clear();
    LINES.with_borrow_mut(|kept| {
        if kept.len() < KEPT {
            kept.push(lines);
        }
    });
}

/// Lends `f` an empty list for [`SymbolResolver::copies`](super::SymbolResolver::copies): one
/// the thread kept, or a new one, kept again after.
pub(super) fn with_copies<R>(f: impl FnOnce(&mut Vec<(intern::Symbol, u64)>) -> R) -> R {
    let mut copies = COPIES.with_borrow_mut(Vec::pop).unwrap_or_default();
    copies.clear();
    let result = f(&mut copies);
    if copies.capacity() <= LARGEST_ROWS {
        copies.clear();
        COPIES.with_borrow_mut(|kept| {
            if kept.len() < KEPT {
                kept.push(copies);
            }
        });
    }
    result
}

/// Empty lists for a build's layouts (`KeptLayouts`): ones the thread kept, or new ones.
pub(super) fn layout_lists() -> LayoutLists {
    LAYOUT_LISTS.with_borrow_mut(Vec::pop).unwrap_or_default()
}

/// Keeps a build's lists of layouts for the thread's next build, emptied. Emptying them drops
/// the layouts, so a kept list holds no `Arc` of salsa's past its build.
pub(super) fn keep_layout_lists(mut asked: Vec<Arc<Layout>>, mut variants: Vec<Box<Layout>>) {
    asked.clear();
    variants.clear();
    if asked.capacity().max(variants.capacity()) > LARGEST_ROWS {
        return;
    }
    LAYOUT_LISTS.with_borrow_mut(|kept| {
        if kept.len() < KEPT {
            kept.push((asked, variants));
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

    /// A kept list comes back empty with its room, and filling it again to what it held takes
    /// nothing of the global allocator.
    #[test]
    fn a_kept_list_of_rows_is_reused_whole() {
        use crate::fragment::{Reloc, RelocName};
        LINES.with_borrow_mut(Vec::clear);
        RELOCS.with_borrow_mut(Vec::clear);
        let row = |at: u32| Reloc {
            offset: at,
            kind: cranelift_codegen::binemit::Reloc::Abs8,
            name: RelocName::Func(at),
            addend: 0,
            elf_type: None,
        };
        let fill = || {
            let mut lines = lines();
            let mut relocs = relocs();
            assert!(lines.is_empty() && relocs.len() == 0);
            lines.extend((0..100).map(|offset| LineRow { offset, span: offset }));
            for at in 0..100 {
                relocs.push(row(at)).expect("a row");
            }
            let made = (lines.len(), relocs.len());
            keep_lines(lines);
            keep_relocs(relocs);
            made
        };
        let cold = fill();
        let warm = blaze_alloc_count::assert_at_most(0, "a warm unit's relocations and line rows", fill);
        assert_eq!(cold, warm);
        assert_eq!(warm, (100, 100));
    }

    /// A list of copies comes back with its room, and filling it again to what it held takes
    /// nothing of the global allocator.
    #[test]
    fn a_kept_list_of_copies_is_reused() {
        COPIES.with_borrow_mut(Vec::clear);
        let symbol = intern::Symbol::intern("_RNvCs1234_5crate4copy");
        let fill = |copies: &mut Vec<(intern::Symbol, u64)>| {
            copies.extend((0..64).map(|at| (symbol, at)));
            copies.len()
        };
        assert_eq!(with_copies(fill), 64);
        let held = blaze_alloc_count::assert_at_most(0, "a warm list of copies", || {
            with_copies(|copies| {
                assert!(copies.is_empty(), "a lent list is empty");
                fill(copies)
            })
        });
        assert_eq!(held, 64);
    }

    /// A build's layout lists come back empty with their room, holding none of the layouts the
    /// build before kept, and filling them again to what they held takes nothing of the global
    /// allocator.
    #[test]
    fn kept_layout_lists_are_reused_whole() {
        LAYOUT_LISTS.with_borrow_mut(Vec::clear);
        let dl = rustc_abi::TargetDataLayout::default();
        let scalar = crate::Scalar::Union { value: crate::Primitive::Int(rustc_abi::Integer::I32, true) };
        let arcs: Vec<Arc<Layout>> = (0..64).map(|_| Arc::new(Layout::scalar(&dl, scalar))).collect();
        let fill = || {
            let (mut asked, variants) = layout_lists();
            assert!(asked.is_empty() && variants.is_empty());
            asked.extend(arcs.iter().cloned());
            let made = asked.len();
            keep_layout_lists(asked, variants);
            made
        };
        let cold = fill();
        assert!(arcs.iter().all(|it| Arc::strong_count(it) == 1));
        let warm = blaze_alloc_count::assert_at_most(0, "a warm build's layout lists", fill);
        assert_eq!((cold, warm), (64, 64));
        assert!(arcs.iter().all(|it| Arc::strong_count(it) == 1));
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
