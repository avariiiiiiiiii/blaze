//! The memory backstop of a spawned process on Linux (docs/v2/plan/lanes/core.md section 4, "Hard
//! backstop"): a cgroup v2 of its own, with `memory.max` at its limit and no swap, which the
//! child joins before its program runs, so the kernel holds it (and whatever it starts) to the
//! limit whatever admission estimated.
//!
//! The cgroups are made under the daemon's own cgroup, which must be delegated to it (systemd's
//! `Delegate=yes`, or a container's cgroup namespace). A cgroup with processes in it cannot
//! enable controllers for its children (the "no internal processes" rule), so the daemon first
//! moves itself into a leaf `daemon` of its cgroup, once, and makes each process's cgroup a
//! sibling of that leaf.

use std::{
    ffi::{CString, OsStr},
    fs::OpenOptions,
    io::{self, Write as _},
    os::{
        fd::AsRawFd as _,
        unix::{ffi::OsStrExt as _, fs::FileExt as _},
    },
    path::{Path, PathBuf},
    sync::{
        Mutex, PoisonError,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

static MADE: AtomicU64 = AtomicU64::new(0);

/// Whether a cgroup made here was seen to have `memory.max` and `memory.peak`.
static KERNEL_CHECKED: AtomicBool = AtomicBool::new(false);

/// The cgroup each process's cgroup is made in, once readied.
static PARENT: Mutex<Option<PathBuf>> = Mutex::new(None);

/// How long a cgroup's end waits for what it killed to be gone.
const EMPTYING: Duration = Duration::from_secs(10);

pub(crate) struct Cgroup {
    dir: PathBuf,
    procs: CString,
}

impl Cgroup {
    /// A cgroup v2 of its own, under the daemon's own cgroup (read from /proc/self/cgroup,
    /// under the cgroup2 mount found in /proc/self/mountinfo), with memory.max = limit and
    /// memory.swap.max = 0. An error naming why where none can be made (no cgroup2, the
    /// daemon's cgroup not delegated, the memory controller not enabled for children): the
    /// spawn then fails, never runs without its limit.
    pub(crate) fn new(limit: u64) -> io::Result<Cgroup> {
        let parent = parent()?;
        let n = MADE.fetch_add(1, Ordering::Relaxed);
        let dir = parent.join(format!("blaze-{}-{n}", std::process::id()));
        let procs = CString::new(dir.join("cgroup.procs").as_os_str().as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "a cgroup path with a NUL"))?;
        if let Err(error) = std::fs::create_dir(&dir) {
            if error.kind() != io::ErrorKind::AlreadyExists {
                return Err(named(
                    error,
                    format!("making {} (is {} delegated to this process?)", dir.display(), parent.display()),
                ));
            }
            // Left by a process of this pid that is gone; it holds nothing once it is empty.
            std::fs::remove_dir(&dir)
                .and_then(|()| std::fs::create_dir(&dir))
                .map_err(|error| named(error, format!("making {} again", dir.display())))?;
        }
        let cgroup = Cgroup { dir, procs };
        cgroup.hold(limit)?;
        Ok(cgroup)
    }

    /// Its cgroup.procs, which the child writes "0" to before it execs.
    pub(crate) fn procs(&self) -> &std::ffi::CStr {
        &self.procs
    }

    /// The most it held: memory.peak (Linux 5.19+); an error naming the kernel if missing.
    pub(crate) fn peak(&self) -> io::Result<u64> {
        let path = self.dir.join("memory.peak");
        match std::fs::read_to_string(&path) {
            Ok(text) => text.trim().parse().map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("{}: {text:?} is not a number of bytes", path.display()),
                )
            }),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!(
                    "{} is missing: Linux 5.19 or later keeps it, and this is Linux {}",
                    path.display(),
                    kernel()
                ),
            )),
            Err(error) => Err(named(error, format!("reading {}", path.display()))),
        }
    }

    fn hold(&self, limit: u64) -> io::Result<()> {
        let max = self.dir.join("memory.max");
        let not_enabled = || {
            io::Error::new(
                io::ErrorKind::Unsupported,
                format!(
                    "{} has no memory.max: the memory controller is not enabled for it",
                    self.dir.display()
                ),
            )
        };
        // The kernel and the parent's controllers are the same for every cgroup made, so the
        // first one's files answer for the rest, which spares each spawn two `stat`s.
        if !KERNEL_CHECKED.load(Ordering::Relaxed) {
            if !max.exists() {
                return Err(not_enabled());
            }
            // Asked of it once the process has exited; refused now rather than after the run.
            // A kernel that keeps it (5.19) also has `cgroup.kill` (5.14), which its end needs.
            if !self.dir.join("memory.peak").exists() {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    format!(
                        "{} has no memory.peak: Linux 5.19 or later keeps it, and this is Linux {}",
                        self.dir.display(),
                        kernel()
                    ),
                ));
            }
            KERNEL_CHECKED.store(true, Ordering::Relaxed);
        }
        write(&max, &limit.to_string()).map_err(|error| match error.kind() {
            io::ErrorKind::NotFound => not_enabled(),
            _ => error,
        })?;
        let swap = self.dir.join("memory.swap.max");
        match write(&swap, "0") {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => match swap_in_use()? {
                false => Ok(()),
                true => Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    format!(
                        "{} is missing (swap accounting is off) while swap is on: the process \
                         could pass its limit by swapping",
                        swap.display()
                    ),
                )),
            },
            Err(error) => Err(error),
        }
    }
}

