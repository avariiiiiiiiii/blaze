//! A commit's tree, from the VM's clone, as ops on the daemon's VFS.
//!
//! `git` runs with no hooks, no system config, no prompt and no local-file transport. The
//! token goes in through the environment, never argv (world-readable in `/proc`).

use std::{
    fmt,
    io::{BufRead, BufReader, Read, Write},
    path::PathBuf,
    process::{Command, Output, Stdio},
};

use base64::{Engine, engine::general_purpose::STANDARD};
use blaze_vfs::Op;

use crate::github::Token;

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct CheckoutError {
    pub what: String,
    /// The git command, or the path in the tree, it failed at.
    pub at: Option<String>,
}

impl CheckoutError {
    fn new(what: impl Into<String>, at: Option<String>) -> CheckoutError {
        CheckoutError { what: what.into(), at }
    }
}

impl fmt::Display for CheckoutError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.at {
            Some(at) => write!(f, "{} (at {at})", self.what),
            None => f.write_str(&self.what),
        }
    }
}

impl std::error::Error for CheckoutError {}

/// The VM's clone of the repository.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Git {
    pub dir: PathBuf,
    pub remote: String,
}

/// The ref GitHub keeps a PR's head under: `refs/pull/<n>/head`.
pub fn pull_ref(number: u64) -> String {
    format!("refs/pull/{number}/head")
}

impl Git {
    /// Fetches `refs` (commits or refspecs) from the remote, authenticated with `token` when
    /// given.
    pub fn fetch(&self, refs: &[String], token: Option<&Token>) -> Result<(), CheckoutError> {
        if refs.is_empty() {
            return Ok(());
        }
        argument("the remote", &self.remote)?;
        for reference in refs {
            argument("a ref to fetch", reference)?;
        }
        let mut args = vec!["fetch", "--quiet", "--no-tags", "--no-recurse-submodules", "--no-write-fetch-head"];
        args.push(&self.remote);
        args.extend(refs.iter().map(String::as_str));
        let mut command = self.git(&args);
        if let Some(token) = token {
            let basic = STANDARD.encode(format!("x-access-token:{}", token.expose()));
            command
                .env("GIT_CONFIG_COUNT", "2")
                .env("GIT_CONFIG_KEY_0", "http.extraHeader")
                .env("GIT_CONFIG_VALUE_0", format!("Authorization: Basic {basic}"))
                .env("GIT_CONFIG_KEY_1", "http.followRedirects")
                .env("GIT_CONFIG_VALUE_1", "false");
        }
        self.run(command, &args)?;
        Ok(())
    }

    /// The ops that take the tree of commit `from` (`None`: the empty tree) to the tree of
    /// `to`, sorted by path.
    pub fn ops(&self, from: Option<&str>, to: &str) -> Result<Vec<Op>, CheckoutError> {
        argument("a commit", to)?;
        let changes = match from {
            Some(from) => {
                argument("a commit", from)?;
                let from = format!("{from}^{{commit}}");
                let to = format!("{to}^{{commit}}");
                let args =
                    ["diff-tree", "-r", "-z", "--no-renames", "--no-abbrev", from.as_str(), to.as_str()];
                diff(&self.run(self.git(&args), &args)?.stdout)?
            }
            None => {
                let to = format!("{to}^{{commit}}");
                let args = ["ls-tree", "-r", "-z", "--full-tree", to.as_str()];
                listing(&self.run(self.git(&args), &args)?.stdout)?
            }
        };
        let blobs: Vec<&str> = changes.iter().filter_map(|it| it.blob.as_deref()).collect();
        let mut contents = self.blobs(&blobs)?.into_iter();
        let mut ops: Vec<Op> = changes
            .into_iter()
            .map(|change| match change.blob {
                Some(_) => Op::Write {
                    path: change.path,
                    bytes: contents.next().expect("a blob for each written file"),
                },
                None => Op::Delete { path: change.path },
            })
            .collect();
        ops.sort_by(|a, b| path(a).cmp(path(b)));
        Ok(ops)
    }

