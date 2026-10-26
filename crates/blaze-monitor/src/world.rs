//! The world: one thread owns it, applies the events it is sent and steps it, which is when
//! the REST server answers.
//!
//! What it holds (module `blaze`; `docs/monitor.md` has the queries):
//!
//! - `workspaces::<name>`: [`Workspace`].
//! - `builds::b<n>`: [`Build`], `Succeeded` or `Failed`, `ByDaemon` or `ByCargo`,
//!   `(In, workspace)`.
//! - `programs::<name>`: [`Program`], `(In, workspace)`.
//! - `patches::p<n>`: [`Patch`] (a patch, an object, or a refusal and why), `Refused` when
//!   the worker did not build it, `(PatchOf, program)`, `(Of, build)` once the build that
//!   asked for it is known.
//! - `processes::<pid>`: [`Process`], `(Runs, program)`.
//! - `live::l<n>`: [`LivePatch`], `Sent` or `Restart`, `(PatchOf, program)`,
//!   `(PatchedInto, process)`, `(Of, patch)`.
//! - `crates::<package>` and `tests::<target>`: [`TestTarget`] of a test executable's last
//!   run, `(TestOf, crate)`, `(Of, build)`; under it each test, [`Test`], with its last
//!   outcome (`Passed`, `Failed`, `Ignored`, `Unknown`) and why it ran or not this time
//!   (`Changed`, `Skipped`, `Always`, `Sandboxed`, `Every`), `(TestOf, target)`.
//! - the singleton [`Totals`], which observers keep: the counts since the daemon started.

use std::{collections::VecDeque, time::Duration};

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender};
use flecs_ecs::prelude::*;

use crate::event::{self, By, Event, Outcome, Picked};

/// How many builds, patches and live edits are kept; the oldest go first.
const KEEP: usize = 1000;

/// How long the thread waits for an event before it steps the world, for REST to answer.
const STEP: Duration = Duration::from_millis(20);

/// How far after a build's end a patch it asked for may be said to end: the worker's clock
/// stops after the client's.
const SLACK_US: u64 = 200_000;

#[derive(Component)]
struct Blaze;

#[derive(Component, Debug, Default, Clone)]
#[flecs(meta)]
struct Workspace {
    root: String,
}

#[derive(Component, Debug, Default, Clone)]
#[flecs(meta)]
struct Build {
    command: String,
    by: String,
    why: String,
    pid: u32,
    at_us: u64,
    took_us: u64,
}

#[derive(Component, Debug, Default, Clone)]
#[flecs(meta)]
struct Program {
    binary: String,
    krate: String,
}

#[derive(Component, Debug, Default, Clone)]
#[flecs(meta)]
struct Patch {
    kind: String,
    functions: u64,
    files: u64,
    why: String,
    at_us: u64,
    took_us: u64,
}

#[derive(Component, Debug, Default, Clone)]
#[flecs(meta)]
struct LivePatch {
    outcome: String,
    why: String,
    sequence: u64,
    functions: u64,
    statics: u64,
    at_us: u64,
    took_us: u64,
}

#[derive(Component, Debug, Default, Clone)]
#[flecs(meta)]
struct Process {
    pid: u32,
    executable: String,
}

#[derive(Component, Debug, Default, Clone)]
#[flecs(meta)]
struct TestTarget {
    package: String,
    kind: String,
    executable: String,
    selection: String,
    total: u64,
    ran: u64,
    failed: u64,
    status: i32,
    at_us: u64,
    took_us: u64,
}

#[derive(Component, Debug, Default, Clone)]
#[flecs(meta)]
struct Test {
    name: String,
    why: String,
    path: String,
    /// When it last ran, and from its executable's start to its result line then.
    at_us: u64,
    ended_us: u64,
}

/// Counts since the daemon started, kept by observers.
#[derive(Component, Debug, Default, Clone)]
#[flecs(meta)]
struct Totals {
    builds: u64,
    by_daemon: u64,
    by_cargo: u64,
    errors: u64,
    patches: u64,
    objects: u64,
    refused: u64,
    live_sent: u64,
    live_restarts: u64,
    tests_passed: u64,
    tests_failed: u64,
    tests_skipped: u64,
}

