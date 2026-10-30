//! A build script's run (#1450, #805, #1480, #804, #1296): what it is given, what its result
//! is keyed by, and what cargo's rule watches of it.
//!
//! - **Given**: the script's code (the content ID of the binary the daemon compiled it to, so
//!   a script rebuilt to the same bytes is the same script), its directory, and each variable
//!   of its environment that blaze sets from the unit. A variable passed through from the
//!   environment the build was started with (`PATH`, `CC`, `HOME`) is not given: what the
//!   script reads of those is traced, so a `PATH` that differs only where the script never
//!   looks keeps its result (#805). A variable counts as passed through when the script sees
//!   it with the value it had where the build started: either way the value the script saw
//!   is in its key, given or traced.
//! - **Traced**: every file, listing and variable the script and every program it ran read,
//!   the C compiler and its headers among them (#1480).
//! - The jobserver's variables ([`RUN_ONLY`]) are of this run alone, as they are not in
//!   cargo's fingerprint: neither given nor traced.
//! - **Elided** (#1296): a script that provably only prints constant cargo directives is not
//!   compiled or run; what it prints is known from its source ([`elided`]).

use std::{
    collections::BTreeMap,
    ffi::{OsStr, OsString},
    path::PathBuf,
};

use blaze_build_script::{Error, Unit, output};
use blaze_core::{Command, Exit, Sandbox, Traced};
use blaze_ids::ContentId;

use crate::{
    Given, RunKey, RunKind,
    rule::{self, CargoRule},
    run::{self, Mode, OutFile, Value},
};

/// Variables of one run alone: a script's jobserver, which another run is given another of.
pub const RUN_ONLY: &[&str] = &["CARGO_MAKEFLAGS", "MAKEFLAGS", "MFLAGS"];

/// What a script's run needs that the unit doesn't say.
#[derive(Clone, Debug)]
pub struct Place {
    /// The content ID of the script's binary.
    pub code: ContentId,
    /// What it may read besides what every sandbox reads: its package, the toolchain.
    pub read: Vec<PathBuf>,
    /// Whether its package's manifest lets it reach the network: then what it reached is
    /// untraceable, and it is kept by cargo's rule.
    pub network: bool,
    pub memory: Option<u64>,
    pub mode: Mode,
}

/// A script's run, ready to begin, and the environment a check of its inputs is made in.
#[derive(Clone, Debug)]
pub struct Prepared {
    pub spec: run::Spec,
    /// What the script sees: the environment [`crate::inputs::Disk`] answers a check with,
    /// so a traced variable is compared with what a run now would see.
    pub env: Vec<(OsString, OsString)>,
    /// The names given, which are left out of what is traced: the key has them already.
    pub given: Vec<OsString>,
}

/// `unit`'s run, in a build started with `inherited` (cargo's environment once started:
/// [`blaze_build_script::env::cargo_started`]).
pub fn prepare(unit: &Unit, inherited: &[(OsString, OsString)], place: &Place) -> Result<Prepared, Error> {
    let env = blaze_build_script::env(unit, inherited)?;
    let inherited: BTreeMap<&OsStr, &OsStr> =
        inherited.iter().map(|(name, value)| (name.as_os_str(), value.as_os_str())).collect();
    let mut given = Given::new(RunKind::BuildScript);
    let mut names = Vec::new();
    for (name, value) in &env.vars {
        if run_only(name) || inherited.get(name.as_os_str()) == Some(&value.as_os_str()) {
            continue;
        }
        given.env(name.clone(), value.clone());
        names.push(name.clone());
    }
    given.part("program", crate::trace::os_bytes(unit.script.as_os_str()));
    given.part("dir", crate::trace::os_bytes(unit.package.manifest_dir.as_os_str()));
    let key = RunKey { code: place.code, given: given.finish() };
    let vars: Vec<(OsString, OsString)> = env.vars.into_iter().collect();
    let command = Command {
        program: unit.script.clone(),
        args: Vec::new(),
        env: vars.clone(),
        dir: unit.package.manifest_dir.clone(),
        stdin: Vec::new(),
        sandbox: Sandbox::On {
            read: place.read.clone(),
            write: vec![unit.out_dir.clone()],
            network: place.network,
            trace: true,
        },
        memory: place.memory,
    };
    let spec = run::Spec { key, command, out_dir: Some(unit.out_dir.clone()), mode: place.mode };
    Ok(Prepared { spec, env: vars, given: names })
}

fn run_only(name: &OsStr) -> bool {
    RUN_ONLY.iter().any(|it| name == *it)
}

