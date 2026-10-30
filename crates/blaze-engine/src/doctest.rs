//! The doc tests of `blaze test`: the examples in the documentation of a library are
//! taken out as rustdoc takes them, and each is a program of its own, a crate blaze adds to
//! the graph, compiles with everything it links over the standard library's unit and links
//! itself. No rustc, rustdoc or cargo runs: an example rust-analyzer finds an error in, or one
//! blaze cannot build whole, fails and says why. One marked `compile_fail` passes when blaze's
//! front end finds an error in it that rustc gives too, with every error code it is marked
//! with; where blaze can't tell, it fails and says why. What they print and how they end is
//! told as rustdoc's test harness tells it. A run of the tests that asks for what is not done
//! here fails.

pub(crate) use blaze_cargo::doctest::{
    BUILD, FAILED, REPLAY, Recorded, Replay, as_asked, in_place, var_in,
};

mod extract;
mod program;

use std::{
    ffi::OsString,
    io::{IsTerminal as _, Write as _},
    path::{Path, PathBuf},
    process::Command,
    sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Instant,
};

use anyhow::Context as _;
use ide_db::RootDatabase;
use syntax::Edition;

use crate::plan::Selection;
use extract::{Example, Marks};
use program::{Frame, Program};

/// A run of the examples of one library, as cargo asks rustdoc for it.
pub(crate) struct Asked {
    crate_name: String,
    /// The root file of the library, as cargo names it from the package's directory.
    source: PathBuf,
    edition: String,
    /// Where rustdoc was run, and with what environment: what an example reads of either is
    /// what it reads under rustdoc.
    cwd: PathBuf,
    env: Vec<(OsString, OsString)>,
    /// Where the examples run.
    run_in: PathBuf,
    /// The arguments rustc compiles every example with.
    rustc_args: Vec<String>,
    target: Option<String>,
    features: Vec<String>,
    /// The build cargo was asked for, when blaze asked it.
    build: Option<Selection>,
    wanted: Wanted,
    /// Where the programs are built.
    dir: PathBuf,
}

/// Which examples the arguments after `--` ask for, and how the harness shows them.
#[derive(Default)]
struct Wanted {
    filters: Vec<String>,
    skipped: Vec<String>,
    exact: bool,
    /// libtest's `--format=terse`: a character for each example that does not fail.
    terse: bool,
}

impl Wanted {
    /// `Err` is the argument that asks for something not done here.
    fn parse(args: &[String]) -> Result<Wanted, String> {
        let mut wanted = Wanted::default();
        let (mut quiet, mut format) = (false, None);
        let mut args = args.iter();
        while let Some(arg) = args.next() {
            let (flag, joined) = match arg.split_once('=') {
                Some((flag, value)) if flag.starts_with("--") => (flag, Some(value)),
                _ => (arg.as_str(), None),
            };
            let mut value = || joined.or_else(|| args.next().map(String::as_str));
            match flag {
                "--exact" => wanted.exact = true,
                "--skip" => wanted.skipped.extend(value().map(str::to_owned)),
                // The examples run as many at a time as there are processors either way.
                "--test-threads" => _ = value(),
                "-q" | "--quiet" => quiet = true,
                "--format" => match value() {
                    Some(it @ ("pretty" | "terse")) => format = Some(it == "terse"),
                    _ => return Err(arg.clone()),
                },
                // Nothing is shown in color, which is what `auto` is when the output is not
                // a terminal.
                "--color" => match value() {
                    Some("never") => {}
                    Some("auto") if !std::io::stdout().is_terminal() => {}
                    _ => return Err(arg.clone()),
                },
                _ if arg.starts_with('-') => return Err(arg.clone()),
                _ => wanted.filters.push(arg.clone()),
            }
        }
        wanted.terse = format.unwrap_or(quiet);
        Ok(wanted)
    }

    fn has(&self, name: &str) -> bool {
        let is = |it: &String| if self.exact { name == it } else { name.contains(it.as_str()) };
        (self.filters.is_empty() || self.filters.iter().any(is)) && !self.skipped.iter().any(is)
    }
}

/// The flags of rustdoc that take a value and are passed on to rustc for every example.
const TO_RUSTC: [&str; 6] = ["--cfg", "--check-cfg", "-L", "--extern", "-C", "--cap-lints"];

/// The flags of rustdoc that take a value and say nothing of how an example is built. The
/// levels of lints are of the documentation itself: rustdoc compiles no example with them.
const UNUSED: [&str; 12] = [
    "--crate-type",
    "--color",
    "--error-format",
    "--diagnostic-width",
    "--allow",
    "--warn",
    "--deny",
    "--forbid",
    "-A",
    "-W",
    "-D",
    "-F",
];

impl Asked {
    /// Reads the arguments of a run of rustdoc. `Err` is why blaze does not run the
    /// examples: they fail with it.
    fn read(recorded: &Recorded) -> Result<Asked, String> {
        let words: Vec<String> = recorded
            .args
            .iter()
            .map(|it| it.to_str().map(str::to_owned))
            .collect::<Option<_>>()
            .ok_or("an argument is not text")?;
        let cwd = recorded.cwd.clone();
        let (mut test, mut source, mut crate_name) = (false, None, None);
        let (mut edition, mut run_in, mut target) = ("2015".to_owned(), cwd.clone(), None);
        let (mut rustc_args, mut test_args, mut features) = (Vec::new(), Vec::new(), Vec::new());
        let mut words = words.into_iter();
        while let Some(word) = words.next() {
            // A flag and its value are one word with `=` between them, or two.
            let (flag, joined) = match word.split_once('=') {
                Some((flag, value)) if flag.starts_with("--") => {
                    (flag.to_owned(), Some(value.to_owned()))
                }
                _ => (word.clone(), None),
            };
            let mut value =
                || joined.clone().or_else(|| words.next()).ok_or(format!("{flag} has no value"));
            match flag.as_str() {
                "--test" => test = true,
                "--edition" => edition = value()?,
                "--crate-name" => crate_name = Some(value()?),
                "--test-run-directory" => run_in = cwd.join(value()?),
                "--test-args" => {
                    let value = value()?;
                    test_args.extend(
                        shlex::split(&value).ok_or("`--test-args` is not quoted as a shell's")?,
                    );
                }
                "--target" => {
                    let triple = value()?;
                    rustc_args.extend(["--target".to_owned(), triple.clone()]);
                    target = Some(triple);
                }
                _ if TO_RUSTC.contains(&flag.as_str()) => {
                    let value = value()?;
                    if let Some(feature) = feature_of(&flag, &value) {
                        features.push(feature);
                    }
                    rustc_args.extend([flag.clone(), value]);
                }
                _ if UNUSED.contains(&flag.as_str()) => _ = value()?,
                _ if flag.starts_with('-') => return Err(format!("rustdoc is run with `{flag}`")),
                _ if source.is_none() => source = Some(PathBuf::from(word)),
                _ => return Err(format!("rustdoc is given `{word}` too")),
            }
        }
        if !test {
            return Err("rustdoc is not asked to run tests".to_owned());
        }
        let wanted =
            Wanted::parse(&test_args).map_err(|arg| format!("the tests are run with `{arg}`"))?;
        let build = match recorded.var(BUILD) {
            Some(build) => {
                Some(serde_json::from_str(build).map_err(|it| format!("{BUILD}: {it}"))?)
            }
            None => None,
        };
        Ok(Asked {
            crate_name: crate_name.ok_or("rustdoc is not told the name of the crate")?,
            source: source.ok_or("rustdoc is given no source file")?,
            edition,
            cwd,
            env: recorded.env.clone(),
            run_in,
            rustc_args,
            target,
            features,
            build,
            wanted,
            dir: std::env::temp_dir().join(format!("blaze-doctest-{}", std::process::id())),
        })
    }

