//! The flight recorder's clock: the CPU's own counter, read in a few cycles with no system
//! call. On aarch64 it is `CNTVCT_EL0`, whose rate `CNTFRQ_EL0` gives (24 MHz on Apple, 1 GHz
//! on Neoverse V2); on x86_64 the invariant TSC, whose rate is measured against the monotonic
//! clock between [`origin`] and the moment a tick is read back ([`Scale`]). Elsewhere a tick is
//! a nanosecond of [`Instant`].

use std::{sync::OnceLock, time::Instant};

/// The counter now.
#[inline(always)]
#[cfg_attr(target_arch = "x86_64", allow(unused_unsafe))]
pub fn ticks() -> u64 {
    #[cfg(target_arch = "aarch64")]
    {
        let at: u64;
        // SAFETY: reading CNTVCT_EL0 has no side effect; every aarch64 OS blaze runs on (macOS,
        // Linux, Windows) gives user space access to it.
        unsafe {
            std::arch::asm!("mrs {}, cntvct_el0", out(reg) at, options(nomem, nostack, preserves_flags))
        };
        at
    }
    #[cfg(target_arch = "x86_64")]
    {
        // SAFETY: RDTSC has no side effect and is available on every x86_64 CPU.
        unsafe { std::arch::x86_64::_rdtsc() }
    }
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    {
        let (_, at) = origin();
        at.elapsed().as_nanos() as u64
    }
}

/// The counter's rate in Hz, where the CPU says it.
#[cfg(target_arch = "aarch64")]
fn frequency() -> u64 {
    let hz: u64;
    // SAFETY: reading CNTFRQ_EL0 has no side effect and user space may read it.
    unsafe {
        std::arch::asm!("mrs {}, cntfrq_el0", out(reg) hz, options(nomem, nostack, preserves_flags))
    };
    hz
}

/// A tick and the instant it was read at, once a process: [`crate::began`] takes it first.
pub fn origin() -> (u64, Instant) {
    static ORIGIN: OnceLock<(u64, Instant)> = OnceLock::new();
    *ORIGIN.get_or_init(|| (ticks(), Instant::now()))
}

/// Ticks turned into instants: a tick and an instant read together, and the nanoseconds a tick
/// lasts. Made once per read-back (a dump, a closed span of a profile), never per span recorded.
#[derive(Clone, Copy, Debug)]
pub struct Scale {
    ticks: u64,
    at: Instant,
    ns_per_tick: f64,
}

impl Scale {
    pub fn now() -> Scale {
        let (first, began) = origin();
        let (ticks, at) = (ticks(), Instant::now());
        #[cfg(target_arch = "aarch64")]
        let ns_per_tick = {
            _ = (first, began);
            1e9 / frequency().max(1) as f64
        };
        #[cfg(target_arch = "x86_64")]
        let ns_per_tick = {
            // A pairing read less than 100 µs after the origin's would be off by its own
            // skew (tens of ns) over too short a time: wait out the rest of the 100 µs, once.
            let (ticks, at) = if at.duration_since(began).as_micros() < 100 {
                while began.elapsed().as_micros() < 100 {
                    std::hint::spin_loop();
                }
                (self::ticks(), Instant::now())
            } else {
                (ticks, at)
            };
            at.duration_since(began).as_nanos() as f64 / ticks.wrapping_sub(first).max(1) as f64
        };
        #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
        let ns_per_tick = {
            _ = (first, began);
            1.0
        };
        Scale { ticks, at, ns_per_tick }
    }

    /// The nanoseconds `ticks` ticks last.
    pub fn ns(&self, ticks: u64) -> u64 {
        (ticks as f64 * self.ns_per_tick) as u64
    }

    /// The instant the counter read `ticks`, which was no later than [`Scale::now`].
    pub fn instant(&self, ticks: u64) -> Instant {
        let ago = self.ns(self.ticks.wrapping_sub(ticks));
        self.at.checked_sub(std::time::Duration::from_nanos(ago)).unwrap_or(self.at)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ticks_read_back_as_the_instants_they_were_read_at() {
        let before = Instant::now();
        let tick = ticks();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let scale = Scale::now();
        let at = scale.instant(tick);
        let off = at.max(before).duration_since(at.min(before));
        assert!(off.as_micros() < 200, "a tick read back {off:?} from when it was read");
        let later = ticks();
        assert!(later > tick, "the counter goes forward");
    }
}
