use std::time::Duration;

use super::*;
use crate::frame::Reader;

fn granule() -> u64 {
    page()
}

#[test]
fn the_granule_is_64_kib() {
    assert_eq!(page(), 64 << 10);
}

#[test]
fn two_views_of_one_object_see_one_memory() {
    let object = Object::create("test", 2 * granule()).unwrap();
    let mut writer = object.map_here(Access::Shared).unwrap();
    let reader = object.duplicate().unwrap().map_here(Access::Read).unwrap();
    assert_ne!(writer.address(), reader.address());
    assert!(reader.bytes(0..16).unwrap().iter().all(|it| *it == 0));
    writer.bytes_mut(granule() as usize - 4..granule() as usize + 4).unwrap().copy_from_slice(b"blazing!");
    assert_eq!(reader.bytes(granule() as usize - 4..granule() as usize + 4).unwrap(), b"blazing!");
    writer.word(8).store(0x1234_5678_9abc_def0, std::sync::atomic::Ordering::Release);
    assert_eq!(reader.word(8).load(std::sync::atomic::Ordering::Acquire), 0x1234_5678_9abc_def0);
    writer.publish(0..64);
}

#[test]
fn a_read_view_refuses_writes_and_out_of_bounds() {
    let object = Object::create("test", granule()).unwrap();
    let mut view = object.map_here(Access::Read).unwrap();
    assert!(view.bytes_mut(0..1).is_err());
    assert!(view.bytes(0..granule() as usize + 1).is_err());
    #[allow(clippy::reversed_empty_ranges)]
    let backwards = 8..4;
    assert!(view.bytes(backwards).is_err());
}

#[test]
fn a_copy_is_private_and_can_be_made_read_only() {
    let object = Object::create("test", granule()).unwrap();
    let mut shared = object.map_here(Access::Shared).unwrap();
    let mut copy = object.map_here(Access::Copy).unwrap();
    shared.bytes_mut(0..4).unwrap().copy_from_slice(b"daem");
    assert_eq!(copy.bytes(0..4).unwrap(), b"daem");
    copy.bytes_mut(0..4).unwrap().copy_from_slice(b"mine");
    assert_eq!(shared.bytes(0..4).unwrap(), b"daem");
    assert_eq!(copy.bytes(0..4).unwrap(), b"mine");
    protect(copy.address(), 4096, Access::Read).unwrap();
    assert!(protect(copy.address(), 4096, Access::Execute).is_err());
    assert!(protect(copy.address(), 4096, Access::Shared).is_err());
}

#[test]
fn finding_a_placeholder() {
    let list = [(0x10_0000, 0x4_0000), (0x20_0000, 0x1_0000)];
    assert_eq!(find(&list, 0x10_0000, 0x4_0000).unwrap(), Some(0));
    assert_eq!(find(&list, 0x11_0000, 0x1_0000).unwrap(), Some(0));
    assert_eq!(find(&list, 0x20_0000, 0x1_0000).unwrap(), Some(1));
    assert_eq!(find(&list, 0x30_0000, 0x1_0000).unwrap(), None);
    assert!(find(&list, 0x13_0000, 0x2_0000).is_err());
    assert!(find(&list, 0x0f_0000, 0x2_0000).is_err());
    assert!(find(&list, u64::MAX - 4, 0x1_0000).is_err());
}

#[test]
fn cutting_a_placeholder() {
    assert_eq!(cut((0x10_0000, 0x4_0000), 0x10_0000, 0x4_0000), (0, 0));
    assert_eq!(cut((0x10_0000, 0x4_0000), 0x11_0000, 0x1_0000), (0x1_0000, 0x2_0000));
    assert_eq!(cut((0x10_0000, 0x4_0000), 0x10_0000, 0x1_0000), (0, 0x3_0000));
    assert_eq!(cut((0x10_0000, 0x4_0000), 0x13_0000, 0x1_0000), (0x3_0000, 0));
}

#[test]
fn views_go_on_granules_only() {
    assert!(granular(0, granule(), 0).is_ok());
    assert!(granular(0, 0, 0).is_err());
    assert!(granular(4096, granule(), 0).is_err());
    assert!(granular(0, 4096, 0).is_err());
    assert!(granular(0, granule(), 4096).is_err());
}

