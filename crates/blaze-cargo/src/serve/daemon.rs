//! The daemon's loop: one driver, and every state machine of the daemon stepped by it.
//!
//! - A completion goes to what asked for it, by its tag: the server, the VFS, the database, the
//!   store, the checkouts' watchers and cookies, the jobs (their lock writes, modes and user
//!   code), or the loop's own (the start, the estimates).
//! - What arrives goes through [`Heads`] (a request on a checkout waits for its watcher) to the
//!   session; what the session answers goes to the server. The VFS is flushed after every
//!   session call, so what a call committed is on its way to durability when it returns.
//! - The plane is stepped after every completion; what it did goes to the jobs, and what the
//!   jobs say to the session.
//! - Between requests the database and the VFS are collected, keeping each view's plan and
//!   every mount; after a cold build the image is written and the database settles onto it.
//! - A stop writes the image and the estimates, waits for the image to be mapped, then stops
//!   listening and hangs up.

#![forbid(unsafe_code)]

use std::{
    net::SocketAddr,
    sync::Arc,
    time::Duration,
};

use blaze_api::{
    Arrived, Reply, Request,
    server::{Config as ServerConfig, Http, Server},
    session::{self, Session},
};
use blaze_cas::Store;
use blaze_core::{Address, Completion, Done, Driver, Effect, FileName, Mapping, Tag};
use blaze_graph::{Budget, PlaneConfig, admission::Estimates, arena};
use blaze_query::{Database, DbError};
use blaze_vfs::{Ticket, Vfs};

use super::{
    heads::{Heads, Out},
    job::{Jobs, Said, Setup, Value, World},
    mounts::Mounts,
    plane::{Happened, Plane, Runner},
    work::Steps,
};

/// The tags of each part: `2^32` each, the watchers' and the runs' ranges of `2^32` after.
const OWN: u64 = 0;
const SERVER: u64 = 1 << 32;
const VFS: u64 = 2 << 32;
const DB: u64 = 3 << 32;
const STORE: u64 = 4 << 32;
const COOKIES: u64 = 5 << 32;
const JOBS: u64 = 6 << 32;
const JOBS_END: u64 = 7 << 32;
const WATCHERS: u64 = 1 << 40;
const RUNS: u64 = 1 << 50;
const RUNS_END: u64 = 1 << 62;

const STAT_JOURNAL: Tag = Tag(OWN);
const READ_JOURNAL: Tag = Tag(OWN + 1);
const STAT_IMAGE: Tag = Tag(OWN + 2);
const MAP_IMAGE: Tag = Tag(OWN + 3);
const STAT_ESTIMATES: Tag = Tag(OWN + 4);
const READ_ESTIMATES: Tag = Tag(OWN + 5);
const DIAL: Tag = Tag(OWN + 6);
const UNLINK: Tag = Tag(OWN + 7);
const HANG_UP: Tag = Tag(OWN + 8);
const WRITE_ESTIMATES: Tag = Tag(OWN + 9);

/// What a task of a kind never measured is taken to hold.
const UNSEEN: u64 = 64 << 20;
/// How long a stop waits for the driver's last effects once it hung up.
const LAST_EFFECTS: Duration = Duration::from_secs(2);

/// The session's view of the daemon for one call, borrowing only the parts it names.
macro_rules! cx {
    ($daemon:ident) => {
        session::Daemon {
            vfs: &mut $daemon.vfs,
            db: &$daemon.db,
            builder: &mut $daemon.jobs,
            memory_bytes: arena::held(),
            budget_bytes: $daemon.plane.budget().map_or(0, |it| it.bytes),
        }
    };
}

/// The jobs' view of the daemon for one call; the jobs themselves are left out of it.
macro_rules! world {
    ($daemon:ident, $said:ident) => {
        World {
            now: $daemon.driver.now(),
            vfs: &mut $daemon.vfs,
            db: &$daemon.db,
            store: &mut $daemon.store,
            plane: &mut $daemon.plane,
            mounts: &mut $daemon.mounts,
            heads: &$daemon.heads,
            steps: &$daemon.steps,
            effects: &mut $daemon.effects,
            said: &mut $said,
        }
    };
}

/// What the daemon starts with. No `Debug`: it holds the HTTP token.
pub struct Options {
    /// Its state directory ([`super::state`]), under the driver's root.
    pub state: FileName,
    pub socket: FileName,
    /// Where HTTP is listened on, and the bearer token its clients send.
    pub http: Option<(SocketAddr, String)>,
    pub budget: Option<Budget>,
    /// Workers of the plane: the cores.
    pub threads: usize,
    /// IO tasks run at once (fetches).
    pub io: usize,
    /// The daemon's `PATH` and `HOME`, an agent's invocation's whole environment.
    pub env: Vec<(String, String)>,
    pub host: String,
    pub color: bool,
}

