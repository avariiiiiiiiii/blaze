// blaze's fork server, the harness side of the protocol in `wire.rs`, which comes before this
// in the same module. Text only: `blaze_usercode::test::FORK_SERVER` puts both in the test
// harness blaze writes, as the module `fork_server` beside the harness's `main`, where
// `super::test` is libtest. Never a module of blaze-usercode. It is compiled in the tested
// crate's edition, so it names everything from `::std` and `::core`.

/// Serves `tests` to blaze over the socket at `socket` (the value of [`SOCKET`]), then exits:
/// what the harness's `main` calls when the variable is set.
#[cfg(unix)]
pub fn serve(tests: &'static [&'static super::test::TestDescAndFn], socket: ::std::ffi::OsString) -> ! {
    let code = match unix::serve(tests, &socket) {
        Ok(()) => 0,
        Err(why) => {
            eprintln!("blaze's fork server: {}", why);
            101
        }
    };
    ::std::process::exit(code)
}

#[cfg(not(unix))]
pub fn serve(tests: &'static [&'static super::test::TestDescAndFn], socket: ::std::ffi::OsString) -> ! {
    let _ = (tests, socket);
    eprintln!("blaze's fork server: this platform has no fork; the package must run its tests as libtest's threads");
    ::std::process::exit(101)
}

