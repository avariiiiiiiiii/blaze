//! Finding a run's result in the store (docs/v2/plan/lanes/usercode/notes.md, "Keys").
//!
//! A result is kept under [`RunKey::traced`]: the run's code, what it was given, and every
//! input it read with its value then. To find one for a run about to happen, the store keeps
//! beside it the run's **shapes**: which paths, listings and variables a run of this
//! [`RunKey`] read, without their values. A shape is kept once, in the first free slot of a
//! chain under the run key ([`slot`]); a run key has as many shapes as its runs read
//! different things (usually one: a script reads the same files every run, whatever is in
//! them). Finding is then, for each shape, the values of its inputs now ([`Shape::values`])
//! and one get of the key they make: no list of past results is searched.
//!
//! The store never replaces a value, so the chain only grows, a slot at a time; two daemons
//! that keep different shapes in one slot see the [`Mismatch`] and move to the next.

use std::{
    ffi::{OsStr, OsString},
    path::PathBuf,
};

use blaze_cas::{Corrupt, Guard, Key, Kind, Mismatch, Reader, Store, Ticket};
use blaze_core::{Traced, Untraceable};
use blaze_ids::{ContentId, Hasher};
use blaze_mem::{Tag, frame};

use crate::{
    RunKey,
    trace::{self, ALL_ENV_NAME, At, Inputs, os_from},
};

/// What a run read, without the values.
#[derive(Clone, PartialEq, Eq, Default, Debug)]
pub struct Shape {
    pub files: Vec<PathBuf>,
    pub dirs: Vec<PathBuf>,
    /// The variables by name, with [`ALL_ENV_NAME`] for a run that read the whole
    /// environment.
    pub env: Vec<OsString>,
    pub untraceable: Vec<Untraceable>,
}

impl Shape {
    /// The shape of `traced`, in the order [`trace::normalize`] leaves it, which is sorted by
    /// reference and never copied: the shape owns only its names.
    pub fn of(traced: &Traced) -> Shape {
        frame::task(Tag::Usercode, |frame| {
            let env = trace::sorted_in(&traced.env, frame);
            let all = env.iter().any(|it| it.0 == ALL_ENV_NAME);
            // The variables a run that read them all saw are any the environment has then;
            // what it asked for by name and was not there is named still.
            let env = env
                .iter()
                .filter(|(name, value)| !all || name == ALL_ENV_NAME || value.is_none())
                .map(|it| it.0.clone()) // clone: the names the shape keeps
                .collect(); // alloc: the shape the caller keeps
            Shape {
                // clone: the paths the shape keeps
                files: trace::sorted_in(&traced.files, frame).iter().map(|it| it.0.clone()).collect(),
                // clone: the paths the shape keeps
                dirs: trace::sorted_in(&traced.dirs, frame).iter().map(|it| it.0.clone()).collect(),
                env,
                untraceable: trace::sorted_in(&traced.untraceable, frame).into_iter().copied().collect(), // alloc: kept
            }
        })
    }

