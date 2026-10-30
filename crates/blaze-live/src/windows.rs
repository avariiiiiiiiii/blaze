//! Patches the memory of a PE program on Windows x86-64.
//!
//! A patch's code and data are committed in the room behind the image and written before
//! anything reaches them; its jumps and statics are written over the image with every other
//! thread suspended ([`stop`], `crate::apply`).

#![allow(unsafe_code)]

mod stop;

use std::{
    ffi::c_void,
    path::{Path, PathBuf},
    sync::mpsc::{Receiver, TryRecvError},
    time::{Duration, Instant, SystemTime},
};

use crate::{
    Outcome,
    apply::{self, Broke, Failed, Overwrites, Plan, Step},
    format::{self, Body, Code, Patch, Process},
};

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetModuleHandleW(name: *const u16) -> *mut c_void;
    fn GetCurrentProcess() -> *mut c_void;
    fn VirtualAlloc(address: *mut c_void, size: usize, kind: u32, protect: u32) -> *mut c_void;
    fn VirtualProtect(address: *mut c_void, size: usize, protect: u32, old: *mut u32) -> i32;
    fn FlushInstructionCache(process: *mut c_void, address: *const c_void, size: usize) -> i32;
    fn RtlAddFunctionTable(table: *const u8, count: u32, base: u64) -> u8;
    fn VirtualQuery(address: *const c_void, info: *mut MemoryBasicInformation, len: usize)
    -> usize;
    fn GetLastError() -> u32;
    fn OpenProcess(access: u32, inherit: i32, pid: u32) -> *mut c_void;
    fn WaitForSingleObject(handle: *mut c_void, milliseconds: u32) -> u32;
    fn CreateEventW(
        attributes: *const c_void,
        manual: i32,
        set: i32,
        name: *const u16,
    ) -> *mut c_void;
    fn CloseHandle(handle: *mut c_void) -> i32;
}

const MEM_COMMIT: u32 = 0x1000;
const MEM_RESERVE: u32 = 0x2000;
const PAGE_NOACCESS: u32 = 0x01;
const PAGE_READWRITE: u32 = 0x04;
const PAGE_EXECUTE_READ: u32 = 0x20;
const PAGE_EXECUTE_READWRITE: u32 = 0x40;
const SYNCHRONIZE: u32 = 0x0010_0000;
const WAIT_TIMEOUT: u32 = 0x102;
const WAIT_OBJECT_0: u32 = 0;
const IMAGE_DEBUG_TYPE_CODEVIEW: u32 = 2;
const PAGE_GUARD: u32 = 0x100;
const PAGE: u64 = 4096;
/// What `VirtualAlloc` reserves at: an address it is asked for is rounded down to it.
const GRANULE: u64 = 64 << 10;
const ROOM: u64 = 256 << 20;
const REACH: u64 = 2 << 30;
const LOOK_EVERY: Duration = Duration::from_millis(30);

pub(crate) struct Live {
    dir: PathBuf,
    id: [u8; 16],
    slide: u64,
    /// Where the image is loaded, which the tables of functions are relative to.
    loaded: u64,
    process: Process,
    patches: Receiver<(Patch, Instant)>,
    overwrites: Overwrites,
    threads: stop::Threads,
    sequence: u64,
    /// How long the executable's image is from `loaded`: every write over it is checked to be
    /// in it.
    image_len: u64,
}

