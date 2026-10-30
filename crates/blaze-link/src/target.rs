//! A target's facts for the link, and where the files of its link line are.
//!
//! [`output`] is rustc's target spec of a triple (the pinned nightly's `rustc_target::spec`) as
//! an [`Output`]: format, architecture, entry, the ELF interpreter, the Mach-O platform and the
//! stack. [`locate`] finds the files of a link line, in its order: the C runtime's start
//! objects, the libraries the crates ask for (`-l`, `#[link]`), the libraries the C toolchain's
//! driver adds, then its end objects.
//!
//! blaze never links with `cc` (docs/linker.md, "The C toolchain stays a source of files, not
//! a linker"): the platform's C toolchain is asked where its files are, never to compile or
//! link.
//!
//! - Linux: `$CC_<triple with _>`, else `$CC`, else `cc`, with `-print-search-dirs` and
//!   `-print-file-name=<file>`, all at once: distro GCC and Clang and nix's cc-wrapper answer
//!   alike. The interpreter is the one that toolchain's `cc` passes: its own libc's loader
//!   where that libc is outside the FHS directories (nix), and the glibc ABI's path otherwise.
//! - macOS: the SDK at `$SDKROOT`, else `xcrun --sdk macosx --show-sdk-path`: its `.tbd`s and
//!   frameworks, and its version from `SDKSettings.json`. The minimum OS is
//!   `$MACOSX_DEPLOYMENT_TARGET`, at least rustc's default for the triple.
//! - Windows (MSVC): the directories of `$LIB`; without it, the MSVC tools vswhere names and
//!   the newest Windows SDK the registry names, as `find_msvc_tools` finds them.
//! - wasm32-wasip1: wasi-libc's `crt1-command.o` and `libc.a`, in blaze's toolchain's
//!   self-contained directory (the `sysroot` [`locate`] is given).
//!
//! What the toolchain says is asked once per process for each triple and each value of the
//! environment variables read, and kept. Each file found is still hashed by the caller, so a
//! changed file relinks; a toolchain whose directory layout changed (a compiler upgraded, an
//! SDK moved) is seen only by a daemon started after the change.
//!
//! A library is searched for as rustc passes `-l` and the system linker finds it: the link's
//! own directories first, then the toolchain's, and in each directory the files the kind
//! allows, in the linker's order (`lib{name}.so` before `lib{name}.a` for a `dylib` on ELF;
//! `.tbd`, `.dylib`, `.so`, `.a` on Mach-O; `{name}.lib` on PE; `lib{name}.a` on wasm). A
//! GNU linker script found as a shared library (glibc's `libc.so`) is followed on the line by
//! the files it names ([`crate::script_inputs`]), which the engine wants there. The engine
//! resolves archives wherever they are on the line, so each file is on it once.

#[cfg(test)]
mod tests;

use std::fmt::Write as _;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, PoisonError};

use blaze_codegen::{Arch, NativeLib, NativeLibKind};
use blaze_ids::ContentId;
use smallvec::SmallVec;

use crate::{Debug, Format, LinkError, Native, NativeKind, Output, OutputKind, Platform};

/// A file of the link line, as [`locate`] found it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Located {
    pub path: PathBuf,
    /// Its path as the link line has it: what errors call it, and what a linker script that
    /// names it is matched against.
    pub name: String,
    pub kind: NativeKind,
}

/// The main thread's stack where the format says it: link.exe's default, which rustc keeps,
/// and wasm-ld's `-z stack-size` as rustc passes it.
const STACK: u64 = 1 << 20;

/// `LC_BUILD_VERSION`'s platform of macOS.
const MACOS: u32 = 1;

/// rustc's minimum macOS for aarch64-apple-darwin, 11.0.0, as `xxxx.yy.zz` nibbles.
const MACOS_MINIMUM: u32 = 0x000b_0000;

/// The domain a linker script's bytes are named in for the engine's reader.
const SCRIPT: &str = "blaze v2 link target script";

const LINKED: &str = "x86_64-unknown-linux-gnu, aarch64-unknown-linux-gnu, aarch64-apple-darwin, \
                      x86_64-pc-windows-msvc, wasm32-wasip1 and wasm32-unknown-unknown";

/// The process's answers of the machine's toolchains.
static CACHE: Cache = Cache::new();

/// How `triple` links: an executable, or the shared library `shared` names (its file name and
/// exports) for a `cdylib` or proc macro. `incremental` keeps room to grow in the image.
pub fn output(
    triple: &str,
    shared: Option<(String, Vec<String>)>,
    debug: Debug,
    incremental: bool,
) -> Result<Output, LinkError> {
    output_with(&CACHE, &System, triple, shared, debug, incremental)
}

/// The files of `output`'s link line, in its order: the C runtime's start objects, `libs`
/// (searched in `search`, the link's `-L`s, then the toolchain's directories), the libraries
/// the toolchain's driver adds, the C runtime's end objects. A linker script's files follow
/// it. A library not found is an error naming it and every directory searched. `sysroot` is
/// blaze's toolchain (`blaze toolchain --path`), whose self-contained C runtime WASI links.
pub fn locate(
    output: &Output,
    libs: &[NativeLib],
    search: &[PathBuf],
    sysroot: &Path,
) -> Result<Vec<Located>, LinkError> {
    locate_with(&CACHE, &System, output, libs, search, sysroot)
}