    /// The inputs of this shape as `inputs` has them: what a run that read them would have
    /// recorded. A directory that is no longer there is a file that is not there, as
    /// [`trace::assemble`] has it.
    pub fn values<I: Inputs>(&self, inputs: &mut I) -> Result<Traced, I::Error> {
        let mut traced = Traced::default();
        for path in &self.files {
            match trace::walked(path) {
                Some((dir, package)) => match trace::walk(inputs, dir, package)? {
                    Some(hash) => traced.dirs.push((path.clone(), hash)),
                    None => traced.files.push((path.clone(), None)),
                },
                None => traced.files.push((path.clone(), inputs.file(path)?)),
            }
        }
        for path in &self.dirs {
            let hash = match trace::walked(path) {
                Some((dir, package)) => trace::walk(inputs, dir, package)?,
                None => inputs.dir(path)?,
            };
            match hash {
                Some(hash) => traced.dirs.push((path.clone(), hash)),
                None => traced.files.push((path.clone(), None)),
            }
        }
        for name in &self.env {
            if name == ALL_ENV_NAME {
                let all = inputs.env_all();
                traced.env.push((ALL_ENV_NAME.into(), Some(trace::names_hash(all).into())));
                traced.env.extend(all.iter().map(|(name, value)| (name.clone(), Some(value.clone()))));
            } else {
                traced.env.push((name.clone(), inputs.env(name).map(OsStr::to_owned)));
            }
        }
        traced.untraceable = self.untraceable.clone();
        trace::normalize(&mut traced);
        Ok(traced)
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = SHAPE_MAGIC.to_vec();
        let part = |out: &mut Vec<u8>, bytes: &[u8]| {
            out.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
            out.extend_from_slice(bytes);
        };
        for list in [&self.files, &self.dirs] {
            out.extend_from_slice(&(list.len() as u64).to_le_bytes());
            for path in list {
                part(&mut out, &trace::os_bytes(path.as_os_str()));
            }
        }
        out.extend_from_slice(&(self.env.len() as u64).to_le_bytes());
        for name in &self.env {
            part(&mut out, &trace::os_bytes(name));
        }
        out.extend_from_slice(&(self.untraceable.len() as u64).to_le_bytes());
        for it in &self.untraceable {
            part(&mut out, trace::untraceable_name(*it).as_bytes());
        }
        out
    }

    pub fn decode(bytes: &[u8]) -> Result<Shape, String> {
        let rest = bytes
            .strip_prefix(SHAPE_MAGIC.as_slice())
            .ok_or_else(|| "not a shape".to_owned())?;
        let mut at = At(rest);
        let mut shape = Shape::default();
        for _ in 0..at.len()? {
            shape.files.push(PathBuf::from(os_from(at.part()?)?));
        }
        for _ in 0..at.len()? {
            shape.dirs.push(PathBuf::from(os_from(at.part()?)?));
        }
        for _ in 0..at.len()? {
            shape.env.push(os_from(at.part()?)?);
        }
        for _ in 0..at.len()? {
            let name = String::from_utf8_lossy(at.part()?).into_owned();
            shape.untraceable.push(
                trace::untraceable_of(&name).ok_or_else(|| format!("no untraceable is {name:?}"))?,
            );
        }
        match at.0.is_empty() {
            true => Ok(shape),
            false => Err("the shape has bytes after its end".to_owned()),
        }
    }
}

/// Another on Windows, whose names are [`trace::os_bytes`]' UTF-16 units: a shape of an
/// older blaze's bytes there is not one.
const SHAPE_MAGIC: &[u8; 16] = if cfg!(windows) { b"blaze v2 shape 2" } else { b"blaze v2 shape 1" };

/// The key of slot `n` of `run`'s chain of shapes.
pub fn slot(run: &RunKey, n: u32) -> Key {
    Key(Hasher::new("blaze v2 run shape slot 1")
        .id(&run.code)
        .id(&run.given)
        .update(&n.to_le_bytes())
        .finish())
}

/// The store's key of a result.
pub fn result_key(run: &RunKey, traced: &Traced) -> Key {
    Key(run.traced(traced))
}

/// What [`find`] found.
#[derive(Debug)]
pub enum Found<'s> {
    /// A result whose every input is as it is now: its key, and the value kept, in place.
    Hit { key: Key, value: &'s [u8] },
    /// No result of a shape the run has had is for the inputs as they are now; `shapes` is
    /// how many it has had.
    Miss { shapes: u32 },
}

#[derive(Debug)]
pub enum FindError<E> {
    Corrupt(Corrupt),
    /// A slot that holds what is not a shape: the store's bytes were checked, so a key of
    /// another schema is in this slot.
    NotAShape { key: Key, why: String },
    Inputs(E),
}

/// The result of `run` for `inputs` as they are now, if the store has one: read in place on
/// any thread, the hit's bytes valid while `guard`'s task runs.
pub fn find<'s, I: Inputs>(
    store: &'s Reader,
    guard: &'s Guard,
    run: &RunKey,
    inputs: &mut I,
) -> Result<Found<'s>, FindError<I::Error>> {
    search(|key| Ok(store.get(guard, key)?.map(|entry| entry.bytes)), run, inputs)
}

