//! Which tests run (#227, #1330): a test runs only when it is new, reaches code it did not
//! reach when it last passed, read something at run time that is not as it was, has no pass
//! kept, or reaches the outside world (codegen marks those: [`Test::outside`]).
//!
//! A pass is kept in the store by content, never by binary, view or worktree: under a
//! [`RunKey`] whose code is the test's [`Test::identity`] (its name, the code hashes it
//! reaches, its attributes) and whose given is its [`Suite`], then by what it read at run time
//! as [`crate::index`] keeps a build script's result (the shapes of its reads, then their
//! values now). So two views at the same code with the same files skip the same tests.
//!
//! Beside the passes, each test has a history in the store: a chain of entries, one per
//! recorded run, each with whether it passed, the attributes and reached set it ran with, how
//! long it took and, for a kept pass, what it read. The latest entry gives the expected
//! duration ([`super::queue`] runs the longest first) and the explanation of a rerun ([`Why`]).
//! The store never replaces a value, so the chain only grows; its end is found by doubling
//! and halving, a few gets however long it is.

use std::{
    collections::BTreeMap, convert::Infallible, ffi::OsString, fmt, path::PathBuf, time::Duration,
};

use blaze_cas::{Corrupt, Key, Kind, Mismatch, Store, Ticket};
use blaze_core::{Traced, Untraceable};
use blaze_ids::{ContentId, Hasher};
use blaze_items::CodeHash;

use crate::{
    Given, Reached, RunKey, RunKind,
    index::{self, Shape},
    trace::{self, Inputs, Report},
};

/// Where passes and histories are kept: the store ([`Kind::Run`]), or a map in a test.
pub trait Passes {
    /// The value of `key`, verified.
    fn get(&self, key: &Key) -> Result<Option<&[u8]>, Corrupt>;
    /// Keeps `bytes` under `key`: a second put of equal bytes is nothing, of other bytes an
    /// error.
    fn put(&mut self, key: Key, bytes: Vec<u8>) -> Result<Ticket, Mismatch>;
}

impl Passes for Store {
    fn get(&self, key: &Key) -> Result<Option<&[u8]>, Corrupt> {
        Ok(Store::get(self, key)?.map(|entry| entry.bytes))
    }

    fn put(&mut self, key: Key, bytes: Vec<u8>) -> Result<Ticket, Mismatch> {
        Store::put(self, key, Kind::Run, bytes)
    }
}

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum ShouldPanic {
    No,
    Yes,
    WithMessage(String),
}

/// What of a test's attributes changes what running it means.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct Attrs {
    pub ignore: bool,
    pub should_panic: ShouldPanic,
    pub bench: bool,
}

impl Default for Attrs {
    fn default() -> Attrs {
        Attrs { ignore: false, should_panic: ShouldPanic::No, bench: false }
    }
}

impl Attrs {
    pub fn id(&self) -> ContentId {
        let mut hasher = Hasher::new("blaze v2 test attributes 1");
        hasher.update(&[u8::from(self.ignore), u8::from(self.bench)]);
        match &self.should_panic {
            ShouldPanic::No => hasher.update(&[0]),
            ShouldPanic::Yes => hasher.update(&[1]),
            ShouldPanic::WithMessage(message) => hasher.update(&[2]).part(message.as_bytes()),
        };
        hasher.finish()
    }
}

/// A test as selection sees it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Test {
    /// The name libtest shows, the path from the crate's root.
    pub name: String,
    /// The code hashes of what it reaches, in the order codegen's walk found them
    /// ([`Reached::reached`]): an explanation names the first that changed.
    pub reached: Vec<CodeHash>,
    pub attrs: Attrs,
    /// It reaches the outside world (files, the environment, processes, the network through
    /// code the walk can't see into), as codegen marks it: it always runs.
    pub outside: bool,
}

/// The name of the test that stands for a whole binary run as libtest's threads.
pub const WHOLE: &str = "\0the whole binary";

impl Test {
    pub fn of(reached: &Reached, attrs: Attrs, outside: bool) -> Test {
        Test { name: reached.test.clone(), reached: reached.reached.clone(), attrs, outside }
    }

