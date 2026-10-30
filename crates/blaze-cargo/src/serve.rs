//! `blaze serve` (docs/v2/plan/lanes/cargo/notes.md section 6): v2's daemon. One loop over the
//! platform's driver holds the API server, the session, the VFS, the database, the store and
//! the plane, and builds what the CLI and agents ask through them.
//!
//! - Its socket is `<home>/v2.sock`, `home` as v1's daemon has it ([`crate::cargo::daemon::home`]);
//!   `--socket PATH` names another, `--http ADDR` also listens on TCP with the bearer token in
//!   `BLAZE_API_TOKEN`.
//! - What it keeps lives in [`state`]: the VFS's journal, the database's image, the store and
//!   the plane's estimates. A start recovers the VFS from the journal and the database from the
//!   image; an image of another schema starts empty and says so on stderr.
//! - It stops on `blaze stop` ([`blaze_api::Request::Stop`]) or SIGTERM, writing its image first.

mod daemon;
mod fetch;
mod heads;
mod job;
mod mounts;
mod plane;
mod signals;
#[cfg(test)]
mod tests;
mod unit;
mod work;

use std::{
    ffi::OsString,
    net::SocketAddr,
    path::{Path, PathBuf},
    process::ExitCode,
    sync::Arc,
};

use anyhow::{Context, bail};
use blaze_core::{Confine, Driver, FileName};

pub use daemon::{Daemon, Options};

/// Where the daemon is and what it runs.
#[derive(Clone, Debug)]
pub struct Paths {
    /// Its state ([`state`]).
    pub state: PathBuf,
    pub socket: PathBuf,
    /// blaze's own executable: the sandbox's launcher.
    pub launcher: PathBuf,
}

/// Where the daemon's socket is: `<home>/v2.sock`.
pub fn socket() -> anyhow::Result<PathBuf> {
    Ok(crate::cargo::daemon::home()?.join("v2.sock"))
}

/// Where the daemon keeps what outlives it: `$BLAZE_HOME/v2`, else `blaze/v2` in the user's
/// cache directory (`XDG_CACHE_HOME`, or `~/.cache`). Unlike the socket it is never in
/// `XDG_RUNTIME_DIR`, which does not outlive the session.
pub fn state() -> anyhow::Result<PathBuf> {
    if let Some(home) = std::env::var_os("BLAZE_HOME").filter(|it| !it.is_empty()) {
        return Ok(PathBuf::from(home).join("v2"));
    }
    if let Some(caches) = std::env::var_os("XDG_CACHE_HOME").filter(|it| !it.is_empty()) {
        return Ok(PathBuf::from(caches).join("blaze/v2"));
    }
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"));
    Ok(PathBuf::from(home.context("no BLAZE_HOME, XDG_CACHE_HOME or HOME says where the daemon keeps its state")?)
        .join(".cache/blaze/v2"))
}

/// An absolute path as the driver names it: the driver's root is `/`.
pub fn file_name(path: &Path) -> anyhow::Result<FileName> {
    let text = path.to_str().with_context(|| format!("{} is not UTF-8", path.display()))?;
    let Some(rest) = text.strip_prefix('/') else { bail!("{text} is not an absolute path") };
    FileName::new(rest.trim_end_matches('/')).with_context(|| format!("{text} has an empty, `.` or `..` part"))
}

struct Args {
    socket: Option<PathBuf>,
    http: Option<SocketAddr>,
}

fn parse(args: impl IntoIterator<Item = OsString>) -> anyhow::Result<Args> {
    let mut parsed = Args { socket: None, http: None };
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        let arg = arg.into_string().map_err(|it| anyhow::anyhow!("{} is not UTF-8", it.to_string_lossy()))?;
        let (flag, inline) = match arg.split_once('=') {
            Some((flag, value)) if flag.starts_with("--") => (flag.to_owned(), Some(value.to_owned())),
            _ => (arg.clone(), None),
        };
        let mut value = || -> anyhow::Result<String> {
            match inline.clone() {
                Some(value) => Ok(value),
                None => args
                    .next()
                    .with_context(|| format!("{flag} takes a value"))?
                    .into_string()
                    .map_err(|it| anyhow::anyhow!("{} is not UTF-8", it.to_string_lossy())),
            }
        };
        match flag.as_str() {
            "--socket" => parsed.socket = Some(PathBuf::from(value()?)),
            "--http" => {
                let at = value()?;
                parsed.http = Some(at.parse().with_context(|| format!("--http {at} is no address and port"))?);
            }
            _ => bail!("blaze serve takes `--socket PATH` and `--http ADDR`, not `{arg}`"),
        }
    }
    Ok(parsed)
}

