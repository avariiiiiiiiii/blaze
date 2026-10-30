//! Starting the process of an [`crate::Effect::Spawn`] on Windows and making its [`Exit`].
//!
//! Each process is made suspended in a job object of its own, which kills whatever is left of
//! it when the job closes and, where a limit is asked for, holds its memory (the commit of the
//! whole job, `JOB_OBJECT_LIMIT_JOB_MEMORY`: Windows' hard backstop). Then it runs. Its pipes
//! are named pipes whose ends of the driver's are overlapped, read and written through the
//! port; the child inherits its three ends and nothing else (`PROC_THREAD_ATTRIBUTE_HANDLE_LIST`).
//!
//! Its end is heard from a wait of the thread pool's on the process, which posts to the port:
//! the job's messages are, as Windows documents them, not sure to arrive, so they say only
//! that the job went past its memory, which kills it.
//!
//! The process is made suspended and runs only once the driver follows it ([`follow`], then
//! [`resume`]): its job's messages and its end come to the port with its spawn's handle, which
//! names it only once it is in the driver's slab.

use std::{
    cell::UnsafeCell,
    ffi::{OsStr, OsString, c_void},
    io,
    marker::PhantomPinned,
    os::windows::{
        ffi::OsStrExt as _,
        io::{AsRawHandle as _, OwnedHandle},
    },
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use windows_sys::Win32::{
    Foundation::{GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE, STATUS_COMMITMENT_LIMIT},
    Security::SECURITY_ATTRIBUTES,
    Storage::FileSystem::{
        CreateFileW, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED, FILE_SHARE_READ,
        FILE_SHARE_WRITE, FILE_WRITE_ATTRIBUTES, OPEN_EXISTING, PIPE_ACCESS_INBOUND,
        PIPE_ACCESS_OUTBOUND,
    },
    System::{
        JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION,
            JOB_OBJECT_LIMIT_JOB_MEMORY, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            JOBOBJECT_ASSOCIATE_COMPLETION_PORT, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
            JobObjectAssociateCompletionPortInformation, JobObjectExtendedLimitInformation,
            QueryInformationJobObject, SetInformationJobObject, TerminateJobObject,
        },
        Pipes::{CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_WAIT},
        Threading::{
            CREATE_NO_WINDOW, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, CreateProcessW,
            DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT, GetExitCodeProcess, INFINITE,
            InitializeProcThreadAttributeList, LPPROC_THREAD_ATTRIBUTE_LIST,
            PROC_THREAD_ATTRIBUTE_HANDLE_LIST, PROCESS_INFORMATION, RegisterWaitForSingleObject,
            ResumeThread, STARTF_USESTDHANDLES, STARTUPINFOEXW, STARTUPINFOW, TerminateProcess,
            UnregisterWaitEx, UpdateProcThreadAttribute, WT_EXECUTEONLYONCE,
        },
    },
};

use super::sys::{self, KEY_EXIT, KEY_JOB, Port};
use crate::{Command, Confine, Exit, Sandbox};

/// What a pipe holds before its writer waits.
const PIPE: u32 = 64 << 10;

/// The exit code of a process [`crate::Effect::Kill`] ended (`TerminateJobObject`): Windows
/// has no signals, so a kill is told by its code, the one `taskkill /f` gives.
pub(super) const KILLED: u32 = 1;
/// The exit code of a process killed for going past its memory: the status Windows refuses
/// the commit with.
pub(super) const OUT_OF_MEMORY: u32 = STATUS_COMMITMENT_LIMIT as u32;

static RUNS: AtomicU64 = AtomicU64::new(0);

/// A process made, suspended until [`resume`], its pipes the driver's.
pub(super) struct Started {
    pub(super) process: OwnedHandle,
    /// Its first thread, suspended.
    pub(super) thread: OwnedHandle,
    pub(super) job: OwnedHandle,
    /// Where its input goes, of a command with input: closing it is the end of the input.
    pub(super) stdin: Option<OwnedHandle>,
    pub(super) stdout: OwnedHandle,
    pub(super) stderr: OwnedHandle,
    pub(super) finish: Finish,
}

/// What is needed to say how the process ended, once it has.
pub(super) struct Finish {
    pub(super) command: Box<Command>,
    // dyn: the caller's sandbox maker, asked once per sandboxed process.
    confine: Option<Arc<dyn Confine>>,
    scratch: Option<PathBuf>,
}

