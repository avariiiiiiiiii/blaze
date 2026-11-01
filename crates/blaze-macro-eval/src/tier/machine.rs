//! The compiled tier's one module that touches the machine (macros-tier.md, "Running"):
//! entering the macro's code through an entry thunk, the frame walk every escape takes, the
//! `resume` routine that lands one, and the fault handler.
//!
//! **Escapes.** Every stencil function keeps a frame pointer, and every frame record is
//! `[fp] = the caller's fp, [fp + 8] = the return address`, on x86-64 and AArch64 alike. A walk
//! goes up those records from a starting pair `(ret, fp)` (a return address and the frame
//! pointer of the function it is in), each frame pointer checked to be 8-aligned, inside the
//! machine stack and above the one before, until it finds a call site to land at: the
//! innermost thunk's (a fault, a stop) or the first with a landing pad (a panic). Landing is
//! Cranelift's exception ABI: `sp = fp - frame`, `fp`, the payload in its two registers;
//! every register else is clobbered at a handler, and the landing function's epilogue restores
//! what its prologue saved. So the frames walked over are abandoned: the macro's (stencil's,
//! with no destructors of their own outside landing pads) and the shim dispatcher's, which
//! holds nothing to drop when it escapes.
//!
//! **Faults.** The handler is installed once for SIGSEGV, SIGBUS, SIGILL, SIGTRAP and SIGFPE,
//! on the alternate stack std gives each thread. It acts only for a thread that runs an
//! expansion ([`Running::enter`]) and a PC in that run's image; it lands at the innermost
//! thunk by rewriting the interrupted context, having recorded the fault (no allocation, no
//! lock). Any other fault goes to the handler installed before (std's), or the default.
//!
//! Unix only, on x86-64 and AArch64: the policy runs the compiled tier nowhere else.

use std::{
    cell::{Cell, UnsafeCell},
    ops::Range,
    ptr,
    sync::OnceLock,
};

use cranelift_codegen::ir;

use super::load::Image;
use crate::guest::Stop;

/// What the thunk's handler block returns when an escape lands there.
pub(crate) const ESCAPED: u64 = 1;

/// The bytes of `resume(pc, sp, fp, payload0, payload1) -> !` for the host: the one routine
/// written by hand, placed in the image as a function of its own.
pub(crate) fn resume_code() -> &'static [u8] {
    #[cfg(target_arch = "x86_64")]
    const CODE: &[u8] = &[
        0x48, 0x89, 0xf4, // mov rsp, rsi
        0x48, 0x89, 0xd5, // mov rbp, rdx
        0x48, 0x89, 0xc8, // mov rax, rcx
        0x4c, 0x89, 0xc2, // mov rdx, r8
        0xff, 0xe7, // jmp rdi
    ];
    #[cfg(target_arch = "aarch64")]
    const CODE: &[u8] = &[
        0x3f, 0x00, 0x00, 0x91, // mov sp, x1
        0xfd, 0x03, 0x02, 0xaa, // mov x29, x2
        0xf0, 0x03, 0x00, 0xaa, // mov x16, x0
        0xe0, 0x03, 0x03, 0xaa, // mov x0, x3
        0xe1, 0x03, 0x04, 0xaa, // mov x1, x4
        0x00, 0x02, 0x1f, 0xd6, // br x16
    ];
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    const CODE: &[u8] = &[];
    CODE
}

/// Where an escape lands.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Landing {
    pub(crate) pc: u64,
    pub(crate) sp: u64,
    pub(crate) fp: u64,
}

/// An entry thunk, as the walk finds it: the address its call of the macro returns to, where
/// its handler block is, and its frame.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Thunk {
    /// Its code: `thunk(args, rets, vmctx, code) -> u64`.
    pub(crate) entry: u64,
    pub(crate) ret: u64,
    pub(crate) landing: u64,
    pub(crate) frame: u32,
}

/// A fault the handler caught: what the expansion's error says.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Fault {
    pub(crate) signal: i32,
    pub(crate) pc: u64,
    pub(crate) trap: Option<ir::TrapCode>,
}

/// What escapes look for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Want {
    /// The innermost thunk: a fault, a stop.
    Thunk,
    /// The first landing pad, or a thunk when there is none: a panic.
    Pad,
}

