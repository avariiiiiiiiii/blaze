//! What a watcher last saw of the disk (docs/v2/plan/design/restore.md): per head, the watched
//! directory, the cursor into the system's journal of changes, and each path's [`Stamp`] (its
//! inode, size, mtime and ctime) when the head took what was there. Saved with the tree in the
//! daemon's image, so a restart asks the journal for what changed since the cursor and reads only
//! paths whose stamp moved: no scan, and no file hashed that did not change.
//!
//! The column of a restored image is read in place ([`Base`]: sorted names, their ends and their
//! stamps, decoded field by field as they are read); what changes after goes into an overlay,
//! and the next image merges both into one sorted column again.

use blaze_core::{Cursor, FileName, Names, Stamp};
use blaze_ids::ContentId;
use rustc_hash::FxHashMap;

use crate::section::Section;

const MAGIC: [u8; 4] = *b"BZD1";

/// A restored column: `count` names sorted bytewise, each with its end in `names` and its stamp.
struct Base {
    section: Section,
    count: usize,
    /// Offsets in `section` of the ends, the stamps and the names.
    ends: usize,
    stamps: usize,
    names: usize,
}

impl Base {
    fn word(&self, at: usize) -> usize {
        let bytes = self.section.bytes();
        u32::from_le_bytes(bytes[at..at + 4].try_into().expect("checked at open")) as usize
    }

    fn name(&self, at: usize) -> &str {
        let start = if at == 0 { 0 } else { self.word(self.ends + (at - 1) * 4) };
        let end = self.word(self.ends + at * 4);
        let bytes = &self.section.bytes()[self.names + start..self.names + end];
        std::str::from_utf8(bytes).expect("checked at open")
    }

    fn stamp(&self, at: usize) -> Stamp {
        Stamp::decode(&self.section.bytes()[self.stamps + at * Stamp::BYTES..])
            .expect("checked at open")
    }

    fn find(&self, name: &str) -> Result<usize, usize> {
        let (mut low, mut high) = (0, self.count);
        while low < high {
            let mid = (low + high) / 2;
            match self.name(mid).cmp(name) {
                std::cmp::Ordering::Less => low = mid + 1,
                std::cmp::Ordering::Greater => high = mid,
                std::cmp::Ordering::Equal => return Ok(mid),
            }
        }
        Err(low)
    }
}

/// One head's watched directory as last seen.
pub(crate) struct Disk {
    pub(crate) dir: FileName,
    pub(crate) cursor: Cursor,
    /// The head's root when the column last held all of it: a rescan takes from the head only
    /// what changed since (its diff), not every file. `None`: the head is looked at whole.
    pub(crate) root: Option<ContentId>,
    base: Option<Base>,
    /// Paths changed since `base`, by name under the driver's root: [`Stamp::GONE`] for one gone.
    over: FxHashMap<Box<str>, Stamp>,
}

impl Disk {
    pub(crate) fn new(dir: FileName) -> Disk {
        Disk { dir, cursor: Cursor::None, root: None, base: None, over: FxHashMap::default() }
    }

    /// What `name` was when last seen; `None` for a name not seen or gone.
    pub(crate) fn get(&self, name: &str) -> Option<Stamp> {
        let stamp = match self.over.get(name) {
            Some(stamp) => *stamp,
            None => {
                let base = self.base.as_ref()?;
                base.stamp(base.find(name).ok()?)
            }
        };
        (!stamp.is_gone()).then_some(stamp)
    }

    pub(crate) fn set(&mut self, name: &str, stamp: Stamp) {
        match self.over.get_mut(name) {
            Some(it) => *it = stamp,
            None => _ = self.over.insert(name.into(), stamp),
        }
    }

    /// `name` and every name under it gone.
    pub(crate) fn remove_under(&mut self, name: &str) {
        let mut gone = Vec::new();
        self.under(name, |it, _| gone.push(Box::<str>::from(it)));
        for it in gone {
            self.over.insert(it, Stamp::GONE);
        }
    }

