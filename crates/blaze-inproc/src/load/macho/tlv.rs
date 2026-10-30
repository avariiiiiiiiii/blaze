//! The program's thread-local variables, done as dyld's `_tlv_get_addr` does them. Each
//! `__thread_vars` descriptor is `{thunk, key, offset}`: the code that reads a variable loads
//! the descriptor's address into `x0` and calls its thunk, which returns the variable's address
//! in `x0`. [`thunk`] reads the calling thread's block for the image's `pthread` key straight
//! from its TSD slots (`TPIDRRO_EL0`, as libpthread's `_pthread_getspecific_direct`), and on a
//! thread's first use makes it from the image's template ([`instantiate`]).
//!
//! The caller saves nothing around the call (LLVM's `CSR_Darwin_AArch64_TLS`): the thunk may
//! change `x16`, `x17`, the flags and nothing else, so its slow path saves `x0`–`x15`, the frame
//! pair and `q0`–`q31` around the call into Rust.
//!
//! A block is freed by the key's destructor in its last round (`PTHREAD_DESTRUCTOR_ITERATIONS`,
//! re-armed until then), so the thread-local destructors dyld runs from a key of its own
//! (`_tlv_atexit`) can still use the variables whichever key's destructor runs first.

use std::alloc::Layout;
use std::ffi::c_void;
use std::sync::atomic::{AtomicPtr, Ordering};

use crate::Error;

/// libpthread's rounds of key destructors at a thread's exit.
const DESTRUCTOR_ROUNDS: u64 = 4;
/// How many images' thread-locals one process sets up (one program, and the tests' images).
const MOST_IMAGES: usize = 8;

/// An image's thread-locals.
struct Template {
    key: libc::pthread_key_t,
    /// The initialized part of the block, copied from the image.
    init: Box<[u8]>,
    /// The whole block, its zero part included.
    size: usize,
    /// The offset of the block in its allocation: room for the key and the destructor's round
    /// count before it, and the block's alignment.
    header: usize,
    layout: Layout,
}

static TEMPLATES: [AtomicPtr<Template>; MOST_IMAGES] = [const { AtomicPtr::new(std::ptr::null_mut()) }; MOST_IMAGES];

fn template(key: u64) -> Option<&'static Template> {
    TEMPLATES.iter().find_map(|slot| {
        let template = slot.load(Ordering::Acquire);
        // SAFETY: a non-null slot is a leaked `Template`, never freed or written after it was
        // published (Release in `install`).
        let template = unsafe { template.as_ref() }?;
        (template.key as u64 == key).then_some(template)
    })
}

fn fail(what: &str) -> ! {
    eprintln!("{}", Error::new(what));
    std::process::abort()
}

core::arch::global_asm!(
    ".p2align 2",
    ".globl _blaze_inproc_tlv_thunk",
    ".private_extern _blaze_inproc_tlv_thunk",
    "_blaze_inproc_tlv_thunk:",
    ".cfi_startproc",
    "ldr x16, [x0, #8]",
    "mrs x17, TPIDRRO_EL0",
    "and x17, x17, #-8",
    "ldr x17, [x17, x16, lsl #3]",
    "cbz x17, 1f",
    "ldr x16, [x0, #16]",
    "add x0, x17, x16",
    "ret",
    "1:",
    "stp x29, x30, [sp, #-16]!",
    ".cfi_def_cfa_offset 16",
    ".cfi_offset w30, -8",
    ".cfi_offset w29, -16",
    "mov x29, sp",
    ".cfi_def_cfa w29, 16",
    "stp x0, x1, [sp, #-16]!",
    "stp x2, x3, [sp, #-16]!",
    "stp x4, x5, [sp, #-16]!",
    "stp x6, x7, [sp, #-16]!",
    "stp x8, x9, [sp, #-16]!",
    "stp x10, x11, [sp, #-16]!",
    "stp x12, x13, [sp, #-16]!",
    "stp x14, x15, [sp, #-16]!",
    "stp q0, q1, [sp, #-32]!",
    "stp q2, q3, [sp, #-32]!",
    "stp q4, q5, [sp, #-32]!",
    "stp q6, q7, [sp, #-32]!",
    "stp q8, q9, [sp, #-32]!",
    "stp q10, q11, [sp, #-32]!",
    "stp q12, q13, [sp, #-32]!",
    "stp q14, q15, [sp, #-32]!",
    "stp q16, q17, [sp, #-32]!",
    "stp q18, q19, [sp, #-32]!",
    "stp q20, q21, [sp, #-32]!",
    "stp q22, q23, [sp, #-32]!",
    "stp q24, q25, [sp, #-32]!",
    "stp q26, q27, [sp, #-32]!",
    "stp q28, q29, [sp, #-32]!",
    "stp q30, q31, [sp, #-32]!",
    "bl {instantiate}",
    "ldp q30, q31, [sp], #32",
    "ldp q28, q29, [sp], #32",
    "ldp q26, q27, [sp], #32",
    "ldp q24, q25, [sp], #32",
    "ldp q22, q23, [sp], #32",
    "ldp q20, q21, [sp], #32",
    "ldp q18, q19, [sp], #32",
    "ldp q16, q17, [sp], #32",
    "ldp q14, q15, [sp], #32",
    "ldp q12, q13, [sp], #32",
    "ldp q10, q11, [sp], #32",
    "ldp q8, q9, [sp], #32",
    "ldp q6, q7, [sp], #32",
    "ldp q4, q5, [sp], #32",
    "ldp q2, q3, [sp], #32",
    "ldp q0, q1, [sp], #32",
    "ldp x14, x15, [sp], #16",
    "ldp x12, x13, [sp], #16",
    "ldp x10, x11, [sp], #16",
    "ldp x8, x9, [sp], #16",
    "ldp x6, x7, [sp], #16",
    "ldp x4, x5, [sp], #16",
    "ldp x2, x3, [sp], #16",
    "ldp x16, x1, [sp], #16",
    "ldr x16, [x16, #16]",
    "add x0, x0, x16",
    "ldp x29, x30, [sp], #16",
    "ret",
    ".cfi_endproc",
    instantiate = sym instantiate,
);

