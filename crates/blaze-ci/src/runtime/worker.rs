//! A job's worker: its checkout, then its requests on a connection of its own to the daemon.
//!
//! The connection is a Unix stream split in two: this thread writes requests (and
//! [`Request::Cancel`]s, at any time), a reader thread cuts the responses into frames and hands
//! them back here. A request ends with its `Done`; when the daemon can't be reached or hangs
//! up, each request still running ends with an error naming the socket.

use std::{
    collections::BTreeMap,
    io::{Read, Write},
    net::Shutdown,
    os::unix::net::UnixStream,
    sync::{Arc, PoisonError, mpsc},
    thread,
};

use blaze_api::{
    ApiError, Envelope, Request, Response, ViewRef,
    client::Client,
    frames::{self, Frames},
};
use blaze_ids::ContentId;
use blaze_vfs::{Op, View};

use super::{EMPTY_HEAD, MOST_BATCH, Shared, api_error, batches, pins, said_or};
use crate::{
    Checkout,
    service::{Input, JobId, RunId},
};

pub(crate) enum Command {
    Checkout { commit: String, fetch: Vec<String>, from: Option<Checkout>, head: Option<String> },
    Send { run: RunId, request: Request },
    /// Cancel every request still running.
    Cancel,
    /// From the reader.
    Received(Envelope<Response>),
    /// From the reader: the connection ended.
    Lost(String),
    End,
}

/// Starts the job's worker; it ends at [`Command::End`].
pub(crate) fn start(job: JobId, shared: Arc<Shared>) -> mpsc::Sender<Command> {
    let (commands, receive) = mpsc::channel();
    let me = commands.clone();
    thread::spawn(move || Worker::new(job, shared, me).run(&receive));
    commands
}

struct Worker {
    job: JobId,
    shared: Arc<Shared>,
    me: mpsc::Sender<Command>,
    /// The writing half of the job's connection.
    stream: Option<UnixStream>,
    /// Ids increase over every connection the job makes, so a late frame of an old one can't be
    /// taken for a new request's.
    next_id: u64,
    running: BTreeMap<u64, RunId>,
}

impl Worker {
    fn new(job: JobId, shared: Arc<Shared>, me: mpsc::Sender<Command>) -> Worker {
        Worker { job, shared, me, stream: None, next_id: 1, running: BTreeMap::new() }
    }

    fn run(mut self, commands: &mpsc::Receiver<Command>) {
        while let Ok(command) = commands.recv() {
            match command {
                Command::Checkout { commit, fetch, from, head } => {
                    let result = checkout(&self.shared, &commit, &fetch, from.as_ref(), head.as_deref());
                    let _ = self.shared.service.send(Input::CheckedOut { job: self.job, result });
                }
                Command::Send { run, request } => match self.send(request) {
                    Ok(id) => {
                        self.running.insert(id, run);
                    }
                    Err(what) => self.fail(run, what),
                },
                Command::Cancel => {
                    let ids: Vec<u64> = self.running.keys().copied().collect();
                    for id in ids {
                        // A connection that broke is reported by the reader as `Lost`.
                        let _ = self.send(Request::Cancel { id });
                    }
                }
                Command::Received(envelope) => {
                    let Some(&run) = self.running.get(&envelope.id) else { continue };
                    let job = self.job;
                    match envelope.body {
                        Response::Done { ok } => {
                            self.running.remove(&envelope.id);
                            let _ = self.shared.service.send(Input::Ended { job, run, ok });
                        }
                        response => {
                            let _ = self.shared.service.send(Input::Response { job, run, response });
                        }
                    }
                }
                Command::Lost(why) => {
                    self.stream = None;
                    for (_, run) in std::mem::take(&mut self.running) {
                        self.fail(run, why.clone());
                    }
                }
                Command::End => break,
            }
        }
        if let Some(stream) = self.stream.take() {
            let _ = stream.shutdown(Shutdown::Both);
        }
    }

    /// Ends `run`'s request with `what`, at the daemon's socket.
    fn fail(&self, run: RunId, what: String) {
        let at = Some(self.shared.socket.display().to_string());
        let response = Response::Error(ApiError::new(what, at));
        let _ = self.shared.service.send(Input::Response { job: self.job, run, response });
        let _ = self.shared.service.send(Input::Ended { job: self.job, run, ok: false });
    }

    fn send(&mut self, request: Request) -> Result<u64, String> {
        let socket = self.shared.socket.display().to_string();
        if self.stream.is_none() {
            let stream = UnixStream::connect(&self.shared.socket)
                .map_err(|error| format!("could not reach the daemon at {socket}: {error}"))?;
            let reader = stream
                .try_clone()
                .map_err(|error| format!("the connection to the daemon at {socket}: {error}"))?;
            let me = self.me.clone();
            let named = socket.clone();
            thread::spawn(move || read(reader, &named, &me));
            self.stream = Some(stream);
        }
        let id = self.next_id;
        self.next_id += 1;
        let frame = frames::encode(&Envelope::new(id, request)).map_err(|error| api_error(&error))?;
        let stream = self.stream.as_mut().expect("connected");
        if let Err(error) = stream.write_all(&frame) {
            let _ = stream.shutdown(Shutdown::Both);
            self.stream = None;
            return Err(format!("sending to the daemon at {socket} failed: {error}"));
        }
        Ok(id)
    }
}

