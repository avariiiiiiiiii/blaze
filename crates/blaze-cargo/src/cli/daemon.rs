//! The daemon this CLI is a client of: found at its socket ([`crate::serve::socket`]), started
//! as `blaze serve` when none answers there, spoken to through [`blaze_api::client::Client`].

use std::{
    path::Path,
    time::{Duration, Instant},
};

use anyhow::Context as _;
use blaze_api::{
    Request, Response, Status,
    client::{Client, ClientError},
};

/// How long a daemon that was started is waited for, and one that was told to stop.
pub const PATIENCE: Duration = Duration::from_secs(10);
const POLL: Duration = Duration::from_millis(5);

/// What the commands ask of the daemon: [`Client::call`], which the tests fake.
pub trait Api {
    /// Sends `request` and hands each of its responses to `on` until its Done; Done's `ok`.
    fn call(&mut self, request: Request, on: &mut dyn FnMut(&Response)) -> Result<bool, ClientError>;
}

impl Api for Client {
    fn call(&mut self, request: Request, on: &mut dyn FnMut(&Response)) -> Result<bool, ClientError> {
        Client::call(self, request, on)
    }
}

/// Whether a daemon is started when none answers.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Start {
    IfMissing,
    Never,
}

/// A connection to the daemon; `None` when none answers and none was to be started.
#[cfg(unix)]
pub fn connect(start: Start) -> anyhow::Result<Option<Client>> {
    let socket = crate::serve::socket()?;
    match Client::unix(&socket) {
        Ok(client) => return Ok(Some(client)),
        Err(_) if start == Start::Never => return Ok(None),
        Err(_) => {}
    }
    let log = started()?;
    let asked = Instant::now();
    loop {
        match Client::unix(&socket) {
            Ok(client) => return Ok(Some(client)),
            Err(error) if asked.elapsed() > PATIENCE => {
                return Err(anyhow::anyhow!(not_listening(&socket, &log, &error.to_string())));
            }
            Err(_) => std::thread::sleep(POLL),
        }
    }
}

#[cfg(not(unix))]
pub fn connect(_: Start) -> anyhow::Result<Option<Client>> {
    anyhow::bail!(
        "blaze's CLI speaks to its daemon over the Unix socket {}, and blaze_api's client has \
         Unix sockets on Unix only",
        crate::serve::socket()?.display()
    )
}

/// What a start that never listened says.
pub fn not_listening(socket: &Path, log: &Path, error: &str) -> String {
    format!(
        "the daemon (`blaze serve`) was started and nothing listens at {} after {}s ({error}); \
         its log is {}",
        socket.display(),
        PATIENCE.as_secs(),
        log.display()
    )
}

/// Starts `blaze serve` in a process group of its own, so that what ends this command or its
/// terminal's foreground does not end it, with its output appended to its log; the log.
#[cfg(unix)]
fn started() -> anyhow::Result<std::path::PathBuf> {
    let state = crate::serve::state()?;
    crate::socket::private_dir(&state).with_context(|| format!("the daemon's directory {}", state.display()))?;
    let log = state.join("serve.log");
    let mut options = std::fs::OpenOptions::new();
    let file = crate::socket::private_file(options.create(true).append(true), &log)
        .with_context(|| format!("the daemon's log {}", log.display()))?;
    let mut daemon = std::process::Command::new(std::env::current_exe().context("finding blaze's own executable")?);
    daemon.arg("serve");
    daemon.stdin(std::process::Stdio::null()).stdout(file.try_clone()?).stderr(file);
    std::os::unix::process::CommandExt::process_group(&mut daemon, 0);
    daemon.spawn().context("starting the daemon (`blaze serve`)")?;
    Ok(log)
}

/// `blaze stop`: the daemon made durable and ended, waited for until its socket is gone.
pub fn stop(err: &mut dyn std::io::Write, color: bool) -> anyhow::Result<i32> {
    let Some(mut client) = connect(Start::Never)? else {
        writeln!(err, "no daemon is running")?;
        return Ok(0);
    };
    let mut said = Vec::new();
    let ok = Api::call(&mut client, Request::Stop, &mut |response| {
        if let Response::Error(error) = response {
            said.push(error.what.clone());
        }
    })?;
    drop(client);
    for what in said {
        err.write_all(super::say::error(&what, color).as_bytes())?;
    }
    if !ok {
        return Ok(super::FAILED);
    }
    let socket = crate::serve::socket()?;
    let asked = Instant::now();
    while answers(&socket) {
        if asked.elapsed() > PATIENCE {
            let what = format!("the daemon at {} was told to stop and still answers after {}s", socket.display(), PATIENCE.as_secs());
            err.write_all(super::say::error(&what, color).as_bytes())?;
            return Ok(super::FAILED);
        }
        std::thread::sleep(POLL);
    }
    Ok(0)
}

#[cfg(unix)]
fn answers(socket: &Path) -> bool {
    Client::unix(socket).is_ok()
}

#[cfg(not(unix))]
fn answers(_: &Path) -> bool {
    false
}

/// `blaze status`: what the daemon is doing, on stdout.
pub fn status(out: &mut dyn std::io::Write, err: &mut dyn std::io::Write, color: bool) -> anyhow::Result<i32> {
    let Some(mut client) = connect(Start::Never)? else {
        writeln!(out, "no daemon is running")?;
        return Ok(0);
    };
    let mut status = None;
    let mut said = Vec::new();
    let ok = Api::call(&mut client, Request::Status, &mut |response| match response {
        Response::Status(it) => status = Some(it.clone()),
        Response::Error(error) => said.push(error.what.clone()),
        _ => {}
    })?;
    for what in said {
        err.write_all(super::say::error(&what, color).as_bytes())?;
    }
    if let Some(status) = status {
        out.write_all(status_lines(&status).as_bytes())?;
    }
    Ok(if ok { 0 } else { super::FAILED })
}

/// The daemon's [`Status`] for people.
pub fn status_lines(status: &Status) -> String {
    let ids: Vec<String> = status.requests.iter().map(u64::to_string).collect();
    let requests = match ids.is_empty() {
        true => "0".to_owned(),
        false => format!("{} ({})", ids.len(), ids.join(", ")),
    };
    format!(
        "requests: {requests}\nviews: {}\nmemory: {} of {}\n",
        status.views,
        bytes(status.memory_bytes),
        bytes(status.budget_bytes)
    )
}

/// `512 B`, `1.5 KiB`, `2.0 GiB`.
pub fn bytes(count: u64) -> String {
    const UNITS: [&str; 5] = ["KiB", "MiB", "GiB", "TiB", "PiB"];
    if count < 1024 {
        return format!("{count} B");
    }
    let mut value = count as f64 / 1024.0;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

#[cfg(test)]
mod tests;
