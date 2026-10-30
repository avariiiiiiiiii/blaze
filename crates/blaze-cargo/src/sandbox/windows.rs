//! The sandbox on Windows: an AppContainer in a job object ([#1390]). The launcher's own process
//! stays outside: it creates the program's process in the container, suspended, puts it in a job
//! that holds it and everything it starts, lets it go, waits for it and takes its write grants
//! back.
//!
//! A process in an AppContainer opens a file only where the file's DACL lets its user and also
//! the container's SID, a capability SID the process holds, or ALL APPLICATION PACKAGES (the
//! system's own directories) open it. The container is one profile, [`PROFILE`], made once; the
//! reads a spec names are granted to its SID, inheritably, and kept, so each directory is granted
//! once. Writes are granted for one run to a capability SID of the run's own, derived from a
//! random name no other process holds, and taken back as the run ends: a grant a killed launcher
//! leaves behind names a SID nothing holds again. There is no network without the internet and
//! private-network capabilities, which only a spec with `network` is given. What is read is not
//! traced on Windows: the caller says so in the record.
//!
//! [#1390]: https://github.com/indexable-inc/blaze/issues/1390

use std::{
    collections::BTreeMap,
    ffi::{OsStr, OsString, c_void},
    fmt::Display,
    os::windows::ffi::OsStrExt as _,
    path::{Path, PathBuf},
    ptr::{null, null_mut},
    sync::{Condvar, Mutex, OnceLock, PoisonError},
    time::{Duration, Instant},
};

use blaze_usercode::confine::Spec;
use windows_sys::{
    Win32::{
        Foundation::{
            CloseHandle, DUPLICATE_SAME_ACCESS, DuplicateHandle, ERROR_ALREADY_EXISTS,
            ERROR_SUCCESS, GENERIC_ALL, GENERIC_EXECUTE, GENERIC_READ, GENERIC_WRITE, HANDLE,
            INVALID_HANDLE_VALUE, LocalFree, STATUS_CONTROL_C_EXIT, WAIT_ABANDONED, WAIT_OBJECT_0,
        },
        Security::{
            ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, ACL_SIZE_INFORMATION, AclSizeInformation,
            Authorization::{
                EXPLICIT_ACCESS_W, GRANT_ACCESS, GetNamedSecurityInfoW, NO_MULTIPLE_TRUSTEE,
                REVOKE_ACCESS, SE_FILE_OBJECT, SetEntriesInAclW, SetNamedSecurityInfoW,
                TRUSTEE_IS_SID, TRUSTEE_IS_WELL_KNOWN_GROUP, TRUSTEE_W,
            },
            CONTAINER_INHERIT_ACE, CreateWellKnownSid, DACL_SECURITY_INFORMATION,
            DeriveCapabilitySidsFromName, EqualSid, FreeSid, GetAce, GetAclInformation,
            INHERIT_ONLY_ACE,
            Isolation::{CreateAppContainerProfile, DeriveAppContainerSidFromAppContainerName},
            NO_INHERITANCE, NO_PROPAGATE_INHERIT_ACE, OBJECT_INHERIT_ACE, PSECURITY_DESCRIPTOR,
            PSID, SECURITY_CAPABILITIES, SECURITY_MAX_SID_SIZE, SID_AND_ATTRIBUTES,
            SUB_CONTAINERS_AND_OBJECTS_INHERIT, WELL_KNOWN_SID_TYPE, WinBuiltinAnyPackageSid,
            WinCapabilityInternetClientServerSid, WinCapabilityInternetClientSid,
            WinCapabilityPrivateNetworkClientServerSid,
        },
        Storage::FileSystem::{
            FILE_ALL_ACCESS, FILE_GENERIC_EXECUTE, FILE_GENERIC_READ, FILE_GENERIC_WRITE,
        },
        System::{
            Console::{
                CTRL_BREAK_EVENT, CTRL_C_EVENT, CTRL_CLOSE_EVENT, GetStdHandle, STD_ERROR_HANDLE,
                STD_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, SetConsoleCtrlHandler,
            },
            JobObjects::{
                AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION,
                JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOB_OBJECT_UILIMIT_DESKTOP,
                JOB_OBJECT_UILIMIT_DISPLAYSETTINGS, JOB_OBJECT_UILIMIT_EXITWINDOWS,
                JOB_OBJECT_UILIMIT_GLOBALATOMS, JOB_OBJECT_UILIMIT_HANDLES,
                JOB_OBJECT_UILIMIT_READCLIPBOARD, JOB_OBJECT_UILIMIT_SYSTEMPARAMETERS,
                JOB_OBJECT_UILIMIT_WRITECLIPBOARD, JOBOBJECT_BASIC_ACCOUNTING_INFORMATION,
                JOBOBJECT_BASIC_UI_RESTRICTIONS, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
                JOBOBJECTINFOCLASS, JobObjectBasicAccountingInformation,
                JobObjectBasicUIRestrictions, JobObjectExtendedLimitInformation,
                QueryInformationJobObject, SetInformationJobObject, TerminateJobObject,
            },
            SystemServices::{ACCESS_ALLOWED_ACE_TYPE, SE_GROUP_ENABLED},
            Threading::{
                CREATE_NO_WINDOW, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, CreateMutexW,
                CreateProcessW, DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT,
                GetCurrentProcess, GetExitCodeProcess, INFINITE, InitializeProcThreadAttributeList,
                LPPROC_THREAD_ATTRIBUTE_LIST, PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
                PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES, PROCESS_INFORMATION, ReleaseMutex,
                ResumeThread, STARTF_USESTDHANDLES, STARTUPINFOEXW, STARTUPINFOW,
                TerminateProcess, UpdateProcThreadAttribute, WaitForSingleObject,
            },
        },
    },
    core::{BOOL, HRESULT},
};

/// The AppContainer profile every run is in.
const PROFILE: &str = "blaze.sandbox";