/// `blaze serve [--socket PATH] [--http ADDR]`.
pub fn main(args: impl IntoIterator<Item = OsString>) -> ExitCode {
    match serve(args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error:#}");
            ExitCode::FAILURE
        }
    }
}

fn serve(args: impl IntoIterator<Item = OsString>) -> anyhow::Result<()> {
    let args = parse(args)?;
    let socket = match args.socket {
        Some(socket) => std::path::absolute(&socket).with_context(|| format!("the socket {}", socket.display()))?,
        None => socket()?,
    };
    let state = state()?;
    std::fs::create_dir_all(&state).with_context(|| format!("making the daemon's state directory {}", state.display()))?;
    if let Some(parent) = socket.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("making the socket's directory {}", parent.display()))?;
    }
    let token = match args.http {
        Some(_) => Some(
            std::env::var("BLAZE_API_TOKEN")
                .ok()
                .filter(|it| !it.is_empty())
                .context("--http needs the bearer token its clients send in BLAZE_API_TOKEN")?,
        ),
        None => None,
    };
    let paths = Paths {
        state,
        socket,
        launcher: std::env::current_exe().context("the path of blaze itself, which launches the sandbox")?,
    };
    let confine: Arc<dyn Confine> = Arc::new(blaze_usercode::confine::Launcher { program: paths.launcher.clone() });
    let threads = std::thread::available_parallelism().map_or(4, |it| it.get());
    let options = Options {
        state: file_name(&paths.state)?,
        socket: file_name(&paths.socket)?,
        http: args.http.zip(token),
        budget: Some(blaze_graph::arena::machine_budget(1 << 30).context("the memory the daemon's tasks may hold")?),
        threads,
        io: 16,
        env: ["PATH", "HOME"]
            .into_iter()
            .filter_map(|name| Some((name.to_owned(), std::env::var(name).ok()?)))
            .collect(),
        host: crate::cargo::toolchain::host()
            .context("blaze has no target triple for this machine's architecture and system")?
            .to_owned(),
        color: false,
    };
    signals::catch();
    let steps: Arc<dyn work::Steps> = Arc::new(unit::Real::new(&paths).map_err(|why| anyhow::anyhow!(why))?);
    let scratch = paths.state.join("spawn");
    let runner = Box::new(plane::Threads::new(options.io));
    #[cfg(target_os = "linux")]
    {
        let driver = blaze_core::uring::Uring::new("/").context("starting io_uring")?;
        run(driver.confined(confine).scratch(scratch), options, steps, runner)
    }
    #[cfg(any(target_os = "macos", target_os = "freebsd", target_os = "openbsd", target_os = "netbsd"))]
    {
        let driver = blaze_core::kqueue::Kqueue::new("/", threads).context("starting kqueue")?;
        run(driver.confined(confine).scratch(scratch), options, steps, runner)
    }
    #[cfg(not(any(
        target_os = "linux",
        target_os = "macos",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd"
    )))]
    {
        let driver = blaze_core::threads::Threads::new("/", threads);
        run(driver.confined(confine).scratch(scratch), options, steps, runner)
    }
}

/// The loop over the platform's driver (io_uring on Linux, kqueue on macOS and the BSDs,
/// threads elsewhere), its root `/`, until it stops.
fn run<D: Driver>(
    driver: D,
    options: Options,
    steps: Arc<dyn work::Steps>,
    runner: Box<dyn plane::Runner<job::Value>>,
) -> anyhow::Result<()> {
    let mut daemon = Daemon::start(driver, options, steps, runner).map_err(|why| anyhow::anyhow!(why))?;
    eprintln!("blaze serve: listening on {}", daemon.listening());
    loop {
        if signals::signalled() {
            daemon.stop();
        }
        if !daemon.turn() {
            break;
        }
    }
    match daemon.failed() {
        Some(why) => bail!("{why}"),
        None => Ok(()),
    }
}
