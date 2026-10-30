//! The daemon this CLI is a client of: found at its socket ([`crate::serve::socket`]), started
//! as `blaze serve` when none answers there, spoken to through [`blaze_api::client::Client`].

use std::{
    io,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use blaze_api::{
    Request, ResponseRef, Status,
    client::{Client, ClientError},
};
#[cfg(unix)]
use blaze_api::units::Installed;

use super::args::DaemonVerb;

/// How long a daemon that was started is waited for, and one that was told to stop.
pub const PATIENCE: Duration = Duration::from_secs(10);
/// A socket that comes up within a millisecond is used within one.
const POLL: Duration = Duration::from_millis(1);

/// What the commands ask of the daemon, which the tests fake.
pub trait Api {
    /// Sends `request` and hands each of its responses to `on`, read where it lies in its frame
    /// ([`ResponseRef`]), until its Done (handed too); Done's `ok`. `on`'s second argument says
    /// whether another whole response is already held ([`Client::call_ref`]'s), so a caller that
    /// prints writes what it holds only when it is false: one write per burst the daemon sent.
    fn call<F: FnMut(ResponseRef<'_>, bool)>(&mut self, request: Request, on: F) -> Result<bool, ClientError>;
}

/// One request per connection: anything held once its Done has come (a response to another
/// request, or more of this one) is the daemon's mistake, said as one.
impl Api for Client {
    fn call<F: FnMut(ResponseRef<'_>, bool)>(&mut self, request: Request, on: F) -> Result<bool, ClientError> {
        let ok = self.call_ref(request, on)?;
        if self.buffered() {
            let said = "the daemon sent more on a connection whose one request had its Done";
            // alloc: error path, the daemon's mistake.
            return Err(ClientError::Refused(blaze_api::ApiError::new(said, None)));
        }
        Ok(ok)
    }
}

/// Whether a daemon is started when none answers.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Start {
    IfMissing,
    Never,
}

/// Why the CLI has no daemon to speak to.
#[derive(Debug)]
pub enum DaemonError {
    /// A connect to the socket failed for another reason than that nothing listens there:
    /// no daemon is started over it.
    Connect { socket: PathBuf, error: ClientError },
    /// The daemon was started and nothing listened at its socket within [`PATIENCE`], after
    /// `tries` connects.
    NotListening { socket: PathBuf, log: PathBuf, tries: u32, error: ClientError },
}

impl std::fmt::Display for DaemonError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DaemonError::Connect { socket, error } => {
                write!(f, "connecting to blaze's daemon at {}: {error}", socket.display())
            }
            DaemonError::NotListening { socket, log, tries, error } => {
                // alloc: error path, said once.
                write!(f, "{} ({tries} connects tried)", not_listening(socket, log, &error.to_string()))
            }
        }
    }
}

impl std::error::Error for DaemonError {}

/// Whether a connect failed because nothing listens at the socket: no file there, or one a
/// daemon that is gone left.
pub fn nothing_listens(error: &ClientError) -> bool {
    matches!(error, ClientError::Io(it) if matches!(it.kind(), io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused))
}

/// A connection to the daemon; `None` when none answers and none was to be started.
///
/// Where systemd or launchd is socket-activating the daemon, the connect itself starts it
/// (the manager listens and the request waits in its backlog); the CLI starts `blaze serve`
/// only where no unit serves the socket, and says so.
#[cfg(unix)]
pub fn connect(start: Start) -> anyhow::Result<Option<Client>> {
    let socket = crate::serve::socket()?;
    let refused = match Client::unix(&socket) {
        Ok(client) => return Ok(Some(client)),
        Err(error) if !nothing_listens(&error) => return Err(DaemonError::Connect { socket, error }.into()),
        Err(_) if start == Start::Never => return Ok(None),
        Err(error) => error,
    };
    match activation(&socket)? {
        Installed::Yes => {
            let place = blaze_api::units::place_here()?;
            anyhow::bail!(
                "blaze's daemon is socket-activated for {} ({}) and nothing listens there ({refused}): \
                 `blaze daemon install` loads its units again",
                socket.display(),
                blaze_api::units::main_file(&place).display()
            );
        }
        Installed::Elsewhere(theirs) => eprintln!(
            "blaze: starting the daemon for {}: the installed units serve {} (`blaze daemon install` with this \
             environment serves this one)",
            socket.display(),
            theirs.display()
        ),
        Installed::No => eprintln!(
            "blaze: starting the daemon (no socket activation is installed: `blaze daemon install` has the system \
             start it on demand)"
        ),
    }
    let log = started()?;
    let asked = Instant::now();
    let mut tries = 0u32;
    loop {
        tries += 1;
        match Client::unix(&socket) {
            Ok(client) => return Ok(Some(client)),
            Err(error) if !nothing_listens(&error) => return Err(DaemonError::Connect { socket, error }.into()),
            Err(error) if asked.elapsed() > PATIENCE => {
                return Err(DaemonError::NotListening { socket, log, tries, error }.into());
            }
            Err(_) => std::thread::sleep(POLL),
        }
    }
}