/// What a read grant gives: open, list, read and run.
const READ: u32 = FILE_GENERIC_READ | FILE_GENERIC_EXECUTE;

/// What a write grant gives.
const WRITE: u32 = FILE_ALL_ACCESS;

/// `HRESULT_FROM_WIN32(ERROR_ALREADY_EXISTS)`: the profile was made before.
const PROFILE_EXISTS: HRESULT = (0x8007_0000 | ERROR_ALREADY_EXISTS) as HRESULT;

/// The lock every launcher of the session holds while it changes a DACL: a change reads the
/// DACL and writes it whole, and two at once would lose one's entry.
const LOCK: &str = "Local\\blaze.sandbox.acl";

/// How long a launcher waits for the processes of its job to be gone once it ended them.
const ENDING: Duration = Duration::from_secs(5);

/// A SID of the system's: as long as the longest, and aligned as a SID's fields are.
type SidBuffer = [u32; (SECURITY_MAX_SID_SIZE / 4) as usize];

/// Runs `spec.program` with `spec.args`, exactly `spec.env` (plus TMP and TEMP pointing at the
/// run's own temporary directory), in `spec.dir`, sandboxed: what it ends with, its exit code.
/// `Err(why)` when no sandbox could be made, and then nothing ran.
pub fn run(spec: &Spec) -> Result<i32, String> {
    let _running = Running::start();
    on_interrupt()?;
    let mut made = make(spec.network)?;
    let temp = temporary(spec)?;
    // What is there was left by a launcher that was killed: the scheduler runs a spec once at a
    // time.
    if temp.exists() {
        std::fs::remove_dir_all(&temp).map_err(|error| {
            format!("removing a killed run's temporary directory {}: {error}", temp.display())
        })?;
    }
    std::fs::create_dir(&temp)
        .map_err(|error| format!("making the run's temporary directory {}: {error}", temp.display()))?;
    let mut granted = Granted { sid: made.run.sid(), writes: Vec::new(), temp: temp.clone() };
    let writes: Vec<&Path> = spec.write.iter().map(PathBuf::as_path).chain([temp.as_path()]).collect();
    for dir in &writes {
        std::fs::create_dir_all(dir).map_err(|error| {
            format!("making the sandbox's writable directory {}: {error}", dir.display())
        })?;
    }
    let program_dir = spec.program.parent().filter(|it| !it.as_os_str().is_empty());
    let readers = [made.container.0, sid(&made.any)];
    let mut reads = Vec::new();
    for path in spec.read.iter().map(PathBuf::as_path).chain(program_dir).chain([spec.dir.as_path()]) {
        let Ok(metadata) = std::fs::metadata(path) else { continue };
        let dir = metadata.is_dir();
        // Read grants are only ever added, so one seen without the lock stays: only a missing
        // one is changed, under the lock, after looking again.
        if !covered(path, dir, &readers, READ, "granting the sandbox read")? {
            reads.push((path, dir));
        }
    }
    {
        let _lock = Lock::take()?;
        for (path, dir) in reads {
            grant(path, dir, made.container.0, &readers, READ, "granting the sandbox read")?;
        }
        // Each is a directory: `create_dir_all` made it or found one.
        for dir in writes {
            if grant(dir, true, granted.sid, &[granted.sid], WRITE, "granting the sandbox write")? {
                granted.writes.push(dir.to_owned());
            }
        }
    }

    let mut env = spec.env.clone();
    env.push(("TMP".into(), temp.clone().into_os_string()));
    env.push(("TEMP".into(), temp.into_os_string()));
    let block = environment(&env)?;
    let mut line = command_line(&spec.program, &spec.args)?;
    let program = wide(spec.program.as_os_str(), "the program's path")?;
    let dir = wide(spec.dir.as_os_str(), "the program's directory")?;
    let mut startup = STARTUPINFOEXW::default();
    startup.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
    startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    let standard = |at: usize| made.standard[at].as_ref().map_or(null_mut(), |it| it.0);
    startup.StartupInfo.hStdInput = standard(0);
    startup.StartupInfo.hStdOutput = standard(1);
    startup.StartupInfo.hStdError = standard(2);
    startup.lpAttributeList = made.attributes.list();
    let inherit = BOOL::from(!made.inherited.is_empty());
    let mut process = PROCESS_INFORMATION::default();
    // SAFETY: the strings end in NUL, the command line is the program's to change, the block is
    // a Unicode environment ending in two NULs, and the attribute list and what it points to
    // (`made`) outlive the call.
    let created = unsafe {
        CreateProcessW(
            program.as_ptr(),
            line.as_mut_ptr(),
            null(),
            null(),
            inherit,
            CREATE_SUSPENDED | EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT | CREATE_NO_WINDOW,
            block.as_ptr().cast(),
            dir.as_ptr(),
            (&raw const startup).cast::<STARTUPINFOW>(),
            &mut process,
        )
    };
    if created == 0 {
        return Err(failed(format!("starting {} in the sandbox", spec.program.display()), "CreateProcessW"));
    }
    let child = Handle(process.hProcess);
    let thread = Handle(process.hThread);
    // SAFETY: both handles are open.
    if unsafe { AssignProcessToJobObject(made.job.0, child.0) } == 0 {
        let why = failed("holding the program in the sandbox's job", "AssignProcessToJobObject");
        // SAFETY: the process is open and has not run.
        unsafe { TerminateProcess(child.0, 1) };
        return Err(why);
    }
    let _registered = Registered::new(&made.job);
    // SAFETY: the thread is open and suspended.
    if unsafe { ResumeThread(thread.0) } == u32::MAX {
        let why = failed("letting the program run", "ResumeThread");
        end(&made.job, 1);
        return Err(why);
    }
    drop(thread);
    // SAFETY: the process is open.
    if unsafe { WaitForSingleObject(child.0, INFINITE) } != WAIT_OBJECT_0 {
        let why = failed("waiting for the program, which started", "WaitForSingleObject");
        end(&made.job, 1);
        return Err(why);
    }
    let mut code = 0u32;
    // SAFETY: the process is open and has ended.
    if unsafe { GetExitCodeProcess(child.0, &mut code) } == 0 {
        let why = failed("taking the exit code of the program, which ran", "GetExitCodeProcess");
        end(&made.job, 1);
        return Err(why);
    }
    end(&made.job, code);
    Ok(code as i32)
}

