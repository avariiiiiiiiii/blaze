//! Where the program's threads are, a few hundred times a second, while the daemon asks.
//!
//! The daemon writes `sample` in the program's directory to ask and removes it to stop. Until
//! then the sampler looks for that file four times a second and does nothing else. While it
//! samples it reads where each running thread is, and writes what it counted to `profile` a
//! few times a second. `sample` names the worker that reads `profile`; once that process is
//! gone the sampler stops as if the file were, and starts again when a worker asks anew.
//!
//! The kernel stops a thread for as long as reading its registers takes and lets it go itself
//! (`thread_get_state` holds a thread other than the caller while it reads), so the sampler
//! never suspends one and none can be left stopped. Stopping a thread that runs on another
//! core is most of what a look costs, about ten microseconds, so each look is at one thread:
//! the busy ones in turn, where busy is having taken a twentieth of a core since the threads
//! were last listed. Each busy thread is looked at as often as the others, so the samples
//! split between them as their time does, and the sampler costs the same however many
//! threads the program runs.

#![allow(unsafe_code)]

use std::{
    collections::HashMap,
    fs::File,
    hash::{BuildHasherDefault, Hasher},
    io::Read as _,
    ops::Range,
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime},
};

use crate::format::{self, Profile, Sample};

unsafe extern "C" {
    fn mach_task_self() -> u32;
    fn mach_thread_self() -> u32;
    fn task_threads(task: u32, threads: *mut *mut u32, count: *mut u32) -> i32;
    fn thread_info(thread: u32, flavor: i32, info: *mut i32, count: *mut u32) -> i32;
    fn thread_get_state(thread: u32, flavor: i32, state: *mut u32, count: *mut u32) -> i32;
    fn mach_port_deallocate(task: u32, name: u32) -> i32;
    fn vm_deallocate(task: u32, address: usize, size: usize) -> i32;
    fn mach_absolute_time() -> u64;
    fn mach_wait_until(deadline: u64) -> i32;
    fn mach_timebase_info(info: *mut Timebase) -> i32;
    fn kill(pid: i32, signal: i32) -> i32;
}

const THREAD_BASIC_INFO: i32 = 3;
const TH_STATE_RUNNING: i32 = 1;
const ARM_THREAD_STATE64: i32 = 6;
/// What a pointer has of an address, without the bits pointer authentication signs it with.
const ADDRESS: u64 = (1 << 47) - 1;

/// How often the sampler looks for `sample` while it is not sampling.
const IDLE: Duration = Duration::from_millis(250);
/// How often the threads of the program are listed again, for those started since and to
/// see which are busy.
const LIST_EVERY: Duration = Duration::from_millis(200);
/// The CPU time a thread takes between two lists to be busy: a twentieth of a core.
const BUSY_NS: u64 = LIST_EVERY.as_nanos() as u64 / 20;
/// How often `profile` is written, and `sample` read again.
const WRITE_EVERY: Duration = Duration::from_millis(250);
/// The least time between two looks at the threads, whatever the daemon asks.
const LEAST_INTERVAL_US: u64 = 250;
/// How many distinct pcs the count of each has room for before it grows.
const PCS: usize = 1 << 12;

/// FxHash's multiply over a pc: pcs are the program's own addresses, not an attacker's keys,
/// so one multiply and a rotate, never SipHash, per sample.
#[derive(Default)]
struct PcHasher(u64);

impl Hasher for PcHasher {
    fn finish(&self) -> u64 {
        self.0.rotate_left(26)
    }

    fn write(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.write_u64(u64::from(byte));
        }
    }

    fn write_u64(&mut self, value: u64) {
        self.0 = (self.0 ^ value).wrapping_mul(0xf135_7aea_2e62_a9c5);
    }
}

/// How often a thread was found at each pc, before the slide.
type Pcs = HashMap<u64, u64, BuildHasherDefault<PcHasher>>;

#[repr(C)]
#[derive(Default)]
struct Time {
    seconds: i32,
    microseconds: i32,
}