    /// The test, binary-independent: its name, the set of code hashes it reaches and its
    /// attributes.
    pub fn identity(&self) -> ContentId {
        let set = sorted(&self.reached);
        let mut hasher = Hasher::new("blaze v2 test identity 1");
        hasher.part(self.name.as_bytes()).id(&self.attrs.id());
        hasher.update(&(set.len() as u64).to_le_bytes());
        for hash in &set {
            hasher.id(&hash.0);
        }
        hasher.finish()
    }

    /// The test that stands for `tests` run together in one process: it reaches what any of
    /// them reaches and is outside when any is. Which tests they are and their attributes
    /// are in [`Suite::given_whole`].
    pub fn whole(tests: &[Test]) -> Test {
        let mut reached = Vec::new();
        let mut seen = std::collections::BTreeSet::new();
        for test in tests {
            for hash in &test.reached {
                if seen.insert(*hash) {
                    reached.push(*hash);
                }
            }
        }
        Test {
            name: WHOLE.to_owned(),
            reached,
            attrs: Attrs::default(),
            outside: tests.iter().any(|it| it.outside),
        }
    }
}

fn sorted(hashes: &[CodeHash]) -> Vec<CodeHash> {
    let mut set = hashes.to_vec();
    set.sort_unstable();
    set.dedup();
    set
}

/// Which test binary a test is of, without the binary: the same test of another package or
/// target is another test. `context` is the hash of everything outside the reached code that
/// what the binary does depends on (the target triple, the toolchain, the codegen settings
/// and panic strategy), made by whoever builds the binary; a key that misses one is a bug.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Suite {
    pub package: String,
    /// The target as cargo names it: `lib`, `test:parse`, `bin:blaze`.
    pub target: String,
    pub context: ContentId,
}

impl Suite {
    fn base(&self) -> Given {
        let mut given = Given::new(RunKind::Test);
        given
            .part("package", self.package.as_bytes())
            .part("target", self.target.as_bytes())
            .id("context", &self.context);
        given
    }

    /// What each test of the suite run in a process of its own is given.
    pub fn given(&self) -> ContentId {
        let mut given = self.base();
        given.part("mode", b"process");
        given.finish()
    }

    /// What `tests` run together as libtest's threads are given: which tests they are, with
    /// their attributes.
    pub fn given_whole(&self, tests: &[Test]) -> ContentId {
        let mut members: Vec<(&str, ContentId)> =
            tests.iter().map(|it| (it.name.as_str(), it.attrs.id())).collect();
        members.sort();
        let mut hasher = Hasher::new("blaze v2 test members 1");
        for (name, attrs) in &members {
            hasher.part(name.as_bytes()).id(attrs);
        }
        let mut given = self.base();
        given.part("mode", b"threads").id("members", &hasher.finish());
        given.finish()
    }
}

/// Whether libtest's flags ask for a test.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Ignored {
    /// `#[ignore]` tests are reported ignored, not run (the default).
    Skip,
    /// `--ignored`: only they run.
    Only,
    /// `--include-ignored`: they run too.
    Include,
}

/// The names asked for, as libtest takes them.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Filter {
    /// A test is asked for when its name holds any of these (equals one, with `exact`); all
    /// are when there are none.
    pub names: Vec<String>,
    pub exact: bool,
    /// `--skip`: never a test whose name holds one of these (equals, with `exact`).
    pub skip: Vec<String>,
    pub ignored: Ignored,
}