fn output_with<A: Ask>(
    cache: &Cache,
    ask: &A,
    triple: &str,
    shared: Option<(String, Vec<String>)>,
    debug: Debug,
    incremental: bool,
) -> Result<Output, LinkError> {
    let target = Target::parse(triple)?;
    let kind = match shared {
        Some((name, exports)) => OutputKind::Shared { name, exports },
        None if target == Target::WasmUnknown => {
            return Err(fail(
                "an executable for wasm32-unknown-unknown: rustc links it `--no-entry`, its C-level \
                 symbols exported, and an executable `Output` has an entry; build it as a `cdylib`"
                    // alloc: error path
                    .to_owned(),
            ));
        }
        // alloc: once per link, the entry's name
        None => OutputKind::Executable { entry: target.entry().to_owned() },
    };
    let executable = matches!(kind, OutputKind::Executable { .. });
    let interpreter = match target {
        // clone: once per link, the path kept for the target
        Target::LinuxX86_64 | Target::LinuxAarch64 if executable => cache.found(ask, target, None)?.interpreter.clone(),
        _ => None,
    };
    let platform = match target {
        Target::MacOs => Some(Platform { platform: MACOS, minimum: minimum(ask)?, sdk: cache.found(ask, target, None)?.sdk }),
        _ => None,
    };
    Ok(Output {
        format: target.format(),
        arch: target.arch(),
        kind,
        interpreter,
        platform,
        incremental,
        debug,
        live: None,
        tags: 0,
        stack: target.stack(),
        windows_subsystem: false,
        dev_run: None,
        // alloc: once per link, the triple the output is for
        triple: triple.to_owned(),
    })
}

fn locate_with<A: Ask>(
    cache: &Cache,
    ask: &A,
    output: &Output,
    libs: &[NativeLib],
    search: &[PathBuf],
    sysroot: &Path,
) -> Result<Vec<Located>, LinkError> {
    let target = Target::parse(&output.triple)?;
    if (output.format, output.arch) != (target.format(), target.arch()) {
        return Err(fail(
            // alloc: error path
            format!(
                "an output of {:?} for {:?} says it is for {}, which is {:?} for {:?}",
                output.format,
                output.arch,
                output.triple,
                target.format(),
                target.arch()
            ),
        ));
    }
    let found = cache.found(ask, target, Some(sysroot))?;
    let executable = matches!(output.kind, OutputKind::Executable { .. });
    let mut line = Line { output, search, found: &found, out: Vec::with_capacity(libs.len() + 8) };
    for name in target.start(executable) {
        line.object(name)?;
    }
    for lib in libs {
        line.library(&lib.name, lib.kind, lib.verbatim)?;
    }
    for &(name, kind) in target.late() {
        line.library(name, kind, false)?;
    }
    for name in target.end() {
        line.object(name)?;
    }
    Ok(line.out)
}

/// The targets blaze links: those `hir_ty_codegen::v2::toolchain` compiles for whose link
/// rustc's spec says.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Target {
    LinuxX86_64,
    LinuxAarch64,
    /// aarch64-apple-darwin.
    MacOs,
    /// x86_64-pc-windows-msvc.
    Msvc,
    Wasip1,
    WasmUnknown,
}

impl Target {
    fn parse(triple: &str) -> Result<Target, LinkError> {
        Ok(match triple {
            "x86_64-unknown-linux-gnu" => Target::LinuxX86_64,
            "aarch64-unknown-linux-gnu" => Target::LinuxAarch64,
            "aarch64-apple-darwin" => Target::MacOs,
            "x86_64-pc-windows-msvc" => Target::Msvc,
            "wasm32-wasip1" => Target::Wasip1,
            "wasm32-unknown-unknown" => Target::WasmUnknown,
            "wasm32-wasip2" => {
                return Err(fail(
                    "wasm32-wasip2: rustc links a component for it (wasm-component-ld), and \
                     blaze's wasm writer makes core modules"
                        // alloc: error path
                        .to_owned(),
                ));
            }
            // alloc: error path
            _ => return Err(fail(format!("blaze links no {triple}: it links {LINKED}"))),
        })
    }