/// [`find`] for the store's own thread, which holds it.
pub(crate) fn find_held<'s, I: Inputs>(
    store: &'s Store,
    run: &RunKey,
    inputs: &mut I,
) -> Result<Found<'s>, FindError<I::Error>> {
    search(|key| Ok(store.get(key)?.map(|entry| entry.bytes)), run, inputs)
}

pub(crate) fn search<'s, I: Inputs>(
    get: impl Fn(&Key) -> Result<Option<&'s [u8]>, Corrupt>,
    run: &RunKey,
    inputs: &mut I,
) -> Result<Found<'s>, FindError<I::Error>> {
    let mut n = 0;
    loop {
        let at = slot(run, n);
        let Some(bytes) = get(&at).map_err(FindError::Corrupt)? else {
            return Ok(Found::Miss { shapes: n });
        };
        let keyed = keyed(bytes, run, inputs).map_err(|error| match error {
            Keyed::NotAShape(why) => FindError::NotAShape { key: at, why },
            Keyed::Inputs(error) => FindError::Inputs(error),
        })?;
        if let Some(key) = keyed.map(Key)
            && let Some(value) = get(&key).map_err(FindError::Corrupt)?
        {
            return Ok(Found::Hit { key, value });
        }
        n += 1;
    }
}

enum Keyed<E> {
    NotAShape(String),
    Inputs(E),
}

/// The key of the result of `run` that read what the shape `bytes` is, with its inputs as
/// `inputs` has them now: the shape read in place and each input hashed into the key as it is
/// asked, so a check builds no [`Traced`] and copies no name. `None` when a directory of the
/// shape is no longer one: what read it then has no result that read this.
///
/// What it hashes is what [`RunKey::traced`] of [`Shape::values`] hashes: the shape is the
/// normalized order, and the whole environment, sorted as [`Inputs::env_all`] lends it, is
/// merged into the names the run asked for by the same order, the names' own (on Windows
/// not their bytes').
fn keyed<I: Inputs>(bytes: &[u8], run: &RunKey, inputs: &mut I) -> Result<Option<ContentId>, Keyed<I::Error>> {
    let bad = Keyed::NotAShape;
    let all_name = trace::os_bytes(OsStr::new(ALL_ENV_NAME));
    let rest = bytes.strip_prefix(SHAPE_MAGIC.as_slice()).ok_or_else(|| bad("not a shape".to_owned()))?;
    let mut at = At(rest);
    let mut keyer = trace::Keyer::new(run);
    for _ in 0..at.len().map_err(bad)? {
        let bytes = at.part().map_err(bad)?;
        let path = trace::path_in(bytes).map_err(bad)?;
        let hash = match trace::walked(&path) {
            // A walked directory that was not there: one there now is what the run did not
            // read.
            Some((dir, package)) => match trace::walk(inputs, dir, package).map_err(Keyed::Inputs)? {
                Some(_) => return Ok(None),
                None => None,
            },
            None => inputs.file(&path).map_err(Keyed::Inputs)?,
        };
        keyer.file(bytes, hash.as_ref());
    }
    for _ in 0..at.len().map_err(bad)? {
        let bytes = at.part().map_err(bad)?;
        let path = trace::path_in(bytes).map_err(bad)?;
        let hash = match trace::walked(&path) {
            Some((dir, package)) => trace::walk(inputs, dir, package),
            None => inputs.dir(&path),
        };
        match hash.map_err(Keyed::Inputs)? {
            Some(hash) => keyer.dir(bytes, &hash),
            None => return Ok(None),
        }
    }
    let names = at.len().map_err(bad)?;
    let mut asked = At(at.0);
    let mut whole = false;
    for _ in 0..names {
        whole |= asked.part().map_err(bad)? == &*all_name;
    }
    if whole {
        let all = inputs.env_all();
        let hex = trace::names_hex(all);
        let hex = trace::os_bytes(OsStr::new(std::str::from_utf8(&hex).expect("a hash in hex is ASCII")));
        let mut all = all.iter().peekable();
        for _ in 0..names {
            let name = at.part().map_err(bad)?;
            let named = trace::os_in(name).map_err(bad)?;
            while let Some((set, value)) = all.next_if(|it| it.0.as_os_str() < &*named) {
                keyer.env(&trace::os_bytes(set), Some(&*trace::os_bytes(value)));
            }
            if name == &*all_name {
                keyer.env(name, Some(&*hex));
            } else if let Some((_, value)) = all.next_if(|it| it.0.as_os_str() == &*named) {
                keyer.env(name, Some(&*trace::os_bytes(value)));
            } else {
                // What the whole environment has not is not set.
                keyer.env(name, None);
            }
        }
        for (set, value) in all {
            keyer.env(&trace::os_bytes(set), Some(&*trace::os_bytes(value)));
        }
    } else {
        for _ in 0..names {
            let name = at.part().map_err(bad)?;
            let value = inputs.env(&trace::os_in(name).map_err(bad)?).map(trace::os_bytes);
            keyer.env(name, value.as_deref());
        }
    }
    for _ in 0..at.len().map_err(bad)? {
        let name = at.part().map_err(bad)?;
        std::str::from_utf8(name)
            .ok()
            .and_then(trace::untraceable_of)
            .ok_or_else(|| bad(format!("no untraceable is {:?}", String::from_utf8_lossy(name))))?;
        keyer.untraceable(name);
    }
    match at.0.is_empty() {
        true => Ok(Some(keyer.finish())),
        false => Err(bad("the shape has bytes after its end".to_owned())),
    }
}