impl Default for Filter {
    fn default() -> Filter {
        Filter { names: Vec::new(), exact: false, skip: Vec::new(), ignored: Ignored::Skip }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Wanted {
    Yes,
    /// Not asked for: not reported.
    Filtered,
    /// Reported ignored, not run.
    Ignored,
}

pub fn wanted(test: &Test, filter: &Filter) -> Wanted {
    let matches = |pattern: &String| match filter.exact {
        true => test.name == *pattern,
        false => test.name.contains(pattern.as_str()),
    };
    if !filter.names.is_empty() && !filter.names.iter().any(matches) {
        return Wanted::Filtered;
    }
    if filter.skip.iter().any(matches) {
        return Wanted::Filtered;
    }
    match (filter.ignored, test.attrs.ignore) {
        (Ignored::Skip, true) => Wanted::Ignored,
        (Ignored::Only, false) => Wanted::Filtered,
        _ => Wanted::Yes,
    }
}

/// What a run read that is not as it was.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Input {
    File(PathBuf),
    Dir(PathBuf),
    Env(OsString),
}

/// Why a test runs (`--explain`).
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Why {
    /// It has never run here.
    New,
    /// Its last run failed, or passed with what can't be kept (it read the network, or what
    /// it read is not all known), or its pass is gone from the store.
    NoPass,
    /// It reaches the outside world.
    Outside,
    /// Every test was asked for.
    All,
    /// The first code hash it reaches, in the walk's order, that it did not reach when it
    /// last passed.
    Changed(CodeHash),
    /// It no longer reaches this, and reaches nothing new.
    Dropped(CodeHash),
    /// Its attributes (`#[ignore]`, `#[should_panic]`) are not as they were.
    Attributes,
    /// The same code, and something it read at run time is not as it was.
    Read(Input),
}

impl Why {
    /// The explanation, with a code hash named by `name_of` when it knows the function.
    pub fn describe(&self, name_of: &dyn Fn(&CodeHash) -> Option<String>) -> String {
        let named = |hash: &CodeHash| name_of(hash).unwrap_or_else(|| format!("code {}", hash.0));
        match self {
            Why::New => "new".to_owned(),
            Why::NoPass => "no pass kept".to_owned(),
            Why::Outside => "outside world".to_owned(),
            Why::All => "all tests asked for".to_owned(),
            Why::Changed(hash) => format!("reaches {}, changed", named(hash)),
            Why::Dropped(hash) => format!("no longer reaches {}", named(hash)),
            Why::Attributes => "attributes changed".to_owned(),
            Why::Read(Input::File(path)) => format!("read {}, changed", path.display()),
            Why::Read(Input::Dir(path)) => format!("listed {}, changed", path.display()),
            Why::Read(Input::Env(name)) => format!("read ${}, changed", name.to_string_lossy()),
        }
    }
}

impl fmt::Display for Why {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.describe(&|_| None))
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Decision {
    /// It runs; `expected` is how long it took when it last ran, if it has.
    Run { why: Why, expected: Option<Duration> },
    /// A pass is kept for it as it is now: it is reported unreached, and took this when it
    /// passed last.
    Skip { took: Option<Duration> },
}

#[derive(Debug)]
pub enum SelectError<E> {
    Corrupt(Corrupt),
    /// A value under a key of ours that is not what we keep there: a key of another schema.
    NotOurs { key: Key, why: String },
    Inputs(E),
}

impl<E> From<Corrupt> for SelectError<E> {
    fn from(it: Corrupt) -> SelectError<E> {
        SelectError::Corrupt(it)
    }
}

impl<E: fmt::Display> fmt::Display for SelectError<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SelectError::Corrupt(it) => it.fmt(f),
            SelectError::NotOurs { key, why } => {
                write!(f, "the store's value of {:?} is not a test's: {why}", key.0)
            }
            SelectError::Inputs(it) => it.fmt(f),
        }
    }
}

/// Whether `test`, of the suite given `given` ([`Suite::given`], or [`Suite::given_whole`]
/// with [`Test::whole`]), runs, with every test asked for when `all`. `inputs` are the files
/// and environment as the test would see them now.
pub fn select<P: Passes + ?Sized, I: Inputs>(
    store: &P,
    given: ContentId,
    test: &Test,
    all: bool,
    inputs: &mut I,
) -> Result<Decision, SelectError<I::Error>> {
    let last = last(store, given, &test.name)
        .map_err(|error| match error {
            SelectError::Corrupt(it) => SelectError::Corrupt(it),
            SelectError::NotOurs { key, why } => SelectError::NotOurs { key, why },
            SelectError::Inputs(never) => match never {},
        })?
        .map(|it| it.1);
    let expected = last.as_ref().map(|it| it.took);
    if test.outside {
        return Ok(Decision::Run { why: Why::Outside, expected });
    }
    if all {
        return Ok(Decision::Run { why: Why::All, expected });
    }
    let run = RunKey { code: test.identity(), given };
    if find(store, &run, inputs)? {
        return Ok(Decision::Skip { took: expected });
    }
    let why = explain(store, test, last.as_ref(), inputs)?;
    Ok(Decision::Run { why, expected })
}

