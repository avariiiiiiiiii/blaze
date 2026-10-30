//! The crate's own tests: frames, grants and the thread-local reserve fuzzed (never a panic on
//! any bytes), the code memory's rules, and on Linux the real thing: code written through the
//! daemon's view runs through another, a patch is one word store, the loader's descriptor can't
//! be mapped to write, and the channel and the host's handshake over a real socket.

use crate::frame::{self, FrameError, Header, Kind, Reader, Writer, kinds};
use crate::load::{GRANT_CODE, GRANT_CONTROL, Grant, read_grant};
use crate::{Access, CodeMemory, CodeRange, Format, TlsReserve};

fn rng(state: &mut u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state
}

fn random_bytes(state: &mut u64, len: usize) -> Vec<u8> {
    (0..len).map(|_| rng(state) as u8).collect()
}

#[test]
fn headers_round_trip_and_refuse_reserved_bits_and_long_bodies() {
    let header = Header { kind: Kind(0x0123), len: 77, seq: 0xdead_beef };
    assert_eq!(Header::decode(&header.encode()), Ok(header));
    let mut bytes = header.encode();
    bytes[2] = 1;
    assert_eq!(Header::decode(&bytes), Err(FrameError::Reserved { bits: 1 }));
    let long = Header { kind: Kind(1), len: frame::MOST_BODY + 1, seq: 0 }.encode();
    assert!(matches!(Header::decode(&long), Err(FrameError::TooLong { .. })));
}

#[test]
fn frames_round_trip_whole_and_wait_when_cut() {
    let mut state = 0x1234_5678_9abc_def1;
    for _ in 0..500 {
        let len = (rng(&mut state) % 300) as usize;
        let body = random_bytes(&mut state, len);
        let kind = Kind(rng(&mut state) as u16);
        let seq = rng(&mut state) as u32;
        let mut bytes = Vec::new();
        frame::encode(kind, seq, &body, &mut bytes).unwrap();
        let (decoded, used) = frame::decode(&bytes).unwrap().unwrap();
        assert_eq!((decoded.kind, decoded.seq, decoded.body, used), (kind, seq, body.as_slice(), bytes.len()));
        let cut = (rng(&mut state) as usize) % bytes.len();
        assert_eq!(frame::decode(&bytes[..cut]), Ok(None));
        let mut two = bytes.clone();
        frame::encode(Kind(9), 1, b"next", &mut two).unwrap();
        let (_, first) = frame::decode(&two).unwrap().unwrap();
        let (second, _) = frame::decode(&two[first..]).unwrap().unwrap();
        assert_eq!(second.body, b"next");
    }
}

#[test]
fn no_bytes_make_a_frame_panic() {
    let mut state = 0x0bad_cafe_f00d_0001;
    for round in 0..20_000 {
        let len = (rng(&mut state) % 64) as usize;
        let mut bytes = random_bytes(&mut state, len);
        if round % 2 == 0 && bytes.len() >= frame::HEADER {
            // Mostly well-formed headers, so the body paths are reached too.
            bytes[2] = 0;
            bytes[3] = 0;
            let body = (rng(&mut state) % 80) as u32;
            bytes[4..8].copy_from_slice(&body.to_le_bytes());
        }
        if let Ok(Some((frame, used))) = frame::decode(&bytes) {
            assert!(used <= bytes.len());
            assert_eq!(used, frame::HEADER + frame.body.len());
        }
        let mut read = Reader::new(&bytes);
        for _ in 0..8 {
            let before = read.left();
            let ok = match rng(&mut state) % 9 {
                0 => read.u8().is_ok(),
                1 => read.u16().is_ok(),
                2 => read.u32().is_ok(),
                3 => read.u64().is_ok(),
                4 => read.i64().is_ok(),
                5 => read.bytes().is_ok(),
                6 => read.str().is_ok(),
                7 => {
                    let each = (rng(&mut state) % 40) as usize;
                    match read.count(each) {
                        Ok(count) => {
                            assert!(count.saturating_mul(each.max(1)) <= read.left());
                            true
                        }
                        Err(_) => false,
                    }
                }
                _ => read.raw((rng(&mut state) % 16) as usize).is_ok(),
            };
            assert!(read.left() <= before);
            if !ok {
                break;
            }
        }
        let _ = read.finish();
    }
}