/// Why a result could not be kept.
#[derive(Debug)]
pub enum KeepError {
    Corrupt(Corrupt),
    /// The store has a value of the result's key with other bytes: the key misses an input
    /// the value depends on, or the run is not deterministic. Never kept over.
    Mismatch(Mismatch),
}

/// Keeps `value`, the result of `run`, which read `traced`: the result under its key and its
/// shape in the chain if the chain has it not. The tickets of what was put.
pub fn keep(
    store: &mut Store,
    run: &RunKey,
    traced: &Traced,
    value: Vec<u8>,
) -> Result<Vec<Ticket>, KeepError> {
    let mut tickets = Vec::new();
    let key = result_key(run, traced);
    tickets.push(store.put(key, Kind::Run, value).map_err(KeepError::Mismatch)?);
    let shape = Shape::of(traced).encode();
    let mut n = 0;
    loop {
        let at = slot(run, n);
        let same = store.get(&at).map_err(KeepError::Corrupt)?.map(|it| it.bytes == shape.as_slice());
        match same {
            Some(true) => return Ok(tickets),
            Some(false) => n += 1,
            None => match store.put(at, Kind::Run, shape.clone()) {
                Ok(ticket) => {
                    tickets.push(ticket);
                    return Ok(tickets);
                }
                // Another kept another shape here first.
                Err(_) => n += 1,
            },
        }
    }
}

