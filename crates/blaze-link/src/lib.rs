//! The parallel, in-place linker (docs/v2/plan/lanes/link/notes.md; #275, #232, #1496).
//!
//! An [`Image`] is an executable or shared library kept linked. A [`Change`] says which
//! functions, statics, names and native inputs came or went; [`Image::stage`] places the new
//! pieces in room the image keeps and relocates them in parallel, on a copy of the image's
//! state, and nothing is visible until [`Image::commit`]: one atomic rename of the file, then
//! one message to running programs, which switch every jump-table slot at once. Callers reach
//! a function through its [`Slot`]'s stub, so a function that moved changes one table entry.
//! [`Image::roll_back`] returns to the image before the last commit: a patch that panicked.
//! Positions the program observes (`line!()`, `Location`) and debug info are filled here
//! from the span side tables, so a reformat compiles nothing.

mod commit;
mod cow;
mod dwarf;
mod elf;
mod file;
mod live;
mod macho;
mod model;
mod native;
mod output;
mod pe;
mod place;
mod position;
mod reloc;
mod stage;
mod state;
mod symbols;
mod table;
mod unwind;
mod wasm;
mod write;

#[cfg(test)]
mod tests;

use std::collections::VecDeque;
use std::fmt;

use blaze_codegen::{CompiledFunction, InstanceKey, Static, Target};
use blaze_core::{Effect, FileName};
use blaze_ids::{ContentId, Hasher};
use blaze_items::ItemKey;
use rustc_hash::FxHashMap;

pub use output::{Debug, Format, Live, Native, NativeKind, Output, OutputKind, Platform};
pub use position::{COLUMN, LINE, Position, SpanTable};

use commit::{History, Pending, Undo, Writes};
use model::{NativeFile, Plan, Writer};
use stage::{Contents, Stage};
use state::{State, has_table};
use symbols::{Obj, SymKey, Symbols};

/// A function's place in the jump table: stable for as long as the image lives.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct Slot(pub u32);

/// What a link changes.
#[derive(Default)]
pub struct Change<'a> {
    pub added: Vec<&'a CompiledFunction>,
    pub removed: Vec<InstanceKey>,
    /// Slots that run another function than the one they are named for, as `(slot, function)`:
    /// a tiered function's LLVM code in the slot its Cranelift code filled (codegen's
    /// `TieredUp::replaced`), an edited function in the slot its callers call. The function
    /// is in `added` or already linked; only an image with a jump table has slots.
    pub slots: Vec<(InstanceKey, InstanceKey)>,
    pub statics: Vec<&'a Static>,
    pub removed_statics: Vec<(ItemKey, ContentId)>,
    /// Names Rust code defines (`#[no_mangle]`, `#[export_name]`, the entry point's callee),
    /// for native code and the loader, and names no longer defined.
    pub names: Vec<(String, Target)>,
    pub unnamed: Vec<String>,
    /// Names for debuggers, symbol tables and backtraces.
    pub debug_names: Vec<(InstanceKey, String)>,
    pub natives: Vec<Native<'a>>,
    pub removed_natives: Vec<ContentId>,
    /// The span side tables the positions are filled from, by anchor ([`SpanTable::encode`]).
    pub spans: Vec<(ContentId, &'a [u8])>,
}

/// The output of a link, made visible only by a commit.
pub struct Staged {
    base: u64,
    state: State,
    writes: Writes,
    /// The ranges of the old file the commit overwrites.
    undo: Vec<(u64, u64)>,
}

impl Staged {
    /// The generation the image is at once this is committed.
    pub fn generation(&self) -> u64 {
        self.state.generation
    }
}

/// Why a link failed: names the function and the relocation.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct LinkError {
    pub function: Option<InstanceKey>,
    pub what: String,
}

impl fmt::Display for LinkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.function {
            Some(function) => write!(f, "linking {function:?}: {}", self.what),
            None => write!(f, "linking: {}", self.what),
        }
    }
}

