//! Code written through one mapping of an entry runs through another, rights the kernel caps,
//! the rendezvous between two ends of a socket in this process, and its messages decoded from
//! any bytes without a panic.

use super::rendezvous::{self, Message, Request, Token, decode_reply, decode_request, encode_reply};
use super::{Granted, Object, granted, page, reserve, thread_id, threads};
use crate::channel::Channel;
use crate::frame::kinds;
use crate::sys::Rights;
use crate::{Access, Error};

/// `mov w0, #42` and `ret`.
const FORTY_TWO: [u8; 8] = [0x40, 0x05, 0x80, 0x52, 0xc0, 0x03, 0x5f, 0xd6];

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

fn pair() -> (Channel, Channel) {
    let mut fds = [0; 2];
    // SAFETY: `fds` is two writable ints for the new sockets.
    assert_eq!(unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, fds.as_mut_ptr()) }, 0);
    (Channel::from_raw(i64::from(fds[0])).unwrap(), Channel::from_raw(i64::from(fds[1])).unwrap())
}

#[test]
fn code_written_in_one_view_runs_in_another() {
    let (mut channel, _other) = pair();
    let len = page() * 2;
    let object = Object::create("test", len).unwrap();
    let mut view = object.map_here(Access::Shared).unwrap();
    view.bytes_mut(0..8).unwrap().copy_from_slice(&FORTY_TWO);
    view.publish(0..8);

    let code: Granted = granted(&object, Rights::Code).unwrap();
    let at = reserve(&mut channel, len, None).unwrap();
    assert_eq!(code.map(&mut channel, 0, page(), at, Access::Execute).unwrap(), at);
    // SAFETY: `at` maps the object's first page readable and executable, which holds
    // `mov w0, #42; ret` made right for instruction fetch above.
    let run: extern "C" fn() -> u32 = unsafe { std::mem::transmute(at as usize) };
    assert_eq!(run(), 42);

    view.bytes_mut(0..4).unwrap().copy_from_slice(&[0xe0, 0x00, 0x80, 0x52]);
    view.publish(0..4);
    assert_eq!(run(), 7, "a store through the daemon's view is what the other view runs");

    let error = code.map(&mut channel, 0, page(), at + page(), Access::Shared).unwrap_err();
    assert!(error.what.contains("mach_vm_map"), "{error}");
}

#[test]
fn a_copy_is_private_and_a_view_is_read_only() {
    let (mut channel, _other) = pair();
    let len = page();
    let object = Object::create("copy", len).unwrap();
    let mut view = object.map_here(Access::Shared).unwrap();
    view.bytes_mut(0..4).unwrap().copy_from_slice(&[1, 2, 3, 4]);

    let code = granted(&object, Rights::Code).unwrap();
    let at = reserve(&mut channel, len, None).unwrap();
    assert_eq!(code.map(&mut channel, 0, len, at, Access::Copy).unwrap(), at);
    // SAFETY: `at` is the private copy just mapped, readable and writable, of `len` bytes.
    let copy = unsafe { std::slice::from_raw_parts_mut(at as *mut u8, len as usize) };
    assert_eq!(&copy[..4], &[1, 2, 3, 4]);
    copy[0] = 9;
    assert_eq!(view.bytes(0..1).unwrap(), &[1]);

    let mut read = code.view(&mut channel).unwrap();
    assert_eq!(read.bytes(0..4).unwrap(), &[1, 2, 3, 4]);
    assert!(read.bytes_mut(0..1).is_err());
    assert!(code.map(&mut channel, len, len, at, Access::Read).is_err(), "past the object");
}

#[test]
fn a_fixed_map_outside_a_reservation_never_replaces() {
    let (mut channel, _other) = pair();
    let object = Object::create("fixed", page()).unwrap();
    let taken = object.map_here(Access::Shared).unwrap();
    let code = granted(&object, Rights::Code).unwrap();
    let error = code.map(&mut channel, 0, page(), taken.address(), Access::Read).unwrap_err();
    assert!(error.what.contains("mach_vm_map"), "{error}");
    let error = reserve(&mut channel, page(), Some(taken.address())).unwrap_err();
    assert!(error.what.contains("are taken"), "{error}");
}

#[test]
fn threads_name_this_one() {
    let mut out = Vec::new();
    threads(std::process::id(), &mut out).unwrap();
    assert!(out.contains(&thread_id()), "{out:?} lacks {}", thread_id());
    let worker = std::thread::spawn(thread_id);
    let id = worker.join().unwrap();
    assert_ne!(id, thread_id());
}