/// One thread's run of a loaded crate: what the walk and the handler read. It is made and
/// read on that thread only; the handler reads it from that thread's signal frame.
pub(crate) struct Running<'i> {
    image: &'i Image,
    /// The entry thunks made so far. Pushed to only by blaze's code between entries (never
    /// while the handler can run for this thread, which is only inside the macro's code), so
    /// the handler's reads never race a push.
    thunks: UnsafeCell<Vec<Thunk>>,
    /// The machine stack: the run thread's own.
    stack: Range<u64>,
    fault: Cell<Option<Fault>>,
    escaped: Cell<Option<Escaped>>,
    /// The shims of the innermost entry, while one runs.
    // dyn: one call per stub call, which leaves the macro's code anyway
    dispatch: Cell<Option<ptr::NonNull<dyn Dispatch + 'static>>>,
    /// The image's `resume` routine.
    resume: u64,
}

/// The stack the dispatcher keeps free for itself and the shims it runs: a call of a stub
/// with less left stops the expansion, so blaze's own code never overflows the run's stack.
pub(crate) const HEADROOM: u64 = 256 << 10;

/// The shims, as the stubs reach them through [`dispatch`]: the safe half of the dispatcher.
pub(crate) trait Dispatch {
    /// How many 16-byte slots the stub of binding `id` has, `None` for no such binding.
    fn slots(&self, id: u32) -> Option<usize>;
    /// Runs binding `id`: its arguments in `slots` (`vmctx` last), its results written to
    /// the first. `running` is the run it is called from, for a nested entry.
    fn call(&mut self, running: &Running<'_>, id: u32, slots: &mut [[u8; 16]]) -> Result<(), Escape>;
}

/// How a shim leaves the macro's code in place of returning to it.
#[derive(Debug)]
pub(crate) enum Escape {
    /// The expansion stops: to the innermost thunk.
    Stop(Stop),
    /// `_Unwind_RaiseException` of the exception at this guest address: to the first landing
    /// pad, or to the innermost thunk when no frame has one (the macro panicked).
    Raise(u64),
}

/// What an entry that returned `false` was left by, besides a fault.
#[derive(Debug)]
pub(crate) enum Escaped {
    Stop(Stop),
    /// A panic no frame of the macro caught: its exception's guest address.
    Uncaught(u64),
}

thread_local! {
    static RUNNING: Cell<*const Running<'static>> = const { Cell::new(ptr::null()) };
}

impl<'i> Running<'i> {
    pub(crate) fn new(image: &'i Image, stack: Range<u64>, resume: u64) -> Result<Running<'i>, Stop> {
        install()?;
        Ok(Running {
            image,
            thunks: UnsafeCell::new(Vec::new()), // alloc: once per run, the thunks' list
            stack,
            fault: Cell::new(None),
            escaped: Cell::new(None),
            dispatch: Cell::new(None),
            resume,
        })
    }

    /// Why the last entry that returned `false` escaped, when it wasn't a fault.
    pub(crate) fn take_escaped(&self) -> Option<Escaped> {
        self.escaped.take()
    }

    pub(crate) fn image(&self) -> &'i Image {
        self.image
    }

    pub(crate) fn add_thunk(&self, thunk: Thunk) {
        // SAFETY: only this thread reaches `self` (it is `!Sync` and the handler acts only on
        // its own thread's run), and no reference into the list outlives a call here: the
        // walk copies what it reads. The handler can't interrupt this push to read the list,
        // since it acts only on a PC in the image's code, and this runs in blaze's.
        unsafe { (*self.thunks.get()).push(thunk) }
    }

    fn thunk_at(&self, ret: u64) -> Option<Thunk> {
        // SAFETY: as in `add_thunk`: read on this thread, never during a push.
        unsafe { (*self.thunks.get()).iter().find(|it| it.ret == ret).copied() }
    }

    /// The fault the handler caught since the last call, if any.
    pub(crate) fn take_fault(&self) -> Option<Fault> {
        self.fault.take()
    }

