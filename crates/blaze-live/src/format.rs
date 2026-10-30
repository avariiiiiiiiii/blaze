//! What the daemon and a running program say to each other, through two files in a
//! directory named after the executable's `LC_UUID` ([`dir`]):
//!
//! - `process`, which the program writes: who it is, which executable it runs, and where
//!   in its memory the next edit goes.
//! - `patch`, which the daemon writes: the code of the functions that differ from the
//!   executable, laid out for that place, or why the program has to be started again.
//! - `wake`, a datagram socket the program listens on: the daemon sends it a byte each time
//!   it has written `patch`, and the program reads the patch at once.
//!
//! And three for tiering, where the daemon compiles the functions the program spends its
//! time in again with LLVM while it runs:
//!
//! - `sample`, which the daemon writes to ask the program to sample where its threads are
//!   ([`Sample`]), and removes to have it stop. Without it the program samples nothing.
//! - `profile`, which the program writes a few times a second while it samples: where its
//!   threads were, and the counters of the code that counts its branches ([`Profile`]).
//! - `tiers`, which the daemon writes: which functions are hot and at which level, and what
//!   it did of late ([`Tiers`]), for the program to show.
//!
//! Every address here is as the executable's load commands have it, before the loader
//! slid the program: the program adds its slide.

use std::path::{Path, PathBuf};

const MAGIC: &[u8; 8] = b"BLAZELV1";

/// The socket in [`dir`] that the daemon wakes the program on once it wrote a patch.
pub const WAKE: &str = "wake";

/// The event the daemon sets on Windows once it wrote a patch in `dir`, where it has no socket
/// to wake the program on: named after the directory, which is named after the executable.
pub fn wake_event(dir: &Path) -> String {
    // alloc: setup, once when the program starts and once per patch the daemon writes
    format!("Local\\blaze-live-{}", dir.file_name().unwrap_or_default().to_string_lossy())
}

/// The directory that holds one for each executable that runs: in the daemon's home, which
/// is in `USERPROFILE` on Windows when nothing says `HOME`.
pub fn root() -> Option<PathBuf> {
    let set = |name| std::env::var_os(name).filter(|it| !it.is_empty());
    let home = set("HOME").or_else(|| set("USERPROFILE"));
    let blaze_home = crate::setting("HOME").filter(|it| !it.is_empty());
    let home = match (blaze_home, set("XDG_RUNTIME_DIR"), home) {
        (Some(home), _, _) => PathBuf::from(home),
        (None, Some(runtime), _) => Path::new(&runtime).join("blaze"),
        (None, None, Some(home)) => Path::new(&home).join(".cache/blaze"),
        (None, None, None) => return None,
    };
    Some(home.join("live"))
}

/// The directory that the edits of the executable with `uuid` go through. It is named
/// after what the executable is, not where: cargo links a program in one place and leaves
/// a hard link of it in another, and a program may be copied and run from anywhere.
pub fn dir(uuid: &[u8; 16]) -> Option<PathBuf> {
    Some(root()?.join(hex(uuid)))
}

/// Text on one line, as an edit goes to the daemon: a backslash, a newline, a tab and a
/// carriage return are written as `\\`, `\n`, `\t` and `\r`.
pub fn escape(text: &str, line: &mut String) {
    for c in text.chars() {
        match c {
            '\\' => line.push_str("\\\\"),
            '\n' => line.push_str("\\n"),
            '\t' => line.push_str("\\t"),
            '\r' => line.push_str("\\r"),
            c => line.push(c),
        }
    }
}

/// What [`escape`] wrote.
pub fn unescape(line: &str) -> String {
    let mut text = String::with_capacity(line.len());
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            text.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => text.push('\n'),
            Some('t') => text.push('\t'),
            Some('r') => text.push('\r'),
            Some(other) => text.push(other),
            None => text.push('\\'),
        }
    }
    text
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(2 * bytes.len());
    push_hex(&mut out, bytes);
    out
}

/// `bytes` as lowercase hex, two digits each, appended to `out`.
fn push_hex(out: &mut String, bytes: &[u8]) {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    for &byte in bytes {
        out.push(char::from(DIGITS[usize::from(byte >> 4)]));
        out.push(char::from(DIGITS[usize::from(byte & 0xf)]));
    }
}