/// `command` made in its sandbox, made by `confine`, unless the sandbox is off; in a job of its
/// own, suspended until the driver follows it.
// dyn: as above.
pub(super) fn start(root: &Path, command: Box<Command>, confine: Option<&Arc<dyn Confine>>) -> io::Result<Started> {
    let n = RUNS.fetch_add(1, Ordering::Relaxed);
    let (confined, scratch, confine) = match &command.sandbox {
        Sandbox::Off => (None, None, None),
        Sandbox::On { .. } => {
            let Some(confine) = confine else {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    // alloc: an error path.
                    format!(
                        "{}: a sandbox is asked for and this driver was given nothing to make \
                         one (no Confine); nothing runs outside one",
                        command.program.display()
                    ),
                ));
            };
            // alloc: a sandboxed process's scratch directory, made once as it starts.
            let scratch = root.join(".blaze-spawn").join(format!("{}-{n}", std::process::id()));
            _ = std::fs::remove_dir_all(&scratch);
            std::fs::create_dir_all(&scratch)?;
            match confine.confine(&command, &scratch) {
                // clone: one reference count, the sandbox's to ask what the process read.
                Ok(confined) => (Some(confined), Some(scratch), Some(confine.clone())),
                Err(why) => {
                    _ = std::fs::remove_dir_all(&scratch);
                    // alloc: an error path.
                    return Err(io::Error::other(format!(
                        "{}: no sandbox can be made here: {why}",
                        command.program.display()
                    )));
                }
            }
        }
    };
    let launch = match &confined {
        Some(confined) => Launch {
            program: &confined.program,
            args: &confined.args,
            env: &confined.env,
            dir: &confined.dir,
        },
        None => Launch { program: &command.program, args: &command.args, env: &command.env, dir: &command.dir },
    };
    let made = run(&launch, &command, n);
    let started = made.map_err(|error| {
        if let Some(scratch) = &scratch {
            _ = std::fs::remove_dir_all(scratch);
        }
        // alloc: an error path.
        io::Error::new(error.kind(), format!("starting {}: {error}", launch.program.display()))
    })?;
    let (process, thread, job, stdin, stdout, stderr) = started;
    Ok(Started { process, thread, job, stdin, stdout, stderr, finish: Finish { command, confine, scratch } })
}

/// What a process is started as: its command's own, or what its sandbox made of it.
struct Launch<'a> {
    program: &'a Path,
    args: &'a [OsString],
    env: &'a [(OsString, OsString)],
    dir: &'a Path,
}

type Handles = (OwnedHandle, OwnedHandle, OwnedHandle, Option<OwnedHandle>, OwnedHandle, OwnedHandle);

fn run(launch: &Launch<'_>, command: &Command, n: u64) -> io::Result<Handles> {
    let job = job(command.memory)?;
    let (stdout, child_stdout) = pipe(true, n, "out")?;
    let (stderr, child_stderr) = pipe(true, n, "err")?;
    let (stdin, child_stdin) = match command.stdin.is_empty() {
        true => (None, nothing()?),
        false => {
            let (ours, theirs) = pipe(false, n, "in")?;
            (Some(ours), theirs)
        }
    };
    let inherited = [child_stdin.as_raw_handle(), child_stdout.as_raw_handle(), child_stderr.as_raw_handle()];
    // alloc: what the process is started with, made once as it starts.
    let program = sys::wide(launch.program.as_os_str())?;
    let mut line = command_line(launch.program, launch.args)?;
    let env = environment(launch.env)?;
    let dir = sys::wide(launch.dir.as_os_str())?;
    let attributes = Attributes::new(&inherited)?;
    // SAFETY: all zeros is a valid `STARTUPINFOEXW` before its fields are set.
    let mut startup: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
    startup.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
    startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    startup.StartupInfo.hStdInput = inherited[0];
    startup.StartupInfo.hStdOutput = inherited[1];
    startup.StartupInfo.hStdError = inherited[2];
    startup.lpAttributeList = attributes.list();
    // SAFETY: all zeros is a valid `PROCESS_INFORMATION` for the call to fill.
    let mut info: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: NUL-terminated strings, a writable command line, a Unicode environment block
    // ending in two NULs, and the attribute list and handles it names, all of which outlive
    // the call.
    sys::check(unsafe {
        CreateProcessW(
            program.as_ptr(),
            line.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            1,
            CREATE_UNICODE_ENVIRONMENT | EXTENDED_STARTUPINFO_PRESENT | CREATE_SUSPENDED | CREATE_NO_WINDOW,
            env.as_ptr().cast(),
            dir.as_ptr(),
            (&raw const startup).cast::<STARTUPINFOW>(),
            &mut info,
        )
    })?;
    drop(attributes);
    let process = sys::owned(info.hProcess)?;
    let thread = sys::owned(info.hThread)?;
    // SAFETY: the job and the process just made, which has not run.
    if let Err(error) = sys::check(unsafe { AssignProcessToJobObject(job.as_raw_handle(), process.as_raw_handle()) }) {
        // SAFETY: the process just made.
        unsafe { TerminateProcess(process.as_raw_handle(), KILLED) };
        // alloc: an error path.
        return Err(io::Error::new(error.kind(), format!("into its job: {error}")));
    }
    // Its ends are its own now: the pipes end when it and what it starts close them.
    drop((child_stdin, child_stdout, child_stderr));
    Ok((process, thread, job, stdin, stdout, stderr))
}

