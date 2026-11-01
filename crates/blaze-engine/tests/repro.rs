//! Each failure blaze hit building a real crate, reduced to a minimal crate under
//! `tests/repro/<property>/` (by `tools/v2/repro/repro.py`) and a test named for the property it
//! broke. A test copies its crate out of the tree, runs this package's `blaze` on it with a
//! `BLAZE_HOME` of its own, and checks what makes the build right: it succeeds and the program
//! prints what the program says it prints.
//!
//! Every case builds std, which is compiled at opt-level 3, so only a blaze with LLVM can build
//! one: without `--features llvm` this target is empty, and a run must pass the feature.
#![cfg(all(unix, feature = "llvm"))]

use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::atomic::{AtomicU32, Ordering},
    thread,
    time::{Duration, Instant},
};

/// Longer than any of these builds takes with the standard library built cold; a build still
/// running then is a hang, and fails the test instead of holding the run.
const DEADLINE: Duration = Duration::from_secs(900);

struct Case {
    root: PathBuf,
    dir: PathBuf,
    home: PathBuf,
    /// `XDG_CACHE_HOME` of every blaze it runs, under `root`: the machine's shared store
    /// (`<cache>/blaze`) is the case's own, never the real one.
    cache: PathBuf,
    /// Whether dropping it removes `root`: a [`Case::second_home`] shares its case's.
    owns_root: bool,
}

struct Out {
    ok: bool,
    stdout: String,
    stderr: String,
}

impl Case {
    fn new(property: &str) -> Case {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        // The daemon's socket is `<home>/v2.sock`, and a Unix socket's path is at most 104 bytes
        // on macOS: a `TMPDIR` such as nix-shell's is already most of that.
        let root = Path::new("/tmp").join(format!(
            "blaze-repro-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&root);
        let dir = root.join(property);
        let home = root.join("home");
        let cache = root.join("cache");
        copy_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/repro").join(property), &dir);
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&cache).unwrap();
        Case { root, dir, home, cache, owns_root: true }
    }

    /// The same crate and machine cache under a `BLAZE_HOME` of its own: a second daemon on the
    /// machine.
    fn second_home(&self) -> Case {
        let home = self.root.join("home2");
        fs::create_dir_all(&home).unwrap();
        // clone: the case's paths, shared by the second home
        Case { root: self.root.clone(), dir: self.dir.clone(), home, cache: self.cache.clone(), owns_root: false }
    }

    fn write(&self, path: &str, text: &str) {
        let path = self.dir.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    /// `blaze args` in `sub`, a directory of the case's crate (`""` for the crate itself).
    fn blaze_in(&self, sub: &str, args: &[&str]) -> Out {
        let dir = self.dir.join(sub);
        let mut child = Command::new(env!("CARGO_BIN_EXE_blaze"))
            .args(args)
            .current_dir(&dir)
            .env("BLAZE_HOME", &self.home)
            .env("XDG_CACHE_HOME", &self.cache)
            .env_remove("CARGO_TARGET_DIR")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let stdout = drain(child.stdout.take().unwrap());
        let stderr = drain(child.stderr.take().unwrap());
        let began = Instant::now();
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if began.elapsed() > DEADLINE {
                child.kill().unwrap();
                child.wait().unwrap();
                self.kill_daemon();
                panic!(
                    "`blaze {}` in {} ran {} s without finishing: a hang\n{}",
                    args.join(" "),
                    dir.display(),
                    DEADLINE.as_secs(),
                    stderr.join().unwrap()
                );
            }
            thread::sleep(Duration::from_millis(50));
        };
        Out { ok: status.success(), stdout: stdout.join().unwrap(), stderr: stderr.join().unwrap() }
    }

    /// `blaze args`, which must succeed; its stdout.
    fn ok(&self, args: &[&str]) -> String {
        self.ok_in("", args).stdout
    }

    /// `blaze args` in `sub`, which must succeed.
    fn ok_in(&self, sub: &str, args: &[&str]) -> Out {
        let out = self.blaze_in(sub, args);
        assert!(
            out.ok,
            "`blaze {}` in {} failed:\n{}\n(the daemon's log: {})",
            args.join(" "),
            self.dir.join(sub).display(),
            out.stderr,
            self.home.join("v2/serve.log").display()
        );
        out
    }

    fn stop(&self) {
        self.ok(&["stop"]);
    }

    /// The daemon `home`'s log last says is listening, killed: one a test left hung or broken.
    fn kill_daemon(&self) {
        let Ok(log) = fs::read_to_string(self.home.join("v2/serve.log")) else { return };
        let Some(pid) = log.rsplit("(pid ").next().and_then(|it| it.split(')').next()) else { return };
        if pid.bytes().all(|b| b.is_ascii_digit()) && !pid.is_empty() {
            let _ = Command::new("kill").args(["-KILL", pid]).status();
        }
    }
}

impl Drop for Case {
    fn drop(&mut self) {
        let stopped = Command::new(env!("CARGO_BIN_EXE_blaze"))
            .arg("stop")
            .current_dir(&self.dir)
            .env("BLAZE_HOME", &self.home)
            .env("XDG_CACHE_HOME", &self.cache)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|it| it.success());
        if !stopped {
            self.kill_daemon();
        }
        if self.owns_root && !thread::panicking() {
            let _ = fs::remove_dir_all(&self.root);
        }
    }
}