    /// Calls `f` with every name seen at or under `name` and its stamp, in name order, merging
    /// the column and what changed since.
    pub(crate) fn under(&self, name: &str, mut f: impl FnMut(&str, Stamp)) {
        let within = |it: &str| {
            it.strip_prefix(name).is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
        };
        let mut changed: Vec<(&str, Stamp)> =
            self.over.iter().filter(|(it, _)| within(it)).map(|(it, stamp)| (&**it, *stamp)).collect();
        changed.sort_unstable_by(|a, b| a.0.cmp(b.0));
        let mut changed = changed.into_iter().peekable();
        if let Some(base) = &self.base {
            let mut at = base.find(name).unwrap_or_else(|it| it);
            while at < base.count {
                let it = base.name(at);
                // `a/b` < `a/b-c` < `a/b/c`: names under `name` need not follow it at once.
                if !within(it) {
                    if it.len() > name.len() && it.as_bytes()[..name.len()] == *name.as_bytes() {
                        at += 1;
                        continue;
                    }
                    break;
                }
                while let Some(&(over, stamp)) = changed.peek().filter(|(over, _)| *over < it) {
                    if !stamp.is_gone() {
                        f(over, stamp);
                    }
                    changed.next();
                }
                match changed.peek() {
                    Some(&(over, stamp)) if over == it => {
                        if !stamp.is_gone() {
                            f(over, stamp);
                        }
                        changed.next();
                    }
                    _ => f(it, base.stamp(at)),
                }
                at += 1;
            }
        }
        for (over, stamp) in changed {
            if !stamp.is_gone() {
                f(over, stamp);
            }
        }
    }

    /// Calls `f` with every name seen directly in `dir` and its stamp, in no set order. The
    /// column's subtree of each child is skipped by a search, not walked.
    pub(crate) fn children(&self, dir: &str, mut f: impl FnMut(&str, Stamp)) {
        let direct = |it: &str| {
            it.strip_prefix(dir)
                .and_then(|rest| rest.strip_prefix('/'))
                .is_some_and(|rest| !rest.is_empty() && !rest.contains('/'))
        };
        for (it, stamp) in &self.over {
            if direct(it) && !stamp.is_gone() {
                f(it, *stamp);
            }
        }
        let Some(base) = &self.base else { return };
        let prefix = format!("{dir}/");
        let mut skip = String::with_capacity(prefix.len() + 32);
        let mut at = base.find(&prefix).unwrap_or_else(|it| it);
        while at < base.count {
            let it = base.name(at);
            let Some(rest) = it.strip_prefix(prefix.as_str()) else { break };
            match rest.find('/') {
                None => {
                    if !self.over.contains_key(it) {
                        f(it, base.stamp(at));
                    }
                    at += 1;
                }
                // Everything under `prefix + child + "/"` sorts before `prefix + child + "0"`.
                Some(slash) => {
                    skip.clear();
                    skip.push_str(&it[..prefix.len() + slash]);
                    skip.push('0');
                    at = base.find(&skip).unwrap_or_else(|it| it);
                }
            }
        }
    }

    /// Every name seen at or under `name` into `names`, in name order, and its stamp into
    /// `stamps`. Every name the column holds was checked as it was opened or set.
    pub(crate) fn sweep(&self, name: &str, names: &mut Names, stamps: &mut Vec<Stamp>) {
        self.under(name, |it, stamp| {
            names.push_checked(it);
            stamps.push(stamp);
        });
    }
}

/// Every watched head's [`Disk`].
#[derive(Default)]
pub(crate) struct Disks {
    pub(crate) by_head: FxHashMap<Box<str>, Disk>,
}

fn put(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(&u32::try_from(bytes.len()).expect("under 4 GiB").to_le_bytes());
    out.extend_from_slice(bytes);
}

impl Disks {
    /// The disk of `head` watching `dir`: kept if it watched `dir`, else a new one.
    pub(crate) fn of(&mut self, head: &str, dir: &FileName) -> &mut Disk {
        let disk = self.by_head.entry(head.into()).or_insert_with(|| Disk::new(dir.clone()));
        if disk.dir != *dir {
            *disk = Disk::new(dir.clone());
        }
        disk
    }

    pub(crate) fn get(&self, head: &str) -> Option<&Disk> {
        self.by_head.get(head)
    }