#[repr(C)]
#[derive(Default)]
struct ThreadBasicInfo {
    user_time: Time,
    system_time: Time,
    cpu_usage: i32,
    policy: i32,
    run_state: i32,
    flags: i32,
    suspend_count: i32,
    sleep_time: i32,
}

#[repr(C)]
#[derive(Default)]
struct ThreadState {
    x: [u64; 29],
    fp: u64,
    lr: u64,
    sp: u64,
    pc: u64,
    cpsr: u32,
    flags: u32,
}

#[repr(C)]
#[derive(Default)]
struct Timebase {
    numer: u32,
    denom: u32,
}

/// Where the program's own code is, before the slide: its image and the room for patches.
pub(crate) struct Code {
    pub(crate) slide: u64,
    pub(crate) image: Range<u64>,
    pub(crate) patches: Range<u64>,
}

impl Code {
    fn has(&self, pc: u64) -> bool {
        self.image.contains(&pc) || self.patches.contains(&pc)
    }
}

/// Starts the thread that samples when the daemon asks, unless `BLAZE_TIER=off`.
pub(crate) fn start(dir: PathBuf, code: Code) {
    if crate::setting("TIER").is_some_and(|it| it == "off") {
        return;
    }
    std::thread::Builder::new()
        // alloc: setup, the thread's name once at start
        .name("blaze-live-sample".to_owned())
        .spawn(move || {
            let mut files = Files::new(&dir);
            loop {
                match files.sample() {
                    Some(sample) => run(&mut files, &code, sample),
                    None => std::thread::sleep(IDLE),
                }
            }
        })
        .expect("spawning the thread that samples");
}

/// The sampler's files in the program's directory, and the text it reads and writes them
/// through, kept from one look to the next: no path is joined and no text allocated per look.
struct Files {
    sample: PathBuf,
    profile: PathBuf,
    /// Where `profile` is written before it is renamed over the last.
    partial: PathBuf,
    text: String,
}

impl Files {
    fn new(dir: &Path) -> Files {
        Files {
            sample: dir.join(format::SAMPLE),
            profile: dir.join(format::PROFILE),
            // alloc: setup, once when the sampler starts
            partial: dir.join(format!("{}.{}", format::PROFILE, std::process::id())),
            text: String::with_capacity(PCS * 24),
        }
    }

    /// What `sample` asks, unless the process it names to read what is sampled is gone: a
    /// worker that exited, or was killed, does not remove the file.
    fn sample(&mut self) -> Option<Sample> {
        self.text.clear();
        File::open(&self.sample).ok()?.read_to_string(&mut self.text).ok()?;
        let sample = Sample::decode(&self.text)?;
        (sample.reader == 0 || alive(sample.reader)).then_some(sample)
    }

    /// Writes `profile` in one write, then renames it over the last.
    #[allow(clippy::print_stderr, reason = "a profile the sampler can't write is said on stderr")]
    fn write(&mut self, profile: &Profile) {
        self.text.clear();
        profile.encode_into(&mut self.text);
        let written = std::fs::write(&self.partial, &self.text)
            .and_then(|()| std::fs::rename(&self.partial, &self.profile));
        if let Err(why) = written {
            eprintln!("blaze: {}: {why}", self.profile.display());
        }
    }
}

/// Whether there is a process `pid`. One that is there and not this user's is.
fn alive(pid: u32) -> bool {
    const ESRCH: i32 = 3;
    let Ok(pid) = i32::try_from(pid) else { return false };
    // SAFETY: signal 0 only checks that the process can be signalled.
    unsafe { kill(pid, 0) == 0 || std::io::Error::last_os_error().raw_os_error() != Some(ESRCH) }
}

