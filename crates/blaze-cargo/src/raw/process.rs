//! Processes: whether one runs, how a child ended, ending a worker's process group, the
//! action of `SIGPIPE`, the CPU time used, and on Windows the handles a child inherits. std
//! has none of these: each is the one system call that does it.

#[cfg(unix)]
use std::time::Duration;

/// Whether the process `pid` runs.
#[cfg(unix)]
pub fn alive(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else { return false };
    // SAFETY: `kill` takes no pointer, so no memory of this process is at stake whatever `pid`
    // is, and a signal of 0 is delivered to nothing: it only asks whether `pid` may be sent one.
    unsafe { libc::kill(pid, 0) == 0 }
}

/// How a child of this process ended.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Reaped {
    Code(i32),
    Signal(i32),
}

/// How `pid` ended, waited for, when it is a child of this process that ended; `None` when
/// it runs, or is no child of this one (whose end only its parent hears).
#[cfg(unix)]
pub fn reap(pid: u32) -> Option<Reaped> {
    let pid = libc::pid_t::try_from(pid).ok()?;
    let mut status = 0;
    // SAFETY: `status` is live for the call to write. `WNOHANG` returns at once; a `pid` that
    // is no child of this process is refused (`ECHILD`) and touches nothing. A child waited
    // for here is one no `std::process::Child` of this process waits for again: the one the
    // CLI starts the daemon as is dropped once spawned.
    if unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) } != pid {
        return None;
    }
    Some(match libc::WIFSIGNALED(status) {
        true => Reaped::Signal(libc::WTERMSIG(status)),
        false => Reaped::Code(libc::WEXITSTATUS(status)),
    })
}

/// Whether the process `pid` runs.
#[cfg(windows)]
pub fn alive(pid: u32) -> bool {
    use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _, OwnedHandle};
    use windows_sys::Win32::{
        Foundation::WAIT_TIMEOUT,
        System::Threading::{OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject},
    };
    // SAFETY: `OpenProcess` takes no pointer; any `pid` is either opened or refused with null.
    // The handle it opens is owned below, so it is closed once.
    let process = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
    if process.is_null() {
        return false;
    }
    // SAFETY: `process` is an open handle that nothing else owns: `OpenProcess` just made it.
    // Were it owned twice, it would be closed twice, the second time maybe another's handle.
    let process = unsafe { OwnedHandle::from_raw_handle(process) };
    // SAFETY: the handle is open for as long as `process` lives, which outlives the call; a
    // wait of no time only asks whether the process ended.
    unsafe { WaitForSingleObject(process.as_raw_handle(), 0) == WAIT_TIMEOUT }
}

#[cfg(not(any(unix, windows)))]
pub fn alive(_: u32) -> bool {
    false
}

/// Ends the process group `child` leads (a worker started in a group of its own, with the
/// processes it started), while `child` has not exited: one that has exited has nothing to
/// kill, and once it is waited for its pid may be another's.
#[cfg(unix)]
pub fn kill_group(child: &mut std::process::Child) {
    if let (Ok(None), Ok(group)) = (child.try_wait(), libc::pid_t::try_from(child.id())) {
        // SAFETY: `killpg` takes no pointer, so no memory of this process is at stake. That
        // the group is the worker's is upheld by `child` not having been waited for, checked
        // just above: until it is, its pid is not given to another process.
        unsafe { libc::killpg(group, libc::SIGKILL) };
    }
}

/// Has a write to a pipe whose reader is gone end the process quietly, as `SIGPIPE`'s
/// default action does, where std ignores the signal so that the write fails with `EPIPE`.
/// Not for a process that writes to sockets: a reset from the other end would end it.
#[cfg(unix)]
pub fn die_on_closed_pipe() {
    // SAFETY: `signal` with `SIG_DFL` installs no handler of this process's, so no code of it
    // ever runs in a signal's context, and it is one atomic change to the process, sound from
    // any thread. What it changes is how a later write to a closed pipe ends: the process ends,
    // which is no undefined behavior.
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
}

/// Whose CPU time [`cpu_time`] reads.
#[cfg(unix)]
#[derive(Clone, Copy)]
pub enum Whose {
    /// This process's own.
    Itself,
    /// That of its children which it waited for, and theirs.
    Waited,
}