impl Live {
    pub(crate) fn start() -> Result<Live, String> {
        let executable =
            std::env::current_exe().map_err(|it| format!("finding the executable: {it}"))?;
        let image = Image::read(&executable)?;
        let dir = format::dir(&image.id).ok_or("no home directory to take edits through")?;
        forget_exited();
        std::fs::create_dir_all(&dir).map_err(|it| format!("{}: {it}", dir.display()))?;
        // SAFETY: no name asks for the executable of the process, which is always loaded.
        let loaded = unsafe { GetModuleHandleW(std::ptr::null()) } as u64;
        let slide = loaded.wrapping_sub(image.base);
        let (next, end) = reserve(&image, loaded, slide)?;
        let started = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |it| it.as_millis() as u64);
        let process = Process { pid: std::process::id(), uuid: image.id, started, next, end };
        let live = Live {
            patches: watch(&dir, image.id),
            dir,
            id: image.id,
            slide,
            loaded,
            process,
            overwrites: Overwrites::default(),
            threads: stop::Threads::new(),
            sequence: 0,
            image_len: image.len,
        };
        live.publish()?;
        Ok(live)
    }

    pub(crate) fn poll(&mut self) -> Option<Outcome> {
        let mut newest = None;
        loop {
            match self.patches.try_recv() {
                Ok(patch) => newest = Some(patch),
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
            }
        }
        let (patch, arrived) = newest?;
        self.take(patch, arrived)
    }

    pub(crate) fn apply_as_they_come(mut self, mut told: impl FnMut(Outcome) + Send + 'static) {
        let applying = move || {
            while let Ok(mut newest) = self.patches.recv() {
                while let Ok(next) = self.patches.try_recv() {
                    newest = next;
                }
                if let Some(outcome) = self.take(newest.0, newest.1) {
                    told(outcome);
                }
            }
        };
        std::thread::Builder::new()
            .name("blaze-live-apply".to_owned())
            .spawn(applying)
            .expect("spawning the thread that applies edits");
    }

    fn take(&mut self, patch: Patch, arrived: Instant) -> Option<Outcome> {
        if patch.sequence <= self.sequence {
            return None;
        }
        self.sequence = patch.sequence;
        if patch.uuid != self.id {
            return Some(Outcome::Restart("the program was linked again".to_owned()));
        }
        Some(match patch.body {
            Body::Restart(why) => Outcome::Restart(why),
            Body::Code(code) => {
                let start = Instant::now();
                let waited = start - arrived;
                match self.apply(&code) {
                    Ok(()) => Outcome::Applied {
                        sequence: patch.sequence,
                        functions: code.redirected,
                        statics: code.statics,
                        restored: code.writes.len() as u64 - code.redirected - code.statics,
                        took: start.elapsed(),
                        waited,
                    },
                    Err(Failed::Declined(why)) => Outcome::Declined(why),
                    Err(Failed::Restart(why)) => Outcome::Restart(why),
                }
            }
        })
    }

    fn apply(&mut self, code: &Code) -> Result<(), Failed> {
        // Every write over the executable is checked, and planned, before anything is mapped.
        let image = self.loaded..self.loaded.saturating_add(self.image_len);
        apply::in_image(&code.writes, self.slide, &image)?;
        let plan = self.overwrites.plan(code, self.slide)?;
        readable(&plan)?;
        // From here the room up to `end` is used, whatever becomes of the patch: jumps or the
        // unwinder may reach what is committed there.
        let end = apply::claim(&mut self.process, code, PAGE)?;
        let len = end - code.base;
        // A patch that only puts back what earlier ones wrote has nothing to map.
        let mapped = if len == 0 { Ok(()) } else { self.map(code, len) };
        let applied = mapped.map_err(Failed::from).and_then(|()| self.redirect(&plan));
        let published = self.publish().map_err(Failed::from);
        applied.and(published)
    }

    /// Writes the patch's jumps and statics, and puts back what the last patch wrote and this
    /// one does not, with every other thread suspended and none inside the bytes written.
    fn redirect(&mut self, plan: &Plan) -> Result<(), Failed> {
        let read = |address: u64, into: &mut [u8]| {
            // SAFETY: the bytes are a step's, in one committed, readable region of the
            // executable's image (`apply::in_image`, `readable`), loaded as long as the process
            // runs; every other thread is suspended, so nothing writes them meanwhile, and
            // `into` is the originals' own buffer.
            unsafe {
                std::ptr::copy_nonoverlapping(address as *const u8, into.as_mut_ptr(), into.len())
            }
        };
        let write = |_: usize, step: &Step| put(step);
        apply::write_stopped(&mut self.threads, &mut self.overwrites, plan, false, read, write)
    }

    /// Maps the code and data of the patch, `len` bytes, where it was laid out.
    fn map(&self, code: &Code, len: u64) -> Result<(), String> {
        let text_len = code.text.len() as u64;
        let mapped = code.base.wrapping_add(self.slide);
        // SAFETY: commits `len` bytes at the page-aligned `mapped` (`code.base` whole pages),
        // in the room `reserve` reserved, above every earlier patch (`apply::claim` checked
        // `process.next..process.end` and moved `next` past it), so no earlier patch's code a
        // thread may run is in them; the room is never released, and no later patch is laid
        // out here.
        let result = unsafe {
            VirtualAlloc(mapped as *mut c_void, len as usize, MEM_COMMIT, PAGE_READWRITE)
        };
        if result as u64 != mapped {
            return Err(format!(
                "mapping the patch at {mapped:#x}: {}",
                std::io::Error::last_os_error()
            ));
        }
        // SAFETY: the pages were just committed read-write at `mapped`, page-aligned so any `u8`
        // alignment holds; no thread reaches them before the jumps `apply` writes after `map`
        // returns, and the slice doesn't outlive `map`.
        let memory = unsafe { std::slice::from_raw_parts_mut(mapped as *mut u8, len as usize) };
        memory[..code.text.len()].copy_from_slice(&code.text);
        memory[code.text.len()..code.text.len() + code.data.len()].copy_from_slice(&code.data);
        for &pointer in &code.pointers {
            apply::slide_at(memory, pointer, self.slide)?;
        }
        if text_len != 0 {
            let mut old = 0;
            // SAFETY: `text_len` bytes (whole pages, checked) at the page-aligned start of the
            // pages committed above; `memory` writes nothing there after this, and no thread
            // runs them yet.
            let protected = unsafe {
                VirtualProtect(
                    mapped as *mut c_void,
                    text_len as usize,
                    PAGE_EXECUTE_READ,
                    &mut old,
                )
            };
            if protected == 0 {
                return Err(format!(
                    "making the patch executable: {}",
                    std::io::Error::last_os_error()
                ));
            }
            // SAFETY: it takes nothing and returns this process's pseudo-handle, which needs no
            // closing.
            let process = unsafe { GetCurrentProcess() };
            // SAFETY: flushes the instruction cache over the code just written; it changes no
            // memory.
            unsafe { FlushInstructionCache(process, mapped as *const c_void, text_len as usize) };
        }
        self.register_functions(mapped, memory, code.frames)
    }

    /// Has the unwinder find the functions of the patch, whose table at `code.frames` is
    /// sorted and relative to where the image is loaded: a Rust panic unwinds through them.
    /// `memory` is the patch, committed at `mapped`.
    fn register_functions(
        &self,
        mapped: u64,
        memory: &[u8],
        frames: (u64, u64),
    ) -> Result<(), String> {
        let patch = mapped.wrapping_sub(self.loaded);
        let count = apply::functions(memory, frames, patch)?;
        if count == 0 {
            return Ok(());
        }
        let table = memory[frames.0 as usize..].as_ptr();
        // SAFETY: `count` RUNTIME_FUNCTIONs at the 4-aligned `table`, inside the patch, each
        // naming a function and unwind information inside it relative to `loaded`
        // (`apply::functions`); the patch is never released or rewritten (`apply::claim` moved
        // `next` past it before it was committed).
        if unsafe { RtlAddFunctionTable(table, count, self.loaded) } == 0 {
            return Err("the unwinder refused the patch's table of functions".to_owned());
        }
        Ok(())
    }

    fn publish(&self) -> Result<(), String> {
        let path = self.dir.join("process");
        let partial = self.dir.join(format!("process.{}", self.process.pid));
        std::fs::write(&partial, self.process.encode())
            .and_then(|()| std::fs::rename(&partial, &path))
            .map_err(|it| format!("{}: {it}", path.display()))
    }
}