/// Makes a sandbox around nothing (no program run): `Ok` when one can be made here, why not
/// otherwise.
pub fn probe() -> Result<(), String> {
    let _made = make(true)?;
    let _lock = Lock::take()?;
    Ok(())
}

/// What a run is made of up to its process: the container, the run's capability, the job and
/// the process's attribute list. The list points into the capabilities, the SIDs and the
/// handles beside it, which are on the heap, where moving `Made` leaves them.
struct Made {
    attributes: Attributes,
    _capabilities: Box<SECURITY_CAPABILITIES>,
    _granted: Vec<SID_AND_ATTRIBUTES>,
    _network: Vec<SidBuffer>,
    any: Box<SidBuffer>,
    run: Capability,
    container: Container,
    job: Handle,
    standard: [Option<Handle>; 3],
    inherited: Vec<HANDLE>,
}

fn make(network: bool) -> Result<Made, String> {
    let container = Container::open()?;
    let run = Capability::new()?;
    let any = Box::new(well_known(WinBuiltinAnyPackageSid, "ALL APPLICATION PACKAGES")?);
    let network = match network {
        true => vec![
            well_known(WinCapabilityInternetClientSid, "the internetClient capability")?,
            well_known(WinCapabilityInternetClientServerSid, "the internetClientServer capability")?,
            well_known(WinCapabilityPrivateNetworkClientServerSid, "the privateNetworkClientServer capability")?,
        ],
        false => Vec::new(),
    };
    let enabled = SE_GROUP_ENABLED as u32;
    let mut granted = vec![SID_AND_ATTRIBUTES { Sid: run.sid(), Attributes: enabled }];
    granted.extend(network.iter().map(|it| SID_AND_ATTRIBUTES { Sid: sid(it), Attributes: enabled }));
    let capabilities = Box::new(SECURITY_CAPABILITIES {
        AppContainerSid: container.0,
        Capabilities: granted.as_mut_ptr(),
        CapabilityCount: granted.len() as u32,
        Reserved: 0,
    });
    let job = job()?;
    let standard = standard()?;
    let inherited: Vec<HANDLE> = standard.iter().flatten().map(|it| it.0).collect();
    let mut attributes = Attributes::new(if inherited.is_empty() { 1 } else { 2 })?;
    attributes.set(
        PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES,
        (&raw const *capabilities).cast(),
        size_of::<SECURITY_CAPABILITIES>(),
        "putting the program in the sandbox's AppContainer",
    )?;
    if !inherited.is_empty() {
        attributes.set(
            PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
            inherited.as_ptr().cast(),
            inherited.len() * size_of::<HANDLE>(),
            "passing the program its standard handles and no other",
        )?;
    }
    Ok(Made {
        attributes,
        _capabilities: capabilities,
        _granted: granted,
        _network: network,
        any,
        run,
        container,
        job,
        standard,
        inherited,
    })
}

fn failed(what: impl Display, call: &str) -> String {
    format!("{what} ({call}): {}", std::io::Error::last_os_error())
}

fn failed_with(what: impl Display, call: &str, code: i32) -> String {
    format!("{what} ({call}): {}", std::io::Error::from_raw_os_error(code))
}

/// The run's temporary directory, which the program sees as TMP and TEMP: named by what the spec
/// is, so the same spec sees the same path.
fn temporary(spec: &Spec) -> Result<PathBuf, String> {
    use sha2::Digest as _;
    let bytes = serde_json::to_vec(spec).map_err(|error| format!("the spec: {error}"))?;
    let digest = sha2::Sha256::digest(&bytes);
    let name: String = digest[..8].iter().map(|byte| format!("{byte:02x}")).collect();
    Ok(std::env::temp_dir().join(format!("blaze-sandbox-{name}")))
}

fn random(what: &str) -> Result<u64, String> {
    getrandom::u64().map_err(|error| format!("random bytes for {what}: {error}"))
}

/// `text` as UTF-16 ending in NUL, or why not when it has a NUL of its own.
fn wide(text: &OsStr, what: &str) -> Result<Vec<u16>, String> {
    let mut out: Vec<u16> = text.encode_wide().collect();
    if out.contains(&0) {
        return Err(format!("{what} has a NUL in it: {}", text.to_string_lossy()));
    }
    out.push(0);
    Ok(out)
}

fn literal(text: &str) -> Vec<u16> {
    text.encode_utf16().chain([0]).collect()
}

fn sid(buffer: &SidBuffer) -> PSID {
    buffer.as_ptr().cast_mut().cast()
}

/// A handle, closed when dropped.
struct Handle(HANDLE);

impl Drop for Handle {
    fn drop(&mut self) {
        if !self.0.is_null() && self.0 != INVALID_HANDLE_VALUE {
            // SAFETY: the handle is this one's to close.
            unsafe { CloseHandle(self.0) };
        }
    }
}

/// The SID of the [`PROFILE`] container, made the first time.
struct Container(PSID);

impl Container {
    fn open() -> Result<Container, String> {
        let name = literal(PROFILE);
        let shown = literal("blaze sandbox");
        let mut sid: PSID = null_mut();
        // SAFETY: the strings end in NUL; no capabilities are given the profile.
        let made = unsafe {
            CreateAppContainerProfile(name.as_ptr(), shown.as_ptr(), shown.as_ptr(), null(), 0, &mut sid)
        };
        if made == PROFILE_EXISTS {
            // SAFETY: the name ends in NUL.
            let found = unsafe { DeriveAppContainerSidFromAppContainerName(name.as_ptr(), &mut sid) };
            if found < 0 {
                return Err(failed_with(
                    format!("finding the sandbox's AppContainer profile {PROFILE}"),
                    "DeriveAppContainerSidFromAppContainerName",
                    found,
                ));
            }
        } else if made < 0 {
            return Err(failed_with(
                format!("making the sandbox's AppContainer profile {PROFILE}"),
                "CreateAppContainerProfile",
                made,
            ));
        }
        Ok(Container(sid))
    }
}