/// The user and system CPU time `whose` had; None where the kernel says nothing of it.
#[cfg(unix)]
pub fn cpu_time(whose: Whose) -> Option<Duration> {
    let who = match whose {
        Whose::Itself => libc::RUSAGE_SELF,
        Whose::Waited => libc::RUSAGE_CHILDREN,
    };
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::zeroed();
    // SAFETY: `usage` is a live, writable `rusage`, and an `rusage` is all `getrusage` writes;
    // a smaller buffer would be written past its end.
    if unsafe { libc::getrusage(who, usage.as_mut_ptr()) } != 0 {
        return None;
    }
    // SAFETY: an `rusage` is integers alone, for which the zeros `usage` began as are a value,
    // and `getrusage` wrote another over them. A type with a niche (a reference, an enum) would
    // have no such value in its zeros, and reading it would be undefined.
    let usage = unsafe { usage.assume_init() };
    let time = |it: libc::timeval| {
        let micros = Duration::from_micros(u64::try_from(it.tv_usec).unwrap_or(0));
        Duration::from_secs(u64::try_from(it.tv_sec).unwrap_or(0)) + micros
    };
    Some(time(usage.ru_utime) + time(usage.ru_stime))
}

/// The ticks of the clock in a second, the unit of `/proc/<pid>/stat`'s times.
#[cfg(target_os = "linux")]
pub fn clock_ticks() -> Option<u64> {
    // SAFETY: `sysconf` takes no pointer and reads a constant of the system; a name it does
    // not know is answered with -1, which is refused below.
    let ticks = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    u64::try_from(ticks).ok().filter(|it| *it > 0)
}

/// The handles of this process that a child would inherit, made not inheritable until this
/// is dropped: [`withhold_inheritance`].
#[cfg(windows)]
pub struct Withheld(Vec<windows_sys::Win32::Foundation::HANDLE>);

/// Has a process started until the result is dropped inherit no handle of this one but those
/// it is given. A process is started with every handle of its parent that can be inherited,
/// and those of a pipe this command writes to are among them, under their own numbers and
/// under those the commands above this one had: whoever reads the pipe would wait for the
/// child to end.
#[cfg(windows)]
pub fn withhold_inheritance() -> Withheld {
    use windows_sys::Win32::Foundation::{
        GetHandleInformation, HANDLE, HANDLE_FLAG_INHERIT, SetHandleInformation,
    };

    /// Handles are multiples of four, and a command as short as this one has few.
    const HANDLES: std::ops::RangeInclusive<usize> = 4..=0x1_0000;

    let mut withheld = Vec::new();
    for handle in HANDLES.step_by(4).map(|it| it as HANDLE) {
        let mut flags = 0;
        // SAFETY: `GetHandleInformation` checks `handle` and fails on a number that is no open
        // handle, and `flags` is a `u32` it only writes; no Rust memory rests on the handle.
        let held = unsafe { GetHandleInformation(handle, &mut flags) } != 0
            && flags & HANDLE_FLAG_INHERIT != 0
            // SAFETY: `handle` was just found open and inheritable; clearing the flag touches no
            // memory, and `Withheld`'s drop sets it again. A number closed and reused meanwhile
            // only has that handle's inheritance changed.
            && unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0) } != 0;
        if held {
            withheld.push(handle);
        }
    }
    Withheld(withheld)
}

#[cfg(windows)]
impl Drop for Withheld {
    fn drop(&mut self) {
        use windows_sys::Win32::Foundation::{HANDLE_FLAG_INHERIT, SetHandleInformation};
        for &handle in &self.0 {
            // SAFETY: the handle was open and inheritable when it was withheld; one closed
            // since is refused, and one whose number another took has its inheritance set,
            // which is no memory of this process's.
            unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn this_process_is_alive() {
        if cfg!(any(unix, windows)) {
            assert!(alive(std::process::id()));
        }
    }

    #[cfg(unix)]
    #[test]
    fn this_process_used_some_cpu() {
        let mut spin = 0u64;
        for it in 0..1_000_000u64 {
            spin = std::hint::black_box(spin.wrapping_add(it));
        }
        assert!(cpu_time(Whose::Itself).is_some_and(|it| it > Duration::ZERO));
        assert!(cpu_time(Whose::Waited).is_some());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn the_clock_ticks() {
        assert!(clock_ticks().is_some());
    }
}
