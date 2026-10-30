//! What blaze-link and a running program say to each other.
//!
//! An image blaze-link made has a descriptor ([`Descriptor`], the symbol [`DESCRIPTOR`]) that
//! says where its jump table is, where new code and data go, and which directory patches come
//! through. The linker writes one [`Message`] per file in that directory, named by
//! [`message_file`] in the order the program reads them, and the program answers each in the
//! file [`ANSWER`] ([`Answer`]).
//!
//! Every address is as linked, before the loader slid the image: the program adds its slide.
//! Every number is little-endian. Decoding checks every length against the bytes there are,
//! and refuses unknown kinds, unknown versions and bytes left over.

/// The name of the descriptor's symbol, which the linker exports.
pub const DESCRIPTOR: &str = "__blaze_live";
pub const DESCRIPTOR_MAGIC: [u8; 8] = *b"BLZLIVE2";
/// The version of [`Descriptor`] this crate reads and writes.
pub const DESCRIPTOR_VERSION: u32 = 2;

/// [`Descriptor::arch`] of x86-64 and of AArch64.
pub const ARCH_X86_64: u32 = 0;
pub const ARCH_AARCH64: u32 = 1;

/// The generation of an image's first full link; each commit of the image counts up from it.
/// A program starts at the generation its descriptor says ([`Descriptor::generation`]).
pub const FIRST_GENERATION: u64 = 0;

/// What the symbol `__blaze_live` of an image blaze-link made is: written by the linker in
/// read-only data, every address as linked (unslid); the slide is where the descriptor is
/// in memory minus `this`.
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Descriptor {
    pub magic: [u8; 8],
    pub version: u32,
    /// 0 x86-64, 1 AArch64.
    pub arch: u32,
    pub id: [u8; 16],
    /// The generation the image was linked at: the program starts there, and accepts a patch
    /// whose `base` is the generation it is at.
    pub generation: u64,
    pub this: u64,
    /// The cell holding the address of the current jump table (an array of `slots` code
    /// addresses, slid by the loader); every call through a slot loads this cell first.
    pub table: u64,
    /// The table's capacity: every slot has its stub from the full link on, so a patch only
    /// sets cells.
    pub slots: u64,
    /// `[start, end)` of the region new code is placed in, and of new data's.
    pub patch_code: [u64; 2],
    pub patch_data: [u64; 2],
    /// The address and length of the UTF-8 path of the directory patches come through.
    pub dir: [u64; 2],
}

impl Descriptor {
    /// Its encoded size, which is also its size in memory: every field is aligned as it is.
    pub const SIZE: usize = 8 + 4 + 4 + 16 + 8 + 8 + 8 + 8 + 16 + 16 + 16;

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Writer(Vec::with_capacity(Self::SIZE));
        out.0.extend_from_slice(&self.magic);
        out.u32(self.version);
        out.u32(self.arch);
        out.0.extend_from_slice(&self.id);
        for value in [self.generation, self.this, self.table, self.slots]
            .into_iter()
            .chain(self.patch_code)
            .chain(self.patch_data)
            .chain(self.dir)
        {
            out.u64(value);
        }
        out.0
    }

    /// The descriptor at the start of `bytes`: none when they are too short or don't start
    /// with [`DESCRIPTOR_MAGIC`]. The version is the reader's to check.
    pub fn decode(bytes: &[u8]) -> Option<Descriptor> {
        let mut reader = Reader(bytes.get(..Self::SIZE)?);
        let magic: [u8; 8] = reader.array().ok()?;
        if magic != DESCRIPTOR_MAGIC {
            return None;
        }
        let version = reader.u32().ok()?;
        let arch = reader.u32().ok()?;
        let id = reader.array().ok()?;
        let mut next = || reader.u64().ok();
        Some(Descriptor {
            magic,
            version,
            arch,
            id,
            generation: next()?,
            this: next()?,
            table: next()?,
            slots: next()?,
            patch_code: [next()?, next()?],
            patch_data: [next()?, next()?],
            dir: [next()?, next()?],
        })
    }
}

