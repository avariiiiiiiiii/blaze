//! What may be published to the global cache or fetched from it.
//!
//! A store [`Key`] is a hash: nothing in it says what it was made from. So the rule is
//! checked where a key is made, from what it was made from: a [`QueryMade`], a [`RunMade`],
//! a [`FunctionMade`] or a side table of a public function. Each check makes the key again
//! with the function its owner makes it with (`Key::of`, `blaze_usercode::index`,
//! `blaze_codegen::query::keys`), so what is checked is what the key is a hash of, and then
//! checks every input's [`Origin`]. What passes is a [`Public`]: the client fetches and the
//! builders publish only those.
//!
//! Public is:
//! - a crates.io crate version, named by the SHA-256 of its `.crate` the lock file gives and
//!   the download was checked against ([`Origin::CratesIo`]);
//! - the pinned toolchain's files: std, core, alloc and the rest of its sysroot
//!   ([`Origin::Toolchain`]);
//! - the system image of blaze's builders, which build scripts read headers and a C compiler
//!   from ([`Origin::System`]);
//! - what public runs wrote: the `OUT_DIR`s of build scripts of public crates
//!   ([`Origin::Outputs`]).
//!
//! Never public: a workspace member, a path dependency, a git dependency, a registry other
//! than crates.io, a variable of the machine's environment a run read with a value, a run
//! that read the whole environment or what can't be traced, a test, and a dev (Cranelift)
//! function. And whatever the inputs, a value holding a [`Secrets`] entry is never published.

use std::{
    ffi::OsString,
    fmt,
    path::{Path, PathBuf},
};

use blaze_cas::{Key, Kind};
use blaze_codegen::{Backend, Instance, InstanceKey, query::keys, settings::Spec, summary::Imports};
use blaze_core::Traced;
use blaze_format::SchemaHash;
use blaze_ids::ContentId;
use blaze_usercode::{Given, RunKey, index, trace};
use memchr::memmem;

use crate::sign::Publisher;

/// Where an input of a key comes from.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum Origin {
    /// A crates.io crate version: the SHA-256 of its `.crate`, as the lock file names it.
    CratesIo { name: String, version: String, checksum: [u8; 32] },
    /// The pinned toolchain, by its code identity (`blaze_codegen::settings::Toolchain::id`).
    Toolchain(ContentId),
    /// A system image of blaze's builders, by its content ID.
    System(ContentId),
    /// The `OUT_DIR`s of build scripts of public crates.
    Outputs,
    /// A member of the user's workspace.
    Workspace,
    /// A path dependency outside the workspace.
    Path,
    Git { url: String },
    /// A registry other than crates.io.
    Registry { index: String },
}

impl fmt::Display for Origin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Origin::CratesIo { name, version, .. } => write!(f, "{name} {version} from crates.io"),
            Origin::Toolchain(id) => write!(f, "the toolchain {:x}", id),
            Origin::System(id) => write!(f, "the system image {:x}", id),
            Origin::Outputs => f.write_str("the outputs of public build scripts"),
            Origin::Workspace => f.write_str("a workspace member"),
            Origin::Path => f.write_str("a path dependency"),
            Origin::Git { url } => write!(f, "the git dependency {url}"),
            Origin::Registry { index } => write!(f, "the registry {index}"),
        }
    }
}

/// A directory of this machine and where what is under it comes from. The longest root a
/// path is under says where it comes from, so a private directory inside a public one (or
/// the reverse) is named as what it is.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Root {
    pub path: PathBuf,
    pub origin: Origin,
}

/// The rule of one machine: what it pins and where its public sources are.
#[derive(Clone, Debug)]
pub struct Rule {
    /// The pinned toolchain (`harness/toolchain`), as `Toolchain::id` names it.
    pub toolchain: ContentId,
    /// The system images of blaze's builders.
    pub systems: Vec<ContentId>,
    /// Where crate versions, the toolchain, system images and outputs are on this machine.
    pub roots: Vec<Root>,
    /// Variables whose values are public, so a run that read one with a value may be. Empty
    /// on blaze's builders, whose runs see no environment of the machine.
    pub public_env: Vec<OsString>,
}