fn under(dir: &FileName, name: &str) -> FileName {
    FileName::new(&format!("{dir}/{name}")).expect("a name under a directory is a name")
}

enum Stopping {
    /// The image and estimates are being written.
    Image,
    /// Hung up: the driver's last effects end.
    Hangup { at: Duration },
    Over,
}

pub struct Daemon<D: Driver> {
    driver: D,
    effects: Vec<Effect>,
    done: Vec<Completion>,
    listening: String,
    estimates_file: FileName,
    server: Server,
    session: Session,
    vfs: Vfs,
    db: Arc<Database>,
    /// Only the loop writes it; workers read it through [`Store::reader`], with no lock.
    store: Store,
    plane: Plane<Value>,
    mounts: Mounts,
    heads: Heads,
    jobs: Jobs,
    steps: Arc<dyn Steps>,
    replies: Vec<Reply>,
    spare: Spare,
    stopping: Option<Stopping>,
    /// A `blaze stop` arrived: the daemon stops once its answer is sent.
    stop_asked: bool,
    settling: bool,
    failed: Option<String>,
}

/// The buffers the loop's calls borrow and give back, so a warm turn allocates none: a call
/// made inside another takes one of its own, so each holds as many as calls nest.
#[derive(Default)]
struct Spare {
    said: Vec<Vec<Said>>,
    out: Vec<Vec<Out>>,
    arrived: Vec<Vec<Arrived>>,
    happened: Vec<Happened<Value>>,
}

/// What the start read, each once it is in.
#[derive(Default)]
struct Read {
    journal: Option<Vec<u8>>,
    image: Option<Option<Mapping>>,
    estimates: Option<Option<Vec<u8>>>,
    socket_free: bool,
}

