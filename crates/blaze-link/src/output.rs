//! What an image is linked as: its format, target and kind, and its native inputs.

use blaze_codegen::Arch;
use blaze_core::FileName;
use blaze_ids::ContentId;

/// The object format of an image.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Format {
    /// Linux and the other ELF systems.
    Elf,
    /// macOS.
    MachO,
    /// Windows.
    Pe,
    /// WebAssembly (WASI or none).
    Wasm,
}

/// What an image is linked as.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Output {
    pub format: Format,
    pub arch: Arch,
    pub kind: OutputKind,
    /// ELF: the program interpreter (`/lib64/ld-linux-x86-64.so.2`); none for a static or
    /// shared image.
    pub interpreter: Option<String>,
    /// Mach-O: `LC_BUILD_VERSION`'s platform (1 is macOS) and its minimum OS and SDK
    /// versions, as `xxxx.yy.zz` nibbles.
    pub platform: Option<Platform>,
    /// Room to grow is kept in every region, for incremental links and live patches; a
    /// release link keeps none.
    pub incremental: bool,
    pub debug: Debug,
    /// Where running programs take patches: under the driver's root for the linker, and the
    /// absolute path the program reads it at.
    pub live: Option<Live>,
    /// The high bits of the tags of the image's effects: each is `tags | n` for `n` below
    /// 2^32, so the caller's other effects need other high bits.
    pub tags: u64,
    /// The stack the main thread gets, where the format says it (PE, wasm).
    pub stack: Option<u64>,
    /// PE: `#![windows_subsystem = "windows"]`, a GUI program with no console (rustc's
    /// `/SUBSYSTEM:WINDOWS`). Other formats have no subsystem.
    pub windows_subsystem: bool,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum OutputKind {
    /// An executable entered at the symbol `entry` (`_start` from the C runtime's
    /// `Scrt1.o`; Mach-O's `main`; PE's `mainCRTStartup`).
    Executable { entry: String },
    /// A shared library (`cdylib`, a proc macro) under `name` (its `DT_SONAME`, install
    /// name or DLL name), exporting `exports`.
    Shared { name: String, exports: Vec<String> },
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Platform {
    pub platform: u32,
    pub minimum: u32,
    pub sdk: u32,
}

/// How much debug info the image carries.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Debug {
    None,
    /// Line tables and function names: backtraces and breakpoints by line.
    Lines,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Live {
    pub dir: FileName,
    pub path: String,
}

/// A native input of the link line: a C object, an archive or a shared library.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Native<'a> {
    /// Its content ID: an input changed is one removed and one added.
    pub id: ContentId,
    /// What errors call it: its path as the link line has it.
    pub name: &'a str,
    pub bytes: &'a [u8],
    pub kind: NativeKind,
    /// Its place on the link line: symbols resolve to the first definition in this order,
    /// and an archive's members are taken for what is undefined when it is reached.
    pub order: u32,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NativeKind {
    Object,
    /// Members taken for undefined symbols, or all of them (`--whole-archive`).
    Archive { whole: bool },
    /// A shared library, a linker script naming them, a `.tbd` stub or an import library;
    /// left out of the image's needs when none of its symbols is used and `as_needed`.
    Shared { as_needed: bool },
}