/// What the daemon has for a running program.
#[derive(Debug, PartialEq)]
pub struct Patch {
    /// The `LC_UUID` of the executable it is for.
    pub uuid: [u8; 16],
    /// Counts up with each patch the daemon writes for the executable.
    pub sequence: u64,
    pub body: Body,
}

#[derive(Debug, PartialEq)]
pub enum Body {
    Code(Code),
    /// The edit has no answer but a new executable, for this reason.
    Restart(String),
}

/// The functions that differ from the executable, and how to make it use them.
#[derive(Debug, PartialEq, Default)]
pub struct Code {
    /// Where the code goes: a multiple of the page size.
    pub base: u64,
    /// Read and run, from `base`. A multiple of the page size long.
    pub text: Vec<u8>,
    /// Read and written, behind `text`.
    pub data: Vec<u8>,
    /// Where `text` has the unwind tables of the code (`__eh_frame`), and how long.
    pub frames: (u64, u64),
    /// Where `text` and `data` store an address, as offsets from `base`.
    pub pointers: Vec<u64>,
    /// What changes in the executable: a jump over the entry of each function replaced,
    /// and the value of each static that has another.
    pub writes: Vec<Write>,
    /// How many of `writes` are jumps, and how many after them are statics, for the log.
    /// The rest put back what the executable had, where an earlier edit wrote.
    pub redirected: u64,
    pub statics: u64,
    /// The first value of each static new since the executable that the code refers to, in
    /// the room the program set aside: the program writes one the first time a patch has it
    /// and never again, so it keeps what was written to it across the patches after, which
    /// all refer to it there. Each patch has all it refers to: one the program did not take
    /// (an older one the newest was written over) is nothing to the next.
    pub lasting: Vec<Write>,
}

#[derive(Debug, PartialEq)]
pub struct Write {
    pub address: u64,
    pub bytes: Vec<u8>,
    /// Where `bytes` stores an address, as offsets into it.
    pub pointers: Vec<u64>,
}

impl Patch {
    /// The size of its encoding.
    pub fn encoded_len(&self) -> usize {
        let writes = |writes: &[Write]| -> usize {
            writes.iter().map(|it| 8 + 8 + it.bytes.len() + 8 + 8 * it.pointers.len()).sum()
        };
        let body = match &self.body {
            Body::Restart(why) => 8 + why.len(),
            Body::Code(code) => {
                8 + 8
                    + code.text.len()
                    + 8
                    + code.data.len()
                    + 16
                    + 8
                    + 8 * code.pointers.len()
                    + 16
                    + 8
                    + writes(&code.lasting)
                    + writes(&code.writes)
                    + 8
            }
        };
        MAGIC.len() + 16 + 8 + 8 + body
    }

