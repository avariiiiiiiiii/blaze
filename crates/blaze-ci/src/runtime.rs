//! Runs the [`Service`] with threads and channels:
//!
//! - the webhook listener ([`listen`]): deliveries checked, deduplicated and handed on;
//! - the pin connection ([`pins`]): one long-lived connection to the daemon holding every
//!   checkout's root open;
//! - a worker per job ([`worker`]): the checkout (fetch, diff, the what-if, the head), then the
//!   job's requests on a connection of its own, so cancelling one is a
//!   [`blaze_api::Request::Cancel`] on it;
//! - one GitHub thread posting check runs, retrying with backoff;
//! - the service's thread, the one [`serve`] runs on.
//!
//! The daemon is reached on its Unix socket only; nothing here writes the store or passes it a
//! secret. The installation token only fetches into the VM's clone.

mod listen;
mod pins;
mod worker;

#[cfg(test)]
mod tests;

use std::{
    collections::BTreeMap,
    convert::Infallible,
    fs,
    io::Write,
    net::TcpListener,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, PoisonError, mpsc},
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use blaze_api::{ApiError, Request, Response, ViewRef, client::Client};
use blaze_vfs::{Op, View};

use crate::{
    CheckRun,
    checkout::Git,
    config::{Config, Secret, SnapshotConfig},
    github::{App, Checks, GitHub, GitHubError, Key},
    service::{Input, JobId, Output, RunId, Service, Settings, State},
    snapshot,
};

/// The most bytes of ops one request carries, well under the daemon's
/// [`blaze_api::LONGEST_REQUEST`]; a larger change goes in several.
const MOST_BATCH: usize = 8 << 20;

/// What an op costs in a request besides its paths and bytes.
const OP_OVERHEAD: usize = 64;

/// How many times a GitHub call is tried again, waiting 1, 2, 4, 8 and 16 seconds.
const GITHUB_RETRIES: u32 = 5;

/// The head whose root is the empty tree: never written.
pub const EMPTY_HEAD: &str = "ci/empty";

/// What every thread shares.
pub(crate) struct Shared {
    pub(crate) socket: PathBuf,
    pub(crate) git: Mutex<Git>,
    pub(crate) github: Arc<Mutex<GitHub>>,
    pub(crate) pins: mpsc::Sender<pins::Command>,
    pub(crate) service: mpsc::Sender<Input>,
}

/// Runs the service until it can't: returns only with why it stopped.
pub fn serve(config: &Config) -> Result<Infallible, String> {
    let key = Secret::read(&config.app.key_file)
        .and_then(|it| it.text(&config.app.key_file))
        .map_err(|error| error.what)?;
    let key = Key::from_pem(&key).map_err(|error| format!("{}: {error}", config.app.key_file.display()))?;
    let app = App { id: config.app.id, installation: config.app.installation, key, api: config.app.api.clone() };
    let github = GitHub::new(app, config.repo.clone()).map_err(|error| error.to_string())?;
    let secret = Secret::read(&config.webhook.secret_file).map_err(|error| error.what)?;
    let state = load(&config.state)?;
    let listener = TcpListener::bind(config.webhook.listen)
        .map_err(|error| format!("the webhook could not listen on {}: {error}", config.webhook.listen))?;

    let (to_service, inputs) = mpsc::channel();
    let (to_pins, pin_commands) = mpsc::channel();
    let (to_github, posts) = mpsc::channel();
    let github = Arc::new(Mutex::new(github));
    let shared = Arc::new(Shared {
        socket: config.daemon.socket.clone(),
        git: Mutex::new(Git { dir: config.git.dir.clone(), remote: config.git.remote.clone() }),
        github: github.clone(),
        pins: to_pins,
        service: to_service.clone(),
    });

    {
        let socket = config.daemon.socket.clone();
        let service = to_service.clone();
        thread::Builder::new()
            .name("blaze-ci pins".to_owned())
            .spawn(move || pins::run(socket, pin_commands, service))
            .map_err(|error| format!("the pin thread did not start: {error}"))?;
    }
    {
        let service = to_service.clone();
        thread::Builder::new()
            .name("blaze-ci github".to_owned())
            .spawn(move || post(github, posts, service, Duration::from_secs(1)))
            .map_err(|error| format!("the GitHub thread did not start: {error}"))?;
    }
    {
        let service = to_service.clone();
        let repo = config.repo.clone();
        thread::Builder::new()
            .name("blaze-ci webhook".to_owned())
            .spawn(move || listen::run(listener, Arc::new(secret), repo, service))
            .map_err(|error| format!("the webhook thread did not start: {error}"))?;
    }
    log(&format!(
        "serving {} on {}, the daemon at {}",
        config.repo,
        config.webhook.listen,
        config.daemon.socket.display()
    ));

    let mut out = Vec::new();
    let mut service = Service::new(Settings::of(config), state, &mut out);
    let mut workers = BTreeMap::new();
    loop {
        for output in out.drain(..) {
            dispatch(config, &shared, &to_github, &mut workers, output);
        }
        let input = inputs.recv().map_err(|_| "every thread feeding the service ended".to_owned())?;
        service.step(now(), input, &mut out);
    }
}

