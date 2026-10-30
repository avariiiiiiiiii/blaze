---
status: plan
summary: blaze v2 lane fastcompile, idea 6 (no binary on disk for dev runs): crates/blaze-inproc, code memory mapped writable in the daemon and executable in the run process from one OS memory object, the run-process base chosen up front, blaze's own loader binary and its handshake, fences for running threads (epochs, stack walks, the OS thread list, model-checked), the arena allocator, the channel's frames, the platforms, the hooks requested of link, cargo and usercode, and the state of the art it matches or beats.
date: 2026-10-29
---
# fastcompile: no binary on disk for dev runs (`crates/blaze-inproc`)

Idea 6 of [compile-time.md](../../design/compile-time.md). The daemon places a dev run's or
test's program into a memory object and runs it from there: no executable is written, no link
step turns placed bytes into a file, and a patch is a write into the daemon's view plus one
atomic store. The executable is written only when a build asks for one (`blaze build`,
release). Owned here: `crates/blaze-inproc/` and this file. The boundary siblings program
against is in [notes.md](notes.md) ("The boundary of `blaze-inproc`"); hooks in other lanes
are requests in `v2/requests/fastcompile.md` (`fastcompile-inproc 1` to `4`).

## The state of the art, and where this stands

- **Dual mapping.** .NET 7's W^X mode and Firefox's JIT map one shared-memory object twice,
  writable and executable, and still flip protections or remap per write in places. Here the
  writable view is in another process (the daemon) for good, so there is no protection change
  after setup, ever: a write is a `memcpy`, a patch is one release store. On AArch64 the
  publish is the architecture's minimum (`dc cvau`/`ic ivau` per line only where `CTR_EL0`'s
  `IDC`/`DIC` say they're needed, one `dsb ish`, one `isb`).
- **Hot patching a running program.** Live++ and Unreal's Live Coding load a patch DLL and hook
  functions; Dioxus's `subsecond` loads a patch dylib and swaps a jump table. Each goes through
  a file and the system loader. Here nothing goes through a file: the code lands in the mapped
  object and a stub's target word is stored.
- **Reclaiming replaced code.** Userspace RCU's QSBR flavor frees after every *registered*
  thread passes a quiescent state; the Linux kernel's livepatch consistency model switches a
  task once its stack holds no frame of the old function. The fence here does both: epochs
  reported at quiescent points, refined by a stack walk (a thread that never entered the
  replaced code does not hold it), and threads that never registered made safe by counting
  the OS's list of threads against the slots (`births` guards the count). It is checked
  exhaustively (below), which neither publishes for its implementation.
- **The arena** is TLSF (Masmano et al., 2004): constant-time take and give, neighbours merged
  through start and end maps hashed with one multiply.
- **The channel** decodes frames in place from one reused buffer (no copy, no allocation per
  frame), with fixed little-endian headers and every read bounds-checked.

## The pieces

### Code memory (`lib.rs`, `sys/`)

