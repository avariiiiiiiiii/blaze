//! blaze's loader: the run process of a dev run or test, with no user executable on disk.
//!
//! The daemon spawns `blaze-inproc-run <argv0> <args>...` with `BLAZE_INPROC` naming its host
//! ([`crate::host`]). The loader takes that variable out of its environment, connects, says
//! hello with its process ID and its thread-local reserve, and is granted the memory
//! objects: the program image, the fence's control words, and any objects mapped at fixed
//! addresses first (`blaze-premap`'s). It maps those, then the format's loader ([`Program`])
//! maps the image's segments from the image object (code shared with the daemon's writable
//! view, data as private copies), links it to the system's libraries, sets up its
//! thread-locals and unwind tables, tells the daemon where it is, and runs its initializers
//! and `main` with the program's own `argv`, in this process.
//!
//! The loader is a separate small binary rather than a mode of the daemon's: the program's
//! thread-locals live in a reserve of the loader's own static thread-local block (ELF, PE),
//! which has to be the loader's and nothing else's, and the run process should not map the
//! daemon's whole image.

use std::ffi::OsString;
use std::sync::OnceLock;

use crate::channel::Channel;
use crate::frame::{Reader, Writer, kinds};
use crate::runtime::{BOOT_MAGIC, BOOT_VAR, Boot};
use crate::sys::{self, Granted};
use crate::{Access, Error, Format, TlsReserve};

#[cfg(all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")))]
mod elf;
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
mod macho;
#[cfg(all(windows, target_arch = "x86_64"))]
mod pe;

/// The environment variable the loader finds the host by: `<address>\t<token in hex>`. The
/// loader takes it out of the environment before the program sees it.
pub const HOST_VAR: &str = "BLAZE_INPROC";

/// The version of the handshake; a host and loader of others refuse each other.
pub(crate) const VERSION: u32 = 1;

/// The bytes of every thread's static thread-local block the loader keeps for a program's
/// thread-locals (ELF, PE). A program with more is refused, naming the sizes.
pub(crate) const RESERVE_SIZE: u64 = 64 << 10;
pub(crate) const RESERVE_ALIGN: u64 = 64;

/// The image as the format's loader gets it.
pub(crate) struct Image<'a> {
    /// The image object, to map segments from.
    pub(crate) object: &'a Granted,
    /// The image object, read-only, to parse headers from: its first `image_len` bytes are the
    /// image as the linker lays out its file. Untrusted as any file is: every offset and length
    /// read from it is checked against it.
    pub(crate) bytes: &'a [u8],
    pub(crate) image_len: u64,
    /// Where offset 0 has to be, when another process maps the memory there already.
    pub(crate) base: Option<u64>,
    /// This loader's thread-local reserve.
    pub(crate) reserve: TlsReserve,
    /// For [`Granted::map`] and [`sys::reserve`] (Windows maps from the daemon's side).
    pub(crate) channel: &'a mut Channel,
}

/// A program mapped and linked, not yet started.
pub(crate) trait Program {
    /// Where the image object's offset 0 is mapped.
    fn base(&self) -> u64;
    /// Runs the program's initializers and `main` with `args` (`argv[0]` first) and the
    /// process's environment, then exits the process with what `main` returned, as the C
    /// runtime does (atexit handlers, then the program's finalizers).
    fn enter(self: Box<Self>, args: Vec<OsString>) -> !;
}

/// What the format of this platform keeps aside for a program's thread-locals.
fn reserve() -> Result<TlsReserve, Error> {
    #[cfg(all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")))]
    return elf::reserve();
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    return macho::reserve();
    #[cfg(all(windows, target_arch = "x86_64"))]
    return pe::reserve();
    #[allow(unreachable_code)]
    Err(Error::new("blaze's loader runs on Linux x86-64 and AArch64, macOS AArch64 and Windows x86-64"))
}

fn load_format(format: Format, image: Image<'_>) -> Result<Box<dyn Program>, Error> {
    match format {
        #[cfg(all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")))]
        Format::Elf => elf::load(image),
        #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
        Format::MachO => macho::load(image),
        #[cfg(all(windows, target_arch = "x86_64"))]
        Format::Pe => pe::load(image),
        #[allow(unreachable_patterns)]
        other => {
            drop(image);
            Err(Error::new(format!("a {other:?} image, which this platform's loader does not load")))
        }
    }
}

/// The boot record the program's runtime reads ([`crate::runtime`]); set once, before the
/// jump into the program, and never written after.
static BOOT: OnceLock<Boot> = OnceLock::new();