/// Whether a pass of `run` is kept for its inputs as they are now: [`index::find`]'s search,
/// over any [`Passes`].
fn find<P: Passes + ?Sized, I: Inputs>(
    store: &P,
    run: &RunKey,
    inputs: &mut I,
) -> Result<bool, SelectError<I::Error>> {
    let mut n = 0;
    loop {
        let key = index::slot(run, n);
        let Some(bytes) = store.get(&key)? else { return Ok(false) };
        let shape = Shape::decode(bytes).map_err(|why| SelectError::NotOurs { key, why })?;
        let traced = shape.values(inputs).map_err(SelectError::Inputs)?;
        let key = index::result_key(run, &traced);
        if let Some(bytes) = store.get(&key)? {
            return match bytes == PASS {
                true => Ok(true),
                false => Err(SelectError::NotOurs { key, why: "not a test's pass".to_owned() }),
            };
        }
        n += 1;
    }
}

fn explain<P: Passes + ?Sized, I: Inputs>(
    store: &P,
    test: &Test,
    last: Option<&History>,
    inputs: &mut I,
) -> Result<Why, SelectError<I::Error>> {
    let Some(last) = last else { return Ok(Why::New) };
    if !last.passed || !last.kept {
        return Ok(Why::NoPass);
    }
    if last.attrs != test.attrs.id() {
        return Ok(Why::Attributes);
    }
    let Some(bytes) = store.get(&last.reached)? else { return Ok(Why::NoPass) };
    let before = decode_reached(bytes).map_err(|why| SelectError::NotOurs { key: last.reached, why })?;
    if let Some(hash) = test.reached.iter().find(|it| before.binary_search(*it).is_err()) {
        return Ok(Why::Changed(*hash));
    }
    let now = sorted(&test.reached);
    if let Some(hash) = before.iter().find(|it| now.binary_search(*it).is_err()) {
        return Ok(Why::Dropped(*hash));
    }
    let Some(traced) = &last.traced else { return Ok(Why::NoPass) };
    let now = Shape::of(traced).values(inputs).map_err(SelectError::Inputs)?;
    Ok(changed_input(traced, &now).map_or(Why::NoPass, Why::Read))
}

/// The first input of `before` whose value `now` does not have.
fn changed_input(before: &Traced, now: &Traced) -> Option<Input> {
    let files: BTreeMap<_, _> = now.files.iter().map(|(path, hash)| (path, hash)).collect();
    let dirs: BTreeMap<_, _> = now.dirs.iter().map(|(path, hash)| (path, hash)).collect();
    let env: BTreeMap<_, _> = now.env.iter().map(|(name, value)| (name, value)).collect();
    for (path, hash) in &before.files {
        if files.get(path) != Some(&hash) {
            return Some(Input::File(path.clone()));
        }
    }
    for (path, hash) in &before.dirs {
        if dirs.get(path) != Some(&hash) {
            return Some(Input::Dir(path.clone()));
        }
    }
    for (name, value) in &before.env {
        if env.get(name) != Some(&value) {
            let name = match name == trace::ALL_ENV_NAME {
                true => OsString::from("*"),
                false => name.clone(),
            };
            return Some(Input::Env(name));
        }
    }
    None
}

/// How a run went, to record.
#[derive(Clone, Copy, Debug)]
pub struct Ran<'a> {
    pub passed: bool,
    pub took: Duration,
    /// What it read at run time ([`super::queue::Trace::traced`]); `None` when that is not
    /// known, and then no pass is kept.
    pub traced: Option<&'a Traced>,
}

#[derive(Debug)]
pub enum RecordError {
    Corrupt(Corrupt),
    /// The store has a value of a pass's key with other bytes: never kept over.
    Mismatch(Mismatch),
    /// The last entry of the test's history is not one.
    NotOurs { key: Key, why: String },
}

impl fmt::Display for RecordError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RecordError::Corrupt(it) => it.fmt(f),
            RecordError::NotOurs { key, why } => {
                write!(f, "the store's value of {:?} is not a test's history: {why}", key.0)
            }
            RecordError::Mismatch(it) => write!(
                f,
                "the store has a test's pass under {:?} with other bytes ({:?}, not {:?})",
                it.key.0, it.kept, it.given
            ),
        }
    }
}