    /// Calls the macro's `code` through `thunk` with the arguments in `args` (16-byte slots)
    /// and its results put in `rets`, its stubs' calls run by `dispatch`: `true` when it
    /// returned, `false` when an escape landed at the thunk (a fault, a stop, a panic that
    /// left the macro; [`Running::take_fault`], [`Running::take_escaped`]). A shim may enter
    /// again from inside a call, passing itself.
    pub(crate) fn enter(
        &self,
        dispatch: &mut dyn Dispatch, // dyn: as the field it is put in
        thunk: Thunk,
        code: u64,
        vmctx: u64,
        args: &[[u8; 16]],
        rets: &mut [[u8; 16]],
    ) -> bool {
        // SAFETY: only the lifetime is erased; the pointer is used only by `dispatch` (the
        // function) while the entry below runs, inside `dispatch`'s borrow here, and the
        // entry's before it is put back before this returns.
        let shims = unsafe {
            // dyn: as the field
            std::mem::transmute::<ptr::NonNull<dyn Dispatch + '_>, ptr::NonNull<dyn Dispatch + 'static>>(
                ptr::NonNull::from(dispatch),
            )
        };
        let outer = self.dispatch.replace(Some(shims));
        let before = RUNNING.with(|it| it.replace(ptr::from_ref(self).cast()));
        // SAFETY: `thunk.entry` is a thunk this run made for the signature of `code`, which the
        // caller took from the table by that signature (`Table::target`): it reads one slot
        // per parameter of `args` and writes one per result of `rets`, as long as the
        // signature says, which its caller sized them to. Its convention is the host's C one.
        // The macro's code it calls reaches only its region (`vmctx`, owned by the caller for
        // the call), the stubs, and the machine stack below this frame.
        let returned = unsafe {
            let thunk: extern "C" fn(u64, u64, u64, u64) -> u64 = std::mem::transmute(thunk.entry as usize);
            thunk(args.as_ptr() as u64, rets.as_mut_ptr() as u64, vmctx, code)
        };
        RUNNING.with(|it| it.set(before));
        self.dispatch.set(outer);
        returned != ESCAPED
    }

    /// The dispatcher's part, on the stub's frame `fp`: runs binding `id`, giving `None` when
    /// it returned, or where its escape lands and with what payload.
    fn dispatched(&self, id: u32, slots: u64, fp: u64) -> Option<(Landing, [u64; 2])> {
        let (want, payload, escaped) = match self.shims_call(id, slots, fp)? {
            Escape::Stop(stop) => (Want::Thunk, [0; 2], Escaped::Stop(stop)),
            Escape::Raise(exception) => (Want::Pad, [exception, 0], Escaped::Uncaught(exception)),
        };
        // The frame records are on the machine stack, which confined code can't write, and
        // the entry's thunk is above every stub: a walk that misses it is blaze's bug, and
        // returning to the macro would run on past the escape.
        let Some(landing) = self.record(fp).and_then(|(caller_fp, ret)| self.walk(ret, caller_fp, want)) else {
            eprintln!("blaze: a compiled macro's escape from binding {id} found no entry above its stub");
            std::process::abort();
        };
        if self.thunk_at_landing(landing.pc) {
            self.escaped.set(Some(escaped));
        }
        Some((landing, payload))
    }

    fn thunk_at_landing(&self, pc: u64) -> bool {
        // SAFETY: as in `add_thunk`: read on this thread, never during a push.
        unsafe { (*self.thunks.get()).iter().any(|it| it.landing == pc) }
    }