fn drain(mut pipe: impl Read + Send + 'static) -> thread::JoinHandle<String> {
    thread::spawn(move || {
        let mut bytes = Vec::new();
        pipe.read_to_end(&mut bytes).unwrap();
        String::from_utf8_lossy(&bytes).into_owned()
    })
}

fn copy_dir(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let to = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &to);
        } else {
            fs::copy(entry.path(), to).unwrap();
        }
    }
}

const AFTER: &str = "fn main() {\n    println!(\"after\");\n}\n";

/// The second build of a crate whose `main.rs` changed, on the daemon that built the first, made
/// its plan panic at `blaze-query/src/segment.rs:367` (`at + bytes.len() <= segment.cap`).
#[test]
fn an_edited_crate_rebuilds_on_one_daemon() {
    let case = Case::new("an_edited_crate_rebuilds_on_one_daemon");
    assert_eq!(case.ok(&["run"]), "before\n");
    case.write("src/main.rs", AFTER);
    assert_eq!(case.ok(&["run"]), "after\n");
}

/// The same crate after `blaze stop`, on the daemon started from the image the stop wrote.
#[test]
fn a_crate_rebuilds_after_its_daemon_restarts() {
    let case = Case::new("a_crate_rebuilds_after_its_daemon_restarts");
    assert_eq!(case.ok(&["run"]), "before\n");
    case.stop();
    case.write("src/main.rs", AFTER);
    assert_eq!(case.ok(&["run"]), "after\n");
}

/// The standard library is built once per daemon, not per workspace: a second workspace on the
/// daemon that built the first compiles its own package and nothing else (it has no
/// dependencies), so no `core`, `alloc`, `std`, `compiler_builtins`, `panic_*` or std's vendored
/// crates. It used to build std again for each workspace, a cold std's minutes each time.
#[test]
fn a_second_workspace_on_one_daemon_compiles_no_std_crate() {
    let case = Case::new("a_second_workspace_on_one_daemon_compiles_no_std_crate");
    assert_eq!(case.ok_in("one", &["run"]).stdout, "one\n");
    let second = case.ok_in("two", &["run"]);
    assert_eq!(second.stdout, "two\n");
    compiled_only(&second, "repro-two", "the second workspace");
    holds_no_std(&case, "one");
    holds_no_std(&case, "two");
}

/// Std is built once per machine: a second `BLAZE_HOME`'s daemon takes it from the machine's
/// shared store (`<XDG_CACHE_HOME>/blaze`) the first daemon put it in, and compiles only its
/// workspace's package.
#[test]
fn a_second_home_on_one_machine_compiles_no_std_crate() {
    let case = Case::new("a_second_home_on_one_machine_compiles_no_std_crate");
    assert_eq!(case.ok_in("one", &["run"]).stdout, "one\n");
    let other = case.second_home();
    let second = other.ok_in("two", &["run"]);
    assert_eq!(second.stdout, "two\n");
    compiled_only(&second, "repro-two", "the second home's daemon");
    holds_no_std(&case, "one");
    holds_no_std(&case, "two");
}

/// `out`'s `Compiling` lines are `package`'s alone: no std crate, vendored ones included.
fn compiled_only(out: &Out, package: &str, who: &str) {
    let compiled: Vec<&str> =
        out.stderr.lines().filter_map(|line| line.trim_start().strip_prefix("Compiling ")).collect();
    assert!(
        compiled.len() == 1 && compiled[0].starts_with(&format!("{package} ")),
        "{who} compiled more than {package}:\n{}",
        compiled.join("\n")
    );
}

/// The workspace `sub`'s target directory holds none of std's outputs, which are the daemon's.
fn holds_no_std(case: &Case, sub: &str) {
    let deps = case.dir.join(sub).join("target/debug/deps");
    let core: Vec<String> = fs::read_dir(&deps)
        .into_iter()
        .flatten()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with("libcore-"))
        .collect();
    assert!(core.is_empty(), "std's outputs are the daemon's, yet {} holds {core:?}", deps.display());
}

/// A registry crate's build script (zmij 1.0.21's) was run before its link wrote it: `could not
/// run .../build/zmij/<hash>/out/build_script_build: No such file or directory`, the link of
/// that path reported after. A package's own script and a path dependency's ran.
#[test]
fn a_registry_dependencys_build_script_runs() {
    let case = Case::new("a_registry_dependencys_build_script_runs");
    assert_eq!(case.ok(&["run"]), "1.5\n");
}

/// The crates.io copy of a crate std vendors (object 0.39.1), as a dependency: once a
/// `CrateKeyClash` of the two build scripts, now the registry script run of
/// [`a_registry_dependencys_build_script_runs`].
#[test]
fn a_dependency_std_also_uses_builds() {
    let case = Case::new("a_dependency_std_also_uses_builds");
    assert_eq!(case.ok(&["run"]), "Little\n");
}

/// A bin over a path dependency with a `thread_local!` whose value has a `Drop` (blaze's own
/// `blaze-reclaim`): its release imports are decided from the build's summaries, and one each
/// of 16 functions imports is called by none of the bodies it builds in ("imports <id>, which
/// nothing it builds calls"), the self-build's wall.
#[test]
fn a_bin_using_a_path_dependencys_thread_local_builds() {
    let case = Case::new("a_bin_using_a_path_dependencys_thread_local_builds");
    assert_eq!(case.ok(&["run"]), "true\n");
}