/// What a program has to know of its executable, every address as the linker has it.
struct Image {
    /// The GUID of the PDB the linker wrote with it.
    id: [u8; 16],
    /// Where the image is loaded when the loader does not move it, and how long it is.
    base: u64,
    len: u64,
}

impl Image {
    fn read(path: &Path) -> Result<Image, String> {
        let bytes =
            std::fs::read(path).map_err(|it| format!("reading {}: {it}", path.display()))?;
        let u16_at = |at: usize| -> Result<u16, String> {
            bytes
                .get(at..at + 2)
                .and_then(|it| it.try_into().ok())
                .map(u16::from_le_bytes)
                .ok_or("PE header ends early".to_owned())
        };
        let u32_at = |at: usize| -> Result<u32, String> {
            bytes
                .get(at..at + 4)
                .and_then(|it| it.try_into().ok())
                .map(u32::from_le_bytes)
                .ok_or("PE header ends early".to_owned())
        };
        let u64_at = |at: usize| -> Result<u64, String> {
            bytes
                .get(at..at + 8)
                .and_then(|it| it.try_into().ok())
                .map(u64::from_le_bytes)
                .ok_or("PE header ends early".to_owned())
        };
        let nt = u32_at(0x3c)? as usize;
        if bytes.get(..2) != Some(b"MZ") || bytes.get(nt..nt + 4) != Some(b"PE\0\0") {
            return Err("the executable is not a PE image".to_owned());
        }
        let header = nt + 4;
        let optional = header + 20;
        if u16_at(header)? != 0x8664 || u16_at(optional)? != 0x20b {
            return Err("the executable is not a PE32+ image for x86-64".to_owned());
        }
        let sections = optional + usize::from(u16_at(header + 16)?);
        let section_count = usize::from(u16_at(header + 2)?);
        let base = u64_at(optional + 24)?;
        let len = u64::from(u32_at(optional + 56)?);
        // Where the file has what is loaded at `address`.
        let offset_of = |address: u32| -> Result<usize, String> {
            for nth in 0..section_count {
                let at = sections + nth * 40;
                let start = u32_at(at + 12)?;
                let raw = u32_at(at + 16)?;
                if (start..start.saturating_add(raw)).contains(&address) {
                    return Ok((u32_at(at + 20)? + (address - start)) as usize);
                }
            }
            Err(format!("nothing in the file is loaded at {address:#x}"))
        };
        if u32_at(optional + 108)? <= 6 {
            return Err("the executable has no debug directory".to_owned());
        }
        let (debug, debug_len) = (u32_at(optional + 112 + 6 * 8)?, u32_at(optional + 116 + 6 * 8)?);
        let debug = if debug_len == 0 { None } else { Some(offset_of(debug)?) };
        let id = (0..debug_len as usize / 28)
            .filter_map(|nth| debug.map(|it| it + nth * 28))
            .filter(|&entry| u32_at(entry + 12) == Ok(IMAGE_DEBUG_TYPE_CODEVIEW))
            .find_map(|entry| {
                let at = u32_at(entry + 24).ok()? as usize;
                let record = bytes.get(at..at + 20)?;
                (record[..4] == *b"RSDS").then(|| record[4..20].try_into().ok()).flatten()
            })
            .ok_or("the executable names no PDB")?;
        Ok(Image { id, base, len })
    }
}