/// The process of `thread`, made suspended in `job`, run; or, refused, its job ended.
pub(super) fn resume(thread: OwnedHandle, job: &OwnedHandle) -> io::Result<()> {
    // SAFETY: the process's thread, suspended once at its making.
    if unsafe { ResumeThread(thread.as_raw_handle()) } == u32::MAX {
        let error = io::Error::last_os_error();
        terminate(job, KILLED);
        return Err(error);
    }
    Ok(())
}

/// The job's messages posted to `port` with `key` (at least [`KEY_JOB`]).
pub(super) fn follow(job: &OwnedHandle, port: &Port, key: usize) -> io::Result<()> {
    debug_assert!(key >= KEY_JOB);
    let associate = JOBOBJECT_ASSOCIATE_COMPLETION_PORT {
        CompletionKey: key as *mut c_void,
        CompletionPort: port.raw(),
    };
    // SAFETY: an open job, and information of the class named, of its size; the key is a
    // number, handed back with each message.
    sys::check(unsafe {
        SetInformationJobObject(
            job.as_raw_handle(),
            JobObjectAssociateCompletionPortInformation,
            (&raw const associate).cast(),
            size_of::<JOBOBJECT_ASSOCIATE_COMPLETION_PORT>() as u32,
        )
    })
}

/// A job that kills what is left in it when it closes and holds the memory of everything in
/// it to `memory`.
fn job(memory: Option<u64>) -> io::Result<OwnedHandle> {
    // SAFETY: no security attributes or name.
    let job = sys::owned(unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) })?;
    // SAFETY: all zeros is a valid limit of nothing.
    let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
    limits.BasicLimitInformation.LimitFlags =
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION;
    if let Some(bytes) = memory {
        limits.BasicLimitInformation.LimitFlags |= JOB_OBJECT_LIMIT_JOB_MEMORY;
        limits.JobMemoryLimit = usize::try_from(bytes).unwrap_or(usize::MAX);
    }
    // SAFETY: the job, and information of the class named, of its size.
    sys::check(unsafe {
        SetInformationJobObject(
            job.as_raw_handle(),
            JobObjectExtendedLimitInformation,
            (&raw const limits).cast(),
            size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    })?;
    Ok(job)
}

/// The job, and with it everything in it, ended with `code`.
pub(super) fn terminate(job: &OwnedHandle, code: u32) {
    // SAFETY: an open job.
    unsafe { TerminateJobObject(job.as_raw_handle(), code) };
}

static PIPES: AtomicU64 = AtomicU64::new(0);

/// A pipe: the driver's end (overlapped, kept from children) and the child's (inheritable).
/// `inbound` is a pipe the child writes.
fn pipe(inbound: bool, n: u64, which: &str) -> io::Result<(OwnedHandle, OwnedHandle)> {
    // alloc: per process, made once as it starts (CreateProcessW costs a thousand times more).
    let name = format!(
        r"\\.\pipe\blaze-{}-{n}-{which}-{}",
        std::process::id(),
        PIPES.fetch_add(1, Ordering::Relaxed)
    );
    let wide = sys::wide(OsStr::new(&name))?;
    let access = match inbound {
        true => PIPE_ACCESS_INBOUND,
        false => PIPE_ACCESS_OUTBOUND,
    };
    // SAFETY: a NUL-terminated name; no security attributes, so it is not inherited.
    let ours = unsafe {
        CreateNamedPipeW(
            wide.as_ptr(),
            access | FILE_FLAG_OVERLAPPED | FILE_FLAG_FIRST_PIPE_INSTANCE,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            1,
            PIPE,
            PIPE,
            0,
            std::ptr::null(),
        )
    };
    let ours = sys::owned(ours)?;
    let inherit = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: std::ptr::null_mut(),
        bInheritHandle: 1,
    };
    // The child's reading end may set its own pipe's state, as a console program does.
    let rights = match inbound {
        true => GENERIC_WRITE,
        false => GENERIC_READ | FILE_WRITE_ATTRIBUTES,
    };
    // SAFETY: the name just made and attributes that outlive the call.
    let theirs = unsafe {
        CreateFileW(wide.as_ptr(), rights, 0, &inherit, OPEN_EXISTING, 0, std::ptr::null_mut())
    };
    Ok((ours, sys::owned(theirs)?))
}

