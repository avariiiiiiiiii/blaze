//! Which CPUs share what (docs/v2/plan/lanes/core.md section 4, docs/v2/plan/lanes/core/plane.md "The executor"), for the
//! worker pool's groups and the order its workers steal in.
//!
//! A **group** is the CPUs that share a last-level cache (an EPYC's CCD: the level 3 cache's
//! `shared_cpu_list`) or, where the machine says so, a cluster (`cluster_cpus_list`, a
//! Graviton's); where both are said, the smaller wins. A cluster that is one core (x86 says its
//! SMT siblings are a cluster) says nothing. On macOS a group is an L2 cluster of one
//! performance level, and the CPUs of level 0 are `fast`. A machine that says nothing is one
//! group on one socket. Only online CPUs in the process's affinity mask are counted.
//!
//! Group ids are dense from 0 and stable for a machine: sorted by their first CPU.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs, io,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Cpu {
    /// The CPU's number: the kernel's on Linux (what affinity takes), the order of the
    /// performance levels on macOS.
    pub id: u32,
    pub group: u32,
    pub socket: u32,
    /// Of the fastest cores: Apple's performance level 0, or the largest `cpu_capacity` of an
    /// Arm big.LITTLE machine; every CPU where the machine says nothing.
    pub fast: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Topology {
    /// By id.
    pub cpus: Vec<Cpu>,
}

impl Topology {
    /// This machine's, as far as the process may use it.
    pub fn detect() -> Topology {
        detect()
    }

    /// `count` CPUs numbered from 0, one group on one socket: what a machine that says nothing
    /// is, and the shape of a pool whose threads are not bound to CPUs.
    pub fn flat(count: usize) -> Topology {
        let count = u32::try_from(count).unwrap_or(u32::MAX);
        Topology { cpus: (0..count).map(|id| Cpu { id, group: 0, socket: 0, fast: true }).collect() }
    }

    /// The topology `root` says, where `root` is the kernel's `/sys/devices/system/cpu` (or a
    /// tree laid out as it is, for tests). The affinity mask is not applied here.
    pub fn from_sysfs(root: &Path) -> io::Result<Topology> {
        let online = list(&read(&root.join("online"))?)
            .map_err(|why| at(&root.join("online"), why))?;
        let mut found = Vec::with_capacity(online.len());
        let mut capacities = BTreeMap::new();
        for &id in &online {
            let cpu = root.join(format!("cpu{id}"));
            let cache = last_level(&cpu)?.map(|it| &it & &online);
            let cluster = cluster(&cpu, id, &online)?;
            let mut share = match (cache, cluster) {
                (Some(cache), Some(cluster)) if cluster.len() < cache.len() => cluster,
                (Some(cache), _) => cache,
                (None, Some(cluster)) => cluster,
                (None, None) => online.clone(),
            };
            share.insert(id);
            let socket = match read_opt(&cpu.join("topology/physical_package_id"))? {
                Some(text) => text.trim().parse::<i64>().ok().and_then(|it| u32::try_from(it).ok()),
                None => None,
            };
            if let Some(text) = read_opt(&cpu.join("cpu_capacity"))?
                && let Ok(capacity) = text.trim().parse::<u64>()
            {
                capacities.insert(id, capacity);
            }
            found.push((id, share, socket.unwrap_or(0)));
        }
        let fastest = capacities.values().copied().max();
        let mut sets: BTreeMap<BTreeSet<u32>, u32> = BTreeMap::new();
        let mut cpus = Vec::with_capacity(found.len());
        for (id, share, socket) in found {
            let next = u32::try_from(sets.len()).unwrap_or(u32::MAX);
            let group = *sets.entry(share).or_insert(next);
            let fast = match (capacities.get(&id), fastest) {
                (Some(capacity), Some(fastest)) => *capacity == fastest,
                _ => true,
            };
            cpus.push(Cpu { id, group, socket, fast });
        }
        Ok(dense(cpus))
    }