impl From<Corrupt> for RecordError {
    fn from(it: Corrupt) -> RecordError {
        RecordError::Corrupt(it)
    }
}

/// Whether a pass that read `traced` can be kept: not when it read the network or what it
/// read is not all known. One that read the clock or random numbers is kept, as libtest's
/// result of it would be taken: a test whose result depends on them is flaky either way.
pub fn keepable(traced: &Traced) -> bool {
    !traced.untraceable.iter().any(|it| matches!(it, Untraceable::Network | Untraceable::Descriptor))
}

/// Records how `test` ran: its pass, when it passed and that can be kept, and an entry of its
/// history. The tickets of what was put.
pub fn record<P: Passes + ?Sized>(
    store: &mut P,
    given: ContentId,
    test: &Test,
    ran: &Ran<'_>,
) -> Result<Vec<Ticket>, RecordError> {
    let mut tickets = Vec::new();
    let traced = ran.traced.map(|it| {
        let mut it = it.clone();
        trace::normalize(&mut it);
        it
    });
    let kept = ran.passed && !test.outside && traced.as_ref().is_some_and(keepable);
    if kept && let Some(traced) = &traced {
        let run = RunKey { code: test.identity(), given };
        tickets.push(store.put(index::result_key(&run, traced), PASS.to_vec()).map_err(RecordError::Mismatch)?);
        let shape = Shape::of(traced).encode();
        let mut n = 0;
        loop {
            let at = index::slot(&run, n);
            match store.get(&at)? {
                Some(bytes) if bytes == shape.as_slice() => break,
                Some(_) => n += 1,
                None => match store.put(at, shape.clone()) {
                    Ok(ticket) => {
                        tickets.push(ticket);
                        break;
                    }
                    // Another kept another shape here first.
                    Err(_) => n += 1,
                },
            }
        }
    }
    let set = encode_reached(&sorted(&test.reached));
    let reached = Key(ContentId::of("blaze v2 test reached set 1", &set));
    if store.get(&reached)?.is_none() {
        tickets.push(store.put(reached, set).map_err(RecordError::Mismatch)?);
    }
    let entry = History {
        passed: ran.passed,
        kept,
        attrs: test.attrs.id(),
        reached,
        took: ran.took,
        traced: traced.filter(|_| kept),
    }
    .encode();
    let mut n = last(store, given, &test.name)
        .map_err(|error| match error {
            SelectError::Corrupt(it) => RecordError::Corrupt(it),
            SelectError::NotOurs { key, why } => RecordError::NotOurs { key, why },
            SelectError::Inputs(never) => match never {},
        })?
        .map_or(0, |it| it.0 + 1);
    loop {
        let at = history_slot(given, &test.name, n);
        match store.get(&at)? {
            Some(bytes) if bytes == entry.as_slice() => break,
            Some(_) => n += 1,
            None => match store.put(at, entry.clone()) {
                Ok(ticket) => {
                    tickets.push(ticket);
                    break;
                }
                Err(_) => n += 1,
            },
        }
    }
    Ok(tickets)
}

/// The value kept under a pass's key: the same bytes for every pass, so two runs that passed
/// with the same inputs keep one value.
const PASS: &[u8] = b"blaze v2 test pass 1";
const HISTORY: &[u8] = b"blaze v2 test history 1";
const REACHED: &[u8] = b"blaze v2 test reached 1";

/// One recorded run of a test.
#[derive(Clone, PartialEq, Eq, Debug)]
struct History {
    passed: bool,
    /// A pass was kept for it.
    kept: bool,
    attrs: ContentId,
    /// The key of the set of code hashes it reached.
    reached: Key,
    took: Duration,
    /// What it read, for a kept pass.
    traced: Option<Traced>,
}