/// A key proven public: by a [`Rule`] check, or by a signed bundle manifest that lists it.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Public {
    key: Key,
    kind: Kind,
}

impl Public {
    pub fn key(&self) -> Key {
        self.key
    }

    pub fn kind(&self) -> Kind {
        self.kind
    }

    /// A key a pinned publisher listed in a verified manifest.
    pub(crate) fn listed(key: Key, kind: Kind) -> Public {
        Public { key, kind }
    }
}

/// Why a key is not public.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Private {
    pub key: Key,
    pub why: String,
}

impl fmt::Display for Private {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:x} is not public: {}", self.key.0, self.why)
    }
}

/// A query's value: `Key::of(schema, producer, inputs)`. `producer` is blaze's own code.
#[derive(Clone, Copy, Debug)]
pub struct QueryMade<'a> {
    pub schema: SchemaHash,
    pub producer: ContentId,
    pub inputs: &'a [(ContentId, Origin)],
}

/// A build script's or proc macro's run.
#[derive(Clone, Copy, Debug)]
pub struct RunMade<'a> {
    pub run: RunKey,
    /// What it was given: [`Given::finish`] of it is `run.given`.
    pub given: &'a Given,
    /// The crate of the build script or proc macro.
    pub code: &'a Origin,
    /// The crate it runs for: the package of a build script, the crate a macro expands in.
    pub caller: &'a Origin,
    pub traced: &'a Traced,
}

/// Which of a run's values.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RunPart {
    /// Its result, under `index::result_key`.
    Result,
    /// Slot `n` of the chain of its shapes, under `index::slot`.
    Shape(u32),
}

/// A function instance and the crates its code and type arguments come from.
#[derive(Clone, Debug)]
pub struct FnSource {
    pub instance: Instance,
    /// The crate of the item its code hash is of.
    pub code: Origin,
    /// The crates of the facts of its type arguments: the types' crates, the toolchain for
    /// primitives.
    pub args: Vec<Origin>,
}

/// An LLVM-optimized function as the release driver keeps it: under
/// `keys::release(imports.blob_key(stored(instance, built_in)))`.
#[derive(Clone, Copy, Debug)]
pub struct FunctionMade<'a> {
    /// The settings its instance's `Settings` are of.
    pub spec: &'a Spec,
    pub function: &'a FnSource,
    /// The bodies its code builds in, in the driver's order.
    pub built_in: &'a [FnSource],
    pub imports: &'a Imports,
    /// Each of `imports.imported`, in its order.
    pub imported: &'a [FnSource],
}

impl Rule {
    /// Whether `origin` is public here.
    pub fn origin(&self, origin: &Origin) -> Result<(), String> {
        match origin {
            Origin::CratesIo { name, version, .. } if !name.is_empty() && !version.is_empty() => Ok(()),
            Origin::CratesIo { .. } => Err("a crates.io crate with no name or version".into()),
            Origin::Toolchain(id) if *id == self.toolchain => Ok(()),
            Origin::Toolchain(id) => Err(format!(
                "it reads the toolchain {:x}, not the pinned {:x}",
                id, self.toolchain
            )),
            Origin::System(id) if self.systems.contains(id) => Ok(()),
            Origin::System(id) => Err(format!("it reads the system image {:x}, which is not a builder's", id)),
            Origin::Outputs => Ok(()),
            private => Err(format!("it reads {private}")),
        }
    }

    /// Where `path` comes from: the origin of the longest root it is under.
    pub fn path(&self, path: &Path) -> Result<&Origin, String> {
        self.roots
            .iter()
            .filter(|root| path.starts_with(&root.path))
            .max_by_key(|root| root.path.components().count())
            .map(|root| &root.origin)
            .ok_or_else(|| format!("it reads {}, which is under no public source", path.display()))
    }