    fn triple(self) -> &'static str {
        match self {
            Target::LinuxX86_64 => "x86_64-unknown-linux-gnu",
            Target::LinuxAarch64 => "aarch64-unknown-linux-gnu",
            Target::MacOs => "aarch64-apple-darwin",
            Target::Msvc => "x86_64-pc-windows-msvc",
            Target::Wasip1 => "wasm32-wasip1",
            Target::WasmUnknown => "wasm32-unknown-unknown",
        }
    }

    fn format(self) -> Format {
        match self {
            Target::LinuxX86_64 | Target::LinuxAarch64 => Format::Elf,
            Target::MacOs => Format::MachO,
            Target::Msvc => Format::Pe,
            Target::Wasip1 | Target::WasmUnknown => Format::Wasm,
        }
    }

    fn arch(self) -> Arch {
        match self {
            Target::LinuxX86_64 | Target::Msvc => Arch::X86_64,
            Target::LinuxAarch64 | Target::MacOs => Arch::Aarch64,
            Target::Wasip1 | Target::WasmUnknown => Arch::Wasm32,
        }
    }

    /// The executable's entry: the C runtime's `_start` (`Scrt1.o`, wasi-libc's
    /// `crt1-command.o`), dyld's `main` (the writer adds Mach-O's `_`), the MSVC CRT's
    /// `mainCRTStartup`.
    fn entry(self) -> &'static str {
        match self.format() {
            Format::Elf | Format::Wasm => "_start",
            Format::MachO => "main",
            Format::Pe => "mainCRTStartup",
        }
    }

    fn stack(self) -> Option<u64> {
        match self.format() {
            Format::Pe | Format::Wasm => Some(STACK),
            Format::Elf | Format::MachO => None,
        }
    }

    /// The environment the toolchain is found from, in [`Key::vars`]'s order. `PATH` is read
    /// by the commands asked.
    fn vars(self) -> &'static [&'static str] {
        match self {
            Target::LinuxX86_64 => &["CC_x86_64_unknown_linux_gnu", "CC", "PATH"],
            Target::LinuxAarch64 => &["CC_aarch64_unknown_linux_gnu", "CC", "PATH"],
            Target::MacOs => &["SDKROOT", "DEVELOPER_DIR", "PATH"],
            Target::Msvc => &["LIB", "ProgramFiles(x86)", "PATH"],
            Target::Wasip1 | Target::WasmUnknown => &[],
        }
    }

    /// The C runtime's objects first on the line: glibc's as the system `cc` adds them for a
    /// PIE (rustc's glibc specs leave them to it), wasi-libc's command start as rustc's
    /// `pre_link_objects_self_contained` has it (none for a `cdylib`).
    fn start(self, executable: bool) -> &'static [&'static str] {
        match (self, executable) {
            (Target::LinuxX86_64 | Target::LinuxAarch64, true) => &["Scrt1.o", "crti.o", "crtbeginS.o"],
            (Target::LinuxX86_64 | Target::LinuxAarch64, false) => &["crti.o", "crtbeginS.o"],
            (Target::Wasip1, true) => &["crt1-command.o"],
            _ => &[],
        }
    }

    /// The C runtime's objects last on the line.
    fn end(self) -> &'static [&'static str] {
        match self {
            Target::LinuxX86_64 | Target::LinuxAarch64 => &["crtendS.o", "crtn.o"],
            _ => &[],
        }
    }

    /// The libraries after the crates': what the system `cc` adds (`-lgcc -lgcc_s -lc` for
    /// glibc, `-lSystem` on macOS) and wasi-libc. MSVC's CRT comes in by the `/DEFAULTLIB`s of
    /// what links it, and rustc's spec adds nothing.
    fn late(self) -> &'static [(&'static str, NativeLibKind)] {
        const STATIC: NativeLibKind = NativeLibKind::Static { whole: false, bundle: true };
        match self {
            Target::LinuxX86_64 | Target::LinuxAarch64 => {
                &[("gcc", STATIC), ("gcc_s", NativeLibKind::Dylib), ("c", NativeLibKind::Dylib)]
            }
            Target::MacOs => &[("System", NativeLibKind::Dylib)],
            Target::Wasip1 => &[("c", STATIC)],
            Target::Msvc | Target::WasmUnknown => &[],
        }
    }
}

/// `$MACOSX_DEPLOYMENT_TARGET`, at least rustc's minimum, as rustc takes it.
fn minimum<A: Ask>(ask: &A) -> Result<u32, LinkError> {
    let Some(given) = ask.var("MACOSX_DEPLOYMENT_TARGET")? else { return Ok(MACOS_MINIMUM) };
    let version = parse_version(&given).ok_or_else(|| {
        // alloc: error path
        fail(format!("MACOSX_DEPLOYMENT_TARGET={given}: not a version (`major[.minor[.patch]]`)"))
    })?;
    Ok(version.max(MACOS_MINIMUM))
}

/// `major[.minor[.patch]]` as `xxxx.yy.zz` nibbles.
fn parse_version(text: &str) -> Option<u32> {
    let mut parts = text.trim().split('.');
    let part = |it: Option<&str>, max: u32| -> Option<u32> {
        match it {
            None => Some(0),
            Some(it) if it.bytes().all(|b| b.is_ascii_digit()) => it.parse().ok().filter(|it| *it <= max),
            Some(_) => None,
        }
    };
    let major = part(Some(parts.next()?), 0xffff)?;
    let minor = part(parts.next(), 0xff)?;
    let patch = part(parts.next(), 0xff)?;
    if parts.next().is_some() {
        return None;
    }
    Some(major << 16 | minor << 8 | patch)
}

/// The string value of `key` in the outermost object of the JSON `text`, unescaped as it is
/// written; `None` when it has none or is not well formed there.
fn top_level_string<'t>(text: &'t str, key: &str) -> Option<&'t str> {
    let bytes = text.as_bytes();
    let mut depth = 0u32;
    let mut at = 0;
    while at < bytes.len() {
        match bytes[at] {
            b'{' | b'[' => {
                depth += 1;
                at += 1;
            }
            b'}' | b']' => {
                depth = depth.checked_sub(1)?;
                at += 1;
            }
            b'"' => {
                let (string, end) = string_at(text, at)?;
                at = end;
                if depth != 1 || string != key {
                    continue;
                }
                let Some(value) = text[at..].trim_start().strip_prefix(':') else { continue };
                let value = value.trim_start();
                if !value.starts_with('"') {
                    return None;
                }
                return string_at(text, text.len() - value.len()).map(|it| it.0);
            }
            _ => at += 1,
        }
    }
    None
}

/// The JSON string whose `"` is at `at`, and where it ends.
fn string_at(text: &str, at: usize) -> Option<(&str, usize)> {
    let bytes = text.as_bytes();
    let mut i = at + 1;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            b'"' => return Some((&text[at + 1..i], i + 1)),
            _ => i += 1,
        }
    }
    None
}