/// Reads the connection's responses until it ends.
fn read(mut stream: UnixStream, socket: &str, to: &mpsc::Sender<Command>) {
    let mut frames = Frames::new();
    let mut buffer = vec![0; 64 << 10];
    loop {
        let read = match stream.read(&mut buffer) {
            Ok(0) => {
                let _ = to.send(Command::Lost(format!("the daemon at {socket} hung up")));
                return;
            }
            Ok(read) => read,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => {
                let _ = to.send(Command::Lost(format!("reading from the daemon at {socket} failed: {error}")));
                return;
            }
        };
        frames.push(&buffer[..read]);
        loop {
            match frames.next() {
                Ok(Some(item)) => match frames::decode::<Response>(&item) {
                    Ok(envelope) => {
                        if to.send(Command::Received(envelope)).is_err() {
                            return;
                        }
                    }
                    Err(refused) => {
                        let what = format!("the daemon at {socket} sent {}", api_error(&refused.error));
                        let _ = stream.shutdown(Shutdown::Both);
                        let _ = to.send(Command::Lost(what));
                        return;
                    }
                },
                Ok(None) => break,
                Err(error) => {
                    let what = format!("the daemon at {socket} sent {}", api_error(&error));
                    let _ = stream.shutdown(Shutdown::Both);
                    let _ = to.send(Command::Lost(what));
                    return;
                }
            }
        }
    }
}

/// The commit's tree as a root held open on the pin connection: fetched, diffed from `from`
/// (the empty tree for `None`), applied to `from`'s root as what-ifs; with `head`, the same
/// ops committed to that head, which must reach the same root.
fn checkout(
    shared: &Shared,
    commit: &str,
    fetch: &[String],
    from: Option<&Checkout>,
    head: Option<&str>,
) -> Result<ContentId, String> {
    let token = shared
        .github
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .token()
        .map_err(|error| format!("minting the installation token to fetch {commit} failed: {error}"))?;
    let ops = {
        let git = shared.git.lock().unwrap_or_else(PoisonError::into_inner);
        git.fetch(fetch, Some(&token)).map_err(|error| {
            format!("fetching {} into {} failed: {error}", fetch.join(" "), git.dir.display())
        })?;
        git.ops(from.map(|it| it.commit.as_str()), commit).map_err(|error| {
            let from = from.map_or("the empty tree".to_owned(), |it| it.commit.clone());
            format!("diffing {from} to {commit} in {} failed: {error}", git.dir.display())
        })?
    };
    let base = match from {
        Some(from) => from.root,
        None => pin(shared, ViewRef::Head(EMPTY_HEAD.to_owned()))
            .map_err(|why| format!("opening the empty tree ({EMPTY_HEAD}) failed: {why}"))?,
    };
    let root = apply(shared, base, &ops);
    if from.is_none() {
        unpin(shared, base);
    }
    let root = root.map_err(|why| match from {
        Some(from) => format!("applying the diff from {} to root {:x} failed: {why}", from.commit, from.root),
        None => format!("writing the tree of {commit} failed: {why}"),
    })?;
    if let Some(head) = head
        && let Err(why) = commit_head(shared, head, commit, base, &ops, root)
    {
        unpin(shared, root);
        return Err(why);
    }
    Ok(root)
}

/// `ops` applied to `base` as what-ifs, a batch at a time; the result is held once, and nothing
/// in between is.
fn apply(shared: &Shared, base: ContentId, ops: &[Op]) -> Result<ContentId, String> {
    let mut at = base;
    let mut between = false;
    for batch in batches(ops, MOST_BATCH)? {
        let view = ViewRef::WhatIf { base: Box::new(ViewRef::Root(View { root: at })), ops: batch };
        let next = pin(shared, view);
        if between {
            unpin(shared, at);
        }
        at = next?;
        between = true;
    }
    Ok(at)
}

/// Commits `ops` to `head` from `base`, a batch at a time, and checks the head reached `root`:
/// one that was not at `base` is rebased to a tree that is not the commit's, an error.
fn commit_head(
    shared: &Shared,
    head: &str,
    commit: &str,
    base: ContentId,
    ops: &[Op],
    root: ContentId,
) -> Result<(), String> {
    let socket = shared.socket.display();
    let mut client = Client::unix(&shared.socket)
        .map_err(|error| format!("could not reach the daemon at {socket} to commit {head}: {error}"))?;
    let mut at = base;
    for batch in batches(ops, MOST_BATCH)? {
        let mut committed = None;
        let mut said = Vec::new();
        let ok = client
            .call(Request::Commit { head: head.to_owned(), base: at, ops: batch }, |response| match response {
                Response::Committed { root, .. } => committed = Some(*root),
                Response::Error(error) => said.push(api_error(error)),
                _ => {}
            })
            .map_err(|error| format!("committing {commit} to the head {head} at {socket}: {error}"))?;
        at = match (ok, committed) {
            (true, Some(root)) => root,
            _ => {
                return Err(format!(
                    "committing {commit} to the head {head} failed: {}",
                    said_or(&said, "the daemon said nothing more")
                ));
            }
        };
    }
    if at != root {
        return Err(format!(
            "the head {head} reached root {at:x} and the tree of {commit} is root {root:x}: the head \
             was not at root {base:x}, where the service last left it"
        ));
    }
    Ok(())
}

fn pin(shared: &Shared, view: ViewRef) -> Result<ContentId, String> {
    let (reply, answer) = mpsc::channel();
    shared
        .pins
        .send(pins::Command::Open { view, reply: Some(reply) })
        .map_err(|_| "the pin connection's thread ended".to_owned())?;
    answer.recv().map_err(|_| "the pin connection's thread ended".to_owned())?
}

fn unpin(shared: &Shared, root: ContentId) {
    let _ = shared.pins.send(pins::Command::Close { root });
}