    fn public_path(&self, path: &Path) -> Result<(), String> {
        let origin = self.path(path)?;
        self.origin(origin).map_err(|why| format!("{why} ({})", path.display()))
    }

    pub fn query(&self, made: &QueryMade<'_>) -> Result<Public, Private> {
        let ids: Vec<ContentId> = made.inputs.iter().map(|(id, _)| *id).collect();
        let key = Key::of(made.schema, made.producer, &ids);
        let private = |why: String| Private { key, why };
        for (_, origin) in made.inputs {
            self.origin(origin).map_err(private)?;
        }
        Ok(Public { key, kind: Kind::Query })
    }

    pub fn run(&self, made: &RunMade<'_>, part: RunPart) -> Result<Public, Private> {
        let key = match part {
            RunPart::Result => index::result_key(&made.run, made.traced),
            RunPart::Shape(n) => index::slot(&made.run, n),
        };
        let private = |why: String| Private { key, why };
        if made.given.finish() != made.run.given {
            return Err(private("what it was given is not what its key was made with".into()));
        }
        self.origin(made.code).map_err(|why| private(format!("its code: {why}")))?;
        self.origin(made.caller).map_err(|why| private(format!("what it runs for: {why}")))?;
        for (name, value) in made.given.vars() {
            if let Some(path) = absolute(value) {
                self.public_path(&path)
                    .map_err(|why| private(format!("{} names a path: {why}", name.to_string_lossy())))?;
            }
        }
        let traced = made.traced;
        if let Some(it) = traced.untraceable.first() {
            return Err(private(format!("it read what can't be traced ({})", trace::untraceable_name(*it))));
        }
        for (path, _) in &traced.files {
            self.public_path(path).map_err(private)?;
        }
        for (path, _) in &traced.dirs {
            self.public_path(path).map_err(private)?;
        }
        for (name, value) in &traced.env {
            if name == trace::ALL_ENV_NAME {
                return Err(private("it read the whole environment".into()));
            }
            if value.is_some() && !self.public_env.contains(name) {
                return Err(private(format!(
                    "it read {} with a value, which is the machine's",
                    name.to_string_lossy()
                )));
            }
        }
        Ok(Public { key, kind: Kind::Run })
    }

    pub fn function(&self, made: &FunctionMade<'_>) -> Result<Public, Private> {
        let built_in: Vec<InstanceKey> = made.built_in.iter().map(|it| it.instance.key()).collect();
        let stored = blaze_codegen::driver::stored(made.function.instance.key(), &built_in);
        let key = keys::release(&made.imports.blob_key(stored));
        let private = |why: String| Private { key, why };
        let spec = made.spec;
        if spec.settings() != made.function.instance.settings {
            return Err(private("its settings are not those of the spec given".into()));
        }
        if !matches!(spec.backend, Backend::Llvm { .. }) {
            return Err(private("it is not compiled by LLVM: only release code is shared".into()));
        }
        if spec.toolchain.id != self.toolchain {
            return Err(private(format!(
                "it is compiled by the toolchain {:x}, not the pinned {:x}",
                spec.toolchain.id, self.toolchain
            )));
        }
        if made.imported.len() != made.imports.imported.len()
            || made.imported.iter().zip(&made.imports.imported).any(|(it, key)| it.instance.key() != *key)
        {
            return Err(private("the functions given are not those it imports".into()));
        }
        let all = std::iter::once(made.function).chain(made.built_in).chain(made.imported);
        for source in all {
            let what = source.instance.key().0;
            self.origin(&source.code).map_err(|why| private(format!("the code of {what:x}: {why}")))?;
            for origin in &source.args {
                self.origin(origin).map_err(|why| private(format!("a type argument of {what:x}: {why}")))?;
            }
        }
        Ok(Public { key, kind: Kind::Code })
    }