/// Sets aside room for patches behind the image, near enough for a jump of 32 bits from any
/// of its functions to any of the patch's and back. Says where, as the linker has addresses.
fn reserve(image: &Image, loaded: u64, slide: u64) -> Result<(u64, u64), String> {
    let mut at = (loaded + image.len).next_multiple_of(GRANULE);
    for _ in 0..64 {
        if at + ROOM - loaded > REACH {
            break;
        }
        // SAFETY: a reservation takes nothing that is in use, and makes nothing accessible.
        let room =
            unsafe { VirtualAlloc(at as *mut c_void, ROOM as usize, MEM_RESERVE, PAGE_NOACCESS) };
        if room as u64 == at {
            return Ok((at.wrapping_sub(slide), at.wrapping_sub(slide) + ROOM));
        }
        at += GRANULE;
    }
    Err("nothing near the program is free to put edits in".to_owned())
}

fn forget_exited() {
    let Some(root) = format::root() else { return };
    let Ok(entries) = std::fs::read_dir(root) else { return };
    for entry in entries.flatten() {
        let process = std::fs::read_to_string(entry.path().join("process")).ok();
        let pid = process.as_deref().and_then(Process::decode).map(|it| it.pid);
        if !pid.is_some_and(running) {
            _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

fn running(pid: u32) -> bool {
    // SAFETY: it takes no pointer; the handle is closed below.
    let process = unsafe { OpenProcess(SYNCHRONIZE, 0, pid) };
    if process.is_null() {
        return false;
    }
    // SAFETY: the handle is open; a wait of no time only asks whether the process ended.
    let running = unsafe { WaitForSingleObject(process, 0) } == WAIT_TIMEOUT;
    // SAFETY: the handle is open and nothing else has it.
    unsafe { CloseHandle(process) };
    running
}

/// Watches for patches: the daemon sets an event once it wrote one ([`format::wake_event`]),
/// and a daemon that does not is looked for every [`LOOK_EVERY`]. Whether the executable was
/// linked again is looked at on a thread of its own: the first read of an executable just
/// written waits for the scan of it, half a second, which the patch that came with it is not
/// to wait for.
fn watch(dir: &Path, id: [u8; 16]) -> Receiver<(Patch, Instant)> {
    let (send, receive) = std::sync::mpsc::channel();
    let path = dir.join("patch");
    let mut seen = stamp(&path);
    let name: Vec<u16> = format::wake_event(dir).encode_utf16().chain([0]).collect();
    // SAFETY: the name ends with a zero. The event is never closed: the thread below waits on
    // it as long as the program runs. A handle is not `Send`, its value is.
    let wake = unsafe { CreateEventW(std::ptr::null(), 0, 0, name.as_ptr()) } as usize;
    if let Some(executable) = std::env::current_exe().ok() {
        let send = send.clone();
        let mut linked = stamp(&executable);
        std::thread::Builder::new()
            .name("blaze-live-linked".to_owned())
            .spawn(move || {
                loop {
                    std::thread::sleep(LOOK_EVERY);
                    let now = stamp(&executable);
                    if now.is_none() || now == linked {
                        continue;
                    }
                    linked = now;
                    let other =
                        Image::read(&executable).ok().map(|it| it.id).filter(|it| *it != id);
                    if let Some(other) = other {
                        let sequence = SystemTime::now()
                            .duration_since(SystemTime::UNIX_EPOCH)
                            .map_or(0, |it| it.as_millis() as u64);
                        let patch = Patch {
                            uuid: other,
                            sequence,
                            body: Body::Restart("the program was linked again".to_owned()),
                        };
                        if send.send((patch, Instant::now())).is_err() {
                            return;
                        }
                    }
                }
            })
            .expect("spawning the thread that watches the executable");
    }
    std::thread::Builder::new()
        .name("blaze-live".to_owned())
        .spawn(move || {
            loop {
                let woke = if wake == 0 {
                    std::thread::sleep(LOOK_EVERY);
                    false
                } else {
                    let wait = LOOK_EVERY.as_millis() as u32;
                    // SAFETY: the event is open for as long as the program runs.
                    unsafe { WaitForSingleObject(wake as *mut c_void, wait) == WAIT_OBJECT_0 }
                };
                let now = stamp(&path);
                if now.is_none() || (now == seen && !woke) {
                    continue;
                }
                seen = now;
                let Ok(bytes) = std::fs::read(&path) else { continue };
                if let Ok(patch) = Patch::decode(&bytes) {
                    if send.send((patch, Instant::now())).is_err() {
                        return;
                    }
                }
            }
        })
        .expect("spawning the thread that watches for edits");
    receive
}

/// When the file at `path` was last written, and how long it is.
fn stamp(path: &Path) -> Option<(SystemTime, u64)> {
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.modified().ok()?, meta.len()))
}

#[repr(C)]
#[allow(dead_code)]
struct MemoryBasicInformation {
    base: *mut c_void,
    allocation_base: *mut c_void,
    allocation_protect: u32,
    partition: u16,
    size: usize,
    state: u32,
    protect: u32,
    kind: u32,
}

/// Checks, before the threads are suspended, that each step of `plan` is in one committed,
/// readable region: its originals are read with the threads suspended.
fn readable(plan: &Plan) -> Result<(), String> {
    for step in &plan.steps {
        let end = step.address + step.bytes.len() as u64;
        let mut info = MemoryBasicInformation {
            base: std::ptr::null_mut(),
            allocation_base: std::ptr::null_mut(),
            allocation_protect: 0,
            partition: 0,
            size: 0,
            state: 0,
            protect: 0,
            kind: 0,
        };
        let len = size_of::<MemoryBasicInformation>();
        // SAFETY: `info` is a whole MEMORY_BASIC_INFORMATION.
        if unsafe { VirtualQuery(step.address as *const c_void, &mut info, len) } != len {
            return Err(format!(
                "querying {:#x}: {}",
                step.address,
                std::io::Error::last_os_error()
            ));
        }
        let region = info.base as u64..(info.base as u64).saturating_add(info.size as u64);
        let protect = info.protect & 0xff;
        if info.state != MEM_COMMIT
            || end > region.end
            || protect == PAGE_NOACCESS
            || info.protect & PAGE_GUARD != 0
        {
            return Err(format!(
                "a write at {:#x}..{end:#x}, in no one readable region of the executable",
                step.address
            ));
        }
    }
    Ok(())
}

/// Writes `step` over the image: made writable for the write, given its protection back,
/// and the instruction cache flushed over it. Called with every other thread suspended; it
/// allocates nothing.
fn put(step: &Step) -> Result<(), Broke> {
    let (address, len) = (step.address as *mut c_void, step.bytes.len());
    let broke = |what, written| Broke {
        what,
        at: step.address,
        // SAFETY: it takes nothing.
        code: i64::from(unsafe { GetLastError() }),
        written,
    };
    let mut old = 0;
    // SAFETY: the bytes are in one committed region of the executable's image (`readable`);
    // PAGE_EXECUTE_READWRITE keeps read and run; `old` is given back below.
    if unsafe { VirtualProtect(address, len, PAGE_EXECUTE_READWRITE, &mut old) } == 0 {
        return Err(broke("making the executable writable", false));
    }
    // SAFETY: the bytes were made writable above; every other thread is suspended and none
    // inside them (`apply::write_stopped`), so nothing runs, reads or writes them meanwhile.
    // `bytes` is the plan's own buffer.
    unsafe { std::ptr::copy_nonoverlapping(step.bytes.as_ptr(), address as *mut u8, len) };
    let mut was = 0;
    // SAFETY: the same bytes, given back the protection `old` VirtualProtect said they had.
    if unsafe { VirtualProtect(address, len, old, &mut was) } == 0 {
        return Err(broke("protecting the executable again", true));
    }
    // SAFETY: it takes nothing and returns this process's pseudo-handle, which needs no
    // closing.
    let process = unsafe { GetCurrentProcess() };
    // SAFETY: flushes the instruction cache over the bytes just written; it changes no
    // memory.
    unsafe { FlushInstructionCache(process, address, len) };
    Ok(())
}

#[link(name = "ws2_32")]
unsafe extern "system" {
    fn WSAStartup(version: u16, data: *mut u8) -> i32;
    fn socket(family: i32, kind: i32, protocol: i32) -> usize;
    fn connect(socket: usize, address: *const u8, len: i32) -> i32;
    fn send(socket: usize, bytes: *const u8, len: i32, flags: i32) -> i32;
    fn recv(socket: usize, bytes: *mut u8, len: i32, flags: i32) -> i32;
    fn closesocket(socket: usize) -> i32;
    fn WSAGetLastError() -> i32;
}

/// A connection to the daemon's AF_UNIX socket, which the standard library has no type for on
/// Windows.
pub(crate) struct Socket(usize);

impl Socket {
    pub(crate) fn connect(path: &Path) -> std::io::Result<Socket> {
        const AF_UNIX: u16 = 1;
        const SOCK_STREAM: i32 = 1;
        const INVALID_SOCKET: usize = !0;
        let path = path.to_str().ok_or(std::io::ErrorKind::InvalidInput)?.as_bytes();
        // `sockaddr_un`: the family, then the path ending with a zero in 108 bytes.
        let mut address = [0u8; 2 + 108];
        if path.len() >= 108 {
            return Err(std::io::ErrorKind::InvalidInput.into());
        }
        address[..2].copy_from_slice(&AF_UNIX.to_le_bytes());
        address[2..2 + path.len()].copy_from_slice(path);
        // SAFETY: `WSADATA` is under 512 bytes; a second start only counts one more user.
        if unsafe { WSAStartup(0x202, [0u8; 512].as_mut_ptr()) } != 0 {
            return Err(Socket::error());
        }
        // SAFETY: it takes no pointer.
        let socket = unsafe { socket(i32::from(AF_UNIX), SOCK_STREAM, 0) };
        if socket == INVALID_SOCKET {
            return Err(Socket::error());
        }
        let socket = Socket(socket);
        // SAFETY: the address is a whole `sockaddr_un`.
        if unsafe { connect(socket.0, address.as_ptr(), address.len() as i32) } != 0 {
            return Err(Socket::error());
        }
        Ok(socket)
    }

    fn error() -> std::io::Error {
        // SAFETY: it takes nothing.
        std::io::Error::from_raw_os_error(unsafe { WSAGetLastError() })
    }
}

impl std::io::Read for Socket {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        let len = bytes.len().min(i32::MAX as usize) as i32;
        // SAFETY: `bytes` has room for `len`.
        let read = unsafe { recv(self.0, bytes.as_mut_ptr(), len, 0) };
        if read < 0 { Err(Socket::error()) } else { Ok(read as usize) }
    }
}

impl std::io::Write for Socket {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let len = bytes.len().min(i32::MAX as usize) as i32;
        // SAFETY: `bytes` has `len` of them.
        let sent = unsafe { send(self.0, bytes.as_ptr(), len, 0) };
        if sent < 0 { Err(Socket::error()) } else { Ok(sent as usize) }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Drop for Socket {
    fn drop(&mut self) {
        // SAFETY: the socket is this one's alone.
        unsafe { closesocket(self.0) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The test's own executable, which rustc linked with a PDB, is read as the loader has it.
    #[test]
    fn the_executable_names_its_pdb_and_where_it_goes() {
        let image = Image::read(&std::env::current_exe().unwrap()).unwrap();
        assert_ne!(image.id, [0; 16]);
        // SAFETY: as in `Live::start`.
        let loaded = unsafe { GetModuleHandleW(std::ptr::null()) } as u64;
        assert!(image.len != 0 && image.len.is_multiple_of(PAGE), "{:#x}", image.len);
        let (next, end) = reserve(&image, loaded, loaded.wrapping_sub(image.base)).unwrap();
        assert!(next >= image.base + image.len && end - next == ROOM, "{next:#x} {end:#x}");
    }
}