/// What the toolchain is found from: the environment, and the answers of the commands that
/// say where its files are. [`System`] asks the machine; tests answer.
pub(crate) trait Ask {
    /// The variable `name`; an error for one that is not UTF-8.
    fn var(&self, name: &str) -> Result<Option<String>, LinkError>;
    /// `program` run once per argument list, all at once: each one's standard output, or why
    /// it failed.
    fn run(&self, program: &Path, runs: &[Vec<String>]) -> Vec<Result<String, String>>;
}

/// The machine: its environment and its commands.
pub(crate) struct System;

impl Ask for System {
    fn var(&self, name: &str) -> Result<Option<String>, LinkError> {
        match std::env::var_os(name) {
            None => Ok(None),
            Some(value) => value
                .into_string()
                .map(Some)
                // alloc: error path
                .map_err(|value| fail(format!("{name}={}: not UTF-8", value.to_string_lossy()))),
        }
    }

    fn run(&self, program: &Path, runs: &[Vec<String>]) -> Vec<Result<String, String>> {
        let mut children = Vec::with_capacity(runs.len());
        for args in runs {
            children.push(
                Command::new(program)
                    .args(args)
                    .stdin(Stdio::null())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn(),
            );
        }
        let mut answers = Vec::with_capacity(runs.len());
        for child in children {
            answers.push((|| {
                // alloc: error path
                let done = child.map_err(|error| error.to_string())?.wait_with_output().map_err(|error| error.to_string())?;
                if !done.status.success() {
                    // alloc: error path
                    return Err(format!("{}: {}", done.status, String::from_utf8_lossy(&done.stderr).trim()));
                }
                // alloc: error path
                String::from_utf8(done.stdout).map_err(|_| "it printed what is not UTF-8".to_owned())
            })());
        }
        answers
    }
}

/// What a toolchain was found from: the target, each of [`Target::vars`] (an empty one is
/// unset) and, for WASI, blaze's toolchain.
#[derive(PartialEq, Eq, Debug)]
struct Key {
    target: Target,
    vars: Vec<Option<String>>,
    sysroot: Option<PathBuf>,
}

impl Key {
    fn of<A: Ask>(ask: &A, target: Target, sysroot: Option<&Path>) -> Result<Key, LinkError> {
        let names = target.vars();
        let mut vars = Vec::with_capacity(names.len());
        for name in names {
            vars.push(ask.var(name)?.filter(|it| !it.is_empty()));
        }
        let sysroot = match target {
            Target::Wasip1 => sysroot.map(Path::to_path_buf),
            _ => None,
        };
        Ok(Key { target, vars, sysroot })
    }

    fn var(&self, at: usize) -> Option<&str> {
        self.vars[at].as_deref()
    }
}

/// The toolchains found, by what each was found from: a few per process, searched in order.
pub(crate) struct Cache(Mutex<Vec<(Key, Arc<Found>)>>);

impl Cache {
    pub(crate) const fn new() -> Cache {
        // alloc: none, an empty `Vec` holds no buffer
        Cache(Mutex::new(Vec::new()))
    }

    /// The toolchain of `target` as the environment is now, with blaze's toolchain at
    /// `sysroot` for WASI: asked once, then kept. The lock is held while it is asked, so no
    /// two links ask the same thing.
    fn found<A: Ask>(&self, ask: &A, target: Target, sysroot: Option<&Path>) -> Result<Arc<Found>, LinkError> {
        let key = Key::of(ask, target, sysroot)?;
        // Only a whole answer is pushed: one a panic interrupted left nothing.
        let mut kept = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some((_, found)) = kept.iter().find(|(it, _)| *it == key) {
            return Ok(Arc::clone(found));
        }
        // alloc: once per process and toolchain, its answers
        let found = Arc::new(query(ask, &key)?);
        kept.push((key, Arc::clone(&found)));
        Ok(found)
    }
}

