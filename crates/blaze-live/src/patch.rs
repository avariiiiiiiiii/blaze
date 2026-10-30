//! What blaze-link and a running program say to each other.
//!
//! An image blaze-link made has a descriptor ([`Descriptor`], the symbol [`DESCRIPTOR`]) that
//! says where its jump table is, where new code and data go, and which directory patches come
//! through. The linker writes one [`Message`] per file in that directory, named by
//! [`message_file`] of the generation a program must be at to take it, and the program
//! answers each in the file [`ANSWER`] ([`Answer`]).
//!
//! Every address is as linked, before the loader slid the image: the program adds its slide.
//! Every number is little-endian. Decoding checks every length against the bytes there are,
//! and refuses unknown kinds, unknown versions and bytes left over.
//!
//! The program reads a message in place ([`Message::view`]: one pass over its headers, no
//! allocation, the bytes of each write borrowed from the mapped file), and the linker writes
//! one from borrowed bytes ([`PatchRef`]) into one buffer of the exact size.

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
        let mut bytes = Vec::with_capacity(Self::SIZE);
        let mut out = Writer(&mut bytes);
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
        bytes
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

impl Patch {
    /// The patch with its bytes borrowed, which encodes to the same message.
    pub fn with<R>(&self, f: impl FnOnce(&PatchRef<'_>) -> R) -> R {
        let code: Vec<(u64, &[u8])> = self.code.iter().map(|(at, it)| (*at, &it[..])).collect();
        let data: Vec<(u64, &[u8])> = self.data.iter().map(|(at, it)| (*at, &it[..])).collect();
        f(&PatchRef {
            id: self.id,
            generation: self.generation,
            base: self.base,
            code: &code,
            data: &data,
            rebase: &self.rebase,
            cells: &self.cells,
            frames: &self.frames,
            windows: &self.windows,
            functions: self.functions,
            statics: self.statics,
        })
    }
}

/// A [`Patch`] whose bytes are borrowed: what the linker encodes straight from its own
/// buffers, copying each byte once, into the message.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PatchRef<'a> {
    pub id: [u8; 16],
    pub generation: u64,
    pub base: u64,
    pub code: &'a [(u64, &'a [u8])],
    pub data: &'a [(u64, &'a [u8])],
    pub rebase: &'a [u64],
    pub cells: &'a [(u32, u64)],
    pub frames: &'a [(u64, u64)],
    pub windows: &'a [(u64, u64, u64)],
    pub functions: u64,
    pub statics: u64,
}

impl PatchRef<'_> {
    /// The size of its message.
    pub fn encoded_len(&self) -> usize {
        let writes =
            |writes: &[(u64, &[u8])]| -> usize { writes.iter().map(|it| 16 + it.1.len()).sum() };
        HEADER
            + 16
            + 8
            + 8
            + 8
            + writes(self.code)
            + 8
            + writes(self.data)
            + 8
            + 8 * self.rebase.len()
            + 8
            + 12 * self.cells.len()
            + 8
            + 16 * self.frames.len()
            + 8
            + 24 * self.windows.len()
            + 16
    }

    /// Appends its message to `out`, which grows at most once.
    pub fn encode_into(&self, out: &mut Vec<u8>) {
        out.reserve(self.encoded_len());
        let mut out = Writer::header(out, MESSAGE_MAGIC);
        out.u32(0);
        out.0.extend_from_slice(&self.id);
        out.u64(self.generation);
        out.u64(self.base);
        for writes in [self.code, self.data] {
            out.len(writes.len());
            for &(address, bytes) in writes {
                out.u64(address);
                out.bytes(bytes);
            }
        }
        out.len(self.rebase.len());
        self.rebase.iter().for_each(|it| out.u64(*it));
        out.len(self.cells.len());
        for &(slot, target) in self.cells {
            out.u32(slot);
            out.u64(target);
        }
        out.len(self.frames.len());
        for &(address, len) in self.frames {
            out.u64(address);
            out.u64(len);
        }
        out.len(self.windows.len());
        for &(begin, end, unwind) in self.windows {
            out.u64(begin);
            out.u64(end);
            out.u64(unwind);
        }
        out.u64(self.functions);
        out.u64(self.statics);
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.encoded_len());
        self.encode_into(&mut out);
        out
    }
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

/// A patch read in place from its message ([`Message::view`]): the bytes of its writes are
/// the message's, and its lists are read from them as they are iterated. Every bound was
/// checked when it was read, so nothing here fails.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PatchView<'a> {
    pub id: [u8; 16],
    pub generation: u64,
    pub base: u64,
    pub code: WriteList<'a>,
    pub data: WriteList<'a>,
    /// 8, 12, 16 and 24 bytes per entry.
    rebase: &'a [u8],
    cells: &'a [u8],
    frames: &'a [u8],
    windows: &'a [u8],
    pub functions: u64,
    pub statics: u64,
    /// The whole message, which the writes' bytes are in.
    pub(crate) message: &'a [u8],
}