unsafe extern "C" {
    /// The descriptors' thunk above; called only by the program's code, through a descriptor.
    fn blaze_inproc_tlv_thunk();
}

/// The thunk every descriptor of an image of this loader's names.
pub(super) fn thunk() -> u64 {
    blaze_inproc_tlv_thunk as *const () as usize as u64
}

/// The calling thread's block for the descriptor at `descriptor`, made now: the thunk found
/// none. Returns the block's start; the thunk adds the variable's offset.
extern "C" fn instantiate(descriptor: *const u64) -> *mut u8 {
    // SAFETY: the thunk passes the descriptor the program called it with, one of an image's
    // `__thread_vars` the loader filled: three words, mapped for the life of the process.
    let key = unsafe { descriptor.add(1).read() };
    let Some(template) = template(key) else { fail(&format!("a thread-local descriptor of key {key}, which no image has")) };
    // SAFETY: the layout has a nonzero size (`install`).
    let block = unsafe { std::alloc::alloc(template.layout) };
    if block.is_null() {
        std::alloc::handle_alloc_error(template.layout);
    }
    // SAFETY: `block` is a new allocation of `header + size` bytes; the header's last two
    // words (8-aligned: `header` is a multiple of 16) take the key and the destructor's round
    // count, and the block after it the template and zeros.
    let data = unsafe {
        let data = block.add(template.header);
        data.sub(16).cast::<u64>().write(key);
        data.sub(8).cast::<u64>().write(0);
        std::ptr::copy_nonoverlapping(template.init.as_ptr(), data, template.init.len());
        std::ptr::write_bytes(data.add(template.init.len()), 0, template.size - template.init.len());
        data
    };
    // SAFETY: `template.key` is a key `install` made; `data` lives until its destructor frees it.
    if unsafe { libc::pthread_setspecific(template.key, data.cast()) } != 0 {
        fail("pthread_setspecific of a thread-local block");
    }
    data
}

/// A thread's block for one image, at its exit: re-armed until the last round, then freed.
unsafe extern "C" fn destroy(value: *mut c_void) {
    let data = value.cast::<u8>();
    // SAFETY: `value` is a block `instantiate` made (the only values this key is set to), whose
    // header holds its key and round count.
    let (key, rounds) = unsafe { (data.sub(16).cast::<u64>().read(), data.sub(8).cast::<u64>()) };
    let Some(template) = template(key) else { return };
    // SAFETY: as above; only this thread touches its own block's header.
    let round = unsafe {
        let round = rounds.read() + 1;
        rounds.write(round);
        round
    };
    if round < DESTRUCTOR_ROUNDS {
        // SAFETY: the thread's own value for its key, set again for the next round.
        unsafe { libc::pthread_setspecific(template.key, value) };
        return;
    }
    // SAFETY: the allocation `instantiate` made with this layout, `header` bytes before `data`;
    // no round of destructors is left to use it.
    unsafe { std::alloc::dealloc(data.sub(template.header), template.layout) };
}

/// Sets up an image's thread-locals: a block of `size` bytes aligned to `align`, of which the
/// first are `init`. Returns the key its descriptors name.
pub(super) fn install(init: &[u8], size: u64, align: u64) -> Result<u64, Error> {
    let size = usize::try_from(size).map_err(|_| Error::new(format!("thread-locals of {size:#x} bytes")))?;
    if init.len() > size || !align.is_power_of_two() || align > 4096 {
        return Err(Error::new(format!(
            "thread-locals of {size:#x} bytes aligned to {align}, {:#x} of them initialized",
            init.len()
        )));
    }
    let align = (align as usize).max(16);
    let header = align;
    let layout = Layout::from_size_align(header + size.max(1), align)
        .map_err(|_| Error::new(format!("thread-locals of {size:#x} bytes aligned to {align}")))?;
    let mut key: libc::pthread_key_t = 0;
    // SAFETY: `key` is a live out-pointer; `destroy` is the destructor for its values.
    if unsafe { libc::pthread_key_create(&raw mut key, Some(destroy)) } != 0 {
        return Err(Error::new(format!("pthread_key_create: {}", std::io::Error::last_os_error())));
    }
    let template = Box::into_raw(Box::new(Template { key, init: init.into(), size, header, layout }));
    for slot in &TEMPLATES {
        if slot.compare_exchange(std::ptr::null_mut(), template, Ordering::Release, Ordering::Relaxed).is_ok() {
            return Ok(key as u64);
        }
    }
    // SAFETY: the box just leaked above, which no slot took.
    drop(unsafe { Box::from_raw(template) });
    // SAFETY: the key just made, with no values set.
    unsafe { libc::pthread_key_delete(key) };
    Err(Error::new(format!("thread-locals of more than {MOST_IMAGES} images in one process")))
}