/// A target's toolchain: where its files are.
#[derive(Default, Debug)]
struct Found {
    /// Searched after the link's own directories, in order.
    dirs: Vec<PathBuf>,
    /// Apple's: where frameworks are, after the link's own directories.
    frameworks: Vec<PathBuf>,
    /// The C runtime's start and end objects, by file name.
    objects: Vec<(&'static str, PathBuf)>,
    /// ELF: an executable's interpreter.
    interpreter: Option<String>,
    /// Mach-O: the SDK's version, as `xxxx.yy.zz` nibbles.
    sdk: u32,
}

impl Found {
    fn object(&self, name: &str) -> Result<&Path, LinkError> {
        self.objects
            .iter()
            .find(|it| it.0 == name)
            .map(|it| it.1.as_path())
            // alloc: error path
            .ok_or_else(|| fail(format!("the C runtime's {name} is not among the toolchain's files")))
    }
}

fn query<A: Ask>(ask: &A, key: &Key) -> Result<Found, LinkError> {
    match key.target {
        Target::LinuxX86_64 | Target::LinuxAarch64 => linux(ask, key),
        Target::MacOs => apple(ask, key),
        Target::Msvc => msvc(ask, key),
        Target::Wasip1 => wasi(key),
        Target::WasmUnknown => Ok(Found::default()),
    }
}

/// glibc's loader for `target`: its file name, where the glibc ABI puts it, and the
/// architecture's multiarch directory name.
fn loader(target: Target) -> (&'static str, &'static str, &'static str) {
    match target {
        Target::LinuxAarch64 => ("ld-linux-aarch64.so.1", "/lib/ld-linux-aarch64.so.1", "aarch64-linux-gnu"),
        _ => ("ld-linux-x86-64.so.2", "/lib64/ld-linux-x86-64.so.2", "x86_64-linux-gnu"),
    }
}

/// Whether `dir` is where an FHS system keeps its libc (`/usr/lib/x86_64-linux-gnu`,
/// `/usr/lib64`, ...), whose loader is at the glibc ABI's path.
fn fhs(dir: &Path, multiarch: &str) -> bool {
    let Some(dir) = dir.to_str() else { return false };
    let dir = dir.trim_end_matches('/');
    matches!(dir, "/lib" | "/lib64" | "/usr/lib" | "/usr/lib64")
        || dir.strip_prefix("/lib/").or_else(|| dir.strip_prefix("/usr/lib/")) == Some(multiarch)
}

/// The C toolchain for a Linux target, asked where its directories and its C runtime's files
/// are.
fn linux<A: Ask>(ask: &A, key: &Key) -> Result<Found, LinkError> {
    let triple = key.target.triple();
    let names = key.target.vars();
    let (cc, said) = match (key.var(0), key.var(1)) {
        // alloc: once per toolchain asked, what its errors say of it
        (Some(cc), _) => (cc, format!("`{cc}`, from {}", names[0])),
        // alloc: once per toolchain asked, what its errors say of it
        (None, Some(cc)) => (cc, format!("`{cc}`, from CC")),
        // alloc: once per toolchain asked, what its errors say of it
        (None, None) => ("cc", format!("`cc`, as neither {} nor CC is set", names[0])),
    };
    let mut words = cc.split_whitespace();
    // alloc: error path
    let program = words.next().ok_or_else(|| fail(format!("the C toolchain for {triple}: {said} names no program")))?;
    // alloc: none up to four words, which the `SmallVec` holds in place
    let given: SmallVec<[&str; 4]> = words.collect();
    let (loader, abi, multiarch) = loader(key.target);
    let files = ["Scrt1.o", "crti.o", "crtbeginS.o", "crtendS.o", "crtn.o", "libc.so.6", loader];
    let args = |last: String| -> Vec<String> {
        let mut args = Vec::with_capacity(given.len() + 1);
        // alloc: once per toolchain asked, its commands
        args.extend(given.iter().map(|it| (*it).to_owned()));
        args.push(last);
        args
    };
    let mut runs = Vec::with_capacity(files.len() + 1);
    // alloc: once per toolchain asked, its commands
    runs.push(args("-print-search-dirs".to_owned()));
    for file in files {
        // alloc: once per toolchain asked, its commands
        runs.push(args(format!("-print-file-name={file}")));
    }
    // alloc: error path
    let shown = |at: usize| format!("`{program} {}`", runs[at].join(" "));
    let mut answers = ask.run(Path::new(program), &runs).into_iter();
    let mut answer = |at: usize| -> Result<String, LinkError> {
        answers
            .next()
            // alloc: error path
            .unwrap_or_else(|| Err("no answer".to_owned()))
            // alloc: error path
            .map_err(|why| fail(format!("{}: {why}; the C toolchain for {triple} is {said}", shown(at))))
    };

    let printed = answer(0)?;
    let listed = printed
        .lines()
        .find_map(|line| line.strip_prefix("libraries:"))
        // alloc: error path
        .ok_or_else(|| fail(format!("{} printed no `libraries:` line", shown(0))))?;
    // alloc: once per toolchain asked, its directories
    let mut dirs: Vec<PathBuf> = Vec::new();
    for dir in listed.trim().trim_start_matches('=').split(':').filter(|it| !it.is_empty()) {
        let dir = PathBuf::from(dir);
        if dir.is_dir() && !dirs.contains(&dir) {
            dirs.push(dir);
        }
    }
    let mut objects = Vec::with_capacity(5);
    let mut libc = PathBuf::new();
    let mut found_loader = PathBuf::new();
    for (at, file) in files.into_iter().enumerate() {
        let printed = answer(at + 1)?;
        let path = PathBuf::from(printed.trim());
        if !path.is_absolute() || !path.is_file() {
            return Err(fail(
                // alloc: error path
                format!(
                    "{} did not find {file}: the C toolchain for {triple} is {said}, and it has no {file}",
                    shown(at + 1)
                ),
            ));
        }
        match at {
            5 => libc = path,
            6 => found_loader = path,
            _ => objects.push((file, path)),
        }
    }
    // alloc: error path
    let real = std::fs::canonicalize(&libc).map_err(|error| fail(format!("{}: {error}", libc.display())))?;
    let interpreter = match real.parent().is_some_and(|it| fhs(it, multiarch)) {
        // alloc: once per toolchain asked, the interpreter's path
        true => abi.to_owned(),
        false => found_loader
            .to_str()
            .ok_or_else(|| {
                // alloc: error path
                fail(format!("{}: an interpreter's path that is not UTF-8", found_loader.display()))
            })?
            // alloc: once per toolchain asked, the interpreter's path
            .to_owned(),
    };
    Ok(Found { dirs, objects, interpreter: Some(interpreter), ..Found::default() })
}

/// The macOS SDK: its library and framework directories and its version.
fn apple<A: Ask>(ask: &A, key: &Key) -> Result<Found, LinkError> {
    let sdk = match key.var(0) {
        Some(root) => PathBuf::from(root),
        None => {
            // alloc: once per toolchain asked, its command
            let run = ["--sdk", "macosx", "--show-sdk-path"].map(str::to_owned).to_vec();
            let printed = ask
                .run(Path::new("xcrun"), std::slice::from_ref(&run))
                .pop()
                // alloc: error path
                .unwrap_or_else(|| Err("no answer".to_owned()))
                .map_err(|why| {
                    // alloc: error path
                    fail(format!("the macOS SDK: SDKROOT is not set, and `xcrun --sdk macosx --show-sdk-path`: {why}"))
                })?;
            PathBuf::from(printed.trim())
        }
    };
    let settings = sdk.join("SDKSettings.json");
    let text = std::fs::read_to_string(&settings)
        // alloc: error path
        .map_err(|error| fail(format!("the macOS SDK's version: {}: {error}", settings.display())))?;
    let version = top_level_string(&text, "Version").and_then(parse_version).ok_or_else(|| {
        // alloc: error path
        fail(format!("{}: no `Version` that is `major[.minor[.patch]]`", settings.display()))
    })?;
    Ok(Found {
        // alloc: once per toolchain asked, its directories
        dirs: vec![sdk.join("usr/lib"), sdk.join("usr/local/lib")],
        // alloc: once per toolchain asked, its directories
        frameworks: vec![sdk.join("Library/Frameworks"), sdk.join("System/Library/Frameworks")],
        sdk: version,
        ..Found::default()
    })
}

/// The MSVC and Windows SDK library directories.
fn msvc<A: Ask>(ask: &A, key: &Key) -> Result<Found, LinkError> {
    if let Some(lib) = key.var(0) {
        // alloc: once per toolchain asked, its directories
        let dirs = lib.split(';').filter(|it| !it.is_empty()).map(PathBuf::from).collect();
        return Ok(Found { dirs, ..Found::default() });
    }
    let missing = |what: String| {
        // alloc: error path
        fail(format!(
            "the MSVC and Windows SDK libraries for x86_64-pc-windows-msvc: LIB is not set (a \
             Developer Command Prompt sets it), and {what}"
        ))
    };
    let Some(programs) = key.var(1) else {
        // alloc: error path
        return Err(missing("ProgramFiles(x86), where vswhere is, is not set".to_owned()));
    };
    let vswhere = Path::new(programs).join("Microsoft Visual Studio").join("Installer").join("vswhere.exe");
    let run = [
        "-latest",
        "-products",
        "*",
        "-requires",
        "Microsoft.VisualStudio.Component.VC.Tools.x86.x64",
        "-property",
        "installationPath",
    ]
    .map(str::to_owned)
    // alloc: once per toolchain asked, its command
    .to_vec();
    let asked = |program: &Path, run: &Vec<String>| -> Result<String, LinkError> {
        ask.run(program, std::slice::from_ref(run))
            .pop()
            // alloc: error path
            .unwrap_or_else(|| Err("no answer".to_owned()))
            // alloc: error path
            .map_err(|why| missing(format!("`{} {}`: {why}", program.display(), run.join(" "))))
    };
    let install = asked(&vswhere, &run)?;
    let install = install.trim();
    if install.is_empty() {
        // alloc: error path
        return Err(missing(format!("{} names no Visual Studio with the MSVC x64 tools", vswhere.display())));
    }
    let install = Path::new(install);
    let default = install.join("VC").join("Auxiliary").join("Build").join("Microsoft.VCToolsVersion.default.txt");
    let version = std::fs::read_to_string(&default)
        // alloc: error path
        .map_err(|error| missing(format!("{}: {error}", default.display())))?;
    let tools = install.join("VC").join("Tools").join("MSVC").join(version.trim()).join("lib").join("x64");
    if !tools.is_dir() {
        // alloc: error path
        return Err(missing(format!("{} is not a directory", tools.display())));
    }
    let run = ["query", r"HKLM\SOFTWARE\Microsoft\Windows Kits\Installed Roots", "/v", "KitsRoot10", "/reg:32"]
        .map(str::to_owned)
        // alloc: once per toolchain asked, its command
        .to_vec();
    let printed = asked(Path::new("reg"), &run)?;
    let root = printed
        .lines()
        .find_map(|line| line.trim_start().strip_prefix("KitsRoot10"))
        .and_then(|rest| rest.trim_start().strip_prefix("REG_SZ"))
        .map(str::trim)
        .filter(|it| !it.is_empty())
        // alloc: error path
        .ok_or_else(|| missing("the registry names no Windows 10 SDK (KitsRoot10)".to_owned()))?;
    let lib = Path::new(root).join("Lib");
    let kit = newest_kit(&lib)
        // alloc: error path
        .map_err(|error| missing(format!("{}: {error}", lib.display())))?
        // alloc: error path
        .ok_or_else(|| missing(format!("{} has no SDK with um\\x64\\kernel32.lib", lib.display())))?;
    let kit = lib.join(kit);
    Ok(Found {
        // alloc: once per toolchain asked, its directories
        dirs: vec![tools, kit.join("ucrt").join("x64"), kit.join("um").join("x64")],
        ..Found::default()
    })
}

/// The newest Windows SDK under `lib` (`10.0.22621.0`) that has the x64 libraries.
fn newest_kit(lib: &Path) -> std::io::Result<Option<String>> {
    let mut newest: Option<(SmallVec<[u32; 4]>, String)> = None;
    for entry in std::fs::read_dir(lib)? {
        let entry = entry?;
        let Ok(name) = entry.file_name().into_string() else { continue };
        let Some(version) = name.split('.').map(|it| it.parse::<u32>().ok()).collect::<Option<SmallVec<[u32; 4]>>>()
        else {
            continue;
        };
        if !entry.path().join("um").join("x64").join("kernel32.lib").is_file() {
            continue;
        }
        if newest.as_ref().is_none_or(|(it, _)| version > *it) {
            newest = Some((version, name));
        }
    }
    Ok(newest.map(|it| it.1))
}

/// wasi-libc in blaze's toolchain.
fn wasi(key: &Key) -> Result<Found, LinkError> {
    let Some(sysroot) = &key.sysroot else {
        return Err(fail(
            "wasm32-wasip1's C runtime (wasi-libc) is in blaze's toolchain, and its files were \
             asked for without it: only `locate`, which is given the sysroot, asks for them"
                // alloc: error path
                .to_owned(),
        ));
    };
    let dir = sysroot.join("lib").join("rustlib").join("wasm32-wasip1").join("lib").join("self-contained");
    let start = dir.join("crt1-command.o");
    if !start.is_file() {
        return Err(fail(
            // alloc: error path
            format!(
                "{} is not there: blaze's toolchain at {} has no wasi-libc for wasm32-wasip1",
                start.display(),
                sysroot.display()
            ),
        ));
    }
    // alloc: once per toolchain asked, its files
    Ok(Found { dirs: vec![dir], objects: vec![("crt1-command.o", start)], ..Found::default() })
}

/// A link line as it is found.
struct Line<'a> {
    output: &'a Output,
    search: &'a [PathBuf],
    found: &'a Found,
    out: Vec<Located>,
}