    /// The arguments of the run of blaze that loads the library the examples are of, with
    /// nothing of rustc's: the examples are built over the standard library's unit, and the
    /// build scripts and proc macros are blaze's (`cargo/compile_time.rs`), as a worker of
    /// the daemon has them, not `cargo check --compile-time-deps`.
    fn build_args(&self) -> anyhow::Result<Vec<String>> {
        std::fs::create_dir_all(&self.dir)?;
        // The package is where cargo runs the examples, not where it runs rustdoc: that is
        // the workspace's root, whose package is another one or none.
        let mut args = vec![
            self.run_in.display().to_string(),
            "--own-target".to_owned(),
            "--crate".to_owned(),
            self.root_file(),
            "--out".to_owned(),
            self.object().display().to_string(),
            "--dev-deps".to_owned(),
        ];
        match &self.build {
            // Built as cargo built it: a dependency of this package and of another one cargo
            // selected has the features both ask for, and its types are laid out with them.
            Some(build) => {
                args.extend(Selection { target: None, ..build.clone() }.flags());
                if let Some(asked_in) = &build.asked_in {
                    args.extend(["--asked-in".to_owned(), asked_in.display().to_string()]);
                }
            }
            // The build scripts run from the workspace's root, whose package is another one:
            // the features are this package's once it is the one selected. `package/feature`
            // would name a dependency of the root's, which this package need not be.
            None => {
                args.push("--no-default-features".to_owned());
                let package = self.var("CARGO_PKG_NAME").filter(|it| !it.is_empty());
                if let Some(package) = package {
                    args.extend(["--package".to_owned(), package.to_owned()]);
                }
                if !self.features.is_empty() {
                    args.extend(["--features".to_owned(), self.features.join(",")]);
                }
            }
        }
        if let Some(target) = &self.target {
            args.extend(["--target".to_owned(), target.clone()]);
        }
        Ok(args)
    }

    /// The root file of the library in full, which names its crate to blaze.
    fn root_file(&self) -> String {
        self.cwd.join(&self.source).display().to_string()
    }

    /// Where the run's `--out` is: nothing is built there, the standard library's unit is
    /// written beside it.
    fn object(&self) -> PathBuf {
        self.dir.join("blaze.o")
    }

    /// The variable of rustdoc's environment called `name`, when it is text.
    fn var(&self, name: &str) -> Option<&str> {
        var_in(&self.env, name)
    }

    /// A command that runs where rustdoc was run and with its environment.
    fn command(&self, program: impl AsRef<std::ffi::OsStr>) -> Command {
        in_place(as_asked(program), &self.cwd, &self.env)
    }

    /// Where blaze's programs of the examples stay from run to run: beside the build's tests,
    /// in `<target>/<profile>/blaze-doctest`. Cargo names the libraries'
    /// directory, `deps` in its first layout and one unit's `build/<package>/<hash>/out` in
    /// its second ([`crate::cargo::layout`]).
    fn memo(&self) -> Option<PathBuf> {
        // In cargo's second layout of the build directory a library of no dependencies is
        // documented with no `-L dependency=`: the directory of the library itself then.
        let dependency = self.rustc_args.windows(2).find_map(|it| match it[0] == "-L" {
            true => it[1].strip_prefix("dependency=").map(Path::new),
            false => None,
        });
        let library = || {
            let externs = self.rustc_args.windows(2).filter(|it| it[0] == "--extern");
            externs.filter_map(|it| Path::new(it[1].split_once('=')?.1).parent()).next()
        };
        let deps = self.cwd.join(dependency.or_else(library)?);
        let profile = match crate::cargo::layout::unit_dir(&deps) {
            Some(unit) => unit.profile()?.to_owned(),
            None => deps.parent()?.to_owned(),
        };
        Some(profile.join("blaze-doctest"))
    }

    /// What tells a program of an example apart from another: everything it is given, and
    /// the libraries named with `--extern` as they are now.
    fn key(&self, source: &str, edition: &str, env: &[(&str, String)], links: bool) -> String {
        use sha2::{Digest as _, Sha256};
        let mut hasher = Sha256::new();
        let mut add = |it: &[u8]| {
            hasher.update((it.len() as u64).to_le_bytes());
            hasher.update(it);
        };
        // The commit too: every build of the tree is `0.0.0-dev`.
        add(env!("BLAZE_VERSION_LINE").as_bytes());
        for name in ["RUSTC", "RUSTUP_TOOLCHAIN"] {
            add(self.var(name).unwrap_or_default().as_bytes());
        }
        for arg in &self.rustc_args {
            add(arg.as_bytes());
        }
        let externs = self.rustc_args.windows(2).filter(|it| it[0] == "--extern");
        for (_, file) in externs.filter_map(|it| it[1].split_once('=')) {
            add(format!("{:?}", stamp(Path::new(file))).as_bytes());
        }
        for (name, value) in env {
            add(name.as_bytes());
            add(value.as_bytes());
        }
        add(edition.as_bytes());
        add(&[u8::from(links)]);
        add(source.as_bytes());
        let digest = hasher.finalize();
        crate::content::hex(&digest[..16])
    }

    /// The path of `file` as rustdoc shows it: from the directory it runs in.
    fn shown(&self, file: &Path) -> String {
        file.strip_prefix(&self.cwd).unwrap_or(file).display().to_string()
    }
}

/// A file's length and when it was written, `None` when there is none.
fn stamp(file: &Path) -> Option<(u64, u128)> {
    let meta = std::fs::metadata(file).ok()?;
    let written = meta.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok()?;
    Some((meta.len(), written.as_nanos()))
}

/// The feature a `--cfg feature="name"` turns on.
fn feature_of(flag: &str, value: &str) -> Option<String> {
    let name = value.strip_prefix("feature=").filter(|_| flag == "--cfg")?;
    Some(name.trim_matches('"').to_owned())
}

/// The libraries whose examples a run of blaze tests over one load of their workspace, in
/// the order cargo ran rustdoc for them.
pub(crate) struct Libraries {
    runs: Vec<(Recorded, Result<Asked, String>)>,
    /// How cargo was asked to run them, when blaze runs them after cargo is done: cargo ran
    /// rustdoc for none of them and said nothing of them.
    replay: Option<Replay>,
}

impl Libraries {
    /// Loads the workspace of the first library blaze can test the examples of and tests
    /// them all, ending as cargo would after them. Each fails when the workspace does not
    /// load.
    fn load(&self) -> anyhow::Result<()> {
        let first = self.runs.iter().find_map(|(_, asked)| asked.as_ref().ok());
        let loaded = match first {
            Some(first) => first
                .build_args()
                .and_then(|args| crate::run(crate::Args::parse(args.into_iter())?, Some(self))),
            None => Ok(()),
        };
        let why = match (first, loaded) {
            (Some(_), Ok(())) => return Ok(()),
            (None, _) => None,
            (Some(_), Err(why)) => Some(format!("{why:#}")),
        };
        self.remove_dirs();
        let code = self.each(|recorded, asked| {
            recorded.refused(asked.err().or(why.as_deref()).unwrap_or("its workspace did not load"))
        });
        std::process::exit(code)
    }

