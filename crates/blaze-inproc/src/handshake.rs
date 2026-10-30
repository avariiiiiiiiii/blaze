//! The start of a run, frame by frame, as the daemon ([`crate::host`]) and the program's
//! start hook ([`crate::runtime`]) speak it:
//!
//! 1. HELLO, hook to daemon: the token from `BLAZE_INPROC`, the process ID, [`VERSION`], the
//!    page, and where the image's headers are in the process ([`Hello`]).
//! 2. GRANT, daemon to hook: the format, the linked header and span, the objects (the image's
//!    code memory, the fence's control words, the asks, the cells, the objects of fixed maps)
//!    and the overlays ([`Grant`]); the objects come with it or after it ([`crate::sys`]).
//! 3. On Windows only, MAP for the control words and the asks (the daemon maps every view) and
//!    OVERLAY, with which the hook asks the daemon to lay the overlays over its image;
//!    OVERLAID or REFUSED answers.
//! 4. STARTED, hook to daemon: the thread that runs `main`. Or FAILED, with why.
//!
//! Every body is bounds-checked and refused whole when any field is out of its range.

use std::ops::Range;

use crate::frame::{FrameError, Reader, Writer};
use crate::image::Layout;
use crate::sys::Rights;
use crate::{Access, Error, Format};

/// The variable naming the host: `<address>\t<token in hex>`. The start hook takes it out of
/// the environment before the program sees it.
pub const HOST_VAR: &str = "BLAZE_INPROC";

/// The version of the handshake; a host and a hook of another refuse each other.
pub(crate) const VERSION: u32 = 3;

/// The most objects and overlays one grant carries.
const MOST: usize = 64;

fn bad(what: &'static str) -> impl Fn(FrameError) -> Error {
    move |it| Error::new(format!("{what}: {it}"))
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Hello {
    pub(crate) token: [u8; 16],
    pub(crate) pid: u32,
    pub(crate) version: u32,
    pub(crate) page: u64,
    /// Where the image's headers are in the process: its slide plus the linked header.
    pub(crate) header: u64,
}

impl Hello {
    pub(crate) fn encode(&self, out: &mut Vec<u8>) {
        Writer::new(out).raw(&self.token).u32(self.pid).u32(self.version).u64(self.page).u64(self.header);
    }

    pub(crate) fn decode(body: &[u8]) -> Result<Hello, Error> {
        let bad = bad("a start hook's hello");
        let mut read = Reader::new(body);
        let hello = Hello {
            token: read.array().map_err(&bad)?,
            pid: read.u32().map_err(&bad)?,
            version: read.u32().map_err(&bad)?,
            page: read.u64().map_err(&bad)?,
            header: read.u64().map_err(&bad)?,
        };
        read.finish().map_err(&bad)?;
        Ok(hello)
    }
}

/// Where an overlay goes.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) enum Place {
    /// Over one of the image's code segments, from the same file offset of the image object.
    Code,
    /// Inside a reservation of the image: the arena, the cells, a fixed map.
    Reserved,
}

/// `len` bytes of object `object` from `offset`, mapped at the linked address `at` with
/// `access`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) struct Overlay {
    pub(crate) object: u32,
    pub(crate) offset: u64,
    pub(crate) len: u64,
    pub(crate) at: u64,
    pub(crate) access: Access,
    pub(crate) place: Place,
}

impl Overlay {
    pub(crate) fn range(&self) -> Range<u64> {
        self.at..self.at.saturating_add(self.len)
    }
}

/// What the daemon gives a start hook. Objects: the image's code memory first (rights
/// [`Rights::Code`]), the fence's control words second and the asks ([`crate::ask`]) third
/// (both [`Rights::Words`]), then the cells and the objects of fixed maps.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct Grant {
    pub(crate) format: Format,
    pub(crate) header: u64,
    pub(crate) span: Range<u64>,
    pub(crate) objects: Vec<(u64, Rights)>,
    /// By address, none overlapping another.
    pub(crate) overlays: Vec<Overlay>,
}

fn rights_tag(rights: Rights) -> u8 {
    match rights {
        Rights::Code => 1,
        Rights::Words => 2,
    }
}

fn place_tag(place: Place) -> u8 {
    match place {
        Place::Code => 1,
        Place::Reserved => 2,
    }
}