impl Line<'_> {
    /// Where a library is searched for: the link's directories, then the toolchain's.
    fn dirs(&self, framework: bool) -> impl Iterator<Item = &Path> {
        let theirs = match framework {
            true => &self.found.frameworks,
            false => &self.found.dirs,
        };
        self.search.iter().chain(theirs).map(PathBuf::as_path)
    }

    fn object(&mut self, name: &str) -> Result<(), LinkError> {
        let path = self.found.object(name)?.to_path_buf();
        self.push(path, NativeKind::Object)
    }

    fn library(&mut self, name: &str, kind: NativeLibKind, verbatim: bool) -> Result<(), LinkError> {
        let (path, kind) = self.find(name, kind, verbatim)?;
        self.push(path, kind)
    }

    /// The first file in the first directory that has one of those `kind` allows.
    fn find(&self, name: &str, kind: NativeLibKind, verbatim: bool) -> Result<(PathBuf, NativeKind), LinkError> {
        if name.is_empty() {
            // alloc: error path
            return Err(fail("a native library with no name".to_owned()));
        }
        let files = files(self.output.format, name, kind, verbatim)?;
        let framework = matches!(kind, NativeLibKind::Framework);
        for dir in self.dirs(framework) {
            for (file, kind) in &files {
                let path = dir.join(file);
                if path.is_file() {
                    return Ok((path, *kind));
                }
            }
        }
        Err(not_found(name, &files, self.dirs(framework)))
    }

    /// `path` on the line, unless it is already, and the files it names when it is a linker
    /// script.
    fn push(&mut self, path: PathBuf, kind: NativeKind) -> Result<(), LinkError> {
        if self.out.iter().any(|it| it.path == path) {
            return Ok(());
        }
        let script =
            self.output.format == Format::Elf && matches!(kind, NativeKind::Shared { .. }) && sniff(&path)? == Sniffed::Text;
        // alloc: once per file of the line, the name the engine knows it by
        let name = path.to_string_lossy().into_owned();
        self.out.push(Located { path, name, kind });
        if script {
            self.follow(self.out.len() - 1)?;
        }
        Ok(())
    }

    /// The files the linker script at `at` names, after it on the line: an absolute path as it
    /// is, another name searched for as a file in the line's directories, `-lNAME` as a
    /// `dylib` is.
    fn follow(&mut self, at: usize) -> Result<(), LinkError> {
        let script = &self.out[at];
        let bytes: Arc<[u8]> = std::fs::read(&script.path)
            // alloc: error path
            .map_err(|error| fail(format!("{}: {error}", script.name)))?
            .into();
        // clone: once per linker script, its name while the line grows
        let name = script.name.clone();
        let native = Native { id: ContentId::of(SCRIPT, &bytes), name: &name, bytes: &bytes, kind: script.kind, order: at as u32 };
        for input in crate::script_inputs(self.output, &native)? {
            let path = match input.strip_prefix("-l") {
                Some(lib) => {
                    let (path, kind) = self.find(lib, NativeLibKind::Dylib, false)?;
                    self.push(path, kind)?;
                    continue;
                }
                None if Path::new(&input).is_absolute() => PathBuf::from(&input),
                None => self.find(&input, NativeLibKind::Dylib, true)?.0,
            };
            if !path.is_file() {
                // alloc: error path
                return Err(fail(format!("{name} names {input}, which is not a file")));
            }
            let kind = match sniff(&path)? {
                Sniffed::Archive => NativeKind::Archive { whole: false },
                Sniffed::Object => NativeKind::Object,
                Sniffed::Shared | Sniffed::Text => NativeKind::Shared { as_needed: true },
            };
            self.push(path, kind)?;
        }
        Ok(())
    }
}

