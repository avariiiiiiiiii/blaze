//! What each name outside a macro crate's own code is bound to in the compiled tier
//! (macros-tier.md, "Externals"), decided when the crate is compiled: the tier's own shims
//! (the allocator, memory and string functions, `errno`, the panic runtime), the shims the
//! evaluator's run claims too (`os.rs`'s, the bridge's `Closure::call`), scalar math the
//! host runs on registers alone, or, for any other function, a stub that stops the
//! expansion when it is called, naming the symbol. The address of unknown data is refused
//! when the crate is compiled: no stub can stand in for bytes.

use crate::guest::Stop;

/// How the code uses a name.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Use {
    Call,
    /// Its address, as data.
    Address,
}

/// The shims of the evaluator's run that the compiled tier binds the same names to.
pub(crate) trait Shims {
    /// The id of the shim that runs `symbol` (an external function or datum, as `used`),
    /// when the run claims it: `os.rs`'s file, environment, clock and random shims.
    fn external(&self, symbol: &str, used: Use) -> Option<u16>;
    /// The id of the shim that runs calls of `func` in place of its body: the bridge's
    /// `Closure::call`.
    fn claimed(&self, func: hir_def::FunctionId) -> Option<u16>;
}

/// The shims of the tier itself.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) enum Own {
    /// `__rust_alloc` and the rest of Rust's allocator ([`super::heap::Heap`]).
    Alloc,
    AllocZeroed,
    Dealloc,
    Realloc,
    /// `__rust_alloc_error_handler`: the expansion stops, naming the size.
    AllocError,
    /// C's allocator.
    Malloc,
    Calloc,
    CRealloc,
    Free,
    PosixMemalign,
    AlignedAlloc,
    /// Memory and strings, each range checked against the region.
    Memcpy,
    Memmove,
    Memset,
    Memcmp,
    Memchr,
    Memrchr,
    Strlen,
    /// The header's `errno` cell.
    Errno,
    /// The unwinder std's own `panic_unwind` calls, compiled with the rest of std: a raise
    /// and a resume go to the first landing pad up the frames (or out of the macro, a panic it
    /// didn't catch); a delete is of a foreign exception, which no macro has; a backtrace
    /// walks no frames.
    RaiseException,
    UnwindResume,
    DeleteException,
    Backtrace,
    /// `abort`: the expansion stops.
    Abort,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Binding {
    Own(Own),
    /// A shim of the run's, by its [`Shims`] id.
    Shim(u16),
    /// A function of scalars in registers that touches no memory (libm, compiler-rt):
    /// the host's own, called directly.
    Pure,
    /// A function with no shim: its stub stops the expansion when it is called, naming it
    /// ([`Refusal::stop`] with [`Why::Function`]). std reaches many a function no macro
    /// calls (threads, sockets, signals), so only a call is an error, never the name.
    Unshimmed,
    /// Data that only has to exist and read as zero: the header's zero cell
    /// (`__rust_no_alloc_shim_is_unstable`, `__rust_alloc_error_handler_should_panic`).
    Zero,
}