impl<D: Driver> Daemon<D> {
    /// Recovers the VFS from its journal and the database from its image, opens the store,
    /// and listens. Fails naming what could not be read, or when another daemon listens.
    pub fn start(
        mut driver: D,
        options: Options,
        steps: Arc<dyn Steps>,
        runner: Box<dyn Runner<Value>>,
    ) -> Result<Daemon<D>, String> {
        let state = options.state.clone();
        let journal = under(&state, "journal");
        let db_dir = under(&state, "db");
        let image = under(&db_dir, "image");
        let estimates_file = under(&state, "estimates");
        let mut effects = vec![
            Effect::Stat { file: journal.clone(), tag: STAT_JOURNAL },
            Effect::Stat { file: image.clone(), tag: STAT_IMAGE },
            Effect::Stat { file: estimates_file.clone(), tag: STAT_ESTIMATES },
            Effect::Connect { at: Address::Unix(options.socket.clone()), tag: DIAL },
        ];
        let store_config = blaze_cas::Config { dir: under(&state, "store"), layers: Vec::new(), tags: STORE };
        let mut store = Store::open(store_config, &mut effects);
        let mut read = Read::default();
        let mut done = Vec::new();
        let socket = format!("/{}", options.socket);
        loop {
            let read_all = read.journal.is_some() && read.image.is_some() && read.estimates.is_some() && read.socket_free;
            if read_all && (store.ready() || store.broken().is_some()) {
                break;
            }
            driver.submit(&mut effects);
            driver.poll(&mut done, Duration::from_millis(50));
            for completion in done.drain(..) {
                if store.owns(completion.tag) {
                    store.complete(completion, &mut effects);
                    continue;
                }
                let Completion { tag, done } = completion;
                match (tag, done) {
                    (STAT_JOURNAL, Done::Stat(None)) => read.journal = Some(Vec::new()),
                    (STAT_JOURNAL, Done::Stat(Some(_))) => effects.push(Effect::ReadAll { file: journal.clone(), tag: READ_JOURNAL }),
                    (READ_JOURNAL, Done::Read(bytes)) => read.journal = Some(bytes),
                    (STAT_IMAGE, Done::Stat(None)) => read.image = Some(None),
                    (STAT_IMAGE, Done::Stat(Some(_))) => effects.push(Effect::Map { file: image.clone(), tag: MAP_IMAGE }),
                    (MAP_IMAGE, Done::Mapped(mapping)) => read.image = Some(Some(mapping)),
                    (STAT_ESTIMATES, Done::Stat(None)) => read.estimates = Some(None),
                    (STAT_ESTIMATES, Done::Stat(Some(_))) => {
                        effects.push(Effect::ReadAll { file: estimates_file.clone(), tag: READ_ESTIMATES });
                    }
                    (READ_ESTIMATES, Done::Read(bytes)) => read.estimates = Some(Some(bytes)),
                    (DIAL, Done::Connected { conn }) => {
                        effects.push(Effect::Hangup { conn, tag: HANG_UP });
                        driver.submit(&mut effects);
                        return Err(format!("another blaze serve listens on {socket}: `blaze stop` stops it"));
                    }
                    (DIAL, Done::Failed(_)) => effects.push(Effect::Remove { file: options.socket.clone(), tag: UNLINK }),
                    (UNLINK, Done::Removed | Done::Failed(_)) => read.socket_free = true,
                    (READ_JOURNAL, Done::Failed(error)) => return Err(format!("reading the VFS's journal /{journal}: {error}")),
                    (MAP_IMAGE, Done::Failed(error)) => return Err(format!("mapping the database's image /{image}: {error}")),
                    (READ_ESTIMATES, Done::Failed(error)) => {
                        return Err(format!("reading the plane's estimates /{estimates_file}: {error}"));
                    }
                    (STAT_JOURNAL | STAT_IMAGE | STAT_ESTIMATES, Done::Failed(error)) => {
                        return Err(format!("the daemon's state in /{state}: {error}"));
                    }
                    (tag, done) => return Err(format!("the start was answered {done:?} for tag {}", tag.0)),
                }
            }
        }
        if let Some(broken) = store.broken() {
            return Err(broken.to_string());
        }
        let (vfs, recovered) = Vfs::recover(journal.clone(), VFS, &read.journal.unwrap_or_default());
        if recovered.skipped > 0 || recovered.heads_dropped > 0 {
            eprintln!(
                "blaze serve: the journal /{journal} ended in {} bytes that were no whole record, and {} heads whose root was lost were dropped",
                recovered.skipped, recovered.heads_dropped
            );
        }
        let config = blaze_query::Config { tree: Some(Arc::new(vfs.reader())), dir: Some(db_dir), tags: DB };
        let mut db = match read.image.flatten() {
            None => Database::new(config),
            Some(mapping) => match Database::from_image_with(config.clone(), mapping) {
                Ok(db) => db,
                Err(DbError::Image(error)) => {
                    eprintln!("blaze serve: the database's image /{image} is not this blaze's ({error}): starting with an empty database");
                    Database::new(config)
                }
                Err(error) => return Err(format!("the database's image /{image}: {error}")),
            },
        };
        steps.register(&mut db);
        let estimates = match read.estimates.flatten() {
            None => Estimates::new(UNSEEN),
            Some(bytes) => Estimates::decode(&bytes).unwrap_or_else(|error| {
                eprintln!("blaze serve: the plane's estimates /{estimates_file} do not decode ({error:?}): starting from none");
                Estimates::new(UNSEEN)
            }),
        };
        let plane_config = PlaneConfig { io: options.io, work: options.threads.max(1), budget: options.budget, starve_after: 8 };
        let plane = Plane::new(plane_config, estimates, runner).map_err(|error| format!("the plane: {error}"))?;
        let mounts = Mounts::recover(&vfs);
        let mut listening = socket.clone();
        if let Some((at, _)) = &options.http {
            listening.push_str(&format!(" and http://{at}"));
        }
        let server_config = ServerConfig {
            unix: Some(options.socket.clone()),
            http: options.http.map(|(at, token)| Http { at, token }),
            tags: SERVER,
            most_connections: 1024,
        };
        let server = Server::new(server_config, &mut effects);
        let jobs = Jobs::new(Setup {
            views: format!("/{state}/views"),
            host: options.host,
            env: options.env,
            color: options.color,
            tags: JOBS,
            runs: RUNS,
            runs_end: RUNS_END,
        });
        Ok(Daemon {
            driver,
            effects,
            done,
            listening,
            estimates_file,
            server,
            session: Session::new(),
            vfs,
            db: Arc::new(db),
            store,
            plane,
            mounts,
            heads: Heads::new(COOKIES, WATCHERS, RUNS),
            jobs,
            steps,
            replies: Vec::new(),
            spare: Spare::default(),
            stopping: None,
            stop_asked: false,
            settling: false,
            failed: None,
        })
    }