    /// Its encoding, in one buffer of the exact size.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.encoded_len());
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&self.uuid);
        put(&mut out, self.sequence);
        match &self.body {
            Body::Restart(why) => {
                put(&mut out, 0);
                bytes(&mut out, why.as_bytes());
            }
            Body::Code(code) => {
                put(&mut out, 1);
                put(&mut out, code.base);
                bytes(&mut out, &code.text);
                bytes(&mut out, &code.data);
                put(&mut out, code.frames.0);
                put(&mut out, code.frames.1);
                list(&mut out, &code.pointers);
                put(&mut out, code.redirected);
                put(&mut out, code.statics);
                // The statics' first values go with the writes, ahead of them, and their count
                // at the end: a program of an older `blaze-live` writes them as it does the
                // others, where it has nothing mapped, and declines the patch before it
                // redirects anything to code that refers to them.
                put(&mut out, (code.lasting.len() + code.writes.len()) as u64);
                for write in code.lasting.iter().chain(&code.writes) {
                    put(&mut out, write.address);
                    bytes(&mut out, &write.bytes);
                    list(&mut out, &write.pointers);
                }
                put(&mut out, code.lasting.len() as u64);
            }
        }
        out
    }

    /// The patch in `bytes`, owned: v1's watcher thread decodes each patch once and hands it
    /// to the thread that applies it. The jump-table runtime reads its messages in place
    /// (`crate::patch::Message::view`).
    pub fn decode(bytes: &[u8]) -> Result<Patch, String> {
        let mut reader = Fields(bytes);
        if reader.take(8)? != MAGIC {
            // alloc: error path
            return Err("not a patch of this version of blaze".to_owned());
        }
        let uuid = reader.take(16)?.try_into().map_err(|_| "a short patch")?;
        let sequence = reader.u64()?;
        let body = match reader.u64()? {
            // alloc: the reason handed to the program, once per patch that has it start again
            0 => Body::Restart(String::from_utf8_lossy(reader.bytes()?).into_owned()),
            1 => {
                let base = reader.u64()?;
                // alloc: v1's owned patch, once per patch (see above)
                let text = reader.bytes()?.to_vec();
                // alloc: v1's owned patch, once per patch (see above)
                let data = reader.bytes()?.to_vec();
                let frames = (reader.u64()?, reader.u64()?);
                let pointers = reader.list()?;
                let redirected = reader.u64()?;
                let statics = reader.u64()?;
                let count = reader.u64()?;
                // Each write is at least 24 bytes, which bounds a count the bytes can't hold.
                let room = usize::try_from(count).unwrap_or(usize::MAX).min(reader.0.len() / 24);
                let mut writes = Vec::with_capacity(room);
                for _ in 0..count {
                    let address = reader.u64()?;
                    // alloc: v1's owned patch, one per write of it (see above)
                    let bytes = reader.bytes()?.to_vec();
                    writes.push(Write { address, bytes, pointers: reader.list()? });
                }
                // The first of them are the statics' first values, when there are any.
                let lasting = if reader.0.is_empty() { 0 } else { reader.u64()? };
                let lasting = usize::try_from(lasting).map_err(|_| "a patch too large")?;
                if lasting > writes.len() {
                    // alloc: error path
                    return Err("a short patch".to_owned());
                }
                let rest = writes.split_off(lasting);
                let lasting = std::mem::replace(&mut writes, rest);
                Body::Code(Code {
                    base,
                    text,
                    data,
                    frames,
                    pointers,
                    writes,
                    redirected,
                    statics,
                    lasting,
                })
            }
            // alloc: error path
            other => return Err(format!("a patch of kind {other}")),
        };
        if !reader.0.is_empty() {
            // alloc: error path
            return Err(format!("{} bytes past the end of the patch", reader.0.len()));
        }
        Ok(Patch { uuid, sequence, body })
    }
}

/// What a running program says of itself.
#[derive(Debug, PartialEq, Clone, Copy)]
pub struct Process {
    pub pid: u32,
    /// The `LC_UUID` of the executable it runs.
    pub uuid: [u8; 16],
    /// When it started, in milliseconds since the epoch.
    pub started: u64,
    /// Where the next patch may start, and where the room for patches ends.
    pub next: u64,
    pub end: u64,
}

impl Process {
    pub fn encode(&self) -> String {
        let mut out = String::with_capacity(128);
        self.encode_into(&mut out);
        out
    }

    /// [`Process::encode`] appended to `out`: nothing allocated once it has room.
    pub fn encode_into(&self, out: &mut String) {
        use std::fmt::Write as _;
        _ = write!(out, "pid {}\nuuid ", self.pid);
        push_hex(out, &self.uuid);
        _ = write!(out, "\nstarted {}\nnext {:#x}\nend {:#x}\n", self.started, self.next, self.end);
    }

    pub fn decode(text: &str) -> Option<Process> {
        let mut process = Process { pid: 0, uuid: [0; 16], started: 0, next: 0, end: 0 };
        let number = |it: &str| u64::from_str_radix(it.trim_start_matches("0x"), 16).ok();
        for line in text.lines() {
            let (key, value) = line.split_once(' ')?;
            match key {
                "pid" => process.pid = value.parse().ok()?,
                "uuid" => {
                    for (at, byte) in process.uuid.iter_mut().enumerate() {
                        *byte = u8::from_str_radix(value.get(at * 2..at * 2 + 2)?, 16).ok()?;
                    }
                }
                "started" => process.started = value.parse().ok()?,
                "next" => process.next = number(value)?,
                "end" => process.end = number(value)?,
                _ => {}
            }
        }
        (process.pid != 0).then_some(process)
    }
}

