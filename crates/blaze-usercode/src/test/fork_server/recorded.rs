
// What each test executed, for a binary built with jump-table slots: the harness text of
// `blaze_usercode::test::fork_server(true)`, after `fork_server.rs`. The runtime's two exports
// (blaze-live's, over the jump table) are asked once for the slot count, and per test the
// server maps the region the child's runtime arms and the server reads once it has ended.
#[cfg(unix)]
mod exec {
    use super::{End, HEADER, header, read_region};
    use ::std::borrow::Cow;

    type Int = ::core::ffi::c_int;

    const PROT_READ: Int = 0x1;
    const PROT_WRITE: Int = 0x2;
    const MAP_SHARED: Int = 0x1;
    #[cfg(all(
        any(target_os = "linux", target_os = "android"),
        not(any(target_arch = "mips", target_arch = "mips64", target_arch = "mips32r6", target_arch = "mips64r6"))
    ))]
    const MAP_ANONYMOUS: Int = 0x20;
    #[cfg(all(
        any(target_os = "linux", target_os = "android"),
        any(target_arch = "mips", target_arch = "mips64", target_arch = "mips32r6", target_arch = "mips64r6")
    ))]
    const MAP_ANONYMOUS: Int = 0x800;
    // Apple's systems and the BSDs.
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    const MAP_ANONYMOUS: Int = 0x1000;

    unsafe extern "C" {
        fn __blaze_exec_slots() -> u32;
        fn __blaze_exec_arm(region: *mut u8, len: usize) -> i32;
        fn mmap(
            at: *mut ::core::ffi::c_void,
            len: usize,
            protection: Int,
            flags: Int,
            fd: Int,
            offset: ::core::ffi::c_long,
        ) -> *mut ::core::ffi::c_void;
        fn munmap(at: *mut ::core::ffi::c_void, len: usize) -> Int;
    }

    /// The binary's slot count, asked once before the first test.
    pub struct Exec {
        slots: u32,
    }

    /// A test's region: [`HEADER`] bytes, then a byte per slot rounded up to whole words, mapped
    /// shared and unmapped when dropped.
    pub struct Region {
        at: *mut u8,
        /// What was mapped: a multiple of 8.
        len: usize,
        slots: u32,
    }

    impl Exec {
        pub fn open() -> Exec {
            // SAFETY: the runtime's export takes nothing and returns the jump table's slot
            // count; a binary built with slots has it (codegen picks this text only for one).
            Exec { slots: unsafe { __blaze_exec_slots() } }
        }

        /// What an end of a test that did not start says it executed.
        pub fn not_run<'a>(&self, why: &'a str) -> &'a str {
            why
        }

        /// The region of the next test, made before its fork, its header written.
        pub fn map(&self) -> Result<Region, String> {
            let len = HEADER + (self.slots as usize).div_ceil(8) * 8;
            // SAFETY: a new anonymous mapping (no address asked, no descriptor) touches no
            // memory of ours; the result is checked against `MAP_FAILED` below.
            let at = unsafe {
                mmap(::core::ptr::null_mut(), len, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS, -1, 0)
            };
            if at as usize == usize::MAX {
                return Err(format!("mapping the execution record: {}", ::std::io::Error::last_os_error()));
            }
            let region = Region { at: at as *mut u8, len, slots: self.slots };
            let head = header(self.slots);
            // SAFETY: `at` is a fresh mapping of `len >= HEADER` bytes, readable and writable,
            // page aligned, which nothing else uses yet (the child is not forked).
            unsafe { ::core::ptr::copy_nonoverlapping(head.as_ptr(), region.at, HEADER) };
            Ok(region)
        }
    }

    impl Region {
        /// Arms the record, in the forked child before the test: 0 when the runtime did.
        pub fn arm(&self) -> i32 {
            // SAFETY: the region is mapped for `len >= HEADER + slots` bytes, which is what the
            // runtime writes (it checks `HEADER + slots` itself), and stays mapped in the child
            // until it ends: the child never drops it (it leaves by `_exit`).
            unsafe { __blaze_exec_arm(self.at, HEADER + self.slots as usize) }
        }

        /// What the child executed, once it has ended as `end`, its hits put in `hits`.
        pub fn read(&self, end: &End, hits: &mut Vec<u8>) -> Result<u32, Cow<'static, str>> {
            let ended = match end {
                End::Exited(_) => Ok(()),
                End::Signaled(signal) => Err(format!("it was ended by signal {}", signal)),
                End::NotStarted(why) => Err(why.clone()),
            };
            let ended = match &ended {
                Ok(()) => Ok(()),
                Err(why) => Err(why.as_str()),
            };
            // A process the test forked and left running shares the region and may still
            // store a slot's byte, so it is read with volatile loads, never as a slice.
            // SAFETY: the header is the first `HEADER` bytes of the live mapping.
            let head = unsafe { ::core::ptr::read_volatile(self.at as *const [u8; HEADER]) };
            let words = self.len - HEADER;
            let base = self.at as usize + HEADER;
            read_region(head, self.slots, ended, |n| {
                assert!(8 * n + 8 <= words, "a word past the execution record's region");
                // SAFETY: the word is inside the mapping (checked above), and 8-aligned: the
                // mapping is page aligned and `HEADER` is 16.
                u64::from_le(unsafe { ::core::ptr::read_volatile((base + 8 * n) as *const u64) })
            }, hits)
            .map_err(Cow::Owned)
        }
    }

    impl Drop for Region {
        fn drop(&mut self) {
            // SAFETY: `at` and `len` are the mapping `map` made, unmapped once here; nothing
            // reads it after (`read` borrows the region).
            unsafe { munmap(self.at as *mut ::core::ffi::c_void, self.len) };
        }
    }
}