/// A post to GitHub.
pub(crate) enum Post {
    Create { run: RunId, check: CheckRun },
    Update { run: RunId, id: u64, check: CheckRun },
}

fn dispatch(
    config: &Config,
    shared: &Arc<Shared>,
    to_github: &mpsc::Sender<Post>,
    workers: &mut BTreeMap<JobId, mpsc::Sender<worker::Command>>,
    output: Output,
) {
    match output {
        Output::Pin { root } => {
            let _ = shared.pins.send(pins::Command::Open { view: ViewRef::Root(View { root }), reply: None });
        }
        Output::Unpin { root } => {
            let _ = shared.pins.send(pins::Command::Close { root });
        }
        Output::Checkout { job, commit, fetch, from, head } => {
            let commands = worker::start(job, shared.clone());
            let _ = commands.send(worker::Command::Checkout { commit, fetch, from, head });
            workers.insert(job, commands);
        }
        Output::Send { job, run, request } => {
            let sent = workers.get(&job).is_some_and(|commands| {
                commands.send(worker::Command::Send { run, request }).is_ok()
            });
            if !sent {
                let what = format!("the worker of job {} is gone, so its request was not sent", job.0);
                let error = Response::Error(ApiError::new(what, None));
                let _ = shared.service.send(Input::Response { job, run, response: error });
                let _ = shared.service.send(Input::Ended { job, run, ok: false });
            }
        }
        Output::Cancel { job } => {
            if let Some(commands) = workers.get(&job) {
                let _ = commands.send(worker::Command::Cancel);
            }
        }
        Output::EndJob { job } => {
            if let Some(commands) = workers.remove(&job) {
                let _ = commands.send(worker::Command::End);
            }
        }
        Output::Create { run, check, .. } => {
            let _ = to_github.send(Post::Create { run, check });
        }
        Output::Update { run, id, check } => {
            let _ = to_github.send(Post::Update { run, id, check });
        }
        Output::Save(state) => {
            if let Err(what) = save(&config.state, &state) {
                log(&what);
            }
        }
        Output::Snapshot { name } => {
            let socket = config.daemon.socket.clone();
            let snapshot = config.snapshot.clone();
            let service = shared.service.clone();
            thread::spawn(move || {
                let _ = service.send(Input::Snapshotted(take_snapshot(&socket, &snapshot, &name)));
            });
        }
        Output::RemoveSnapshot { id } => {
            let snapshot = config.snapshot.clone();
            let service = shared.service.clone();
            thread::spawn(move || {
                let result = snapshot::remove(&snapshot, &id).map_err(|error| error.what);
                let _ = service.send(Input::Removed { id, result });
            });
        }
        Output::Log(line) => log(&line),
    }
}

/// The daemon's state made durable, then the VM's snapshot taken.
fn take_snapshot(socket: &Path, config: &SnapshotConfig, name: &str) -> Result<snapshot::Kept, String> {
    let mut client = Client::unix(socket)
        .map_err(|error| format!("could not reach the daemon at {}: {error}", socket.display()))?;
    let mut durable = false;
    let mut said = Vec::new();
    let ok = client
        .call(Request::Snapshot, |response| match response {
            Response::Snapshotted { .. } => durable = true,
            Response::Error(error) => said.push(api_error(error)),
            _ => {}
        })
        .map_err(|error| format!("asking the daemon at {} to snapshot: {error}", socket.display()))?;
    if !(ok && durable) {
        return Err(format!(
            "the daemon at {} did not make its state durable: {}",
            socket.display(),
            said_or(&said, "it said nothing more")
        ));
    }
    snapshot::take(config, name, now()).map_err(|error| error.what)
}

/// Posts check runs in order, each tried [`GITHUB_RETRIES`] times more with backoff from
/// `backoff`; a run that never posts is logged by name.
pub(crate) fn post(
    github: Arc<Mutex<impl Checks>>,
    posts: mpsc::Receiver<Post>,
    service: mpsc::Sender<Input>,
    backoff: Duration,
) {
    for post in posts {
        match post {
            Post::Create { run, check } => {
                let result = retry(backoff, || github.lock().unwrap_or_else(PoisonError::into_inner).create(&check));
                let result = result.map_err(|error| {
                    let what = format!(
                        "creating check run {} on {} failed {} times: {error}",
                        check.name,
                        check.commit,
                        GITHUB_RETRIES + 1
                    );
                    log(&what);
                    what
                });
                let _ = service.send(Input::Created { run, result });
            }
            Post::Update { run, id, check } => {
                let result =
                    retry(backoff, || github.lock().unwrap_or_else(PoisonError::into_inner).update(id, &check));
                if let Err(error) = result {
                    log(&format!(
                        "updating check run {} ({id}, run {}) on {} to {:?} failed {} times: {error}",
                        check.name,
                        run.0,
                        check.commit,
                        check.status,
                        GITHUB_RETRIES + 1
                    ));
                }
            }
        }
    }
}