    /// Runs binding `id` through the entry's shims: `None` when it returned.
    fn shims_call(&self, id: u32, slots: u64, fp: u64) -> Option<Escape> {
        let Some(mut shims) = self.dispatch.get() else {
            return Some(Escape::Stop(Stop("called a stub outside an entry".to_owned()))); // alloc: error path
        };
        if fp.saturating_sub(self.stack.start) < HEADROOM {
            // alloc: error path
            return Some(Escape::Stop(Stop("overflowed its stack (the compiled tier's run thread's)".to_owned())));
        }
        // SAFETY: the pointer is the innermost entry's `dispatch`, borrowed by `enter` for as
        // long as that entry runs, which it does: a stub runs only inside one. No other
        // reference to it is live: the entry's caller is inside `enter`, and an outer call of
        // `dispatched` that entered again made this one from its own reference.
        let shims = unsafe { shims.as_mut() };
        let Some(count) = shims.slots(id) else {
            // alloc: error path
            return Some(Escape::Stop(Stop(format!("called the stub of binding {id}, which it has none of"))));
        };
        // SAFETY: the stub of `id` passes its frame's slot area of `count` 16-byte slots,
        // 16-aligned, live for this call, which no one else reaches until the stub returns.
        let slots = unsafe { std::slice::from_raw_parts_mut(slots as *mut [u8; 16], count) };
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| shims.call(self, id, slots))) {
            Ok(Ok(())) => None,
            Ok(Err(escape)) => Some(escape),
            // alloc: error path
            Err(_) => Some(Escape::Stop(Stop(format!("stopped: blaze's shim of binding {id} panicked")))),
        }
    }

    /// The landing of an escape that starts at the return address `ret` in the function
    /// whose frame pointer is `fp`.
    pub(crate) fn walk(&self, mut ret: u64, mut fp: u64, want: Want) -> Option<Landing> {
        loop {
            if let Some(thunk) = self.thunk_at(ret) {
                return Some(Landing { pc: thunk.landing, sp: fp.checked_sub(u64::from(thunk.frame))?, fp });
            }
            if want == Want::Pad
                && let Some((placed, _)) = self.image.function_at(ret)
                && let Some(call) = placed.calls.iter().find(|it| placed.start + u64::from(it.ret) == ret)
                && let Some(landing) = call.landing
            {
                let sp = fp.checked_sub(u64::from(placed.frame))?;
                return Some(Landing { pc: placed.start + u64::from(landing), sp, fp });
            }
            let (next_fp, next_ret) = self.record(fp)?;
            if next_fp <= fp {
                return None;
            }
            (fp, ret) = (next_fp, next_ret);
        }
    }

    /// The frame record at `fp`: the caller's frame pointer and the return address.
    fn record(&self, fp: u64) -> Option<(u64, u64)> {
        if !fp.is_multiple_of(8) || fp < self.stack.start || fp.checked_add(16)? > self.stack.end {
            return None;
        }
        // SAFETY: `fp..fp + 16` lies inside the run thread's stack, mapped for as long as the
        // thread runs, and 8-aligned; the words are plain integers, any bits valid.
        unsafe { Some((ptr::read_volatile(fp as *const u64), ptr::read_volatile((fp + 8) as *const u64))) }
    }

    /// Lands `landing` with the payload, from blaze's own code on the machine stack (the shim
    /// dispatcher): every frame from here to the landing is abandoned.
    ///
    /// # Safety
    ///
    /// Every frame between the caller and the landing must hold nothing to drop, and the
    /// landing must come from [`Running::walk`] on this thread's stack, under the caller.
    pub(crate) unsafe fn land(&self, landing: Landing, payload: [u64; 2]) -> ! {
        // SAFETY: `resume` is the image's routine of that signature, in the host's C
        // convention, and never returns; the caller upholds what abandoning the frames needs.
        unsafe {
            let resume: extern "C" fn(u64, u64, u64, u64, u64) -> ! = std::mem::transmute(self.resume as usize);
            resume(landing.pc, landing.sp, landing.fp, payload[0], payload[1])
        }
    }

    /// The handler's part: whether it caught the fault at `regs`, landing it.
    fn catch(&self, signal: i32, regs: Regs) -> Option<Landing> {
        let (placed, _) = self.image.function_at(regs.pc)?;
        // At a function's first instruction its own frame isn't made yet: the return address
        // is still in the link register or on top of the stack, and `fp` is the caller's.
        let (ret, fp) = if regs.pc == placed.start {
            (regs.entry_ret(self)?, regs.fp)
        } else {
            (regs.pc, regs.fp)
        };
        let landing = self.walk(ret, fp, Want::Thunk)?;
        let offset = (regs.pc - placed.start) as u32;
        let trap = placed.traps.iter().find(|(at, _)| *at == offset).map(|(_, code)| *code);
        self.fault.set(Some(Fault { signal, pc: regs.pc, trap }));
        Some(landing)
    }
}