impl Grant {
    pub(crate) fn encode(&self, out: &mut Vec<u8>) {
        out.reserve(29 + 9 * self.objects.len() + 34 * self.overlays.len());
        let mut write = Writer::new(out);
        write.u8(self.format.tag()).u64(self.header).u64(self.span.start).u64(self.span.end);
        write.u32(self.objects.len() as u32);
        for &(len, rights) in &self.objects {
            write.u64(len).u8(rights_tag(rights));
        }
        write.u32(self.overlays.len() as u32);
        for overlay in &self.overlays {
            write
                .u32(overlay.object)
                .u64(overlay.offset)
                .u64(overlay.len)
                .u64(overlay.at)
                .u8(overlay.access.tag())
                .u8(place_tag(overlay.place));
        }
    }

    /// The grant of `body`, refused unless every object and overlay is one a hook may map:
    /// the code memory never shared writable, shared words never executable and only in a
    /// reservation, code only from the code memory and executable, nothing out of its object
    /// or the span, no overlay over another.
    pub(crate) fn decode(body: &[u8]) -> Result<Grant, Error> {
        let bad = bad("the daemon's grant");
        let mut read = Reader::new(body);
        let tag = read.u8().map_err(&bad)?;
        let format = Format::from_tag(tag).ok_or_else(|| Error::new(format!("a grant of format {tag}")))?;
        let header = read.u64().map_err(&bad)?;
        let span = read.u64().map_err(&bad)?..read.u64().map_err(&bad)?;
        let count = read.count(9).map_err(&bad)?;
        if !(3..=MOST).contains(&count) {
            return Err(Error::new(format!("a grant of {count} objects")));
        }
        let mut objects = Vec::with_capacity(count);
        for _ in 0..count {
            let len = read.u64().map_err(&bad)?;
            let rights = match read.u8().map_err(&bad)? {
                1 => Rights::Code,
                2 => Rights::Words,
                other => return Err(Error::new(format!("a granted object of rights {other}"))),
            };
            objects.push((len, rights));
        }
        if objects[0].1 != Rights::Code || objects[1].1 != Rights::Words || objects[2].1 != Rights::Words {
            return Err(Error::new("a grant whose first objects are not the code memory, the control words and the asks"));
        }
        let count = read.count(34).map_err(&bad)?;
        if count > MOST {
            return Err(Error::new(format!("a grant of {count} overlays")));
        }
        let mut overlays: Vec<Overlay> = Vec::with_capacity(count);
        for _ in 0..count {
            let object = read.u32().map_err(&bad)?;
            let offset = read.u64().map_err(&bad)?;
            let len = read.u64().map_err(&bad)?;
            let at = read.u64().map_err(&bad)?;
            let tag = read.u8().map_err(&bad)?;
            let access = Access::from_tag(tag).ok_or_else(|| Error::new(format!("an overlay of access {tag}")))?;
            let place = match read.u8().map_err(&bad)? {
                1 => Place::Code,
                2 => Place::Reserved,
                other => return Err(Error::new(format!("an overlay of place {other}"))),
            };
            let overlay = Overlay { object, offset, len, at, access, place };
            let Some(&(object_len, rights)) = objects.get(object as usize).filter(|_| object != 1 && object != 2) else {
                return Err(Error::new(format!("an overlay of object {object} of {}", objects.len())));
            };
            let allowed = match (rights, place, access) {
                (Rights::Code, Place::Code, Access::Execute) => object == 0,
                (Rights::Code, Place::Reserved, access) => access != Access::Shared,
                (Rights::Words, Place::Reserved, Access::Shared) => true,
                _ => false,
            };
            if !allowed {
                return Err(Error::new(format!(
                    "an overlay of object {object} ({rights:?}) at {at:#x} as {access:?} over {place:?}"
                )));
            }
            let fits = len != 0
                && offset.checked_add(len).is_some_and(|end| end <= object_len)
                && at.checked_add(len).is_some_and(|end| at >= span.start && end <= span.end);
            if !fits {
                return Err(Error::new(format!(
                    "an overlay of {len:#x} bytes from offset {offset:#x} of object {object} at {at:#x}: out of its object \
                     or the span {:#x}..{:#x}",
                    span.start, span.end
                )));
            }
            if let Some(last) = overlays.last()
                && last.range().end > at
            {
                return Err(Error::new(format!("overlays at {:#x} and {at:#x} out of order or overlapping", last.at)));
            }
            overlays.push(overlay);
        }
        read.finish().map_err(&bad)?;
        Ok(Grant { format, header, span, objects, overlays })
    }