    /// Where it listens, for people.
    pub fn listening(&self) -> &str {
        &self.listening
    }

    /// Why it stopped when it was not asked to.
    pub fn failed(&self) -> Option<&str> {
        self.failed.as_deref()
    }

    pub fn driver(&mut self) -> &mut D {
        &mut self.driver
    }

    /// The driver, the daemon let go: what a restart starts on.
    pub fn into_driver(self) -> D {
        self.driver
    }

    /// Whether no job runs and the database has settled onto the image of its last cold
    /// build.
    pub fn settled(&self) -> bool {
        self.jobs.idle() && !self.jobs.cold && !self.settling
    }

    pub fn vfs(&self) -> &Vfs {
        &self.vfs
    }

    pub fn db(&self) -> &Database {
        &self.db
    }

    pub fn mounts(&self) -> &Mounts {
        &self.mounts
    }

    /// Asks it to stop: the image first.
    pub fn stop(&mut self) {
        if self.stopping.is_some() {
            return;
        }
        self.stopping = Some(Stopping::Image);
        self.heads.stop(&mut self.effects);
        self.db.write_image(&mut self.effects);
        self.write_estimates();
        self.store.flush(&mut self.effects);
        self.vfs.flush(&mut self.effects);
    }

    fn write_estimates(&mut self) {
        let bytes = self.plane.estimates().encode();
        self.effects.push(Effect::Write { file: self.estimates_file.clone(), bytes, tag: WRITE_ESTIMATES });
    }

    /// One turn of the loop: what is asked submitted, what completed taken, the plane and the
    /// jobs stepped. `false` once it has stopped.
    pub fn turn(&mut self) -> bool {
        if matches!(self.stopping, Some(Stopping::Over)) {
            return false;
        }
        let wait = if !self.effects.is_empty() || self.plane.ready() {
            Duration::ZERO
        } else if self.plane.running() > 0 {
            Duration::from_millis(1)
        } else {
            Duration::from_millis(50)
        };
        self.driver.submit(&mut self.effects);
        let mut done = std::mem::take(&mut self.done);
        self.driver.poll(&mut done, wait);
        for completion in done.drain(..) {
            self.dispatch(completion);
        }
        self.done = done;
        self.step_plane();
        self.between();
        self.stepping_stop();
        self.driver.submit(&mut self.effects);
        !matches!(self.stopping, Some(Stopping::Over))
    }

    /// A completion to its part, by the block of tags it is in: compares on the tag alone,
    /// no part asked and no lock taken.
    fn dispatch(&mut self, completion: Completion) {
        match completion.tag.0 {
            tag if tag == WRITE_ESTIMATES.0 => {
                if let Done::Failed(error) = completion.done {
                    eprintln!("blaze serve: writing the plane's estimates /{}: {error}", self.estimates_file);
                }
            }
            SERVER..VFS => {
                let mut arrived = self.spare.arrived.pop().unwrap_or_default();
                self.server.complete(completion, &mut self.effects, &mut arrived);
                for it in arrived.drain(..) {
                    self.arrive(it);
                }
                self.spare.arrived.push(arrived);
                if let Some(why) = self.server.failed()
                    && self.failed.is_none()
                {
                    self.failed = Some(why.to_owned());
                    self.stop();
                }
            }
            VFS..DB => {
                let tickets = self.vfs.complete(completion, &mut self.effects);
                if !tickets.is_empty() {
                    self.durable(&tickets);
                }
            }
            DB..STORE => self.db.complete(completion, &mut self.effects),
            STORE..COOKIES => {
                debug_assert!(self.store.owns(completion.tag), "the store's tags are its");
                self.store.complete(completion, &mut self.effects);
                let placed = self.store.placed();
                let mut said = self.spare.said.pop().unwrap_or_default();
                self.jobs.placed(&mut world!(self, said), placed);
                self.said(said);
            }
            COOKIES..JOBS | WATCHERS..RUNS => {
                debug_assert!(self.heads.owns(completion.tag), "the checkouts' tags are theirs");
                let mut out = self.spare.out.pop().unwrap_or_default();
                self.heads.complete(&mut self.vfs, completion, &mut self.effects, &mut out);
                self.outs(out);
            }
            JOBS..JOBS_END | RUNS..RUNS_END => {
                debug_assert!(self.jobs.owns(completion.tag), "the jobs' tags are theirs");
                let mut said = self.spare.said.pop().unwrap_or_default();
                self.jobs.complete(&mut world!(self, said), completion);
                self.said(said);
            }
            _ => {}
        }
    }

