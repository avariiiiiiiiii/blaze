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
    use super::{End, Frame, Frames, LONGEST_OUTPUT, MARK, SOCKET, VERSION};
    use ::std::io::{Read, Write};
    use ::std::os::unix::io::AsRawFd;
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
    }

    pub fn serve(tests: &'static [&'static test::TestDescAndFn], socket: &::std::ffi::OsStr) -> Result<(), String> {
        // Taken out before any test runs, so a test that runs its own binary gets libtest.
        // SAFETY: the server has one thread, and nothing reads the environment as it changes.
        unsafe {
            ::std::env::remove_var(SOCKET);
        }
        let mut stream = connect(socket)?;
        let names: Vec<String> = tests.iter().map(|it| it.desc.name.as_slice().to_owned()).collect();
        send(&mut stream, &Frame::Hello { version: VERSION, pid: ::std::process::id(), tests: names })?;
        let mut frames = Frames::new();
        let mut children: Vec<Child> = Vec::new();
        let mut quitting = false;
        let mut buffer = vec![0u8; 1 << 16];
        loop {
            if quitting && children.is_empty() {
                return Ok(());
            }
            let watched: Vec<usize> = (0..children.len()).filter(|&at| children[at].open).collect();
            let mut fds = Vec::with_capacity(watched.len() + 1);
            fds.push(PollFd { fd: stream.as_raw_fd(), events: POLLIN, revents: 0 });
            for &at in &watched {
                fds.push(PollFd { fd: children[at].output.as_raw_fd(), events: POLLIN, revents: 0 });
            }
            // A child's end is seen by `waitpid`, so the wait is short while any runs.
            let timeout = if children.is_empty() { -1 } else { 5 };
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
                        Frame::Run { id, index } => match start(tests, id, index, &stream, &children) {
                            Ok(child) => children.push(child),
                            Err(why) => {
                                let ended = Frame::Ended { id: id, end: End::NotStarted(why), output: Vec::new() };
                                if let Err(why) = send(&mut stream, &ended) {
                                    kill_all(&mut children);
                                    return Err(why);
                                }
                            }
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
                let ended = Frame::Ended { id: child.id, end: end_of(status), output: child.bytes };
                if let Err(why) = send(&mut stream, &ended) {
                    kill_all(&mut children);
                    return Err(why);
                }
            }
        }
    }

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
            // SAFETY: plain calls on descriptors this process holds; none is used after.
            unsafe {
                setpgid(0, 0);
                close(stream.as_raw_fd());
                close(ours.as_raw_fd());
                for child in children {
                    close(child.output.as_raw_fd());
                }
                dup2(theirs.as_raw_fd(), 1);
                dup2(theirs.as_raw_fd(), 2);
                if theirs.as_raw_fd() > 2 {
                    close(theirs.as_raw_fd());
                }
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
        Ok(Child { id: id, pid: pid, output: ours, bytes: Vec::new(), cut: false, open: true, status: None })
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
        // SAFETY: plain calls; the group is the child's own.
        unsafe {
            kill(-pid, SIGKILL);
            kill(pid, SIGKILL);
        }
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
