//! One run of user code that ends (a build script, a probe): its result from the store when
//! one is there for its inputs as they are now ([`crate::index::find`]), else a spawn in a
//! sandbox that traces, whose result is checked and kept under exactly what it read
//! ([`crate::index::keep`]).
//!
//! What a run writes is its `OUT_DIR`, given empty: what is in it when it ends is read back
//! ([`collect`]), every path checked to be inside it, and kept with what it printed. A run
//! reused writes its `OUT_DIR` back as it was ([`restore`]). A run that failed is not kept:
//! cargo reruns a failed script every build, and so does blaze.
//!
//! With [`Mode::Verify`] a run found in the store runs anyway, and a result that differs
//! from the one kept is an error naming the run: its key misses an input it depends on, or it
//! is not a function of its inputs.

use std::{
    fmt, io,
    path::{Path, PathBuf},
};

use blaze_cas::{Store, Ticket};
use blaze_core::{Command, Completion, Done, Effect, Exit, Sandbox, Tag, Traced};
use serde::{Deserialize, Serialize};

use crate::{
    RunKey,
    index::{self, FindError, Found, KeepError},
    rule::{self, CargoRule},
    trace::Inputs,
};

/// What a run's result is, as kept.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct Value {
    pub code: Option<i32>,
    pub signal: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    /// What its `OUT_DIR` held as it ended, by path in it, sorted.
    pub files: Vec<OutFile>,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct OutFile {
    /// `/`-separated, relative, with no `.` or `..`.
    pub path: String,
    pub executable: bool,
    /// `None` for a directory, which is kept even when empty.
    pub bytes: Option<Vec<u8>>,
}

impl Value {
    pub fn succeeded(&self) -> bool {
        self.code == Some(0)
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = b"blaze v2 run value 1".to_vec();
        out.extend(postcard::to_allocvec(self).expect("a value is always encodable"));
        out
    }

    pub fn decode(bytes: &[u8]) -> Result<Value, String> {
        let rest = bytes
            .strip_prefix(b"blaze v2 run value 1".as_slice())
            .ok_or_else(|| "not a run's value".to_owned())?;
        postcard::from_bytes(rest).map_err(|error| format!("a run's value: {error}"))
    }

    fn of(exit: &Exit, files: Vec<OutFile>) -> Value {
        Value {
            code: exit.code,
            signal: exit.signal,
            stdout: exit.stdout.clone(),
            stderr: exit.stderr.clone(),
            files,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    Normal,
    Verify,
}

/// What a run is.
#[derive(Clone, Debug)]
pub struct Spec {
    pub key: RunKey,
    /// Run in a sandbox that traces; its `OUT_DIR`, when it has one, writable in it.
    pub command: Command,
    pub out_dir: Option<PathBuf>,
    pub mode: Mode,
}

/// Why a run gave no result.
#[derive(Debug)]
pub enum RunError<E> {
    Find(FindError<E>),
    Inputs(E),
    Keep(KeepError),
    /// The store's value is not a run's.
    BadValue(String),
    /// Its sandbox did not trace, or the spawn did not start, or ended oddly.
    Spawn(String),
    /// Reading or writing its `OUT_DIR`.
    OutDir(String),
    /// Verify mode: run again, it gave other than what was kept.
    Differs { key: RunKey, what: String },
}

impl<E: fmt::Debug> fmt::Display for RunError<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RunError::Find(error) => write!(f, "finding the run's result: {error:?}"),
            RunError::Inputs(error) => write!(f, "hashing what the run read: {error:?}"),
            RunError::Keep(error) => write!(f, "keeping the run's result: {error:?}"),
            RunError::BadValue(why) => write!(f, "the run's kept result: {why}"),
            RunError::Spawn(why) => write!(f, "the run: {why}"),
            RunError::OutDir(why) => write!(f, "its OUT_DIR: {why}"),
            RunError::Differs { key, what } => write!(
                f,
                "verify: the run of {} given {} gave other than the result kept for the same \
                 inputs ({what}): its key misses an input, or it is not a function of its inputs",
                key.code, key.given
            ),
        }
    }
}