    /// Tests each library with `test`, which is given its run and what blaze makes of it,
    /// until one fails where cargo would stop. The code to end with.
    fn each(&self, mut test: impl FnMut(&Recorded, Result<&Asked, &str>) -> i32) -> i32 {
        let mut test = |(recorded, asked): &(Recorded, Result<Asked, String>)| {
            test(recorded, asked.as_ref().map_err(String::as_str))
        };
        let Some(replay) = &self.replay else {
            return self.runs.iter().map(&mut test).fold(0, |code, it| code.max(it));
        };
        use crate::cargo::say;
        let mut failed = Vec::new();
        for run in &self.runs {
            say::status(replay.quiet, "Doc-tests", run.0.crate_name());
            if test(run) == 0 {
                continue;
            }
            let rerun = format!("-p {} --doc", run.0.var("CARGO_PKG_NAME").unwrap_or_default());
            say::error(&format!("doctest failed, to rerun pass `{rerun}`"));
            if !replay.no_fail_fast {
                return FAILED;
            }
            failed.push(rerun);
        }
        if failed.is_empty() {
            return 0;
        }
        // Cargo lists what failed again when it went on past the first.
        let targets = if failed.len() == 1 { "target" } else { "targets" };
        say::error(&format!("{} {targets} failed:", failed.len()));
        failed.iter().for_each(|it| eprintln!("    `{it}`"));
        FAILED
    }

    /// Removes the directories the programs of the examples were built in.
    fn remove_dirs(&self) {
        for (_, asked) in &self.runs {
            if let Ok(asked) = asked {
                _ = std::fs::remove_dir_all(&asked.dir);
            }
        }
    }
}

/// Makes the runs of rustdoc [`write_run`] wrote down when blaze is run with [`REPLAY`]:
/// the examples of every library, over one load of their workspace.
pub(crate) fn replay() -> Option<anyhow::Result<()>> {
    let mut args = std::env::args().skip(1);
    if args.next().as_deref() != Some(REPLAY) {
        return None;
    }
    let replay: Replay = match serde_json::from_str(&args.next().unwrap_or_default()) {
        Ok(replay) => replay,
        Err(why) => return Some(Err(anyhow::anyhow!("{REPLAY}: {why}"))),
    };
    let runs = match Recorded::all_in(&replay.dir) {
        Ok(runs) => runs,
        Err(why) => return Some(Err(why)),
    };
    let runs = runs.into_iter().map(|it| (Asked::read(&it), it)).map(|(asked, it)| (it, asked));
    Some(Libraries { runs: runs.collect(), replay: Some(replay) }.load())
}

/// What the crate asks of every example with `#![doc(test(..))]`.
#[derive(Default)]
struct OfTheCrate {
    attrs: Vec<String>,
    no_crate_inject: bool,
}

impl OfTheCrate {
    fn read(db: &RootDatabase, krate: hir::Crate) -> OfTheCrate {
        use syntax::{NodeOrToken, SyntaxKind, view as ast};
        fn before(node: ast::Node<'_>) -> Option<&str> {
            let mut prev = node.prev_sibling_or_token();
            while prev.is_some_and(|it| it.kind().is_trivia()) {
                prev = prev.and_then(|it| it.prev_sibling_or_token());
            }
            prev.map(|it| match it {
                NodeOrToken::Node(it) => it.text(),
                NodeOrToken::Token(it) => it.text(),
            })
        }
        let root = krate.root_file(db);
        let parse = syntax::SourceFile::parse(db.file_text(root).text(db), Edition::CURRENT);
        let mut of_the_crate = OfTheCrate::default();
        let trees = parse.root().descendants().filter(|it| it.kind() == SyntaxKind::TOKEN_TREE);
        // `test(..)` directly inside `doc(..)` of an attribute of the file itself.
        for tree in trees {
            let outer = tree.parent().filter(|it| it.kind() == SyntaxKind::TOKEN_TREE);
            let in_doc = outer.is_some_and(|it| before(it) == Some("doc"));
            if before(tree) != Some("test") || !in_doc {
                continue;
            }
            for inner in tree.children().filter(|it| it.kind() == SyntaxKind::TOKEN_TREE) {
                if before(inner) == Some("attr") {
                    let text = inner.text();
                    of_the_crate.attrs.extend(split_attrs(&text[1..text.len() - 1]));
                }
            }
            let words = tree.children_with_tokens().filter_map(|it| it.into_token());
            of_the_crate.no_crate_inject |=
                words.into_iter().any(|it| it.text() == "no_crate_inject");
        }
        of_the_crate
    }
}

/// The attributes in a list separated by commas, which their own arguments have too.
fn split_attrs(list: &str) -> Vec<String> {
    let (mut attrs, mut depth, mut start) = (Vec::new(), 0, 0);
    for (at, byte) in list.bytes().enumerate() {
        match byte {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth -= 1,
            b',' if depth == 0 => {
                attrs.push(list[start..at].trim().to_owned());
                start = at + 1;
            }
            _ => {}
        }
    }
    attrs.push(list[start..].trim().to_owned());
    attrs.retain(|it| !it.is_empty());
    attrs
}

/// How many examples' programs blaze built and how many it could not, and how many of
/// blaze's were taken from an earlier run.
#[derive(Default)]
struct Counted {
    blaze: AtomicUsize,
    refused: AtomicUsize,
    /// Of `refused`, the examples marked `compile_fail` that blaze can't tell fail as marked.
    unconfirmed: AtomicUsize,
    remembered: AtomicUsize,
}

/// How an example ended.
enum Ended {
    Passed,
    Ignored,
    /// With what the harness shows of it.
    Failed(String),
}

/// One example ready to build: its name as the harness shows it and its program.
struct Test {
    name: String,
    marks: Marks,
    program: Program,
    /// The path rustc is to show for the example.
    shown_path: String,
    edition: Edition,
    /// Whether rustdoc builds the example in one program with the others, as it does from
    /// edition 2024: the test harness of that program names it.
    in_one: bool,
}

/// What blaze built of an example.
enum Ours {
    /// Nothing, the example being only to compile, and rust-analyzer finds no error in it.
    Checked,
    /// Nothing, the example being marked `compile_fail`, and failing as marked
    /// ([`Found::fails_as_marked`]).
    FailsToCompile,
    /// Its object and the standard library's unit, to link in `dir` with `line`. A `kept`
    /// directory is the memo's, where the program stays for the next run.
    Compiled { objects: [PathBuf; 2], dir: PathBuf, line: crate::link::Line, kept: bool },
    /// The program, linked in an earlier run from what is unchanged since.
    Linked(PathBuf),
}

