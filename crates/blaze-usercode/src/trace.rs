//! What a sandbox saw a program read, from the line it notes as each read happens to the
//! [`Traced`] its result is keyed by (docs/v2/usercode-notes.md, "The record").
//!
//! A sandbox appends one [`Line`] per thing it saw, as it sees it, to the run's live record:
//! a warm process (a proc-macro server, a fork server) serves many requests, each bracketed by
//! the sandbox's marker, and its record is read after each answer, so a line is written before
//! the call it notes goes on. [`assemble`] turns the lines of one scope into a [`Traced`],
//! hashing what was read as it is when the run ends; [`encode`] and [`decode`] are the report
//! the sandbox's launcher leaves for the driver that ran it.

use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::{OsStr, OsString},
    fmt,
    path::{Path, PathBuf},
};

use blaze_core::{Traced, Untraceable};
use blaze_ids::Hasher;

/// What a line says was seen.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum Seen {
    /// `R`: a path opened to read, or run.
    Read,
    /// `L`: a path only looked at (`stat`, `access`, `readlink`): what it is, but not what is
    /// in it. Traced as a read: a superset of what it showed.
    Looked,
    /// `D`: a directory listed.
    Listed,
    /// `E`: an environment variable read by name, or `*` for the whole environment (a
    /// process that walked `environ`, or one whose reads by name can't be seen).
    Env,
    /// `T`: something that can't be traced: `clock`, `random`, `network` or `descriptor`.
    Untraceable,
    /// `W`: a write the sandbox refused. Not an input: the refusal is the same every run.
    RefusedWrite,
    /// `N`: an address the sandbox refused. Not an input, as a refused write is not.
    RefusedNet,
    /// `X`: the record is not whole, and why: what was read is not all known.
    Incomplete,
}

impl Seen {
    pub fn letter(self) -> char {
        match self {
            Seen::Read => 'R',
            Seen::Looked => 'L',
            Seen::Listed => 'D',
            Seen::Env => 'E',
            Seen::Untraceable => 'T',
            Seen::RefusedWrite => 'W',
            Seen::RefusedNet => 'N',
            Seen::Incomplete => 'X',
        }
    }

    pub fn of(letter: &str) -> Option<Seen> {
        Some(match letter {
            "R" => Seen::Read,
            "L" => Seen::Looked,
            "D" => Seen::Listed,
            "E" => Seen::Env,
            "T" => Seen::Untraceable,
            "W" => Seen::RefusedWrite,
            "N" => Seen::RefusedNet,
            "X" => Seen::Incomplete,
            _ => return None,
        })
    }
}

/// The name of an [`Untraceable`] in a `T` line.
pub fn untraceable_name(it: Untraceable) -> &'static str {
    match it {
        Untraceable::Clock => "clock",
        Untraceable::Random => "random",
        Untraceable::Network => "network",
        Untraceable::Descriptor => "descriptor",
    }
}

pub fn untraceable_of(name: &str) -> Option<Untraceable> {
    Some(match name {
        "clock" => Untraceable::Clock,
        "random" => Untraceable::Random,
        "network" => Untraceable::Network,
        "descriptor" => Untraceable::Descriptor,
        _ => return None,
    })
}

/// What an `E` line names for the whole environment.
pub const ALL_ENV: &str = "*";

/// The name a [`Traced`] of a run that read the whole environment has, beside every variable,
/// with the [`listing`] hash of the names (in hex) as its value: a variable added later is
/// one it would have seen. No variable has this name: a name is a C string.
pub const ALL_ENV_NAME: &str = "\0*";

/// One line of a record: the letter, the scope and what, by tabs, ended by a line break. The
/// scope is the name the sandbox's marker gave the thread or process that made the call (a
/// test, an expansion), or empty when none did: a line with an empty scope is every scope's.
/// What has a tab or a line break in it can't be written; the sandbox writes an `X` line for
/// it instead ([`Line::new`]).
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct Line {
    pub seen: Seen,
    pub scope: String,
    pub what: String,
}

impl Line {
    /// The line, or an `X` line when `scope` or `what` can't be written in one.
    pub fn new(seen: Seen, scope: &str, what: &str) -> Line {
        let bad = |it: &str| it.contains(['\t', '\n']);
        match (bad(scope), bad(what)) {
            (false, false) => Line { seen, scope: scope.to_owned(), what: what.to_owned() },
            (true, _) => Line {
                seen: Seen::Incomplete,
                scope: String::new(),
                what: "a scope with a tab or a line break in it".to_owned(),
            },
            (false, true) => Line {
                seen: Seen::Incomplete,
                scope: scope.to_owned(),
                what: "a path or a name with a tab or a line break in it".to_owned(),
            },
        }
    }