/// Whether a launchd or systemd unit serves `socket`. Units live under `HOME`: without one,
/// none is installed.
#[cfg(unix)]
fn activation(socket: &Path) -> anyhow::Result<Installed> {
    if blaze_api::activation::Manager::here().is_none() || std::env::var_os("HOME").is_none_or(|it| it.is_empty()) {
        return Ok(Installed::No);
    }
    Ok(blaze_api::units::installed(&blaze_api::units::place_here()?, socket)?)
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
    use anyhow::Context as _;
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
// dyn: the CLI's output writer, written a few times per command.
pub fn stop(err: &mut dyn std::io::Write, color: bool) -> anyhow::Result<i32> {
    let Some(mut client) = connect(Start::Never)? else {
        writeln!(err, "no daemon is running")?;
        return Ok(0);
    };
    let mut said = Vec::new();
    let ok = Api::call(&mut client, Request::Stop, |response, _| {
        if let ResponseRef::Error(error) = response {
            // alloc: error path, the daemon's errors, said once it is done.
            said.push(error.what.to_owned());
        }
    })?;
    for what in said {
        err.write_all(super::say::error(&what, color).as_bytes())?;
    }
    if !ok {
        return Ok(super::FAILED);
    }
    let socket = crate::serve::socket()?;
    // With a unit installed the socket answers on (the manager listens, and a connect would
    // start the daemon again): the daemon hangs up its connection last, once its image is
    // written.
    #[cfg(unix)]
    if activation(&socket)? == Installed::Yes {
        if let Err(error) = client.until_hung_up(PATIENCE) {
            let what = format!("the daemon at {} was told to stop: {error}", socket.display());
            err.write_all(super::say::error(&what, color).as_bytes())?;
            return Ok(super::FAILED);
        }
        return Ok(0);
    }
    drop(client);
    let asked = Instant::now();
    while answers(&socket)? {
        if asked.elapsed() > PATIENCE {
            let what = format!("the daemon at {} was told to stop and still answers after {}s", socket.display(), PATIENCE.as_secs());
            err.write_all(super::say::error(&what, color).as_bytes())?;
            return Ok(super::FAILED);
        }
        std::thread::sleep(POLL);
    }
    Ok(0)
}

/// Whether the daemon still answers at `socket`; a connect that fails for another reason than
/// that nothing listens is the error.
#[cfg(unix)]
fn answers(socket: &Path) -> Result<bool, DaemonError> {
    match Client::unix(socket) {
        Ok(_) => Ok(true),
        Err(error) if nothing_listens(&error) => Ok(false),
        Err(error) => Err(DaemonError::Connect { socket: socket.to_owned(), error }),
    }
}

#[cfg(not(unix))]
fn answers(_: &Path) -> Result<bool, DaemonError> {
    Ok(false)
}

/// `blaze status`: what the daemon is doing, on stdout.
// dyn: the CLI's output writer, written a few times per command.
pub fn status(out: &mut dyn std::io::Write, err: &mut dyn std::io::Write, color: bool) -> anyhow::Result<i32> {
    let Some(mut client) = connect(Start::Never)? else {
        writeln!(out, "no daemon is running")?;
        return Ok(0);
    };
    let mut status = None;
    let mut said = Vec::new();
    let ok = Api::call(&mut client, Request::Status, |response, _| match response {
        ResponseRef::Status(it) => status = Some(it),
        // alloc: error path, the daemon's errors, said once it is done.
        ResponseRef::Error(error) => said.push(error.what.to_owned()),
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

/// `blaze daemon install`: a running daemon stopped, then the launchd agent or systemd units
/// written and loaded ([`blaze_api::units::install`]), so the system listens on the socket and
/// starts the daemon at the first connection. `blaze daemon uninstall`: the daemon stopped,
/// the units unloaded and removed.
#[cfg(unix)]
// dyn: the CLI's output writer, written a few times per command.
pub fn units(verb: DaemonVerb, err: &mut dyn std::io::Write, color: bool) -> anyhow::Result<i32> {
    use anyhow::Context as _;
    use blaze_api::units;
    let socket = crate::serve::socket()?;
    let place = units::place_here()?;
    let steps = match verb {
        DaemonVerb::Install => {
            let program = std::env::current_exe().context("finding blaze's own executable")?;
            let log = crate::serve::state()?.join("serve.log");
            let (unit, place) = units::here(program, socket.clone(), log)?;
            units::install(&unit, &place)?
        }
        DaemonVerb::Uninstall => match units::installed(&place, &socket)? {
            Installed::No => {
                writeln!(err, "no units of blaze's are installed in {}", place.dir.display())?;
                return Ok(0);
            }
            Installed::Yes => units::uninstall(&place, &socket),
            Installed::Elsewhere(theirs) => units::uninstall(&place, &theirs),
        },
    };
    // A daemon the CLI started holds the socket's path: stopped first, so the manager's socket
    // is the only one there, and its exit does not remove the manager's file.
    let stopped = stop(err, color)?;
    if stopped != 0 {
        return Ok(stopped);
    }
    units::run(&steps)?;
    match verb {
        DaemonVerb::Install => writeln!(
            err,
            "installed {}: the system starts the daemon at the first connection to {}",
            units::main_file(&place).display(),
            socket.display()
        )?,
        DaemonVerb::Uninstall => writeln!(err, "uninstalled the units in {}", place.dir.display())?,
    }
    Ok(0)
}

#[cfg(not(unix))]
// dyn: the CLI's output writer, written a few times per command.
pub fn units(_: DaemonVerb, _: &mut dyn std::io::Write, _: bool) -> anyhow::Result<i32> {
    anyhow::bail!("blaze daemon: socket activation is systemd's and launchd's, and this system has neither")
}

/// The daemon's [`Status`] for people.
pub fn status_lines(status: &Status) -> String {
    let ids: Vec<String> = status.requests.iter().map(u64::to_string).collect();
    let requests = match ids.is_empty() {
        true => "0".to_owned(),
        false => format!("{} ({})", ids.len(), ids.join(", ")),
    };
    format!(
        "pid: {}\nrequests: {requests}\nviews: {}\nmemory: {} of {}\n",
        status.pid,
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
pub(crate) mod tests;

#[cfg(test)]
mod refusals {
    use super::*;

    fn failed(kind: io::ErrorKind) -> ClientError {
        ClientError::Io(io::Error::new(kind, "it"))
    }

    #[test]
    fn only_a_socket_nothing_listens_at_is_no_daemon() {
        assert!(nothing_listens(&failed(io::ErrorKind::NotFound)));
        assert!(nothing_listens(&failed(io::ErrorKind::ConnectionRefused)));
        for kind in [io::ErrorKind::PermissionDenied, io::ErrorKind::InvalidInput, io::ErrorKind::TimedOut] {
            assert!(!nothing_listens(&failed(kind)), "{kind:?}");
        }
        assert!(!nothing_listens(&ClientError::Schema { theirs: 0 }));
    }

    #[test]
    fn a_refused_connect_names_the_socket_and_a_start_the_tries() {
        let socket = PathBuf::from("/run/blaze/v2.sock");
        let said = DaemonError::Connect { socket: socket.clone(), error: failed(io::ErrorKind::PermissionDenied) }.to_string();
        assert!(said.starts_with("connecting to blaze's daemon at /run/blaze/v2.sock: "), "{said}");
        let log = PathBuf::from("/c/blaze/v2/serve.log");
        let said = DaemonError::NotListening { socket, log, tries: 9001, error: failed(io::ErrorKind::ConnectionRefused) }.to_string();
        assert!(said.contains("nothing listens at /run/blaze/v2.sock after 10s"), "{said}");
        assert!(said.ends_with("(9001 connects tried)"), "{said}");
    }
}