/// The file of [`Sample`] in [`dir`].
pub const SAMPLE: &str = "sample";
/// The file of [`Profile`] in [`dir`].
pub const PROFILE: &str = "profile";
/// The file of [`Tiers`] in [`dir`].
pub const TIERS: &str = "tiers";

/// How many counters [`counters`](crate::blaze_live_counters) has room for.
pub const COUNTERS: usize = 1 << 16;

/// What the daemon asks of the sampler.
#[derive(Debug, PartialEq, Clone, Copy)]
pub struct Sample {
    /// How long the sampler sleeps between two looks at the threads, in microseconds.
    pub interval_us: u64,
    /// How many of the counters code of the daemon's counts into: the program reads those.
    pub counters: u64,
    /// The process that reads what is sampled: the sampler stops once it is gone, as when
    /// a worker of the daemon exits. 0 for one not named.
    pub reader: u32,
}

impl Sample {
    pub fn encode(&self) -> String {
        // alloc: the daemon's, once each time it asks a program to sample
        format!(
            "interval_us {}\ncounters {}\nreader {}\n",
            self.interval_us, self.counters, self.reader
        )
    }

    pub fn decode(text: &str) -> Option<Sample> {
        let mut sample = Sample { interval_us: 0, counters: 0, reader: 0 };
        for line in text.lines() {
            let (key, value) = line.split_once(' ')?;
            match key {
                "interval_us" => sample.interval_us = value.parse().ok()?,
                "counters" => sample.counters = value.parse().ok()?,
                "reader" => sample.reader = value.parse().ok()?,
                _ => {}
            }
        }
        (sample.interval_us != 0).then_some(sample)
    }
}

/// Where a program's threads were, since its sampler started. Counts only go up: the daemon
/// takes the difference of two to know where the time went between them.
#[derive(Debug, PartialEq, Default)]
pub struct Profile {
    pub pid: u32,
    /// When the sampler started and when this was written, in milliseconds since the epoch.
    pub started: u64,
    pub at: u64,
    /// How many times the sampler looked at the threads.
    pub ticks: u64,
    /// The threads it found running in the program's own code, which `pcs` has, and those
    /// it found running anywhere else (a library, the kernel's side of a call).
    pub samples: u64,
    pub elsewhere: u64,
    /// The CPU time the sampler itself took, in nanoseconds.
    pub sampler_ns: u64,
    /// Each address a thread was found at, before the slide, and how often.
    pub pcs: Vec<(u64, u64)>,
    /// Each counter that is not zero, by its place among the counters.
    pub counters: Vec<(u32, u64)>,
}

impl Profile {
    pub fn encode(&self) -> String {
        let mut out = String::with_capacity(160 + 24 * (self.pcs.len() + self.counters.len()));
        self.encode_into(&mut out);
        out
    }

    /// [`Profile::encode`] appended to `out`: the sampler's own buffer, so a profile written
    /// a few times a second allocates nothing once it has room.
    pub fn encode_into(&self, out: &mut String) {
        use std::fmt::Write as _;
        _ = write!(
            out,
            "pid {}\nstarted {}\nat {}\nticks {}\nsamples {}\nelsewhere {}\nsampler_ns {}\n",
            self.pid,
            self.started,
            self.at,
            self.ticks,
            self.samples,
            self.elsewhere,
            self.sampler_ns
        );
        for (pc, count) in &self.pcs {
            _ = writeln!(out, "pc {pc:x} {count}");
        }
        for (at, count) in &self.counters {
            _ = writeln!(out, "c {at} {count}");
        }
    }