impl Ours {
    /// Links the program, which calls nothing blaze could not compile.
    fn link(&self) -> anyhow::Result<PathBuf> {
        let (objects, dir, line, kept) = match self {
            Ours::Compiled { objects, dir, line, kept } => (objects, dir, line, *kept),
            Ours::Linked(binary) => return Ok(binary.clone()),
            Ours::Checked | Ours::FailsToCompile => anyhow::bail!("nothing to link"),
        };
        let binary = dir.join("rust_out");
        // A name the standard library's unit refused, which nothing defines then.
        let refused = |failed: anyhow::Error| {
            let refused = crate::refused_undefined(&format!("{failed:#}"), &objects[1]);
            match refused.first() {
                Some(first) => anyhow::anyhow!(
                    "it calls {} functions blaze could not compile: {first}",
                    refused.len()
                ),
                None => failed,
            }
        };
        let linked = line.link(&[&objects[0], &objects[1]], &binary).map_err(refused);
        let linked = linked.and_then(|_| {
            // On Windows the executable keeps every stand-in of the unit, reached or not
            // (`link_whole`): one the example calls says so when it runs.
            if cfg!(windows) {
                return Ok(());
            }
            let stand_ins = crate::link::stand_ins(&binary)?;
            match stand_ins.first() {
                Some(first) => anyhow::bail!(
                    "it calls {} functions blaze could not compile: {first}",
                    stand_ins.len()
                ),
                None => Ok(()),
            }
        });
        if kept {
            _ = std::fs::remove_file(&objects[0]);
            if linked.is_err() {
                _ = std::fs::remove_dir_all(dir);
            }
        }
        linked.map(|()| binary)
    }

    /// The directory to remove once the program ran: one that is not the memo's.
    fn scratch(&self) -> Option<&Path> {
        match self {
            Ours::Compiled { dir, kept: false, .. } => Some(dir),
            _ => None,
        }
    }
}

impl Test {
    fn of(asked: &Asked, example: Example, frame: &Frame<'_>) -> Test {
        let shown_path = asked.shown(&example.file);
        let item =
            if example.item.is_empty() { String::new() } else { format!("{} ", example.item) };
        let name = format!("{shown_path} - {item}(line {})", example.line);
        let id: String = format!("{shown_path}_{}_0", example.line)
            .chars()
            .map(|it| if it.is_ascii_alphanumeric() { it } else { '_' })
            .collect();
        let edition = example.marks.edition.as_deref().unwrap_or(&asked.edition);
        let edition = edition.parse().unwrap_or(Edition::CURRENT);
        let program = Program::of(&example.code, frame, &id, edition);
        let in_one = edition.at_least_2024();
        Test { name, marks: example.marks, program, shown_path, edition, in_one }
    }

    /// The name with what the harness adds for how the example is tested.
    fn shown_name(&self) -> String {
        let how = match &self.marks {
            it if it.compile_fail => " - compile fail",
            it if it.no_run => " - compile",
            it if it.should_panic && self.in_one => " - should panic",
            _ => "",
        };
        format!("{}{how}", self.name)
    }

    fn is_ignored(&self, target: &str) -> bool {
        self.marks.ignore || self.marks.ignored_on.iter().any(|it| target.contains(it.as_str()))
    }

    /// Links and runs what blaze built of the example. `ours` is why blaze built nothing,
    /// for one it does not: the example fails with it.
    fn run(
        &self,
        asked: &Asked,
        ours: Result<&Ours, &str>,
        target: &str,
        counted: &Counted,
    ) -> anyhow::Result<Ended> {
        if self.is_ignored(target) {
            return Ok(Ended::Ignored);
        }
        let marks = &self.marks;
        let refused = |why: String| {
            counted.refused.fetch_add(1, Ordering::Relaxed);
            if marks.compile_fail {
                counted.unconfirmed.fetch_add(1, Ordering::Relaxed);
            }
            Ok(Ended::Failed(format!("blaze did not build the example: {why}")))
        };
        let binary = match ours {
            Ok(Ours::Checked) => {
                counted.blaze.fetch_add(1, Ordering::Relaxed);
                return Ok(Ended::Passed);
            }
            Ok(Ours::FailsToCompile) => {
                counted.blaze.fetch_add(1, Ordering::Relaxed);
                return Ok(Ended::Passed);
            }
            Ok(ours) => match ours.link() {
                Ok(binary) => binary,
                Err(why) => return refused(format!("{why:#}")),
            },
            Err(why) => return refused(why.to_owned()),
        };
        counted.blaze.fetch_add(1, Ordering::Relaxed);
        let ran = match asked.command(&binary).current_dir(&asked.run_in).output() {
            Ok(ran) => ran,
            Err(error) => return Ok(Ended::Failed(format!("Couldn't run the test: {error}"))),
        };
        Ok(match (ran.status.success(), marks.should_panic) {
            (true, false) | (false, true) => Ended::Passed,
            (true, true) => Ended::Failed(
                "Test executable succeeded, but it's marked `should_panic`.".to_owned(),
            ),
            (false, false) => Ended::Failed(failure(&ran)),
        })
    }
}

/// libtest's line of the results of `--format=terse` ends after this many of them.
const TERSE_COLUMNS: usize = 88;

/// What the harness has shown of the examples that ended so far.
#[derive(Default)]
struct Progress {
    terse: bool,
    total: usize,
    count: usize,
    column: usize,
}

impl Progress {
    /// Shows how `test` ended as libtest's pretty or terse output does.
    fn ended(
        &mut self,
        out: &mut impl std::io::Write,
        test: &Test,
        result: &anyhow::Result<Ended>,
    ) -> std::io::Result<()> {
        if !self.terse {
            let word = match result {
                Ok(Ended::Passed) => "ok",
                Ok(Ended::Ignored) => "ignored",
                Ok(Ended::Failed(_)) | Err(_) => "FAILED",
            };
            return writeln!(out, "test {} ... {word}", test.shown_name());
        }
        let short = match result {
            Ok(Ended::Passed) => ".",
            Ok(Ended::Ignored) => "i",
            Ok(Ended::Failed(_)) | Err(_) => {
                // A failure is on a line of its own, after the progress of the one before.
                if self.column != 0 {
                    self.progress(out)?;
                }
                self.count += 1;
                return writeln!(out, "{} --- FAILED", test.name);
            }
        };
        out.write_all(short.as_bytes())?;
        self.count += 1;
        self.column += 1;
        if self.column % TERSE_COLUMNS == TERSE_COLUMNS - 1 {
            self.progress(out)?;
        }
        out.flush()
    }

    fn progress(&mut self, out: &mut impl std::io::Write) -> std::io::Result<()> {
        self.column = 0;
        writeln!(out, " {}/{}", self.count, self.total)
    }
}

/// What the harness shows of an example that ran and failed.
fn failure(ran: &std::process::Output) -> String {
    let (stdout, stderr) =
        (String::from_utf8_lossy(&ran.stdout), String::from_utf8_lossy(&ran.stderr));
    let mut shown = format!("Test executable failed ({}).", ran.status);
    if !stdout.is_empty() || !stderr.is_empty() {
        shown.push('\n');
        if !stdout.is_empty() {
            shown.push_str(&format!("\nstdout:\n{stdout}"));
        }
        if !stderr.is_empty() {
            shown.push_str(&format!("\nstderr:\n{stderr}"));
        }
    }
    shown
}

/// Has `krate` be read as rustdoc reads a crate whose examples it tests, with `cfg(doc)` and
/// `cfg(doctest)`: documentation that is only there for them has examples too.
fn document(db: &mut RootDatabase, krate: hir::Crate) {
    use ide_db::base_db::salsa::Setter as _;
    let krate: ide_db::base_db::Crate = krate.into();
    let mut cfg = krate.cfg_options(db).clone();
    cfg.insert_atom(intern::Symbol::intern("doc"));
    cfg.insert_atom(intern::Symbol::intern("doctest"));
    krate.set_cfg_options(db).to(cfg);
}