/// A run in flight, or done.
#[derive(Debug)]
pub struct Run {
    spec: Spec,
    tag: Tag,
    /// In verify mode, what the store had.
    kept: Option<Value>,
}

/// How a run began.
#[derive(Debug)]
pub enum Begun {
    /// From the store, its `OUT_DIR` written back: done.
    Reused(Value),
    /// Its spawn asked for: [`Run::complete`] takes the spawn's completion.
    Running(Run),
}

impl Run {
    /// Begins `spec`: its result as `inputs` are now, or its spawn as `tag`.
    pub fn begin<I: Inputs>(
        mut spec: Spec,
        store: &Store,
        inputs: &mut I,
        tag: Tag,
        effects: &mut Vec<Effect>,
    ) -> Result<Begun, RunError<I::Error>> {
        let kept = match index::find_held(store, &spec.key, inputs).map_err(RunError::Find)? {
            Found::Hit { value, .. } => Some(Value::decode(value).map_err(RunError::BadValue)?),
            Found::Miss { .. } => None,
        };
        if let (Some(value), Mode::Normal) = (&kept, spec.mode) {
            if let Some(out_dir) = &spec.out_dir {
                restore(out_dir, &value.files).map_err(RunError::OutDir)?;
            }
            return Ok(Begun::Reused(value.clone()));
        }
        match &mut spec.command.sandbox {
            Sandbox::On { trace, write, .. } => {
                *trace = true;
                if let Some(out_dir) = &spec.out_dir
                    && !write.contains(out_dir)
                {
                    write.push(out_dir.clone());
                }
            }
            Sandbox::Off => {
                return Err(RunError::Spawn(format!(
                    "{}: user code runs in a sandbox that traces; this one is off, so what it \
                     reads can't be known and its result can't be kept",
                    spec.command.program.display()
                )));
            }
        }
        if let Some(out_dir) = &spec.out_dir {
            empty(out_dir).map_err(RunError::OutDir)?;
        }
        effects.push(Effect::Spawn { command: Box::new(spec.command.clone()), tag });
        Ok(Begun::Running(Run { spec, tag, kept }))
    }

    pub fn tag(&self) -> Tag {
        self.tag
    }

    /// Takes the spawn's completion: the result, kept when it succeeded (what `rule` says
    /// cargo would watch is added when it read what can't be traced). `inputs` hash what it
    /// read as it is now, when it has just ended. `keyed` takes out of what was traced what
    /// the run's key has already ([`crate::script::keyed`]); what it read in its own
    /// `OUT_DIR`, given empty, it wrote itself, and is no input.
    pub fn complete<I: Inputs>(
        self,
        completion: Completion,
        store: &mut Store,
        inputs: &mut I,
        rule: impl FnOnce(&Exit) -> CargoRule,
        keyed: impl FnOnce(&mut Traced),
    ) -> Result<(Value, Traced, Vec<Ticket>), RunError<I::Error>> {
        assert_eq!(completion.tag, self.tag, "a run takes its own completion");
        let exit = match completion.done {
            Done::Exited(exit) => exit,
            Done::Failed(error) => {
                return Err(RunError::Spawn(format!("{}: {error}", self.spec.command.program.display())));
            }
            other => return Err(RunError::Spawn(format!("a spawn completed as {other:?}"))),
        };
        let Some(mut traced) = exit.traced.clone() else {
            return Err(RunError::Spawn(format!(
                "{}: its sandbox traces, and its exit has no record of what it read",
                self.spec.command.program.display()
            )));
        };
        keyed(&mut traced);
        if let Some(out_dir) = &self.spec.out_dir {
            traced.files.retain(|it| !it.0.starts_with(out_dir));
            traced.dirs.retain(|it| !it.0.starts_with(out_dir));
        }
        let files = match &self.spec.out_dir {
            Some(out_dir) => collect(out_dir).map_err(RunError::OutDir)?,
            None => Vec::new(),
        };
        let value = Value::of(&exit, files);
        if let Some(kept) = &self.kept {
            if let Some(what) = differs(kept, &value) {
                return Err(RunError::Differs { key: self.spec.key, what });
            }
        }
        if !value.succeeded() {
            return Ok((value, traced, Vec::new()));
        }
        rule::widen(&mut traced, &rule(&exit), inputs).map_err(RunError::Inputs)?;
        let tickets =
            index::keep(store, &self.spec.key, &traced, value.encode()).map_err(RunError::Keep)?;
        Ok((value, traced, tickets))
    }
}