    pub fn decode(text: &str) -> Option<Profile> {
        let mut profile = Profile::default();
        for line in text.lines() {
            let mut words = line.split(' ');
            let key = words.next()?;
            let mut next = || words.next();
            match key {
                "pc" => {
                    let pc = u64::from_str_radix(next()?, 16).ok()?;
                    profile.pcs.push((pc, next()?.parse().ok()?));
                }
                "c" => profile.counters.push((next()?.parse().ok()?, next()?.parse().ok()?)),
                "pid" => profile.pid = next()?.parse().ok()?,
                "started" => profile.started = next()?.parse().ok()?,
                "at" => profile.at = next()?.parse().ok()?,
                "ticks" => profile.ticks = next()?.parse().ok()?,
                "samples" => profile.samples = next()?.parse().ok()?,
                "elsewhere" => profile.elsewhere = next()?.parse().ok()?,
                "sampler_ns" => profile.sampler_ns = next()?.parse().ok()?,
                _ => {}
            }
        }
        (profile.pid != 0).then_some(profile)
    }
}

/// The levels a function is compiled at, from the quickest to build to the quickest to run.
pub const LEVELS: [&str; 3] = ["Cranelift", "LLVM -O3", "LLVM -O3 + PGO"];

/// What `level` (1, 2 or 3) is called.
pub fn level_name(level: u8) -> &'static str {
    LEVELS.get(usize::from(level).wrapping_sub(1)).copied().unwrap_or("?")
}

/// What the daemon says of the tiers of a running program.
#[derive(Debug, PartialEq, Default)]
pub struct Tiers {
    /// When it was written, in milliseconds since the epoch.
    pub at: u64,
    /// How long the counts of `functions` are over, in milliseconds.
    pub window_ms: u64,
    /// The samples taken in that window.
    pub samples: u64,
    /// The CPU the sampler took, in thousandths of the time it ran.
    pub sampler_permille: u64,
    /// The functions the samples fell in, the hottest first.
    pub functions: Vec<TierFunction>,
    /// What the daemon did, the newest last.
    pub events: Vec<TierEvent>,
}

#[derive(Debug, PartialEq, Default)]
pub struct TierFunction {
    /// 1, 2 or 3 ([`level_name`]); 0 for code the daemon did not compile and can't tier.
    pub level: u8,
    /// Samples in the window, and their share of all of them in thousandths.
    pub samples: u64,
    pub share: u64,
    pub name: String,
    /// What is under way or why it stays where it is, or nothing.
    pub note: String,
}

// clone: the daemon copies its recent events into each `Tiers` it writes, a few times a second
#[derive(Debug, PartialEq, Clone, Default)]
pub struct TierEvent {
    /// In milliseconds since the epoch.
    pub at: u64,
    pub text: String,
}

impl Tiers {
    /// Its text, written by the daemon a few times a second, every name and note on one line:
    /// a tab or a newline in one is written as a space.
    pub fn encode(&self) -> String {
        use std::fmt::Write as _;
        let mut out = String::with_capacity(
            96 + (self.functions.iter().map(|f| 48 + f.name.len() + f.note.len()))
                .chain(self.events.iter().map(|event| 32 + event.text.len()))
                .sum::<usize>(),
        );
        _ = write!(
            out,
            "at {}\nwindow_ms {}\nsamples {}\nsampler_permille {}\n",
            self.at, self.window_ms, self.samples, self.sampler_permille
        );
        for f in &self.functions {
            _ = write!(out, "fn\t{}\t{}\t{}\t", f.level, f.samples, f.share);
            push_clean(&mut out, &f.name);
            out.push('\t');
            push_clean(&mut out, &f.note);
            out.push('\n');
        }
        for event in &self.events {
            _ = write!(out, "event\t{}\t", event.at);
            push_clean(&mut out, &event.text);
            out.push('\n');
        }
        out
    }