#[test]
fn fields_round_trip() {
    let mut body = Vec::new();
    Writer::new(&mut body).u8(1).u16(2).u32(3).u64(4).i64(-5).bytes(b"six").str("seven").raw(&[8, 8]);
    let mut read = Reader::new(&body);
    assert_eq!(read.u8(), Ok(1));
    assert_eq!(read.u16(), Ok(2));
    assert_eq!(read.u32(), Ok(3));
    assert_eq!(read.u64(), Ok(4));
    assert_eq!(read.i64(), Ok(-5));
    assert_eq!(read.bytes(), Ok(&b"six"[..]));
    assert_eq!(read.str(), Ok("seven"));
    assert_eq!(read.array::<2>(), Ok([8, 8]));
    assert_eq!(read.finish(), Ok(()));
    let mut bad = Vec::new();
    Writer::new(&mut bad).bytes(&[0xff, 0xfe]);
    assert_eq!(Reader::new(&bad).str(), Err(FrameError::Utf8));
    assert!(matches!(Reader::new(&[1, 2]).finish(), Err(FrameError::Trailing { left: 2 })));
    let frame = frame::Frame { kind: kinds::HELLO, seq: 0, body: &[] };
    assert!(frame.reader(kinds::HELLO).is_ok());
    assert_eq!(frame.reader(kinds::GRANT).err(), Some(FrameError::Kind { kind: kinds::HELLO }));
}

#[test]
fn kind_ranges_are_apart() {
    assert!(kinds::INPROC.end <= kinds::LAZY.start);
    assert!(kinds::LAZY.end <= kinds::PREMAP.start);
    for kind in [kinds::HELLO, kinds::GRANT, kinds::MAP, kinds::MAPPED, kinds::STARTED, kinds::FAILED, kinds::REFUSED] {
        assert!(kinds::INPROC.contains(&kind.0));
    }
}

fn a_grant() -> Grant {
    Grant {
        format: Format::Elf,
        base: 0x7f00_0000_0000,
        image_len: 0x4_0000,
        objects: vec![(0x8_0000, GRANT_CODE), (0x2_0000, GRANT_CONTROL), (0x10_0000, GRANT_CODE)],
        maps: vec![(2, 0, 0x8000, 0x6000_0000_0000, Access::Execute), (2, 0x8000, 0x4000, 0x6000_0000_8000, Access::Copy)],
    }
}

#[test]
fn grants_round_trip() {
    let grant = a_grant();
    let mut bytes = Vec::new();
    grant.encode(&mut bytes);
    assert_eq!(read_grant(&bytes), Ok(grant));
}

#[test]
fn a_grant_cut_or_mutated_is_an_error_not_a_panic() {
    let mut bytes = Vec::new();
    a_grant().encode(&mut bytes);
    for cut in 0..bytes.len() {
        assert!(read_grant(&bytes[..cut]).is_err(), "a grant cut at {cut} was read");
    }
    let mut longer = bytes.clone();
    longer.push(0);
    assert!(read_grant(&longer).is_err());
    let mut state = 0x5eed_0000_0000_0007;
    for _ in 0..20_000 {
        let mut mutated = bytes.clone();
        for _ in 0..1 + rng(&mut state) % 3 {
            let at = (rng(&mut state) as usize) % mutated.len();
            mutated[at] = rng(&mut state) as u8;
        }
        if let Ok(grant) = read_grant(&mutated) {
            assert!(grant.objects.len() >= 2);
            assert_eq!(grant.objects[1].1, GRANT_CONTROL);
            assert!(grant.maps.iter().all(|it| it.0 != 1 && (it.0 as usize) < grant.objects.len()));
            assert!(grant.maps.iter().all(|it| it.4 != Access::Shared));
        }
        let len = (rng(&mut state) % 128) as usize;
        let _ = read_grant(&random_bytes(&mut state, len));
    }
}

#[test]
fn a_grant_that_maps_the_control_words_or_swaps_them_is_refused() {
    let mut grant = a_grant();
    grant.maps[0].0 = 1;
    let mut bytes = Vec::new();
    grant.encode(&mut bytes);
    assert!(read_grant(&bytes).is_err());
    let mut grant = a_grant();
    grant.objects.swap(0, 1);
    let mut bytes = Vec::new();
    grant.encode(&mut bytes);
    assert!(read_grant(&bytes).is_err());
    let mut grant = a_grant();
    grant.maps[1].4 = Access::Shared;
    let mut bytes = Vec::new();
    grant.encode(&mut bytes);
    assert!(read_grant(&bytes).is_err());
}

