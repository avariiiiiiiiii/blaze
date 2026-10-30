---
status: plan
summary: blaze v2 fastcompile, the Windows x86-64 half of in-process runs (crates/blaze-inproc sys/windows.rs and load/pe.rs) - dual-mapped code from a pagefile-backed section mapped into the run process by the daemon through placeholders, a named pipe for this user only, and blaze's own loader for PE images (three 64 KiB views, never writable and executable, imports, TLS in the loader's reserve, unwind tables, the program's command line), with what blaze-link must lay out.
date: 2026-10-29
---
# fastcompile-inproc-windows: dual-mapped code and a PE loader

The Windows x86-64 platform layer of [inproc.md](inproc.md): `crates/blaze-inproc/src/sys/windows.rs`
(+ `sys/windows/pipe.rs`) and the loader `crates/blaze-inproc/src/load/pe.rs` (+ `load/pe/parse.rs`).

## State of the art matched

- **Dual mapping:** .NET 7's W^X (`ExecutableAllocator`) maps one pagefile-backed section twice: a
  writable view for the JIT and an executable view for running. blaze does the same across processes.
  The daemon holds the writable view; the run process gets executable views of the same section.
  The daemon maps those views into the run process itself with `MapViewOfFile3` on a process handle.
  The run process never receives a handle to the section.
- **No race between reserve and map:** placeholders (`VirtualAlloc2` with `MEM_RESERVE_PLACEHOLDER`,
  split with `VirtualFree(MEM_PRESERVE_PLACEHOLDER)`, replaced with `MEM_REPLACE_PLACEHOLDER`) are
  Microsoft's documented way to do this. Chromium and Wine's `ntdll` use them to place views
  exactly. Nothing else can take the range between reserving it and mapping it.
- **Loading from memory:** MemoryModule and reflective loaders map a PE image from memory, relocate
  it, resolve its imports and call TLS callbacks. blaze's loader does the same but goes further:
  it registers `.pdata` with `RtlAddFunctionTable` so stack walks and SEH work, gives the program
  real static thread-locals, and makes Rust panics (C++ exceptions) work through the image (see
  "Panics").

## Memory

- **Memory object:** `CreateFileMappingW(INVALID_HANDLE_VALUE, PAGE_EXECUTE_READWRITE | SEC_COMMIT)`.
  The whole object is charged against the commit limit when created, so no write can fault for
  lack of commit later. The daemon maps it read-write here.
- **Granularity:** every view starts and ends on the 64 KiB allocation granularity, which is what
  `sys::page()` returns. All offsets, lengths and addresses of the image, the arena, the control
  object and fixed maps are multiples of it.
- **MAP frames:** the loader's `Granted::map`, `Granted::view` and `reserve` work as follows.
  - The loader keeps its placeholders in a list.
  - To map into one, it carves out exactly `[at, at + len)` and asks the daemon with MAP (object
    u32, offset u64, len u64, at u64, access u8).
  - The daemon checks the object's rights (code may not be mapped shared-writable; control words
    are only shared or read), the granularity and the bounds. It then reads the target's state
    with `VirtualQueryEx`: free means a plain map, a placeholder means `MEM_REPLACE_PLACEHOLDER`,
    anything else is refused as taken. It answers MAPPED with the address, or REFUSED with why.
  - On a refusal the loader puts the placeholder back.
- **Object lengths:** these reach the loader in a second GRANT frame, at sequence number 1, after
  the grant. Its body is a count, then for each object its length (u64) and rights (u8). The
  daemon keeps the handles.
- **Channel:** a named pipe `\\.\pipe\blaze-inproc-<pid>-<random>`, with remote clients rejected.
  - The DACL gives the user everything and gives ALL APPLICATION PACKAGES connect rights only, so
    a program in the `blaze.sandbox` AppContainer can connect.
  - A low mandatory label with NO_WRITE_UP lets the low-integrity sandbox in.
  - On every accept, the peer's process is looked up with `GetNamedPipeClientProcessId`, and its
    token user must be ours.
  - The client opens the pipe at SecurityIdentification, so the server can't impersonate it.
  - A read that times out cancels the I/O but still returns a transfer that completed meanwhile.
- **Other functions:**
  - `threads`: a Toolhelp32 walk. A walk that fails partway is an error, never a shorter list,
    because the fence reaps the slots of threads that aren't listed.
  - `backtrace`: a walk with `RtlLookupFunctionEntry` and `RtlVirtualUnwind`. It covers every
    frame with unwind information, including the program's, once `.pdata` is registered. It
    returns `None` at a frame with no function entry or when the buffer is full.
  - `publish`: `FlushInstructionCache`.
  - `random`: `ProcessPrng`.

## The PE loader

**One object, three views.** The file must be laid out as it is in memory: each section's
`PointerToRawData == VirtualAddress`. Otherwise the loader stops with an error naming the section.
The object is mapped as three views:

| Range | Access | Why |
| --- | --- | --- |
| `[0, code_end)` | execute-read, shared | headers and code: the daemon's patches show at once |
| `[code_end, image_len)` | write-copy | data: private on first write |
| `[image_len, len)` | execute-read, shared | the lazy arena |