/// The examples of the library, each as the program rustdoc makes of it, and how many the
/// arguments leave out.
fn tests(db: &RootDatabase, asked: &Asked, krate: hir::Crate) -> (Vec<Test>, usize) {
    let of_the_crate = OfTheCrate::read(db, krate);
    let frame = Frame {
        crate_name: &asked.crate_name,
        attrs: &of_the_crate.attrs,
        no_crate_inject: of_the_crate.no_crate_inject,
    };
    let mut tests: Vec<Test> =
        extract::examples(db, krate).into_iter().map(|it| Test::of(asked, it, &frame)).collect();
    let all = tests.len();
    tests.retain(|it| asked.wanted.has(&it.name));
    tests.sort_by(|a, b| a.name.cmp(&b.name));
    let left_out = all - tests.len();
    (tests, left_out)
}

/// How an example is built.
enum Plan {
    /// Not at all, and why: it fails.
    Refused(String),
    /// Not at all: it is only to compile, and rust-analyzer finds no error in it.
    Checked,
    /// Not at all: it is marked `compile_fail`, and fails as marked.
    FailsToCompile,
    /// By blaze, from its crate of this name.
    Ours(String),
}

/// How each of `tests` is built, by its place, and the standard library's unit the ones
/// blaze builds link: by blaze when rust-analyzer finds no error in it, else not, and why.
fn plan(
    db: &mut RootDatabase,
    building: &Building<'_>,
    asked: &Asked,
    krate: hir::Crate,
    tests: &[Test],
    target: &str,
    files: &mut Vec<vfs::FileId>,
) -> (Vec<Plan>, Option<PathBuf>) {
    let refused = |why: &str| tests.iter().map(|_| Plan::Refused(why.to_owned())).collect();
    let wanted: Vec<usize> = (0..tests.len()).filter(|&at| !tests[at].is_ignored(target)).collect();
    if wanted.is_empty() {
        return (refused("it is ignored"), None);
    }
    let unit = match crate::std_unit(db, building.table, building.args, building.listed) {
        Ok(Some(unit)) => unit,
        Ok(None) => return (refused("the standard library is not built apart"), None),
        Err(error) => return (refused(&format!("{error:#}")), None),
    };
    let mut plans: Vec<Plan> = refused("blaze could not add it to the workspace");
    let mixing = with_another_copy(db, krate);
    let added = match add_crates(db, asked, krate, tests, &wanted, files) {
        Ok(added) => added,
        Err(why) => return (refused(&why), None),
    };
    for (at, example) in added {
        let test = &tests[at];
        let source = &test.program.source;
        let names = |name: &String| {
            source.contains(&format!("{name}::")) || source.contains(&format!("crate {name}"))
        };
        let found = || Found::in_file(db, example.root_file, denies_lints(source));
        if let Some(name) = mixing.iter().find(|it| names(it)) {
            let why = format!("`{name}` links another copy of the library");
            plans[at] = Plan::Refused(why);
        } else if test.marks.compile_fail {
            plans[at] = match found().fails_as_marked(&test.marks.error_codes) {
                Ok(()) => Plan::FailsToCompile,
                Err(why) => Plan::Refused(why),
            };
        } else if let Some(error) = found().first() {
            plans[at] = Plan::Refused(format!("blaze finds an error in it: {error}"));
        } else if test.marks.no_run {
            plans[at] = Plan::Checked;
        } else {
            plans[at] = Plan::Ours(example.name);
        }
    }
    (plans, Some(unit))
}

/// What blaze-diagnostics finds in an example's crate: the errors rustc gives, and what blaze
/// cannot check, which refuses the example as an error would. Where the program denies lints,
/// as `#![doc(test(attr(deny(warnings))))]` has it do, a use of something deprecated is an
/// error too, which blaze-diagnostics does not give yet.
#[derive(Default)]
struct Found {
    /// rustc's errors, each by its code (an error's, a lint's name, or empty for none), with
    /// what it says.
    errors: Vec<(String, String)>,
    /// What blaze cannot check, as its errors say it ("blaze cannot borrow-check ...").
    cannot: Vec<String>,
    deprecated: bool,
}

impl Found {
    fn in_file(db: &RootDatabase, file: vfs::FileId, denies_lints: bool) -> Found {
        let mut found = Found::default();
        let Some(&krate) = ide_db::base_db::relevant_crates(db, file).first() else {
            found.cannot.push("blaze cannot check the example: its file is in no crate".to_owned());
            return found;
        };
        // An example's errors are told apart by their codes and messages, not their places.
        let paths = std::collections::HashMap::new();
        let files = blaze_diagnostics::Files::new(&paths, "");
        let options = blaze_diagnostics::Options::default();
        hir::attach_db(db, || {
            let diags = blaze_diagnostics::check_crate(db, hir::Crate::from(krate), files, &options);
            let errors = diags.into_iter().filter(|it| it.is_error());
            for diag in errors.filter(|it| !it.message.starts_with("aborting due to")) {
                if diag.message.starts_with("blaze cannot") {
                    found.cannot.push(diag.message);
                    continue;
                }
                let code = diag.code.as_ref().map_or_else(String::new, |it| it.as_str().to_owned());
                found.errors.push((code, diag.message));
            }
            found.deprecated = denies_lints && uses_deprecated(db, file);
        });
        found
    }

    /// The first error, as the example's refusal says it.
    fn first(&self) -> Option<String> {
        if let Some((code, message)) = self.errors.first() {
            return Some(match code.is_empty() {
                true => message.clone(),
                false => format!("`{code}`: {message}"),
            });
        }
        if let Some(cannot) = self.cannot.first() {
            return Some(cannot.clone());
        }
        self.deprecated.then(|| "it uses something `#[deprecated]`".to_owned())
    }

    /// Whether the example fails to compile as one marked `compile_fail` with `codes` is to:
    /// with an error rustc gives, and every code among them, as rustdoc checks it. `Err` is
    /// why it does not, or why blaze can't tell, which the example fails with.
    fn fails_as_marked(&self, codes: &[String]) -> Result<(), String> {
        let found = |code: &String| self.errors.iter().any(|(it, _)| it == code);
        let missing: Vec<&String> = codes.iter().filter(|it| !found(it)).collect();
        if !self.errors.is_empty() && missing.is_empty() {
            return Ok(());
        }
        let marked = std::iter::once("compile_fail").chain(codes.iter().map(String::as_str));
        let marked = format!("it is marked `{}`", marked.collect::<Vec<_>>().join(","));
        let mut why = match self.errors.is_empty() {
            true => format!("{marked}, and blaze finds no error in it"),
            false => {
                let coded: Vec<&String> =
                    self.errors.iter().map(|(code, _)| code).filter(|it| !it.is_empty()).collect();
                let named = match coded.is_empty() {
                    true => "only errors with no code".to_owned(),
                    false => quoted(coded),
                };
                format!("{marked}, and blaze finds {named} in it but not {}", quoted(missing))
            }
        };
        if let Some(cannot) = self.cannot.first() {
            why.push_str(&format!(": {cannot}"));
        }
        Err(why)
    }
}