    /// macOS's shape: for each performance level from the fastest, its CPUs and how many of
    /// them share an L2 (`hw.perflevel{N}.logicalcpu`, `hw.perflevel{N}.cpusperl2`).
    pub fn from_perflevels(levels: &[(u32, u32)]) -> Topology {
        let (mut cpus, mut id, mut first_group) = (Vec::new(), 0u32, 0u32);
        for (level, &(logical, per_l2)) in levels.iter().enumerate() {
            let per_l2 = if per_l2 == 0 { logical.max(1) } else { per_l2 };
            for at in 0..logical {
                cpus.push(Cpu { id, group: first_group + at / per_l2, socket: 0, fast: level == 0 });
                id += 1;
            }
            first_group += logical.div_ceil(per_l2);
        }
        dense(cpus)
    }

    /// Only the CPUs of `allowed`, their groups numbered again.
    pub fn only(&self, allowed: &BTreeSet<u32>) -> Topology {
        dense(self.cpus.iter().copied().filter(|it| allowed.contains(&it.id)).collect())
    }

    pub fn groups(&self) -> u32 {
        self.cpus.iter().map(|it| it.group + 1).max().unwrap_or(0)
    }

    /// The groups a worker of `group` takes work from, in order: its own, then the others of
    /// its socket nearest by id, then the rest nearest by id (ties to the lower). Empty for a
    /// group the topology does not have.
    pub fn steal_order(&self, group: u32) -> Vec<u32> {
        let mut socket_of: BTreeMap<u32, u32> = BTreeMap::new();
        for cpu in &self.cpus {
            socket_of.entry(cpu.group).or_insert(cpu.socket);
        }
        let Some(&home) = socket_of.get(&group) else { return Vec::new() };
        let mut rest: Vec<u32> = socket_of.keys().copied().filter(|&it| it != group).collect();
        rest.sort_by_key(|&it| (socket_of[&it] != home, it.abs_diff(group), it));
        std::iter::once(group).chain(rest).collect()
    }
}

/// `cpus` by id, their groups numbered from 0 in the order of their first CPU.
fn dense(mut cpus: Vec<Cpu>) -> Topology {
    cpus.sort_by_key(|it| it.id);
    let mut renamed: BTreeMap<u32, u32> = BTreeMap::new();
    for cpu in &mut cpus {
        let next = u32::try_from(renamed.len()).unwrap_or(u32::MAX);
        cpu.group = *renamed.entry(cpu.group).or_insert(next);
    }
    Topology { cpus }
}

#[cfg(target_os = "linux")]
fn detect() -> Topology {
    let allowed = linux::affinity();
    match (Topology::from_sysfs(Path::new("/sys/devices/system/cpu")), allowed) {
        (Ok(topology), Some(allowed)) => {
            let only = topology.only(&allowed);
            if only.cpus.is_empty() { topology } else { only }
        }
        (Ok(topology), None) => topology,
        (Err(_), Some(allowed)) => {
            dense(allowed.into_iter().map(|id| Cpu { id, group: 0, socket: 0, fast: true }).collect())
        }
        (Err(_), None) => Topology::flat(parallelism()),
    }
}

#[cfg(target_os = "macos")]
fn detect() -> Topology {
    let levels = macos::sysctl("hw.nperflevels").unwrap_or(0);
    let mut shape = Vec::new();
    for level in 0..levels {
        let Some(logical) = macos::sysctl(&format!("hw.perflevel{level}.logicalcpu")) else {
            break;
        };
        let per_l2 = macos::sysctl(&format!("hw.perflevel{level}.cpusperl2")).unwrap_or(0);
        shape.push((logical, per_l2));
    }
    if shape.iter().all(|&(logical, _)| logical == 0) {
        let logical = macos::sysctl("hw.logicalcpu").map_or_else(parallelism, |it| it as usize);
        return Topology::flat(logical);
    }
    Topology::from_perflevels(&shape)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn detect() -> Topology {
    Topology::flat(parallelism())
}

fn parallelism() -> usize {
    std::thread::available_parallelism().map_or(1, std::num::NonZero::get)
}

/// The level 3 cache's CPUs, where the CPU says.
fn last_level(cpu: &Path) -> io::Result<Option<BTreeSet<u32>>> {
    let dir = cpu.join("cache");
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(why) if why.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(why) => return Err(at(&dir, why)),
    };
    let mut indexes: Vec<PathBuf> = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|why| at(&dir, why))?;
        if entry.file_name().to_string_lossy().starts_with("index") {
            indexes.push(entry.path());
        }
    }
    indexes.sort();
    for index in indexes {
        if read_opt(&index.join("level"))?.is_some_and(|it| it.trim() == "3")
            && let Some(text) = read_opt(&index.join("shared_cpu_list"))?
        {
            let path = index.join("shared_cpu_list");
            return list(&text).map(Some).map_err(|why| at(&path, why));
        }
    }
    Ok(None)
}