const _: () = assert!(Descriptor::SIZE == size_of::<Descriptor>());

#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Patch {
    pub id: [u8; 16],
    /// Generations count commits of the image; a patch applies on `base` only.
    pub generation: u64,
    pub base: u64,
    /// Bytes for the patch code region, and for data anywhere in the image (a static's new
    /// value in place, a new static in the patch data region, a panic location's record).
    pub code: Vec<(u64, Vec<u8>)>,
    pub data: Vec<(u64, Vec<u8>)>,
    /// Addresses within those writes holding an unslid image address: add the slide.
    pub rebase: Vec<u64>,
    /// Slots and their new targets (unslid).
    pub cells: Vec<(u32, u64)>,
    /// FDEs (address, length) within the writes to register with the unwinder, and on
    /// Windows the RUNTIME_FUNCTIONs (begin, end, unwind info) to add.
    pub frames: Vec<(u64, u64)>,
    pub windows: Vec<(u64, u64, u64)>,
    /// How many functions and statics the patch changes, for the program to say.
    pub functions: u64,
    pub statics: u64,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Message {
    Patch(Patch),
    /// Back to the table of `to` (a generation the program applied or started at).
    RollBack {
        id: [u8; 16],
        to: u64,
    },
    /// The edit can't be applied to a running program: start it again.
    Restart {
        id: [u8; 16],
        generation: u64,
        why: String,
    },
}

const MESSAGE_MAGIC: [u8; 8] = *b"BLZLMSG2";
const ANSWER_MAGIC: [u8; 8] = *b"BLZLANS2";
/// The version of [`Message`] and [`Answer`] this crate reads and writes.
pub const MESSAGE_VERSION: u32 = 1;

impl Message {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Writer::header(MESSAGE_MAGIC);
        match self {
            Message::Patch(patch) => {
                out.u32(0);
                out.0.extend_from_slice(&patch.id);
                out.u64(patch.generation);
                out.u64(patch.base);
                for writes in [&patch.code, &patch.data] {
                    out.len(writes.len());
                    for (address, bytes) in writes {
                        out.u64(*address);
                        out.bytes(bytes);
                    }
                }
                out.len(patch.rebase.len());
                patch.rebase.iter().for_each(|it| out.u64(*it));
                out.len(patch.cells.len());
                for &(slot, target) in &patch.cells {
                    out.u32(slot);
                    out.u64(target);
                }
                out.len(patch.frames.len());
                for &(address, len) in &patch.frames {
                    out.u64(address);
                    out.u64(len);
                }
                out.len(patch.windows.len());
                for &(begin, end, unwind) in &patch.windows {
                    out.u64(begin);
                    out.u64(end);
                    out.u64(unwind);
                }
                out.u64(patch.functions);
                out.u64(patch.statics);
            }
            Message::RollBack { id, to } => {
                out.u32(1);
                out.0.extend_from_slice(id);
                out.u64(*to);
            }
            Message::Restart { id, generation, why } => {
                out.u32(2);
                out.0.extend_from_slice(id);
                out.u64(*generation);
                out.bytes(why.as_bytes());
            }
        }
        out.0
    }

    pub fn decode(bytes: &[u8]) -> Result<Message, String> {
        let mut reader = Reader(bytes);
        reader.header(MESSAGE_MAGIC, "message")?;
        let message = match reader.u32()? {
            0 => {
                let id = reader.array()?;
                let generation = reader.u64()?;
                let base = reader.u64()?;
                let mut writes = || -> Result<Vec<(u64, Vec<u8>)>, String> {
                    let count = reader.count(16)?;
                    let mut writes = Vec::with_capacity(count);
                    for _ in 0..count {
                        writes.push((reader.u64()?, reader.bytes()?.to_vec()));
                    }
                    Ok(writes)
                };
                let code = writes()?;
                let data = writes()?;
                let count = reader.count(8)?;
                let rebase = (0..count).map(|_| reader.u64()).collect::<Result<_, _>>()?;
                let count = reader.count(12)?;
                let cells = (0..count)
                    .map(|_| Ok((reader.u32()?, reader.u64()?)))
                    .collect::<Result<_, String>>()?;
                let count = reader.count(16)?;
                let frames = (0..count)
                    .map(|_| Ok((reader.u64()?, reader.u64()?)))
                    .collect::<Result<_, String>>()?;
                let count = reader.count(24)?;
                let windows = (0..count)
                    .map(|_| Ok((reader.u64()?, reader.u64()?, reader.u64()?)))
                    .collect::<Result<_, String>>()?;
                let functions = reader.u64()?;
                let statics = reader.u64()?;
                Message::Patch(Patch {
                    id,
                    generation,
                    base,
                    code,
                    data,
                    rebase,
                    cells,
                    frames,
                    windows,
                    functions,
                    statics,
                })
            }
            1 => Message::RollBack { id: reader.array()?, to: reader.u64()? },
            2 => Message::Restart {
                id: reader.array()?,
                generation: reader.u64()?,
                why: reader.text()?,
            },
            other => return Err(format!("a message of kind {other}")),
        };
        reader.end("message")?;
        Ok(message)
    }
}

