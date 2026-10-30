//! The run process's side: what the resolver ([`crate::stub`]) calls on a lazy function's
//! first call, linked into the program's image and compiled ahead with it.
//!
//! [`blaze_lazy_resolve`] asks the daemon for the function's code over blaze-inproc's
//! channel and returns where the daemon published it; the resolver jumps there. The daemon
//! writes the hook's address (the image's [`SYMBOL`]) into the stub region's cell before
//! the program starts. An answer that the function can't be compiled, a channel that fails
//! or an answer that is not this ask's stops the process with that error on stderr: the
//! resolver never jumps anywhere the daemon did not publish.
//!
//! Many threads may miss at once: each asks, and the daemon compiles the function once and
//! answers each of them (single flight, [`crate::fetch`]). A thread that loaded the slot
//! before the daemon stored it misses too, and is answered at once with the published
//! address.

use std::io::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};

use blaze_inproc::runtime;

use crate::wire::{self, ASK_KIND, Frame, READY_LEN};

/// The hook's symbol, which the daemon looks up in the placed image.
pub const SYMBOL: &str = "blaze_lazy_resolve";

/// The process's request ids: unique among its asks.
static REQUESTS: AtomicU64 = AtomicU64::new(1);

/// Where the daemon published lazy function `lazy`'s code, or why it can't.
pub fn fetch(lazy: u32) -> Result<u64, String> {
    let attached = runtime::attach().map_err(|it| format!("asking the daemon for its code: {it}"))?;
    let request = REQUESTS.fetch_add(1, Ordering::Relaxed);
    let mut answer = Vec::with_capacity(READY_LEN);
    let kind = attached
        .request(ASK_KIND, &wire::ask(request, lazy), &mut answer)
        .map_err(|it| format!("asking the daemon for its code: {it}"))?;
    let frame = Frame::decode(&answer).map_err(|it| format!("the daemon's answer: {it}"))?;
    if wire::channel_kind(&frame) != kind {
        return Err(format!("the daemon's answer of kind {:#06x} carries {frame:?}", kind.0));
    }
    match frame {
        Frame::Ready { request: asked, lazy: of, target } if asked == request && of == lazy && target != 0 => {
            Ok(target)
        }
        Frame::Failed { request: asked, lazy: of, why } if asked == request && of == lazy => Err(why.to_owned()),
        other => Err(format!("the daemon answered {other:?} to ask {request}")),
    }
}

/// The resolver's call: the address to jump to for lazy function `lazy`. Never returns
/// otherwise; a panic can't unwind out of it (it is `extern "C"`), so it aborts too.
// SAFETY: the one definition of this symbol in a lazy run's image: the crate is linked into
// it once, and no other crate defines `blaze_lazy_resolve`.
#[allow(unsafe_code)]
#[unsafe(no_mangle)]
pub extern "C" fn blaze_lazy_resolve(lazy: u32) -> u64 {
    match fetch(lazy) {
        Ok(target) => target,
        Err(why) => stop(lazy, &why),
    }
}

fn stop(lazy: u32, why: &str) -> ! {
    let mut err = std::io::stderr().lock();
    let _ = writeln!(err, "blaze: the first call of lazy function {lazy} stopped the run: {why}");
    let _ = err.flush();
    std::process::abort()
}