/// The loader's `main`: never returns. `--reserve` prints the thread-local reserve and exits.
pub fn main() -> ! {
    let args: Vec<OsString> = std::env::args_os().collect();
    if args.len() == 2 && args[1] == "--reserve" {
        match reserve() {
            Ok(reserve) => {
                print!("{}", reserve.encode());
                std::process::exit(0)
            }
            Err(error) => {
                eprintln!("{error}");
                std::process::exit(2)
            }
        }
    }
    match run(args) {
        Ok(never) => match never {},
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(127)
        }
    }
}

/// The body of a GRANT frame: what the daemon gives a loader.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct Grant {
    pub(crate) format: Format,
    /// Where the image has to be mapped, 0 for anywhere.
    pub(crate) base: u64,
    pub(crate) image_len: u64,
    /// Each object's length and what it is ([`GRANT_CODE`], [`GRANT_CONTROL`]): the image
    /// first, the control words second, then the objects of fixed maps.
    pub(crate) objects: Vec<(u64, u8)>,
    /// Object, offset, length, address and access of each fixed map.
    pub(crate) maps: Vec<(u32, u64, u64, u64, Access)>,
}

pub(crate) const GRANT_CODE: u8 = 1;
pub(crate) const GRANT_CONTROL: u8 = 2;

impl Grant {
    pub(crate) fn encode(&self, out: &mut Vec<u8>) {
        out.reserve(29 + 9 * self.objects.len() + 29 * self.maps.len());
        let mut write = Writer::new(out);
        write.u8(self.format.tag()).u64(self.base).u64(self.image_len).u32(self.objects.len() as u32);
        for &(len, what) in &self.objects {
            write.u64(len).u8(what);
        }
        write.u32(self.maps.len() as u32);
        for &(object, offset, len, at, access) in &self.maps {
            write.u32(object).u64(offset).u64(len).u64(at).u8(access.tag());
        }
    }
}

pub(crate) fn read_grant(body: &[u8]) -> Result<Grant, Error> {
    let bad = |it: crate::frame::FrameError| Error::new(format!("the daemon's grant: {it}"));
    let mut read = Reader::new(body);
    let tag = read.u8().map_err(bad)?;
    let format = Format::from_tag(tag).ok_or_else(|| Error::new(format!("the daemon granted an image of format {tag}")))?;
    let base = read.u64().map_err(bad)?;
    let image_len = read.u64().map_err(bad)?;
    let count = read.count(9).map_err(bad)?;
    let mut objects = Vec::with_capacity(count);
    for _ in 0..count {
        objects.push((read.u64().map_err(bad)?, read.u8().map_err(bad)?));
    }
    if objects.len() < 2 {
        return Err(Error::new(format!("the daemon granted {} objects, not the image and the control words", objects.len())));
    }
    for (index, &(_, what)) in objects.iter().enumerate() {
        let want = if index == 1 { GRANT_CONTROL } else { GRANT_CODE };
        if what != want {
            return Err(Error::new(format!("granted object {index} is of kind {what}, where {want} belongs")));
        }
    }
    let count = read.count(29).map_err(bad)?;
    let mut maps = Vec::with_capacity(count);
    for _ in 0..count {
        let object = read.u32().map_err(bad)?;
        let offset = read.u64().map_err(bad)?;
        let len = read.u64().map_err(bad)?;
        let at = read.u64().map_err(bad)?;
        let tag = read.u8().map_err(bad)?;
        let access = Access::from_tag(tag).ok_or_else(|| Error::new(format!("a fixed map of access {tag}")))?;
        if object as usize >= objects.len() || object == 1 {
            return Err(Error::new(format!("a fixed map of object {object} of {}", objects.len())));
        }
        if access == Access::Shared {
            return Err(Error::new(format!("a fixed map of object {object} at {at:#x} shared writable")));
        }
        maps.push((object, offset, len, at, access));
    }
    read.finish().map_err(bad)?;
    Ok(Grant { format, base, image_len, objects, maps })
}