/// The file names a library may have, in the order the linker tries them in a directory, and
/// what each is read as. ELF's shared libraries are `--as-needed`, as rustc links them.
fn files(
    format: Format,
    name: &str,
    kind: NativeLibKind,
    verbatim: bool,
) -> Result<SmallVec<[(String, NativeKind); 4]>, LinkError> {
    let shared = NativeKind::Shared { as_needed: format == Format::Elf };
    let mut files = SmallVec::new();
    match kind {
        NativeLibKind::Framework if format != Format::MachO => {
            // alloc: error path
            return Err(fail(format!("the framework `{name}`: frameworks are Apple's, and this links {format:?}")));
        }
        NativeLibKind::Framework => {
            // alloc: once per library searched, its file names
            files.push((format!("{name}.framework/{name}.tbd"), shared));
            // alloc: once per library searched, its file names
            files.push((format!("{name}.framework/{name}"), shared));
        }
        // alloc: once per library searched, its file name
        _ if verbatim => files.push((name.to_owned(), by_extension(format, name, kind))),
        NativeLibKind::Static { whole, .. } => {
            let archive = NativeKind::Archive { whole };
            match format {
                // alloc: once per library searched, its file name
                Format::Pe => files.push((format!("{name}.lib"), archive)),
                // alloc: once per library searched, its file name
                Format::Elf | Format::MachO | Format::Wasm => files.push((format!("lib{name}.a"), archive)),
            }
        }
        NativeLibKind::Dylib => {
            let archive = NativeKind::Archive { whole: false };
            match format {
                Format::Elf => {
                    // alloc: once per library searched, its file names
                    files.push((format!("lib{name}.so"), shared));
                    // alloc: once per library searched, its file names
                    files.push((format!("lib{name}.a"), archive));
                }
                Format::MachO => {
                    for extension in ["tbd", "dylib", "so"] {
                        // alloc: once per library searched, its file names
                        files.push((format!("lib{name}.{extension}"), shared));
                    }
                    // alloc: once per library searched, its file names
                    files.push((format!("lib{name}.a"), archive));
                }
                // An import library or a static one: the archive reader takes both.
                // alloc: once per library searched, its file name
                Format::Pe => files.push((format!("{name}.lib"), archive)),
                // alloc: once per library searched, its file name
                Format::Wasm => files.push((format!("lib{name}.a"), archive)),
            }
        }
    }
    Ok(files)
}

