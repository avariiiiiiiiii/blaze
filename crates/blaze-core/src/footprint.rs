//! The memory backstop of a spawned process on macOS (docs/v2/plan/lanes/core.md section 4, "Hard
//! backstop"): resource limits set in the child before its program runs, and one thread for
//! every watched process that reads its physical footprint, the number Activity Monitor and the
//! kernel's memory pressure go by, and kills it once it passes the limit.

use std::{
    io,
    sync::{
        Condvar, Mutex, MutexGuard, Once, PoisonError,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

/// How often the footprint is read: a process faulting pages in as fast as it can gets tens of
/// megabytes past its limit at most before it is killed.
const INTERVAL: Duration = Duration::from_millis(2);

/// In the child between fork and exec (system calls only, no allocation): resource limits
/// for `limit` bytes (RLIMIT_DATA and RLIMIT_AS as macOS honours them).
pub(crate) fn limit_self(limit: u64) -> io::Result<()> {
    for resource in [libc::RLIMIT_DATA, libc::RLIMIT_AS] {
        // SAFETY: all zeros is a valid `rlimit`; the call writes it.
        let mut now: libc::rlimit = unsafe { std::mem::zeroed() };
        // SAFETY: an `rlimit` to write.
        if unsafe { libc::getrlimit(resource, &mut now) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let most = limit.min(now.rlim_max);
        let wanted = libc::rlimit { rlim_cur: most, rlim_max: most };
        // SAFETY: an `rlimit` to read.
        if unsafe { libc::setrlimit(resource, &wanted) } == 0 {
            continue;
        }
        let error = io::Error::last_os_error();
        // macOS refuses a limit below the address space the process has reserved already,
        // which is hundreds of gigabytes of unbacked ranges in every process: that limit is
        // not one macOS holds, and the footprint watch holds it instead.
        if error.raw_os_error() != Some(libc::EINVAL) {
            return Err(error);
        }
    }
    Ok(())
}

/// One watched process.
struct Entry {
    id: u64,
    pid: libc::pid_t,
    limit: u64,
    /// The most footprint read so far.
    seen: u64,
    /// Killed or gone: read no more.
    done: bool,
}

/// The one thread that reads every watched process's footprint, and what it reads. A kill is
/// made with the lock held and the entry in the list, so once [`Watch::stop`] has taken the entry
/// out, nothing signals its pid.
struct Sampler {
    entries: Mutex<Vec<Entry>>,
    wake: Condvar,
}

static SAMPLER: Sampler = Sampler { entries: Mutex::new(Vec::new()), wake: Condvar::new() };
static STARTED: Once = Once::new();
static NEXT: AtomicU64 = AtomicU64::new(0);

fn entries() -> MutexGuard<'static, Vec<Entry>> {
    SAMPLER.entries.lock().unwrap_or_else(PoisonError::into_inner)
}

pub(crate) struct Watch {
    id: u64,
    pid: libc::pid_t,
}

impl Watch {
    /// Watches `pid`'s physical footprint (proc_pid_rusage RUSAGE_INFO_V4 ri_phys_footprint)
    /// on the one footprint thread, every few milliseconds, and SIGKILLs it once it passes
    /// `limit`.
    pub(crate) fn start(pid: libc::pid_t, limit: u64) -> Watch {
        STARTED.call_once(|| {
            std::thread::Builder::new()
                .name("blaze-footprint".into())
                .spawn(sample)
                .expect("starting the footprint watch");
        });
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        entries().push(Entry { id, pid, limit, seen: 0, done: false });
        SAMPLER.wake.notify_one();
        Watch { id, pid }
    }

    /// Called once the process exited and before it is reaped: stops the thread and says
    /// the most it saw (ri_lifetime_max_phys_footprint where it could read it).
    pub(crate) fn stop(self) -> u64 {
        let seen = self.remove();
        match footprint(self.pid) {
            Ok(usage) => seen.max(usage.now).max(usage.most),
            Err(_) => seen,
        }
    }

    /// Out of the sampler's list: nothing signals the pid once this returns. The most seen.
    fn remove(&self) -> u64 {
        let mut entries = entries();
        match entries.iter().position(|it| it.id == self.id) {
            Some(at) => entries.swap_remove(at).seen,
            None => 0,
        }
    }
}

impl Drop for Watch {
    fn drop(&mut self) {
        self.remove();
    }
}

fn sample() {
    let mut entries = entries();
    loop {
        if entries.iter().all(|it| it.done) {
            entries = SAMPLER.wake.wait(entries).unwrap_or_else(PoisonError::into_inner);
            continue;
        }
        for entry in entries.iter_mut().filter(|it| !it.done) {
            check(entry);
        }
        entries = SAMPLER
            .wake
            .wait_timeout(entries, INTERVAL)
            .unwrap_or_else(PoisonError::into_inner)
            .0;
    }
}

fn check(entry: &mut Entry) {
    match footprint(entry.pid) {
        Ok(usage) => {
            entry.seen = entry.seen.max(usage.now).max(usage.most);
            if usage.now > entry.limit {
                // SAFETY: no pointers; the pid is in the list, so not yet reaped.
                unsafe { libc::kill(entry.pid, libc::SIGKILL) };
                entry.done = true;
            }
        }
        // Gone: there is nothing left to hold.
        Err(error) if error.raw_os_error() == Some(libc::ESRCH) => entry.done = true,
        // Its footprint cannot be read, so its limit cannot be held: it does not run on.
        Err(_) => {
            // SAFETY: as above.
            unsafe { libc::kill(entry.pid, libc::SIGKILL) };
            entry.done = true;
        }
    }
}

struct Usage {
    now: u64,
    most: u64,
}

fn footprint(pid: libc::pid_t) -> io::Result<Usage> {
    // SAFETY: all zeros is a valid `rusage_info_v4`; the call writes it.
    let mut info: libc::rusage_info_v4 = unsafe { std::mem::zeroed() };
    // SAFETY: a `rusage_info_v4`, the size the flavor names, to write.
    let read = unsafe {
        libc::proc_pid_rusage(pid, libc::RUSAGE_INFO_V4, (&raw mut info).cast::<libc::rusage_info_t>())
    };
    if read != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(Usage { now: info.ri_phys_footprint, most: info.ri_lifetime_max_phys_footprint })
}

#[cfg(test)]
mod tests {
    use std::os::unix::process::CommandExt as _;

    use super::*;

    fn spawn(script: &str, limit: Option<u64>) -> std::process::Child {
        let mut command = std::process::Command::new("/bin/sh");
        command.args(["-c", script]).env_clear().stdout(std::process::Stdio::piped());
        if let Some(limit) = limit {
            // SAFETY: `limit_self` makes system calls alone.
            unsafe { command.pre_exec(move || limit_self(limit)) };
        }
        command.spawn().unwrap()
    }

    /// Waits for it to exit, stops the watch, then reaps it, as a driver does.
    fn end(child: std::process::Child, watch: Watch) -> (libc::c_int, u64) {
        let pid = child.id() as libc::pid_t;
        drop(child);
        crate::process::wait_exited(pid).unwrap();
        let peak = watch.stop();
        let (status, _) = crate::process::reap(pid).unwrap();
        (status, peak)
    }

    #[test]
    fn a_process_past_its_limit_is_killed() {
        let limit = 32 << 20;
        // Doubles a string until it is a gigabyte, unless it is stopped first.
        let child = spawn(
            r#"x=a; i=0; while [ $i -lt 30 ]; do x="$x$x"; i=$((i+1)); done; exit 0"#,
            None,
        );
        let watch = Watch::start(child.id() as libc::pid_t, limit);
        let (status, peak) = end(child, watch);
        assert!(libc::WIFSIGNALED(status), "it ran to its end: {status:#x}");
        assert_eq!(libc::WTERMSIG(status), libc::SIGKILL);
        assert!(peak > limit, "{peak}");
    }

    #[test]
    fn a_process_within_its_limit_runs_to_its_end() {
        let child = spawn("exit 3", None);
        let watch = Watch::start(child.id() as libc::pid_t, 1 << 30);
        let (status, peak) = end(child, watch);
        assert!(!libc::WIFSIGNALED(status));
        assert_eq!(libc::WEXITSTATUS(status), 3);
        assert!(peak > 0 && peak < 1 << 30, "{peak}");
    }

    /// The one thread holds each process to its own limit.
    #[test]
    fn watches_at_once_keep_their_own_limits() {
        let grows = spawn(
            r#"x=a; i=0; while [ $i -lt 30 ]; do x="$x$x"; i=$((i+1)); done; exit 0"#,
            None,
        );
        let small = spawn("sleep 0.3; exit 0", None);
        let small_watch = Watch::start(small.id() as libc::pid_t, 1 << 30);
        let grows_watch = Watch::start(grows.id() as libc::pid_t, 32 << 20);
        let (grew, _) = end(grows, grows_watch);
        let (stayed, _) = end(small, small_watch);
        assert!(libc::WIFSIGNALED(grew) && libc::WTERMSIG(grew) == libc::SIGKILL, "{grew:#x}");
        assert!(!libc::WIFSIGNALED(stayed) && libc::WEXITSTATUS(stayed) == 0, "{stayed:#x}");
    }

    /// The resource limits leave a program that fits them as it is.
    #[test]
    fn limits_set_before_exec_let_a_small_program_run() {
        let child = spawn("echo hi", Some(1 << 30));
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success(), "{output:?}");
        assert_eq!(output.stdout, b"hi\n");
    }
}