/// What is traced of a run of `prepared`, without what its key has already and what is of
/// that run alone: what the run's result is kept under.
pub fn keyed(prepared: &Prepared, traced: &mut Traced) {
    traced.env.retain(|(name, _)| !run_only(name) && !prepared.given.contains(name));
}

/// What cargo watches of `unit`'s run that ended as `exit` did: its `rerun-if-changed` and
/// `rerun-if-env-changed`, or its whole package when it named no path. Output that can't be
/// read watches the whole package: the run fails when its output is read anyway.
pub fn cargo_rule(unit: &Unit, inherited: &[(OsString, OsString)], exit: &Exit) -> CargoRule {
    let cx = output::Context::of(unit, inherited);
    let dir = &unit.package.manifest_dir;
    match output::parse(&exit.stdout, &cx) {
        Ok(parsed) => {
            let env: Vec<OsString> = parsed.rerun_if_env_changed.iter().map(OsString::from).collect();
            rule::of_directives(dir, &parsed.rerun_if_changed, &env)
        }
        Err(_) => CargoRule { paths: Vec::new(), package: Some(dir.clone()), env: Vec::new() },
    }
}

/// The result of a script that [`blaze_build_script::elide`] proves prints `stdout` and does
/// nothing else: what a run of it would have been, with an empty `OUT_DIR`.
pub fn elided(stdout: Vec<u8>) -> Value {
    Value { code: Some(0), signal: None, stdout, stderr: Vec::new(), files: Vec::<OutFile>::new() }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unit() -> Unit {
        let mut unit = Unit::default();
        unit.package.name = "p".into();
        unit.package.version = "0.1.0".into();
        unit.package.manifest_dir = "/ws/p".into();
        unit.script = "/t/build/p/script".into();
        unit.out_dir = "/t/build/p/0123456789abcdef/out".into();
        unit.target = "x86_64-unknown-linux-gnu".into();
        unit.host = "x86_64-unknown-linux-gnu".into();
        unit
    }

    fn place() -> Place {
        Place { code: ContentId::of("blaze v2 test 1", b"script"), read: vec!["/ws/p".into()], network: false, memory: None, mode: Mode::Normal }
    }

    fn inherited(path: &str) -> Vec<(OsString, OsString)> {
        vec![("PATH".into(), path.into()), ("HOME".into(), "/home/u".into())]
    }

    #[test]
    fn a_path_passed_through_is_traced_not_given() {
        let one = prepare(&unit(), &inherited("/a"), &place()).unwrap();
        let two = prepare(&unit(), &inherited("/b"), &place()).unwrap();
        assert_eq!(one.spec.key, two.spec.key);
        assert!(!one.given.iter().any(|it| it == "PATH"));
        assert!(one.given.iter().any(|it| it == "OUT_DIR"));
        let mut other = unit();
        other.target = "aarch64-unknown-linux-gnu".into();
        let three = prepare(&other, &inherited("/a"), &place()).unwrap();
        assert_ne!(one.spec.key, three.spec.key);
        let Sandbox::On { write, trace, .. } = &one.spec.command.sandbox else { panic!() };
        assert!(*trace && write == &[unit().out_dir]);
    }

    #[test]
    fn what_is_given_or_of_one_run_is_not_traced() {
        let prepared = prepare(&unit(), &inherited("/a"), &place()).unwrap();
        let mut traced = Traced {
            env: vec![
                ("OUT_DIR".into(), Some("/t/build/p/0123456789abcdef/out".into())),
                ("CARGO_MAKEFLAGS".into(), Some("-j --jobserver-auth=fifo:/tmp/x".into())),
                ("CC".into(), None),
            ],
            ..Traced::default()
        };
        keyed(&prepared, &mut traced);
        assert_eq!(traced.env, vec![("CC".into(), None)]);
    }

    #[test]
    fn cargo_watches_what_was_named_or_the_whole_package() {
        let exit = |stdout: &[u8]| Exit {
            code: Some(0),
            signal: None,
            stdout: stdout.to_vec(),
            stderr: Vec::new(),
            traced: None,
            peak_memory: 0,
        };
        let none = cargo_rule(&unit(), &inherited("/a"), &exit(b"cargo:rustc-cfg=x\n"));
        assert_eq!(none.package, Some(PathBuf::from("/ws/p")));
        let named = cargo_rule(
            &unit(),
            &inherited("/a"),
            &exit(b"cargo:rerun-if-changed=build.rs\ncargo:rerun-if-env-changed=VERSION\n"),
        );
        assert_eq!(named.package, None);
        assert_eq!(named.paths, vec![PathBuf::from("/ws/p/build.rs")]);
        assert_eq!(named.env, vec![OsString::from("VERSION")]);
    }
}