/// What a `+verbatim` file is read as, by its extension, as the linker takes `-l:NAME`.
fn by_extension(format: Format, name: &str, kind: NativeLibKind) -> NativeKind {
    let whole = matches!(kind, NativeLibKind::Static { whole: true, .. });
    let extension = Path::new(name).extension().and_then(|it| it.to_str()).unwrap_or_default();
    let is = |it: &str| extension.eq_ignore_ascii_case(it);
    if is("o") || is("obj") {
        NativeKind::Object
    } else if is("a") || is("lib") {
        NativeKind::Archive { whole }
    } else {
        NativeKind::Shared { as_needed: format == Format::Elf }
    }
}

fn not_found<'a>(name: &str, files: &[(String, NativeKind)], dirs: impl Iterator<Item = &'a Path>) -> LinkError {
    // alloc: error path
    let mut what = String::new();
    let _ = write!(what, "the native library `{name}` is not found: no ");
    for (at, (file, _)) in files.iter().enumerate() {
        what.push_str(if at == 0 { "" } else { ", " });
        what.push_str(file);
    }
    what.push_str(" in ");
    let mut any = false;
    for dir in dirs {
        what.push_str(if any { ", " } else { "" });
        let _ = write!(what, "{}", dir.display());
        any = true;
    }
    if !any {
        what.push_str("any directory: the link names none, and the toolchain has none");
    }
    fail(what)
}

/// What a file's first bytes say it is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Sniffed {
    Archive,
    Object,
    Shared,
    /// Neither an archive nor an ELF file: a linker script, for the reader to read or refuse.
    Text,
}

fn sniff(path: &Path) -> Result<Sniffed, LinkError> {
    // alloc: error path
    let failed = |error: std::io::Error| fail(format!("{}: {error}", path.display()));
    let mut file = std::fs::File::open(path).map_err(failed)?;
    let mut head = [0u8; 18];
    let mut read = 0;
    while read < head.len() {
        match file.read(&mut head[read..]) {
            Ok(0) => break,
            Ok(n) => read += n,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(failed(error)),
        }
    }
    let head = &head[..read];
    if head.starts_with(b"!<arch>\n") {
        return Ok(Sniffed::Archive);
    }
    if !head.starts_with(b"\x7fELF") {
        return Ok(Sniffed::Text);
    }
    let e_type = match (head.get(5), head.get(16..18)) {
        (Some(1), Some(it)) => u16::from_le_bytes([it[0], it[1]]),
        (Some(2), Some(it)) => u16::from_be_bytes([it[0], it[1]]),
        // alloc: error path
        _ => return Err(fail(format!("{}: an ELF file too short to say what it is", path.display()))),
    };
    match e_type {
        1 => Ok(Sniffed::Object),
        3 => Ok(Sniffed::Shared),
        // alloc: error path
        other => Err(fail(format!("{}: an ELF file of type {other}, neither an object nor a shared library", path.display()))),
    }
}

fn fail(what: String) -> LinkError {
    LinkError { function: None, what }
}