#[test]
fn the_rendezvous_hands_objects_to_the_peer() {
    let (mut daemon, mut loader) = pair();
    let image = Object::create("image", page()).unwrap();
    let control = Object::create("control", page()).unwrap();
    let mut view = image.map_here(Access::Shared).unwrap();
    view.bytes_mut(0..3).unwrap().copy_from_slice(b"abc");

    let taker = std::thread::spawn(move || -> Result<Vec<Granted>, Error> {
        let frame = loader.recv()?;
        assert_eq!(frame.kind, kinds::GRANT);
        assert_eq!(frame.body, b"grant");
        let granted = super::recv_objects(&mut loader, 2)?;
        let read = granted[0].view(&mut loader)?;
        assert_eq!(read.bytes(0..3)?, b"abc");
        let mut shared = granted[1].view(&mut loader)?;
        assert!(shared.bytes_mut(0..1).is_err());
        drop((read, shared));
        Ok(granted)
    });
    super::send_objects(&mut daemon, std::process::id(), b"grant", &[(&image, Rights::Code), (&control, Rights::Control)])
        .unwrap();
    let granted = taker.join().unwrap().unwrap();
    assert_eq!(granted.iter().map(Granted::len).collect::<Vec<_>>(), [page(), page()]);
}

#[test]
fn a_grant_for_another_process_is_refused() {
    let (mut daemon, _loader) = pair();
    let image = Object::create("other", page()).unwrap();
    let error = super::send_objects(&mut daemon, std::process::id() + 1, b"", &[(&image, Rights::Code)]).unwrap_err();
    assert!(error.what.contains("not the run process"), "{error}");
}

fn token(seed: u32) -> Token {
    std::array::from_fn(|index| seed.wrapping_mul(31).wrapping_add(index as u32))
}

fn with_trailer(message: &mut Message, size: usize, token: &Token) {
    let at = size.next_multiple_of(4);
    message.0[at..at + 4].copy_from_slice(&0u32.to_le_bytes());
    message.0[at + 4..at + 8].copy_from_slice(&52u32.to_le_bytes());
    for (index, word) in token.iter().enumerate() {
        message.0[at + 20 + index * 4..at + 24 + index * 4].copy_from_slice(&word.to_le_bytes());
    }
}

/// A request as the kernel delivers it: the reply port in the remote field, a send-once right.
fn received_request(reply: u32, count: u32, token: &Token) -> Message {
    let mut message = Message::new();
    rendezvous::encode_request(&mut message, 0x1234, reply, count);
    message.0[0..4].copy_from_slice(&18u32.to_le_bytes());
    message.0[8..12].copy_from_slice(&reply.to_le_bytes());
    with_trailer(&mut message, 32, token);
    message
}

#[test]
fn messages_round_trip() {
    let message = received_request(0x707, 3, &token(5));
    assert_eq!(decode_request(&message.0), Some(Request { reply: 0x707, count: 3, token: token(5) }));

    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    for round in 0..200 {
        let count = (rng.next() % 33) as usize;
        let objects: Vec<(u32, u64)> = (0..count).map(|_| ((rng.next() as u32) | 1, rng.next())).collect();
        let objects: Vec<(u32, u64)> = objects.into_iter().map(|(port, len)| (port.min(u32::MAX - 1), len)).collect();
        let mut reply = Message::new();
        let size = encode_reply(&mut reply, 0x55, &objects) as usize;
        with_trailer(&mut reply, size, &token(round));
        assert_eq!(decode_reply(&reply.0, count), Ok((objects.clone(), token(round))));
        if count > 0 {
            assert!(decode_reply(&reply.0, count - 1).is_err());
        }
    }
}

#[test]
fn messages_from_any_bytes_decode_or_fail() {
    let mut rng = Rng(0x2545_f491_4f6c_dd1d);
    let request = received_request(9, 2, &token(1));
    let mut reply = Message::new();
    let size = encode_reply(&mut reply, 9, &[(3, 4), (5, 6)]) as usize;
    with_trailer(&mut reply, size, &token(2));
    for _ in 0..20_000 {
        let mut bytes = if rng.next() % 2 == 0 { request.0 } else { reply.0 };
        for _ in 0..1 + rng.next() % 8 {
            let at = (rng.next() % 128) as usize;
            bytes[at] = rng.next() as u8;
        }
        let len = (rng.next() % bytes.len() as u64) as usize;
        let _ = decode_request(&bytes[..len]);
        let _ = decode_reply(&bytes[..len], (rng.next() % 40) as usize);
        let _ = decode_request(&bytes);
        let _ = decode_reply(&bytes, 2);
    }
}