    /// The side table (debug info, panic locations) of a public function, by its bytes' ID.
    pub fn side(&self, id: &ContentId, of: &Public) -> Result<Public, Private> {
        let key = keys::side(id);
        if of.kind != Kind::Code {
            return Err(Private { key, why: format!("a side table of {:x}, which is not code", of.key.0) });
        }
        Ok(Public { key, kind: Kind::Debug })
    }
}

/// `value` as a path when it is an absolute one.
fn absolute(value: &OsString) -> Option<PathBuf> {
    let path = Path::new(value);
    path.is_absolute().then(|| path.to_path_buf())
}

/// What no published value may hold: the publisher's seed, and every value of the
/// publishing machine's environment but the public ones. Named by where each came from;
/// the secret itself is never in a message.
pub struct Secrets {
    finders: Vec<(String, memmem::Finder<'static>)>,
}

impl fmt::Debug for Secrets {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.finders.iter().map(|(name, _)| name)).finish()
    }
}

impl Secrets {
    /// Secrets shorter than this are not looked for: they would be found in most values.
    pub const SHORTEST: usize = 8;

    pub fn new() -> Secrets {
        Secrets { finders: Vec::new() }
    }

    /// `secret`, named `name`. One shorter than [`Secrets::SHORTEST`] is refused.
    pub fn add(&mut self, name: impl Into<String>, secret: &[u8]) -> Result<(), String> {
        let name = name.into();
        if secret.len() < Secrets::SHORTEST {
            return Err(format!("the secret {name} is shorter than {} bytes", Secrets::SHORTEST));
        }
        self.finders.push((name, memmem::Finder::new(secret).into_owned()));
        Ok(())
    }

    /// The publisher's seed, raw and as hex.
    pub fn publisher(&mut self, publisher: &Publisher) {
        let seed = publisher.seed();
        self.add("the publisher's seed", seed).expect("a seed is 32 bytes");
        self.add("the publisher's seed in hex", crate::sign::hex(seed).as_bytes()).expect("64 bytes");
    }

    /// Every value of `vars` of at least [`Secrets::SHORTEST`] bytes whose name is not in
    /// `public`, named by its variable.
    pub fn env(&mut self, vars: impl IntoIterator<Item = (OsString, OsString)>, public: &[OsString]) {
        for (name, value) in vars {
            if public.contains(&name) {
                continue;
            }
            let bytes = trace::os_bytes(&value);
            if bytes.len() >= Secrets::SHORTEST {
                let name = format!("the value of {}", name.to_string_lossy());
                self.add(name, bytes).expect("long enough");
            }
        }
    }

    /// Refuses `bytes`, the value of `key`, when it holds a secret.
    pub fn check(&self, key: Key, bytes: &[u8]) -> Result<(), Private> {
        match self.finders.iter().find(|(_, finder)| finder.find(bytes).is_some()) {
            Some((name, _)) => Err(Private { key, why: format!("it holds {name}") }),
            None => Ok(()),
        }
    }
}

impl Default for Secrets {
    fn default() -> Secrets {
        Secrets::new()
    }
}

#[cfg(test)]
mod tests {
    use blaze_codegen::{
        Settings,
        settings::{DebugInfo, Panic, Relocation, Toolchain},
        Arch,
    };
    use blaze_core::Untraceable;
    use blaze_items::CodeHash;
    use blaze_usercode::RunKind;

    use super::*;

    const TOOLCHAIN: ContentId = ContentId([1; 32]);
    const SYSTEM: ContentId = ContentId([2; 32]);

    fn serde() -> Origin {
        Origin::CratesIo { name: "serde".into(), version: "1.0.219".into(), checksum: [3; 32] }
    }

