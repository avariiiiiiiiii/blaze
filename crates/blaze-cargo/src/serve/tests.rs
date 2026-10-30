//! The daemon on the simulation driver, with a build of fakes: a load that needs two sources
//! before it plans, a front end that compiles every unit at once, build scripts that end as
//! they start. Tasks run inline in a seeded order, so a run is a function of its seed.

#![forbid(unsafe_code)]

use std::{
    any::Any,
    cell::{Cell, RefCell},
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

use blaze_api::{
    ApiError, Artifact, Envelope, FetchVerb, Level, Reach, Request, Response, SCHEMA, Table, Targets, TestOutcome,
    TestResult, Ub, Verb, ViewRef,
    frames::{self, Frames},
};
use blaze_cas::Store;
use blaze_core::{
    Address, Completion, Conn, Effect, FileName, Tag,
    sim::{Faults, Rng, Sim},
};
use blaze_ids::ContentId;
use blaze_query::{Cancelled, Cx, Database, DbError, Query, QueryKey, QueryKind};
use blaze_units::{
    query::{BuildConfig, Cbor, Command, Fetch, FetchKind, Invocation, LockWrite, Origin, PackagePlan, Planned, UnitPlan, Units},
    schema::{DebugInfo, Mode, Profile, Strip, Stripped, Target, Unit, UnitDep, UnitGraph},
};
use blaze_vfs::{Change, Op, ViewId};

use super::{
    Daemon, Options,
    job::Value,
    plane::Inline,
    work::{Compiled, Failed, Front, Goal, JobInput, Runs, Selection, Source, SourceFile, Steps},
};

const SOCKET: &str = "run/blaze/v2.sock";
const STATE: &str = "cache/blaze/v2";
const FINISHED: &str = "`dev` profile [unoptimized + debuginfo]";
const LIBRARY: &str = "/rustc/0123abcd/library";
const DEP: &str = "/home/u/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/dep-1.0.0";
const INDEX: &str = "/home/u/.cargo/registry/index/index.crates.io-1949cf8c6b5b557f/.cache/de/p/dep";
/// How many turns a test gives the daemon before it says it hangs.
const TURNS: usize = 100_000;

fn name(it: &str) -> FileName {
    FileName::new(it).unwrap()
}

thread_local! {
    /// How many times the load ran on this thread (tasks run inline, on the test's).
    static PLANS: Cell<usize> = const { Cell::new(0) };
    /// The configuration of each run of the load.
    static SAW: RefCell<Vec<BuildConfig>> = const { RefCell::new(Vec::new()) };
}

fn needed() -> Vec<Fetch> {
    vec![
        Fetch {
            what: "crates.io index".into(),
            url: "https://index.crates.io/de/p/dep".into(),
            kind: FetchKind::IndexFile,
            at: INDEX.into(),
        },
        Fetch {
            what: "dep v1.0.0".into(),
            url: "https://static.crates.io/crates/dep/1.0.0/download".into(),
            kind: FetchKind::Crate { sha256: "ab".repeat(32) },
            at: DEP.into(),
        },
    ]
}

/// The load: the sources it lacks, else the units of the checkout's `app` on `dep`, with the
/// checkout's manifest as a warning so a test sees what it read.
struct FakePlan;

impl Query for FakePlan {
    const KIND: QueryKind = QueryKind(0x7e01);
    const NAME: &'static str = "fake plan";
    const PRODUCER: ContentId = ContentId([7; 32]);
    type Key = ();
    type Value = Cbor<Planned>;

    fn input(_: &()) -> ContentId {
        ContentId::of("blaze serve tests fake plan", b"")
    }

    fn execute(cx: &mut Cx<'_>, _: &()) -> Result<Cbor<Planned>, Cancelled> {
        PLANS.with(|it| it.set(it.get() + 1));
        let config = BuildConfig::decode(cx.config()?).expect("a view of the daemon has a build config");
        SAW.with(|it| it.borrow_mut().push(config.clone()));
        let manifest = cx.file("Cargo.toml")?.map(|it| String::from_utf8_lossy(it.bytes).into_owned()).unwrap_or_default();
        let missing: Vec<Fetch> =
            needed().into_iter().filter(|fetch| !config.mounts.iter().any(|it| it.at == fetch.at)).collect();
        if !missing.is_empty() {
            return Ok(Cbor(Planned::Needs(missing)));
        }
        Ok(Cbor(Planned::Ready(units(&config.invocation.root, manifest))))
    }
}

fn profile() -> Profile {
    Profile {
        name: "dev".into(),
        opt_level: "0".into(),
        lto: "false".into(),
        codegen_backend: None,
        codegen_units: None,
        debuginfo: DebugInfo::Level(2),
        split_debuginfo: None,
        debug_assertions: true,
        overflow_checks: true,
        rpath: false,
        incremental: false,
        panic: "unwind".into(),
        strip: Strip::Resolved(Stripped::None),
        rustflags: Vec::new(),
        trim_paths: None,
        hint_mostly_unused: None,
    }
}

fn unit(pkg_id: &str, kind: &str, mode: Mode, deps: &[usize]) -> Unit {
    Unit {
        pkg_id: pkg_id.into(),
        target: Target {
            kind: vec![kind.into()],
            crate_types: vec!["bin".into()],
            name: kind.into(),
            src_path: "src/main.rs".into(),
            edition: "2024".into(),
            required_features: None,
            doc: false,
            doctest: false,
            test: false,
        },
        profile: profile(),
        platform: None,
        mode,
        checked_as_test: false,
        features: Vec::new(),
        is_std: false,
        dependencies: deps
            .iter()
            .map(|&index| UnitDep {
                index,
                extern_crate_name: "dep".into(),
                public: false,
                noprelude: false,
                nounused: false,
            })
            .collect(),
    }
}

fn units(root: &str, manifest: String) -> Units {
    let app = format!("path+file://{root}#app@0.1.0");
    let dep = "registry+https://github.com/rust-lang/crates.io-index#dep@1.0.0".to_owned();
    let graph = UnitGraph {
        version: 1,
        units: vec![
            unit(&dep, "lib", Mode::Build, &[]),
            unit(&app, "custom-build", Mode::Build, &[]),
            unit(&app, "custom-build", Mode::RunCustomBuild, &[1]),
            unit(&app, "bin", Mode::Build, &[0, 2]),
        ],
        roots: vec![3],
    };
    let plan = UnitPlan {
        checked_as_test: false,
        metadata: "0".into(),
        extra_filename: String::new(),
        cfgs: Vec::new(),
        check_cfg: Vec::new(),
        rustflags: Vec::new(),
        lints: Vec::new(),
        env: Vec::new(),
        outputs: Vec::new(),
        out_dir: None,
        links: None,
    };
    let package = |id: &str, name: &str, version: &str, source: Option<String>, is_member| PackagePlan {
        id: id.into(),
        name: name.into(),
        version: version.into(),
        manifest_path: format!("{root}/Cargo.toml"),
        source,
        is_member,
    };
    Units {
        plans: vec![plan; graph.units.len()],
        graph,
        packages: vec![
            package(&dep, "dep", "1.0.0", Some("registry `crates-io`".into()), false),
            package(&app, "app", "0.1.0", Some(format!("({root})")), true),
        ],
        lock: Some(LockWrite { path: format!("{root}/Cargo.lock"), text: "# a lock\n".into() }),
        warnings: vec![manifest],
        target_dir: format!("{root}/target"),
        build_dir: format!("{root}/target"),
        finished: FINISHED.into(),
    }
}

/// The build of fakes; `fetched` counts each fetch by its `at`.
#[derive(Default)]
struct Fake {
    fetched: Mutex<BTreeMap<String, usize>>,
}

impl Steps for Fake {
    fn fetch(&self, fetch: &Fetch) -> Result<Source, ApiError> {
        *self.fetched.lock().unwrap().entry(fetch.at.clone()).or_default() += 1;
        let source = match fetch.kind {
            FetchKind::IndexFile => Source {
                files: vec![SourceFile { path: "dep".into(), bytes: b"{}\n".to_vec(), mode: blaze_vfs::Mode::Regular }],
                under: "dep".into(),
            },
            _ => Source {
                files: vec![SourceFile {
                    path: "Cargo.toml".into(),
                    bytes: format!("# {}\n", fetch.what).into_bytes(),
                    mode: blaze_vfs::Mode::Regular,
                }],
                under: String::new(),
            },
        };
        Ok(source)
    }

    fn library(&self) -> Result<(String, Source), ApiError> {
        let file = SourceFile { path: "core/src/lib.rs".into(), bytes: b"#![no_core]\n".to_vec(), mode: blaze_vfs::Mode::Regular };
        Ok((LIBRARY.into(), Source { files: vec![file], under: String::new() }))
    }

    fn lower(&self, job: &JobInput) -> Result<Arc<dyn Front>, Failed> {
        Ok(Arc::new(FakeFront { goal: job.goal }))
    }

    fn register(&self, db: &mut Database) {
        db.register::<FakePlan>();
    }

    fn plan(&self, db: &Database, view: ViewId) -> Result<Planned, Failed> {
        match db.get::<FakePlan>(view, &()) {
            Ok(planned) => Ok(planned.get()),
            Err(DbError::Cancelled) => Err(Failed::Again),
            Err(error) => Err(Failed::Error(ApiError::new(error.to_string(), None))),
        }
    }

    fn plan_key(&self) -> QueryKey {
        QueryKey { kind: FakePlan::KIND, input: FakePlan::input(&()) }
    }
}

struct FakeFront {
    goal: Goal,
}

/// User code that is over as it starts.
struct Over(Vec<Response>);

impl Runs for Over {
    fn start(&mut self, _: &Store, _: &mut Vec<Effect>, out: &mut Vec<Response>) -> Option<bool> {
        out.append(&mut self.0);
        Some(true)
    }

    fn owns(&self, _: Tag) -> bool {
        false
    }

    fn complete(&mut self, _: Completion, _: &mut Store, _: &mut Vec<Effect>, _: &mut Vec<Response>) -> Option<bool> {
        unreachable!("it asked for nothing")
    }

    fn cancel(&mut self, _: &mut Vec<Effect>) -> Option<bool> {
        Some(false)
    }
}

impl Front for FakeFront {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn fresh(&self, _: usize) -> bool {
        false
    }

    fn compile(&self, _: usize) -> Result<Compiled, Failed> {
        assert_ne!(self.goal, Goal::Reach, "a reach compiles nothing");
        Ok(Compiled { diagnostics: Vec::new(), ok: true, puts: Vec::new(), outputs: Vec::new(), artifact: None })
    }

    fn script(&self, _: usize, _: u64) -> Result<Box<dyn Runs>, ApiError> {
        Ok(Box::new(Over(Vec::new())))
    }

    fn tests(&self, _: &[Artifact], selection: Selection<'_>, _: u64) -> Result<Box<dyn Runs>, ApiError> {
        assert!(selection.ub.is_none(), "tests under the evaluator are evaluated, not run");
        Ok(Box::new(Over(Vec::new())))
    }

    fn ub_tests(&self, selection: Selection<'_>) -> Result<Vec<String>, Failed> {
        assert_eq!(self.goal, Goal::Check, "tests under the evaluator only check their units");
        let all = ["a::adds", "a::overflows", "b::reads"];
        Ok(all.iter().filter(|it| selection.filter.iter().all(|filter| it.contains(&**filter))).map(|it| (*it).to_owned()).collect())
    }

    fn evaluate(&self, test: &str, ub: Ub) -> Result<TestResult, Failed> {
        let (outcome, output) = match test {
            "a::overflows" => (TestOutcome::Failed, format!("undefined behavior\nseed: {}\n", ub.seed.unwrap_or(0)).into_bytes()),
            _ => (TestOutcome::Passed, Vec::new()),
        };
        Ok(TestResult { name: test.to_owned(), binary: "a".into(), millis: 0, output, outcome })
    }

    fn run(&self, _: &[Artifact], _: Option<&str>, _: &[String], _: u64) -> Result<Box<dyn Runs>, ApiError> {
        Ok(Box::new(Over(vec![Response::Ran { code: Some(0), stdout: b"hi\n".to_vec(), stderr: Vec::new() }])))
    }

    fn reach(&self, _: &dyn Front, changes: &[Change]) -> Result<Reach, Failed> {
        Ok(Reach { changes: changes.to_vec(), items: Vec::new(), compiled: Vec::new(), tests: Vec::new() })
    }

    fn codegen_inputs(&self) -> Result<Vec<(ContentId, ContentId)>, Failed> {
        Ok(Vec::new())
    }

    fn tables(&self) -> Result<Vec<Table>, Failed> {
        Ok(Vec::new())
    }
}

fn options() -> Options {
    Options {
        state: name(STATE),
        socket: name(SOCKET),
        http: None,
        budget: None,
        threads: 2,
        io: 2,
        env: vec![("PATH".into(), "/bin".into()), ("HOME".into(), "/home/u".into())],
        host: "x86_64-unknown-linux-gnu".into(),
        color: false,
    }
}

fn checkout(sim: &mut Sim, manifest: &str) {
    sim.change(&name("w/Cargo.toml"), manifest.as_bytes());
    sim.change(&name("w/src/main.rs"), b"fn main() {}\n");
}

fn start(sim: Sim, steps: &Arc<Fake>, seed: u64) -> Daemon<Sim> {
    let steps: Arc<dyn Steps> = steps.clone();
    Daemon::start(sim, options(), steps, Box::new(Inline::<Value>::new(seed))).unwrap_or_else(|why| panic!("{why}"))
}

struct Client {
    conn: Conn,
    frames: Frames,
    next: u64,
    got: Vec<(u64, Response)>,
}

fn dial(daemon: &mut Daemon<Sim>) -> Client {
    for _ in 0..TURNS {
        if let Ok(conn) = daemon.driver().dial(&Address::Unix(name(SOCKET))) {
            return Client { conn, frames: Frames::new(), next: 1, got: Vec::new() };
        }
        daemon.turn();
    }
    panic!("the daemon never listened");
}

impl Client {
    fn send(&mut self, daemon: &mut Daemon<Sim>, request: Request) -> u64 {
        let id = self.next;
        self.next += 1;
        let bytes = frames::encode(&Envelope { schema: SCHEMA, id, body: request }).unwrap();
        daemon.driver().peer_send(self.conn, &bytes);
        id
    }

    fn take(&mut self, daemon: &mut Daemon<Sim>) {
        self.frames.push(&daemon.driver().peer_received(self.conn));
        while let Some(item) = self.frames.next().unwrap() {
            let envelope: Envelope<Response> = frames::decode(&item).unwrap();
            self.got.push((envelope.id, envelope.body));
        }
    }

    fn done(&self, id: u64) -> bool {
        self.got.iter().any(|(at, it)| *at == id && matches!(it, Response::Done { .. }))
    }

    fn of(&self, id: u64) -> Vec<Response> {
        self.got.iter().filter(|(at, _)| *at == id).map(|(_, it)| it.clone()).collect()
    }

    /// Turns the daemon until request `id` is done; what it got.
    fn wait(&mut self, daemon: &mut Daemon<Sim>, id: u64) -> Vec<Response> {
        for _ in 0..TURNS {
            daemon.turn();
            self.take(daemon);
            if self.done(id) {
                return self.of(id);
            }
        }
        panic!("request {id} never ended; it got {:?}", self.of(id));
    }
}

fn agent() -> Targets {
    Targets {
        packages: Vec::new(),
        profile: String::new(),
        target: None,
        features: Vec::new(),
        all_features: false,
        no_default_features: false,
        cargo: None,
    }
}

fn warnings(responses: &[Response]) -> Vec<String> {
    responses
        .iter()
        .filter_map(|it| match it {
            Response::Diagnostic(diagnostic) if diagnostic.level == Level::Warning => Some(diagnostic.message.clone()),
            _ => None,
        })
        .collect()
}

fn turn_until(daemon: &mut Daemon<Sim>, mut what: impl FnMut(&mut Daemon<Sim>) -> bool) {
    for _ in 0..TURNS {
        if what(daemon) {
            return;
        }
        daemon.turn();
    }
    panic!("the daemon never got there");
}

/// Stops it as `blaze stop` would, and starts another on the same disk.
fn restart(mut daemon: Daemon<Sim>, steps: &Arc<Fake>, seed: u64) -> Daemon<Sim> {
    daemon.stop();
    let mut turns = 0;
    while daemon.turn() {
        turns += 1;
        assert!(turns < TURNS, "the daemon never stopped");
    }
    start(daemon.into_driver(), steps, seed)
}

#[test]
fn a_build_of_a_checkout_goes_from_the_server_to_built() {
    let mut sim = Sim::new(1, Faults::default());
    checkout(&mut sim, "[package]\nname = \"app\"\n");
    let steps = Arc::new(Fake::default());
    let mut daemon = start(sim, &steps, 1);
    let mut client = dial(&mut daemon);
    let id = client.send(&mut daemon, Request::Build { view: ViewRef::Head("/w".into()), targets: agent() });
    let got = client.wait(&mut daemon, id);

    assert_eq!(got.last(), Some(&Response::Done { ok: true }), "{got:#?}");
    assert!(got.contains(&Response::Fetched { verb: FetchVerb::Downloaded, what: "dep v1.0.0".into() }), "{got:#?}");
    assert!(got.contains(&Response::Fetched { verb: FetchVerb::Updating, what: "crates.io index".into() }), "{got:#?}");
    assert_eq!(warnings(&got), vec!["[package]\nname = \"app\"\n".to_owned()]);
    let units: Vec<(String, Verb)> = got
        .iter()
        .filter_map(|it| match it {
            Response::Unit(unit) => Some((unit.name.clone(), unit.verb)),
            _ => None,
        })
        .collect();
    assert_eq!(units.len(), 2, "one `Compiling` for each package: {units:?}");
    assert!(units.contains(&("dep".into(), Verb::Compiling)) && units.contains(&("app".into(), Verb::Compiling)));
    let built = got.iter().position(|it| matches!(it, Response::Built { profile, .. } if profile == FINISHED));
    let worked = got.iter().position(|it| matches!(it, Response::Work(_)));
    assert!(built.is_some() && built < worked, "`Built`, then `Work`: {got:#?}");
    assert_eq!(daemon.driver().contents(&name("w/Cargo.lock")), b"# a lock\n");
    assert!(daemon.mounts().has(DEP) && daemon.mounts().has(INDEX) && daemon.mounts().has(LIBRARY));
}

#[test]
fn a_request_on_a_checkout_sees_what_was_saved_before_it() {
    let mut sim = Sim::new(2, Faults::default());
    checkout(&mut sim, "first\n");
    let steps = Arc::new(Fake::default());
    let mut daemon = start(sim, &steps, 2);
    let mut client = dial(&mut daemon);
    let first = client.send(&mut daemon, Request::Check { view: ViewRef::Head("/w".into()), targets: agent() });
    assert_eq!(warnings(&client.wait(&mut daemon, first)), vec!["first\n".to_owned()]);
    for round in 0..8 {
        let saved = format!("saved before request {round}\n");
        daemon.driver().change(&name("w/Cargo.toml"), saved.as_bytes());
        let id = client.send(&mut daemon, Request::Check { view: ViewRef::Head("/w".into()), targets: agent() });
        let got = client.wait(&mut daemon, id);
        assert_eq!(warnings(&got), vec![saved], "{got:#?}");
    }
}

#[test]
fn the_mount_table_survives_a_restart() {
    let mut sim = Sim::new(3, Faults::default());
    checkout(&mut sim, "[package]\n");
    let steps = Arc::new(Fake::default());
    let mut daemon = start(sim, &steps, 3);
    let mut client = dial(&mut daemon);
    let id = client.send(&mut daemon, Request::Check { view: ViewRef::Head("/w".into()), targets: agent() });
    assert_eq!(client.wait(&mut daemon, id).last(), Some(&Response::Done { ok: true }));
    let before: Vec<_> = daemon.mounts().all().cloned().collect();
    assert_eq!(before.len(), 3);

    let mut daemon = restart(daemon, &steps, 4);
    assert_eq!(daemon.mounts().all().cloned().collect::<Vec<_>>(), before);
    let mut client = dial(&mut daemon);
    let id = client.send(&mut daemon, Request::Check { view: ViewRef::Head("/w".into()), targets: agent() });
    let got = client.wait(&mut daemon, id);
    assert_eq!(got.last(), Some(&Response::Done { ok: true }), "{got:#?}");
    assert!(!got.iter().any(|it| matches!(it, Response::Fetched { .. })), "nothing fetched again: {got:#?}");
    let fetched = steps.fetched.lock().unwrap().clone();
    assert!(fetched.values().all(|&it| it == 1), "each source fetched once: {fetched:?}");
}

#[test]
fn an_agents_targets_are_the_views_invocation() {
    let sim = Sim::new(5, Faults::default());
    let steps = Arc::new(Fake::default());
    let mut daemon = start(sim, &steps, 5);
    let mut client = dial(&mut daemon);
    let base = daemon.vfs().empty();
    let ops = vec![Op::Write { path: "Cargo.toml".into(), bytes: b"agent\n".to_vec(), mode: blaze_vfs::Mode::Regular }];
    let commit = client.send(&mut daemon, Request::Commit { head: "agents/one".into(), base, ops });
    let root = match client.wait(&mut daemon, commit).first() {
        Some(Response::Committed { root, .. }) => *root,
        other => panic!("the commit answered {other:?}"),
    };
    let targets = Targets {
        packages: vec!["app".into()],
        profile: "release".into(),
        target: None,
        features: vec!["x".into()],
        all_features: false,
        no_default_features: true,
        cargo: None,
    };
    SAW.with(|it| it.borrow_mut().clear());
    let id = client.send(&mut daemon, Request::Build { view: ViewRef::Head("agents/one".into()), targets });
    let got = client.wait(&mut daemon, id);
    assert_eq!(got.last(), Some(&Response::Done { ok: true }), "{got:#?}");
    assert_eq!(warnings(&got), vec!["agent\n".to_owned()]);

    let at = format!("/{STATE}/views/{root:x}");
    let config = SAW.with(|it| it.borrow().last().cloned()).expect("the load ran");
    let args = ["-p", "app", "--profile", "release", "-F", "x", "--no-default-features"];
    assert_eq!(
        config.invocation,
        Invocation {
            command: Command::Build,
            args: args.iter().map(|it| (*it).to_owned()).collect(),
            root: at.clone(),
            cwd: at.clone(),
            env: options().env,
            files: Vec::new(),
        }
    );
    assert_eq!(config.library, LIBRARY);
    assert_eq!(config.host, "x86_64-unknown-linux-gnu");
    let view = config.mounts.iter().find(|it| it.origin == Origin::View).expect("the view is mounted");
    assert_eq!(view.at, at);
    assert!(config.mounts.windows(2).all(|it| it[0].at <= it[1].at), "mounts sorted by where");
    assert_eq!(daemon.driver().contents(&FileName::new(&format!("{}/Cargo.lock", &at[1..])).unwrap()), b"# a lock\n");
}

#[test]
fn a_start_from_the_image_runs_no_load_again() {
    let mut sim = Sim::new(6, Faults::default());
    checkout(&mut sim, "[package]\n");
    let steps = Arc::new(Fake::default());
    let mut daemon = start(sim, &steps, 6);
    let mut client = dial(&mut daemon);
    let id = client.send(&mut daemon, Request::Check { view: ViewRef::Head("/w".into()), targets: agent() });
    assert_eq!(client.wait(&mut daemon, id).last(), Some(&Response::Done { ok: true }));
    turn_until(&mut daemon, |it| it.settled());
    assert!(daemon.driver().exists(&name(&format!("{STATE}/db/image"))), "the cold build wrote the image");

    let mut daemon = restart(daemon, &steps, 7);
    PLANS.with(|it| it.set(0));
    let mut client = dial(&mut daemon);
    let id = client.send(&mut daemon, Request::Check { view: ViewRef::Head("/w".into()), targets: agent() });
    assert_eq!(client.wait(&mut daemon, id).last(), Some(&Response::Done { ok: true }));
    assert_eq!(PLANS.with(Cell::get), 0, "the load's result came from the image");
}

#[test]
fn a_run_reports_what_ran_after_built() {
    let mut sim = Sim::new(8, Faults::default());
    checkout(&mut sim, "[package]\n");
    let steps = Arc::new(Fake::default());
    let mut daemon = start(sim, &steps, 8);
    let mut client = dial(&mut daemon);
    let id = client.send(
        &mut daemon,
        Request::Run { view: ViewRef::Head("/w".into()), targets: agent(), bin: None, args: Vec::new() },
    );
    let got = client.wait(&mut daemon, id);
    let at = |what: fn(&Response) -> bool| got.iter().position(what);
    let built = at(|it| matches!(it, Response::Built { .. }));
    let ran = at(|it| matches!(it, Response::Ran { code: Some(0), .. }));
    let worked = at(|it| matches!(it, Response::Work(_)));
    assert!(built.is_some() && built < ran && ran < worked, "{got:#?}");
    assert_eq!(got.last(), Some(&Response::Done { ok: true }));
}

#[test]
fn tests_under_the_evaluator_are_each_evaluated_and_one_failing_fails_the_job() {
    let mut sim = Sim::new(9, Faults::default());
    checkout(&mut sim, "[package]\n");
    let steps = Arc::new(Fake::default());
    let mut daemon = start(sim, &steps, 9);
    let mut client = dial(&mut daemon);
    let test = |filter: &str| Request::Test {
        view: ViewRef::Head("/w".into()),
        targets: agent(),
        filter: vec![filter.to_owned()],
        all: true,
        args: Vec::new(),
        ub: Some(Ub { seed: Some(3), schedules: None }),
    };
    let results = |got: &[Response]| -> Vec<(String, TestOutcome)> {
        let mut results: Vec<_> = got
            .iter()
            .filter_map(|it| match it {
                Response::TestResult(result) => Some((result.name.clone(), result.outcome)),
                _ => None,
            })
            .collect();
        results.sort();
        results
    };
    let id = client.send(&mut daemon, test("a::"));
    let got = client.wait(&mut daemon, id);
    assert_eq!(
        results(&got),
        vec![("a::adds".to_owned(), TestOutcome::Passed), ("a::overflows".to_owned(), TestOutcome::Failed)],
        "{got:#?}"
    );
    let failed = got.iter().find_map(|it| match it {
        Response::TestResult(result) if result.outcome == TestOutcome::Failed => Some(result.output.clone()),
        _ => None,
    });
    assert_eq!(failed.as_deref(), Some(&b"undefined behavior\nseed: 3\n"[..]));
    assert_eq!(got.last(), Some(&Response::Done { ok: false }));
    let id = client.send(&mut daemon, test("b::"));
    let got = client.wait(&mut daemon, id);
    assert_eq!(results(&got), vec![("b::reads".to_owned(), TestOutcome::Passed)], "{got:#?}");
    assert_eq!(got.last(), Some(&Response::Done { ok: true }));
}

/// What one client asked and how it ended.
struct Asked {
    client: usize,
    id: u64,
    cancel_at: Option<usize>,
}

/// Every request answered by exactly one `Done`, nothing of it after, an `Error` only right
/// before its `Done`.
fn keeps_the_contract(got: &[(u64, Response)], ids: &[u64]) {
    for &id in ids {
        let of: Vec<&Response> = got.iter().filter(|(at, _)| *at == id).map(|(_, it)| it).collect();
        let dones = of.iter().filter(|it| matches!(it, Response::Done { .. })).count();
        assert_eq!(dones, 1, "request {id} got {dones} `Done`s: {of:#?}");
        assert!(matches!(of.last(), Some(Response::Done { .. })), "request {id} got something after its `Done`: {of:#?}");
        for (at, it) in of.iter().enumerate() {
            if matches!(it, Response::Error(_)) {
                assert_eq!(at + 2, of.len(), "request {id}'s error is not right before its `Done`: {of:#?}");
            }
        }
    }
    let known: Vec<u64> = got.iter().map(|(id, _)| *id).collect();
    assert!(known.iter().all(|it| ids.contains(it)), "a reply to a request no one made");
}

#[test]
fn every_request_ends_once_over_many_seeds() {
    for seed in 0..48 {
        let mut rng = Rng::new(seed);
        let faults = Faults { max_latency: std::time::Duration::from_millis(rng.below(4)), ..Faults::default() };
        let mut sim = Sim::new(seed, faults);
        checkout(&mut sim, "[package]\n");
        let steps = Arc::new(Fake::default());
        let mut daemon = start(sim, &steps, seed);
        let mut clients = vec![dial(&mut daemon), dial(&mut daemon), dial(&mut daemon)];
        let gone = rng.below(3) as usize;
        let hang_up_at = if rng.one_in(2) { Some(rng.below(400) as usize) } else { None };
        let mut asked = Vec::new();
        for _ in 0..6 {
            let client = rng.below(3) as usize;
            let view = match rng.below(3) {
                0 => ViewRef::Head("/w".into()),
                1 => ViewRef::WhatIf {
                    base: Box::new(ViewRef::Head("/w".into())),
                    ops: vec![Op::Write { path: "Cargo.toml".into(), bytes: b"what if\n".to_vec(), mode: blaze_vfs::Mode::Regular }],
                },
                _ => ViewRef::Head("agents/none".into()),
            };
            let request = match rng.below(4) {
                0 => Request::Check { view, targets: agent() },
                1 => Request::Build { view, targets: agent() },
                2 => Request::Test { view, targets: agent(), filter: Vec::new(), all: false, args: Vec::new(), ub: None },
                _ => Request::Run { view, targets: agent(), bin: None, args: Vec::new() },
            };
            let id = clients[client].send(&mut daemon, request);
            let cancel_at = rng.one_in(3).then(|| rng.below(300) as usize);
            asked.push(Asked { client, id, cancel_at });
        }
        let mut cancels: Vec<(usize, u64)> = Vec::new();
        let mut hung_up = false;
        for turn in 0..TURNS {
            daemon.turn();
            for it in &mut clients {
                it.take(&mut daemon);
            }
            if hang_up_at == Some(turn) && !hung_up {
                hung_up = true;
                let conn = clients[gone].conn;
                daemon.driver().peer_hangup(conn);
            }
            for it in &asked {
                if it.cancel_at == Some(turn) && !(hung_up && it.client == gone) {
                    let id = clients[it.client].send(&mut daemon, Request::Cancel { id: it.id });
                    cancels.push((it.client, id));
                }
            }
            let live = |client: usize| !(hung_up && client == gone);
            let over = asked.iter().filter(|it| live(it.client)).all(|it| clients[it.client].done(it.id))
                && cancels.iter().filter(|it| live(it.0)).all(|(client, id)| clients[*client].done(*id));
            if over && turn > hang_up_at.unwrap_or(0) && turn > 300 {
                break;
            }
        }
        for _ in 0..500 {
            daemon.turn();
            for it in &mut clients {
                it.take(&mut daemon);
            }
        }
        for (at, client) in clients.iter().enumerate() {
            if hung_up && at == gone {
                continue;
            }
            let mut ids: Vec<u64> = asked.iter().filter(|it| it.client == at).map(|it| it.id).collect();
            ids.extend(cancels.iter().filter(|it| it.0 == at).map(|it| it.1));
            keeps_the_contract(&client.got, &ids);
        }
        turn_until(&mut daemon, |it| it.settled());
    }
}
