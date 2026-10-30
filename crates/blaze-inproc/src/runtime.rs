//! The run process's side, linked into the program: its quiescent points, its threads' fence
//! slots, and requests to the daemon over the channel.
//!
//! The loader leaves `BLAZE_INPROC_BOOT` in the program's environment: the address of its
//! [`Boot`], which says where the control words are and which channel to use. A program
//! started otherwise has no fence to report to: [`attach`] says so and [`quiesce`] does
//! nothing.

use std::cell::Cell;
use std::sync::{Mutex, OnceLock};

use crate::channel::Channel;
use crate::fence::{MOST_FRAMES, Thread};
use crate::frame::Kind;
use crate::{Error, sys};

/// The environment variable naming the [`Boot`]'s address, in hex.
pub const BOOT_VAR: &str = "BLAZE_INPROC_BOOT";

pub(crate) const BOOT_MAGIC: u64 = u64::from_le_bytes(*b"bzboot01");

/// What the loader tells the program it started, at a fixed layout: the loader and the
/// program each have their own copy of this crate.
#[repr(C)]
pub struct Boot {
    pub magic: u64,
    /// Where the control words are mapped, and their length.
    pub control: u64,
    pub control_len: u64,
    /// The channel's fd (Unix) or handle (Windows), inherited from the loader.
    pub channel: i64,
    /// Where the code memory's offset 0 is, and its length.
    pub base: u64,
    pub len: u64,
}

const _: () = assert!(size_of::<Boot>() == 48);

pub struct Attached {
    control: &'static [std::sync::atomic::AtomicU64],
    /// Taken for one request and its answer, by the thread that asks: requests are stub misses
    /// and the like, never a hot path, and nothing is called with it held.
    channel: Mutex<Channel>,
    base: u64,
    len: u64,
    pid: u32,
}

static ATTACHED: OnceLock<Result<Attached, Error>> = OnceLock::new();

/// This process's side of the fence and channel, once.
pub fn attach() -> Result<&'static Attached, Error> {
    ATTACHED.get_or_init(open).as_ref().map_err(Clone::clone)
}

fn open() -> Result<Attached, Error> {
    let at = std::env::var(BOOT_VAR)
        .map_err(|_| Error::new(format!("{BOOT_VAR} is not set: this program was not started by blaze's loader")))?;
    let at = u64::from_str_radix(at.trim_start_matches("0x"), 16)
        .map_err(|_| Error::new(format!("{BOOT_VAR}={at} is not a hex address")))?;
    let boot = sys::boot(at)?;
    if boot.magic != BOOT_MAGIC {
        return Err(Error::new(format!("{BOOT_VAR} at {at:#x} is not a boot record of this version")));
    }
    let control = sys::adopt_words(boot.control, boot.control_len)?;
    if !Thread::ready(&control) {
        return Err(Error::new(format!("the control words at {:#x} are not set up", boot.control)));
    }
    let channel = Channel::from_raw(boot.channel)?;
    Ok(Attached {
        control,
        channel: Mutex::new(channel),
        base: boot.base,
        len: boot.len,
        pid: std::process::id(),
    })
}

/// A quiescent point of the calling thread, when this process was started by the loader.
pub fn quiesce() {
    if let Ok(attached) = attach() {
        attached.quiesce();
    }
}

/// The calling thread's slot and the epoch it last reported, and its release when it ends.
struct Local {
    slot: Cell<Option<usize>>,
    reported: Cell<u64>,
}

impl Drop for Local {
    fn drop(&mut self) {
        let Some(index) = self.slot.get() else { return };
        let Some(Ok(attached)) = ATTACHED.get() else { return };
        let mut frames = [0u64; MOST_FRAMES];
        let walked = sys::backtrace(&mut frames).map(|len| &frames[..len]);
        let epoch = Thread::epoch(&attached.control);
        if Thread::seen(&attached.control, epoch, walked) == epoch {
            Thread::release(&attached.control, index);
        } else {
            // A frame returns into retired code: the slot holds its fence until the daemon
            // finds the thread gone.
            Thread::report(&attached.control, index, Thread::seen(&attached.control, epoch, walked));
        }
    }
}

thread_local! {
    static LOCAL: Local = const { Local { slot: Cell::new(None), reported: Cell::new(0) } };
}

impl Attached {
    /// Where the code memory's offset 0 is in this process.
    pub fn base(&self) -> u64 {
        self.base
    }

    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// A quiescent point of the calling thread: it is in no code it could leave and return
    /// to except what its stack shows. While the epoch has not changed since the thread last
    /// reported, one atomic load.
    pub fn quiesce(&self) {
        let epoch = Thread::epoch(&self.control);
        let _ = LOCAL.try_with(|local| {
            if let Some(index) = local.slot.get()
                && local.reported.get() == epoch
            {
                if Thread::holds(&self.control, index, self.pid, sys::thread_id()) {
                    return;
                }
                local.slot.set(None);
            }
            sys::serialize();
            let mut frames = [0u64; MOST_FRAMES];
            let walked = sys::backtrace(&mut frames).map(|len| &frames[..len]);
            let seen = Thread::seen(&self.control, epoch, walked);
            let tid = sys::thread_id();
            match local.slot.get() {
                Some(index) if Thread::holds(&self.control, index, self.pid, tid) => {
                    Thread::report(&self.control, index, seen);
                }
                _ => local.slot.set(Thread::claim(&self.control, self.pid, tid, seen)),
            }
            local.reported.set(if seen == epoch { epoch } else { 0 });
        });
    }

    /// Sends a request of `kind` with `body` and puts the answer's body in `answer`; says the
    /// answer's kind. One request at a time over the process; a thread waits its turn.
    pub fn request(&self, kind: Kind, body: &[u8], answer: &mut Vec<u8>) -> Result<Kind, Error> {
        let mut channel = self.channel.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let frame = channel.request(kind, body)?;
        answer.clear();
        answer.extend_from_slice(frame.body);
        Ok(frame.kind)
    }
}