    /// What [`Tiers::encode`] wrote, owned: the program's copy, read from the daemon's file a
    /// few times a second ([`crate::tiers`]), not per frame.
    pub fn decode(text: &str) -> Option<Tiers> {
        let mut tiers = Tiers::default();
        for line in text.lines() {
            if let Some(rest) = line.strip_prefix("fn\t") {
                let mut fields = rest.split('\t');
                let mut next = || fields.next();
                tiers.functions.push(TierFunction {
                    level: next()?.parse().ok()?,
                    samples: next()?.parse().ok()?,
                    share: next()?.parse().ok()?,
                    // alloc: the program's copy (see above)
                    name: next()?.to_owned(),
                    // alloc: the program's copy (see above)
                    note: next().unwrap_or_default().to_owned(),
                });
                continue;
            }
            if let Some(rest) = line.strip_prefix("event\t") {
                let (at, text) = rest.split_once('\t')?;
                // alloc: the program's copy (see above)
                tiers.events.push(TierEvent { at: at.parse().ok()?, text: text.to_owned() });
                continue;
            }
            let (key, value) = line.split_once(' ')?;
            match key {
                "at" => tiers.at = value.parse().ok()?,
                "window_ms" => tiers.window_ms = value.parse().ok()?,
                "samples" => tiers.samples = value.parse().ok()?,
                "sampler_permille" => tiers.sampler_permille = value.parse().ok()?,
                _ => {}
            }
        }
        (tiers.at != 0).then_some(tiers)
    }
}

fn put(out: &mut Vec<u8>, value: u64) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    put(out, bytes.len() as u64);
    out.extend_from_slice(bytes);
}

fn list(out: &mut Vec<u8>, values: &[u64]) {
    put(out, values.len() as u64);
    for value in values {
        put(out, *value);
    }
}

/// `text` appended to `out` with each tab and newline a space.
fn push_clean(out: &mut String, text: &str) {
    out.extend(text.chars().map(|c| if c == '\t' || c == '\n' { ' ' } else { c }));
}

/// A patch's fields, read in order. Not `blaze_format::cursor::Reader`: this crate runs
/// inside the patched program and depends on no other crate.
struct Fields<'a>(&'a [u8]);