/// The child's input where it is given none: the null device, as on Unix.
fn nothing() -> io::Result<OwnedHandle> {
    let inherit = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: std::ptr::null_mut(),
        bInheritHandle: 1,
    };
    let wide = sys::wide(OsStr::new("NUL"))?;
    // SAFETY: a NUL-terminated name and attributes that outlive the call.
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            GENERIC_READ,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            &inherit,
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        )
    };
    sys::owned(handle)
}

/// A list of one attribute: the handles the child inherits, which outlive it.
struct Attributes {
    memory: Vec<usize>,
}

impl Attributes {
    fn new(handles: &[HANDLE; 3]) -> io::Result<Attributes> {
        let mut size = 0;
        // SAFETY: a query of the size, writing nothing but `size`; it fails, as it must.
        unsafe { InitializeProcThreadAttributeList(std::ptr::null_mut(), 1, 0, &mut size) };
        // alloc: per process, made once as it starts (CreateProcessW costs a thousand times more).
        let mut memory = vec![0usize; size.div_ceil(size_of::<usize>())];
        let list: LPPROC_THREAD_ATTRIBUTE_LIST = memory.as_mut_ptr().cast();
        // SAFETY: memory of the size asked for, aligned for the pointers it holds.
        sys::check(unsafe { InitializeProcThreadAttributeList(list, 1, 0, &mut size) })?;
        let attributes = Attributes { memory };
        // SAFETY: the list just made; the handles live in the caller's array for as long as
        // the list is used.
        sys::check(unsafe {
            UpdateProcThreadAttribute(
                attributes.list(),
                0,
                PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
                handles.as_ptr().cast(),
                size_of_val(handles),
                std::ptr::null_mut(),
                std::ptr::null(),
            )
        })?;
        Ok(attributes)
    }

    fn list(&self) -> LPPROC_THREAD_ATTRIBUTE_LIST {
        self.memory.as_ptr().cast_mut().cast()
    }
}
impl Drop for Attributes {
    fn drop(&mut self) {
        // SAFETY: the list `new` made.
        unsafe { DeleteProcThreadAttributeList(self.list()) };
    }
}

/// The program and its arguments as one command line, each quoted as the C runtime's
/// parser (`CommandLineToArgvW`) reads it back.
fn command_line(program: &Path, args: &[OsString]) -> io::Result<Vec<u16>> {
    // alloc: per process, made once as it starts (CreateProcessW costs a thousand times more).
    let mut line = Vec::new();
    quote(program.as_os_str(), &mut line)?;
    for arg in args {
        line.push(u16::from(b' '));
        quote(arg, &mut line)?;
    }
    line.push(0);
    Ok(line)
}

fn quote(arg: &OsStr, line: &mut Vec<u16>) -> io::Result<()> {
    // alloc: per process, made once as it starts (CreateProcessW costs a thousand times more).
    let wide: Vec<u16> = arg.encode_wide().collect();
    if wide.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            // alloc: an error path.
            format!("{}: a NUL inside an argument", arg.to_string_lossy()),
        ));
    }
    let special = |c: u16| matches!(c, 0x20 | 0x09 | 0x0A | 0x0B | 0x22);
    if !wide.is_empty() && !wide.iter().copied().any(special) {
        line.extend(wide);
        return Ok(());
    }
    let (quote, backslash) = (u16::from(b'"'), u16::from(b'\\'));
    line.push(quote);
    let mut backslashes = 0;
    for c in wide {
        if c == backslash {
            backslashes += 1;
        } else {
            // Backslashes before a quote are doubled, and the quote escaped.
            if c == quote {
                line.extend(std::iter::repeat_n(backslash, backslashes + 1));
            }
            backslashes = 0;
        }
        line.push(c);
    }
    // And before the closing quote.
    line.extend(std::iter::repeat_n(backslash, backslashes));
    line.push(quote);
    Ok(())
}