`code_end` is the end of the last code section, rounded up to 64 KiB. Every code section must come
before every data section, and the first data section must start at or after `code_end`. Otherwise
the error names the section and says what blaze-link must do.

**Why not 4 KiB sections?** Views can't split a 64 KiB granule, so a granule holding both code and
data would need one view that is both writable and executable. The other options are worse:

- Copying the data out to private memory loses sharing and doubles the bytes.
- Remapping each section's pages separately is impossible, because offsets must be 64 KiB-aligned
  too.

Padding the start of data to 64 KiB costs at most 60 KiB of address space, and no memory: the
padding is never touched.

After relocation and imports, read-only data sections are protected read-only. Writable data stays
`PAGE_WRITECOPY`, which Windows turns into private read-write pages on first write. A code section
whose memory size is larger than its raw size must have zeros past its raw bytes in the object,
since that view can't be written. Data sections past their raw bytes are zeroed only where the
object holds anything else there.

**Base:** exactly the address the grant says, which the daemon's code memory chose when it was
created. If that address is taken, or the grant gives no base, the load stops with an error; it
never maps anywhere else. An image without relocations must have that address as its `ImageBase`.

**Relocations:** DIR64 only, and into data only. A relocation into code is an error even when the
image doesn't move, so a layout bug shows whatever the address. The code is shared with the daemon
and is never written.

**Imports:** each library is loaded with `LoadLibraryExW` (standard search order), and each symbol
is resolved with `GetProcAddress`, by name or by ordinal. Delay-load imports and .NET headers are
errors. Bound imports are ignored, because blaze resolves every import itself.

**Thread-locals:**
- The loader keeps 64 KiB of its own static TLS block: a `thread_local!` with a nonzero `const`
  initializer, so the bytes are raw data of `.tls` and every new thread is started from them.
- `reserve()` reports the reserve's offset from the start of the loader's block, 64 KiB, and an
  alignment of 16. The NT loader allocates blocks from the heap, so no larger alignment holds.
- A program whose template is larger, or more aligned, is refused.
- The program's `AddressOfIndex` gets the loader's `_tls_index`. blaze-link biases the program's
  TLS offsets by the reserve's offset, so `gs:[0x58][_tls_index] + offset` lands in the reserve.
- The relocated template is copied into the calling thread's reserve, and into the loader's raw
  template, which is made writable for the copy. Each later thread therefore starts with it.
- A loader TLS callback (`.CRT$XLM`) forwards THREAD_ATTACH, THREAD_DETACH and PROCESS_DETACH to
  the program's callbacks once the program is entered. The program's PROCESS_ATTACH callbacks are
  called once, before the entry.

**Unwinding:** `.pdata` is checked (sorted, inside the code) and registered with
`RtlAddFunctionTable`.

**Panics:** `_CxxThrowException`, which Rust's MSVC panics go through, asks `RtlPcToFileHeader`
which image holds the throw's type information. The system only knows modules it loaded itself.
The loader therefore interposes a wrapper that answers the program's base for addresses in the
program's object and calls the real function for everything else:
- in the program's own imports of `RtlPcToFileHeader`;
- in the import address tables of each library the program imports (in practice
  `vcruntime140.dll`), for slots holding ntdll's or kernel32's `RtlPcToFileHeader`.

**Command line:** the daemon spawns `loader argv0 args...`. The loader drops the first argument
(its own path, read by the C runtime's argv[0] rule) in place:
- in the PEB's `ProcessParameters->CommandLine`, updating its length;
- in `GetCommandLineW`'s buffer, if that's a separate buffer;
- in `GetCommandLineA`'s buffer.

The C runtime's `_wcmdln` and `_acmdln` point at these same buffers. The program's
`mainCRTStartup` parses `argv` from them, and Rust's std reads `GetCommandLineW`. So every reader
sees `argv0 args...`.

**Entry:** `AddressOfEntryPoint`, which is the image's C runtime start (`mainCRTStartup`), called
with the PEB as the system's thread start passes it, then `ExitProcess` with what it returns. The
program runs on the loader's main thread, so a program asking for a larger stack reserve than that
thread has is refused.

## What blaze-link must do

- Put every code section before every data section, and start the first data section on a 64 KiB
  RVA.
- Keep `PointerToRawData == VirtualAddress`.
- Write no base relocation into code: from code, address RIP-relative only.
- Bias thread-local offsets (SECREL to `.tls`) by `TlsReserve.bias`, keep the template within
  `TlsReserve.size`, and keep its alignment at 16 or less.
- Write no delay-load imports.
- The entry is the CRT start (`mainCRTStartup`).
- Link the loader with a stack reserve at least as large as any program's.

## Known differences from running the executable

- `GetModuleHandle(NULL)`, `GetModuleFileName`, `current_exe` and `_pgmptr` name the loader.
- Resources aren't found through the module handle.
- Threads created with stack size 0 get the loader's default stack size.
- ntdll worker threads that existed before the load keep the loader's pattern in the reserve.
- The arena's functions have no function tables until blaze-lazy registers unwind information for
  them.