    /// The line as the record has it, with its line break.
    pub fn text(&self) -> String {
        format!("{}\t{}\t{}\n", self.seen.letter(), self.scope, self.what)
    }

    /// The line `text` is, without its line break.
    pub fn parse(text: &str) -> Option<Line> {
        let mut parts = text.splitn(3, '\t');
        let seen = Seen::of(parts.next()?)?;
        let scope = parts.next()?.to_owned();
        let what = parts.next()?.to_owned();
        Some(Line { seen, scope, what })
    }
}

/// The lines of a record's text. A last line with no line break is one the sandbox was
/// writing as it was read, and is left for the next read; any other line that is not a line
/// is an error: the record is not whole.
pub fn parse(text: &str) -> Result<(Vec<Line>, usize), BadRecord> {
    let mut lines = Vec::new();
    let mut used = 0;
    for (number, piece) in text.split_inclusive('\n').enumerate() {
        let Some(line) = piece.strip_suffix('\n') else { break };
        lines.push(Line::parse(line).ok_or_else(|| BadRecord {
            line: number + 1,
            text: line.chars().take(200).collect(),
        })?);
        used += piece.len();
    }
    Ok((lines, used))
}

/// A record with a line that is not one.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct BadRecord {
    pub line: usize,
    pub text: String,
}

impl fmt::Display for BadRecord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "line {} of the sandbox's record is not a record's line: {:?}", self.line, self.text)
    }
}

/// What a read is checked against: the files, directories and environment as they are where
/// a result is to be used (a view and the machine, [`crate::inputs`]) or as they were when a
/// run ended (the machine, [`crate::inputs::Disk`]).
pub trait Inputs {
    type Error;
    /// The BLAKE3 hash of the file's bytes, [`DIRECTORY`] for a directory, `None` for nothing
    /// there. A file that is there and can't be read is an error, never `None` (#1480).
    fn file(&mut self, path: &Path) -> Result<Option<[u8; 32]>, Self::Error>;
    /// The [`listing`] hash of the directory's names, `None` for no directory there.
    fn dir(&mut self, path: &Path) -> Result<Option<[u8; 32]>, Self::Error>;
    fn env(&mut self, name: &OsStr) -> Option<OsString>;
    /// The whole environment, for a run that read all of it.
    fn env_all(&mut self) -> Vec<(OsString, OsString)>;
}

/// What a traced file that is a directory hashes as: what a look at it showed is that it is
/// one, and a listing of it is a [`Seen::Listed`] of its own.
pub const DIRECTORY: [u8; 32] = *b"blaze v2 traced: a directory\0\0\0\0";

/// The hash of a directory's listing: its names, sorted by their bytes, each framed.
pub fn listing<'a>(names: impl IntoIterator<Item = &'a OsStr>) -> [u8; 32] {
    let mut names: Vec<&[u8]> = names.into_iter().map(os_bytes).collect();
    names.sort_unstable();
    names.dedup();
    let mut hasher = Hasher::new("blaze v2 traced listing 1");
    for name in names {
        hasher.part(name);
    }
    hasher.finish().0
}

/// The lines of `scope` (all of them for `None`), with what they read as `inputs` has it now:
/// sorted and each once, so equal reads give equal [`Traced`]s whatever order they came in.
pub fn assemble<I: Inputs>(
    lines: &[Line],
    scope: Option<&str>,
    inputs: &mut I,
) -> Result<Traced, I::Error> {
    let mut files = BTreeSet::new();
    let mut dirs = BTreeSet::new();
    let mut env = BTreeSet::new();
    let mut all_env = false;
    let mut untraceable = BTreeSet::new();
    let ours = |line: &Line| match scope {
        None => true,
        Some(scope) => line.scope.is_empty() || line.scope == scope,
    };
    for line in lines.iter().filter(|it| ours(it)) {
        match line.seen {
            Seen::Read | Seen::Looked => _ = files.insert(PathBuf::from(&line.what)),
            Seen::Listed => _ = dirs.insert(PathBuf::from(&line.what)),
            Seen::Env if line.what == ALL_ENV => all_env = true,
            Seen::Env => _ = env.insert(OsString::from(&line.what)),
            Seen::Untraceable => {
                // A name this blaze doesn't know is something it can't trace either.
                untraceable.insert(untraceable_of(&line.what).unwrap_or(Untraceable::Descriptor));
            }
            Seen::Incomplete => _ = untraceable.insert(Untraceable::Descriptor),
            Seen::RefusedWrite | Seen::RefusedNet => {}
        }
    }
    let mut traced = Traced::default();
    for path in files {
        let hash = inputs.file(&path)?;
        traced.files.push((path, hash));
    }
    for path in dirs {
        match inputs.dir(&path)? {
            Some(hash) => traced.dirs.push((path, hash)),
            // A listing of nothing is a look that found nothing.
            None if !traced.files.iter().any(|it| it.0 == path) => {
                traced.files.push((path, None));
            }
            None => {}
        }
    }
    traced.files.sort();
    let mut values: BTreeMap<OsString, Option<OsString>> = BTreeMap::new();
    if all_env {
        let all = inputs.env_all();
        values.insert(ALL_ENV_NAME.into(), Some(names_hash(&all).into()));
        for (name, value) in all {
            values.insert(name, Some(value));
        }
    }
    for name in env {
        let value = inputs.env(&name);
        values.entry(name).or_insert(value);
    }
    traced.env = values.into_iter().collect();
    traced.untraceable = untraceable.into_iter().collect();
    Ok(traced)
}