    fn rule() -> Rule {
        Rule {
            toolchain: TOOLCHAIN,
            systems: vec![SYSTEM],
            roots: vec![
                Root { path: "/reg/serde-1.0.219".into(), origin: serde() },
                Root { path: "/toolchain".into(), origin: Origin::Toolchain(TOOLCHAIN) },
                Root { path: "/usr".into(), origin: Origin::System(SYSTEM) },
                Root { path: "/work".into(), origin: Origin::Workspace },
                Root { path: "/work/target/build".into(), origin: Origin::Outputs },
                Root { path: "/reg/serde-1.0.219/vendored".into(), origin: Origin::Path },
            ],
            public_env: vec!["TARGET".into()],
        }
    }

    fn private_origins() -> Vec<Origin> {
        vec![
            Origin::Workspace,
            Origin::Path,
            Origin::Git { url: "https://example.com/x.git".into() },
            Origin::Registry { index: "sparse+https://registry.example.com/".into() },
            Origin::Toolchain(ContentId([9; 32])),
            Origin::System(ContentId([9; 32])),
            Origin::CratesIo { name: String::new(), version: "1.0.0".into(), checksum: [0; 32] },
        ]
    }

    #[test]
    fn each_origin_is_public_or_not() {
        let rule = rule();
        for public in [serde(), Origin::Toolchain(TOOLCHAIN), Origin::System(SYSTEM), Origin::Outputs] {
            assert_eq!(rule.origin(&public), Ok(()), "{public}");
        }
        for private in private_origins() {
            assert!(rule.origin(&private).is_err(), "{private}");
        }
    }

    #[test]
    fn a_path_is_what_its_longest_root_says() {
        let rule = rule();
        assert_eq!(rule.path(Path::new("/reg/serde-1.0.219/src/lib.rs")), Ok(&serde()));
        assert_eq!(rule.path(Path::new("/reg/serde-1.0.219/vendored/a.c")), Ok(&Origin::Path));
        assert_eq!(rule.path(Path::new("/work/target/build/x/out/a.rs")), Ok(&Origin::Outputs));
        assert_eq!(rule.path(Path::new("/work/src/main.rs")), Ok(&Origin::Workspace));
        assert!(rule.path(Path::new("/reg/serde-1.0.2190/src/lib.rs")).is_err(), "a root is whole components");
        assert!(rule.path(Path::new("/home/alice/.ssh/id_ed25519")).is_err());
    }

    #[test]
    fn a_query_is_public_when_every_input_is_and_keyed_as_the_store_keys_it() {
        let rule = rule();
        let schema = SchemaHash(ContentId([4; 32]));
        let producer = ContentId([5; 32]);
        let inputs = [(ContentId([6; 32]), serde()), (ContentId([7; 32]), Origin::Toolchain(TOOLCHAIN))];
        let public = rule.query(&QueryMade { schema, producer, inputs: &inputs }).unwrap();
        assert_eq!(public.key(), Key::of(schema, producer, &[ContentId([6; 32]), ContentId([7; 32])]));
        assert_eq!(public.kind(), Kind::Query);
        for private in private_origins() {
            let inputs = [(ContentId([6; 32]), serde()), (ContentId([8; 32]), private.clone())];
            let error = rule.query(&QueryMade { schema, producer, inputs: &inputs }).unwrap_err();
            assert_eq!(error.key, Key::of(schema, producer, &[ContentId([6; 32]), ContentId([8; 32])]));
        }
    }

    fn given() -> Given {
        let mut given = Given::new(RunKind::BuildScript);
        given.env("TARGET", "x86_64-unknown-linux-gnu").env("CARGO_MANIFEST_DIR", "/reg/serde-1.0.219");
        given.env("OUT_DIR", "/work/target/build/serde-1/out");
        given
    }