/// The whole environment as a block: `NAME=value` each, NUL-terminated, sorted by name as
/// Windows sorts them (names are of no case), a later one of a name replacing the earlier,
/// then a NUL.
fn environment(env: &[(OsString, OsString)]) -> io::Result<Vec<u16>> {
    // alloc: per process, made once as it starts (CreateProcessW costs a thousand times more).
    let mut by_name: Vec<(Vec<u16>, &OsString, &OsString)> = Vec::new();
    for (name, value) in env {
        // alloc: per process, made once as it starts (CreateProcessW costs a thousand times more).
        let wide: Vec<u16> = name.encode_wide().collect();
        if wide.is_empty() || wide[1..].contains(&u16::from(b'=')) || wide.contains(&0) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                // alloc: an error path.
                format!("{}: not a name an environment holds", name.to_string_lossy()),
            ));
        }
        if value.encode_wide().any(|it| it == 0) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                // alloc: an error path.
                format!("{}: a NUL inside its value", name.to_string_lossy()),
            ));
        }
        // alloc: per process, made once as it starts (CreateProcessW costs a thousand times more).
        let key: Vec<u16> = wide.iter().map(|&c| upper(c)).collect();
        by_name.retain(|(it, ..)| *it != key);
        by_name.push((key, name, value));
    }
    by_name.sort_by(|a, b| a.0.cmp(&b.0));
    // alloc: per process, made once as it starts (CreateProcessW costs a thousand times more).
    let mut block = Vec::new();
    for (_, name, value) in by_name {
        block.extend(name.encode_wide());
        block.push(u16::from(b'='));
        block.extend(value.encode_wide());
        block.push(0);
    }
    // An empty block is two NULs.
    if block.is_empty() {
        block.push(0);
    }
    block.push(0);
    Ok(block)
}

/// `c` in upper case where it is an ASCII letter: names in an environment are compared
/// without case.
fn upper(c: u16) -> u16 {
    match char::from_u32(u32::from(c)) {
        Some(it) if it.is_ascii_lowercase() => it.to_ascii_uppercase() as u16,
        _ => c,
    }
}

/// What the thread pool's wait posts once the process has ended: [`KEY_EXIT`] with the bits
/// of its spawn's handle, to the driver's port. Inline in the spawn's entry of the driver's
/// slab, which never moves while it is there, and read by a thread of the pool's while the
/// wait is registered (`UnsafeCell`; no `&mut` of the spawn may assume it alone reaches it:
/// `PhantomPinned`). The driver drops the spawn's [`Waiting`], which waits out a call in
/// progress, before the spawn leaves its entry.
pub(super) struct Waiter {
    posted: UnsafeCell<(HANDLE, u64)>,
    _pinned: PhantomPinned,
}

impl Waiter {
    /// Posting to `port`, which outlives every wait of the driver's, with `id`.
    pub(super) fn new(port: &Port, id: u64) -> Waiter {
        Waiter { posted: UnsafeCell::new((port.raw(), id)), _pinned: PhantomPinned }
    }
}

/// Asks the thread pool to post as its [`Waiter`] says once the process has ended;
/// unregistered, and waited out, when dropped.
pub(super) struct Waiting {
    wait: HANDLE,
}

/// # Safety
/// `context` is a [`Waiter`] that stays where it is until the wait that calls this is
/// unregistered by `Waiting::drop`, which waits for a call in progress.
unsafe extern "system" fn ended(context: *mut c_void, _timed_out: bool) {
    // SAFETY: as above; the waiter is written only as it is made, before its wait.
    let (port, id) = unsafe { *context.cast::<(HANDLE, u64)>() };
    _ = sys::post(port, KEY_EXIT, 0, id as usize);
}