/// The CPU's cluster, where it is more than the CPU's core.
fn cluster(cpu: &Path, id: u32, online: &BTreeSet<u32>) -> io::Result<Option<BTreeSet<u32>>> {
    let path = cpu.join("topology/cluster_cpus_list");
    let Some(text) = read_opt(&path)? else { return Ok(None) };
    let cluster = &list(&text).map_err(|why| at(&path, why))? & online;
    let mut core = BTreeSet::from([id]);
    for name in ["topology/core_cpus_list", "topology/thread_siblings_list"] {
        let path = cpu.join(name);
        if let Some(text) = read_opt(&path)? {
            core.extend(&list(&text).map_err(|why| at(&path, why))? & online);
            break;
        }
    }
    Ok((cluster.len() > core.len()).then_some(cluster))
}

/// A kernel CPU list: `0-3,8,10-11`.
fn list(text: &str) -> io::Result<BTreeSet<u32>> {
    let number = |it: &str| {
        it.trim().parse::<u32>().map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidData, format!("{it:?} is not a CPU number"))
        })
    };
    let mut cpus = BTreeSet::new();
    for part in text.trim().split(',').filter(|it| !it.trim().is_empty()) {
        let (from, to) = match part.split_once('-') {
            Some((from, to)) => (number(from)?, number(to)?),
            None => (number(part)?, number(part)?),
        };
        if from > to {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{part:?} is a range that ends before it begins"),
            ));
        }
        cpus.extend(from..=to);
    }
    Ok(cpus)
}

fn read(path: &Path) -> io::Result<String> {
    fs::read_to_string(path).map_err(|why| at(path, why))
}

fn read_opt(path: &Path) -> io::Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(why) if why.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(why) => Err(at(path, why)),
    }
}

fn at(path: &Path, why: io::Error) -> io::Error {
    io::Error::new(why.kind(), format!("{}: {why}", path.display()))
}

#[cfg(target_os = "linux")]
pub(crate) mod linux {
    use std::{collections::BTreeSet, io, mem::size_of};

    const BITS: usize = 8 * size_of::<libc::c_ulong>();

    /// The CPUs the process may run on, or `None` where the kernel does not say.
    pub(crate) fn affinity() -> Option<BTreeSet<u32>> {
        let mut words = 1024 / BITS;
        loop {
            let mut mask: Vec<libc::c_ulong> = vec![0; words];
            // SAFETY: the kernel writes at most `words * size_of::<c_ulong>()` bytes of the mask,
            // which is what `mask` holds; a `cpu_set_t` is such words.
            let got = unsafe {
                libc::sched_getaffinity(
                    0,
                    words * size_of::<libc::c_ulong>(),
                    mask.as_mut_ptr().cast::<libc::cpu_set_t>(),
                )
            };
            if got == 0 {
                let mut cpus = BTreeSet::new();
                for (at, word) in mask.iter().enumerate() {
                    for bit in 0..BITS {
                        if word & (1 << bit) != 0 {
                            cpus.insert(u32::try_from(at * BITS + bit).ok()?);
                        }
                    }
                }
                return Some(cpus);
            }
            // EINVAL: the mask is smaller than the kernel's.
            if io::Error::last_os_error().raw_os_error() == Some(libc::EINVAL) && words < 1 << 16 {
                words *= 2;
                continue;
            }
            return None;
        }
    }

    /// Binds the calling thread to `cpu`.
    pub(crate) fn bind(cpu: u32) -> io::Result<()> {
        let cpu = cpu as usize;
        let mut mask: Vec<libc::c_ulong> = vec![0; (cpu / BITS + 1).max(1024 / BITS)];
        mask[cpu / BITS] |= 1 << (cpu % BITS);
        // SAFETY: the kernel reads `mask.len() * size_of::<c_ulong>()` bytes, which `mask` holds.
        let set = unsafe {
            libc::sched_setaffinity(
                0,
                mask.len() * size_of::<libc::c_ulong>(),
                mask.as_ptr().cast::<libc::cpu_set_t>(),
            )
        };
        if set == 0 {
            Ok(())
        } else {
            let why = io::Error::last_os_error();
            Err(io::Error::new(why.kind(), format!("binding a worker to CPU {cpu}: {why}")))
        }
    }
}

