//! The stall watchdog: turns a build that stopped moving into an error naming where, within a
//! bound, instead of a daemon at 0% CPU that only `sample` can explain.
//!
//! [`Watchdog`] is sans IO: the daemon's loop tells it, each turn, the time on its driver's
//! clock (virtual under the simulation), a count that grows with every span closed and every
//! task ended ([`crate::flight::progress`] plus the tasks the plane saw end), and whether work
//! only the daemon's own threads can finish is under way (tasks on its workers; a wait on a
//! child process or on I/O is not the daemon's to finish). When that work makes no progress for
//! the bound, [`Watchdog::tick`] says so once, and [`stall`] reads every thread's open spans and
//! the flight recorder into a [`Stall`], the error the daemon fails the requests with.

use std::{fmt, time::Duration};

use crate::flight::{self, ThreadNow};

/// How much of the flight recorder a stall keeps: the stretch it was quiet and some before.
const KEPT: Duration = Duration::from_secs(120);

pub struct Watchdog {
    bound_ns: u64,
    seen: u64,
    /// When the work under way last moved, while there is some.
    quiet_since: Option<u64>,
    /// Said for the present quiet stretch already.
    said: bool,
}

/// No progress from `since_ns` for `for_ns`, on the clock the watchdog was told.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Quiet {
    pub since_ns: u64,
    pub for_ns: u64,
}

impl Watchdog {
    /// The bound `blaze serve` gives it: longer than any task that closes no info-level span
    /// should run.
    pub const BOUND: Duration = Duration::from_secs(60);

    pub fn new(bound: Duration) -> Watchdog {
        Watchdog { bound_ns: bound.as_nanos() as u64, seen: 0, quiet_since: None, said: false }
    }

    /// One turn: `now_ns` on the loop's clock, `progress` the count of spans closed and tasks
    /// ended, `busy` whether the daemon's threads have work under way. `Some` once a quiet
    /// stretch reaches the bound, and not again until progress resumes.
    pub fn tick(&mut self, now_ns: u64, progress: u64, busy: bool) -> Option<Quiet> {
        if progress != self.seen || !busy {
            self.seen = progress;
            self.quiet_since = busy.then_some(now_ns);
            self.said = false;
            return None;
        }
        let since = *self.quiet_since.get_or_insert(now_ns);
        let for_ns = now_ns.saturating_sub(since);
        if self.said || for_ns < self.bound_ns {
            return None;
        }
        self.said = true;
        Some(Quiet { since_ns: since, for_ns })
    }
}

/// A build that stopped: how long nothing moved, how many tasks were under way, each thread's
/// open spans, and the flight recorder's Chrome trace of the time up to it.
pub struct Stall {
    pub quiet_ns: u64,
    pub running: usize,
    pub threads: Vec<ThreadNow>,
    pub trace: Vec<u8>,
}

/// The stall `quiet` found, with `running` tasks under way: every thread read now.
pub fn stall(quiet: Quiet, running: usize) -> Stall {
    Stall {
        quiet_ns: quiet.for_ns,
        running,
        threads: flight::threads_now(),
        trace: flight::dump(KEPT),
    }
}

impl fmt::Display for Stall {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let secs = |ns: u64| ns as f64 / 1e9;
        write!(
            f,
            "blaze stalled: no span closed and no task ended for {:.1} s while {} tasks ran",
            secs(self.quiet_ns),
            self.running
        )?;
        if self.threads.is_empty() {
            return f.write_str("; no thread is inside a span");
        }
        f.write_str("; the threads inside spans:")?;
        for thread in &self.threads {
            write!(f, "\n  {} (tid {}):", thread.name, thread.tid)?;
            for (i, span) in thread.open.iter().enumerate() {
                let sep = if i == 0 { " " } else { " > " };
                write!(f, "{sep}{} ({:.1} s)", span.name, secs(span.for_ns))?;
            }
            if thread.deeper > 0 {
                write!(f, " > {} more", thread.deeper)?;
            }
        }
        Ok(())
    }
}

impl fmt::Debug for Stall {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Stall")
            .field("quiet_ns", &self.quiet_ns)
            .field("running", &self.running)
            .field("threads", &self.threads)
            .field("trace_bytes", &self.trace.len())
            .finish()
    }
}