impl Drop for Cgroup {
    /// Kills what is left in it, waits until it is empty, and removes it.
    fn drop(&mut self) {
        _ = write(&self.dir.join("cgroup.kill"), "1");
        let deadline = Instant::now() + EMPTYING;
        wait_empty(&self.dir.join("cgroup.events"), deadline);
        loop {
            match std::fs::remove_dir(&self.dir) {
                Ok(()) => return,
                Err(error) if error.kind() == io::ErrorKind::NotFound => return,
                // Empty, but the kernel has not let go of it yet.
                Err(_) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(1)),
                Err(_) => return,
            }
        }
    }
}

/// Waits until `events` says `populated 0`, or until `deadline`.
fn wait_empty(events: &Path, deadline: Instant) {
    let Ok(file) = std::fs::File::open(events) else { return };
    let mut bytes = [0u8; 256];
    loop {
        // A read also arms the poll below: kernfs wakes a poller of a file changed since it read.
        let Ok(n) = file.read_at(&mut bytes, 0) else { return };
        if !populated(&String::from_utf8_lossy(&bytes[..n])) {
            return;
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return;
        }
        let mut fds = [libc::pollfd { fd: file.as_raw_fd(), events: libc::POLLPRI, revents: 0 }];
        let ms = left.as_millis().clamp(1, 100) as libc::c_int;
        // SAFETY: one pollfd of an open descriptor.
        unsafe { libc::poll(fds.as_mut_ptr(), 1, ms) };
    }
}

fn populated(events: &str) -> bool {
    events.lines().any(|line| line.trim() == "populated 1")
}

/// The cgroup to make each process's cgroup in: this process's own, readied the first time.
fn parent() -> io::Result<PathBuf> {
    let mut parent = PARENT.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(it) = &*parent {
        return Ok(it.clone());
    }
    let cgroups = read(Path::new("/proc/self/cgroup"))?;
    let mountinfo = read(Path::new("/proc/self/mountinfo"))?;
    let own = own_dir(&cgroups, &mountinfo)
        .map_err(|why| io::Error::new(io::ErrorKind::Unsupported, why))?;
    ready(&own)?;
    *parent = Some(own.clone());
    Ok(own)
}