/// Error codes as a refusal lists them.
fn quoted<'a>(codes: impl IntoIterator<Item = &'a String>) -> String {
    codes.into_iter().map(|it| format!("`{it}`")).collect::<Vec<_>>().join(", ")
}

/// Whether the program has an inner attribute that denies or forbids lints.
fn denies_lints(source: &str) -> bool {
    let attrs = source.lines().map(str::trim_start);
    attrs.into_iter().any(|it| it.starts_with("#![deny") || it.starts_with("#![forbid"))
}

/// Whether the file names an item, or calls a method, marked `#[deprecated]`, the arguments
/// of its macros (`assert_eq!`) included.
fn uses_deprecated(db: &RootDatabase, file: vfs::FileId) -> bool {
    use syntax::view::{self as ast, AstNode as _};
    let sema = hir::view::Semantics::new(db);
    // alloc: the stack of an example's file and its expansions, once per example
    let mut trees = vec![sema.parse_guess_edition(file).syntax()];
    while let Some(tree) = trees.pop() {
        for node in tree.descendants() {
            let used = if let Some(call) = ast::MacroCall::cast(node) {
                trees.extend(sema.expand_macro_call(call).map(|it| it.value));
                None
            } else if let Some(path) = ast::Path::cast(node) {
                match sema.resolve_path(path) {
                    Some(hir::PathResolution::Def(def)) => Some(def),
                    _ => None,
                }
            } else {
                ast::MethodCallExpr::cast(node)
                    .and_then(|call| sema.resolve_method_call(call))
                    .map(hir::ModuleDef::Function)
            };
            if used.and_then(|it| it.attrs(db)).is_some_and(|it| it.is_deprecated()) {
                return true;
            }
        }
    }
    false
}

/// The names of the dependencies of the library that link another copy of it, as a
/// dev-dependency on a crate that depends on it from crates.io does. Its types and the
/// library's are not each other's, which rustc says where an example mixes them, and
/// rust-analyzer's checks do not.
fn with_another_copy(db: &RootDatabase, library: hir::Crate) -> Vec<String> {
    let library: ide_db::base_db::Crate = library.into();
    let name = |krate: ide_db::base_db::Crate| {
        hir::Crate::from(krate).display_name(db).map(|it| it.crate_name().as_str().to_owned())
    };
    let Some(own) = name(library) else { return Vec::new() };
    let dependencies = library.data(db).dependencies.iter();
    let mixing = dependencies.filter(|dependency| {
        let linked = dependency.crate_id.transitive_deps(db);
        linked.iter().copied().any(|it| it != library && name(it).as_deref() == Some(own.as_str()))
    });
    mixing.map(|it| it.name.as_str().to_owned()).collect()
}

/// The crate of an example in the graph.
struct Added {
    name: String,
    root_file: vfs::FileId,
}

/// The first of the source roots of the examples' crates, counted down from: ids no source root
/// of the workspace has.
const FIRST_SOURCE_ROOT: u32 = u32::MAX;

/// Adds a crate for each example of `tests` at `wanted`: a binary whose one file has the
/// program of the example, its lines numbered as rustdoc has rustc number them, under the path
/// of the file the example is in, and that depends on the library and on what the library
/// depends on, with its cfgs and variables. The `n`th example's file is `files[n]`, taken from
/// base-db the first time a library has that many: the examples of each library replace the
/// last one's, so their files are written again rather than numbered anew.
fn add_crates(
    db: &mut RootDatabase,
    asked: &Asked,
    library: hir::Crate,
    tests: &[Test],
    wanted: &[usize],
    files: &mut Vec<vfs::FileId>,
) -> Result<Vec<(usize, Added)>, String> {
    use ide_db::base_db::{
        self, AbsPathBuf, BuiltCrateData, BuiltDependency, CrateDisplayName, CrateName,
        CrateOrigin, ExtraCrateData, FileSet, SourceRoot, SourceRootId, TargetKind, VfsPath,
        salsa::Durability,
    };
    let library: base_db::Crate = library.into();
    let data = library.data(db).clone();
    let named = CrateName::normalize_dashes(&asked.crate_name);
    let dependencies: Vec<BuiltDependency> = std::iter::once(BuiltDependency::new(named, library))
        .chain(data.dependencies.iter().cloned())
        .collect();
    let (workspace, cfg, env) = (
        library.workspace_data(db).clone(),
        library.cfg_options(db).clone(),
        library.env(db).clone(),
    );
    let (mut added, mut crates) = (Vec::new(), Vec::new());
    for (n, &at) in wanted.iter().enumerate() {
        let test = &tests[at];
        let path = asked.cwd.join(&test.shown_path);
        let Ok(vfs_path) = AbsPathBuf::try_from(path.to_string_lossy().as_ref()) else {
            continue;
        };
        let file = match files.get(n) {
            Some(&file) => file,
            None => {
                let file = db.new_file_id().ok_or_else(|| {
                    format!("base-db has no file ID left for example {n} of `{}`", asked.crate_name)
                })?;
                files.push(file);
                file
            }
        };
        let root = SourceRootId(FIRST_SOURCE_ROOT - n as u32);
        db.set_file_text_with_durability(file, &test.program.source, Durability::LOW);
        db.set_file_source_root_with_durability(file, root, Durability::LOW);
        let mut files = FileSet::default();
        files.insert(file, VfsPath::from(vfs_path));
        let source_root = triomphe::Arc::new(SourceRoot::new_local(files));
        db.set_source_root_with_durability(root, source_root, Durability::LOW);
        let name = format!("rust_out_{at}");
        let data = BuiltCrateData {
            root_file_id: file,
            edition: test.edition,
            dependencies: dependencies.clone(),
            origin: CrateOrigin::Local { repo: None, name: None },
            crate_attrs: Box::default(),
            target_kind: TargetKind::Bin,
            proc_macro_cwd: data.proc_macro_cwd.clone(),
        };
        let extra = ExtraCrateData {
            version: None,
            display_name: Some(CrateDisplayName::from_canonical_name(&name)),
            potential_cfg_options: None,
        };
        let krate =
            base_db::Crate::builder(data, extra, workspace.clone(), cfg.clone(), env.clone())
                .durability(Durability::MEDIUM)
                .new(db);
        crates.push(krate);
        added.push((at, Added { name, root_file: file }));
    }
    let all: Vec<base_db::Crate> = base_db::all_crates(db).iter().copied().chain(crates).collect();
    base_db::set_all_crates_with_durability(db, all, Durability::MEDIUM);
    crate::prepare_macro_sessions(db);
    Ok(added)
}