impl Drop for Container {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: the SID is the profile functions', which are freed with FreeSid.
            unsafe { FreeSid(self.0) };
        }
    }
}

/// The capability SID of one run, derived from a random name.
struct Capability {
    groups: *mut PSID,
    group_count: u32,
    sids: *mut PSID,
    count: u32,
}

impl Capability {
    fn new() -> Result<Capability, String> {
        let what = "the run's capability";
        let name = literal(&format!("blaze.sandbox.run.{:016x}{:016x}", random(what)?, random(what)?));
        let mut it = Capability { groups: null_mut(), group_count: 0, sids: null_mut(), count: 0 };
        // SAFETY: the name ends in NUL; the lists it makes are freed when `it` is dropped.
        let derived = unsafe {
            DeriveCapabilitySidsFromName(
                name.as_ptr(),
                &mut it.groups,
                &mut it.group_count,
                &mut it.sids,
                &mut it.count,
            )
        };
        if derived == 0 {
            return Err(failed("deriving the run's capability SID", "DeriveCapabilitySidsFromName"));
        }
        if it.count == 0 || it.sids.is_null() {
            return Err("deriving the run's capability SID (DeriveCapabilitySidsFromName): it gave none".into());
        }
        Ok(it)
    }

    fn sid(&self) -> PSID {
        // SAFETY: there is at least one.
        unsafe { *self.sids }
    }
}

impl Drop for Capability {
    fn drop(&mut self) {
        for (list, count) in [(self.groups, self.group_count), (self.sids, self.count)] {
            if list.is_null() {
                continue;
            }
            // SAFETY: the list and each SID in it were made with LocalAlloc.
            unsafe {
                for at in 0..count as usize {
                    LocalFree(*list.add(at));
                }
                LocalFree(list.cast());
            }
        }
    }
}

fn well_known(kind: WELL_KNOWN_SID_TYPE, what: &str) -> Result<SidBuffer, String> {
    let mut buffer: SidBuffer = [0; (SECURITY_MAX_SID_SIZE / 4) as usize];
    let mut size = SECURITY_MAX_SID_SIZE;
    // SAFETY: the buffer is `size` bytes.
    if unsafe { CreateWellKnownSid(kind, null_mut(), buffer.as_mut_ptr().cast(), &mut size) } == 0 {
        return Err(failed(format!("making the SID of {what}"), "CreateWellKnownSid"));
    }
    Ok(buffer)
}

/// The job a run's processes are held in: killed when it is closed, a crash ending a process
/// rather than waiting on a dialog, no way out for a child, and none of the desktop's shared
/// state (the clipboard, atoms, other processes' windows, the display and system settings,
/// logging off).
fn job() -> Result<Handle, String> {
    // SAFETY: no attributes, no name.
    let job = Handle(unsafe { CreateJobObjectW(null(), null()) });
    if job.0.is_null() {
        return Err(failed("making the sandbox's job", "CreateJobObjectW"));
    }
    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    limits.BasicLimitInformation.LimitFlags =
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION;
    set_job(&job, JobObjectExtendedLimitInformation, &limits, "limiting the sandbox's job")?;
    let ui = JOBOBJECT_BASIC_UI_RESTRICTIONS {
        UIRestrictionsClass: JOB_OBJECT_UILIMIT_DESKTOP
            | JOB_OBJECT_UILIMIT_DISPLAYSETTINGS
            | JOB_OBJECT_UILIMIT_EXITWINDOWS
            | JOB_OBJECT_UILIMIT_GLOBALATOMS
            | JOB_OBJECT_UILIMIT_HANDLES
            | JOB_OBJECT_UILIMIT_READCLIPBOARD
            | JOB_OBJECT_UILIMIT_WRITECLIPBOARD
            | JOB_OBJECT_UILIMIT_SYSTEMPARAMETERS,
    };
    set_job(&job, JobObjectBasicUIRestrictions, &ui, "keeping the sandbox's job off the desktop")?;
    Ok(job)
}

fn set_job<T>(job: &Handle, class: JOBOBJECTINFOCLASS, info: &T, what: &str) -> Result<(), String> {
    // SAFETY: `info` is the structure `class` names.
    let set = unsafe { SetInformationJobObject(job.0, class, (info as *const T).cast(), size_of::<T>() as u32) };
    match set {
        0 => Err(failed(what, "SetInformationJobObject")),
        _ => Ok(()),
    }
}

