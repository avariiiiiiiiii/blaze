//! Each failure blaze hit building a real crate, reduced to a minimal crate under
//! `tests/repro/<property>/` (by `tools/v2/repro/repro.py`) and a test named for the property it
//! broke. A test copies its crate out of the tree, runs this package's `blaze` on it with a
//! `BLAZE_HOME` of its own, and checks what makes the build right: it succeeds and the program
//! prints what the program says it prints.
#![cfg(unix)]

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
        copy_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/repro").join(property), &dir);
        fs::create_dir_all(&home).unwrap();
        Case { root, dir, home }
    }

    fn write(&self, path: &str, text: &str) {
        let path = self.dir.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    fn blaze(&self, args: &[&str]) -> Out {
        let mut child = Command::new(env!("CARGO_BIN_EXE_blaze"))
            .args(args)
            .current_dir(&self.dir)
            .env("BLAZE_HOME", &self.home)
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
                    self.dir.display(),
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
        let out = self.blaze(args);
        assert!(
            out.ok,
            "`blaze {}` in {} failed:\n{}\n(the daemon's log: {})",
            args.join(" "),
            self.dir.display(),
            out.stderr,
            self.home.join("v2/serve.log").display()
        );
        out.stdout
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
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|it| it.success());
        if !stopped {
            self.kill_daemon();
        }
        if !thread::panicking() {
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
