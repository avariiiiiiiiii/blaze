//! Finding a run's result in the store (docs/v2/usercode-notes.md, "Keys").
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
    ffi::OsString,
    path::{Path, PathBuf},
};

use blaze_cas::{Corrupt, Key, Kind, Mismatch, Store, Ticket};
use blaze_core::{Traced, Untraceable};
use blaze_ids::{ContentId, Hasher};

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
    pub fn of(traced: &Traced) -> Shape {
        let mut traced = traced.clone();
        trace::normalize(&mut traced);
        let mut env: Vec<OsString> = traced.env.into_iter().map(|it| it.0).collect();
        // The variables a run that read them all saw are any the environment has then.
        if env.iter().any(|it| it == ALL_ENV_NAME) {
            env = vec![ALL_ENV_NAME.into()];
        }
        Shape {
            files: traced.files.into_iter().map(|it| it.0).collect(),
            dirs: traced.dirs.into_iter().map(|it| it.0).collect(),
            env,
            untraceable: traced.untraceable,
        }
    }

    /// The inputs of this shape as `inputs` has them: what a run that read them would have
    /// recorded. A directory that is no longer there is a file that is not there, as
    /// [`trace::assemble`] has it.
    pub fn values<I: Inputs>(&self, inputs: &mut I) -> Result<Traced, I::Error> {
        let mut traced = Traced::default();
        for path in &self.files {
            traced.files.push((path.clone(), inputs.file(path)?));
        }
        for path in &self.dirs {
            match inputs.dir(path)? {
                Some(hash) => traced.dirs.push((path.clone(), hash)),
                None => traced.files.push((path.clone(), None)),
            }
        }
        for name in &self.env {
            if name == ALL_ENV_NAME {
                let all = inputs.env_all();
                traced.env.push((ALL_ENV_NAME.into(), Some(trace::names_hash(&all).into())));
                traced.env.extend(all.into_iter().map(|(name, value)| (name, Some(value))));
            } else {
                traced.env.push((name.clone(), inputs.env(name)));
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
                part(&mut out, trace::os_bytes(path.as_os_str()));
            }
        }
        out.extend_from_slice(&(self.env.len() as u64).to_le_bytes());
        for name in &self.env {
            part(&mut out, trace::os_bytes(name));
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
            shape.files.push(PathBuf::from(os_from(at.part()?)));
        }
        for _ in 0..at.len()? {
            shape.dirs.push(PathBuf::from(os_from(at.part()?)));
        }
        for _ in 0..at.len()? {
            shape.env.push(os_from(at.part()?));
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

const SHAPE_MAGIC: &[u8; 16] = b"blaze v2 shape 1";

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
    /// A result whose every input is as it is now: what was read, and the value kept.
    Hit { traced: Traced, key: Key, value: &'s [u8] },
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

/// The result of `run` for `inputs` as they are now, if the store has one.
pub fn find<'s, I: Inputs>(
    store: &'s Store,
    run: &RunKey,
    inputs: &mut I,
) -> Result<Found<'s>, FindError<I::Error>> {
    let mut n = 0;
    loop {
        let key = slot(run, n);
        let Some(entry) = store.get(&key).map_err(FindError::Corrupt)? else {
            return Ok(Found::Miss { shapes: n });
        };
        let shape =
            Shape::decode(entry.bytes).map_err(|why| FindError::NotAShape { key, why })?;
        let traced = shape.values(inputs).map_err(FindError::Inputs)?;
        let key = result_key(run, &traced);
        if let Some(entry) = store.get(&key).map_err(FindError::Corrupt)? {
            return Ok(Found::Hit { traced, key, value: entry.bytes });
        }
        n += 1;
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
        match store.get(&at).map_err(KeepError::Corrupt)? {
            Some(entry) if entry.bytes == shape.as_slice() => return Ok(tickets),
            Some(_) => n += 1,
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

/// `path` made whole against `dir`, as a sandbox names what a process opened.
pub fn whole(dir: &Path, path: &Path) -> PathBuf {
    match path.is_absolute() {
        true => path.to_owned(),
        false => dir.join(path),
    }
}

/// The content ID of a shape, for an explanation of a rerun.
pub fn shape_id(shape: &Shape) -> ContentId {
    ContentId::of("blaze v2 shape id 1", &shape.encode())
}

#[cfg(test)]
mod tests {
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
    fn slots_are_per_run_and_per_number() {
        let run = RunKey { code: ContentId::of("blaze v2 test 1", b"a"), given: ContentId::default() };
        let other = RunKey { code: ContentId::of("blaze v2 test 1", b"b"), given: ContentId::default() };
        assert_ne!(slot(&run, 0), slot(&run, 1));
        assert_ne!(slot(&run, 0), slot(&other, 0));
    }
}