/// The value [`ALL_ENV_NAME`] has for the environment `all`.
pub fn names_hash(all: &[(OsString, OsString)]) -> String {
    let hash = listing(all.iter().map(|it| it.0.as_os_str()));
    hash.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// `traced` sorted and each entry once: what [`assemble`] makes, for a [`Traced`] made
/// otherwise (a wasm macro's host, which sees every call itself).
pub fn normalize(traced: &mut Traced) {
    traced.files.sort();
    traced.files.dedup();
    traced.dirs.sort();
    traced.dirs.dedup();
    traced.env.sort();
    traced.env.dedup();
    traced.untraceable.sort();
    traced.untraceable.dedup();
}

/// The bytes of `name` as the program had them.
pub fn os_bytes(name: &OsStr) -> &[u8] {
    #[cfg(unix)]
    {
        std::os::unix::ffi::OsStrExt::as_bytes(name)
    }
    #[cfg(not(unix))]
    {
        name.as_encoded_bytes()
    }
}

pub(crate) fn os_from(bytes: &[u8]) -> OsString {
    #[cfg(unix)]
    {
        std::os::unix::ffi::OsStrExt::from_bytes(bytes).to_owned()
    }
    #[cfg(not(unix))]
    {
        // SAFETY: the bytes are what `os_bytes` gave of an `OsStr` of this platform, as
        // `encode` wrote them.
        unsafe { OsStr::from_encoded_bytes_unchecked(bytes) }.to_owned()
    }
}

/// What a sandbox's launcher leaves for the driver that ran it ([`encode`]).
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Report {
    /// The sandbox was made and the program ran; what it read when the sandbox traces.
    Ran(Option<Traced>),
    /// The sandbox could not be made, and why: the spawn failed, nothing ran outside it.
    NotMade(String),
}

const MAGIC: &[u8; 16] = b"blaze v2 report1";

/// `report` as bytes: the magic, then each part length-prefixed, little-endian. Equal reports
/// are equal bytes.
pub fn encode(report: &Report) -> Vec<u8> {
    let mut out = MAGIC.to_vec();
    let part = |out: &mut Vec<u8>, bytes: &[u8]| {
        out.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
        out.extend_from_slice(bytes);
    };
    match report {
        Report::NotMade(why) => {
            out.push(0);
            part(&mut out, why.as_bytes());
        }
        Report::Ran(None) => out.push(1),
        Report::Ran(Some(traced)) => {
            out.push(2);
            out.extend_from_slice(&(traced.files.len() as u64).to_le_bytes());
            for (path, hash) in &traced.files {
                part(&mut out, os_bytes(path.as_os_str()));
                match hash {
                    Some(hash) => {
                        out.push(1);
                        out.extend_from_slice(hash);
                    }
                    None => out.push(0),
                }
            }
            out.extend_from_slice(&(traced.dirs.len() as u64).to_le_bytes());
            for (path, hash) in &traced.dirs {
                part(&mut out, os_bytes(path.as_os_str()));
                out.extend_from_slice(hash);
            }
            out.extend_from_slice(&(traced.env.len() as u64).to_le_bytes());
            for (name, value) in &traced.env {
                part(&mut out, os_bytes(name));
                match value {
                    Some(value) => {
                        out.push(1);
                        part(&mut out, os_bytes(value));
                    }
                    None => out.push(0),
                }
            }
            out.extend_from_slice(&(traced.untraceable.len() as u64).to_le_bytes());
            for it in &traced.untraceable {
                part(&mut out, untraceable_name(*it).as_bytes());
            }
        }
    }
    out
}

/// Reads what [`encode`] and [`crate::index::Shape::encode`] write.
pub(crate) struct At<'a>(pub(crate) &'a [u8]);