    /// Each blob's bytes, in order, through one `git cat-file --batch`.
    fn blobs(&self, blobs: &[&str]) -> Result<Vec<Vec<u8>>, CheckoutError> {
        if blobs.is_empty() {
            return Ok(Vec::new());
        }
        let args = ["cat-file", "--batch"];
        let at = || Some(shown(&args));
        let mut child = self
            .git(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| CheckoutError::new(format!("git did not start: {error}"), at()))?;
        let mut stdin = child.stdin.take().expect("a piped stdin");
        let stdout = child.stdout.take().expect("a piped stdout");
        let mut stderr = child.stderr.take().expect("a piped stderr");
        let (read, written, errors) = std::thread::scope(|scope| {
            let writer = scope.spawn(move || {
                for blob in blobs {
                    stdin.write_all(blob.as_bytes())?;
                    stdin.write_all(b"\n")?;
                }
                stdin.flush()
            });
            let errors = scope.spawn(move || {
                let mut text = Vec::new();
                stderr.read_to_end(&mut text).map(|_| text)
            });
            let read = batch(&mut BufReader::new(stdout), blobs);
            let written = writer.join().expect("the cat-file writer does not panic");
            let errors = errors.join().expect("the cat-file stderr reader does not panic");
            (read, written, errors)
        });
        let status =
            child.wait().map_err(|error| CheckoutError::new(format!("waiting for git: {error}"), at()))?;
        let errors = errors.unwrap_or_default();
        let failed = CheckoutError::new(format!("git failed ({status}): {}", last_line(&errors)), at());
        match read {
            // A record misread hangs git up, so its own failure says more only if it printed one.
            Err(misread) if status.success() || errors.trim_ascii().is_empty() => Err(misread),
            _ if !status.success() => Err(failed),
            read => {
                written.map_err(|error| CheckoutError::new(format!("writing to git: {error}"), at()))?;
                read
            }
        }
    }

    fn git(&self, args: &[&str]) -> Command {
        let mut command = Command::new("git");
        command
            .arg("-C")
            .arg(&self.dir)
            .args(["-c", "core.hooksPath=/dev/null", "-c", "protocol.file.allow=never"])
            .args(["-c", "protocol.ext.allow=never", "-c", "core.fsmonitor=false"])
            .args(args)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env_remove("GIT_CONFIG_COUNT")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .env_remove("GIT_OBJECT_DIRECTORY")
            .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
            .env_remove("GIT_NAMESPACE")
            .stdin(Stdio::null());
        command
    }

    fn run(&self, mut command: Command, args: &[&str]) -> Result<Output, CheckoutError> {
        let at = || Some(shown(args));
        let output = command
            .output()
            .map_err(|error| CheckoutError::new(format!("git did not start: {error}"), at()))?;
        if output.status.success() {
            Ok(output)
        } else {
            Err(CheckoutError::new(
                format!("git failed ({}): {}", output.status, last_line(&output.stderr)),
                at(),
            ))
        }
    }
}

/// A file of the tree that changed: its new blob, or `None` when it is gone.
struct Change {
    path: String,
    blob: Option<String>,
}

/// `diff-tree -r -z` raw output: `:<mode> <mode> <sha> <sha> <status>\0<path>\0` each.
fn diff(output: &[u8]) -> Result<Vec<Change>, CheckoutError> {
    let mut fields = output.split(|it| *it == 0).filter(|it| !it.is_empty());
    let mut changes = Vec::new();
    while let Some(header) = fields.next() {
        let header = text(header)?;
        let path = text(fields.next().ok_or_else(|| odd("diff-tree", header))?)?;
        let parts: Vec<&str> = header.strip_prefix(':').unwrap_or_default().split(' ').collect();
        let [old_mode, new_mode, old_blob, new_blob, status] = parts[..] else {
            return Err(odd("diff-tree", header));
        };
        let blob = match status {
            "A" | "M" | "T" => {
                file_mode(new_mode, path)?;
                if old_blob == new_blob && file_mode(old_mode, path).is_ok() {
                    continue;
                }
                Some(new_blob.to_owned())
            }
            "D" => {
                file_mode(old_mode, path)?;
                None
            }
            _ => return Err(odd("diff-tree", header)),
        };
        changes.push(Change { path: path.to_owned(), blob });
    }
    Ok(changes)
}