A `CodeMemory` is one OS memory object of `image + arena` bytes: the first `image` bytes are
the program image as the linker lays out its file (a file offset is an offset in the object),
the arena after them is TLSF-allocated (`alloc.rs`) for code placed while the program runs
(`blaze-lazy`'s functions). The daemon maps it once, shared and writable. Each run process
maps it from blaze's loader: code shared and executable, data as private copies, so a
program's writes to its statics stay its own and the daemon's writes to code show at once.

The run-process base is chosen when the memory is made: at random, 2 MiB-aligned, in
`BASES` (64 GiB to 256 GiB), which is free in a fresh loader process on every target (above a
4 GiB zero page and the dyld shared cache, below the top of a 39-bit AArch64 address space,
below where Linux, macOS and Windows put mmaps, heaps and PIE images). Every process maps it
there (`MAP_FIXED_NOREPLACE`, placeholders on Windows), so a test queue's processes and every
fence entry agree on addresses, and `blaze-lazy` can place code for run-process addresses
before any process exists. A base taken in a process is an error naming it, never another
address.

Changes: `write` (bytes nothing can reach yet), `publish` (the instruction-cache maintenance),
`store_word` (one aligned release store of a slot or a stub's target word; x86-64 and AArch64
stubs load it) and on AArch64 `store_instruction` for the instructions Arm lets one thread
change while another runs them (`B`, `BL`, `NOP`, `BRK`, `ISB`, `SVC`; Arm ARM B2.2.5).

### Fences (`fence.rs`)

A page of words shared by the daemon and every run process (the control object): the epoch
(the daemon's), a table of 256 ranges being retired by run-process address with their grace,
1024 thread slots of 128 bytes each (written by their thread only: process, thread ID, `seen`),
and `births`. `retire` posts a range unlinked by stores already made under grace `g`, then the
epoch becomes `g + 1`. At a quiescent point (`runtime::quiesce`, called where `blaze-live`
already polls, and at a thread's end through a thread-local's destructor) a thread reads the
epoch `e`, and while it has not changed since its last report that is all it does (one
load). Otherwise it serializes (`isb`; `cpuid`), walks its stack through the unwinder, and
reports `e`, or the oldest grace of a posted range one of its return addresses falls in. A
range is freed once every live thread of every process has a slot past its grace: the daemon
reads `births`, lists each process's threads from the OS, reads the slots, reads `births`
again, and trusts the slots only if `births` did not move and each process has exactly as many
online slots as listed threads. Slots of threads no longer listed are reaped; a thread whose
ID the OS reused adopts the slot its dead namesake left, so a long-lived thread never stalls
the count.

What the fence rests on, which `blaze-lazy` keeps: replaced code is reachable only through
slot and stub words and from stacks. A function pointer the program holds is a stub's address,
never a body's, and no quiescent point is between a stub's load of its word and the jump.

A process that forks children running the program (a test binary's fork server) is attached
with `attach_forking`: a child exists before anyone can tell the fence of it, so nothing
retired is freed until the server and its children have exited. The arena then fills with an
error naming the process, never a reuse.

Proof (`fence/model.rs`): an explicit-state model of the daemon and up to three threads at the
grain of single shared accesses, every interleaving explored (sequentially consistent, which
is what the code's `SeqCst` accesses and the fence before the epoch store give), including
threads started while the daemon lists, a thread list that may or may not include threads
born or ending during the listing, threads dying with their slot held, thread IDs reused, and
more threads than slots. Checked: safety (when the range is freed no thread is in it or about
to call into it), liveness (from every reachable state a state with the range freed is
reachable), non-vacuity (it is freed while a thread runs), and teeth: dropping the count, moving
the epoch before the unlink, or ignoring the stack is each caught. `fence/tests.rs` checks the
real code's rules one by one and stress-tests it: threads calling through a slot the daemon
keeps moving while it poisons every range the fence frees, with thread churn, reused IDs and
threads dying with their slot held; a thread reading poison fails the test.

### The loader (`load/`, `bin/blaze-inproc-run.rs`)

A separate small binary, not a mode of the daemon's: the program's thread-locals live in a
64 KiB reserve of the loader's own static TLS block (ELF, PE), which has to be the loader's and
nothing else's, and a run process should not map the daemon's whole image (its size, its
threads' state, its secrets). `blaze-inproc-run --reserve` prints the reserve
(`bias size align`), which the linker places the program's thread-locals at.

Linux can't `execveat` the memfd instead: exec refuses a file with a writable mapping
(`ETXTBSY`), and a loader of our own is needed on macOS and Windows anyway.

The daemon spawns the loader through `blaze-core`'s `Effect::Spawn` with the run's own
sandbox, which gains the loader's path to read and, on Unix, the host's socket (only the
socket, not its directory: the program can't touch other runs' sockets). `BLAZE_INPROC` names
the socket and a 128-bit random token; the loader removes it from its environment, connects,
and says hello (token, its process ID, version, page, reserve). The host checks the token
names a waiting launch and that the process ID matches what the OS says of the peer
(`SO_PEERCRED`, `LOCAL_PEERPID`, `GetNamedPipeClientProcessId`) and that the peer is this user,
then grants the objects: the image as a descriptor that can't be mapped writable shared (on
Linux a read-only reopen through `/proc/self/fd`), the control words, and the objects of fixed
maps (`blaze-premap`'s image). The loader maps them, the format's loader (ELF, Mach-O, PE)
maps segments, links to the system's libraries, sets up thread-locals and unwind info, says
STARTED with its base, and runs the program's initializers and `main` in its own process with
the program's `argv`. A loader's failure is a FAILED frame naming it, and its exit code 127.

### The channel (`frame.rs`, `channel.rs`)

12-byte little-endian headers (kind `u16`, zero `u16`, length `u32`, sequence `u32`), bodies
of at most 64 MiB, kinds partitioned by crate (`0x000` inproc, `0x100` `blaze-lazy`, `0x200`
`blaze-premap`). Every read is bounds-checked; a short buffer, reserved bits, a long body,
trailing bytes, bad UTF-8 or a count larger than the bytes left are each an error, never a
panic or an allocation for bytes that are not there (fuzzed in `tests.rs`).

## Platforms

- **Linux x86-64 and AArch64**: a `memfd` sealed against growing and shrinking, mapped
  `PROT_READ|PROT_WRITE` shared in the daemon; the loader gets a read-only reopen over
  `SCM_RIGHTS` and maps it `PROT_READ|PROT_EXEC`; `vm.memfd_noexec=2` is an error naming the
  sysctl. AArch64 checks `CTR_EL0` at `CodeMemory::new`: an instruction cache that aliases by
  virtual address (VIPT with ways larger than a page, or AIVIVT) is an error, since code is
  written through one mapping and run through another. Threads from `/proc/<pid>/task`.
- **macOS AArch64** (`sys/macos.rs`, `sys/macos/rendezvous.rs`, `load/macho.rs`): a Mach
  memory entry (`mach_make_memory_entry_64`, `MAP_MEM_NAMED_CREATE`, R|W|X), each access a
  sub-entry mapped with `mach_vm_map`. What the hardened runtime allows, from XNU and dyld:
  - a POSIX shared-memory mapping is never executable, so it has to be a memory entry;
  - W^X holds per mapping, not per object: the daemon's R|W and the run's R|X mappings of the
    same pages are allowed;
  - unsigned executable pages kill a process only under `CS_ENFORCEMENT`, which the hardened
    runtime sets unless `allow-unsigned-executable-memory` is granted; `allow-jit` does not
    help, since a shared entry can't be `MAP_JIT`. So `blaze-inproc-run` is signed without the
    hardened runtime (request `fastcompile-inproc-macos 3`), and the loader reads `csops`
    first and stops with an error naming the entitlement instead of being killed;
  - a mapping can't gain rights its entry lacks, even copied, so `Access::Copy` is a snapshot
    in anonymous memory taken when it is mapped;
  - `CTR_EL0` traps at EL0, so the instruction-cache check reads `hw.optional.arm64`, and a
    publish is `sys_icache_invalidate`.

  A Unix socket can't carry Mach ports, so the entries go through a bootstrap service the
  daemon checks in (`dev.blaze.inproc.<daemon pid>`, as Chromium's rendezvous does), each side
  checking the other's audit token against the channel's `LOCAL_PEERTOKEN`. A sandboxed run
  must be allowed to look it up (request `fastcompile-inproc-macos 2`; until it lands such a
  run stops with an error naming the service). The Mach-O loader applies rebase and bind
  opcodes (no chained fixups), fills `__thread_vars` with its own thunk over a pthread key,
  registers unwind sections with libunwind's dynamic finder and runs `LC_MAIN` with dyld's
  `ProgramVars`. Unverified until a Mac run: whether the code-signing monitor on newer Apple
  silicon polices unsigned pages without `CS_ENFORCEMENT`, and `bootstrap_check_in` of an
  undeclared name.
- **Windows x86-64** (`sys/windows.rs`, `sys/windows/pipe.rs`, `load/pe.rs`): a
  pagefile-backed `SEC_COMMIT` section that the daemon maps into the run process with
  `MapViewOfFile3` into placeholders the loader reserved (MAP and MAPPED frames), so the run
  process never holds a handle it could map writable. Views are whole 64 KiB granules, so the
  image maps as three views (code shared executable, data copy-on-write, the arena
  executable), and blaze-link starts the first data section on a 64 KiB RVA (request 1, item
  6). The channel is a named pipe whose DACL admits this user (and AppContainers, to connect
  only), with a low integrity label and the peer's token checked. Threads come from Toolhelp32,
  stack walks from `RtlVirtualUnwind`. The thread-local reserve's alignment is 16, all the NT
  loader's heap-allocated TLS blocks promise. Details in [inproc-windows.md](inproc-windows.md).
- Elsewhere every constructor stops with an error naming the platform (`sys/none.rs`).

## Wiring (requests)

- `fastcompile-inproc 1` (link): `Image::commit_into`, committing a staged image into a
  `CodeMemory` instead of a file, and an in-process image's layout (entry `main`, no C runtime
  start object, thread-locals at the loader's reserve).
- `fastcompile-inproc 2` (cargo): the dev `run` path links into a `CodeMemory` and spawns
  through `host::Program::command`; `blaze build` and release write the executable as today.
  A configured `target.<triple>.runner` takes a file, so that run writes its executable: the
  configuration decides, not a failure.
- `fastcompile-inproc 3` (usercode): the test queue spawns a binary through
  `host::Program::command` when it has one, forking servers attached as forking.
- `fastcompile-inproc 4` (coordinator): the loader binary installed beside `blaze` (on
  Windows with an 8 MiB stack reserve and the CRT's `tlssup`), and `windows-sys` features.
- `fastcompile-inproc-macos 1` to `3` (link, core and cargo, coordinator): the in-process
  Mach-O layout, the sandbox's `mach-lookup` of the rendezvous service, and signing the loader
  without the hardened runtime.

## Open: what a program sees of its own executable

Under the loader, the process's executable is `blaze-inproc-run`: `std::env::current_exe()`,
`/proc/self/exe`, `_NSGetExecutablePath`, `GetModuleFileName(NULL)` and image 0 of dyld name
it, not the program. A program that finds files beside itself through them behaves
differently from its rustc build, which superset-of-rust does not allow. Linux could point
`/proc/self/exe` at the program's path only with `PR_SET_MM_EXE_FILE` (`CAP_SYS_RESOURCE`, and
it needs a file); the other platforms have no call for it. Until the lane settles it (an
interposed `current_exe` in the program's std, or writing the executable for programs whose
std calls it), this is a known difference, stated here and in the report rather than hidden.