/// Readies `own`, this process's cgroup, to hold a cgroup per process with the memory
/// controller: the controller must be delegated to it, and once it is enabled for its
/// children no process may be in it, so this one moves into the leaf `daemon` first.
fn ready(own: &Path) -> io::Result<()> {
    let controllers = read(&own.join("cgroup.controllers"))?;
    if !listed(&controllers, "memory") {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!(
                "the memory controller is not delegated to {} (its parent's \
                 cgroup.subtree_control has no +memory; it has {:?})",
                own.display(),
                controllers.trim()
            ),
        ));
    }
    let subtree = own.join("cgroup.subtree_control");
    if listed(&read(&subtree)?, "memory") {
        return Ok(());
    }
    if !read(&own.join("cgroup.procs"))?.trim().is_empty() {
        let leaf = own.join("daemon");
        match std::fs::create_dir(&leaf) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(named(
                    error,
                    format!(
                        "making {} to move this process into (is {} delegated to it?)",
                        leaf.display(),
                        own.display()
                    ),
                ));
            }
        }
        write(&leaf.join("cgroup.procs"), &std::process::id().to_string())?;
    }
    write(&subtree, "+memory").map_err(|error| match error.raw_os_error() {
        Some(libc::EBUSY) => {
            let others = read(&own.join("cgroup.procs")).unwrap_or_default();
            io::Error::new(
                io::ErrorKind::ResourceBusy,
                format!(
                    "{} still has processes of others in it ({}), so it cannot enable memory \
                     for its children: {error}",
                    own.display(),
                    others.split_whitespace().collect::<Vec<_>>().join(", ")
                ),
            )
        }
        _ => error,
    })
}

/// The directory of this process's cgroup v2, from /proc/self/cgroup and /proc/self/mountinfo.
fn own_dir(cgroups: &str, mountinfo: &str) -> Result<PathBuf, String> {
    let path = unified_path(cgroups).ok_or(
        "this process is in no cgroup v2 (/proc/self/cgroup has no `0::` line): only cgroup v1 \
         is mounted here",
    )?;
    let mounts = cgroup2_mounts(mountinfo);
    if mounts.is_empty() {
        return Err("no cgroup2 file system is mounted (none in /proc/self/mountinfo)".into());
    }
    for (root, at) in mounts {
        if let Some(rel) = relative(path, &root) {
            return Ok(if rel.is_empty() { at } else { at.join(rel) });
        }
    }
    Err(format!("no cgroup2 mount in /proc/self/mountinfo shows this process's cgroup {path}"))
}

/// The cgroup of the unified hierarchy in /proc/self/cgroup.
fn unified_path(cgroups: &str) -> Option<&str> {
    let path = cgroups.lines().find_map(|line| line.strip_prefix("0::"))?;
    Some(path.strip_suffix(" (deleted)").unwrap_or(path))
}

/// Each cgroup2 mount in mountinfo: the cgroup it shows (its root) and where it is.
fn cgroup2_mounts(mountinfo: &str) -> Vec<(String, PathBuf)> {
    mountinfo
        .lines()
        .filter_map(|line| {
            let fields: Vec<&str> = line.split(' ').collect();
            // The optional fields end with a lone "-", then the file system's type.
            let dash = 6 + fields.get(6..)?.iter().position(|it| *it == "-")?;
            if *fields.get(dash + 1)? != "cgroup2" {
                return None;
            }
            let root = String::from_utf8_lossy(&unescape(fields[3])).into_owned();
            let at = PathBuf::from(OsStr::from_bytes(&unescape(fields[4])));
            Some((root, at))
        })
        .collect()
}

/// `path`, a cgroup, relative to `root`, the cgroup a mount shows; `None` when it is not under
/// it.
fn relative<'a>(path: &'a str, root: &str) -> Option<&'a str> {
    let root = root.trim_end_matches('/');
    let rest = path.strip_prefix(root)?;
    (rest.is_empty() || rest.starts_with('/')).then(|| rest.trim_start_matches('/'))
}

/// A field of mountinfo with its octal escapes (`\040` for a space) undone.
fn unescape(field: &str) -> Vec<u8> {
    let bytes = field.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        let octal = bytes.get(at + 1..at + 4).filter(|it| it.iter().all(|b| (b'0'..=b'7').contains(b)));
        match (bytes[at], octal) {
            (b'\\', Some(digits)) => {
                out.push(digits.iter().fold(0u8, |n, d| n.wrapping_mul(8).wrapping_add(d - b'0')));
                at += 4;
            }
            (byte, _) => {
                out.push(byte);
                at += 1;
            }
        }
    }
    out
}