impl<'a> PatchView<'a> {
    pub fn rebase(self) -> impl ExactSizeIterator<Item = u64> + 'a {
        self.rebase.chunks_exact(8).map(le64)
    }

    pub fn cells(self) -> impl ExactSizeIterator<Item = (u32, u64)> + 'a {
        self.cells.chunks_exact(12).map(|it| (le32(it), le64(&it[4..])))
    }

    pub fn frames(self) -> impl ExactSizeIterator<Item = (u64, u64)> + 'a {
        self.frames.chunks_exact(16).map(|it| (le64(it), le64(&it[8..])))
    }

    pub fn windows(self) -> impl ExactSizeIterator<Item = (u64, u64, u64)> + 'a {
        self.windows.chunks_exact(24).map(|it| (le64(it), le64(&it[8..]), le64(&it[16..])))
    }

    pub fn to_patch(&self) -> Patch {
        let owned = |writes: WriteList<'_>| writes.map(|(at, it)| (at, it.to_vec())).collect();
        Patch {
            id: self.id,
            generation: self.generation,
            base: self.base,
            code: owned(self.code),
            data: owned(self.data),
            rebase: self.rebase().collect(),
            cells: self.cells().collect(),
            frames: self.frames().collect(),
            windows: self.windows().collect(),
            functions: self.functions,
            statics: self.statics,
        }
    }
}

/// Writes read in place: each an address and the bytes to write there, borrowed from the
/// message, whose bounds [`Message::view`] checked.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct WriteList<'a> {
    len: usize,
    /// `len` entries: an address, a length and that many bytes.
    bytes: &'a [u8],
}

impl<'a> Iterator for WriteList<'a> {
    type Item = (u64, &'a [u8]);

    fn next(&mut self) -> Option<(u64, &'a [u8])> {
        if self.len == 0 {
            return None;
        }
        self.len -= 1;
        let at = le64(self.bytes);
        let len = le64(&self.bytes[8..]) as usize;
        let (write, rest) = self.bytes[16..].split_at(len);
        self.bytes = rest;
        Some((at, write))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.len, Some(self.len))
    }
}

impl ExactSizeIterator for WriteList<'_> {}

/// A [`Message`] read in place ([`Message::view`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MessageView<'a> {
    Patch(PatchView<'a>),
    RollBack { id: [u8; 16], to: u64 },
    Restart { id: [u8; 16], generation: u64, why: &'a str },
}

impl MessageView<'_> {
    pub fn to_message(&self) -> Message {
        match *self {
            MessageView::Patch(patch) => Message::Patch(patch.to_patch()),
            MessageView::RollBack { id, to } => Message::RollBack { id, to },
            MessageView::Restart { id, generation, why } => {
                Message::Restart { id, generation, why: why.to_owned() }
            }
        }
    }
}

const MESSAGE_MAGIC: [u8; 8] = *b"BLZLMSG2";
const ANSWER_MAGIC: [u8; 8] = *b"BLZLANS2";
/// The version of [`Message`] and [`Answer`] this crate reads and writes.
pub const MESSAGE_VERSION: u32 = 1;
/// A message's or an answer's magic, version and kind.
const HEADER: usize = 8 + 4 + 4;

impl Message {
    /// The size of its encoding.
    pub fn encoded_len(&self) -> usize {
        match self {
            Message::Patch(patch) => patch.with(|it| it.encoded_len()),
            Message::RollBack { .. } => HEADER + 16 + 8,
            Message::Restart { why, .. } => HEADER + 16 + 8 + 8 + why.len(),
        }
    }

    /// Appends its encoding to `out`, which grows at most once.
    pub fn encode_into(&self, out: &mut Vec<u8>) {
        match self {
            Message::Patch(patch) => patch.with(|it| it.encode_into(out)),
            Message::RollBack { id, to } => {
                out.reserve(self.encoded_len());
                let mut out = Writer::header(out, MESSAGE_MAGIC);
                out.u32(1);
                out.0.extend_from_slice(id);
                out.u64(*to);
            }
            Message::Restart { id, generation, why } => {
                out.reserve(self.encoded_len());
                let mut out = Writer::header(out, MESSAGE_MAGIC);
                out.u32(2);
                out.0.extend_from_slice(id);
                out.u64(*generation);
                out.bytes(why.as_bytes());
            }
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.encoded_len());
        self.encode_into(&mut out);
        out
    }