impl std::error::Error for LinkError {}

/// One linked executable or shared library.
pub struct Image {
    to: FileName,
    next: FileName,
    name: String,
    output: Output,
    writer: Box<dyn Writer>,
    plan: Plan,
    symbols: Symbols,
    objects: FxHashMap<Obj, native::Read>,
    state: State,
    history: VecDeque<Undo>,
    pending: Option<Pending>,
    tags: u32,
    /// The last generation given out: generations are never reused.
    generations: u64,
}

impl Image {
    /// A new image to be written at `to`, from scratch, linked as `output` says.
    pub fn new(to: FileName, output: Output) -> Result<Image, LinkError> {
        let fail = |what: String| LinkError { function: None, what };
        if output.tags & 0xffff_ffff != 0 {
            return Err(fail(format!("the effect tags {:#x} have low bits set: they are the image's", output.tags)));
        }
        if output.live.is_some() && !output.incremental {
            return Err(fail("a live image must be incremental: it keeps room for patches".into()));
        }
        let writer = model::writer(&output)?;
        let plan = writer.plan();
        if output.live.is_some() && plan.indexed_code {
            return Err(fail(format!("live patching of {:?} images", output.format)));
        }
        let next = FileName::new(&format!("{}.blaze-next", to.as_str()))
            .ok_or_else(|| fail(format!("{to}: no name beside it to write the next image at")))?;
        let name = to.as_str().rsplit('/').next().unwrap_or(to.as_str()).to_owned();
        Ok(Image {
            to,
            next,
            name,
            output,
            writer,
            plan,
            symbols: Symbols::default(),
            objects: FxHashMap::default(),
            state: State::new(),
            history: VecDeque::new(),
            pending: None,
            tags: 0,
            generations: blaze_live::patch::FIRST_GENERATION,
        })
    }

    /// Places and relocates what `change` says, in parallel, into address space not yet
    /// visible. The image is as it was until the result is committed; a failed stage leaves
    /// nothing behind.
    pub fn stage(&mut self, change: Change<'_>) -> Result<Staged, LinkError> {
        if self.pending.is_some() {
            return Err(LinkError { function: None, what: "a commit is in flight".into() });
        }
        let full = !self.state.laid_out || !self.output.incremental;
        let table = has_table(&self.output, self.plan.indexed_code);
        let mut new = self.state.clone();
        new.generation = self.generations + 1;
        if new.wants_compaction() {
            new.compact();
        }
        let arena = bumpalo::Bump::new();
        let contents = Contents::new(new.piece_count());
        let mut stage = Stage {
            output: &self.output,
            writer: &*self.writer,
            plan: &self.plan,
            symbols: &mut self.symbols,
            objects: &mut self.objects,
            native: native::Pass::default(),
            old: &self.state,
            new,
            name: &self.name,
            arena: &arena,
            contents,
            fresh: Default::default(),
            write: Default::default(),
            changed: Default::default(),
            dirty_cells: Default::default(),
            removed: Vec::new(),
            emptied: Vec::new(),
            touched: Vec::new(),
            counted: Vec::new(),
            full,
            table,
            functions: 0,
            statics: 0,
            owned: Vec::new(),
            scratch: Vec::new(),
            spans_changed: false,
        };
        stage.inputs(&change)?;
        stage.apply(&change)?;
        stage.descriptor();
        stage.resolve()?;
        stage.referrers();
        stage.needs()?;
        stage.sweep();
        stage.dangling()?;
        for _ in 0..2 {
            stage.place()?;
            stage.referrers();
            let before = stage.fresh.len();
            stage.needs()?;
            if stage.fresh.len() == before {
                break;
            }
        }
        if !self.state.laid_out {
            stage.new.lineage = lineage(&stage, &self.to);
        }
        stage.fill_descriptor()?;
        stage.relocate_all()?;
        let base = self.state.generation;
        let finished = stage.finish()?;
        let message = finished.message.map(|it| (base, it));
        Ok(Staged {
            base,
            state: finished.state,
            writes: Writes {
                full: finished.full,
                len: finished.len,
                old_len: finished.old_len,
                writes: finished.writes,
                remove: Vec::new(),
                message: if self.state.laid_out { message } else { None },
            },
            undo: finished.undo,
        })
    }