    /// The image's bytes of every disk: `MAGIC`, a count, then per disk (heads in name order)
    /// its head, its directory, its cursor, its root (a byte saying whether, then 32), its count
    /// of names, its names' length, each name's end, each stamp, the names.
    pub(crate) fn encode(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&MAGIC);
        let mut heads: Vec<&Box<str>> = self.by_head.keys().collect();
        heads.sort_unstable();
        out.extend_from_slice(&(heads.len() as u32).to_le_bytes());
        let (mut ends, mut stamps, mut names) = (Vec::new(), Vec::new(), Vec::new());
        for head in heads {
            let disk = &self.by_head[head];
            put(out, head.as_bytes());
            put(out, disk.dir.as_str().as_bytes());
            disk.cursor.encode(out);
            out.push(u8::from(disk.root.is_some()));
            out.extend_from_slice(&disk.root.unwrap_or_default().0);
            (ends.clear(), stamps.clear(), names.clear());
            let mut count = 0u32;
            disk.under(disk.dir.as_str(), |name, stamp| {
                names.extend_from_slice(name.as_bytes());
                ends.extend_from_slice(&(names.len() as u32).to_le_bytes());
                stamp.encode(&mut stamps);
                count += 1;
            });
            out.extend_from_slice(&count.to_le_bytes());
            out.extend_from_slice(&u32::try_from(names.len()).expect("under 4 GiB").to_le_bytes());
            out.extend_from_slice(&ends);
            out.extend_from_slice(&stamps);
            out.extend_from_slice(&names);
        }
    }

    /// The disks of an image's section, read in place: every length, end and name is checked
    /// once here (bounds, order, UTF-8), so reads after never fail.
    pub(crate) fn open(section: Section) -> Result<Disks, String> {
        let bytes = section.bytes();
        let bad = |what: &str| format!("the VFS image's disks section: {what}");
        if bytes.get(..4) != Some(&MAGIC[..]) {
            return Err(bad("no magic"));
        }
        let mut at = 4;
        let word = |at: &mut usize| -> Result<usize, String> {
            let word = bytes.get(*at..*at + 4).ok_or_else(|| bad("cut short"))?;
            *at += 4;
            Ok(u32::from_le_bytes(word.try_into().expect("4 bytes")) as usize)
        };
        let text = |at: &mut usize| -> Result<&str, String> {
            let len = word(at)?;
            let text = bytes.get(*at..*at + len).ok_or_else(|| bad("cut short"))?;
            *at += len;
            std::str::from_utf8(text).map_err(|_| bad("a name that is not UTF-8"))
        };
        let mut disks = Disks::default();
        for _ in 0..word(&mut at)? {
            let head = text(&mut at)?;
            let dir = FileName::new(text(&mut at)?).ok_or_else(|| bad("a directory that is no name"))?;
            let cursor = bytes.get(at..).and_then(Cursor::decode).ok_or_else(|| bad("a bad cursor"))?;
            at += Cursor::BYTES;
            let root = match bytes.get(at..at + 33).ok_or_else(|| bad("cut short"))? {
                [0, ..] => None,
                [1, id @ ..] => Some(ContentId(id.try_into().expect("32 bytes"))),
                _ => return Err(bad("a bad root")),
            };
            at += 33;
            let count = word(&mut at)?;
            let names_len = word(&mut at)?;
            let (ends, stamps) = (at, at + count * 4);
            let names = stamps + count * Stamp::BYTES;
            let end = names + names_len;
            if end > bytes.len() {
                return Err(bad("cut short"));
            }
            let base = Base { section: section.clone(), count, ends, stamps, names };
            let mut last = 0;
            for n in 0..count {
                let stop = base.word(ends + n * 4);
                if stop < last || stop > names_len {
                    return Err(bad("a name's end out of order"));
                }
                let name = std::str::from_utf8(&bytes[names + last..names + stop])
                    .map_err(|_| bad("a name that is not UTF-8"))?;
                if FileName::new(name).is_none_or(|it| !it.is_under(&dir))
                    || (n > 0 && base.name(n - 1) >= name)
                {
                    return Err(bad("names out of order or outside the directory"));
                }
                last = stop;
            }
            at = end;
            let disk = Disk { dir, cursor, root, base: Some(base), over: FxHashMap::default() };
            disks.by_head.insert(head.into(), disk);
        }
        Ok(disks)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(it: &str) -> FileName {
        FileName::new(it).unwrap()
    }

    fn stamp(n: u64) -> Stamp {
        Stamp { ino: n, size: n, mtime_ns: n, ctime_ns: n, mode: blaze_core::S_IFREG | 0o644 }
    }

    fn reopen(disks: &Disks) -> Disks {
        let mut bytes = Vec::new();
        disks.encode(&mut bytes);
        let len = bytes.len();
        let map = blaze_core::Mapping(std::sync::Arc::new(bytes));
        Disks::open(Section::new(map, 0..len).unwrap()).unwrap()
    }

    fn all(disk: &Disk, under: &str) -> Vec<(String, u64)> {
        let mut out = Vec::new();
        disk.under(under, |it, stamp| out.push((it.to_owned(), stamp.ino)));
        out
    }

    #[test]
    fn a_column_reopens_in_place_and_merges_what_changed_after() {
        let mut disks = Disks::default();
        let disk = disks.of("main", &name("w"));
        disk.cursor = Cursor::Sim { journal: 1, next: 9 };
        disk.root = Some(ContentId([7; 32]));
        for (n, it) in ["w/a", "w/a/x.rs", "w/a-b", "w/b.rs", "w/z/y"].into_iter().enumerate() {
            disk.set(it, stamp(n as u64 + 1));
        }
        disks.of("other", &name("o")).set("o/f", stamp(9));
        let mut disks = reopen(&disks);
        assert_eq!(disks.by_head["other"].root, None);
        let disk = disks.by_head.get_mut("main").unwrap();
        assert_eq!(disk.cursor, Cursor::Sim { journal: 1, next: 9 });
        assert_eq!(disk.root, Some(ContentId([7; 32])));
        assert_eq!(disk.get("w/b.rs"), Some(stamp(4)));
        assert_eq!(all(disk, "w/a"), [("w/a".into(), 1), ("w/a/x.rs".into(), 2)]);
        disk.set("w/a/n.rs", stamp(7));
        disk.set("w/b.rs", stamp(8));
        disk.remove_under("w/z");
        assert_eq!(disk.get("w/z/y"), None);
        let names: Vec<(String, u64)> = all(disk, "w");
        let want = [("w/a", 1), ("w/a-b", 3), ("w/a/n.rs", 7), ("w/a/x.rs", 2), ("w/b.rs", 8)];
        assert_eq!(names, want.map(|(it, n)| (it.to_owned(), n)));
        let disks = reopen(&disks);
        assert_eq!(all(&disks.by_head["main"], "w"), want.map(|(it, n)| (it.to_owned(), n)));
    }

    #[test]
    fn children_are_the_names_directly_in_a_directory() {
        let mut disks = Disks::default();
        let disk = disks.of("main", &name("w"));
        let names = ["w/a", "w/a/x/deep.rs", "w/a/y.rs", "w/a-b", "w/a.c", "w/a0", "w/b/c", "w/z"];
        for (n, it) in names.into_iter().enumerate() {
            disk.set(it, stamp(n as u64 + 1));
        }
        let children = |disk: &Disk, dir: &str| {
            let mut out = Vec::new();
            disk.children(dir, |it, stamp| out.push((it.to_owned(), stamp.ino)));
            out.sort_unstable();
            out
        };
        let want = |names: &[(&str, u64)]| -> Vec<(String, u64)> {
            names.iter().map(|&(it, n)| (it.to_owned(), n)).collect()
        };
        let reopened = reopen(&disks);
        for disk in [&disks.by_head["main"], &reopened.by_head["main"]] {
            assert_eq!(children(disk, "w/a"), want(&[("w/a/y.rs", 3)]));
            assert_eq!(
                children(disk, "w"),
                want(&[("w/a", 1), ("w/a-b", 4), ("w/a.c", 5), ("w/a0", 6), ("w/z", 8)])
            );
        }
        // What changed since the column is merged: a new name, and one gone.
        let mut disk = reopen(&disks).by_head.remove("main").unwrap();
        disk.set("w/new", stamp(9));
        disk.remove_under("w/a0");
        assert_eq!(
            children(&disk, "w"),
            want(&[("w/a", 1), ("w/a-b", 4), ("w/a.c", 5), ("w/new", 9), ("w/z", 8)])
        );
    }

    #[test]
    fn a_bad_section_is_refused_naming_why() {
        let mut disks = Disks::default();
        disks.of("main", &name("w")).set("w/a", stamp(1));
        let mut bytes = Vec::new();
        disks.encode(&mut bytes);
        for cut in 0..bytes.len() {
            let map = blaze_core::Mapping(std::sync::Arc::new(bytes[..cut].to_vec()));
            assert!(Disks::open(Section::new(map, 0..cut).unwrap()).is_err(), "cut at {cut}");
        }
    }
}