macro_rules! tags {
    ($($name:ident),* $(,)?) => {
        $(#[derive(Component)] struct $name;)*
        fn register_tags(world: &World) {
            $(world.component::<$name>();)*
        }
    };
}

tags!(
    // builds and tests
    Succeeded,
    Failed,
    ByDaemon,
    ByCargo,
    // patches and live edits
    Refused,
    Sent,
    Restart,
    // a test's last outcome
    Passed,
    Ignored,
    Unknown,
    // why a test ran or not
    Changed,
    Skipped,
    Always,
    Sandboxed,
    Every,
    // relations
    In,
    Of,
    PatchOf,
    PatchedInto,
    Runs,
    TestOf,
);

impl Module for Blaze {
    fn module(world: &World) {
        // Rooted, for the names to be `blaze.Build` whatever the module's crate is called.
        world.module::<Blaze>("::blaze");
        world.component::<Workspace>();
        world.component::<Build>();
        world.component::<Program>();
        world.component::<Patch>();
        world.component::<LivePatch>();
        world.component::<Process>();
        world.component::<TestTarget>();
        world.component::<Test>();
        world.component::<Totals>();
        register_tags(world);
    }
}

/// Builds the world, says on `ready` which port REST serves on, and applies events until
/// every sender is gone.
pub(crate) fn run(events: &Receiver<Event>, port: Option<u16>, ready: &Sender<Option<u16>>) {
    let world = World::new();
    world.import::<Blaze>();
    world.set(Totals::default());
    observe(&world);
    if let Some(port) = port {
        world.set(flecs::rest::Rest {
            port,
            ipaddr: c"127.0.0.1".as_ptr() as *mut _,
            impl_: core::ptr::null_mut(),
        });
    }
    world.progress();
    let _ = ready.send(port);
    let mut mirror = Mirror::default();
    loop {
        match events.recv_timeout(STEP) {
            Ok(event) => {
                mirror.apply(&world, event);
                for event in events.try_iter() {
                    mirror.apply(&world, event);
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
        world.progress();
    }
}

/// The observers: each change to the world is an event to them, and these count.
fn observe(world: &World) {
    world.observer::<flecs::OnSet, &Build>().each_entity(|e, build| {
        let by = build.by.clone();
        e.world().get::<&mut Totals>(|totals| {
            totals.builds += 1;
            match by.as_str() {
                "daemon" => totals.by_daemon += 1,
                "cargo" => totals.by_cargo += 1,
                _ => totals.errors += 1,
            }
        });
    });
    world.observer::<flecs::OnSet, &Patch>().each_entity(|e, patch| {
        let kind = patch.kind.clone();
        e.world().get::<&mut Totals>(|totals| match kind.as_str() {
            "patch" => totals.patches += 1,
            "object" => totals.objects += 1,
            _ => totals.refused += 1,
        });
    });
    world.observer::<flecs::OnSet, &LivePatch>().each_entity(|e, live| {
        let sent = live.outcome == "sent";
        e.world().get::<&mut Totals>(|totals| match sent {
            true => totals.live_sent += 1,
            false => totals.live_restarts += 1,
        });
    });
    let count = |world: &World, tag: Entity, add: fn(&mut Totals)| {
        world.observer::<flecs::OnAdd, ()>().with(tag).with(Test::id()).each_entity(
            move |e, ()| {
                e.world().get::<&mut Totals>(add);
            },
        );
    };
    count(world, world.component::<Passed>().id(), |it| it.tests_passed += 1);
    count(world, world.component::<Failed>().id(), |it| it.tests_failed += 1);
    count(world, world.component::<Skipped>().id(), |it| it.tests_skipped += 1);
}

/// What the world's thread remembers to link what comes in to what came before, and to
/// drop the oldest.
#[derive(Default)]
struct Mirror {
    next: u64,
    builds: VecDeque<Seen>,
    patches: VecDeque<Seen>,
    live: VecDeque<Entity>,
}

/// A build or a patch: when it ran, where, and for a patch whether its build is known.
struct Seen {
    entity: Entity,
    workspace: String,
    from_us: u64,
    to_us: u64,
    program: Option<Entity>,
    linked: bool,
}

impl Mirror {
    fn apply(&mut self, world: &World, event: Event) {
        self.next += 1;
        match event {
            Event::Build(build) => self.build(world, build),
            Event::Compiled(compiled) => self.compiled(world, compiled),
            Event::Live(live) => self.live(world, live),
            Event::Tests(tests) => self.tests(world, &tests),
        }
    }

    fn build(&mut self, world: &World, build: event::Build) {
        let entity = world.entity_named(&format!("builds::b{}", self.next));
        let by = match build.by {
            By::Daemon => "daemon",
            By::Cargo => "cargo",
            By::Error => "error",
        };
        match build.by {
            By::Daemon => entity.add(ByDaemon),
            By::Cargo => entity.add(ByCargo),
            By::Error => entity,
        };
        match build.succeeded {
            true => entity.add(Succeeded),
            false => entity.add(Failed),
        };
        entity.add((In, workspace(world, &build.workspace)));
        entity.set(Build {
            command: build.command,
            by: by.to_owned(),
            why: build.why,
            pid: build.pid,
            at_us: build.at_us,
            took_us: build.took_us,
        });
        let seen = Seen {
            entity: entity.id(),
            workspace: build.workspace,
            from_us: build.at_us,
            to_us: build.at_us + build.took_us,
            program: None,
            linked: true,
        };
        // The patches a build asked for end before the client hears of them.
        for patch in self.patches.iter_mut().filter(|it| !it.linked && seen.holds(it)) {
            world.entity_from_id(patch.entity).add((Of, seen.entity));
            patch.linked = true;
        }
        keep(world, &mut self.builds, seen);
    }

    fn compiled(&mut self, world: &World, compiled: event::Compiled) {
        let program = program(world, &compiled.binary, &compiled.workspace);
        program.set(Program { binary: compiled.binary.clone(), krate: compiled.krate.clone() });
        let entity = world.entity_named(&format!("patches::p{}", self.next));
        if matches!(compiled.kind.as_str(), "error" | "cargo") {
            entity.add(Refused);
        }
        entity.add((PatchOf, program));
        entity.set(Patch {
            kind: compiled.kind,
            functions: compiled.functions,
            files: compiled.files,
            why: compiled.why,
            at_us: compiled.at_us,
            took_us: compiled.took_us,
        });
        let mut seen = Seen {
            entity: entity.id(),
            workspace: compiled.workspace,
            from_us: compiled.at_us.saturating_sub(compiled.took_us),
            to_us: compiled.at_us,
            program: Some(program.id()),
            linked: false,
        };
        // An editor's edit, or a build whose client has already told.
        if let Some(build) = self.builds.iter().rev().find(|it| it.holds(&seen)) {
            entity.add((Of, build.entity));
            seen.linked = true;
        }
        keep(world, &mut self.patches, seen);
    }

    fn live(&mut self, world: &World, live: event::Live) {
        let program = program(world, &live.executable, "");
        let process = world.entity_named(&format!("processes::{}", live.pid));
        process.set(Process { pid: live.pid, executable: live.executable.clone() });
        process.add((Runs, program));
        let entity = world.entity_named(&format!("live::l{}", self.next));
        match live.outcome.as_str() {
            "sent" => entity.add(Sent),
            _ => entity.add(Restart),
        };
        entity.add((PatchOf, program));
        entity.add((PatchedInto, process));
        let patch = self.patches.iter().rev().find(|it| it.program == Some(program.id()));
        if let Some(patch) = patch {
            entity.add((Of, patch.entity));
        }
        entity.set(LivePatch {
            outcome: live.outcome,
            why: live.why,
            sequence: live.sequence,
            functions: live.functions,
            statics: live.statics,
            at_us: live.at_us,
            took_us: live.took_us,
        });
        self.live.push_back(entity.id());
        if self.live.len() > KEEP
            && let Some(oldest) = self.live.pop_front()
        {
            world.entity_from_id(oldest).destruct();
        }
    }

    fn tests(&mut self, world: &World, tests: &event::Tests) {
        let krate = world.entity_named(&format!("crates::{}", name_of(&tests.package)));
        let stem = std::path::Path::new(&tests.executable)
            .file_name()
            .map_or_else(|| tests.executable.clone(), |it| it.to_string_lossy().into_owned());
        let target = world.entity_named(&format!("tests::{}", name_of(&stem)));
        target.add((TestOf, krate));
        target.add((In, workspace(world, &tests.workspace)));
        let build = self
            .builds
            .iter()
            .rev()
            .find(|it| related(&it.workspace, &tests.workspace) && it.from_us <= tests.at_us);
        if let Some(build) = build {
            // Each run says which build it followed: the last.
            target.remove((Of, flecs::Wildcard::ID));
            target.add((Of, build.entity));
        }
        let ran = tests.tests.iter().filter(|it| it.outcome != Outcome::NotRun).count();
        let failed = tests.tests.iter().filter(|it| it.outcome == Outcome::Failed).count();
        target.set(TestTarget {
            package: tests.package.clone(),
            kind: tests.kind.clone(),
            executable: tests.executable.clone(),
            selection: tests.selection.clone(),
            total: tests.total,
            ran: ran as u64,
            failed: failed as u64,
            status: tests.status,
            at_us: tests.at_us,
            took_us: tests.took_us,
        });
        for test in &tests.tests {
            let path = format!("tests::{}::{}", name_of(&stem), name_of(&test.name));
            let entity = world.entity_named(&path);
            entity.add((TestOf, target));
            entity.remove(Changed).remove(Skipped).remove(Always).remove(Sandboxed).remove(Every);
            match test.picked {
                Picked::Changed => entity.add(Changed),
                Picked::Skipped => entity.add(Skipped),
                Picked::Always => entity.add(Always),
                Picked::Sandboxed => entity.add(Sandboxed),
                Picked::Every => entity.add(Every),
            };
            let had = entity.cloned::<Option<&Test>>();
            let (at_us, ended_us) = match test.outcome {
                // One that did not run keeps the outcome of its last run.
                Outcome::NotRun => had.as_ref().map_or((0, 0), |it| (it.at_us, it.ended_us)),
                _ => (tests.at_us, test.ended_us),
            };
            if test.outcome != Outcome::NotRun {
                entity.remove(Passed).remove(Failed).remove(Ignored).remove(Unknown);
                match test.outcome {
                    Outcome::Passed => entity.add(Passed),
                    Outcome::Failed => entity.add(Failed),
                    Outcome::Ignored => entity.add(Ignored),
                    Outcome::Unknown | Outcome::NotRun => entity.add(Unknown),
                };
            }
            entity.set(Test {
                name: test.name.clone(),
                why: test.why.clone(),
                path: test.path.clone(),
                at_us,
                ended_us,
            });
            if entity.doc_name().is_none() {
                entity.set_doc_name(&test.name);
            }
        }
    }
}

impl Seen {
    /// Whether `patch` ran during this build, of its workspace.
    fn holds(&self, patch: &Seen) -> bool {
        related(&self.workspace, &patch.workspace)
            && self.from_us <= patch.to_us
            && patch.to_us <= self.to_us + SLACK_US
    }
}

/// Adds `seen` to `kept`, dropping the oldest past [`KEEP`].
fn keep(world: &World, kept: &mut VecDeque<Seen>, seen: Seen) {
    kept.push_back(seen);
    if kept.len() > KEEP
        && let Some(oldest) = kept.pop_front()
    {
        world.entity_from_id(oldest.entity).destruct();
    }
}

/// Whether one directory is in the other: a package's directory is in its workspace's.
fn related(a: &str, b: &str) -> bool {
    a.is_empty() || b.is_empty() || a.starts_with(b) || b.starts_with(a)
}

fn workspace<'w>(world: &'w World, root: &str) -> EntityView<'w> {
    let entity = world.entity_named(&format!("workspaces::{}", name_of_path(root)));
    if !entity.has(Workspace::id()) {
        entity.set(Workspace { root: root.to_owned() });
    }
    entity
}

fn program<'w>(world: &'w World, binary: &str, root: &str) -> EntityView<'w> {
    let entity = world.entity_named(&format!("programs::{}", name_of_path(binary)));
    if !root.is_empty() && !entity.has((In, flecs::Wildcard::ID)) {
        entity.add((In, workspace(world, root)));
    }
    entity
}

/// A name for the entity of a path: its last component and a hash of all of it, as two
/// checkouts have programs of one name.
fn name_of_path(path: &str) -> String {
    let last = path.trim_end_matches('/').rsplit('/').next().unwrap_or(path);
    let hash = path.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
    });
    format!("{}-{:06x}", name_of(last), hash & 0xff_ffff)
}

/// `name` as an entity name: `::` stays the separator of a test's modules, and what REST
/// reads as one (`.`, `/`) or what a query can't hold becomes `_`.
fn name_of(name: &str) -> String {
    let kept = |c: char| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | ':');
    let name: String = name.chars().map(|c| if kept(c) { c } else { '_' }).collect();
    // A lone `:` would be read as part of a separator.
    let name = name.replace("::", "\u{0}").replace(':', "_").replace('\u{0}', "::");
    name.trim_matches(':').to_owned()
}