    /// Makes `staged` the image, all at once, through effects: the new file renamed over the
    /// old, then the message to running programs. Completions go to [`Image::complete`].
    pub fn commit(&mut self, staged: Staged, effects: &mut Vec<Effect>) -> Result<(), LinkError> {
        if self.pending.is_some() {
            return Err(LinkError { function: None, what: "a commit is in flight".into() });
        }
        if staged.base != self.state.generation {
            return Err(LinkError {
                function: None,
                what: format!(
                    "staged on generation {}, and the image is at {}: stage it again",
                    staged.base, self.state.generation
                ),
            });
        }
        let full = staged.writes.full;
        let steps = self.steps(staged.writes)?;
        self.generations = self.generations.max(staged.state.generation);
        let before = std::mem::replace(&mut self.state, staged.state);
        let history = if before.laid_out {
            let len = before.file_len;
            self.remember(Undo { state: before.clone(), full, len, writes: staged.undo });
            History::Pop
        } else {
            History::Keep
        };
        self.begin(steps, (before, history), effects);
        Ok(())
    }

    /// Back to the image before the last commit: a patch that panicked, or one whose borrow
    /// check, run beside it, refused its body. Running programs switch back to its table.
    /// Each call undoes one more commit, so a refusal that comes in after later commits rolls
    /// them back too; their changes are then staged again without the refused body. A commit
    /// in flight must complete first.
    pub fn roll_back(&mut self, effects: &mut Vec<Effect>) -> Result<(), LinkError> {
        if self.pending.is_some() {
            return Err(LinkError { function: None, what: "a commit is in flight".into() });
        }
        let Some(undo) = self.history.back() else {
            return Err(LinkError { function: None, what: "no commit to roll back".into() });
        };
        let from = self.state.generation;
        let to = undo.state.generation;
        let message = self.output.live.as_ref().map(|_| {
            (from, blaze_live::patch::Message::RollBack { id: self.state.lineage, to }.encode())
        });
        let writes = Writes {
            full: undo.full,
            len: undo.len,
            old_len: self.state.file_len,
            writes: file::restore(&undo.state, undo.full, &undo.writes),
            remove: if message.is_some() { vec![to] } else { Vec::new() },
            message,
        };
        let steps = self.steps(writes)?;
        let undo = self.history.pop_back().expect("looked at above");
        let current = std::mem::replace(&mut self.state, undo.state.clone());
        self.begin(steps, (current, History::Push(undo)), effects);
        Ok(())
    }

    pub fn slot(&self, function: &InstanceKey) -> Option<Slot> {
        let sym = self.symbols.find_slot(function)?;
        let (piece, offset) = self.state.home(sym)?;
        let piece = self.state.piece(piece)?;
        if !matches!(self.symbols.key(piece.sym), SymKey::Stubs) {
            return None;
        }
        Some(Slot((offset / table::stub_size(self.output.arch)) as u32))
    }

    /// The generation the image is at: counts commits, never reused.
    pub fn generation(&self) -> u64 {
        self.state.generation
    }
}

/// The further inputs a native input names (a linker script's `INPUT`, `GROUP` objects and
/// archives such as `libc_nonshared.a`), which the caller reads and passes as natives too.
pub fn script_inputs(output: &Output, native: &Native<'_>) -> Result<Vec<String>, LinkError> {
    let writer = model::writer(output)?;
    let file = writer
        .read(native.name, native.bytes, &native.kind)
        .map_err(|what| LinkError { function: None, what: format!("{}: {what}", native.name) })?;
    let mut out = Vec::new();
    if let NativeFile::Shared(shared) = file {
        let mut all = vec![shared];
        while let Some(it) = all.pop() {
            out.extend(it.inputs);
            all.extend(it.more);
        }
    }
    Ok(out)
}