#[cfg(unix)]
mod unix {
    use super::super::test;
    use super::{End, Frame, Frames, LONGEST_OUTPUT, MARK, SOCKET, VERSION, encode_ended};
    use ::std::io::{Read, Write};
    use ::std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd};
    use ::std::os::unix::net::UnixStream;

    type Int = ::core::ffi::c_int;
    #[cfg(target_os = "linux")]
    type Nfds = ::core::ffi::c_ulong;
    #[cfg(not(target_os = "linux"))]
    type Nfds = ::core::ffi::c_uint;

    #[repr(C)]
    struct PollFd {
        fd: Int,
        events: i16,
        revents: i16,
    }

    const POLLIN: i16 = 0x1;
    const WNOHANG: Int = 1;
    const SIGKILL: Int = 9;

    unsafe extern "C" {
        fn fork() -> Int;
        fn waitpid(pid: Int, status: *mut Int, options: Int) -> Int;
        fn kill(pid: Int, signal: Int) -> Int;
        fn setpgid(pid: Int, group: Int) -> Int;
        fn dup2(from: Int, to: Int) -> Int;
        fn close(fd: Int) -> Int;
        fn poll(fds: *mut PollFd, count: Nfds, timeout: Int) -> Int;
        fn access(path: *const ::core::ffi::c_char, mode: Int) -> Int;
        fn _exit(code: Int) -> !;
    }

    /// A test's child: its process (the leader of its own group), where its output comes in,
    /// and how it ended once it has.
    struct Child {
        id: u64,
        pid: Int,
        output: UnixStream,
        bytes: Vec<u8>,
        cut: bool,
        open: bool,
        status: Option<Int>,
        /// Readable once the child has ended, where each child has one (Linux's pidfd).
        exit: Option<OwnedFd>,
    }

    // What wakes the server the moment a child ends, so it never waits on a clock: a pidfd
    // per child on Linux, one kqueue for all of them on Apple's systems.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    mod exits {
        use super::{FromRawFd, Int, OwnedFd};

        unsafe extern "C" {
            fn syscall(number: ::core::ffi::c_long, ...) -> ::core::ffi::c_long;
        }

        /// `pidfd_open`, the same number on every Linux architecture.
        const PIDFD_OPEN: ::core::ffi::c_long = 434;

        pub fn queue() -> Result<Option<OwnedFd>, String> {
            Ok(None)
        }

        /// The child's pidfd, which is close-on-exec. `pid` is not reaped yet, so it is there
        /// even if it has ended.
        pub fn watch(_queue: &Option<OwnedFd>, pid: Int) -> Result<(Option<OwnedFd>, bool), String> {
            // SAFETY: `pidfd_open(pid, 0)` takes two integers and returns a descriptor or -1.
            let fd = unsafe { syscall(PIDFD_OPEN, pid as ::core::ffi::c_long, 0 as ::core::ffi::c_long) };
            if fd < 0 {
                return Err(format!("pidfd_open: {}", ::std::io::Error::last_os_error()));
            }
            // SAFETY: the descriptor was just made and nothing else owns it.
            Ok((Some(unsafe { OwnedFd::from_raw_fd(fd as Int) }), false))
        }

        pub fn clear(_queue: &Option<OwnedFd>) {}
    }

    #[cfg(any(target_os = "macos", target_os = "ios", target_os = "tvos", target_os = "watchos", target_os = "visionos"))]
    mod exits {
        use super::{AsRawFd, FromRawFd, Int, OwnedFd};

        #[repr(C)]
        struct Kevent {
            ident: usize,
            filter: i16,
            flags: u16,
            fflags: u32,
            data: isize,
            udata: *mut ::core::ffi::c_void,
        }

        #[repr(C)]
        struct Timespec {
            seconds: ::core::ffi::c_long,
            nanoseconds: ::core::ffi::c_long,
        }

        const EVFILT_PROC: i16 = -5;
        const EV_ADD: u16 = 0x1;
        const EV_ONESHOT: u16 = 0x10;
        const NOTE_EXIT: u32 = 0x8000_0000;
        const ESRCH: i32 = 3;

        unsafe extern "C" {
            fn kqueue() -> Int;
            fn kevent(
                queue: Int,
                changes: *const Kevent,
                changed: Int,
                events: *mut Kevent,
                most: Int,
                timeout: *const Timespec,
            ) -> Int;
        }

        /// A kqueue, which a forked child doesn't inherit.
        pub fn queue() -> Result<Option<OwnedFd>, String> {
            // SAFETY: a plain call.
            let fd = unsafe { kqueue() };
            if fd < 0 {
                return Err(format!("kqueue: {}", ::std::io::Error::last_os_error()));
            }
            // SAFETY: the descriptor was just made and nothing else owns it.
            Ok(Some(unsafe { OwnedFd::from_raw_fd(fd) }))
        }

        /// Asks the kqueue for `pid`'s end. A child that has already ended can't be watched
        /// (`ESRCH`): the server looks at once instead.
        pub fn watch(queue: &Option<OwnedFd>, pid: Int) -> Result<(Option<OwnedFd>, bool), String> {
            let queue = queue.as_ref().expect("a kqueue on this system");
            let change = Kevent {
                ident: pid as usize,
                filter: EVFILT_PROC,
                flags: EV_ADD | EV_ONESHOT,
                fflags: NOTE_EXIT,
                data: 0,
                udata: ::core::ptr::null_mut(),
            };
            // SAFETY: one change from a live struct, no events back.
            let done = unsafe { kevent(queue.as_raw_fd(), &change, 1, ::core::ptr::null_mut(), 0, ::core::ptr::null()) };
            if done < 0 {
                let error = ::std::io::Error::last_os_error();
                if error.raw_os_error() == Some(ESRCH) {
                    return Ok((None, true));
                }
                return Err(format!("kevent: {}", error));
            }
            Ok((None, false))
        }

        /// Takes the ends the kqueue has, so it is not readable for them again; `waitpid`
        /// says which children they were.
        pub fn clear(queue: &Option<OwnedFd>) {
            let queue = queue.as_ref().expect("a kqueue on this system");
            let zero = Timespec { seconds: 0, nanoseconds: 0 };
            // SAFETY: integers and a raw pointer, for which zero is a value.
            let mut events: [Kevent; 16] = unsafe { ::core::mem::zeroed() };
            loop {
                // SAFETY: `events` holds 16 entries, and the call doesn't wait.
                let got = unsafe { kevent(queue.as_raw_fd(), ::core::ptr::null(), 0, events.as_mut_ptr(), 16, &zero) };
                if got < 16 {
                    return;
                }
            }
        }
    }

    #[cfg(not(any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "ios",
        target_os = "tvos",
        target_os = "watchos",
        target_os = "visionos"
    )))]
    mod exits {
        use super::{Int, OwnedFd};

        pub fn queue() -> Result<Option<OwnedFd>, String> {
            Err("blaze's fork server waits on a pidfd (Linux) or a kqueue (Apple), and this system has neither it knows; the package must run its tests as libtest's threads".to_owned())
        }

        pub fn watch(_queue: &Option<OwnedFd>, _pid: Int) -> Result<(Option<OwnedFd>, bool), String> {
            Err("no way to wait on a child here".to_owned())
        }

        pub fn clear(_queue: &Option<OwnedFd>) {}
    }

    pub fn serve(tests: &'static [&'static test::TestDescAndFn], socket: &::std::ffi::OsStr) -> Result<(), String> {
        // Taken out before any test runs, so a test that runs its own binary gets libtest.
        // SAFETY: the server has one thread, and nothing reads the environment as it changes.
        unsafe {
            ::std::env::remove_var(SOCKET);
        }
        let queue = exits::queue()?;
        // A child ended before its end could be watched: the next poll doesn't wait.
        let mut now = false;
        let mut stream = connect(socket)?;
        let names: Vec<String> = tests.iter().map(|it| it.desc.name.as_slice().to_owned()).collect();
        send(&mut stream, &Frame::Hello { version: VERSION, pid: ::std::process::id(), tests: names })?;
        let mut frames = Frames::new();
        let mut children: Vec<Child> = Vec::new();
        let mut quitting = false;
        let mut buffer = vec![0u8; 1 << 16];
        // Kept across turns of the loop, so a turn allocates nothing: what is polled, which
        // children's outputs those are, the frames to send (one write a turn), and the output
        // buffers of children that ended, for the next ones.
        let mut watched: Vec<usize> = Vec::new();
        let mut fds: Vec<PollFd> = Vec::new();
        let mut out: Vec<u8> = Vec::new();
        let mut spare: Vec<Vec<u8>> = Vec::new();
        loop {
            if quitting && children.is_empty() {
                return Ok(());
            }
            watched.clear();
            watched.extend((0..children.len()).filter(|&at| children[at].open));
            fds.clear();
            fds.push(PollFd { fd: stream.as_raw_fd(), events: POLLIN, revents: 0 });
            for &at in &watched {
                fds.push(PollFd { fd: children[at].output.as_raw_fd(), events: POLLIN, revents: 0 });
            }
            if let Some(queue) = &queue {
                fds.push(PollFd { fd: queue.as_raw_fd(), events: POLLIN, revents: 0 });
            }
            for child in children.iter().filter(|it| it.status.is_none()) {
                if let Some(exit) = &child.exit {
                    fds.push(PollFd { fd: exit.as_raw_fd(), events: POLLIN, revents: 0 });
                }
            }
            let timeout = if now { 0 } else { -1 };
            now = false;
            // SAFETY: `fds` holds `fds.len()` entries for the length of the call.
            let ready = unsafe { poll(fds.as_mut_ptr(), fds.len() as Nfds, timeout) };
            if ready < 0 {
                let error = ::std::io::Error::last_os_error();
                if error.kind() == ::std::io::ErrorKind::Interrupted {
                    continue;
                }
                kill_all(&mut children);
                return Err(format!("poll: {}", error));
            }
            if fds[0].revents != 0 {
                match stream.read(&mut buffer) {
                    Ok(0) => {
                        // blaze is gone: nothing will read what the tests say.
                        kill_all(&mut children);
                        return Ok(());
                    }
                    Ok(n) => frames.push(&buffer[..n]),
                    Err(ref error) if error.kind() == ::std::io::ErrorKind::Interrupted => {}
                    Err(error) => {
                        kill_all(&mut children);
                        return Err(format!("reading the socket: {}", error));
                    }
                }
                loop {
                    let frame = match frames.pop() {
                        Ok(Some(frame)) => frame,
                        Ok(None) => break,
                        Err(why) => {
                            kill_all(&mut children);
                            return Err(why);
                        }
                    };
                    match frame {
                        Frame::Run { id, index } => match start(tests, id, index, &stream, &children, spare.pop().unwrap_or_default()) {
                            Ok(mut child) => {
                                match exits::watch(&queue, child.pid) {
                                    Ok((exit, ended)) => {
                                        child.exit = exit;
                                        now |= ended;
                                    }
                                    Err(why) => {
                                        children.push(child);
                                        kill_all(&mut children);
                                        return Err(why);
                                    }
                                }
                                children.push(child);
                            }
                            Err(why) => encode_ended(&mut out, id, &End::NotStarted(why), &[]),
                        },
                        Frame::Kill { id } => {
                            if let Some(child) = children.iter().find(|it| it.id == id && it.status.is_none()) {
                                kill_group(child.pid);
                            }
                        }
                        Frame::Quit => quitting = true,
                        other => {
                            kill_all(&mut children);
                            return Err(format!("blaze sent {:?}, which only a server sends", other));
                        }
                    }
                }
            }
            for (slot, &at) in watched.iter().enumerate() {
                if fds[slot + 1].revents != 0 {
                    drain(&mut children[at]);
                }
            }
            if queue.is_some() && fds[watched.len() + 1].revents != 0 {
                exits::clear(&queue);
            }
            reap(&mut children);
            let mut at = 0;
            while at < children.len() {
                let status = match children[at].status {
                    Some(status) => status,
                    None => {
                        at += 1;
                        continue;
                    }
                };
                let mut child = children.swap_remove(at);
                // What it wrote before it ended is in the socket's buffer now; a process it
                // left behind that writes later is not waited for.
                drain(&mut child);
                if child.cut {
                    let note = format!("\n[blaze: the output was cut at {} bytes]\n", LONGEST_OUTPUT);
                    child.bytes.extend_from_slice(note.as_bytes());
                }
                encode_ended(&mut out, child.id, &end_of(status), &child.bytes);
                // A buffer that held a long output is let go rather than kept at that size.
                if child.bytes.capacity() <= KEPT {
                    child.bytes.clear();
                    spare.push(child.bytes);
                }
            }
            // Every end of this turn in one write: blaze reads them as one batch.
            if !out.is_empty() {
                if let Err(error) = stream.write_all(&out) {
                    kill_all(&mut children);
                    return Err(format!("writing to the socket: {}", error));
                }
                out.clear();
                if out.capacity() > KEPT {
                    out.shrink_to(KEPT);
                }
            }
        }
    }

    /// The most bytes a buffer of output or of frames keeps between uses.
    const KEPT: usize = 1 << 20;

    fn connect(socket: &::std::ffi::OsStr) -> Result<UnixStream, String> {
        // blaze listens as it starts the binary; the listen may be made a moment after.
        let mut tries = 0;
        loop {
            match UnixStream::connect(socket) {
                Ok(stream) => return Ok(stream),
                Err(ref error)
                    if tries < 400
                        && (error.kind() == ::std::io::ErrorKind::NotFound
                            || error.kind() == ::std::io::ErrorKind::ConnectionRefused) =>
                {
                    tries += 1;
                    ::std::thread::sleep(::std::time::Duration::from_millis(25));
                }
                Err(error) => return Err(format!("connecting to {:?}: {}", socket, error)),
            }
        }
    }

    fn send(stream: &mut UnixStream, frame: &Frame) -> Result<(), String> {
        stream.write_all(&frame.to_bytes()).map_err(|error| format!("writing to the socket: {}", error))
    }

    fn start(
        tests: &'static [&'static test::TestDescAndFn],
        id: u64,
        index: u32,
        stream: &UnixStream,
        children: &[Child],
        bytes: Vec<u8>,
    ) -> Result<Child, String> {
        let test: &'static test::TestDescAndFn = match tests.get(index as usize) {
            Some(test) => *test,
            None => return Err(format!("the binary has {} tests, and no test {}", tests.len(), index)),
        };
        let (ours, theirs) =
            UnixStream::pair().map_err(|error| format!("a socket pair for the test's output: {}", error))?;
        // SAFETY: the server runs on one thread, so the child is a whole copy of it.
        let pid = unsafe { fork() };
        if pid < 0 {
            return Err(format!("fork: {}", ::std::io::Error::last_os_error()));
        }
        if pid == 0 {
            // The child closes the server's descriptors under the owners that hold them: it
            // never uses those owners again, since it leaves by `_exit` below, which runs no
            // destructor, so no descriptor is closed twice or used after its close.
            // SAFETY: integers only; the child leads a group of its own.
            unsafe { setpgid(0, 0) };
            // SAFETY: as above, the socket's owner is not used in the child.
            unsafe { close(stream.as_raw_fd()) };
            // SAFETY: as above.
            unsafe { close(ours.as_raw_fd()) };
            for child in children {
                // SAFETY: as above, the other children's owners are not used in this one.
                unsafe { close(child.output.as_raw_fd()) };
                if let Some(exit) = &child.exit {
                    // SAFETY: as above.
                    unsafe { close(exit.as_raw_fd()) };
                }
            }
            // SAFETY: `theirs` is open until the close below; 1 and 2 become copies of it.
            unsafe { dup2(theirs.as_raw_fd(), 1) };
            // SAFETY: as above.
            unsafe { dup2(theirs.as_raw_fd(), 2) };
            if theirs.as_raw_fd() > 2 {
                // SAFETY: as above, `theirs` is not used in the child after this; 1 and 2 stay.
                unsafe { close(theirs.as_raw_fd()) };
            }
            let code = run(test);
            let _ = ::std::io::stdout().flush();
            let _ = ::std::io::stderr().flush();
            // SAFETY: ends this process without running the server's exit handlers twice.
            unsafe { _exit(code) }
        }
        // Made by both, so a kill sent at once finds the group.
        // SAFETY: a plain call.
        unsafe { setpgid(pid, pid) };
        drop(theirs);
        ours.set_nonblocking(true).map_err(|error| format!("the test's output socket: {}", error))?;
        Ok(Child { id: id, pid: pid, output: ours, bytes: bytes, cut: false, open: true, status: None, exit: None })
    }

    fn drain(child: &mut Child) {
        let mut buffer = [0u8; 1 << 14];
        while child.open {
            match child.output.read(&mut buffer) {
                Ok(0) => child.open = false,
                Ok(n) => {
                    let room = LONGEST_OUTPUT.saturating_sub(child.bytes.len());
                    if n > room {
                        child.cut = true;
                    }
                    child.bytes.extend_from_slice(&buffer[..n.min(room)]);
                }
                Err(ref error) if error.kind() == ::std::io::ErrorKind::WouldBlock => break,
                Err(ref error) if error.kind() == ::std::io::ErrorKind::Interrupted => {}
                Err(_) => child.open = false,
            }
        }
    }

    fn reap(children: &mut [Child]) {
        loop {
            let mut status: Int = 0;
            // SAFETY: `status` is a live `int`.
            let pid = unsafe { waitpid(-1, &mut status, WNOHANG) };
            if pid <= 0 {
                return;
            }
            if let Some(child) = children.iter_mut().find(|it| it.pid == pid) {
                child.status = Some(status);
            }
        }
    }

    fn end_of(status: Int) -> End {
        match status & 0x7f {
            0 => End::Exited((status >> 8) & 0xff),
            signal => End::Signaled(signal),
        }
    }

    fn kill_group(pid: Int) {
        // SAFETY: integers only. `pid` is a child not yet reaped (callers pass only those with
        // no status), so neither it nor its group can be another process's.
        unsafe { kill(-pid, SIGKILL) };
        // SAFETY: as above.
        unsafe { kill(pid, SIGKILL) };
    }

    fn kill_all(children: &mut Vec<Child>) {
        for child in children.iter().filter(|it| it.status.is_none()) {
            kill_group(child.pid);
        }
        for child in children.iter().filter(|it| it.status.is_none()) {
            let mut status: Int = 0;
            // SAFETY: `status` is a live `int`.
            unsafe { waitpid(child.pid, &mut status, 0) };
        }
        children.clear();
    }

    /// Ends the child, as a function of `()` so a panic hook can end with it.
    fn exit_now(code: Int) {
        // SAFETY: ends this process; nothing of it is needed after.
        unsafe { _exit(code) }
    }

    fn mark(name: &str) {
        let mut path = Vec::with_capacity(MARK.len() + name.len() + 1);
        path.extend_from_slice(MARK.as_bytes());
        path.extend_from_slice(name.as_bytes());
        path.push(0);
        // SAFETY: `path` ends with a nul.
        unsafe { access(path.as_ptr() as *const ::core::ffi::c_char, 0) };
    }

    #[derive(Clone)]
    enum Panics {
        No,
        Yes,
        WithMessage(&'static str),
    }

    enum Body {
        Test(fn() -> Result<(), String>),
        Bench(fn(&mut test::Bencher) -> Result<(), String>),
    }

    enum Ran<'a> {
        Returned,
        /// The test returned an error: libtest judges its message as a panic's.
        Errored(&'a str),
        Panicked(Option<&'a str>),
    }

    fn payload_str(payload: &(dyn ::std::any::Any + Send)) -> Option<&str> {
        if let Some(text) = payload.downcast_ref::<String>() {
            return Some(text.as_str());
        }
        payload.downcast_ref::<&'static str>().map(|text| *text)
    }

    /// The exit code of a test that ran as `ran`: 0 for a pass as libtest's `calc_result`
    /// has it, 1 for a failure, with why on standard error (the test's output).
    fn judge(ran: Ran, panics: &Panics) -> Int {
        let payload = match ran {
            Ran::Returned => match *panics {
                Panics::No => return 0,
                _ => {
                    eprintln!("note: test did not panic as expected");
                    return 1;
                }
            },
            Ran::Errored(message) => {
                if let Panics::No = *panics {
                    eprintln!("{}", message);
                    return 1;
                }
                Some(message)
            }
            Ran::Panicked(payload) => payload,
        };
        match *panics {
            Panics::No => 1,
            Panics::Yes => 0,
            Panics::WithMessage(expected) => match payload {
                Some(message) if message.contains(expected) => 0,
                Some(message) => {
                    eprintln!(
                        "note: panic did not contain expected string\n      panic message: {:?}\n expected substring: {:?}",
                        message, expected
                    );
                    1
                }
                None => {
                    eprintln!(
                        "note: expected panic with string value,\n found non-string value\n     expected substring: {:?}",
                        expected
                    );
                    1
                }
            },
        }
    }

    /// Runs one test in this child as libtest runs it: on a thread named after it, judged by
    /// its `should_panic`. The exit code [`judge`] gives.
    fn run(test: &'static test::TestDescAndFn) -> Int {
        let name = test.desc.name.as_slice();
        mark(name);
        let panics = match test.desc.should_panic {
            test::ShouldPanic::No => Panics::No,
            test::ShouldPanic::Yes => Panics::Yes,
            test::ShouldPanic::YesWithMessage(message) => Panics::WithMessage(message),
        };
        let body = match test.testfn {
            test::StaticTestFn(f) => Body::Test(f),
            test::StaticBenchFn(f) => Body::Bench(f),
            _ => {
                eprintln!("blaze's fork server runs static tests only, and `{}` is not one", name);
                return 1;
            }
        };
        if cfg!(panic = "abort") {
            // No unwinding to catch: the first panic is judged where it happens.
            let judged = panics.clone();
            let default = ::std::panic::take_hook();
            ::std::panic::set_hook(Box::new(move |info| {
                default(info);
                let code = judge(Ran::Panicked(payload_str(info.payload())), &judged);
                let _ = ::std::io::stderr().flush();
                exit_now(code);
            }));
        }
        let thread = ::std::thread::Builder::new().name(name.to_owned()).spawn(move || match body {
            Body::Test(f) => f(),
            Body::Bench(f) => test::bench::run_once(f),
        });
        let handle = match thread {
            Ok(handle) => handle,
            Err(error) => {
                eprintln!("blaze's fork server: the test's thread could not start: {}", error);
                return 1;
            }
        };
        let code = match handle.join() {
            Ok(Ok(())) => judge(Ran::Returned, &panics),
            Ok(Err(message)) => judge(Ran::Errored(&message), &panics),
            Err(payload) => judge(Ran::Panicked(payload_str(&*payload)), &panics),
        };
        mark(name);
        code
    }
}