#[test]
fn reserves_round_trip_and_refuse_junk() {
    let reserve = TlsReserve { bias: -0x1_2340, size: 64 << 10, align: 64 };
    assert_eq!(TlsReserve::decode(reserve.encode().as_bytes()), Some(reserve));
    for junk in [&b""[..], b"1 2", b"1 2 3", b"1 2 4 5", b"x 2 4", b"\xff 2 4", b"1 -2 4"] {
        assert_eq!(TlsReserve::decode(junk), None, "{:?}", String::from_utf8_lossy(junk));
    }
}

#[test]
fn tokens_are_32_hex_digits() {
    assert_eq!(crate::load::unhex("000102030405060708090a0b0c0d0e0f"), Some(core::array::from_fn(|it| it as u8)));
    assert_eq!(crate::load::unhex("000102030405060708090a0b0c0d0e0"), None);
    assert_eq!(crate::load::unhex("000102030405060708090a0b0c0d0eFF"), None);
    assert_eq!(crate::load::unhex("zz0102030405060708090a0b0c0d0e0f"), None);
}

#[test]
fn only_the_instructions_arm_allows_are_changed_running() {
    assert!(crate::is_concurrently_modifiable(0x1400_0010));
    assert!(crate::is_concurrently_modifiable(0x9400_0010));
    assert!(crate::is_concurrently_modifiable(0xd503_201f));
    assert!(crate::is_concurrently_modifiable(0xd503_3fdf));
    assert!(crate::is_concurrently_modifiable(0xd420_0000));
    assert!(crate::is_concurrently_modifiable(0xd400_0001));
    assert!(!crate::is_concurrently_modifiable(0x8b02_0020));
    assert!(!crate::is_concurrently_modifiable(0xd65f_03c0));
    assert!(!crate::is_concurrently_modifiable(0x5400_0000));
}

const SUPPORTED: bool = cfg!(any(
    all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")),
    all(target_os = "macos", target_arch = "aarch64"),
    all(windows, target_arch = "x86_64"),
));

/// A code memory, or on a platform without one, the check that saying so is what happens.
fn memory(image: u64, arena: u64) -> Option<CodeMemory> {
    match CodeMemory::new("test", image, arena) {
        Ok(memory) => Some(memory),
        Err(error) => {
            assert!(!SUPPORTED, "{error}");
            assert!(error.what.contains("blaze-inproc runs on"), "{error}");
            None
        }
    }
}

#[test]
fn sizes_round_up_to_pages() {
    let Some(memory) = memory(1, 1) else { return };
    let page = memory.page();
    assert!(page.is_power_of_two());
    assert_eq!(memory.image_len(), page);
    assert_eq!(memory.len(), 2 * page);
    assert!(!memory.is_empty());
    assert!(CodeMemory::new("test", 0, 0).is_err());
    assert!(CodeMemory::new("test", u64::MAX, 1).is_err());
}

#[test]
fn writes_and_words_are_bounds_and_alignment_checked() {
    let Some(mut memory) = memory(1, 1) else { return };
    let len = memory.len();
    memory.write(16, b"hello").unwrap();
    assert_eq!(memory.bytes(CodeRange { offset: 16, len: 5 }).unwrap(), b"hello");
    memory.fill(16, 2, 0xcc).unwrap();
    assert_eq!(memory.bytes(CodeRange { offset: 16, len: 5 }).unwrap(), b"\xcc\xccllo");
    assert!(memory.write(len - 2, b"abc").is_err());
    assert!(memory.write(u64::MAX, b"a").is_err());
    assert!(memory.bytes(CodeRange { offset: len, len: 1 }).is_err());
    memory.store_word(64, 0x1122_3344_5566_7788).unwrap();
    assert_eq!(memory.load_word(64), Ok(0x1122_3344_5566_7788));
    assert!(memory.store_word(65, 1).is_err());
    assert!(memory.store_word(len - 4, 1).is_err());
    assert!(memory.load_word(4).is_err());
    memory.publish(CodeRange { offset: 0, len }).unwrap();
    assert!(memory.publish(CodeRange { offset: len, len: 1 }).is_err());
}