fn retry<T>(backoff: Duration, mut attempt: impl FnMut() -> Result<T, GitHubError>) -> Result<T, GitHubError> {
    let mut wait = backoff;
    for _ in 0..GITHUB_RETRIES {
        match attempt() {
            Ok(it) => return Ok(it),
            Err(_) => {
                thread::sleep(wait);
                wait *= 2;
            }
        }
    }
    attempt()
}

/// `ops` cut into requests of at most `most` bytes each, in order; always at least one, maybe
/// empty. A file too large for one is written in pieces: a [`Op::Write`] of its start, then
/// [`Op::Edit`]s appending the rest.
pub(crate) fn batches(ops: &[Op], most: usize) -> Result<Vec<Vec<Op>>, String> {
    let mut batches = vec![Vec::new()];
    let mut size = 0;
    let mut add = |op: Op, cost: usize, batches: &mut Vec<Vec<Op>>| {
        if size + cost > most && batches.last().is_some_and(|it| !it.is_empty()) {
            batches.push(Vec::new());
            size = 0;
        }
        size += cost;
        batches.last_mut().expect("one at least").push(op);
    };
    for op in ops {
        match op {
            Op::Write { path, bytes } if path.len() + bytes.len() + OP_OVERHEAD > most => {
                let piece = most.checked_sub(path.len() + OP_OVERHEAD).filter(|it| *it > 0).ok_or_else(|| {
                    format!("the path {path} is too long for a request of the daemon's")
                })?;
                let mut pieces = bytes.chunks(piece);
                let first = pieces.next().unwrap_or_default().to_vec();
                let mut at = first.len() as u64;
                add(Op::Write { path: path.clone(), bytes: first }, most, &mut batches);
                for piece in pieces {
                    let edit = Op::Edit { path: path.clone(), at, len: 0, bytes: piece.to_vec() };
                    at += piece.len() as u64;
                    add(edit, most, &mut batches);
                }
            }
            Op::Write { path, bytes } | Op::Edit { path, bytes, .. } => {
                add(op.clone(), path.len() + bytes.len() + OP_OVERHEAD, &mut batches);
            }
            Op::Delete { path } => add(op.clone(), path.len() + OP_OVERHEAD, &mut batches),
            Op::Rename { from, to } => add(op.clone(), from.len() + to.len() + OP_OVERHEAD, &mut batches),
        }
    }
    Ok(batches)
}

/// The state file, or the empty state when there is none yet.
pub(crate) fn load(path: &Path) -> Result<State, String> {
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map_err(|error| format!("the state file {} is not one: {error}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(State::default()),
        Err(error) => Err(format!("could not read the state file {}: {error}", path.display())),
    }
}

/// Writes the state file whole or not at all: a temporary file beside it, synced, renamed over.
pub(crate) fn save(path: &Path, state: &State) -> Result<(), String> {
    let failed = |what: &str, error: &dyn std::fmt::Display| {
        format!("could not save the state to {}: {what}: {error}", path.display())
    };
    let bytes = serde_json::to_vec_pretty(state).map_err(|error| failed("encoding it", &error))?;
    let mut temp = path.as_os_str().to_owned();
    temp.push(".tmp");
    let temp = PathBuf::from(temp);
    let mut file = fs::File::create(&temp).map_err(|error| failed("creating the temporary file", &error))?;
    file.write_all(&bytes)
        .and_then(|()| file.sync_all())
        .map_err(|error| failed("writing the temporary file", &error))?;
    drop(file);
    fs::rename(&temp, path).map_err(|error| failed("renaming the temporary file over it", &error))?;
    if let Some(dir) = path.parent().filter(|it| !it.as_os_str().is_empty())
        && let Ok(dir) = fs::File::open(dir)
    {
        let _ = dir.sync_all();
    }
    Ok(())
}

pub(crate) fn api_error(error: &ApiError) -> String {
    match &error.at {
        Some(at) => format!("{} (at {at})", error.what),
        None => error.what.clone(),
    }
}

pub(crate) fn said_or(said: &[String], nothing: &str) -> String {
    if said.is_empty() { nothing.to_owned() } else { said.join("; ") }
}

pub(crate) fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |it| it.as_secs())
}

#[allow(clippy::print_stderr)]
pub(crate) fn log(line: &str) {
    eprintln!("blaze-ci: {line}");
}