#[cfg(target_os = "macos")]
pub(crate) mod macos {
    use std::{ffi::CString, mem::size_of, ptr};

    /// A 32-bit integer the kernel says by `name`, if it says it.
    pub(crate) fn sysctl(name: &str) -> Option<u32> {
        let name = CString::new(name).ok()?;
        let mut value: i32 = 0;
        let mut len = size_of::<i32>();
        // SAFETY: `value` has room for the `len` bytes the kernel may write, and `name` is a C
        // string that outlives the call.
        let got = unsafe {
            libc::sysctlbyname(
                name.as_ptr(),
                (&raw mut value).cast::<libc::c_void>(),
                &mut len,
                ptr::null_mut(),
                0,
            )
        };
        (got == 0 && len == size_of::<i32>()).then(|| u32::try_from(value).ok()).flatten()
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeSet, fs, path::PathBuf};

    use super::{Cpu, Topology};

    /// A fake `/sys/devices/system/cpu`, removed when dropped.
    struct Tree(PathBuf);

    impl Tree {
        fn new(name: &str) -> Tree {
            let dir = std::env::temp_dir()
                .join(format!("blaze-topology-{}-{name}", std::process::id()));
            _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Tree(dir)
        }

        fn file(&self, path: &str, text: &str) {
            let path = self.0.join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, format!("{text}\n")).unwrap();
        }
    }

    impl Drop for Tree {
        fn drop(&mut self) {
            _ = fs::remove_dir_all(&self.0);
        }
    }

    fn groups(topology: &Topology) -> Vec<u32> {
        topology.cpus.iter().map(|it| it.group).collect()
    }

    /// Two sockets of four CCDs of eight cores, each core two threads (`c` and `c + 64`). The
    /// SMT siblings say they are a cluster, which is one core: it says nothing.
    #[test]
    fn an_epyc_groups_by_ccd_and_steals_from_its_socket_first() {
        let tree = Tree::new("epyc");
        tree.file("online", "0-127");
        for cpu in 0..128u32 {
            let core = cpu % 64;
            let ccd = core / 8;
            let siblings = format!("{core},{}", core + 64);
            let at = format!("cpu{cpu}");
            tree.file(&format!("{at}/cache/index0/level"), "1");
            tree.file(&format!("{at}/cache/index0/shared_cpu_list"), &siblings);
            tree.file(&format!("{at}/cache/index2/level"), "2");
            tree.file(&format!("{at}/cache/index2/shared_cpu_list"), &siblings);
            tree.file(&format!("{at}/cache/index3/level"), "3");
            tree.file(
                &format!("{at}/cache/index3/shared_cpu_list"),
                &format!("{}-{},{}-{}", ccd * 8, ccd * 8 + 7, 64 + ccd * 8, 64 + ccd * 8 + 7),
            );
            tree.file(&format!("{at}/topology/physical_package_id"), &(ccd / 4).to_string());
            tree.file(&format!("{at}/topology/cluster_cpus_list"), &siblings);
            tree.file(&format!("{at}/topology/core_cpus_list"), &siblings);
        }
        let topology = Topology::from_sysfs(&tree.0).unwrap();
        assert_eq!(topology.cpus.len(), 128);
        assert_eq!(topology.groups(), 8);
        for cpu in &topology.cpus {
            let ccd = (cpu.id % 64) / 8;
            assert_eq!((cpu.group, cpu.socket, cpu.fast), (ccd, ccd / 4, true), "{cpu:?}");
        }
        assert_eq!(topology.steal_order(0), [0, 1, 2, 3, 4, 5, 6, 7]);
        assert_eq!(topology.steal_order(5), [5, 4, 6, 7, 3, 2, 1, 0]);
        assert_eq!(topology.steal_order(8), Vec::<u32>::new());
    }

    /// One L3 for the whole chip, clusters of four cores: the cluster is smaller, so it wins.
    #[test]
    fn a_graviton_groups_by_cluster() {
        let tree = Tree::new("graviton");
        tree.file("online", "0-63");
        for cpu in 0..64u32 {
            let at = format!("cpu{cpu}");
            let first = cpu / 4 * 4;
            tree.file(&format!("{at}/cache/index3/level"), "3");
            tree.file(&format!("{at}/cache/index3/shared_cpu_list"), "0-63");
            tree.file(&format!("{at}/topology/physical_package_id"), "0");
            tree.file(
                &format!("{at}/topology/cluster_cpus_list"),
                &format!("{first}-{}", first + 3),
            );
            tree.file(&format!("{at}/topology/core_cpus_list"), &cpu.to_string());
        }
        let topology = Topology::from_sysfs(&tree.0).unwrap();
        assert_eq!(topology.groups(), 16);
        assert!(topology.cpus.iter().all(|it| it.group == it.id / 4 && it.socket == 0));
        assert_eq!(topology.steal_order(3)[..7], [3, 2, 4, 1, 5, 0, 6]);
    }

    #[test]
    fn a_machine_that_says_nothing_is_one_group() {
        let tree = Tree::new("silent");
        tree.file("online", "0-3");
        let topology = Topology::from_sysfs(&tree.0).unwrap();
        assert_eq!(topology, Topology::flat(4));
        assert_eq!(topology.steal_order(0), [0]);
    }

    #[test]
    fn offline_cpus_and_the_affinity_mask_are_left_out() {
        let tree = Tree::new("offline");
        tree.file("online", "0-5,7");
        for cpu in 0..8u32 {
            let at = format!("cpu{cpu}");
            tree.file(&format!("{at}/cache/index3/level"), "3");
            tree.file(
                &format!("{at}/cache/index3/shared_cpu_list"),
                if cpu < 4 { "0-3" } else { "4-7" },
            );
        }
        let topology = Topology::from_sysfs(&tree.0).unwrap();
        let ids: Vec<u32> = topology.cpus.iter().map(|it| it.id).collect();
        assert_eq!(ids, [0, 1, 2, 3, 4, 5, 7]);
        assert_eq!(groups(&topology), [0, 0, 0, 0, 1, 1, 1]);
        let only = topology.only(&BTreeSet::from([4, 5, 7]));
        assert_eq!(groups(&only), [0, 0, 0]);
        let only = topology.only(&BTreeSet::from([0, 5]));
        assert_eq!(groups(&only), [0, 1]);
        tree.file("online", "0-3,x");
        assert!(Topology::from_sysfs(&tree.0).is_err());
    }

    /// Arm big.LITTLE: the cores of the largest capacity are the fast ones.
    #[test]
    fn the_largest_capacity_is_fast() {
        let tree = Tree::new("biglittle");
        tree.file("online", "0-7");
        for cpu in 0..8u32 {
            let at = format!("cpu{cpu}");
            tree.file(&format!("{at}/cpu_capacity"), if cpu < 4 { "446" } else { "1024" });
            tree.file(
                &format!("{at}/topology/cluster_cpus_list"),
                if cpu < 4 { "0-3" } else { "4-7" },
            );
        }
        let topology = Topology::from_sysfs(&tree.0).unwrap();
        let fast: Vec<bool> = topology.cpus.iter().map(|it| it.fast).collect();
        assert_eq!(fast, [false, false, false, false, true, true, true, true]);
        assert_eq!(groups(&topology), [0, 0, 0, 0, 1, 1, 1, 1]);
    }

    /// An M-series Max: twelve performance cores in L2 clusters of six, four efficiency cores
    /// sharing one.
    #[test]
    fn apple_groups_by_l2_cluster_and_level_zero_is_fast() {
        let topology = Topology::from_perflevels(&[(12, 6), (4, 4)]);
        assert_eq!(topology.cpus.len(), 16);
        assert_eq!(groups(&topology), [0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 1, 1, 2, 2, 2, 2]);
        assert!(topology.cpus.iter().all(|it| it.fast == (it.id < 12)));
        assert_eq!(topology.steal_order(2), [2, 1, 0]);
        assert_eq!(
            Topology::from_perflevels(&[(2, 0)]).cpus,
            [
                Cpu { id: 0, group: 0, socket: 0, fast: true },
                Cpu { id: 1, group: 0, socket: 0, fast: true }
            ]
        );
    }

    #[test]
    fn detect_finds_at_least_one_cpu() {
        let topology = Topology::detect();
        assert!(!topology.cpus.is_empty());
        let groups = topology.groups();
        assert!(topology.cpus.iter().all(|it| it.group < groups));
    }
}