fn listed(list: &str, name: &str) -> bool {
    list.split_whitespace().any(|it| it == name)
}

/// Whether any swap is on (more than the header line of /proc/swaps).
fn swap_in_use() -> io::Result<bool> {
    Ok(read(Path::new("/proc/swaps"))?.lines().skip(1).any(|line| !line.trim().is_empty()))
}

/// The kernel's release, as `uname -r` says it.
pub(crate) fn kernel() -> String {
    // SAFETY: all zeros is a valid `utsname`; the call fills it.
    let mut name: libc::utsname = unsafe { std::mem::zeroed() };
    // SAFETY: a `utsname` to write.
    if unsafe { libc::uname(&mut name) } != 0 {
        return "of an unknown release".into();
    }
    let release: Vec<u8> =
        name.release.iter().take_while(|&&it| it != 0).map(|&it| it as u8).collect();
    String::from_utf8_lossy(&release).into_owned()
}

fn read(path: &Path) -> io::Result<String> {
    std::fs::read_to_string(path).map_err(|error| named(error, format!("reading {}", path.display())))
}

/// `value` written to a file of the cgroup file system, which is never made: a file that is
/// not there is `NotFound`.
fn write(path: &Path, value: &str) -> io::Result<()> {
    OpenOptions::new()
        .write(true)
        .open(path)
        .and_then(|mut file| file.write_all(value.as_bytes()))
        .map_err(|error| named(error, format!("writing {value:?} to {}", path.display())))
}

fn named(error: io::Error, what: String) -> io::Error {
    io::Error::new(error.kind(), format!("{what}: {error}"))
}

#[cfg(test)]
mod tests {
    use std::io::Read as _;

    use super::*;
    use crate::{Command, Sandbox};

    const MOUNTINFO: &str = "\
22 28 0:21 / /proc rw,nosuid,nodev,noexec,relatime shared:12 - proc proc rw
23 28 0:22 / /sys rw,nosuid,nodev,noexec,relatime shared:2 - sysfs sysfs rw
26 23 0:25 / /sys/fs/cgroup rw,nosuid,nodev,noexec,relatime shared:4 - cgroup2 cgroup2 rw,nsdelegate,memory_recursiveprot
28 1 259:2 / / rw,relatime shared:1 - ext4 /dev/nvme0n1p2 rw
";

    #[test]
    fn the_unified_cgroup_is_the_zero_line() {
        let systemd = "0::/user.slice/user-1000.slice/user@1000.service/app.slice/blaze.service\n";
        assert_eq!(
            unified_path(systemd),
            Some("/user.slice/user-1000.slice/user@1000.service/app.slice/blaze.service")
        );
        let hybrid = "12:memory:/user.slice\n1:name=systemd:/user.slice/s.scope\n0::/user.slice/s.scope\n";
        assert_eq!(unified_path(hybrid), Some("/user.slice/s.scope"));
        assert_eq!(unified_path("0::/gone (deleted)\n"), Some("/gone"));
        assert_eq!(unified_path("12:memory:/user.slice\n1:name=systemd:/x\n"), None);
        assert_eq!(unified_path("0::/\n"), Some("/"));
    }

    #[test]
    fn the_cgroup2_mount_is_found_past_optional_fields() {
        assert_eq!(
            cgroup2_mounts(MOUNTINFO),
            vec![("/".to_owned(), PathBuf::from("/sys/fs/cgroup"))]
        );
        let none = "28 1 259:2 / / rw,relatime shared:1 master:3 - ext4 /dev/root rw\n";
        assert!(cgroup2_mounts(none).is_empty());
        let escaped = "40 23 0:40 /a\\040b /mnt/my\\040cgroups rw - cgroup2 none rw\n";
        assert_eq!(
            cgroup2_mounts(escaped),
            vec![("/a b".to_owned(), PathBuf::from("/mnt/my cgroups"))]
        );
    }