/// Samples until `sample` is removed, counting from nothing. Once the pcs the threads are
/// found at and the threads have been seen, a look and a write of `profile` allocate nothing.
fn run(files: &mut Files, code: &Code, mut sample: Sample) {
    // SAFETY: the port of this thread, given back at the end.
    let me = unsafe { mach_thread_self() };
    let spent_before = cpu_ns(me);
    let mut timebase = Timebase::default();
    // SAFETY: `timebase` is what the call fills in.
    unsafe { mach_timebase_info(&mut timebase) };
    let ticks_of = |us: u64| us * 1000 * u64::from(timebase.denom) / u64::from(timebase.numer);
    let mut profile = Profile { pid: std::process::id(), started: now_ms(), ..Profile::default() };
    let mut pcs = Pcs::with_capacity_and_hasher(PCS, BuildHasherDefault::default());
    profile.pcs.reserve(PCS);
    let mut threads = Threads::default();
    let mut listed = Instant::now() - LIST_EVERY;
    let mut written = Instant::now();
    let mut turn = 0;
    // SAFETY: a clock, which every call reads.
    let mut next = unsafe { mach_absolute_time() };
    loop {
        if listed.elapsed() >= LIST_EVERY {
            threads.list(me);
            listed = Instant::now();
        }
        if !threads.busy.is_empty() {
            turn = (turn + 1) % threads.busy.len();
            match pc_of(threads.busy[turn]) {
                Some(pc) if code.has(pc.wrapping_sub(code.slide)) => {
                    *pcs.entry(pc.wrapping_sub(code.slide)).or_default() += 1;
                    profile.samples += 1;
                }
                Some(_) => profile.elsewhere += 1,
                None => {}
            }
        }
        profile.ticks += 1;
        if written.elapsed() >= WRITE_EVERY {
            written = Instant::now();
            let Some(now) = files.sample() else { break };
            sample = now;
            profile.at = now_ms();
            profile.sampler_ns = cpu_ns(me).saturating_sub(spent_before);
            profile.pcs.clear();
            profile.pcs.extend(pcs.iter().map(|(pc, count)| (*pc, *count)));
            profile.pcs.sort_unstable();
            counters(sample.counters, &mut profile.counters);
            files.write(&profile);
        }
        let interval = ticks_of(sample.interval_us.max(LEAST_INTERVAL_US));
        next += interval;
        // SAFETY: a clock read; it takes nothing and changes nothing.
        let now = unsafe { mach_absolute_time() };
        // A sampler that fell behind, as when the machine slept, keeps its pace from now
        // rather than looking in a burst to catch up.
        if next < now {
            next = now + interval;
        }
        // SAFETY: waits until `next`, returning at once for a time that has passed; it touches
        // no memory.
        unsafe { mach_wait_until(next) };
    }
    // SAFETY: `mach_task_self` takes nothing and returns this task's port name, which needs
    // no release.
    let task = unsafe { mach_task_self() };
    // SAFETY: `me` is the right `mach_thread_self` gave at the start of `run`, given back
    // once, here.
    unsafe { mach_port_deallocate(task, me) };
}

/// Where `thread` is, if it is running.
fn pc_of(thread: u32) -> Option<u64> {
    if basic_info(thread)?.run_state != TH_STATE_RUNNING {
        return None;
    }
    let mut state = ThreadState::default();
    let mut count = (size_of::<ThreadState>() / 4) as u32;
    // SAFETY: `state` is as large as `count` says.
    let read = unsafe {
        thread_get_state(thread, ARM_THREAD_STATE64, &raw mut state as *mut u32, &mut count)
    };
    (read == 0).then_some(state.pc & ADDRESS)
}

fn basic_info(thread: u32) -> Option<ThreadBasicInfo> {
    let mut info = ThreadBasicInfo::default();
    let mut count = (size_of::<ThreadBasicInfo>() / 4) as u32;
    // SAFETY: `info` is as large as `count` says.
    let asked =
        unsafe { thread_info(thread, THREAD_BASIC_INFO, &raw mut info as *mut i32, &mut count) };
    (asked == 0).then_some(info)
}