/// Reads `bytes` as a native input of `format` (for a fuzz target): whether it read. A
/// reader returns an error, never panics, on any bytes.
#[cfg(feature = "fuzz")]
pub fn fuzz_read(format: Format, arch: blaze_codegen::Arch, kind: NativeKind, bytes: &[u8]) -> bool {
    let output = Output {
        format,
        arch,
        kind: OutputKind::Executable { entry: "_start".to_owned() },
        interpreter: None,
        platform: (format == Format::MachO).then_some(Platform { platform: 1, minimum: 0x000b_0000, sdk: 0x000b_0000 }),
        incremental: false,
        debug: Debug::None,
        live: None,
        tags: 0,
        stack: None,
        windows_subsystem: false,
    };
    model::writer(&output).is_ok_and(|writer| writer.read("fuzz", bytes, &kind).is_ok())
}

/// The default libraries the objects among `natives` ask for (COFF `/DEFAULTLIB`, Mach-O
/// `LC_LINKER_OPTION`), in order and once each, without those any of them takes out with
/// `/NODEFAULTLIB`: the caller finds them on its library path and passes them as natives too.
/// Archive members are not read: the libraries a member asks for once pulled are not among
/// them.
pub fn default_libraries(output: &Output, natives: &[Native<'_>]) -> Result<Vec<String>, LinkError> {
    let writer = model::writer(output)?;
    let mut out: Vec<String> = Vec::new();
    let mut none = false;
    let mut left_out: Vec<String> = Vec::new();
    for native in natives.iter().filter(|it| matches!(it.kind, NativeKind::Object)) {
        let file = writer
            .read(native.name, native.bytes, &native.kind)
            .map_err(|what| LinkError { function: None, what: format!("{}: {what}", native.name) })?;
        let NativeFile::Object(object) = file else { continue };
        for library in object.libraries {
            if !out.iter().any(|it| it.eq_ignore_ascii_case(&library)) {
                out.push(library);
            }
        }
        for directive in object.directives {
            match directive {
                model::Directive::NoDefaultLib(Some(library)) => left_out.push(library),
                model::Directive::NoDefaultLib(None) => none = true,
                _ => {}
            }
        }
    }
    if none {
        return Ok(Vec::new());
    }
    out.retain(|it| !left_out.iter().any(|gone| gone.eq_ignore_ascii_case(it)));
    Ok(out)
}

/// What an image is across its generations: its first full link's pieces and inputs.
fn lineage(stage: &Stage<'_>, to: &FileName) -> [u8; 16] {
    let mut bytes: Vec<u8> = Vec::new();
    let mut keys: Vec<(u32, u32)> = Vec::with_capacity(stage.new.piece_count() as usize);
    for piece in stage.new.live_pieces() {
        let start = bytes.len();
        stage.symbols.write_key(piece.sym, &mut bytes);
        keys.push((start as u32, (bytes.len() - start) as u32));
    }
    let key = |it: &(u32, u32)| &bytes[it.0 as usize..(it.0 + it.1) as usize];
    keys.sort_unstable_by(|a, b| key(a).cmp(key(b)));
    let mut hasher = Hasher::new("blaze v2 link lineage 2");
    hasher.part(to.as_str().as_bytes()).part(format!("{:?}", stage.output).as_bytes());
    for it in &keys {
        hasher.part(key(it));
    }
    for input in stage.new.inputs.iter() {
        hasher.id(&input.id);
    }
    let id = hasher.finish();
    let mut out = [0; 16];
    out.copy_from_slice(&id.0[..16]);
    out
}