#[test]
fn instruction_stores_are_aarch64s_alone() {
    let Some(mut memory) = memory(1, 1) else { return };
    let nop = 0xd503_201f;
    if cfg!(target_arch = "aarch64") {
        memory.store_instruction(8, nop).unwrap();
        assert_eq!(memory.bytes(CodeRange { offset: 8, len: 4 }).unwrap(), nop.to_le_bytes());
        assert!(memory.store_instruction(10, nop).is_err());
        assert!(memory.store_instruction(8, 0x8b02_0020).is_err());
    } else {
        assert!(memory.store_instruction(8, nop).is_err());
    }
}

#[test]
fn the_arena_hands_out_aligned_ranges_after_the_image() {
    let Some(mut memory) = memory(1, 4) else { return };
    let image = memory.image_len();
    assert!(memory.reserve(0, 16).is_err());
    assert!(memory.reserve(16, 24).is_err());
    let mut taken = Vec::new();
    for align in [16, 64, 256, 4096] {
        let range = memory.reserve(100, align).unwrap();
        assert!(range.offset >= image && range.end() <= memory.len());
        assert!(range.offset.is_multiple_of(align));
        assert!(taken.iter().all(|it: &CodeRange| it.end() <= range.offset || range.end() <= it.offset));
        taken.push(range);
    }
    for range in taken {
        memory.unreserve(range).unwrap();
    }
    let all = memory.reserve(memory.len() - image, 16).unwrap();
    let full = memory.reserve(16, 16).unwrap_err();
    assert!(full.what.contains("arena is full"), "{full}");
    memory.retire(all).unwrap();
    assert_eq!(memory.held(), 0, "with no run process, nothing can be in retired code");
    memory.reserve(16, 16).unwrap();
    assert!(memory.retire(CodeRange { offset: 0, len: 16 }).is_err());
    assert!(memory.unreserve(CodeRange { offset: memory.len(), len: 16 }).is_err());
}

#[test]
fn every_process_maps_the_memory_at_one_base() {
    let Some(mut memory) = memory(1, 1) else { return };
    assert_eq!(memory.base(), None);
    assert_eq!(memory.address(0), None);
    memory.attach(100, 0x7000_0000).unwrap();
    memory.attach(101, 0x7000_0000).unwrap();
    assert!(memory.attach(102, 0x7100_0000).is_err());
    assert_eq!(memory.address(0x10), Some(0x7000_0010));
    assert_eq!(memory.address(memory.len()), None);
    assert_eq!(memory.offset_of(0x7000_0010), Some(0x10));
    assert_eq!(memory.offset_of(0x6fff_ffff), None);
    assert_eq!(memory.offset_of(0x7000_0000 + memory.len()), None);
}

#[test]
fn retired_code_waits_for_the_threads_of_a_live_process() {
    let Some(mut memory) = memory(1, 1) else { return };
    let image = memory.image_len();
    let arena = memory.len() - image;
    let pid = std::process::id();
    memory.attach(pid, 0x7000_0000).unwrap();
    let all = memory.reserve(arena, 16).unwrap();
    memory.retire(all).unwrap();
    assert_eq!(memory.held(), arena);
    // This process's threads never reported to the fence: none of them may have left it.
    assert_eq!(memory.collect(), Ok(0));
    let full = memory.reserve(16, 16).unwrap_err();
    assert!(full.what.contains("held by fences"), "{full}");
    memory.exited(pid);
    assert_eq!(memory.held(), 0);
    memory.reserve(arena, 16).unwrap();
}

#[cfg(all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")))]
mod linux {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::Duration;

    use super::*;
    use crate::channel::Channel;
    use crate::sys::{self, Rights};