/// The CPU time `thread` has taken, in nanoseconds.
fn cpu_ns(thread: u32) -> u64 {
    let ns = |time: &Time| time.seconds as u64 * 1_000_000_000 + time.microseconds as u64 * 1000;
    basic_info(thread).map_or(0, |info| ns(&info.user_time) + ns(&info.system_time))
}

/// The counters in use that are not zero, by their place, into `into`.
fn counters(used: u64, into: &mut Vec<(u32, u64)>) {
    // Called through its address, so that the linker keeps the function for the daemon's
    // code to call by name rather than only a copy inlined here.
    let find: extern "C" fn() -> *mut u64 = crate::blaze_live_counters;
    std::hint::black_box(find)();
    let used = (used as usize).min(format::COUNTERS);
    into.clear();
    into.extend(
        (crate::COUNTERS[..used].iter().enumerate())
            .map(|(at, counter)| (at as u32, counter.load(std::sync::atomic::Ordering::Relaxed)))
            .filter(|(_, count)| *count != 0),
    );
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_or(0, |it| it.as_millis() as u64)
}

/// The threads of the task as the sampler last listed them, each a port it holds a right to
/// with the CPU time the thread had taken then. The lists are reused from one listing to the
/// next: a listing allocates nothing once they have room for the program's threads.
#[derive(Default)]
struct Threads {
    /// Sorted by port, for a binary search per thread of the next listing.
    all: Vec<(u32, u64)>,
    /// Those but the sampler that took [`BUSY_NS`] since the list before, or were not on it.
    busy: Vec<u32>,
    /// The listing being made, swapped with `all` once it is.
    next: Vec<(u32, u64)>,
}

impl Threads {
    fn list(&mut self, me: u32) {
        let (mut list, mut count) = (std::ptr::null_mut::<u32>(), 0u32);
        // SAFETY: `mach_task_self` takes nothing and returns this task's port name, which
        // needs no release.
        let task = unsafe { mach_task_self() };
        // SAFETY: `list` and `count` are what the call fills in: an array the kernel allocates
        // in this task, and its length.
        if unsafe { task_threads(task, &mut list, &mut count) } != 0 {
            return;
        }
        // SAFETY: the kernel gave `count` ports at `list`, page-aligned memory of this task;
        // they are copied out into `next` before it is given back.
        let ports = unsafe { std::slice::from_raw_parts(list, count as usize) };
        self.next.clear();
        self.next.extend(ports.iter().map(|&port| (port, cpu_ns(port))));
        // SAFETY: `list` is the kernel's array of `count` ports, copied out above and not used
        // after; the rights to the ports are given back by `release`.
        unsafe { vm_deallocate(task, list as usize, count as usize * size_of::<u32>()) };
        self.next.sort_unstable_by_key(|it| it.0);
        let all = &self.all;
        let before =
            |port: u32| all.binary_search_by_key(&port, |it| it.0).ok().map(|at| all[at].1);
        let busy = self.next.iter().filter(|(port, ns)| {
            *port != me && before(*port).is_none_or(|was| ns.saturating_sub(was) >= BUSY_NS)
        });
        self.busy.clear();
        self.busy.extend(busy.map(|it| it.0));
        // The rights are given back after the new ones are taken, so that a thread keeps its
        // name from one list to the next.
        self.release();
        std::mem::swap(&mut self.all, &mut self.next);
    }

    /// Gives back the right to each port of `all`, which it empties.
    fn release(&mut self) {
        // SAFETY: `mach_task_self` takes nothing and returns this task's port name, which
        // needs no release.
        let task = unsafe { mach_task_self() };
        for (port, _) in self.all.drain(..) {
            // SAFETY: each port is one `task_threads` gave a right to, given back once: `drain`
            // takes it out of `all`.
            unsafe { mach_port_deallocate(task, port) };
        }
    }

    fn give_back(&mut self) {
        self.release();
        self.busy.clear();
    }
}

impl Drop for Threads {
    fn drop(&mut self) {
        self.give_back();
    }
}