/// The function every stub calls: `dispatch(id, slots, fp)`, in the host's C convention.
/// What the loader resolves the glue's dispatcher name to.
pub(crate) fn dispatch_address() -> u64 {
    dispatch as extern "C" fn(u64, u64, u64) as usize as u64
}

/// The address of the host's own function `symbol` (libm, compiler-rt), as the dynamic
/// linker finds it among the process's objects: what a call bound to
/// [`super::bind::Binding::Pure`] jumps to. `None` when the process exports no such symbol.
#[cfg(unix)]
pub(crate) fn host_symbol(symbol: &str) -> Option<u64> {
    let name = std::ffi::CString::new(symbol).ok()?; // alloc: once per pure symbol, per crate compiled
    // SAFETY: `name` is NUL-terminated and outlives the call; `RTLD_DEFAULT` searches the
    // objects already loaded, and the result is only read as an address.
    let at = unsafe { libc::dlsym(libc::RTLD_DEFAULT, name.as_ptr()) };
    (!at.is_null()).then_some(at as u64)
}

#[cfg(not(unix))]
pub(crate) fn host_symbol(_symbol: &str) -> Option<u64> {
    None
}

/// The calling thread's stack, where the walk reads frame records ([`Running::new`]).
#[cfg(target_os = "linux")]
pub(crate) fn current_stack() -> Result<Range<u64>, Stop> {
    // SAFETY: `attr` is initialized by `pthread_getattr_np` before it is read and destroyed
    // once read; the out-parameters are a pointer and an integer, written by the call.
    let (got, start, size) = unsafe {
        let mut attr: libc::pthread_attr_t = std::mem::zeroed();
        if libc::pthread_getattr_np(libc::pthread_self(), &mut attr) != 0 {
            return Err(Stop("could not read its thread's stack".to_owned())); // alloc: error path
        }
        let mut start: *mut libc::c_void = ptr::null_mut();
        let mut size: libc::size_t = 0;
        let got = libc::pthread_attr_getstack(&attr, &mut start, &mut size);
        libc::pthread_attr_destroy(&mut attr);
        (got, start as u64, size as u64)
    };
    if got != 0 {
        return Err(Stop("could not read its thread's stack".to_owned())); // alloc: error path
    }
    Ok(start..start + size)
}