    pub fn decode(bytes: &[u8]) -> Result<Message, String> {
        Message::view(bytes).map(|it| it.to_message())
    }

    /// The message in `bytes`, read in place: every length is checked against the bytes
    /// there are in one pass over the headers, and nothing is copied or allocated.
    pub fn view(bytes: &[u8]) -> Result<MessageView<'_>, String> {
        let mut reader = Reader(bytes);
        reader.header(MESSAGE_MAGIC, "message")?;
        let view = match reader.u32()? {
            0 => MessageView::Patch(PatchView {
                id: reader.array()?,
                generation: reader.u64()?,
                base: reader.u64()?,
                code: reader.writes()?,
                data: reader.writes()?,
                rebase: reader.list(8)?,
                cells: reader.list(12)?,
                frames: reader.list(16)?,
                windows: reader.list(24)?,
                functions: reader.u64()?,
                statics: reader.u64()?,
                message: bytes,
            }),
            1 => MessageView::RollBack { id: reader.array()?, to: reader.u64()? },
            2 => MessageView::Restart {
                id: reader.array()?,
                generation: reader.u64()?,
                why: reader.str()?,
            },
            other => return Err(format!("a message of kind {other}")),
        };
        reader.end("message")?;
        Ok(view)
    }
}

/// What every message file's name starts with.
pub const MESSAGE_PREFIX: &str = "message-";

/// The file of the message a program at generation `generation` waits for (a patch on it, or
/// a rollback or restart from it).
pub fn message_file(generation: u64) -> String {
    let mut name = String::with_capacity(MESSAGE_PREFIX.len() + 16);
    message_file_into(generation, &mut name);
    name
}

/// [`message_file`] into `name`, cleared first: no allocation once it has room.
pub fn message_file_into(generation: u64, name: &mut String) {
    use std::fmt::Write as _;
    name.clear();
    name.push_str(MESSAGE_PREFIX);
    write!(name, "{generation:016x}").expect("a String takes any text");
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
    /// The size of its encoding.
    pub fn encoded_len(&self) -> usize {
        match self {
            Answer::Applied { .. } => HEADER + 8,
            Answer::RolledBack { why, .. } => HEADER + 8 + 8 + 8 + why.len(),
            Answer::Declined { why, .. } => HEADER + 8 + 8 + why.len(),
        }
    }

    /// Appends its encoding to `out`, which grows at most once.
    pub fn encode_into(&self, out: &mut Vec<u8>) {
        out.reserve(self.encoded_len());
        let mut out = Writer::header(out, ANSWER_MAGIC);
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
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.encoded_len());
        self.encode_into(&mut out);
        out
    }

    pub fn decode(bytes: &[u8]) -> Result<Answer, String> {
        let mut reader = Reader(bytes);
        reader.header(ANSWER_MAGIC, "answer")?;
        let answer = match reader.u32()? {
            0 => Answer::Applied { generation: reader.u64()? },
            1 => Answer::RolledBack {
                generation: reader.u64()?,
                to: reader.u64()?,
                why: reader.str()?.to_owned(),
            },
            2 => Answer::Declined { generation: reader.u64()?, why: reader.str()?.to_owned() },
            other => return Err(format!("an answer of kind {other}")),
        };
        reader.end("answer")?;
        Ok(answer)
    }
}

fn le32(bytes: &[u8]) -> u32 {
    let mut value = [0; 4];
    value.copy_from_slice(&bytes[..4]);
    u32::from_le_bytes(value)
}

fn le64(bytes: &[u8]) -> u64 {
    let mut value = [0; 8];
    value.copy_from_slice(&bytes[..8]);
    u64::from_le_bytes(value)
}

struct Writer<'o>(&'o mut Vec<u8>);

impl<'o> Writer<'o> {
    fn header(out: &'o mut Vec<u8>, magic: [u8; 8]) -> Writer<'o> {
        let mut out = Writer(out);
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

    fn str(&mut self) -> Result<&'a str, String> {
        std::str::from_utf8(self.bytes()?).map_err(|_| "text that is not UTF-8".to_owned())
    }

    /// A count of entries each `each` bytes long, and their bytes.
    fn list(&mut self, each: usize) -> Result<&'a [u8], String> {
        let count = self.count(each)?;
        self.take(count * each)
    }

    /// A count of writes, each an address, a length and its bytes, checked whole.
    fn writes(&mut self) -> Result<WriteList<'a>, String> {
        let len = self.count(16)?;
        let start = self.0;
        for _ in 0..len {
            self.u64()?;
            self.bytes()?;
        }
        Ok(WriteList { len, bytes: &start[..start.len() - self.0.len()] })
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
    use blaze_ids::rng::SplitMix64;

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