impl std::error::Error for Stall {}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Barrier, Condvar, Mutex};

    use super::*;

    const SEC: u64 = 1_000_000_000;

    /// A job of the request: inside its span, it waits for the next job, which waits for the
    /// one after, around to itself, until the test lets them go.
    fn job(k: usize, ready: &Barrier, gate: &(Mutex<bool>, Condvar)) {
        let _span = match k {
            0 => tracing::info_span!(target: "blaze", "job a").entered(),
            1 => tracing::info_span!(target: "blaze", "job b").entered(),
            _ => tracing::info_span!(target: "blaze", "job c").entered(),
        };
        let _wait = tracing::info_span!(target: "blaze", "waits on the next job").entered();
        ready.wait();
        let (released, cv) = gate;
        let mut released = released.lock().unwrap();
        while !*released {
            released = cv.wait(released).unwrap();
        }
    }

    #[test]
    fn a_request_whose_jobs_all_wait_on_each_other_fails_with_the_stall_error_instead_of_hanging() {
        crate::install();
        const JOBS: usize = 3;
        let ready = Arc::new(Barrier::new(JOBS + 1));
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let workers: Vec<_> = (0..JOBS)
            .map(|k| {
                let (ready, gate) = (ready.clone(), gate.clone());
                std::thread::Builder::new()
                    .name(format!("worker {k}"))
                    .spawn(move || job(k, &ready, &gate))
                    .unwrap()
            })
            .collect();
        ready.wait();
        // The daemon's loop on a virtual clock, a turn each 100 ms: the jobs never end, so the
        // count of tasks ended stays where it is.
        let mut watchdog = Watchdog::new(Watchdog::BOUND);
        let ended = 0;
        let mut now = 0;
        let quiet = loop {
            assert!(
                now < 3600 * SEC,
                "an hour of the clock went by with no stall said: the request hangs"
            );
            if let Some(quiet) = watchdog.tick(now, flight::progress() + ended, true) {
                break quiet;
            }
            now += SEC / 10;
        };
        let error = stall(quiet, JOBS);
        let (released, cv) = &*gate;
        *released.lock().unwrap() = true;
        cv.notify_all();
        for worker in workers {
            worker.join().unwrap();
        }
        assert!(error.quiet_ns >= Watchdog::BOUND.as_nanos() as u64, "{error}");
        assert!(
            error.quiet_ns < Watchdog::BOUND.as_nanos() as u64 + SEC,
            "said within a turn of the bound: {error}"
        );
        let text = error.to_string();
        for name in [
            "job a",
            "job b",
            "job c",
            "waits on the next job",
            "worker 0",
            "worker 2",
            "3 tasks ran",
        ] {
            assert!(text.contains(name), "the stall names `{name}`: {text}");
        }
        let stuck = error
            .threads
            .iter()
            .filter(|it| it.open.iter().any(|span| span.name.starts_with("job ")))
            .count();
        assert_eq!(stuck, JOBS, "{text}");
        let trace: serde_json::Value = serde_json::from_slice(&error.trace).unwrap();
        assert!(trace["traceEvents"].is_array());
    }

    #[test]
    fn work_that_keeps_moving_within_the_bound_never_stalls() {
        let mut rng = 0x2545_f491_4f6c_dd1du64;
        let mut next = move || {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng
        };
        for _ in 0..64 {
            let mut watchdog = Watchdog::new(Duration::from_secs(10));
            let (mut now, mut progress) = (0, 0);
            for _ in 0..2000 {
                now += next() % (2 * SEC);
                // Moves at least every 10 s minus a turn; is idle (nothing under way) at times.
                let quiet_for = next() % (8 * SEC);
                let busy = next() % 5 != 0;
                assert_eq!(watchdog.tick(now, progress, busy), None);
                now += quiet_for;
                assert_eq!(watchdog.tick(now, progress, busy), None);
                progress += 1 + next() % 3;
            }
        }
    }

    #[test]
    fn a_stall_is_said_once_a_quiet_stretch_and_again_after_progress() {
        let mut watchdog = Watchdog::new(Duration::from_secs(10));
        assert_eq!(watchdog.tick(0, 5, true), None);
        assert_eq!(watchdog.tick(9 * SEC, 5, true), None);
        assert_eq!(watchdog.tick(10 * SEC, 5, true), Some(Quiet { since_ns: 0, for_ns: 10 * SEC }));
        assert_eq!(watchdog.tick(30 * SEC, 5, true), None);
        // Nothing under way is no stall, however long.
        assert_eq!(watchdog.tick(31 * SEC, 6, false), None);
        assert_eq!(watchdog.tick(500 * SEC, 6, false), None);
        assert_eq!(watchdog.tick(501 * SEC, 6, true), None);
        assert_eq!(
            watchdog.tick(511 * SEC, 6, true),
            Some(Quiet { since_ns: 501 * SEC, for_ns: 10 * SEC })
        );
    }
}