/// The first way `ran` differs from `kept`, if it does.
pub fn differs(kept: &Value, ran: &Value) -> Option<String> {
    if (kept.code, kept.signal) != (ran.code, ran.signal) {
        return Some(format!("it ended {:?} {:?}, not {:?} {:?}", ran.code, ran.signal, kept.code, kept.signal));
    }
    if kept.stdout != ran.stdout {
        let line = first_line_differing(&kept.stdout, &ran.stdout);
        return Some(format!("what it printed differs at line {line}"));
    }
    let kept_paths: Vec<&str> = kept.files.iter().map(|it| it.path.as_str()).collect();
    let ran_paths: Vec<&str> = ran.files.iter().map(|it| it.path.as_str()).collect();
    if kept_paths != ran_paths {
        return Some(format!("its OUT_DIR holds {ran_paths:?}, not {kept_paths:?}"));
    }
    kept.files
        .iter()
        .zip(&ran.files)
        .find(|(a, b)| a != b)
        .map(|(a, _)| format!("{} in its OUT_DIR differs", a.path))
}

fn first_line_differing(a: &[u8], b: &[u8]) -> usize {
    let a: Vec<&[u8]> = a.split(|it| *it == b'\n').collect();
    let b: Vec<&[u8]> = b.split(|it| *it == b'\n').collect();
    (0..a.len().max(b.len())).find(|&at| a.get(at) != b.get(at)).unwrap_or(0) + 1
}

/// `dir` made, empty.
fn empty(dir: &Path) -> Result<(), String> {
    match std::fs::remove_dir_all(dir) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("{}: emptying it: {error}", dir.display())),
    }
    std::fs::create_dir_all(dir).map_err(|error| format!("{}: making it: {error}", dir.display()))
}