/// A name or construct the compiled tier refuses, found when the crate is compiled.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct Refusal {
    /// The symbol, or the function holding the construct.
    pub(crate) name: String,
    pub(crate) why: Why,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Why {
    /// A call of an external function with no shim.
    Function,
    /// The address of external data with no shim.
    Data,
    /// Inline assembly (`asm!`, `global_asm!`, `naked_asm!`): it could do anything.
    Assembly,
    /// An operation whose result the sandbox can't confine (its stack or frame pointer, a
    /// stack switch, a dynamic stack allocation, a `byval` argument on the machine's stack).
    Operation(&'static str),
}

impl Refusal {
    /// The expansion's error: the run puts the macro's name before it.
    pub(crate) fn stop(&self) -> Stop {
        const TAIL: &str = ": the compiled tier refuses it when it compiles the macro";
        let name = &self.name;
        Stop(match self.why {
            // alloc: error path
            Why::Function => format!("calls `{name}`, a function outside its own code that blaze has no shim for"),
            // alloc: error path
            Why::Data => format!("uses `{name}`, data outside its own code that blaze has no shim for{TAIL}"),
            // alloc: error path
            Why::Assembly => format!("has inline assembly in `{name}`{TAIL}"),
            // alloc: error path
            Why::Operation(op) => format!("uses `{op}` in `{name}`, which can't be confined{TAIL}"),
        })
    }
}

/// The binding of the external `symbol`, used as `used`.
pub(crate) fn external(symbol: &str, used: Use, shims: &impl Shims) -> Result<Binding, Refusal> {
    let own = match used {
        Use::Call => own_function(symbol).map(Binding::Own),
        Use::Address => zero_data(symbol).then_some(Binding::Zero),
    };
    if let Some(binding) = own {
        return Ok(binding);
    }
    if used == Use::Call && pure(symbol) {
        return Ok(Binding::Pure);
    }
    if let Some(id) = shims.external(symbol, used) {
        return Ok(Binding::Shim(id));
    }
    match used {
        Use::Call => Ok(Binding::Unshimmed),
        // alloc: the refusal, once per crate compiled
        Use::Address => Err(Refusal { name: symbol.to_owned(), why: Why::Data }),
    }
}

/// The tier's own shim for a function of the macro's crate graph, by its link name, run in
/// place of its body: Rust's allocator, which the tier's heap is.
pub(crate) fn instance(link_name: &str) -> Option<Own> {
    own_allocator(link_name)
}

fn own_allocator(symbol: &str) -> Option<Own> {
    Some(match symbol {
        "__rust_alloc" => Own::Alloc,
        "__rust_alloc_zeroed" => Own::AllocZeroed,
        "__rust_dealloc" => Own::Dealloc,
        "__rust_realloc" => Own::Realloc,
        "__rust_alloc_error_handler" => Own::AllocError,
        _ => return None,
    })
}

fn own_function(symbol: &str) -> Option<Own> {
    if let Some(own) = instance(symbol) {
        return Some(own);
    }
    Some(match symbol {
        "malloc" => Own::Malloc,
        "calloc" => Own::Calloc,
        "realloc" => Own::CRealloc,
        "free" => Own::Free,
        "posix_memalign" => Own::PosixMemalign,
        "aligned_alloc" | "memalign" => Own::AlignedAlloc,
        "memcpy" => Own::Memcpy,
        "memmove" => Own::Memmove,
        "memset" => Own::Memset,
        "memcmp" | "bcmp" => Own::Memcmp,
        "memchr" => Own::Memchr,
        "memrchr" => Own::Memrchr,
        "strlen" => Own::Strlen,
        "__errno_location" | "__error" | "_errno" => Own::Errno,
        "_Unwind_RaiseException" => Own::RaiseException,
        "_Unwind_Resume" => Own::UnwindResume,
        "_Unwind_DeleteException" => Own::DeleteException,
        "_Unwind_Backtrace" => Own::Backtrace,
        "abort" => Own::Abort,
        _ => return None,
    })
}

fn zero_data(symbol: &str) -> bool {
    matches!(
        symbol,
        "__rust_no_alloc_shim_is_unstable"
            | "__rust_no_alloc_shim_is_unstable_v2"
            | "__rust_alloc_error_handler_should_panic"
            | "__rust_alloc_error_handler_should_panic_v2"
    )
}

/// libm and compiler-rt functions whose arguments and results are all scalars (integers,
/// floats of every width, `i128`) passed in registers, and which read and write no memory
/// but their own stack. Functions that write through a pointer (`frexp`, `modf`,
/// `lgamma_r`, `sincos`, `remquo`) or a global (`lgamma`'s `signgam`) are not among them.
fn pure(symbol: &str) -> bool {
    matches!(
        symbol,
        // libm, f64 and f32
        "acos" | "acosf" | "acosh" | "acoshf" | "asin" | "asinf" | "asinh" | "asinhf"
            | "atan" | "atanf" | "atan2" | "atan2f" | "atanh" | "atanhf"
            | "cbrt" | "cbrtf" | "ceil" | "ceilf" | "copysign" | "copysignf"
            | "cos" | "cosf" | "cosh" | "coshf" | "erf" | "erff" | "erfc" | "erfcf"
            | "exp" | "expf" | "exp2" | "exp2f" | "exp10" | "exp10f" | "expm1" | "expm1f"
            | "fabs" | "fabsf" | "fdim" | "fdimf" | "floor" | "floorf"
            | "fma" | "fmaf" | "fmax" | "fmaxf" | "fmin" | "fminf" | "fmod" | "fmodf"
            | "hypot" | "hypotf" | "ldexp" | "ldexpf" | "log" | "logf" | "log10" | "log10f"
            | "log1p" | "log1pf" | "log2" | "log2f" | "nearbyint" | "nearbyintf"
            | "pow" | "powf" | "rint" | "rintf" | "round" | "roundf" | "roundeven" | "roundevenf"
            | "scalbn" | "scalbnf" | "sin" | "sinf" | "sinh" | "sinhf" | "sqrt" | "sqrtf"
            | "tan" | "tanf" | "tanh" | "tanhf" | "tgamma" | "tgammaf" | "trunc" | "truncf"
            | "fmaximum" | "fmaximumf" | "fminimum" | "fminimumf"
            // f16 and f128 conversions and arithmetic (compiler-rt)
            | "__extendhfsf2" | "__extendhfdf2" | "__extendhftf2" | "__truncsfhf2" | "__truncdfhf2"
            | "__trunctfhf2" | "__extendsftf2" | "__extenddftf2" | "__trunctfsf2" | "__trunctfdf2"
            | "__addtf3" | "__subtf3" | "__multf3" | "__divtf3" | "__powitf2"
            | "__eqtf2" | "__netf2" | "__lttf2" | "__letf2" | "__gttf2" | "__getf2" | "__unordtf2"
            | "__fixtfsi" | "__fixtfdi" | "__fixtfti" | "__fixunstfsi" | "__fixunstfdi" | "__fixunstfti"
            | "__floatsitf" | "__floatditf" | "__floattitf" | "__floatunsitf" | "__floatunditf"
            | "__floatuntitf"
            | "fmaf128" | "sqrtf128" | "fmodf128" | "ceilf128" | "floorf128" | "truncf128"
            | "roundf128" | "rintf128" | "roundevenf128" | "fminf128" | "fmaxf128" | "fminimumf128"
            | "fmaximumf128" | "copysignf128"
            | "fmaf16" | "sqrtf16" | "fmodf16" | "ceilf16" | "floorf16" | "truncf16" | "roundf16"
            | "rintf16" | "roundevenf16" | "fminf16" | "fmaxf16" | "fminimumf16" | "fmaximumf16"
            | "copysignf16"
            // 128-bit integers and float conversions (compiler-rt)
            | "__multi3" | "__divti3" | "__udivti3" | "__modti3" | "__umodti3"
            | "__ashlti3" | "__ashrti3" | "__lshrti3" | "__muloti4"
            | "__fixdfti" | "__fixsfti" | "__fixunsdfti" | "__fixunssfti"
            | "__floattidf" | "__floattisf" | "__floatuntidf" | "__floatuntisf"
            | "__powidf2" | "__powisf2"
            // the machine stack's probe: it touches the native stack only, which no guest
            // address names
            | "__rust_probestack" | "__chkstk" | "__chkstk_darwin"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Run;

    impl Shims for Run {
        fn external(&self, symbol: &str, used: Use) -> Option<u16> {
            match (symbol, used) {
                ("getenv", Use::Call) => Some(1),
                ("syscall", Use::Call) => Some(2),
                ("environ", Use::Address) => Some(3),
                _ => None,
            }
        }

        fn claimed(&self, _: hir_def::FunctionId) -> Option<u16> {
            None
        }
    }

    #[test]
    fn the_tiers_own_shims_come_first() {
        assert_eq!(external("__rust_alloc", Use::Call, &Run), Ok(Binding::Own(Own::Alloc)));
        assert_eq!(external("memcpy", Use::Call, &Run), Ok(Binding::Own(Own::Memcpy)));
        assert_eq!(external("bcmp", Use::Call, &Run), Ok(Binding::Own(Own::Memcmp)));
        assert_eq!(external("__error", Use::Call, &Run), Ok(Binding::Own(Own::Errno)));
        assert_eq!(external("_Unwind_Resume", Use::Call, &Run), Ok(Binding::Own(Own::UnwindResume)));
        assert_eq!(external("_Unwind_RaiseException", Use::Call, &Run), Ok(Binding::Own(Own::RaiseException)));
        assert_eq!(
            external("__rust_no_alloc_shim_is_unstable_v2", Use::Address, &Run),
            Ok(Binding::Zero)
        );
        assert_eq!(instance("__rust_alloc"), Some(Own::Alloc));
        assert_eq!(instance("__rust_start_panic"), None, "std's panic runtime is compiled, not shimmed");
        assert_eq!(instance("main"), None);
    }

    #[test]
    fn scalar_math_is_the_hosts_and_pointer_math_is_not() {
        for pure in ["sin", "fmodf", "__udivti3", "__extendhfsf2", "sqrtf128"] {
            assert_eq!(external(pure, Use::Call, &Run), Ok(Binding::Pure), "{pure}");
        }
        for writes in ["frexp", "modf", "lgamma_r", "sincos", "lgamma"] {
            assert!(external(writes, Use::Call, &Run).is_err(), "{writes} writes memory");
        }
        assert!(external("sin", Use::Address, &Run).is_err(), "a function's address is a call's");
    }

    #[test]
    fn the_runs_shims_bind_what_they_claim() {
        assert_eq!(external("getenv", Use::Call, &Run), Ok(Binding::Shim(1)));
        assert_eq!(external("syscall", Use::Call, &Run), Ok(Binding::Shim(2)));
        assert_eq!(external("environ", Use::Address, &Run), Ok(Binding::Shim(3)));
    }

    #[test]
    fn any_other_function_stops_when_called_and_other_data_is_refused() {
        assert_eq!(external("pthread_create", Use::Call, &Run), Ok(Binding::Unshimmed));
        let stop = Refusal { name: "pthread_create".to_owned(), why: Why::Function }.stop();
        assert_eq!(stop.0, "calls `pthread_create`, a function outside its own code that blaze has no shim for");
        let data = external("__tls_get_addr", Use::Address, &Run).unwrap_err();
        assert!(data.stop().0.starts_with("uses `__tls_get_addr`, data"), "{}", data.stop());
        let asm = Refusal { name: "core::arch::x86_64::_rdtsc".to_owned(), why: Why::Assembly };
        assert!(asm.stop().0.contains("inline assembly"));
    }
}