impl History {
    fn encode(&self) -> Vec<u8> {
        let mut out = HISTORY.to_vec();
        out.push(u8::from(self.passed) | (u8::from(self.kept) << 1));
        out.extend_from_slice(&self.attrs.0);
        out.extend_from_slice(&self.reached.0.0);
        out.extend_from_slice(&(self.took.as_micros().min(u128::from(u64::MAX)) as u64).to_le_bytes());
        let traced = match &self.traced {
            Some(traced) => trace::encode(&Report::Ran(Some(traced.clone()))),
            None => Vec::new(),
        };
        out.extend_from_slice(&(traced.len() as u64).to_le_bytes());
        out.extend_from_slice(&traced);
        out
    }

    fn decode(bytes: &[u8]) -> Result<History, String> {
        let rest = bytes.strip_prefix(HISTORY).ok_or_else(|| "not a test's history".to_owned())?;
        if rest.len() < 1 + 32 + 32 + 8 + 8 {
            return Err("a test's history ends early".to_owned());
        }
        let flags = rest[0];
        let id = |at: usize| ContentId(rest[at..at + 32].try_into().expect("32 bytes"));
        let took = u64::from_le_bytes(rest[65..73].try_into().expect("8 bytes"));
        let len = u64::from_le_bytes(rest[73..81].try_into().expect("8 bytes"));
        let traced = &rest[81..];
        if traced.len() as u64 != len {
            return Err("a test's history has a record of another length than it says".to_owned());
        }
        let traced = match traced.is_empty() {
            true => None,
            false => match trace::decode(traced)? {
                Report::Ran(Some(traced)) => Some(traced),
                _ => return Err("a test's history holds a report with no record".to_owned()),
            },
        };
        Ok(History {
            passed: flags & 1 != 0,
            kept: flags & 2 != 0,
            attrs: id(1),
            reached: Key(id(33)),
            took: Duration::from_micros(took),
            traced,
        })
    }
}

fn encode_reached(set: &[CodeHash]) -> Vec<u8> {
    let mut out = REACHED.to_vec();
    out.extend_from_slice(&(set.len() as u64).to_le_bytes());
    for hash in set {
        out.extend_from_slice(&hash.0.0);
    }
    out
}

/// The sorted set [`encode_reached`] wrote.
fn decode_reached(bytes: &[u8]) -> Result<Vec<CodeHash>, String> {
    let rest = bytes.strip_prefix(REACHED).ok_or_else(|| "not a reached set".to_owned())?;
    let (count, rest) = rest.split_at_checked(8).ok_or_else(|| "a reached set ends early".to_owned())?;
    let count = u64::from_le_bytes(count.try_into().expect("8 bytes"));
    if rest.len() as u64 != count.saturating_mul(32) {
        return Err("a reached set holds another number of hashes than it says".to_owned());
    }
    Ok(rest
        .chunks_exact(32)
        .map(|it| CodeHash(ContentId(it.try_into().expect("32 bytes"))))
        .collect())
}

fn history_slot(given: ContentId, name: &str, n: u32) -> Key {
    Key(Hasher::new("blaze v2 test history slot 1")
        .id(&given)
        .part(name.as_bytes())
        .update(&n.to_le_bytes())
        .finish())
}

/// The last entry of a test's history and its slot. Slots are filled from 0 with no gap (an
/// entry is put only in the first free one), so the end is found by doubling then halving.
fn last<P: Passes + ?Sized>(
    store: &P,
    given: ContentId,
    name: &str,
) -> Result<Option<(u32, History)>, SelectError<Infallible>> {
    let filled = |n: u32| -> Result<bool, Corrupt> { Ok(store.get(&history_slot(given, name, n))?.is_some()) };
    if !filled(0)? {
        return Ok(None);
    }
    // `low` is filled; `high` is empty, or `u32::MAX` and filled.
    let (mut low, mut high) = (0u32, 1u32);
    while filled(high)? {
        low = high;
        if high == u32::MAX {
            break;
        }
        high = high.saturating_mul(2);
    }
    while high - low > 1 {
        let middle = low + (high - low) / 2;
        match filled(middle)? {
            true => low = middle,
            false => high = middle,
        }
    }
    let key = history_slot(given, name, low);
    let Some(bytes) = store.get(&key)? else { return Ok(None) };
    let history = History::decode(bytes).map_err(|why| SelectError::NotOurs { key, why })?;
    Ok(Some((low, history)))
}

#[cfg(test)]
mod tests;