impl<'a> Fields<'a> {
    fn take(&mut self, len: usize) -> Result<&'a [u8], String> {
        if self.0.len() < len {
            // alloc: error path
            return Err("a patch that ends early".to_owned());
        }
        let (taken, rest) = self.0.split_at(len);
        self.0 = rest;
        Ok(taken)
    }

    fn u64(&mut self) -> Result<u64, String> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().map_err(|_| "a short patch")?))
    }

    fn bytes(&mut self) -> Result<&'a [u8], String> {
        let len = usize::try_from(self.u64()?).map_err(|_| "a patch too large")?;
        self.take(len)
    }

    /// A list of `u64`s, owned for v1's patch ([`Patch::decode`]), sized once.
    fn list(&mut self) -> Result<Vec<u64>, String> {
        let count = self.u64()?;
        // Each value is 8 bytes, which bounds a count the bytes can't hold.
        let room = usize::try_from(count).unwrap_or(usize::MAX).min(self.0.len() / 8);
        let mut values = Vec::with_capacity(room);
        for _ in 0..count {
            values.push(self.u64()?);
        }
        Ok(values)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let code = Code {
            base: 0x1_4000_0000,
            text: vec![1, 2, 3],
            data: vec![4],
            frames: (1, 2),
            pointers: vec![8, 16],
            writes: vec![Write { address: 0x1_0000_4000, bytes: vec![9; 4], pointers: vec![] }],
            redirected: 1,
            statics: 0,
            lasting: vec![Write { address: 0x1_7fff_fff0, bytes: vec![1; 8], pointers: vec![0] }],
        };
        let patch = Patch { uuid: [7; 16], sequence: 3, body: Body::Code(code) };
        assert_eq!(Patch::decode(&patch.encode()).unwrap(), patch);
        assert_eq!(patch.encode().len(), patch.encoded_len());
        let restart = Patch { uuid: [1; 16], sequence: 4, body: Body::Restart("why".into()) };
        assert_eq!(Patch::decode(&restart.encode()).unwrap(), restart);
        assert_eq!(restart.encode().len(), restart.encoded_len());
        let empty = Patch { uuid: [0; 16], sequence: 0, body: Body::Code(Code::default()) };
        assert_eq!(empty.encode().len(), empty.encoded_len());
        for whole in [patch.encode(), restart.encode()] {
            let longer = [&whole[..], &[0]].concat();
            assert_eq!(Patch::decode(&longer), Err("1 bytes past the end of the patch".to_owned()));
        }
        let process = Process {
            pid: 42,
            uuid: [0xab; 16],
            started: 1_792_000_000_000,
            next: 0x1_8000_0000,
            end: 0x2_0000_0000,
        };
        assert_eq!(Process::decode(&process.encode()), Some(process));
        // The text as it was written with one `format!` and a hex string.
        let uuid: String = process.uuid.iter().map(|it| format!("{it:02x}")).collect();
        let was = format!(
            "pid {}\nuuid {uuid}\nstarted {}\nnext {:#x}\nend {:#x}\n",
            process.pid, process.started, process.next, process.end
        );
        assert_eq!(process.encode(), was);
        let mut reused = String::from("x");
        process.encode_into(&mut reused);
        assert_eq!(reused, format!("x{was}"));
        assert_eq!(hex(&[0x00, 0x0f, 0xa0, 0xff]), "000fa0ff");
    }

    #[test]
    fn tiering_round_trip() {
        let sample = Sample { interval_us: 2000, counters: 12, reader: 4321 };
        assert_eq!(Sample::decode(&sample.encode()), Some(sample));
        let profile = Profile {
            pid: 7,
            started: 1,
            at: 2,
            ticks: 3,
            samples: 4,
            elsewhere: 5,
            sampler_ns: 6,
            pcs: vec![(0x1_0000_4abc, 9)],
            counters: vec![(3, 100)],
        };
        let mut reused = String::from("x");
        profile.encode_into(&mut reused);
        assert_eq!(reused, format!("x{}", profile.encode()));
        assert_eq!(Profile::decode(&profile.encode()), Some(profile));
        let tiers = Tiers {
            at: 10,
            window_ms: 1000,
            samples: 400,
            sampler_permille: 3,
            functions: vec![TierFunction {
                level: 2,
                samples: 300,
                share: 750,
                name: "<life::Life as Step>::step".into(),
                note: "profiling".into(),
            }],
            events: vec![TierEvent { at: 9, text: "step: LLVM -O3 in 120 ms".into() }],
        };
        assert_eq!(Tiers::decode(&tiers.encode()), Some(tiers));
        assert_eq!(level_name(3), "LLVM -O3 + PGO");
    }

    #[test]
    fn tiers_write_tabs_and_newlines_in_text_as_spaces() {
        let tiers = Tiers {
            at: 1,
            functions: vec![TierFunction {
                level: 1,
                samples: 2,
                share: 3,
                name: "a\tb\nc".into(),
                note: "\t".into(),
            }],
            events: vec![TierEvent { at: 4, text: "x\ny".into() }],
            ..Tiers::default()
        };
        let text = tiers.encode();
        assert_eq!(
            text,
            "at 1\nwindow_ms 0\nsamples 0\nsampler_permille 0\nfn\t1\t2\t3\ta b c\t \n\
             event\t4\tx y\n"
        );
        let read = Tiers::decode(&text).expect("tiers");
        assert_eq!((&read.functions[0].name[..], &read.events[0].text[..]), ("a b c", "x y"));
    }

    /// The sampler writes its profile a few times a second into the same buffer: once it has
    /// room, that allocates nothing.
    #[test]
    fn a_profile_encoded_into_a_reused_buffer_allocates_nothing() {
        let profile = Profile {
            pid: 7,
            started: 1,
            at: 2,
            pcs: (0..512).map(|it| (0x1_0000_0000 + 4 * it, it)).collect(),
            counters: (0..64).map(|it| (it as u32, it)).collect(),
            ..Profile::default()
        };
        let mut text = String::new();
        profile.encode_into(&mut text);
        let mut process = String::new();
        let it = Process { pid: 1, uuid: [0xab; 16], started: 2, next: 3, end: 4 };
        it.encode_into(&mut process);
        blaze_alloc_count::assert_at_most(0, "a warm profile and process text", || {
            for _ in 0..16 {
                text.clear();
                profile.encode_into(&mut text);
                process.clear();
                it.encode_into(&mut process);
            }
        });
    }
}