    fn traced() -> Traced {
        Traced {
            files: vec![
                ("/reg/serde-1.0.219/build.rs".into(), Some([1; 32])),
                ("/usr/include/stdio.h".into(), Some([2; 32])),
                ("/toolchain/lib/rustlib/x".into(), None),
            ],
            dirs: vec![("/reg/serde-1.0.219/src".into(), [3; 32])],
            env: vec![("TARGET".into(), Some("x".into())), ("HOME".into(), None)],
            untraceable: Vec::new(),
        }
    }

    fn run_key(given: &Given) -> RunKey {
        RunKey { code: ContentId([9; 32]), given: given.finish() }
    }

    #[test]
    fn a_public_run_is_keyed_as_usercode_keeps_it() {
        let (rule, given, traced) = (rule(), given(), traced());
        let run = run_key(&given);
        let made = RunMade { run, given: &given, code: &serde(), caller: &serde(), traced: &traced };
        let result = rule.run(&made, RunPart::Result).unwrap();
        assert_eq!((result.key(), result.kind()), (index::result_key(&run, &traced), Kind::Run));
        assert_eq!(rule.run(&made, RunPart::Shape(2)).unwrap().key(), index::slot(&run, 2));
    }

    #[test]
    fn a_run_touching_anything_private_is_not_public() {
        let rule = rule();
        let given = given();
        let run = run_key(&given);
        let check = |traced: &Traced, code: &Origin, caller: &Origin, given: &Given| {
            let made = RunMade { run, given, code, caller, traced };
            rule.run(&made, RunPart::Result).err().map(|it| it.why)
        };
        let traced = traced();
        assert_eq!(check(&traced, &serde(), &serde(), &given), None);
        let changes: Vec<(&str, fn(&mut Traced))> = vec![
            ("/work/src/main.rs", |it| it.files.push(("/work/src/main.rs".into(), Some([0; 32])))),
            ("/home", |it| it.files.push(("/home/alice/.cargo/config.toml".into(), None))),
            ("vendored", |it| it.dirs.push(("/reg/serde-1.0.219/vendored".into(), [0; 32]))),
            ("HOME", |it| it.env[1].1 = Some("/home/alice".into())),
            ("whole environment", |it| it.env.push((trace::ALL_ENV_NAME.into(), Some("h".into())))),
            ("traced", |it| it.untraceable.push(Untraceable::Network)),
        ];
        for (says, change) in changes {
            let mut changed = traced.clone();
            change(&mut changed);
            let why = check(&changed, &serde(), &serde(), &given).unwrap_or_else(|| panic!("{says} is public"));
            assert!(why.contains(says), "{why:?} does not say {says:?}");
        }
        for private in private_origins() {
            assert!(check(&traced, &private, &serde(), &given).unwrap().contains("its code"));
            assert!(check(&traced, &serde(), &private, &given).unwrap().contains("runs for"));
        }
        let mut other = given.clone();
        other.env("OUT_DIR", "/home/alice/proj/target/out");
        assert!(check(&traced, &serde(), &serde(), &other).unwrap().contains("given"));
        let other_run = RunKey { code: run.code, given: other.finish() };
        let made = RunMade { run: other_run, given: &other, code: &serde(), caller: &serde(), traced: &traced };
        assert!(rule.run(&made, RunPart::Result).unwrap_err().why.contains("OUT_DIR names a path"));
    }

    fn spec(backend: Backend, toolchain: ContentId) -> Spec {
        Spec {
            toolchain: Toolchain {
                id: toolchain,
                target: "x86_64-unknown-linux-gnu".into(),
                arch: Arch::X86_64,
                cpu: "x86-64".into(),
                features: Vec::new(),
                cranelift: "0.136".into(),
                llvm: Some("23.1".into()),
                frame_pointers: false,
                relocation: Relocation::Pic,
            },
            backend,
            overflow_checks: false,
            debug_assertions: false,
            panic: Panic::Unwind,
            debuginfo: DebugInfo::None,
            fold: false,
            policy: ContentId([0; 32]),
        }
    }