#[cfg(target_os = "macos")]
pub(crate) fn current_stack() -> Result<Range<u64>, Stop> {
    // SAFETY: both calls read the calling thread's own attributes, which live as long as it.
    let (top, size) = unsafe {
        let this = libc::pthread_self();
        (libc::pthread_get_stackaddr_np(this) as u64, libc::pthread_get_stacksize_np(this) as u64)
    };
    Ok(top.saturating_sub(size)..top)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub(crate) fn current_stack() -> Result<Range<u64>, Stop> {
    Err(Stop("can't find its thread's stack on this system".to_owned())) // alloc: error path
}

extern "C" fn dispatch(id: u64, slots: u64, fp: u64) {
    let running = RUNNING.with(Cell::get);
    // SAFETY: a stub runs only inside `Running::enter` on this thread, which set `RUNNING`
    // to its run, borrowed for the entry.
    let Some(running) = (unsafe { running.as_ref() }) else {
        eprintln!("blaze: a compiled macro's stub ran outside an entry");
        std::process::abort();
    };
    let Ok(id) = u32::try_from(id) else {
        eprintln!("blaze: a compiled macro's stub passed binding {id}");
        std::process::abort();
    };
    let Some((landing, payload)) = running.dispatched(id, slots, fp) else { return };
    // SAFETY: what this frame and `dispatched`'s held is moved out or dropped by now (the
    // escape is recorded in `running`), so abandoning them drops nothing; the landing comes
    // from the walk up this thread's stack from the stub's frame, above this one.
    unsafe { running.land(landing, payload) }
}

/// The registers of an interrupted context the handler reads.
#[derive(Clone, Copy, Debug)]
struct Regs {
    pc: u64,
    sp: u64,
    fp: u64,
    /// AArch64's link register; 0 on x86-64.
    lr: u64,
}

impl Regs {
    /// The return address at a function's first instruction.
    fn entry_ret(&self, running: &Running<'_>) -> Option<u64> {
        if cfg!(target_arch = "aarch64") {
            return Some(self.lr);
        }
        if self.sp < running.stack.start || self.sp.checked_add(8)? > running.stack.end {
            return None;
        }
        // SAFETY: `sp..sp + 8` lies in the run thread's stack, checked above.
        Some(unsafe { ptr::read_volatile(self.sp as *const u64) })
    }
}

#[cfg(unix)]
const SIGNALS: [libc::c_int; 5] = [libc::SIGSEGV, libc::SIGBUS, libc::SIGILL, libc::SIGTRAP, libc::SIGFPE];

/// The handlers installed before ours, by the index of the signal in [`SIGNALS`].
#[cfg(unix)]
struct Previous([libc::sigaction; SIGNALS.len()]);

// SAFETY: written once inside `install`'s `OnceLock`, before any handler can read it, and
// only read after.
#[cfg(unix)]
unsafe impl Sync for Previous {}
// SAFETY: as above; the actions are plain data.
#[cfg(unix)]
unsafe impl Send for Previous {}

#[cfg(unix)]
static PREVIOUS: OnceLock<Result<Previous, String>> = OnceLock::new();

#[cfg(not(unix))]
fn install() -> Result<(), Stop> {
    Err(Stop("can't run compiled here: blaze has no fault handler on this system".to_owned())) // alloc: error path
}

#[cfg(unix)]
fn install() -> Result<(), Stop> {
    let installed = PREVIOUS.get_or_init(|| {
        // SAFETY: a zeroed `sigaction` is a valid value of it (no handler, empty mask).
        let mut previous: [libc::sigaction; SIGNALS.len()] = unsafe { std::mem::zeroed() };
        for (signal, old) in SIGNALS.iter().zip(&mut previous) {
            // SAFETY: as above.
            let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
            action.sa_sigaction = on_fault as extern "C" fn(libc::c_int, *mut libc::siginfo_t, *mut libc::c_void) as usize;
            action.sa_flags = libc::SA_SIGINFO | libc::SA_ONSTACK;
            // SAFETY: `action` is a valid action whose handler has the `SA_SIGINFO`
            // signature, and `old` is writable.
            let failed = unsafe { libc::sigemptyset(&mut action.sa_mask) != 0 || libc::sigaction(*signal, &action, old) != 0 };
            if failed {
                // alloc: error path
                return Err(format!("could not install its fault handler for signal {signal}: {}", std::io::Error::last_os_error()));
            }
        }
        Ok(Previous(previous))
    });
    // clone: error path
    installed.as_ref().map(|_| ()).map_err(|what| Stop(what.clone()))
}

#[cfg(unix)]
extern "C" fn on_fault(signal: libc::c_int, info: *mut libc::siginfo_t, context: *mut libc::c_void) {
    let running = RUNNING.try_with(Cell::get).unwrap_or(ptr::null());
    if !running.is_null() {
        // SAFETY: the pointer is set by `Running::enter` on this thread to a run that lives
        // until it resets it, and the handler runs on the same thread, inside that call.
        let running = unsafe { &*running };
        // SAFETY: the kernel passes a valid `ucontext_t` to an `SA_SIGINFO` handler.
        let regs = unsafe { read(context) };
        if let Some(landing) = running.catch(signal, regs) {
            // SAFETY: as above; the landing is the innermost thunk's, on this thread's stack.
            unsafe { write(context, landing, [ESCAPED, 0]) };
            return;
        }
    }
    chain(signal, info, context);
}

/// The fault is blaze's: given to the handler before ours, or the default action.
#[cfg(unix)]
fn chain(signal: libc::c_int, info: *mut libc::siginfo_t, context: *mut libc::c_void) {
    let Some(Ok(previous)) = PREVIOUS.get() else { return };
    let Some(at) = SIGNALS.iter().position(|it| *it == signal) else { return };
    let old = &previous.0[at];
    match old.sa_sigaction {
        libc::SIG_IGN => {}
        libc::SIG_DFL => {
            // SAFETY: restoring the default action; returning re-runs the faulting
            // instruction, which then takes it.
            unsafe { libc::signal(signal, libc::SIG_DFL) };
        }
        handler if old.sa_flags & libc::SA_SIGINFO != 0 => {
            // SAFETY: an `SA_SIGINFO` handler, which takes these three arguments.
            let handler: extern "C" fn(libc::c_int, *mut libc::siginfo_t, *mut libc::c_void) =
                unsafe { std::mem::transmute(handler) };
            handler(signal, info, context);
        }
        handler => {
            // SAFETY: a plain handler, which takes the signal alone.
            let handler: extern "C" fn(libc::c_int) = unsafe { std::mem::transmute(handler) };
            handler(signal);
        }
    }
}

/// # Safety
///
/// `context` is the `ucontext_t` the kernel gave an `SA_SIGINFO` handler.
#[cfg(unix)]
unsafe fn read(context: *mut libc::c_void) -> Regs {
    let uc = context.cast::<libc::ucontext_t>();
    // SAFETY: the caller's; each arm reads the general registers of this target's context.
    unsafe {
        #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
        {
            let gregs = &(*uc).uc_mcontext.gregs;
            Regs {
                pc: gregs[libc::REG_RIP as usize] as u64,
                sp: gregs[libc::REG_RSP as usize] as u64,
                fp: gregs[libc::REG_RBP as usize] as u64,
                lr: 0,
            }
        }
        #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
        {
            let mc = &(*uc).uc_mcontext;
            Regs { pc: mc.pc, sp: mc.sp, fp: mc.regs[29], lr: mc.regs[30] }
        }
        #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
        {
            let ss = &(*(*uc).uc_mcontext).__ss;
            Regs { pc: ss.__rip, sp: ss.__rsp, fp: ss.__rbp, lr: 0 }
        }
        #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
        {
            let ss = &(*(*uc).uc_mcontext).__ss;
            Regs { pc: ss.__pc, sp: ss.__sp, fp: ss.__fp, lr: ss.__lr }
        }
        #[cfg(not(any(
            all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")),
            all(target_os = "macos", any(target_arch = "x86_64", target_arch = "aarch64")),
        )))]
        {
            let _ = uc;
            Regs { pc: 0, sp: 0, fp: 0, lr: 0 }
        }
    }
}