/// What the examples of `library` are compiled from besides their own text, as a hash: each
/// crate of the library and of what it depends on, with its cfgs, and the files of those of
/// the workspace as they are now. Rustdoc's run under cargo names the libraries cargo built
/// with `--extern`, whose files [`Asked::key`] stamps; blaze's own run names none, and
/// compiles them from these.
fn built_from(db: &RootDatabase, library: ide_db::base_db::Crate) -> String {
    use sha2::{Digest as _, Sha256};
    let mut crates: Vec<String> = library
        .transitive_deps(db)
        .iter()
        .copied()
        .map(|krate| {
            let mut hasher = Sha256::new();
            hasher.update(crate::content::Contents::version(db, krate));
            let mut cfgs: Vec<String> =
                krate.cfg_options(db).into_iter().map(|it| it.to_string()).collect();
            cfgs.sort_unstable();
            hasher.update(cfgs.join("\n"));
            let data = krate.data(db);
            if data.origin.is_local() {
                let root = db.file_source_root(data.root_file_id).source_root_id(db);
                let root = db.source_root(root).source_root(db);
                let mut files: Vec<(String, [u8; 32])> = root
                    .iter()
                    .filter_map(|file| {
                        let path = root.path_for_file(&file)?.as_path()?.to_string();
                        Some((path.clone(), crate::content::file_hash(db, file, Path::new(&path))))
                    })
                    .collect();
                files.sort_unstable();
                for (path, hash) in files {
                    hasher.update((path.len() as u64).to_le_bytes());
                    hasher.update(path);
                    hasher.update(hash);
                }
            }
            crate::content::hex(&hasher.finalize())
        })
        .collect();
    crates.sort_unstable();
    crates.join(",")
}

/// Compiles the example whose crate is `name` with everything it links over the standard
/// library's `unit`, into an object in a directory of its own, to link with nothing of
/// rustc's. The program blaze linked of it in an earlier run is taken again while what it
/// was built from is unchanged ([`Asked::key`], and `built_from` of [`built_from`]), and while
/// blaze is.
fn compile(
    db: &RootDatabase,
    building: &Building<'_>,
    asked: &Asked,
    test: &Test,
    name: &str,
    unit: &Path,
    built_from: &str,
) -> anyhow::Result<Ours> {
    // What a program reads while it is compiled, besides the libraries, is named by rustc's
    // dep-info only, which blaze does not write.
    let reads = ["include!", "include_str!", "include_bytes!", "env!"];
    let reads = reads.iter().any(|it| test.program.source.contains(it));
    let blaze = std::env::current_exe().ok().and_then(|it| stamp(&it));
    // Examples of the same code and line (a file included in two places, or one item's and
    // the crate's) are compiled at once: each has a directory of its own to link in.
    let env = [
        ("BLAZE", format!("{blaze:?}")),
        ("built from", built_from.to_owned()),
        ("example", test.name.clone()),
    ];
    let edition = format!("{:?}", test.edition);
    let memo = asked.memo().filter(|_| !reads && blaze.is_some()).map(|memo| {
        memo.join(format!("blaze-{}", asked.key(&test.program.source, &edition, &env, true)))
    });
    if let Some(binary) = memo.as_ref().map(|it| it.join("rust_out")).filter(|it| it.is_file()) {
        return Ok(Ours::Linked(binary));
    }
    let kept = memo.as_ref().is_some_and(|it| std::fs::create_dir_all(it).is_ok());
    let dir = memo.filter(|_| kept).unwrap_or_else(|| asked.dir.join(name));
    std::fs::create_dir_all(&dir)?;
    let args = crate::Args {
        krate: name.to_owned(),
        batch: Vec::new(),
        widened: Vec::new(),
        out: dir.join("example.o"),
        exe: None,
        report: None,
        prebuilt: Some(unit.to_owned()),
        library: false,
        ..building.args.clone()
    };
    let built = || {
        let mut known = crate::Rejections::default();
        let built = crate::build_unheld(db, building.table, &args, &mut known, building.listed)?;
        if let Some(unset) = &built.unset {
            anyhow::bail!("{unset}");
        }
        if let Some(why) = built.refused_reached(&args) {
            anyhow::bail!(why);
        }
        let example = crate::crates_named(db, name, &args)?;
        crate::link::Line::of(db, &example, building.sysroot, false, &[], args.debug.level)
    };
    match built() {
        Ok(line) => {
            Ok(Ours::Compiled { objects: [args.out.clone(), unit.to_owned()], dir, line, kept })
        }
        Err(why) => {
            if kept {
                _ = std::fs::remove_dir_all(&dir);
            }
            Err(why)
        }
    }
}

/// What the examples are built with: the run of blaze that loaded the library, with nothing
/// of rustc's.
pub(crate) struct Building<'a> {
    pub(crate) table: &'a crate::SymbolTable,
    pub(crate) args: &'a crate::Args,
    pub(crate) listed: &'a crate::Listed,
    pub(crate) sysroot: Option<&'a Path>,
}

/// Runs the examples of each library for `target` over the workspace `building` loaded, as
/// rustdoc would have one after another. The code to end with.
pub(crate) fn run_all(
    db: &mut RootDatabase,
    building: &Building<'_>,
    libraries: &Libraries,
    target: &str,
) -> i32 {
    use ide_db::base_db::{self, salsa::Durability};
    let mut files = Vec::new();
    let code = libraries.each(|recorded, asked| {
        let asked = match asked {
            Ok(asked) => asked,
            Err(why) => return recorded.refused(why),
        };
        let args = crate::Args {
            krate: asked.root_file(),
            batch: Vec::new(),
            widened: Vec::new(),
            ..building.args.clone()
        };
        let building = Building { args: &args, ..*building };
        let crates: Vec<base_db::Crate> = base_db::all_crates(db).iter().copied().collect();
        let ran = crate::crates_named(db, &args.krate, &args).and_then(|found| {
            let krate = *found.first().context("no such crate")?;
            run(db, &building, asked, krate, target, &mut files)
        });
        // The examples of the next library are crates of the same names.
        base_db::set_all_crates_with_durability(db, crates, Durability::MEDIUM);
        crate::prepare_macro_sessions(db);
        match ran {
            Ok(code) => code,
            Err(why) => recorded.refused(&format!("{why:#}")),
        }
    });
    libraries.remove_dirs();
    code
}