#[test]
fn rights_bound_what_a_run_process_maps() {
    assert!(allowed(Rights::Code, Access::Execute));
    assert!(allowed(Rights::Code, Access::Read));
    assert!(allowed(Rights::Code, Access::Copy));
    assert!(!allowed(Rights::Code, Access::Shared));
    assert!(allowed(Rights::Control, Access::Shared));
    assert!(allowed(Rights::Control, Access::Read));
    assert!(!allowed(Rights::Control, Access::Execute));
    assert!(!allowed(Rights::Control, Access::Copy));
}

/// A connected pair: the daemon's end (with the peer's ID) and the loader's.
fn pair() -> ((Channel, u32), Channel) {
    let dir = std::env::temp_dir();
    let (listener, address) = Listener::bind(&dir).unwrap();
    let accepting = std::thread::spawn(move || listener.accept().unwrap());
    let loader = Channel::new(connect(&address).unwrap());
    let (stream, pid) = accepting.join().unwrap();
    ((Channel::new(stream), pid), loader)
}

#[test]
fn the_pipe_carries_frames_and_names_the_peer() {
    let ((mut daemon, pid), mut loader) = pair();
    assert_eq!(pid, std::process::id());
    loader.send(kinds::HELLO, 7, b"hello").unwrap();
    let frame = daemon.recv().unwrap();
    assert_eq!((frame.kind, frame.seq, frame.body), (kinds::HELLO, 7, &b"hello"[..]));
    let long = vec![0x5a; 300 << 10];
    daemon.send(kinds::GRANT, 0, &long).unwrap();
    assert_eq!(loader.recv().unwrap().body, &long[..]);
}

#[test]
fn a_read_times_out_and_the_pipe_still_works() {
    let ((mut daemon, _), mut loader) = pair();
    daemon.set_timeout(Some(Duration::from_millis(50))).unwrap();
    let error = daemon.recv().unwrap_err();
    assert!(error.what.contains("nothing within"), "{error}");
    loader.send(kinds::HELLO, 1, b"late").unwrap();
    assert_eq!(daemon.recv().unwrap().body, b"late");
}

#[test]
fn a_hang_up_is_said() {
    let ((mut daemon, _), loader) = pair();
    drop(loader);
    let error = daemon.recv().unwrap_err();
    assert!(error.what.contains("hung up"), "{error}");
}

#[test]
fn only_pipes_of_blaze_inproc_are_opened() {
    assert!(connect(r"C:\Windows\notepad.exe").is_err());
    assert!(connect(r"\\.\pipe\someone-else").is_err());
}

#[test]
fn a_raw_handle_must_be_a_pipe() {
    assert!(Stream::from_raw(0).is_err());
    assert!(Stream::from_raw(-1).is_err());
}

/// Serves MAP frames as the host does, until the loader hangs up.
fn serve(mut channel: Channel, remote: Remote) {
    loop {
        let Ok(frame) = channel.recv() else { return };
        let (kind, seq, body) = (frame.kind, frame.seq, frame.body.to_vec());
        assert_eq!(kind, kinds::MAP);
        let mut read = Reader::new(&body);
        let (object, offset, len, at) =
            (read.u32().unwrap(), read.u64().unwrap(), read.u64().unwrap(), read.u64().unwrap());
        let access = Access::from_tag(read.u8().unwrap()).unwrap();
        let mut answer = Vec::new();
        match remote.map(object, offset, len, at, access) {
            Ok(address) => {
                Writer::new(&mut answer).u64(address);
                channel.send(kinds::MAPPED, seq, &answer).unwrap();
            }
            Err(error) => {
                Writer::new(&mut answer).str(&error.to_string());
                channel.send(kinds::REFUSED, seq, &answer).unwrap();
            }
        }
    }
}