/// What `dir` holds: every file and directory below it, by path, sorted. A link is what it
/// leads to when that is inside `dir`, and an error otherwise: what a later step reads
/// through it would be read outside every trace.
pub fn collect(dir: &Path) -> Result<Vec<OutFile>, String> {
    let top = std::fs::canonicalize(dir).map_err(|error| format!("{}: {error}", dir.display()))?;
    let mut out = Vec::new();
    let mut stack = vec![PathBuf::new()];
    while let Some(rel) = stack.pop() {
        let at = dir.join(&rel);
        let entries = std::fs::read_dir(&at).map_err(|error| format!("{}: {error}", at.display()))?;
        for entry in entries {
            let entry = entry.map_err(|error| format!("{}: {error}", at.display()))?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                return Err(format!("{}: a name that is not UTF-8 ({name:?})", at.display()));
            };
            let rel = rel.join(name);
            let path = dir.join(&rel);
            let link = std::fs::symlink_metadata(&path).map_err(|error| format!("{}: {error}", path.display()))?;
            if link.file_type().is_symlink() {
                let target = std::fs::canonicalize(&path).map_err(|error| format!("{}: a link to nothing: {error}", path.display()))?;
                if !target.starts_with(&top) {
                    return Err(format!(
                        "{} is a link to {}, outside the OUT_DIR: what is read through it \
                         would be read outside every trace",
                        path.display(),
                        target.display()
                    ));
                }
            }
            let meta = std::fs::metadata(&path).map_err(|error| format!("{}: {error}", path.display()))?;
            let name = rel_name(&rel);
            match meta.is_dir() {
                true => {
                    out.push(OutFile { path: name, executable: false, bytes: None });
                    // A link to a directory inside is kept as what it leads to, but not
                    // walked twice.
                    if !link.file_type().is_symlink() {
                        stack.push(rel);
                    }
                }
                false => {
                    let bytes = std::fs::read(&path).map_err(|error| format!("{}: {error}", path.display()))?;
                    out.push(OutFile { path: name, executable: executable(&meta), bytes: Some(bytes) });
                }
            }
        }
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

fn rel_name(rel: &Path) -> String {
    let parts: Vec<String> = rel.components().map(|it| it.as_os_str().to_string_lossy().into_owned()).collect();
    parts.join("/")
}

#[cfg(unix)]
fn executable(meta: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    meta.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn executable(_: &std::fs::Metadata) -> bool {
    false
}

/// Writes `files` into `dir`, emptied first: the `OUT_DIR` a run reused left. Each path is
/// checked to stay inside it; one that would not is the store's bytes being wrong.
pub fn restore(dir: &Path, files: &[OutFile]) -> Result<(), String> {
    empty(dir)?;
    for file in files {
        let ok = !file.path.is_empty()
            && !file.path.starts_with('/')
            && file.path.split('/').all(|it| !it.is_empty() && it != "." && it != ".." && !it.contains('\\'));
        if !ok {
            return Err(format!("a kept OUT_DIR names {:?}, which would leave it", file.path));
        }
        let path = dir.join(&file.path);
        match &file.bytes {
            None => std::fs::create_dir_all(&path).map_err(|error| format!("{}: {error}", path.display()))?,
            Some(bytes) => {
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent).map_err(|error| format!("{}: {error}", parent.display()))?;
                }
                std::fs::write(&path, bytes).map_err(|error| format!("{}: {error}", path.display()))?;
                #[cfg(unix)]
                if file.executable {
                    use std::os::unix::fs::PermissionsExt as _;
                    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
                        .map_err(|error| format!("{}: {error}", path.display()))?;
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{
        cell::RefCell,
        collections::BTreeMap,
        ffi::{OsStr, OsString},
        rc::Rc,
        time::Duration,
    };

    use blaze_cas::Config;
    use blaze_core::{
        FileName, run_until_idle,
        sim::{Faults, Ran, Sim},
    };
    use blaze_ids::ContentId;

    use super::*;
    use crate::trace::{DIRECTORY, listing};

    struct Fixed {
        files: BTreeMap<PathBuf, [u8; 32]>,
        env: Vec<(OsString, OsString)>,
    }

    impl Inputs for Fixed {
        type Error = String;
        fn file(&mut self, path: &Path) -> Result<Option<[u8; 32]>, String> {
            Ok(match path == Path::new("/ws") {
                true => Some(DIRECTORY),
                false => self.files.get(path).copied(),
            })
        }
        fn dir(&mut self, path: &Path) -> Result<Option<[u8; 32]>, String> {
            Ok((path == Path::new("/ws")).then(|| listing([OsStr::new("data")])))
        }
        fn env(&mut self, name: &OsStr) -> Option<OsString> {
            self.env.iter().find(|it| it.0 == name).map(|it| it.1.clone())
        }
        fn env_all(&mut self) -> Vec<(OsString, OsString)> {
            self.env.clone()
        }
        fn names(&mut self, path: &Path) -> Result<Option<Vec<(OsString, bool)>>, String> {
            Ok((path == Path::new("/ws")).then(|| vec![(OsString::from("data"), false)]))
        }
    }

    fn opened(sim: &mut Sim) -> Store {
        let mut effects = Vec::new();
        let config = Config { dir: FileName::new("store").unwrap(), layers: Vec::new(), tags: 1 << 48 };
        let mut store = Store::open(config, &mut effects);
        run_until_idle(sim, &mut effects, |completion, effects| {
            store.complete(completion, effects);
        });
        assert!(store.ready());
        store
    }

    fn spec(mode: Mode) -> Spec {
        Spec {
            key: RunKey { code: ContentId::of("blaze v2 test 1", b"script"), given: ContentId::default() },
            command: Command {
                program: "/script".into(),
                args: Vec::new(),
                env: vec![("CC".into(), "cc".into())],
                dir: "/ws".into(),
                stdin: Vec::new(),
                sandbox: Sandbox::On { read: vec!["/ws".into()], write: Vec::new(), network: false, trace: false },
                memory: None,
            },
            out_dir: None,
            mode,
        }
    }

    /// A script that reads `/ws/data` and `CC` and prints what `printed` holds then.
    fn script(sim: &mut Sim, data: Rc<RefCell<[u8; 32]>>, printed: Rc<RefCell<Vec<u8>>>, runs: Rc<RefCell<u32>>) {
        sim.program("/script", move |command, _| {
            *runs.borrow_mut() += 1;
            let Sandbox::On { trace: true, .. } = command.sandbox else { panic!("a run traces") };
            let traced = Traced {
                files: vec![("/ws/data".into(), Some(*data.borrow()))],
                dirs: vec![("/ws".into(), listing([OsStr::new("data")]))],
                env: vec![("CC".into(), Some("cc".into()))],
                untraceable: Vec::new(),
            };
            let exit = Exit {
                code: Some(0),
                signal: None,
                stdout: printed.borrow().clone(),
                stderr: Vec::new(),
                traced: Some(traced),
                peak_memory: 0,
            };
            Ran { exit, takes: Duration::from_millis(1) }
        });
    }

    fn go(sim: &mut Sim, store: &mut Store, inputs: &mut Fixed, mode: Mode) -> Result<(Value, bool), RunError<String>> {
        let mut effects = Vec::new();
        let run = match Run::begin(spec(mode), store, inputs, Tag(1), &mut effects)? {
            Begun::Reused(value) => return Ok((value, false)),
            Begun::Running(run) => run,
        };
        let mut done = None;
        run_until_idle(sim, &mut effects, |completion, _| done = Some(completion));
        let (value, _, _) = run.complete(done.unwrap(), store, inputs, |_| CargoRule::default(), |_| {})?;
        Ok((value, true))
    }

    #[test]
    fn a_run_is_reused_until_what_it_read_changes_and_verified_on_request() {
        let mut sim = Sim::new(3, Faults::default());
        let mut store = opened(&mut sim);
        let data = Rc::new(RefCell::new([1; 32]));
        let printed = Rc::new(RefCell::new(b"cargo:rustc-cfg=one\n".to_vec()));
        let runs = Rc::new(RefCell::new(0));
        script(&mut sim, data.clone(), printed.clone(), runs.clone());
        let mut inputs = Fixed { files: [("/ws/data".into(), [1; 32])].into(), env: vec![("CC".into(), "cc".into())] };

        let (first, ran) = go(&mut sim, &mut store, &mut inputs, Mode::Normal).unwrap();
        assert!(ran && first.stdout == b"cargo:rustc-cfg=one\n");
        let (again, ran) = go(&mut sim, &mut store, &mut inputs, Mode::Normal).unwrap();
        assert!(!ran && again == first);
        // A worker's read, through a reader under its task's guard, finds what the store's
        // thread finds.
        let key = spec(Mode::Normal).key;
        let reader = store.reader();
        let guard = blaze_cas::enter();
        let Ok(Found::Hit { value: read, .. }) = index::find(&reader, &guard, &key, &mut inputs) else {
            panic!("a reader misses a kept run")
        };
        let Ok(Found::Hit { value: held, .. }) = index::find_held(&store, &key, &mut inputs) else {
            panic!("the store misses a kept run")
        };
        assert_eq!(read, held);
        drop(guard);
        // A variable the run never read changes nothing.
        inputs.env.push(("PATH".into(), "/elsewhere".into()));
        assert!(!go(&mut sim, &mut store, &mut inputs, Mode::Normal).unwrap().1);

        inputs.files.insert("/ws/data".into(), [2; 32]);
        *data.borrow_mut() = [2; 32];
        *printed.borrow_mut() = b"cargo:rustc-cfg=two\n".to_vec();
        let (second, ran) = go(&mut sim, &mut store, &mut inputs, Mode::Normal).unwrap();
        assert!(ran && second.stdout == b"cargo:rustc-cfg=two\n");
        // Both kept: going back reuses the first.
        inputs.files.insert("/ws/data".into(), [1; 32]);
        assert_eq!(go(&mut sim, &mut store, &mut inputs, Mode::Normal).unwrap(), (first.clone(), false));
        assert_eq!(*runs.borrow(), 2);

        *data.borrow_mut() = [1; 32];
        *printed.borrow_mut() = b"cargo:rustc-cfg=one\n".to_vec();
        assert_eq!(go(&mut sim, &mut store, &mut inputs, Mode::Verify).unwrap(), (first, true));
        *printed.borrow_mut() = b"cargo:rustc-cfg=other\n".to_vec();
        let differs = go(&mut sim, &mut store, &mut inputs, Mode::Verify).unwrap_err();
        assert!(matches!(differs, RunError::Differs { .. }), "{differs:?}");
    }

    #[test]
    fn a_run_outside_a_sandbox_is_refused() {
        let mut sim = Sim::new(4, Faults::default());
        let store = opened(&mut sim);
        let mut off = spec(Mode::Normal);
        off.command.sandbox = Sandbox::Off;
        let mut inputs = Fixed { files: BTreeMap::new(), env: Vec::new() };
        let begun = Run::begin(off, &store, &mut inputs, Tag(1), &mut Vec::new());
        assert!(matches!(begun, Err(RunError::Spawn(_))));
    }

    fn value() -> Value {
        Value {
            code: Some(0),
            signal: None,
            stdout: b"cargo:rustc-cfg=a\ncargo:rustc-cfg=b\n".to_vec(),
            stderr: Vec::new(),
            files: vec![
                OutFile { path: "gen".into(), executable: false, bytes: None },
                OutFile { path: "gen/a.rs".into(), executable: false, bytes: Some(b"x".to_vec()) },
            ],
        }
    }

    #[test]
    fn a_value_round_trips() {
        assert_eq!(Value::decode(&value().encode()), Ok(value()));
        assert!(Value::decode(b"other").is_err());
    }

    #[test]
    fn differences_are_named() {
        assert_eq!(differs(&value(), &value()), None);
        let mut printed = value();
        printed.stdout = b"cargo:rustc-cfg=a\ncargo:rustc-cfg=c\n".to_vec();
        assert_eq!(differs(&value(), &printed).as_deref(), Some("what it printed differs at line 2"));
        let mut file = value();
        file.files[1].bytes = Some(b"y".to_vec());
        assert_eq!(differs(&value(), &file).as_deref(), Some("gen/a.rs in its OUT_DIR differs"));
        let mut failed = value();
        failed.code = Some(1);
        assert!(differs(&value(), &failed).unwrap().contains("ended"));
    }

    #[test]
    fn an_out_dir_is_collected_and_restored_and_kept_inside() {
        let dir = std::env::temp_dir().join(format!("blaze-out-dir-test-{}", std::process::id()));
        empty(&dir).unwrap();
        restore(&dir, &value().files).unwrap();
        assert_eq!(collect(&dir).unwrap(), value().files);
        let bad = [OutFile { path: "../x".into(), executable: false, bytes: Some(Vec::new()) }];
        assert!(restore(&dir, &bad).is_err());
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("/etc/hosts", dir.join("out")).unwrap();
            assert!(collect(&dir).unwrap_err().contains("outside the OUT_DIR"));
            std::fs::remove_file(dir.join("out")).unwrap();
            std::os::unix::fs::symlink(dir.join("gen/a.rs"), dir.join("in")).unwrap();
            let files = collect(&dir).unwrap();
            assert!(files.iter().any(|it| it.path == "in" && it.bytes.as_deref() == Some(b"x".as_slice())));
        }
        _ = std::fs::remove_dir_all(&dir);
    }
}