    fn arrive(&mut self, arrived: Arrived) {
        let mut out = self.spare.out.pop().unwrap_or_default();
        self.heads.arrive(arrived, &mut self.effects, &mut out);
        self.outs(out);
    }

    fn outs(&mut self, mut out: Vec<Out>) {
        for it in out.drain(..) {
            match it {
                Out::Pass(arrived) => {
                    if matches!(arrived, Arrived::Request { request: Request::Stop, .. }) {
                        self.stop_asked = true;
                    }
                    let mut replies = std::mem::take(&mut self.replies);
                    self.session.arrive(&mut cx!(self), arrived, &mut replies);
                    self.replies = replies;
                    self.after_session();
                }
                Out::Answer(reply) => self.server.reply(reply, &mut self.effects),
            }
        }
        self.spare.out.push(out);
        if self.stop_asked {
            self.stop();
        }
    }

    fn durable(&mut self, tickets: &[Ticket]) {
        let mut replies = std::mem::take(&mut self.replies);
        self.session.durable(&mut cx!(self), tickets, &mut replies);
        self.replies = replies;
        let mut said = self.spare.said.pop().unwrap_or_default();
        self.jobs.durable(&mut world!(self, said), tickets);
        self.said(said);
        self.after_session();
    }

    /// After a session call: the VFS flushed, what the session asked of the build started,
    /// and the replies sent.
    fn after_session(&mut self) {
        self.vfs.flush(&mut self.effects);
        let mut said = self.spare.said.pop().unwrap_or_default();
        self.jobs.step(&mut world!(self, said));
        self.said(said);
        let mut replies = std::mem::take(&mut self.replies);
        for reply in replies.drain(..) {
            self.server.reply(reply, &mut self.effects);
        }
        self.replies = replies;
    }

    /// What the jobs said, told to the session.
    fn said(&mut self, mut said: Vec<Said>) {
        if said.is_empty() {
            self.spare.said.push(said);
            return;
        }
        let mut replies = std::mem::take(&mut self.replies);
        for it in said.drain(..) {
            match it {
                Said::Report(job, response) => self.session.report(&mut cx!(self), job, response, &mut replies),
                Said::Finish(job, ok, error) => self.session.finish(&mut cx!(self), job, ok, error, &mut replies),
            }
        }
        self.spare.said.push(said);
        self.replies = replies;
        self.after_session();
    }

    /// The plane's thread tasks that ended taken, and what it did given to the jobs, until it
    /// does nothing more.
    fn step_plane(&mut self) {
        let mut happened = std::mem::take(&mut self.spare.happened);
        let mut said = self.spare.said.pop().unwrap_or_default();
        self.jobs.apply_writes(&mut world!(self, said));
        self.said(said);
        loop {
            self.plane.poll(self.driver.now());
            self.plane.happened(&mut happened);
            if happened.is_empty() {
                break;
            }
            let mut said = self.spare.said.pop().unwrap_or_default();
            self.jobs.happened(&mut world!(self, said), &mut happened);
            self.said(said);
        }
        self.spare.happened = happened;
    }

    /// Between requests: the collections, and after a cold build the image and the settle.
    fn between(&mut self) {
        if self.stopping.is_some() || !self.jobs.idle() {
            return;
        }
        if self.jobs.ended {
            self.jobs.ended = false;
            self.db.collect(&[self.steps.plan_key()], &mut self.effects);
            self.vfs.collect(&self.mounts.roots());
            self.heads.sweep(&mut self.effects);
        }
        if self.jobs.cold && !self.settling {
            self.jobs.cold = false;
            self.settling = true;
            self.db.write_image(&mut self.effects);
            self.write_estimates();
        }
        if self.settling
            && self.db.settled()
            && let Some(db) = Arc::get_mut(&mut self.db)
        {
            self.settling = false;
            match db.settle(&mut self.effects) {
                Ok(_) => db.reclaim(),
                Err(error) => eprintln!("blaze serve: the database could not settle onto its image: {error}"),
            }
        }
    }

    fn stepping_stop(&mut self) {
        let now = self.driver.now();
        match self.stopping {
            Some(Stopping::Image) if self.db.settled() => {
                self.server.stop(&mut self.effects);
                self.stopping = Some(Stopping::Hangup { at: now });
            }
            Some(Stopping::Hangup { at }) if self.driver.in_flight() == 0 || now.saturating_sub(at) > LAST_EFFECTS => {
                self.stopping = Some(Stopping::Over);
            }
            _ => {}
        }
    }
}