/// `ls-tree -r -z` output: `<mode> <type> <sha>\t<path>\0` each.
fn listing(output: &[u8]) -> Result<Vec<Change>, CheckoutError> {
    let mut changes = Vec::new();
    for entry in output.split(|it| *it == 0).filter(|it| !it.is_empty()) {
        let entry = text(entry)?;
        let (header, path) = entry.split_once('\t').ok_or_else(|| odd("ls-tree", entry))?;
        let [mode, _, blob] = header.split(' ').collect::<Vec<_>>()[..] else {
            return Err(odd("ls-tree", entry));
        };
        file_mode(mode, path)?;
        changes.push(Change { path: path.to_owned(), blob: Some(blob.to_owned()) });
    }
    Ok(changes)
}

/// A regular file; an executable is written as one (the VFS has no mode). The VFS has no
/// symlink or submodule (v2/requests/ci.md item 5).
fn file_mode(mode: &str, path: &str) -> Result<(), CheckoutError> {
    let refuse = |what: &str| -> Result<(), CheckoutError> {
        Err(CheckoutError::new(what, Some(path.to_owned())))
    };
    match mode {
        "100644" | "100755" | "100664" => Ok(()),
        "120000" => refuse("the tree has a symlink, and the VFS has none"),
        "160000" => refuse("the tree has a submodule, and the VFS has none"),
        _ => refuse(&format!("the tree has an entry of mode {mode}, which is no file")),
    }
}

/// `cat-file --batch` output: `<sha> blob <size>\n<bytes>\n` each, in the order asked.
fn batch(from: &mut impl BufRead, blobs: &[&str]) -> Result<Vec<Vec<u8>>, CheckoutError> {
    let at = || Some("git cat-file --batch".to_owned());
    let mut contents = Vec::with_capacity(blobs.len());
    let mut line = String::new();
    for blob in blobs {
        line.clear();
        from.read_line(&mut line)
            .map_err(|error| CheckoutError::new(format!("reading from git: {error}"), at()))?;
        let header = line.trim_end_matches('\n');
        let size = match header.split(' ').collect::<Vec<_>>()[..] {
            [sha, "blob", size] if sha == *blob => size.parse::<usize>().ok(),
            [sha, "missing"] if sha == *blob => {
                return Err(CheckoutError::new(format!("the clone has no blob {blob}"), at()));
            }
            _ => None,
        };
        let size = size.ok_or_else(|| {
            CheckoutError::new(format!("git cat-file answered {header:?} for blob {blob}"), at())
        })?;
        let mut bytes = vec![0; size + 1];
        from.read_exact(&mut bytes)
            .map_err(|error| CheckoutError::new(format!("reading blob {blob} from git: {error}"), at()))?;
        if bytes.pop() != Some(b'\n') {
            return Err(CheckoutError::new(format!("git cat-file's blob {blob} did not end its record"), at()));
        }
        contents.push(bytes);
    }
    Ok(contents)
}

fn path(op: &Op) -> &str {
    match op {
        Op::Write { path, .. } | Op::Edit { path, .. } | Op::Delete { path } => path,
        Op::Rename { from, .. } => from,
    }
}

fn text(bytes: &[u8]) -> Result<&str, CheckoutError> {
    std::str::from_utf8(bytes).map_err(|_| {
        CheckoutError::new(
            "the tree has a path that is not UTF-8, which the VFS can't name",
            Some(String::from_utf8_lossy(bytes).into_owned()),
        )
    })
}

fn odd(command: &str, output: &str) -> CheckoutError {
    CheckoutError::new(format!("git {command} printed {output:?}, which is not its format"), None)
}

/// An argument git must not take as an option.
fn argument(what: &str, value: &str) -> Result<(), CheckoutError> {
    if value.is_empty() || value.starts_with('-') || value.bytes().any(|it| it.is_ascii_control()) {
        Err(CheckoutError::new(format!("{what} {value:?} is not one git can be given"), None))
    } else {
        Ok(())
    }
}

/// The command as an error shows it, any user and password in a URL hidden.
fn shown(args: &[&str]) -> String {
    let mut shown = "git".to_owned();
    for arg in args {
        shown.push(' ');
        match arg.split_once("://") {
            Some((scheme, rest)) if rest.split('/').next().is_some_and(|host| host.contains('@')) => {
                let after = &rest[rest.find('@').expect("an @ in the host part") + 1..];
                shown.push_str(&format!("{scheme}://***@{after}"));
            }
            _ => shown.push_str(arg),
        }
    }
    shown
}

fn last_line(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    text.lines().map(str::trim).rfind(|line| !line.is_empty()).unwrap_or("no message").to_owned()
}

#[cfg(all(test, unix))]
mod tests;