impl Waiting {
    /// # Safety
    ///
    /// `waiter` stays where it is, alive, until the `Waiting` is dropped.
    pub(super) unsafe fn new(process: &OwnedHandle, waiter: &Waiter) -> io::Result<Waiting> {
        let context = waiter.posted.get();
        let mut wait = std::ptr::null_mut();
        // SAFETY: an open process, a callback of the signature asked for, and a context that
        // lives until the wait is unregistered (the caller's word).
        let ok = unsafe {
            RegisterWaitForSingleObject(
                &mut wait,
                process.as_raw_handle(),
                Some(ended),
                context.cast_const().cast(),
                INFINITE,
                WT_EXECUTEONLYONCE,
            )
        };
        match ok {
            0 => Err(io::Error::last_os_error()),
            _ => Ok(Waiting { wait }),
        }
    }
}

impl Drop for Waiting {
    fn drop(&mut self) {
        // SAFETY: the wait `new` registered; `INVALID_HANDLE_VALUE` waits for a callback in
        // progress to return, after which no call reads the waiter.
        unsafe { UnregisterWaitEx(self.wait, INVALID_HANDLE_VALUE) };
    }
}

impl Finish {
    pub(super) fn program(&self) -> &Path {
        &self.command.program
    }

    /// How the process ended: its exit code, the most memory its job held, what its sandbox
    /// says it read. Removes what was made for it.
    pub(super) fn exited(
        &self,
        process: &OwnedHandle,
        job: &OwnedHandle,
        stdout: Vec<u8>,
        stderr: Vec<u8>,
    ) -> io::Result<Exit> {
        let ended = (|| {
            let mut code = 0u32;
            // SAFETY: an open process and a code to write.
            sys::check(unsafe { GetExitCodeProcess(process.as_raw_handle(), &mut code) })?;
            Ok::<_, io::Error>((code, peak(job)?))
        })();
        let traced = match (&ended, &self.command.sandbox, &self.confine, &self.scratch) {
            (Ok(_), Sandbox::On { trace: true, .. }, Some(confine), Some(scratch)) => {
                Some(confine.traced(&self.command, scratch).map_err(|why| {
                    // alloc: an error path.
                    io::Error::other(format!(
                        "{}: its sandbox's record of what it read: {why}",
                        self.command.program.display()
                    ))
                }))
            }
            _ => None,
        };
        if let Some(scratch) = &self.scratch {
            _ = std::fs::remove_dir_all(scratch);
        }
        let (code, peak_memory) = ended?;
        let traced = traced.transpose()?;
        Ok(Exit { code: Some(code as i32), signal: None, stdout, stderr, traced, peak_memory })
    }
}

/// The most memory (commit) everything in the job held at once.
fn peak(job: &OwnedHandle) -> io::Result<u64> {
    // SAFETY: all zeros is a valid buffer for the call to fill.
    let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: an open job and a buffer of the class named, of its size.
    sys::check(unsafe {
        QueryInformationJobObject(
            job.as_raw_handle(),
            JobObjectExtendedLimitInformation,
            (&raw mut limits).cast(),
            size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            std::ptr::null_mut(),
        )
    })?;
    Ok(limits.PeakJobMemoryUsed as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(args: &[&str]) -> String {
        let args: Vec<OsString> = args.iter().map(OsString::from).collect();
        let wide = command_line(Path::new(r"C:\Program Files\x.exe"), &args).unwrap();
        String::from_utf16(&wide[..wide.len() - 1]).unwrap()
    }

    #[test]
    fn arguments_are_quoted_as_the_runtime_reads_them() {
        assert_eq!(line(&["plain", ""]), r#""C:\Program Files\x.exe" plain """#);
        assert_eq!(line(&["a b", r#"say "hi""#]), r#""C:\Program Files\x.exe" "a b" "say \"hi\"""#);
        assert_eq!(line(&[r"dir\", r"a\\b c\"]), r#""C:\Program Files\x.exe" dir\ "a\\b c\\""#);
        assert_eq!(line(&[r#"\"x"#]), r#""C:\Program Files\x.exe" "\\\"x""#);
    }

    #[test]
    fn the_environment_is_sorted_its_later_names_kept() {
        let env: Vec<(OsString, OsString)> = [("Path", "a"), ("HOME", "h"), ("PATH", "b"), ("=C:", "C:\\")]
            .iter()
            .map(|(k, v)| (OsString::from(k), OsString::from(v)))
            .collect();
        let block = environment(&env).unwrap();
        let text = String::from_utf16(&block).unwrap();
        assert_eq!(text, "=C:=C:\\\0HOME=h\0PATH=b\0\0");
        assert_eq!(environment(&[]).unwrap(), [0, 0]);
        assert!(environment(&[(OsString::from("A=B"), OsString::new())]).is_err());
    }
}