/// Where a run read something it can't see again: what [`crate::rule`] keys such a run by.
pub fn untraceable(traced: &Traced) -> bool {
    !traced.untraceable.is_empty()
}
/// The content ID of a shape, for an explanation of a rerun.
pub fn shape_id(shape: &Shape) -> ContentId {
    ContentId::of(trace::os_domain!("blaze v2 shape id 1"), &shape.encode())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    #[test]
    fn a_shape_round_trips_and_forgets_values() {
        let traced = Traced {
            files: vec![("/b".into(), Some([1; 32])), ("/a".into(), None)],
            dirs: vec![("/d".into(), [2; 32])],
            env: vec![("CC".into(), Some("cc".into())), ("X".into(), None)],
            untraceable: vec![Untraceable::Clock],
        };
        let shape = Shape::of(&traced);
        assert_eq!(shape.files, vec![PathBuf::from("/a"), PathBuf::from("/b")]);
        assert_eq!(Shape::decode(&shape.encode()), Ok(shape.clone()));
        let mut other = traced.clone();
        other.files[0].1 = Some([9; 32]);
        assert_eq!(Shape::of(&other), shape);
        let mut bytes = shape.encode();
        bytes.push(0);
        assert!(Shape::decode(&bytes).is_err());
        assert!(Shape::decode(&bytes[..10]).is_err());
    }

    #[test]
    fn a_run_that_read_the_whole_environment_has_one_name_for_it() {
        let traced = Traced {
            env: vec![
                (ALL_ENV_NAME.into(), Some("h".into())),
                ("A".into(), Some("1".into())),
            ],
            ..Traced::default()
        };
        assert_eq!(Shape::of(&traced).env, vec![OsString::from(ALL_ENV_NAME)]);
    }

    #[test]
    fn a_whole_environment_and_a_name_not_there_find_their_result() {
        use std::ffi::OsStr;

        use crate::trace::{Line, Seen, assemble};

        struct Env(Vec<(OsString, OsString)>);
        impl Inputs for Env {
            type Error = ();
            fn file(&mut self, _: &Path) -> Result<Option<[u8; 32]>, ()> {
                Ok(None)
            }
            fn dir(&mut self, _: &Path) -> Result<Option<[u8; 32]>, ()> {
                Ok(None)
            }
            fn env(&mut self, name: &OsStr) -> Option<&OsStr> {
                self.0.iter().find(|it| it.0 == name).map(|it| it.1.as_os_str())
            }
            fn env_all(&mut self) -> &[(OsString, OsString)] {
                self.0.sort();
                &self.0
            }
            fn names(&mut self, _: &Path, _: &mut trace::Names<'_>) -> Result<bool, ()> {
                Ok(false)
            }
        }
        let lines = [Line::new(Seen::Env, "", "*"), Line::new(Seen::Env, "", "UNSET"), Line::new(Seen::Env, "", "A")];
        let mut env = Env(vec![("A".into(), "1".into())]);
        let traced = assemble(&lines, None, &mut env).unwrap();
        let shape = Shape::of(&traced);
        assert_eq!(shape.values(&mut env).unwrap(), traced);
        env.0.push(("UNSET".into(), "now".into()));
        assert_ne!(shape.values(&mut env).unwrap(), traced);
        let mut added = Env(vec![("A".into(), "1".into()), ("B".into(), "2".into())]);
        assert_ne!(shape.values(&mut added).unwrap(), traced);
    }

    /// The key a check streams from a shape's bytes is the key of the [`Traced`] the shape's
    /// values are, for each way the inputs can have changed; a directory gone is no key.
    #[test]
    fn a_check_keys_a_shape_as_its_values_are_keyed() {
        use std::{collections::BTreeMap, ffi::OsStr};

        #[derive(Clone)]
        struct World {
            files: BTreeMap<PathBuf, [u8; 32]>,
            dirs: BTreeMap<PathBuf, [u8; 32]>,
            env: Vec<(OsString, OsString)>,
        }
        impl Inputs for World {
            type Error = ();
            fn file(&mut self, path: &Path) -> Result<Option<[u8; 32]>, ()> {
                Ok(self.files.get(path).copied())
            }
            fn dir(&mut self, path: &Path) -> Result<Option<[u8; 32]>, ()> {
                Ok(self.dirs.get(path).copied())
            }
            fn env(&mut self, name: &OsStr) -> Option<&OsStr> {
                self.env.iter().find(|it| it.0 == name).map(|it| it.1.as_os_str())
            }
            fn env_all(&mut self) -> &[(OsString, OsString)] {
                self.env.sort();
                &self.env
            }
            fn names(&mut self, _: &Path, _: &mut trace::Names<'_>) -> Result<bool, ()> {
                Ok(false)
            }
        }
        let run = RunKey { code: ContentId::of("blaze v2 test 1", b"run"), given: ContentId::default() };
        let world = World {
            files: [("/a".into(), [1; 32]), ("/b".into(), [2; 32])].into(),
            dirs: [("/d".into(), [3; 32])].into(),
            env: vec![("B".into(), "2".into()), ("A".into(), "1".into())],
        };
        for whole in [false, true] {
            let mut traced = Traced {
                files: vec![("/a".into(), Some([1; 32])), ("/b".into(), Some([2; 32])), ("/gone".into(), None)],
                dirs: vec![("/d".into(), [3; 32])],
                env: vec![("A".into(), Some("1".into())), ("UNSET".into(), None)],
                untraceable: vec![Untraceable::Clock],
            };
            if whole {
                let hash = trace::names_hash(&world.clone().env_all().to_vec());
                traced.env.push((ALL_ENV_NAME.into(), Some(hash.into())));
                traced.env.push(("B".into(), Some("2".into())));
            }
            let shape = Shape::of(&traced).encode();
            let changes: [&dyn Fn(&mut World); 5] = [
                &|_: &mut World| {},
                &|it: &mut World| _ = it.files.insert("/b".into(), [9; 32]),
                &|it: &mut World| _ = it.files.insert("/gone".into(), [9; 32]),
                &|it: &mut World| it.env.push(("UNSET".into(), "now".into())),
                &|it: &mut World| it.env.push(("C".into(), "3".into())),
            ];
            for change in changes {
                let mut now = world.clone();
                change(&mut now);
                let values = Shape::decode(&shape).unwrap().values(&mut now.clone()).unwrap();
                let streamed = keyed(&shape, &run, &mut now).ok().unwrap();
                assert_eq!(streamed, Some(run.traced(&values)), "whole {whole}");
            }
            assert_eq!(keyed(&shape, &run, &mut world.clone()).ok().unwrap(), Some(run.traced(&traced)));
            let mut gone = world.clone();
            gone.dirs.clear();
            assert_eq!(keyed(&shape, &run, &mut gone).ok().unwrap(), None);
        }
    }

    /// A check of a kept run's shape, whose files, directories and variables are as they were,
    /// allocates nothing: the shape is read in place, each value lent and hashed into the key.
    #[cfg(unix)]
    #[test]
    fn a_warm_check_of_a_shape_allocates_nothing() {
        use std::{collections::BTreeMap, ffi::OsStr};

        struct World {
            files: BTreeMap<PathBuf, [u8; 32]>,
            dirs: BTreeMap<PathBuf, [u8; 32]>,
            env: Vec<(OsString, OsString)>,
        }
        impl Inputs for World {
            type Error = ();
            fn file(&mut self, path: &Path) -> Result<Option<[u8; 32]>, ()> {
                Ok(self.files.get(path).copied())
            }
            fn dir(&mut self, path: &Path) -> Result<Option<[u8; 32]>, ()> {
                Ok(self.dirs.get(path).copied())
            }
            fn env(&mut self, name: &OsStr) -> Option<&OsStr> {
                self.env.iter().find(|it| it.0 == name).map(|it| it.1.as_os_str())
            }
            fn env_all(&mut self) -> &[(OsString, OsString)] {
                &self.env
            }
            fn names(&mut self, _: &Path, _: &mut trace::Names<'_>) -> Result<bool, ()> {
                Ok(false)
            }
        }
        let run = RunKey { code: ContentId::of("blaze v2 test 1", b"run"), given: ContentId::default() };
        let traced = Traced {
            files: vec![("/a".into(), Some([1; 32])), ("/gone".into(), None)],
            dirs: vec![("/d".into(), [3; 32])],
            env: vec![("A".into(), Some("1".into())), ("UNSET".into(), None)],
            untraceable: vec![],
        };
        let shape = Shape::of(&traced).encode();
        let mut world = World {
            files: [("/a".into(), [1; 32])].into(),
            dirs: [("/d".into(), [3; 32])].into(),
            env: vec![("A".into(), "1".into())],
        };
        let key = blaze_alloc_count::assert_at_most(0, "a kept run's shape checked", || {
            keyed(&shape, &run, &mut world).ok().unwrap()
        });
        assert_eq!(key, Some(run.traced(&traced)));
    }

    #[test]
    fn slots_are_per_run_and_per_number() {
        let run = RunKey { code: ContentId::of("blaze v2 test 1", b"a"), given: ContentId::default() };
        let other = RunKey { code: ContentId::of("blaze v2 test 1", b"b"), given: ContentId::default() };
        assert_ne!(slot(&run, 0), slot(&run, 1));
        assert_ne!(slot(&run, 0), slot(&other, 0));
    }
}