impl<'a> At<'a> {
    pub(crate) fn take(&mut self, len: usize) -> Result<&'a [u8], String> {
        if self.0.len() < len {
            return Err("it ends early".to_owned());
        }
        let (head, rest) = self.0.split_at(len);
        self.0 = rest;
        Ok(head)
    }

    pub(crate) fn byte(&mut self) -> Result<u8, String> {
        Ok(self.take(1)?[0])
    }

    pub(crate) fn len(&mut self) -> Result<usize, String> {
        let len = u64::from_le_bytes(self.take(8)?.try_into().unwrap());
        // Each entry takes at least a byte: a count past what is left is not one written.
        usize::try_from(len)
            .ok()
            .filter(|it| *it <= self.0.len())
            .ok_or_else(|| "it counts more than it holds".to_owned())
    }

    pub(crate) fn part(&mut self) -> Result<&'a [u8], String> {
        let len = self.len()?;
        self.take(len)
    }

    fn hash(&mut self) -> Result<[u8; 32], String> {
        Ok(self.take(32)?.try_into().unwrap())
    }
}

/// The report `bytes` are, or why they are not one.
pub fn decode(bytes: &[u8]) -> Result<Report, String> {
    let mut at = At(bytes);
    if at.take(MAGIC.len())? != MAGIC {
        return Err("not a sandbox's report".to_owned());
    }
    let report = match at.byte()? {
        0 => Report::NotMade(String::from_utf8_lossy(at.part()?).into_owned()),
        1 => Report::Ran(None),
        2 => {
            let mut traced = Traced::default();
            for _ in 0..at.len()? {
                let path = PathBuf::from(os_from(at.part()?));
                let hash = match at.byte()? {
                    0 => None,
                    1 => Some(at.hash()?),
                    _ => return Err("a file's hash is neither there nor not".to_owned()),
                };
                traced.files.push((path, hash));
            }
            for _ in 0..at.len()? {
                let path = PathBuf::from(os_from(at.part()?));
                traced.dirs.push((path, at.hash()?));
            }
            for _ in 0..at.len()? {
                let name = os_from(at.part()?);
                let value = match at.byte()? {
                    0 => None,
                    1 => Some(os_from(at.part()?)),
                    _ => return Err("a variable's value is neither there nor not".to_owned()),
                };
                traced.env.push((name, value));
            }
            for _ in 0..at.len()? {
                let name = String::from_utf8_lossy(at.part()?).into_owned();
                traced.untraceable.push(
                    untraceable_of(&name).ok_or_else(|| format!("no untraceable is {name:?}"))?,
                );
            }
            Report::Ran(Some(traced))
        }
        other => return Err(format!("no report is of kind {other}")),
    };
    match at.0.is_empty() {
        true => Ok(report),
        false => Err("the report has bytes after its end".to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixed {
        files: BTreeMap<PathBuf, [u8; 32]>,
        dirs: BTreeMap<PathBuf, Vec<&'static str>>,
        env: Vec<(OsString, OsString)>,
    }

    impl Inputs for Fixed {
        type Error = String;
        fn file(&mut self, path: &Path) -> Result<Option<[u8; 32]>, String> {
            if self.dirs.contains_key(path) {
                return Ok(Some(DIRECTORY));
            }
            Ok(self.files.get(path).copied())
        }
        fn dir(&mut self, path: &Path) -> Result<Option<[u8; 32]>, String> {
            Ok(self.dirs.get(path).map(|names| listing(names.iter().map(OsStr::new))))
        }
        fn env(&mut self, name: &OsStr) -> Option<OsString> {
            self.env.iter().find(|it| it.0 == name).map(|it| it.1.clone())
        }
        fn env_all(&mut self) -> Vec<(OsString, OsString)> {
            self.env.clone()
        }
    }

    fn fixed() -> Fixed {
        Fixed {
            files: [("/ws/a.txt".into(), [1; 32]), ("/usr/bin/cc".into(), [2; 32])].into(),
            dirs: [("/ws/data".into(), vec!["b", "a"])].into(),
            env: vec![("PATH".into(), "/usr/bin".into()), ("CC".into(), "cc".into())],
        }
    }

    #[test]
    fn a_line_round_trips_and_a_tab_makes_it_incomplete() {
        let line = Line::new(Seen::Env, "tests::a", "CC");
        assert_eq!(line.text(), "E\ttests::a\tCC\n");
        assert_eq!(Line::parse(line.text().trim_end_matches('\n')), Some(line));
        assert_eq!(Line::new(Seen::Read, "", "/a\tb").seen, Seen::Incomplete);
        assert_eq!(Line::new(Seen::Read, "x\ny", "/a").seen, Seen::Incomplete);
    }

    #[test]
    fn a_record_being_written_leaves_its_last_line() {
        let (lines, used) = parse("R\t\t/a\nE\t\tP").unwrap();
        assert_eq!(lines.len(), 1);
        assert_eq!(used, "R\t\t/a\n".len());
        assert!(parse("Q\t\t/a\n").is_err());
    }

    #[test]
    fn assembling_is_sorted_once_and_by_scope() {
        let lines = [
            Line::new(Seen::Read, "", "/usr/bin/cc"),
            Line::new(Seen::Read, "e1", "/ws/a.txt"),
            Line::new(Seen::Looked, "e1", "/ws/a.txt"),
            Line::new(Seen::Read, "e2", "/ws/missing"),
            Line::new(Seen::Listed, "e1", "/ws/data"),
            Line::new(Seen::Listed, "e1", "/ws/nodir"),
            Line::new(Seen::Env, "e1", "CC"),
            Line::new(Seen::Env, "e1", "UNSET"),
            Line::new(Seen::RefusedWrite, "e1", "/etc/x"),
        ];
        let traced = assemble(&lines, Some("e1"), &mut fixed()).unwrap();
        assert_eq!(
            traced.files,
            vec![
                ("/usr/bin/cc".into(), Some([2; 32])),
                ("/ws/a.txt".into(), Some([1; 32])),
                ("/ws/nodir".into(), None),
            ]
        );
        assert_eq!(traced.dirs, vec![("/ws/data".into(), listing(["a", "b"].map(OsStr::new)))]);
        assert_eq!(traced.env, vec![("CC".into(), Some("cc".into())), ("UNSET".into(), None)]);
        assert!(traced.untraceable.is_empty());
        let other = assemble(&lines, Some("e2"), &mut fixed()).unwrap();
        assert_eq!(other.files, vec![("/usr/bin/cc".into(), Some([2; 32])), ("/ws/missing".into(), None)]);
    }

    #[test]
    fn the_whole_environment_and_what_cannot_be_traced() {
        let lines = [
            Line::new(Seen::Env, "", ALL_ENV),
            Line::new(Seen::Untraceable, "", "clock"),
            Line::new(Seen::Untraceable, "", "sundial"),
            Line::new(Seen::Incomplete, "", "a program that did not load the library"),
        ];
        let mut inputs = fixed();
        let traced = assemble(&lines, None, &mut inputs).unwrap();
        assert_eq!(traced.env.len(), 3);
        assert_eq!(traced.env[0].0, ALL_ENV_NAME);
        assert_eq!(traced.env[0].1.as_deref(), Some(OsStr::new(&names_hash(&inputs.env))));
        assert_eq!(traced.untraceable, vec![Untraceable::Clock, Untraceable::Descriptor]);
    }

    #[test]
    fn listings_ignore_order() {
        assert_eq!(listing(["b", "a"].map(OsStr::new)), listing(["a", "b"].map(OsStr::new)));
        assert_ne!(listing(["ab"].map(OsStr::new)), listing(["a", "b"].map(OsStr::new)));
    }

    #[test]
    fn reports_round_trip() {
        let lines = [
            Line::new(Seen::Read, "", "/ws/a.txt"),
            Line::new(Seen::Listed, "", "/ws/data"),
            Line::new(Seen::Env, "", "CC"),
            Line::new(Seen::Env, "", "UNSET"),
            Line::new(Seen::Untraceable, "", "random"),
        ];
        let traced = assemble(&lines, None, &mut fixed()).unwrap();
        for report in [
            Report::Ran(Some(traced)),
            Report::Ran(None),
            Report::NotMade("no user namespaces".into()),
        ] {
            let bytes = encode(&report);
            assert_eq!(decode(&bytes), Ok(report));
            assert!(decode(&bytes[..bytes.len() - 1]).is_err());
        }
        assert!(decode(b"blaze v2 report1\x02\xff\xff\xff\xff\xff\xff\xff\xff").is_err());
    }
}