/// Runs the examples of `krate` for `target` and says how they went as the test harness does.
/// The code to end with.
fn run(
    db: &mut RootDatabase,
    building: &Building<'_>,
    asked: &Asked,
    krate: hir::Crate,
    target: &str,
    files: &mut Vec<vfs::FileId>,
) -> anyhow::Result<i32> {
    use ide_db::base_db::salsa::Setter as _;
    let start = Instant::now();
    // The examples are taken out of the library as rustdoc reads it.
    let library: ide_db::base_db::Crate = krate.into();
    let cfg = library.cfg_options(db).clone();
    document(db, krate);
    crate::prepare_macro_sessions(db);
    let (tests, left_out) = hir::attach_db(db, || tests(db, asked, krate));
    library.set_cfg_options(db).to(cfg);
    crate::prepare_macro_sessions(db);
    let built_from = built_from(db, library);
    let (plans, unit) = plan(db, building, asked, krate, &tests, target, files);
    let db: &RootDatabase = db;
    let out = std::io::stdout();
    writeln!(&out, "\nrunning {} test{}", tests.len(), if tests.len() == 1 { "" } else { "s" })?;
    let shown = Mutex::new(Progress {
        terse: asked.wanted.terse,
        total: tests.len(),
        ..Default::default()
    });
    let ended: Mutex<Vec<(usize, anyhow::Result<Ended>)>> = Mutex::new(Vec::new());
    let counted = Counted::default();
    // The examples are compiled a few at a time on the one shared database (a small
    // program's build leaves most cores idle), and each is linked, run and removed as soon as
    // it is compiled.
    let (ready, next) = std::sync::mpsc::channel::<(usize, Result<Ours, String>)>();
    let next = Mutex::new(next);
    let cores = std::thread::available_parallelism().map_or(1, |it| it.get());
    let threads = cores.min(tests.len());
    let ours: Vec<(usize, &String)> = match &unit {
        Some(_) => plans
            .iter()
            .enumerate()
            .filter_map(|(at, plan)| match plan {
                Plan::Ours(name) => Some((at, name)),
                _ => None,
            })
            .collect(),
        None => Vec::new(),
    };
    let compilers = (cores / 16).clamp(1, 4).min(ours.len());
    let ours = Mutex::new(ours.into_iter());
    let compiling = Instant::now();
    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| {
                loop {
                    let job = next.lock().unwrap_or_else(|it| it.into_inner()).recv();
                    let Ok((at, ours)) = job else { break };
                    let test = &tests[at];
                    let result =
                        test.run(asked, ours.as_ref().map_err(String::as_str), target, &counted);
                    if let Some(dir) = ours.as_ref().ok().and_then(Ours::scratch) {
                        _ = std::fs::remove_dir_all(dir);
                    }
                    let mut progress = shown.lock().unwrap_or_else(|it| it.into_inner());
                    _ = progress.ended(&mut out.lock(), test, &result);
                    drop(progress);
                    ended.lock().unwrap_or_else(|it| it.into_inner()).push((at, result));
                }
            });
        }
        for (at, plan) in plans.iter().enumerate() {
            match plan {
                Plan::Refused(why) => _ = ready.send((at, Err(why.clone()))),
                Plan::Checked => _ = ready.send((at, Ok(Ours::Checked))),
                Plan::FailsToCompile => _ = ready.send((at, Ok(Ours::FailsToCompile))),
                Plan::Ours(_) => {}
            }
        }
        let compilers: Vec<_> = (0..compilers)
            .map(|_| {
                let (ready, ours) = (ready.clone(), &ours);
                let unit = unit.as_deref();
                let (tests, counted, built_from) = (&tests, &counted, &built_from);
                scope.spawn(move || {
                    let Some(unit) = unit else { return };
                    loop {
                        let job = ours.lock().unwrap_or_else(|it| it.into_inner()).next();
                        let Some((at, name)) = job else { break };
                        let test = &tests[at];
                        let compiled = compile(db, building, asked, test, name, unit, built_from);
                        if let Ok(Ours::Linked(_)) = &compiled {
                            counted.remembered.fetch_add(1, Ordering::Relaxed);
                        }
                        _ = ready.send((at, compiled.map_err(|why| format!("{why:#}"))));
                    }
                })
            })
            .collect();
        drop(ready);
        for compiler in compilers {
            _ = compiler.join();
        }
    });
    let compiling = compiling.elapsed();
    let remembered = counted.remembered.load(Ordering::Relaxed);
    let remembered = match remembered {
        0 => String::new(),
        n => format!(" ({n} unchanged since the last run)"),
    };
    let unconfirmed = match counted.unconfirmed.load(Ordering::Relaxed) {
        0 => String::new(),
        n => format!(" ({n} marked `compile_fail` that blaze can't tell fail as marked)"),
    };
    eprintln!(
        "doctests: {} built by blaze, {} not built{unconfirmed}; compiling took \
         {compiling:.1?}{remembered}",
        counted.blaze.load(Ordering::Relaxed),
        counted.refused.load(Ordering::Relaxed),
    );
    let mut ended = ended.into_inner().unwrap_or_else(|it| it.into_inner());
    blaze_ids::radix::sort_by_key(&mut ended, |(at, _)| *at);
    let (mut passed, mut ignored, mut failed) = (0, 0, Vec::new());
    for (at, result) in ended {
        match result {
            Ok(Ended::Passed) => passed += 1,
            Ok(Ended::Ignored) => ignored += 1,
            // libtest lists a failure by the name alone, without how it is tested.
            Ok(Ended::Failed(shown)) => failed.push((tests[at].name.clone(), shown)),
            Err(error) => failed.push((tests[at].name.clone(), format!("blaze: {error:#}"))),
        }
    }
    if !failed.is_empty() {
        writeln!(&out, "\nfailures:\n")?;
        for (name, shown) in &failed {
            writeln!(&out, "---- {name} stdout ----\n{shown}\n")?;
        }
        writeln!(&out, "\nfailures:")?;
        for (name, _) in &failed {
            writeln!(&out, "    {name}")?;
        }
    }
    writeln!(
        &out,
        "\ntest result: {}. {passed} passed; {} failed; {ignored} ignored; 0 measured; {left_out} filtered out; finished in {:.2}s\n",
        if failed.is_empty() { "ok" } else { "FAILED" },
        failed.len(),
        start.elapsed().as_secs_f64(),
    )?;
    Ok(if failed.is_empty() { 0 } else { FAILED })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn found(errors: &[&str], cannot: &[&str]) -> Found {
        Found {
            errors: errors.iter().map(|it| (it.to_string(), "m".into())).collect(),
            cannot: cannot.iter().map(|it| it.to_string()).collect(),
            deprecated: false,
        }
    }

    fn codes(codes: &[&str]) -> Vec<String> {
        codes.iter().map(|it| it.to_string()).collect()
    }

    #[test]
    fn a_compile_fail_example_passes_on_an_error_rustc_gives() {
        assert_eq!(found(&["E0308"], &[]).fails_as_marked(&[]), Ok(()));
        assert_eq!(found(&["E0499"], &[]).fails_as_marked(&codes(&["E0499"])), Ok(()));
        assert_eq!(found(&["unused_variables"], &[]).fails_as_marked(&[]), Ok(()));
    }

    #[test]
    fn every_code_it_is_marked_with_is_found() {
        assert_eq!(
            found(&["E0308"], &[]).fails_as_marked(&codes(&["E0308", "E0599"])),
            Err("it is marked `compile_fail,E0308,E0599`, and blaze finds `E0308` in it but not \
                 `E0599`"
                .to_owned())
        );
        assert_eq!(
            found(&[""], &[]).fails_as_marked(&codes(&["E0599"])),
            Err("it is marked `compile_fail,E0599`, and blaze finds only errors with no code in \
                 it but not `E0599`"
                .to_owned())
        );
    }

    /// What blaze cannot check is no error rustc gives: it passes no `compile_fail` example,
    /// and it is why one with no error fails.
    #[test]
    fn what_blaze_cannot_check_is_no_pass() {
        assert_eq!(
            found(&[], &["blaze cannot borrow-check `f`: a closure"]).fails_as_marked(&[]),
            Err("it is marked `compile_fail`, and blaze finds no error in it: blaze cannot \
                 borrow-check `f`: a closure"
                .to_owned())
        );
        assert_eq!(
            found(&[], &[]).fails_as_marked(&codes(&["E0277"])),
            Err("it is marked `compile_fail,E0277`, and blaze finds no error in it".to_owned())
        );
    }

    /// An example blaze cannot check is refused as one with an error is: it is never built.
    #[test]
    fn what_blaze_cannot_check_refuses_the_example() {
        let cannot = "blaze cannot borrow-check `f`: a closure";
        assert_eq!(found(&[], &[cannot]).first().as_deref(), Some(cannot));
        assert_eq!(found(&["E0308"], &[cannot]).first().as_deref(), Some("`E0308`: m"));
        assert_eq!(found(&[""], &[]).first().as_deref(), Some("m"));
        assert_eq!(found(&[], &[]).first(), None);
    }
}
