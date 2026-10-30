//! The smallest input that still fails, as libFuzzer's `-minimize_crash`: a failure may end
//! the process (an AddressSanitizer report), so each try is a child process running
//! `<this binary> run <try>`. Chunks are cut out, halving their size down to one byte, then each
//! byte is tried as zero; a try counts when it fails the way the input did (a failing exit, or
//! a timeout for a timeout).

use std::{
    path::Path,
    process::{Command, ExitCode, Stdio},
    time::{Duration, Instant},
};

use crate::corpus;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Outcome {
    Passed,
    Failed,
    TimedOut,
}

struct Tries<'a> {
    exe: &'a Path,
    file: &'a Path,
    timeout: Duration,
    deadline: Instant,
    runs: u64,
}

impl Tries<'_> {
    fn outcome(&mut self, bytes: &[u8]) -> Result<Outcome, String> {
        std::fs::write(self.file, corpus::to_hex(bytes)).map_err(|e| format!("{}: {e}", self.file.display()))?;
        self.runs += 1;
        let mut child = Command::new(self.exe)
            .arg("run")
            .arg(self.file)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("{}: {e}", self.exe.display()))?;
        let started = Instant::now();
        loop {
            if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
                return Ok(if status.success() { Outcome::Passed } else { Outcome::Failed });
            }
            if started.elapsed() > self.timeout {
                _ = child.kill();
                _ = child.wait();
                return Ok(Outcome::TimedOut);
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    fn left(&self) -> bool {
        Instant::now() < self.deadline
    }
}

pub(crate) fn minimize(input: &Path, out: &Path, budget: Duration, timeout: Duration) -> Result<ExitCode, String> {
    let exe = std::env::current_exe().map_err(|e| format!("this binary's path: {e}"))?;
    std::fs::create_dir_all(out).map_err(|e| format!("{}: {e}", out.display()))?;
    let file = out.join(format!("minimizing-{}.hex", std::process::id()));
    let mut tries = Tries { exe: &exe, file: &file, timeout, deadline: Instant::now() + budget, runs: 0 };
    let mut best = corpus::read(input)?;
    let wanted = tries.outcome(&best)?;
    if wanted == Outcome::Passed {
        _ = std::fs::remove_file(&file);
        return Err(format!("{} passes: nothing to minimize", input.display()));
    }
    let mut chunk = best.len().div_ceil(2).max(1);
    while !best.is_empty() && tries.left() {
        let mut cut = false;
        let mut at = 0;
        while at < best.len() && tries.left() {
            let mut smaller = best.clone();
            smaller.drain(at..(at + chunk).min(best.len()));
            if tries.outcome(&smaller)? == wanted {
                best = smaller;
                cut = true;
            } else {
                at += chunk;
            }
        }
        if chunk == 1 && !cut {
            break;
        }
        if !cut {
            chunk = chunk.div_ceil(2);
        }
    }
    for at in 0..best.len() {
        if !tries.left() {
            break;
        }
        if best[at] == 0 {
            continue;
        }
        let mut simpler = best.clone();
        simpler[at] = 0;
        if tries.outcome(&simpler)? == wanted {
            best = simpler;
        }
    }
    _ = std::fs::remove_file(&file);
    let path = corpus::write(out, "minimized-", &best)?;
    eprintln!(
        "blaze-fuzzer: minimized {} to {} bytes in {} runs ({:?}): {}",
        input.display(),
        best.len(),
        tries.runs,
        wanted,
        path.display()
    );
    Ok(ExitCode::SUCCESS)
}