    #[test]
    fn the_own_cgroup_is_under_its_mount() {
        assert_eq!(
            own_dir("0::/system.slice/blaze.service\n", MOUNTINFO),
            Ok(PathBuf::from("/sys/fs/cgroup/system.slice/blaze.service"))
        );
        assert_eq!(own_dir("0::/\n", MOUNTINFO), Ok(PathBuf::from("/sys/fs/cgroup")));
        // A mount that shows only part of the tree.
        let partial = "40 23 0:40 /system.slice /mnt/cg rw - cgroup2 none rw\n";
        assert_eq!(
            own_dir("0::/system.slice/blaze.service\n", partial),
            Ok(PathBuf::from("/mnt/cg/blaze.service"))
        );
        assert!(own_dir("0::/user.slice/x\n", partial).unwrap_err().contains("/user.slice/x"));
        assert!(own_dir("0::/system.slicex/y\n", partial).is_err());
        assert!(own_dir("1:name=systemd:/x\n", MOUNTINFO).unwrap_err().contains("cgroup v1"));
        let no_cgroup2 = "28 1 259:2 / / rw - ext4 /dev/root rw\n";
        assert!(own_dir("0::/x\n", no_cgroup2).unwrap_err().contains("no cgroup2"));
    }

    #[test]
    fn events_say_when_it_is_empty() {
        assert!(populated("populated 1\nfrozen 0\n"));
        assert!(!populated("populated 0\nfrozen 0\n"));
    }

    fn sh(script: &str, memory: u64) -> Box<Command> {
        Box::new(Command {
            program: "/bin/sh".into(),
            args: vec!["-c".into(), script.into()],
            env: vec![("PATH".into(), "/usr/bin:/bin".into())],
            dir: "/".into(),
            stdin: Vec::new(),
            sandbox: Sandbox::Off,
            memory: Some(memory),
        })
    }

    fn run(command: Box<Command>) -> crate::Exit {
        let started = crate::process::start(Path::new("/"), command, None).unwrap();
        let mut stdout = Vec::new();
        std::fs::File::from(started.stdout).read_to_end(&mut stdout).unwrap();
        let mut stderr = Vec::new();
        std::fs::File::from(started.stderr).read_to_end(&mut stderr).unwrap();
        started.finish.exited(started.pid, stdout, stderr).unwrap()
    }

    /// Where a delegated cgroup is writable: a cgroup is made with its limit, measured and
    /// removed, and a process in one is held to its limit.
    #[test]
    fn a_process_is_held_to_its_limit() {
        let cgroup = match Cgroup::new(64 << 20) {
            Ok(it) => it,
            Err(why) => {
                eprintln!("skipped: no cgroup can be made here: {why}");
                return;
            }
        };
        let dir = cgroup.dir.clone();
        assert_eq!(std::fs::read_to_string(dir.join("memory.max")).unwrap().trim(), "67108864");
        assert!(cgroup.procs().to_bytes().ends_with(b"/cgroup.procs"));
        match cgroup.peak() {
            Ok(peak) => assert!(peak < 64 << 20, "{peak}"),
            Err(why) => {
                eprintln!("skipped: {why}");
                return;
            }
        }
        drop(cgroup);
        assert!(!dir.exists(), "{} is left", dir.display());

        let within = run(sh("head -c 1000000 /dev/zero | wc -c", 64 << 20));
        assert_eq!((within.code, within.signal), (Some(0), None), "{within:?}");
        assert_eq!(String::from_utf8_lossy(&within.stdout).trim(), "1000000");
        assert!(within.peak_memory > 0);

        let past = run(sh("x=$(head -c 200000000 /dev/zero | tr '\\0' a); echo ${#x}", 32 << 20));
        assert_eq!(past.signal, Some(libc::SIGKILL), "{past:?}");
        assert!(past.stdout.is_empty(), "{past:?}");
    }
}