/// Ends every process of `job` with `code` and waits, for a while, for them to be gone, so what
/// they had open is closed before the run's directories are taken back.
fn end(job: &Handle, code: u32) {
    // SAFETY: the job is open.
    unsafe { TerminateJobObject(job.0, code) };
    let started = Instant::now();
    loop {
        let mut info = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
        // SAFETY: `info` is the structure the class names.
        let asked = unsafe {
            QueryInformationJobObject(
                job.0,
                JobObjectBasicAccountingInformation,
                (&raw mut info).cast(),
                size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
                null_mut(),
            )
        };
        if asked == 0 || info.ActiveProcesses == 0 {
            return;
        }
        if started.elapsed() > ENDING {
            eprintln!("blaze: {} processes of the sandbox's job did not end", info.ActiveProcesses);
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Copies of the launcher's standard input, output and error the program inherits, `None` for
/// one the launcher has not.
fn standard() -> Result<[Option<Handle>; 3], String> {
    let mut out = [None, None, None];
    let which: [(STD_HANDLE, &str); 3] =
        [(STD_INPUT_HANDLE, "input"), (STD_OUTPUT_HANDLE, "output"), (STD_ERROR_HANDLE, "error")];
    for (at, (which, name)) in which.into_iter().enumerate() {
        // SAFETY: no arguments but which.
        let handle = unsafe { GetStdHandle(which) };
        if handle.is_null() || handle == INVALID_HANDLE_VALUE {
            continue;
        }
        let mut copy: HANDLE = null_mut();
        // SAFETY: the handle is the process's own; the copy is closed when dropped.
        let copied = unsafe {
            let process = GetCurrentProcess();
            DuplicateHandle(process, handle, process, &mut copy, 0, 1, DUPLICATE_SAME_ACCESS)
        };
        if copied == 0 {
            return Err(failed(format!("passing the program the launcher's standard {name}"), "DuplicateHandle"));
        }
        out[at] = Some(Handle(copy));
    }
    Ok(out)
}

/// The attribute list of the program's process.
struct Attributes(Vec<usize>);

impl Attributes {
    fn new(count: u32) -> Result<Attributes, String> {
        let mut size = 0;
        // SAFETY: a null list asks for its size alone.
        unsafe { InitializeProcThreadAttributeList(null_mut(), count, 0, &mut size) };
        if size == 0 {
            return Err(failed("sizing the program's attribute list", "InitializeProcThreadAttributeList"));
        }
        let mut buffer = vec![0usize; size.div_ceil(size_of::<usize>())];
        // SAFETY: the buffer is at least `size` bytes.
        if unsafe { InitializeProcThreadAttributeList(buffer.as_mut_ptr().cast(), count, 0, &mut size) } == 0 {
            return Err(failed("making the program's attribute list", "InitializeProcThreadAttributeList"));
        }
        Ok(Attributes(buffer))
    }

    fn list(&mut self) -> LPPROC_THREAD_ATTRIBUTE_LIST {
        self.0.as_mut_ptr().cast()
    }

    /// Sets `attribute` to the `size` bytes at `value`, which the list points to and which
    /// must outlive it.
    fn set(&mut self, attribute: u32, value: *const c_void, size: usize, what: &str) -> Result<(), String> {
        // SAFETY: the list is made, and `value` is `size` bytes.
        let set = unsafe {
            UpdateProcThreadAttribute(self.list(), 0, attribute as usize, value, size, null_mut(), null())
        };
        match set {
            0 => Err(failed(what, "UpdateProcThreadAttribute")),
            _ => Ok(()),
        }
    }
}

impl Drop for Attributes {
    fn drop(&mut self) {
        // SAFETY: the list was made.
        unsafe { DeleteProcThreadAttributeList(self.list()) };
    }
}

/// The session's [`LOCK`], held while it is not dropped.
struct Lock(Handle);

impl Lock {
    fn take() -> Result<Lock, String> {
        let name = literal(LOCK);
        // SAFETY: the name ends in NUL.
        let mutex = Handle(unsafe { CreateMutexW(null(), 0, name.as_ptr()) });
        if mutex.0.is_null() {
            return Err(failed(format!("opening the lock on the sandbox's grants {LOCK}"), "CreateMutexW"));
        }
        // SAFETY: the mutex is open.
        match unsafe { WaitForSingleObject(mutex.0, INFINITE) } {
            WAIT_OBJECT_0 | WAIT_ABANDONED => Ok(Lock(mutex)),
            _ => Err(failed(format!("taking the lock on the sandbox's grants {LOCK}"), "WaitForSingleObject")),
        }
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        // SAFETY: this thread holds the mutex.
        unsafe { ReleaseMutex((self.0).0) };
    }
}

/// The DACL of a file or directory, and the security descriptor it is in.
struct Dacl {
    descriptor: PSECURITY_DESCRIPTOR,
    dacl: *mut ACL,
}

impl Dacl {
    fn of(name: &[u16], path: &Path, what: &str) -> Result<Dacl, String> {
        let mut it = Dacl { descriptor: null_mut(), dacl: null_mut() };
        // SAFETY: the name ends in NUL; the descriptor is freed when `it` is dropped.
        let code = unsafe {
            GetNamedSecurityInfoW(
                name.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                null_mut(),
                null_mut(),
                &mut it.dacl,
                null_mut(),
                &mut it.descriptor,
            )
        };
        match code {
            ERROR_SUCCESS => Ok(it),
            _ => Err(failed_with(format!("{what} on {}", path.display()), "GetNamedSecurityInfoW", code as i32)),
        }
    }

    /// Whether one of `sids` has `mask` on what this is set on and, for a directory, on what is
    /// below it, by the entries that allow it.
    fn covers(&self, sids: &[PSID], mask: u32, dir: bool) -> bool {
        let mut info = ACL_SIZE_INFORMATION::default();
        // SAFETY: the DACL is not null, and `info` is the structure the class names.
        let asked = unsafe {
            GetAclInformation(
                self.dacl,
                (&raw mut info).cast(),
                size_of::<ACL_SIZE_INFORMATION>() as u32,
                AclSizeInformation,
            )
        };
        if asked == 0 {
            return false;
        }
        let (mut here, mut dirs, mut files) = (0, 0, 0);
        for at in 0..info.AceCount {
            let mut ace: *mut c_void = null_mut();
            // SAFETY: `at` is below the count.
            if unsafe { GetAce(self.dacl, at, &mut ace) } == 0 {
                continue;
            }
            // SAFETY: every entry starts with its header, and an allowing one is an
            // ACCESS_ALLOWED_ACE whose SID starts at `SidStart`.
            let (header, mask_of, entry_sid) = unsafe {
                let header = *ace.cast::<ACE_HEADER>();
                let allowed = ace.cast::<ACCESS_ALLOWED_ACE>();
                (header, (*allowed).Mask, (&raw mut (*allowed).SidStart).cast::<c_void>())
            };
            if u32::from(header.AceType) != ACCESS_ALLOWED_ACE_TYPE {
                continue;
            }
            // SAFETY: both are SIDs.
            if !sids.iter().any(|it| unsafe { EqualSid(*it, entry_sid) } != 0) {
                continue;
            }
            let given = mapped(mask_of);
            let flags = u32::from(header.AceFlags);
            if flags & INHERIT_ONLY_ACE == 0 {
                here |= given;
            }
            if flags & NO_PROPAGATE_INHERIT_ACE == 0 {
                if flags & CONTAINER_INHERIT_ACE != 0 {
                    dirs |= given;
                }
                if flags & OBJECT_INHERIT_ACE != 0 {
                    files |= given;
                }
            }
        }
        let has = |it: u32| it & mask == mask;
        has(here) && (!dir || (has(dirs) && has(files)))
    }
}

impl Drop for Dacl {
    fn drop(&mut self) {
        if !self.descriptor.is_null() {
            // SAFETY: GetNamedSecurityInfoW made it with LocalAlloc.
            unsafe { LocalFree(self.descriptor) };
        }
    }
}

/// `mask` with its generic rights as the specific ones of a file.
fn mapped(mask: u32) -> u32 {
    let mut out = mask & !(GENERIC_READ | GENERIC_WRITE | GENERIC_EXECUTE | GENERIC_ALL);
    for (generic, specific) in [
        (GENERIC_READ, FILE_GENERIC_READ),
        (GENERIC_WRITE, FILE_GENERIC_WRITE),
        (GENERIC_EXECUTE, FILE_GENERIC_EXECUTE),
        (GENERIC_ALL, FILE_ALL_ACCESS),
    ] {
        if mask & generic != 0 {
            out |= specific;
        }
    }
    out
}

fn trustee(sid: PSID) -> TRUSTEE_W {
    TRUSTEE_W {
        pMultipleTrustee: null_mut(),
        MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
        TrusteeForm: TRUSTEE_IS_SID,
        TrusteeType: TRUSTEE_IS_WELL_KNOWN_GROUP,
        ptstrName: sid.cast(),
    }
}

/// Whether one of `already` has `mask` on `path` (and below it, when it is a directory `dir`), or
/// its DACL is null and lets everyone in.
fn covered(path: &Path, dir: bool, already: &[PSID], mask: u32, what: &str) -> Result<bool, String> {
    let name = wide(path.as_os_str(), "a path the sandbox grants")?;
    let current = Dacl::of(&name, path, what)?;
    Ok(current.dacl.is_null() || current.covers(already, mask, dir))
}

/// Gives `sid` `mask` on `path`, inheritably when it is a directory `dir`, unless one of
/// `already` has it there: whether an entry was added.
fn grant(path: &Path, dir: bool, sid: PSID, already: &[PSID], mask: u32, what: &str) -> Result<bool, String> {
    let name = wide(path.as_os_str(), "a path the sandbox grants")?;
    let current = Dacl::of(&name, path, what)?;
    // A null DACL lets everyone do everything, and has no entries to add to.
    if current.dacl.is_null() || current.covers(already, mask, dir) {
        return Ok(false);
    }
    let access = EXPLICIT_ACCESS_W {
        grfAccessPermissions: mask,
        grfAccessMode: GRANT_ACCESS,
        grfInheritance: if dir { SUB_CONTAINERS_AND_OBJECTS_INHERIT } else { NO_INHERITANCE },
        Trustee: trustee(sid),
    };
    set_dacl(&name, path, &current, &access, what)?;
    Ok(true)
}

/// Takes every entry for `sid` out of the DACL of `path`.
fn revoke(path: &Path, sid: PSID, what: &str) -> Result<(), String> {
    let name = wide(path.as_os_str(), "a path the sandbox granted")?;
    let current = Dacl::of(&name, path, what)?;
    if current.dacl.is_null() {
        return Ok(());
    }
    let access = EXPLICIT_ACCESS_W {
        grfAccessPermissions: 0,
        grfAccessMode: REVOKE_ACCESS,
        grfInheritance: NO_INHERITANCE,
        Trustee: trustee(sid),
    };
    set_dacl(&name, path, &current, &access, what)
}

fn set_dacl(name: &[u16], path: &Path, current: &Dacl, access: &EXPLICIT_ACCESS_W, what: &str) -> Result<(), String> {
    let mut new: *mut ACL = null_mut();
    // SAFETY: one entry, the current DACL as GetNamedSecurityInfoW gave it.
    let code = unsafe { SetEntriesInAclW(1, access, current.dacl, &mut new) };
    if code != ERROR_SUCCESS {
        return Err(failed_with(format!("{what} on {}", path.display()), "SetEntriesInAclW", code as i32));
    }
    // SAFETY: the name ends in NUL and `new` is a DACL, freed after.
    let code = unsafe {
        let code = SetNamedSecurityInfoW(
            name.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            null_mut(),
            null_mut(),
            new,
            null(),
        );
        LocalFree(new.cast());
        code
    };
    match code {
        ERROR_SUCCESS => Ok(()),
        _ => Err(failed_with(format!("{what} on {}", path.display()), "SetNamedSecurityInfoW", code as i32)),
    }
}

/// What a run takes back as it ends, however it ends: the write grants it added and its
/// temporary directory.
struct Granted {
    sid: PSID,
    writes: Vec<PathBuf>,
    temp: PathBuf,
}

impl Drop for Granted {
    fn drop(&mut self) {
        let lock = Lock::take();
        if let Err(why) = &lock {
            eprintln!("blaze: the sandbox's write grants are taken back without the lock: {why}");
        }
        for dir in &self.writes {
            if let Err(why) = revoke(dir, self.sid, "taking back the sandbox's write") {
                eprintln!("blaze: {why}");
            }
        }
        drop(lock);
        if let Err(error) = std::fs::remove_dir_all(&self.temp) {
            eprintln!("blaze: removing the run's temporary directory {}: {error}", self.temp.display());
        }
    }
}

/// The jobs of the runs in progress, for the console's interrupt to end.
static JOBS: Mutex<Vec<usize>> = Mutex::new(Vec::new());

/// How many runs have not taken back their grants yet, for the console's close to wait on.
static RUNS: (Mutex<usize>, Condvar) = (Mutex::new(0), Condvar::new());

/// Counts a run in [`RUNS`] while it is not dropped.
struct Running;

impl Running {
    fn start() -> Running {
        *RUNS.0.lock().unwrap_or_else(PoisonError::into_inner) += 1;
        Running
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        *RUNS.0.lock().unwrap_or_else(PoisonError::into_inner) -= 1;
        RUNS.1.notify_all();
    }
}

/// Names a job in [`JOBS`] while it is not dropped.
struct Registered(usize);

impl Registered {
    fn new(job: &Handle) -> Registered {
        let job = job.0 as usize;
        JOBS.lock().unwrap_or_else(PoisonError::into_inner).push(job);
        Registered(job)
    }
}

impl Drop for Registered {
    fn drop(&mut self) {
        let mut jobs = JOBS.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(at) = jobs.iter().position(|it| *it == self.0) {
            jobs.swap_remove(at);
        }
    }
}

/// Ctrl-C, Ctrl-Break and closing the console end the program's job rather than the launcher,
/// which then takes its grants back and ends as the program did. On a close Windows ends the
/// launcher once this returns, so it waits for that first.
unsafe extern "system" fn interrupted(event: u32) -> BOOL {
    if !matches!(event, CTRL_C_EVENT | CTRL_BREAK_EVENT | CTRL_CLOSE_EVENT) {
        return 0;
    }
    let jobs = JOBS.lock().unwrap_or_else(PoisonError::into_inner);
    for &job in jobs.iter() {
        // SAFETY: a job stays open while it is named in JOBS, whose lock this holds.
        unsafe { TerminateJobObject(job as HANDLE, STATUS_CONTROL_C_EXIT as u32) };
    }
    drop(jobs);
    if event == CTRL_CLOSE_EVENT {
        let runs = RUNS.0.lock().unwrap_or_else(PoisonError::into_inner);
        _ = RUNS.1.wait_timeout_while(runs, Duration::from_secs(4), |runs| *runs > 0);
    }
    1
}

fn on_interrupt() -> Result<(), String> {
    static INSTALLED: OnceLock<Result<(), String>> = OnceLock::new();
    INSTALLED
        .get_or_init(|| {
            // SAFETY: `interrupted` only locks, ends a job and waits.
            match unsafe { SetConsoleCtrlHandler(Some(interrupted), 1) } {
                0 => Err(failed("passing the console's interrupt to the sandbox", "SetConsoleCtrlHandler")),
                _ => Ok(()),
            }
        })
        .clone()
}

/// The command line of `program` and `args`, ending in NUL: the program quoted, each argument
/// as the MSVC runtime splits it back (`std::process::Command`'s rules).
fn command_line(program: &Path, args: &[OsString]) -> Result<Vec<u16>, String> {
    let quote = u16::from(b'"');
    let name: Vec<u16> = program.as_os_str().encode_wide().collect();
    if name.iter().any(|&it| it == 0 || it == quote) {
        return Err(format!("the program's path has a quote or a NUL in it: {}", program.display()));
    }
    let mut line = vec![quote];
    line.extend(name);
    line.push(quote);
    for arg in args {
        line.push(u16::from(b' '));
        append_arg(&mut line, arg)?;
    }
    line.push(0);
    Ok(line)
}

/// `arg`, quoted when it is empty or has a space or a tab; the backslashes before a quote are
/// doubled and the quote escaped, and so are those at the end of a quoted one.
fn append_arg(line: &mut Vec<u16>, arg: &OsStr) -> Result<(), String> {
    let (quote, slash) = (u16::from(b'"'), u16::from(b'\\'));
    let units: Vec<u16> = arg.encode_wide().collect();
    if units.contains(&0) {
        return Err(format!("an argument has a NUL in it: {}", arg.to_string_lossy()));
    }
    let quoted = units.is_empty() || units.iter().any(|&it| it == u16::from(b' ') || it == u16::from(b'\t'));
    if quoted {
        line.push(quote);
    }
    let mut slashes = 0;
    for unit in units {
        if unit == slash {
            slashes += 1;
        } else {
            if unit == quote {
                line.extend(std::iter::repeat_n(slash, slashes + 1));
            }
            slashes = 0;
        }
        line.push(unit);
    }
    if quoted {
        line.extend(std::iter::repeat_n(slash, slashes));
        line.push(quote);
    }
    Ok(())
}

/// The environment block of `env`: `NAME=VALUE` and a NUL each, sorted by name without regard
/// to case as Windows keeps it, a later one of a name in place of an earlier, and a NUL after.
fn environment(env: &[(OsString, OsString)]) -> Result<Vec<u16>, String> {
    let equals = u16::from(b'=');
    let mut sorted = BTreeMap::new();
    for (name, value) in env {
        let units: Vec<u16> = name.encode_wide().collect();
        // A name may start with `=` (cmd's `=C:`), and has none after.
        if units.is_empty() || units.contains(&0) || units[1..].contains(&equals) {
            return Err(format!("the environment has a variable that can't be named: {:?}", name));
        }
        let value: Vec<u16> = value.encode_wide().collect();
        if value.contains(&0) {
            return Err(format!("the variable {} has a NUL in it", name.to_string_lossy()));
        }
        sorted.insert(upper(&units), (units, value));
    }
    let mut block = Vec::new();
    for (name, value) in sorted.into_values() {
        block.extend(name);
        block.push(equals);
        block.extend(value);
        block.push(0);
    }
    if block.is_empty() {
        block.push(0);
    }
    block.push(0);
    Ok(block)
}

/// `units` with each in upper case where that is one unit, as Windows compares names.
fn upper(units: &[u16]) -> Vec<u16> {
    units
        .iter()
        .map(|&unit| {
            let Some(c) = char::from_u32(u32::from(unit)) else { return unit };
            let mut up = c.to_uppercase();
            match (up.next(), up.next()) {
                (Some(one), None) => u16::try_from(u32::from(one)).unwrap_or(unit),
                _ => unit,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(units: &[u16]) -> String {
        String::from_utf16(units).unwrap()
    }

    #[test]
    fn arguments_are_quoted_as_the_msvc_runtime_splits_them() {
        let args = ["", "plain", "a b", "tab\there", r#"say "hi""#, r"tail\", r"tail dir\", r#"a\\"b"#, r"\\host\share"]
            .map(OsString::from);
        let line = command_line(Path::new(r"C:\p q\a.exe"), &args).unwrap();
        assert_eq!(line.last(), Some(&0));
        let expected = [
            r#""C:\p q\a.exe""#,
            r#""""#,
            "plain",
            r#""a b""#,
            "\"tab\there\"",
            r#""say \"hi\"""#,
            r"tail\",
            r#""tail dir\\""#,
            r#"a\\\\\"b"#,
            r"\\host\share",
        ]
        .join(" ");
        assert_eq!(text(&line[..line.len() - 1]), expected);
        assert!(command_line(Path::new("a.exe"), &[OsString::from("a\0b")]).is_err());
        assert!(command_line(Path::new("a\".exe"), &[]).is_err());
        assert_eq!(text(&command_line(Path::new("a.exe"), &[]).unwrap()), "\"a.exe\"\0");
    }

    #[test]
    fn the_environment_block_is_sorted_without_regard_to_case_and_ends_in_two_nuls() {
        let env = [("b", "2"), ("Path", "x"), ("A", "1"), ("_z", ""), ("a", "3"), ("=C:", r"C:\w")]
            .map(|(name, value)| (OsString::from(name), OsString::from(value)));
        let block = environment(&env).unwrap();
        assert_eq!(text(&block), "=C:=C:\\w\0a=3\0b=2\0Path=x\0_z=\0\0");
        assert_eq!(environment(&[]).unwrap(), vec![0, 0]);
        assert!(environment(&[("A=B".into(), "c".into())]).is_err());
        assert!(environment(&[("".into(), "c".into())]).is_err());
        assert!(environment(&[("A".into(), "c\0d".into())]).is_err());
    }

    #[test]
    fn generic_rights_are_mapped_to_a_files() {
        assert_eq!(mapped(GENERIC_READ | GENERIC_EXECUTE), READ);
        assert_eq!(mapped(GENERIC_ALL), FILE_ALL_ACCESS);
        assert_eq!(mapped(FILE_GENERIC_READ), FILE_GENERIC_READ);
    }

    #[test]
    fn a_program_writes_where_it_is_granted_alone_and_ends_as_it_ended() {
        if let Err(why) = probe() {
            eprintln!("no sandbox can be made here, so this is not run: {why}");
            return;
        }
        let root = std::env::var_os("SystemRoot").map_or_else(|| PathBuf::from(r"C:\Windows"), PathBuf::from);
        let cmd = root.join("System32").join("cmd.exe");
        let base = std::env::temp_dir().join(format!("blaze-windows-sandbox-test-{}", std::process::id()));
        _ = std::fs::remove_dir_all(&base);
        let (written, denied) = (base.join("w"), base.join("d"));
        std::fs::create_dir_all(&denied).unwrap();
        let spec = |args: &[&OsStr]| Spec {
            program: cmd.clone(),
            args: args.iter().map(|it| it.to_os_string()).collect(),
            env: vec![("SystemRoot".into(), root.clone().into_os_string())],
            dir: base.clone(),
            read: vec![base.clone()],
            write: vec![written.clone()],
            network: false,
            trace: false,
            live: base.join("live"),
            report: base.join("report"),
            not_inputs: vec![cmd.clone()],
        };
        let (c, echo, hi) = (OsStr::new("/c"), OsStr::new("echo"), OsStr::new("hi>"));

        let inside = written.join("out.txt");
        assert_eq!(run(&spec(&[c, echo, hi, inside.as_os_str()])), Ok(0));
        assert_eq!(std::fs::read_to_string(&inside).unwrap().trim(), "hi");

        let outside = denied.join("out.txt");
        assert_ne!(run(&spec(&[c, echo, hi, outside.as_os_str()])), Ok(0));
        assert!(!outside.exists());

        let exit = spec(&[c, OsStr::new("exit"), OsStr::new("7")]);
        assert_eq!(run(&exit), Ok(7));
        assert_eq!(temporary(&exit), temporary(&exit.clone()));
        assert_ne!(temporary(&exit), temporary(&spec(&[c, OsStr::new("exit"), OsStr::new("8")])));
        assert!(!temporary(&exit).unwrap().exists(), "the run's temporary directory was left behind");
        _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_read_seen_granted_without_the_lock_is_one_granting_would_not_add() {
        if let Err(why) = probe() {
            eprintln!("no sandbox can be made here, so this is not run: {why}");
            return;
        }
        let base = std::env::temp_dir().join(format!("blaze-windows-sandbox-covered-{}", std::process::id()));
        _ = std::fs::remove_dir_all(&base);
        let (dir, file) = (base.join("d"), base.join("f.txt"));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&file, "x").unwrap();
        let made = make(false).unwrap();
        let readers = [made.container.0, sid(&made.any)];
        for (path, is_dir) in [(&dir, true), (&file, false)] {
            let before = covered(path, is_dir, &readers, READ, "test").unwrap();
            let added = {
                let _lock = Lock::take().unwrap();
                grant(path, is_dir, made.container.0, &readers, READ, "test").unwrap()
            };
            assert_eq!(added, !before, "{}", path.display());
            assert!(covered(path, is_dir, &readers, READ, "test").unwrap(), "{}", path.display());
            let _lock = Lock::take().unwrap();
            assert!(!grant(path, is_dir, made.container.0, &readers, READ, "test").unwrap(), "{}", path.display());
        }
        _ = std::fs::remove_dir_all(&base);
    }
}