/// The file of the `sequence`-th message in the directory: the program reads them in order.
pub fn message_file(sequence: u64) -> String {
    format!("message-{sequence:016x}")
}

/// What the program answers, in the file `ANSWER` of the directory (replaced each time).
pub const ANSWER: &str = "answer";

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Answer {
    Applied { generation: u64 },
    RolledBack { generation: u64, to: u64, why: String },
    Declined { generation: u64, why: String },
}

impl Answer {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Writer::header(ANSWER_MAGIC);
        match self {
            Answer::Applied { generation } => {
                out.u32(0);
                out.u64(*generation);
            }
            Answer::RolledBack { generation, to, why } => {
                out.u32(1);
                out.u64(*generation);
                out.u64(*to);
                out.bytes(why.as_bytes());
            }
            Answer::Declined { generation, why } => {
                out.u32(2);
                out.u64(*generation);
                out.bytes(why.as_bytes());
            }
        }
        out.0
    }

    pub fn decode(bytes: &[u8]) -> Result<Answer, String> {
        let mut reader = Reader(bytes);
        reader.header(ANSWER_MAGIC, "answer")?;
        let answer = match reader.u32()? {
            0 => Answer::Applied { generation: reader.u64()? },
            1 => Answer::RolledBack {
                generation: reader.u64()?,
                to: reader.u64()?,
                why: reader.text()?,
            },
            2 => Answer::Declined { generation: reader.u64()?, why: reader.text()? },
            other => return Err(format!("an answer of kind {other}")),
        };
        reader.end("answer")?;
        Ok(answer)
    }
}

struct Writer(Vec<u8>);

impl Writer {
    fn header(magic: [u8; 8]) -> Writer {
        let mut out = Writer(Vec::new());
        out.0.extend_from_slice(&magic);
        out.u32(MESSAGE_VERSION);
        out
    }

    fn u32(&mut self, value: u32) {
        self.0.extend_from_slice(&value.to_le_bytes());
    }

    fn u64(&mut self, value: u64) {
        self.0.extend_from_slice(&value.to_le_bytes());
    }

    fn len(&mut self, len: usize) {
        self.u64(len as u64);
    }

    fn bytes(&mut self, bytes: &[u8]) {
        self.len(bytes.len());
        self.0.extend_from_slice(bytes);
    }
}

struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, len: usize) -> Result<&'a [u8], String> {
        if self.0.len() < len {
            return Err(format!("ends early: {len} bytes wanted, {} left", self.0.len()));
        }
        let (taken, rest) = self.0.split_at(len);
        self.0 = rest;
        Ok(taken)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], String> {
        let mut array = [0; N];
        array.copy_from_slice(self.take(N)?);
        Ok(array)
    }

    fn u32(&mut self) -> Result<u32, String> {
        Ok(u32::from_le_bytes(self.array()?))
    }

    fn u64(&mut self) -> Result<u64, String> {
        Ok(u64::from_le_bytes(self.array()?))
    }

    /// A count of things each at least `each` bytes long, which the bytes left can hold.
    fn count(&mut self, each: usize) -> Result<usize, String> {
        let count = self.u64()?;
        let left = (self.0.len() / each) as u64;
        if count > left {
            return Err(format!("{count} entries where {left} fit"));
        }
        Ok(count as usize)
    }

    fn bytes(&mut self) -> Result<&'a [u8], String> {
        let len = self.count(1)?;
        self.take(len)
    }

    fn text(&mut self) -> Result<String, String> {
        let bytes = self.bytes()?;
        String::from_utf8(bytes.to_vec()).map_err(|_| "text that is not UTF-8".to_owned())
    }

    fn header(&mut self, magic: [u8; 8], what: &str) -> Result<(), String> {
        if self.array::<8>().ok() != Some(magic) {
            return Err(format!("not a blaze-live {what}"));
        }
        match self.u32()? {
            MESSAGE_VERSION => Ok(()),
            other => Err(format!(
                "a {what} of version {other}, where this program reads {MESSAGE_VERSION}"
            )),
        }
    }

    fn end(&self, what: &str) -> Result<(), String> {
        if self.0.is_empty() {
            Ok(())
        } else {
            Err(format!("{} bytes after the {what}", self.0.len()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn descriptor() -> Descriptor {
        Descriptor {
            magic: DESCRIPTOR_MAGIC,
            version: DESCRIPTOR_VERSION,
            arch: ARCH_AARCH64,
            id: [0x5a; 16],
            generation: 9,
            this: 0x1_0000_8000,
            table: 0x1_0004_0000,
            slots: 1234,
            patch_code: [0x1_0100_0000, 0x1_0900_0000],
            patch_data: [0x1_0900_0000, 0x1_0a00_0000],
            dir: [0x1_0000_9000, 37],
        }
    }

    fn patch() -> Patch {
        Patch {
            id: [7; 16],
            generation: 3,
            base: 2,
            code: vec![(0x1_0100_0000, vec![0xd5, 0x03, 0x20, 0x1f]), (0x1_0100_4000, vec![])],
            data: vec![(0x1_0900_0010, vec![1; 24])],
            rebase: vec![0x1_0900_0010, 0x1_0900_0018],
            cells: vec![(0, 0x1_0100_0000), (u32::MAX, 1)],
            frames: vec![(0x1_0100_0040, 0x18)],
            windows: vec![(0x1000, 0x1040, 0x2000)],
            functions: 2,
            statics: 1,
        }
    }

    fn messages() -> Vec<Message> {
        vec![
            Message::Patch(patch()),
            Message::Patch(Patch::default()),
            Message::RollBack { id: [9; 16], to: 1 },
            Message::Restart {
                id: [1; 16],
                generation: 4,
                why: "a struct's layout — changed".into(),
            },
        ]
    }

    fn answers() -> Vec<Answer> {
        vec![
            Answer::Applied { generation: 3 },
            Answer::RolledBack { generation: 3, to: 2, why: "panicked at src/x.rs:1:2: no".into() },
            Answer::Declined { generation: 5, why: String::new() },
        ]
    }

    #[test]
    fn descriptor_round_trips_and_is_its_layout() {
        let descriptor = descriptor();
        let bytes = descriptor.encode();
        assert_eq!(bytes.len(), Descriptor::SIZE);
        assert_eq!(Descriptor::decode(&bytes), Some(descriptor));
        // The linker writes the encoding, the program may read the struct: they agree.
        // SAFETY: `Descriptor` is `repr(C)` of integers and byte arrays, every bit pattern
        // valid, and `bytes` has `SIZE` of them; the read is unaligned.
        let read = unsafe { (bytes.as_ptr() as *const Descriptor).read_unaligned() };
        assert_eq!(read, descriptor);
        let mut longer = bytes.clone();
        longer.extend_from_slice(&[0; 8]);
        assert_eq!(Descriptor::decode(&longer), Some(descriptor));
        for len in 0..bytes.len() {
            assert_eq!(Descriptor::decode(&bytes[..len]), None, "{len}");
        }
        let mut other = bytes.clone();
        other[0] ^= 1;
        assert_eq!(Descriptor::decode(&other), None);
    }

    #[test]
    fn messages_and_answers_round_trip() {
        for message in messages() {
            assert_eq!(Message::decode(&message.encode()), Ok(message));
        }
        for answer in answers() {
            assert_eq!(Answer::decode(&answer.encode()), Ok(answer));
        }
        assert_eq!(message_file(0x2a), "message-000000000000002a");
    }

    #[test]
    fn truncated_and_extended_bytes_are_errors() {
        for message in messages() {
            let bytes = message.encode();
            for len in 0..bytes.len() {
                assert!(Message::decode(&bytes[..len]).is_err(), "{message:?} at {len}");
            }
            let mut longer = bytes.clone();
            longer.push(0);
            assert!(Message::decode(&longer).is_err());
            assert!(Answer::decode(&bytes).is_err(), "a message is no answer");
        }
        for answer in answers() {
            let bytes = answer.encode();
            for len in 0..bytes.len() {
                assert!(Answer::decode(&bytes[..len]).is_err(), "{answer:?} at {len}");
            }
        }
    }

    #[test]
    fn wrong_headers_kinds_and_text_are_errors() {
        let bytes = Message::RollBack { id: [0; 16], to: 0 }.encode();
        let mut version = bytes.clone();
        version[8] = 2;
        assert!(Message::decode(&version).unwrap_err().contains("version 2"));
        let mut kind = bytes.clone();
        kind[12] = 9;
        assert_eq!(Message::decode(&kind), Err("a message of kind 9".to_owned()));
        let restart = Message::Restart { id: [0; 16], generation: 0, why: "ab".into() }.encode();
        let mut text = restart.clone();
        let last = text.len() - 1;
        text[last] = 0xff;
        assert_eq!(Message::decode(&text), Err("text that is not UTF-8".to_owned()));
    }

    #[test]
    fn huge_counts_are_errors_without_allocating_them() {
        let mut bytes = Message::Patch(Patch::default()).encode();
        // The count of code writes, after the header, kind, id, generation and base.
        let at = 8 + 4 + 4 + 16 + 8 + 8;
        bytes[at..at + 8].copy_from_slice(&u64::MAX.to_le_bytes());
        assert!(Message::decode(&bytes).unwrap_err().contains("entries where"));
        let mut answer = Answer::Declined { generation: 0, why: "x".into() }.encode();
        let at = 8 + 4 + 4 + 8;
        answer[at..at + 8].copy_from_slice(&(u64::MAX - 1).to_le_bytes());
        assert!(Answer::decode(&answer).is_err());
    }

    /// Every byte flipped, and many random corruptions, decode to something or an error,
    /// never a panic.
    #[test]
    fn corrupted_bytes_never_panic() {
        let mut state = 0x9e37_79b9_7f4a_7c15u64;
        let mut random = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let encoded: Vec<Vec<u8>> = messages()
            .iter()
            .map(Message::encode)
            .chain(answers().iter().map(Answer::encode))
            .collect();
        for bytes in &encoded {
            for at in 0..bytes.len() {
                for flip in [0x01, 0x80, 0xff] {
                    let mut corrupt = bytes.clone();
                    corrupt[at] ^= flip;
                    _ = Message::decode(&corrupt);
                    _ = Answer::decode(&corrupt);
                    _ = Descriptor::decode(&corrupt);
                }
            }
            for _ in 0..2000 {
                let mut corrupt = bytes.clone();
                for _ in 0..1 + random() % 4 {
                    let at = (random() % corrupt.len() as u64) as usize;
                    corrupt[at] = random() as u8;
                }
                let len = (random() % (corrupt.len() as u64 + 1)) as usize;
                corrupt.truncate(len);
                _ = Message::decode(&corrupt);
                _ = Answer::decode(&corrupt);
            }
        }
    }
}