/// # Safety
///
/// As [`read`]; on return from the handler the thread resumes at `landing`.
#[cfg(unix)]
unsafe fn write(context: *mut libc::c_void, landing: Landing, payload: [u64; 2]) {
    let uc = context.cast::<libc::ucontext_t>();
    // SAFETY: the caller's; each arm writes this target's pc, sp, fp and payload registers.
    unsafe {
        #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
        {
            let gregs = &mut (*uc).uc_mcontext.gregs;
            gregs[libc::REG_RIP as usize] = landing.pc as i64;
            gregs[libc::REG_RSP as usize] = landing.sp as i64;
            gregs[libc::REG_RBP as usize] = landing.fp as i64;
            gregs[libc::REG_RAX as usize] = payload[0] as i64;
            gregs[libc::REG_RDX as usize] = payload[1] as i64;
        }
        #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
        {
            let mc = &mut (*uc).uc_mcontext;
            (mc.pc, mc.sp, mc.regs[29], mc.regs[0], mc.regs[1]) = (landing.pc, landing.sp, landing.fp, payload[0], payload[1]);
        }
        #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
        {
            let ss = &mut (*(*uc).uc_mcontext).__ss;
            (ss.__rip, ss.__rsp, ss.__rbp, ss.__rax, ss.__rdx) = (landing.pc, landing.sp, landing.fp, payload[0], payload[1]);
        }
        #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
        {
            let ss = &mut (*(*uc).uc_mcontext).__ss;
            (ss.__pc, ss.__sp, ss.__fp, ss.__x[0], ss.__x[1]) = (landing.pc, landing.sp, landing.fp, payload[0], payload[1]);
        }
        #[cfg(not(any(
            all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")),
            all(target_os = "macos", any(target_arch = "x86_64", target_arch = "aarch64")),
        )))]
        {
            let _ = (uc, landing, payload);
        }
    }
}

/// Whether this target runs the compiled tier: the targets [`read`] and [`write`] know.
pub(crate) const SUPPORTED: bool = cfg!(any(
    all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")),
    all(target_os = "macos", any(target_arch = "x86_64", target_arch = "aarch64")),
));

#[cfg(all(test, unix))]
mod tests;