    fn temp_dir() -> PathBuf {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        std::env::temp_dir().join(format!("bzi-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)))
    }

    /// A function returning `value`, as bytes.
    fn returning(value: u16) -> Vec<u8> {
        #[cfg(target_arch = "x86_64")]
        {
            let mut code = vec![0xb8];
            code.extend_from_slice(&u32::from(value).to_le_bytes());
            code.push(0xc3);
            code
        }
        #[cfg(target_arch = "aarch64")]
        {
            let mut code = (0x5280_0000u32 | (u32::from(value) << 5)).to_le_bytes().to_vec();
            code.extend_from_slice(&0xd65f_03c0u32.to_le_bytes());
            code
        }
    }

    /// A stub that jumps to the address in the 8-byte word right after it, and where in the
    /// stub that word is. x86-64: `jmp [rip + 0]`; AArch64: `ldr x16, #8`, `br x16`.
    fn stub() -> (Vec<u8>, u64) {
        #[cfg(target_arch = "x86_64")]
        {
            (vec![0xff, 0x25, 0, 0, 0, 0], 6)
        }
        #[cfg(target_arch = "aarch64")]
        {
            let mut code = 0x5800_0050u32.to_le_bytes().to_vec();
            code.extend_from_slice(&0xd61f_0200u32.to_le_bytes());
            (code, 8)
        }
    }

    fn call(address: u64) -> u32 {
        // SAFETY: `address` is the start of code this test wrote whole and published, mapped
        // executable for the rest of the test: a function taking nothing and returning a
        // `u32` in the C ABI (`returning`), or a stub jumping to one (`stub`).
        let function: extern "C" fn() -> u32 = unsafe { std::mem::transmute(address as usize) };
        function()
    }

    #[test]
    fn code_written_here_runs_there_and_a_patch_is_one_store() {
        let mut memory = CodeMemory::new("dual", 1, 1).unwrap();
        let run = memory.object().map_here(Access::Execute).unwrap();
        let (a, b) = (0x200, 0x300);
        let (stub, word) = stub();
        // The word after the stub is 8-aligned.
        let at = 0x108 - word;
        memory.write(a, &returning(42)).unwrap();
        memory.write(b, &returning(43)).unwrap();
        memory.write(at, &stub).unwrap();
        memory.store_word(at + word, run.address() + a).unwrap();
        memory.publish(CodeRange { offset: 0, len: 0x400 }).unwrap();
        assert_eq!(call(run.address() + a), 42);
        assert_eq!(call(run.address() + at), 42);
        memory.store_word(at + word, run.address() + b).unwrap();
        assert_eq!(call(run.address() + at), 43);
        memory.write(a, &returning(44)).unwrap();
        memory.publish(CodeRange { offset: a, len: 16 }).unwrap();
        assert_eq!(call(run.address() + a), 44);
    }

    fn connected() -> (Channel, Channel, u32) {
        let dir = temp_dir();
        let (listener, address) = sys::Listener::bind(&dir).unwrap();
        let loader = sys::connect(&address).unwrap();
        let (daemon, pid) = listener.accept().unwrap();
        drop(listener);
        let _ = std::fs::remove_dir(&dir);
        (Channel::new(daemon), Channel::new(loader), pid)
    }

    #[test]
    fn the_listener_is_this_users_alone_and_knows_its_peer() {
        let dir = temp_dir();
        let (listener, address) = sys::Listener::bind(&dir).unwrap();
        let mode = std::os::unix::fs::PermissionsExt::mode(&std::fs::metadata(&dir).unwrap().permissions());
        assert_eq!(mode & 0o077, 0);
        let _loader = sys::connect(&address).unwrap();
        let (_, pid) = listener.accept().unwrap();
        assert_eq!(pid, std::process::id());
        drop(listener);
        assert!(!std::path::Path::new(&address).exists());
        let _ = std::fs::remove_dir(&dir);

        let open = temp_dir();
        std::fs::create_dir(&open).unwrap();
        std::fs::set_permissions(&open, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        assert!(sys::Listener::bind(&open).is_err());
        let _ = std::fs::remove_dir(&open);
    }

    #[test]
    fn frames_cross_the_channel_whole_in_order() {
        let (mut daemon, mut loader, _) = connected();
        let mut state = 0x77_0000_0001;
        let bodies: Vec<Vec<u8>> = (0..64)
            .map(|index| {
                let len = if index % 16 == 0 { 200_000 } else { (rng(&mut state) % 5000) as usize };
                random_bytes(&mut state, len)
            })
            .collect();
        let sent = bodies.clone();
        let sender = std::thread::spawn(move || {
            for (index, body) in sent.iter().enumerate() {
                loader.send(Kind(0x100 + index as u16), index as u32, body).unwrap();
            }
            loader
        });
        for (index, body) in bodies.iter().enumerate() {
            let frame = daemon.recv().unwrap();
            assert_eq!((frame.kind, frame.seq), (Kind(0x100 + index as u16), index as u32));
            assert_eq!(frame.body, body.as_slice());
        }
        let loader = sender.join().unwrap();
        drop(loader);
        let hung = daemon.recv().unwrap_err();
        assert!(hung.what.contains("hung up"), "{hung}");
    }

    #[test]
    fn a_request_waits_for_its_own_answer() {
        let (mut daemon, mut loader, _) = connected();
        let answerer = std::thread::spawn(move || {
            for _ in 0..3 {
                let (seq, body) = {
                    let frame = daemon.recv().unwrap();
                    (frame.seq, frame.body.to_vec())
                };
                let mut answer = body.clone();
                answer.reverse();
                daemon.send(Kind(0x101), seq, &answer).unwrap();
            }
            let frame = daemon.recv().unwrap();
            let seq = frame.seq;
            daemon.send(Kind(0x101), seq.wrapping_add(1), b"wrong").unwrap();
        });
        for body in [&b"abc"[..], b"", b"hello"] {
            let frame = loader.request(Kind(0x100), body).unwrap();
            let mut want = body.to_vec();
            want.reverse();
            assert_eq!(frame.body, want.as_slice());
        }
        assert!(loader.request(Kind(0x100), b"x").is_err());
        answerer.join().unwrap();
    }

    #[test]
    fn bad_bytes_on_the_channel_are_an_error() {
        let (mut daemon, mut loader, _) = connected();
        let mut header = Header { kind: Kind(1), len: 0, seq: 0 }.encode();
        header[3] = 0x80;
        loader.stream().write_all(&header).unwrap();
        assert!(daemon.recv().is_err());
        let (mut daemon, _loader, _) = connected();
        daemon.set_timeout(Some(Duration::from_millis(50))).unwrap();
        let timed = daemon.recv().unwrap_err();
        assert!(timed.what.contains("timed out"), "{timed}");
    }

    #[test]
    fn the_loader_gets_code_it_can_run_and_never_write() {
        let mut memory = CodeMemory::new("grant", 1, 1).unwrap();
        memory.write(0, &returning(7)).unwrap();
        memory.publish(CodeRange { offset: 0, len: 16 }).unwrap();
        let (mut daemon, mut loader, pid) = connected();
        let grant = Grant {
            format: Format::Elf,
            base: 0,
            image_len: memory.image_len(),
            objects: vec![(memory.len(), GRANT_CODE), (memory.control().len(), GRANT_CONTROL)],
            maps: Vec::new(),
        };
        let mut body = Vec::new();
        grant.encode(&mut body);
        let objects = [(memory.object(), Rights::Code), (memory.control(), Rights::Control)];
        sys::send_objects(&mut daemon, pid, &body, &objects).unwrap();
        let got = {
            let frame = loader.recv().unwrap();
            assert_eq!(frame.kind, kinds::GRANT);
            read_grant(frame.body).unwrap()
        };
        assert_eq!(got, grant);
        let granted = sys::recv_objects(&mut loader, 2).unwrap();
        assert_eq!(granted[0].len(), memory.len());
        assert_eq!(granted[1].len(), memory.control().len());

        let len = memory.len();
        let at = sys::reserve(&mut loader, len, None).unwrap();
        assert!(granted[0].map(&mut loader, 0, len, at, Access::Shared).is_err(), "the image mapped writable shared");
        let code = granted[0].map(&mut loader, 0, len, at, Access::Execute).unwrap();
        assert_eq!(code, at);
        assert_eq!(call(code), 7);
        memory.write(0, &returning(8)).unwrap();
        memory.publish(CodeRange { offset: 0, len: 16 }).unwrap();
        assert_eq!(call(code), 8);
        assert!(granted[0].map(&mut loader, len, 4096, at, Access::Read).is_err());

        let control_len = granted[1].len();
        let control_at = sys::reserve(&mut loader, control_len, None).unwrap();
        let control = granted[1].map(&mut loader, 0, control_len, control_at, Access::Shared).unwrap();
        let words = sys::adopt_words(control, control_len).unwrap();
        assert!(crate::fence::Thread::ready(&words));
        assert_eq!(crate::fence::Thread::epoch(&words), memory.fences.epoch());
    }

    #[test]
    fn a_thread_lists_itself() {
        let mut threads = Vec::new();
        sys::threads(std::process::id(), &mut threads).unwrap();
        assert!(threads.contains(&sys::thread_id()));
        sys::threads(u32::MAX - 1, &mut threads).unwrap();
        assert!(threads.is_empty());
    }

    #[test]
    fn a_backtrace_ends_at_the_outermost_frame() {
        let mut frames = [0u64; crate::fence::MOST_FRAMES];
        let len = sys::backtrace(&mut frames).unwrap();
        assert!(len > 1);
        let mut short = [0u64; 1];
        assert_eq!(sys::backtrace(&mut short), None);
    }

    #[cfg(feature = "host")]
    #[test]
    fn the_host_hands_a_loader_its_program() {
        use blaze_core::Sandbox;

        use crate::host::{Host, Launch};
        use crate::load::{HOST_VAR, VERSION, unhex};

        let dir = temp_dir();
        let loader_path = PathBuf::from("/blaze/libexec/blaze-inproc-run");
        let reserve = TlsReserve { bias: -0x1_0000, size: 64 << 10, align: 64 };
        let host = Host::new(&dir, loader_path.clone(), reserve).unwrap();
        assert_eq!(host.reserve(), reserve);
        let memory = CodeMemory::new("host", 1, 1).unwrap();
        let launch = Launch {
            image: &memory,
            format: Format::Elf,
            maps: Vec::new(),
            argv0: "prog".into(),
            args: vec!["a".into(), "b".into()],
            env: vec![("X".into(), "1".into()), (HOST_VAR.into(), "stale".into())],
            dir: dir.clone(),
            stdin: Vec::new(),
            sandbox: Sandbox::On { read: Vec::new(), write: Vec::new(), network: false, trace: false },
            memory: None,
        };
        let (command, pending) = host.launch(&launch).unwrap();
        assert_eq!(command.program, loader_path);
        assert_eq!(command.args, ["prog", "a", "b"]);
        let vars: Vec<_> = command.env.iter().filter(|(name, _)| name == HOST_VAR).collect();
        assert_eq!(vars.len(), 1);
        assert!(command.env.iter().any(|(name, value)| name == "X" && value == "1"));
        let var = vars[0].1.to_str().unwrap().to_owned();
        let (address, token) = var.split_once('\t').unwrap();
        let Sandbox::On { read, write, network, .. } = &command.sandbox else { panic!("the run left its sandbox") };
        assert!(read.contains(&loader_path));
        assert_eq!(write, &[PathBuf::from(address)]);
        assert!(!network);

        let token = unhex(token).unwrap();
        let mut channel = Channel::new(sys::connect(address).unwrap());
        let mut hello = Vec::new();
        Writer::new(&mut hello)
            .raw(&token)
            .u32(std::process::id())
            .u32(VERSION)
            .u64(sys::page())
            .i64(reserve.bias)
            .u64(reserve.size)
            .u64(reserve.align);
        channel.send(kinds::HELLO, 0, &hello).unwrap();
        let grant = {
            let frame = channel.recv().unwrap();
            assert_eq!(frame.kind, kinds::GRANT);
            read_grant(frame.body).unwrap()
        };
        assert_eq!(grant.image_len, memory.image_len());
        let granted = sys::recv_objects(&mut channel, grant.objects.len()).unwrap();
        let at = sys::reserve(&mut channel, granted[0].len(), None).unwrap();
        let base = granted[0].map(&mut channel, 0, granted[0].len(), at, Access::Execute).unwrap();
        let mut started = Vec::new();
        Writer::new(&mut started).u64(base).u64(sys::thread_id());
        channel.send(kinds::STARTED, 0, &started).unwrap();
        let session = pending.wait(Duration::from_secs(10)).unwrap();
        assert_eq!((session.pid, session.base, session.thread), (std::process::id(), base, sys::thread_id()));

        let (_, stale) = host.launch(&launch).unwrap();
        let mut wrong = Channel::new(sys::connect(address).unwrap());
        let mut hello = hello.clone();
        hello[0] ^= 1;
        wrong.send(kinds::HELLO, 0, &hello).unwrap();
        assert!(stale.wait(Duration::from_millis(300)).is_err());
    }
}