    fn source(n: u8, settings: Settings, code: Origin) -> FnSource {
        FnSource {
            instance: Instance { code: CodeHash(ContentId([n; 32])), args: ContentId([n + 1; 32]), settings },
            code,
            args: vec![Origin::Toolchain(TOOLCHAIN)],
        }
    }

    #[test]
    fn an_llvm_function_of_public_crates_is_public_and_nothing_else_is() {
        let rule = rule();
        let release = spec(Backend::Llvm { opt: 3 }, TOOLCHAIN);
        let settings = release.settings();
        let function = source(10, settings, serde());
        let built_in = [source(20, settings, Origin::Toolchain(TOOLCHAIN))];
        let imported = [source(30, settings, serde())];
        let imports = Imports { imported: vec![imported[0].instance.key()], ..Imports::default() };
        let made = FunctionMade { spec: &release, function: &function, built_in: &built_in, imports: &imports, imported: &imported };
        let public = rule.function(&made).unwrap();
        let stored = blaze_codegen::driver::stored(function.instance.key(), &[built_in[0].instance.key()]);
        assert_eq!((public.key(), public.kind()), (keys::release(&imports.blob_key(stored)), Kind::Code));
        assert_eq!(rule.side(&ContentId([1; 32]), &public).unwrap().kind(), Kind::Debug);

        let dev = spec(Backend::Cranelift, TOOLCHAIN);
        let dev_fn = source(10, dev.settings(), serde());
        let made_dev = FunctionMade { spec: &dev, function: &dev_fn, ..made };
        assert!(rule.function(&made_dev).unwrap_err().why.contains("LLVM"));

        let other = spec(Backend::Llvm { opt: 3 }, ContentId([9; 32]));
        let other_fn = source(10, other.settings(), serde());
        let made_other = FunctionMade { spec: &other, function: &other_fn, ..made };
        assert!(rule.function(&made_other).unwrap_err().why.contains("pinned"));

        let made_wrong = FunctionMade { spec: &dev, ..made };
        assert!(rule.function(&made_wrong).unwrap_err().why.contains("settings"));

        for private in private_origins() {
            let built_in = [source(20, settings, private.clone())];
            assert!(rule.function(&FunctionMade { built_in: &built_in, ..made }).is_err());
            let mut imported = [source(30, settings, serde())];
            imported[0].args.push(private.clone());
            assert!(rule.function(&FunctionMade { imported: &imported, ..made }).is_err());
        }
        assert!(rule.function(&FunctionMade { imported: &[], ..made }).unwrap_err().why.contains("imports"));
    }

    #[test]
    fn secrets_are_found_and_never_named_by_value() {
        let publisher = crate::sign::tests::publisher(42);
        let mut secrets = Secrets::new();
        secrets.publisher(&publisher);
        secrets.env(
            [
                ("GITHUB_TOKEN".into(), "ghp_abcdefghijklmnop".into()),
                ("TARGET".into(), "x86_64-unknown-linux-gnu".into()),
                ("SHORT".into(), "abc".into()),
            ],
            &["TARGET".into()],
        );
        let key = Key(ContentId([1; 32]));
        assert_eq!(secrets.check(key, b"fn main() { println!(\"x86_64-unknown-linux-gnu abc\") }"), Ok(()));
        let error = secrets.check(key, b"token=ghp_abcdefghijklmnop;").unwrap_err();
        assert!(error.why.contains("GITHUB_TOKEN") && !error.why.contains("ghp_"), "{error}");
        let mut seeded = b"head ".to_vec();
        seeded.extend_from_slice(&[42; 32]);
        assert!(secrets.check(key, &seeded).unwrap_err().why.contains("seed"));
        let hex = crate::sign::hex(&[42; 32]);
        assert!(secrets.check(key, hex.as_bytes()).unwrap_err().why.contains("hex"));
        assert!(Secrets::new().add("x", b"short").is_err());
        assert!(!format!("{secrets:?}").contains("ghp_"));
    }
}