    /// The encoding as it was written before messages were read in place: one field after
    /// another into a growing buffer. The wire format is this, byte for byte.
    fn encode_by_fields(message: &Message) -> Vec<u8> {
        let mut bytes = Vec::new();
        let mut out = Writer::header(&mut bytes, MESSAGE_MAGIC);
        match message {
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
        bytes
    }

    #[test]
    #[allow(unsafe_code)]
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
        assert!(message_file(u64::MAX).starts_with(MESSAGE_PREFIX));
    }

    #[test]
    fn the_wire_bytes_are_as_they_were_and_sized_exactly() {
        for message in messages() {
            let bytes = message.encode();
            assert_eq!(bytes, encode_by_fields(&message), "{message:?}");
            assert_eq!(bytes.len(), message.encoded_len());
            let mut appended = vec![0xee; 3];
            message.encode_into(&mut appended);
            assert_eq!(&appended[..3], [0xee; 3]);
            assert_eq!(&appended[3..], bytes);
            if let Message::Patch(patch) = &message {
                assert_eq!(patch.with(|it| it.encode()), bytes);
            }
        }
        for answer in answers() {
            let bytes = answer.encode();
            assert_eq!(bytes.len(), answer.encoded_len());
            let mut reused = vec![1, 2];
            reused.clear();
            answer.encode_into(&mut reused);
            assert_eq!(reused, bytes);
        }
    }

    #[test]
    fn a_view_reads_what_decode_does_in_place() {
        let patch = patch();
        let bytes = Message::Patch(patch.clone()).encode();
        let Ok(MessageView::Patch(view)) = Message::view(&bytes) else { panic!("a patch") };
        assert_eq!((view.id, view.generation, view.base), (patch.id, patch.generation, patch.base));
        assert_eq!(view.code.len(), patch.code.len());
        for ((at, slice), (want_at, want)) in view.code.zip(&patch.code) {
            assert_eq!((at, slice), (*want_at, &want[..]));
            let inside = bytes.as_ptr_range();
            assert!(slice.is_empty() || inside.contains(&slice.as_ptr()), "borrowed, not copied");
        }
        assert_eq!(view.data.collect::<Vec<_>>(), vec![(0x1_0900_0010, &[1u8; 24][..])]);
        assert_eq!(view.rebase().collect::<Vec<_>>(), patch.rebase);
        assert_eq!(view.cells().collect::<Vec<_>>(), patch.cells);
        assert_eq!(view.cells().len(), 2);
        assert_eq!(view.frames().collect::<Vec<_>>(), patch.frames);
        assert_eq!(view.windows().collect::<Vec<_>>(), patch.windows);
        assert_eq!((view.functions, view.statics), (2, 1));
        assert_eq!(view.to_patch(), patch);
        for message in messages() {
            let bytes = message.encode();
            assert_eq!(Message::view(&bytes).map(|it| it.to_message()), Ok(message));
        }
        let restart = Message::Restart { id: [3; 16], generation: 1, why: "why".into() }.encode();
        assert!(matches!(Message::view(&restart), Ok(MessageView::Restart { why: "why", .. })));
    }

    #[test]
    fn truncated_and_extended_bytes_are_errors() {
        for message in messages() {
            let bytes = message.encode();
            for len in 0..bytes.len() {
                assert!(Message::decode(&bytes[..len]).is_err(), "{message:?} at {len}");
                assert!(Message::view(&bytes[..len]).is_err(), "{message:?} at {len}");
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
    /// never a panic; whatever a view reads, iterating it doesn't panic either.
    #[test]
    fn corrupted_bytes_never_panic() {
        let mut rng = SplitMix64::new(0x9e37_79b9_7f4a_7c15);
        let encoded: Vec<Vec<u8>> = messages()
            .iter()
            .map(Message::encode)
            .chain(answers().iter().map(Answer::encode))
            .collect();
        let read = |bytes: &[u8]| {
            if let Ok(MessageView::Patch(view)) = Message::view(bytes) {
                _ = view.to_patch();
            }
            _ = Message::decode(bytes);
            _ = Answer::decode(bytes);
        };
        for bytes in &encoded {
            for at in 0..bytes.len() {
                for flip in [0x01, 0x80, 0xff] {
                    let mut corrupt = bytes.clone();
                    corrupt[at] ^= flip;
                    read(&corrupt);
                    _ = Descriptor::decode(&corrupt);
                }
            }
            for _ in 0..2000 {
                let mut corrupt = bytes.clone();
                for _ in 0..1 + rng.below(4) {
                    let at = rng.index(corrupt.len());
                    corrupt[at] = rng.next_u64() as u8;
                }
                let len = rng.index(corrupt.len() + 1);
                corrupt.truncate(len);
                read(&corrupt);
            }
        }
    }
}