fn run(args: Vec<OsString>) -> Result<std::convert::Infallible, Error> {
    let var = std::env::var(HOST_VAR)
        .map_err(|_| Error::new(format!("{HOST_VAR} is not set: blaze's loader is started by the daemon")))?;
    // SAFETY: the loader has one thread until it jumps into the program: nothing reads the
    // environment concurrently.
    unsafe { std::env::remove_var(HOST_VAR) };
    let (address, token) =
        var.split_once('\t').ok_or_else(|| Error::new(format!("{HOST_VAR} names no token")))?;
    let token =
        unhex(token).ok_or_else(|| Error::new(format!("{HOST_VAR} has a token that is not 32 hex digits")))?;
    let mut channel = Channel::new(sys::connect(address)?);
    let started = start(&mut channel, token, args);
    if let Err(error) = &started {
        let mut body = Vec::new();
        Writer::new(&mut body).str(&error.to_string());
        let _ = channel.send(kinds::FAILED, 0, &body);
    }
    started
}

fn start(channel: &mut Channel, token: [u8; 16], args: Vec<OsString>) -> Result<std::convert::Infallible, Error> {
    let reserve = reserve()?;
    let mut hello = Vec::with_capacity(64);
    Writer::new(&mut hello)
        .raw(&token)
        .u32(std::process::id())
        .u32(VERSION)
        .u64(sys::page())
        .i64(reserve.bias)
        .u64(reserve.size)
        .u64(reserve.align);
    channel.send(kinds::HELLO, 0, &hello)?;
    let grant = {
        let frame = channel.recv()?;
        match frame.kind {
            kinds::GRANT => read_grant(frame.body)?,
            kinds::REFUSED => {
                let why = Reader::new(frame.body).str().unwrap_or("(not UTF-8)").to_owned();
                return Err(Error::new(format!("the daemon refused the loader: {why}")));
            }
            other => return Err(Error::new(format!("the daemon answered hello with a frame of kind {:#06x}", other.0))),
        }
    };
    let objects = sys::recv_objects(channel, grant.objects.len())?;
    for (index, (object, (len, _))) in objects.iter().zip(&grant.objects).enumerate() {
        if object.len() != *len {
            return Err(Error::new(format!("object {index} is {:#x} bytes, where the grant says {len:#x}", object.len())));
        }
    }
    for &(object, offset, len, at, access) in &grant.maps {
        let mapped = objects[object as usize].map(channel, offset, len, at, access)?;
        if mapped != at {
            return Err(Error::new(format!("object {object} was mapped at {mapped:#x}, where it has to be at {at:#x}")));
        }
    }
    let control = &objects[1];
    let control_at = sys::reserve(channel, control.len(), None)?;
    let control_at = control.map(channel, 0, control.len(), control_at, Access::Shared)?;

    let view = objects[0].view(channel)?;
    let image_len = grant.image_len;
    if image_len > view.len() as u64 {
        return Err(Error::new(format!("an image of {image_len:#x} bytes in an object of {:#x}", view.len())));
    }
    let bytes = view.bytes(0..view.len())?;
    let program = load_format(
        grant.format,
        Image {
            object: &objects[0],
            bytes,
            image_len,
            base: (grant.base != 0).then_some(grant.base),
            reserve,
            channel,
        },
    )?;
    let base = program.base();
    if grant.base != 0 && base != grant.base {
        return Err(Error::new(format!("the image was mapped at {base:#x}, where it has to be at {:#x}", grant.base)));
    }

    let boot = BOOT.get_or_init(|| Boot {
        magic: BOOT_MAGIC,
        control: control_at,
        control_len: control.len(),
        channel: channel.raw(),
        base,
        len: objects[0].len(),
    });
    // SAFETY: still one thread: nothing reads the environment concurrently.
    unsafe { std::env::set_var(BOOT_VAR, format!("{:x}", boot as *const Boot as u64)) };

    let mut started = Vec::with_capacity(16);
    Writer::new(&mut started).u64(base).u64(sys::thread_id());
    channel.send(kinds::STARTED, 0, &started)?;
    // The program's runtime owns the channel's fd or handle from here, and every mapping
    // stays for the life of the process.
    std::mem::forget(view);
    std::mem::forget(objects);
    let mut args = args;
    if !args.is_empty() {
        args.remove(0);
    }
    program.enter(args)
}

pub(crate) fn unhex(text: &str) -> Option<[u8; 16]> {
    let text = text.as_bytes();
    if text.len() != 32 {
        return None;
    }
    let digit = |it: u8| match it {
        b'0'..=b'9' => Some(it - b'0'),
        b'a'..=b'f' => Some(it - b'a' + 10),
        _ => None,
    };
    let mut out = [0; 16];
    for (index, pair) in text.chunks_exact(2).enumerate() {
        out[index] = (digit(pair[0])? << 4) | digit(pair[1])?;
    }
    Some(out)
}