    /// Checks the grant against `own`, the layout the hook read from its own image in memory,
    /// with pages of `page` bytes: every code segment is overlaid whole from its own file
    /// offset, everything else lands inside a reservation. Compared relative to each one's
    /// header, so a header the system loader moved changes nothing.
    pub(crate) fn check(&self, own: &Layout, page: u64) -> Result<(), Error> {
        if self.format != own.format {
            return Err(Error::new(format!("a grant for a {:?} image, where this one is {:?}", self.format, own.format)));
        }
        let theirs = |at: u64| at.wrapping_sub(self.header);
        let ours = |at: u64| at.wrapping_sub(own.header);
        if theirs(self.span.start) != ours(own.span.start) || theirs(self.span.end) != ours(own.span.end) {
            return Err(Error::new(format!(
                "the daemon's image spans {:#x}..{:#x} from its header {:#x}, this one {:#x}..{:#x} from {:#x}: not the \
                 executable the daemon wrote",
                self.span.start, self.span.end, self.header, own.span.start, own.span.end, own.header
            )));
        }
        for overlay in &self.overlays {
            if !(overlay.at | overlay.len | overlay.offset).is_multiple_of(page) {
                return Err(Error::new(format!("an overlay at {:#x} of {:#x} bytes: not whole pages", overlay.at, overlay.len)));
            }
            let at = theirs(overlay.at);
            let fits = match overlay.place {
                Place::Code => own.code.iter().any(|it| ours(it.linked) == at && it.offset == overlay.offset && it.len == overlay.len),
                Place::Reserved => own
                    .reserved
                    .iter()
                    .any(|it| ours(it.start) <= at && at.checked_add(overlay.len).is_some_and(|end| end <= ours(it.end))),
            };
            if !fits {
                return Err(Error::new(format!(
                    "the daemon's overlay of {:#x} bytes at {:#x} ({:?}, file offset {:#x}) is not {} of this image",
                    overlay.len,
                    overlay.at,
                    overlay.place,
                    overlay.offset,
                    match overlay.place {
                        Place::Code => "a code segment",
                        Place::Reserved => "inside a reservation",
                    }
                )));
            }
        }
        for segment in &own.code {
            let covered = self.overlays.iter().any(|it| it.place == Place::Code && theirs(it.at) == ours(segment.linked));
            if !covered {
                return Err(Error::new(format!(
                    "the code segment at {:#x} has no overlay: it would run from the file, not the code memory",
                    segment.linked
                )));
            }
        }
        Ok(())
    }
}

/// A MAP frame's body: the whole of granted object `object`, anywhere, with `access`.
pub(crate) fn encode_map(object: u32, access: Access, out: &mut Vec<u8>) {
    Writer::new(out).u32(object).u8(access.tag());
}

pub(crate) fn decode_map(body: &[u8]) -> Result<(u32, Access), Error> {
    let bad = bad("a start hook's map");
    let mut read = Reader::new(body);
    let object = read.u32().map_err(&bad)?;
    let tag = read.u8().map_err(&bad)?;
    let access = Access::from_tag(tag).ok_or_else(|| Error::new(format!("a map of access {tag}")))?;
    read.finish().map_err(&bad)?;
    Ok((object, access))
}

pub(crate) fn hex(bytes: &[u8; 16]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(32);
    for byte in bytes {
        out.push(DIGITS[(byte >> 4) as usize] as char);
        out.push(DIGITS[(byte & 15) as usize] as char);
    }
    out
}

pub(crate) fn unhex(text: &[u8]) -> Option<[u8; 16]> {
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

/// The address and token of a `BLAZE_INPROC` value.
pub(crate) fn host_var(value: &[u8]) -> Result<(&str, [u8; 16]), Error> {
    let tab = value
        .iter()
        .position(|it| *it == b'\t')
        .ok_or_else(|| Error::new(format!("{HOST_VAR} names no token")))?;
    let address =
        std::str::from_utf8(&value[..tab]).map_err(|_| Error::new(format!("{HOST_VAR} names an address that is not UTF-8")))?;
    let token = unhex(&value[tab + 1..])
        .ok_or_else(|| Error::new(format!("{HOST_VAR} has a token that is not 32 lowercase hex digits")))?;
    Ok((address, token))
}