#[test]
fn the_daemon_maps_into_the_loaders_placeholders() {
    let ((mut daemon, pid), mut loader) = pair();
    let image = Object::create("image", 4 * granule()).unwrap();
    let control = Object::create("control", granule()).unwrap();
    let mut view = image.map_here(Access::Shared).unwrap();
    view.bytes_mut(0..4).unwrap().copy_from_slice(b"code");
    view.bytes_mut(2 * granule() as usize..2 * granule() as usize + 4).unwrap().copy_from_slice(b"data");
    let remote = send_objects(&mut daemon, pid, b"grant", &[(&image, Rights::Code), (&control, Rights::Control)]).unwrap();
    let serving = std::thread::spawn(move || serve(daemon, remote));

    let frame = loader.recv().unwrap();
    assert_eq!((frame.kind, frame.seq, frame.body), (kinds::GRANT, 0, &b"grant"[..]));
    let objects = recv_objects(&mut loader, 2).unwrap();
    assert_eq!((objects[0].len(), objects[1].len()), (4 * granule(), granule()));

    let base = reserve(&mut loader, 4 * granule(), None).unwrap();
    assert!(base.is_multiple_of(granule()));
    // Code and data in one reservation, cut to fit: first the middle, then the ends.
    let at = objects[0].map(&mut loader, 2 * granule(), granule(), base + 2 * granule(), Access::Copy).unwrap();
    assert_eq!(at, base + 2 * granule());
    assert_eq!(objects[0].map(&mut loader, 0, 2 * granule(), base, Access::Execute).unwrap(), base);
    assert_eq!(
        objects[0].map(&mut loader, 3 * granule(), granule(), base + 3 * granule(), Access::Execute).unwrap(),
        base + 3 * granule()
    );
    // SAFETY: the views were just mapped readable at these addresses and stay for the test.
    let (code, data) = unsafe { (*(base as *const [u8; 4]), *((base + 2 * granule()) as *const [u8; 4])) };
    assert_eq!((&code, &data), (b"code", b"data"));
    // The daemon's later writes show through the shared code view.
    view.bytes_mut(4..8).unwrap().copy_from_slice(b"more");
    // SAFETY: as above.
    assert_eq!(unsafe { *((base + 4) as *const [u8; 4]) }, *b"more");

    // Mapping over what is mapped, writable code, executable control words: refused.
    assert!(objects[0].map(&mut loader, 0, granule(), base, Access::Execute).is_err());
    let spare = reserve(&mut loader, granule(), None).unwrap();
    assert!(objects[0].map(&mut loader, 0, granule(), spare, Access::Shared).is_err());
    assert!(objects[1].map(&mut loader, 0, granule(), spare, Access::Execute).is_err());
    // A map larger than its placeholder is refused here; one past the object's end by the
    // daemon. Either leaves the placeholder there to map into.
    assert!(objects[0].map(&mut loader, 0, 2 * granule(), spare, Access::Read).is_err());
    assert!(objects[0].map(&mut loader, 4 * granule(), granule(), spare, Access::Read).is_err());
    let words = objects[1].map(&mut loader, 0, granule(), spare, Access::Shared).unwrap();
    assert_eq!(words, spare);

    let whole = objects[0].view(&mut loader).unwrap();
    assert_eq!(whole.bytes(0..8).unwrap(), b"codemore");
    assert_eq!(whole.bytes(2 * granule() as usize..2 * granule() as usize + 4).unwrap(), b"data");
    drop(loader);
    serving.join().unwrap();
}

#[test]
fn a_listed_thread_is_one_of_ours() {
    let mut threads = Vec::new();
    super::threads(std::process::id(), &mut threads).unwrap();
    assert!(threads.contains(&thread_id()));
    let other = std::thread::spawn(thread_id).join().unwrap();
    assert_ne!(other, thread_id());
    super::threads(u32::MAX - 3, &mut threads).unwrap();
    assert!(threads.is_empty());
}

#[test]
fn a_backtrace_reaches_the_outermost_frame() {
    let mut frames = [0u64; 512];
    let len = backtrace(&mut frames).unwrap();
    assert!(len > 2, "{len} frames");
    assert!(frames[..len].iter().all(|it| *it != 0));
    let mut few = [0u64; 1];
    assert_eq!(backtrace(&mut few), None);
}

#[test]
fn random_bytes_differ() {
    let (mut a, mut b) = ([0u8; 16], [0u8; 16]);
    random(&mut a).unwrap();
    random(&mut b).unwrap();
    assert_ne!(a, b);
}

#[test]
fn serializing_returns() {
    serialize();
}
