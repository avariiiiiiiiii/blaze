//! Build scripts compiled for wasm32-wasip1 and run under Wasmtime, an experiment beside the
//! native sandbox (docs/cargo.md, "Build scripts under WASI").
//!
//! - [`STD_PROCESS`] is blaze's `std::process` for such a script: the standard library blaze
//!   compiles for it (`blaze --run-tool`) has it in place of
//!   `library/std/src/sys/process/unsupported.rs`, and its commands go to the host through the
//!   import `blaze.run_tool`.
//! - [`Request`] and [`Response`] are what that import is given and answers, as
//!   `std/process.rs` writes and reads them.
//! - With the feature `run`, [`run::run`] runs a script's module under Wasmtime with WASI
//!   preview 1, its preopens and environment, and answers `blaze.run_tool` with what the
//!   caller says.

// The one unsafe call is `run`'s load of machine code the store kept (`run::kept`).
#![deny(unsafe_code)]

#[cfg(feature = "run")]
pub mod run;

/// blaze's `sys/process` module for wasm32-wasip1 build scripts.
pub const STD_PROCESS: &str = include_str!("../std/process.rs");

/// Where in `library/std/src` [`STD_PROCESS`] stands.
pub const STD_PROCESS_AT: &str = "sys/process/unsupported.rs";

/// The SHA-256 of the toolchain's file at [`STD_PROCESS_AT`] that [`STD_PROCESS`] was written
/// against (nightly-2026-10-25): it uses the module's private interface to `std::process`, so
/// a toolchain whose file differs needs it written again, and a build with it is refused.
pub const STD_PROCESS_FOR: &str =
    "26313906a12d1cebdd61925039ba7e81e2d4e261ab572cee0ef682c3e000a184";

/// The wasm import module of the host's functions.
pub const IMPORT_MODULE: &str = "blaze";

/// One command a script asked the host to run, owned by the host that runs it.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Request {
    pub program: Vec<u8>,
    pub cwd: Option<Vec<u8>>,
    pub args: Vec<Vec<u8>>,
    /// The whole environment the command runs with, as std worked it out.
    pub env: Vec<(Vec<u8>, Vec<u8>)>,
    pub stdin: Vec<u8>,
}

/// What the host answers a [`Request`].
#[derive(Debug, PartialEq, Eq)]
pub enum Response {
    Ran {
        status: i32,
        stdout: Vec<u8>,
        stderr: Vec<u8>,
    },
    /// The program is not one a script may run; the message names it.
    Refused(String),
    /// It could not be started.
    NotStarted(String),
}

impl Request {
    /// The request `std/process.rs` wrote, `None` when it is not one.
    pub fn read(bytes: &[u8]) -> Option<Request> {
        let rest = &mut &*bytes;
        let program = owned(field(rest)?);
        let cwd = field(rest)?;
        let cwd = (!cwd.is_empty()).then(|| owned(cwd));
        // Each argument is at least its length, each variable two: a count past what the
        // bytes can hold reserves no more than they can.
        let n = count(rest)?;
        let mut args = Vec::with_capacity(n.min(rest.len() / 4));
        for _ in 0..n {
            args.push(owned(field(rest)?));
        }
        let n = count(rest)?;
        let mut env = Vec::with_capacity(n.min(rest.len() / 8));
        for _ in 0..n {
            env.push((owned(field(rest)?), owned(field(rest)?)));
        }
        let stdin = owned(field(rest)?);
        rest.is_empty().then_some(Request { program, cwd, args, env, stdin })
    }

    /// Appends to `out` the bytes `std/process.rs` writes of it.
    pub fn write(&self, out: &mut Vec<u8>) {
        put(out, &self.program);
        put(out, self.cwd.as_deref().unwrap_or_default());
        out.extend((self.args.len() as u32).to_le_bytes());
        for arg in &self.args {
            put(out, arg);
        }
        out.extend((self.env.len() as u32).to_le_bytes());
        for (name, value) in &self.env {
            put(out, name);
            put(out, value);
        }
        put(out, &self.stdin);
    }
}

impl Response {
    /// Appends to `out`, a buffer the host keeps for the run, the bytes `std/process.rs`
    /// reads.
    pub fn write(&self, out: &mut Vec<u8>) {
        match self {
            Response::Ran { status, stdout, stderr } => {
                out.reserve(1 + 4 + 4 + stdout.len() + 4 + stderr.len());
                out.push(0);
                out.extend(status.to_le_bytes());
                put(out, stdout);
                put(out, stderr);
            }
            Response::Refused(message) => {
                out.push(1);
                put(out, message.as_bytes());
            }
            Response::NotStarted(message) => {
                out.push(2);
                put(out, message.as_bytes());
            }
        }
    }
}

/// A field of a request, owned: the host is given the command it runs as its own (the edge:
/// a process it starts).
fn owned(bytes: &[u8]) -> Vec<u8> {
    bytes.to_vec() // alloc: the edge: the command the host runs is its own
}

fn put(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend((bytes.len() as u32).to_le_bytes());
    out.extend_from_slice(bytes);
}

fn count(rest: &mut &[u8]) -> Option<usize> {
    let (head, tail) = rest.split_at_checked(4)?;
    *rest = tail;
    Some(u32::from_le_bytes(head.try_into().ok()?) as usize)
}

fn field<'a>(rest: &mut &'a [u8]) -> Option<&'a [u8]> {
    let len = count(rest)?;
    let (head, tail) = rest.split_at_checked(len)?;
    *rest = tail;
    Some(head)
}

/// Counts each test thread's allocations, for the allocation-budget tests of the host's calls.
#[cfg(test)]
#[global_allocator]
static ALLOC: blaze_alloc_count::Counting = blaze_alloc_count::Counting;

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> Request {
        Request {
            program: b"cc".to_vec(),
            cwd: Some(b"/pkg".to_vec()),
            args: vec![b"-c".to_vec(), b"a.c".to_vec()],
            env: vec![(b"OUT_DIR".to_vec(), b"/out".to_vec())],
            stdin: b"int a;".to_vec(),
        }
    }

    fn written(request: &Request) -> Vec<u8> {
        let mut out = Vec::new();
        request.write(&mut out);
        out
    }

    #[test]
    fn a_request_reads_back_as_it_was_written() {
        let request = request();
        assert_eq!(Request::read(&written(&request)).as_ref(), Some(&request));
        let no_cwd = Request { cwd: None, ..request };
        let bytes = written(&no_cwd);
        assert_eq!(Request::read(&bytes).as_ref(), Some(&no_cwd));
        assert_eq!(Request::read(&bytes[1..]), None);
        // A count of arguments past what the bytes hold is no request, and reserves no more
        // than one argument for each 4 bytes there are.
        let mut lying = written(&Request::default());
        lying[8..12].copy_from_slice(&u32::MAX.to_le_bytes());
        let (read, used) = blaze_alloc_count::measure(|| Request::read(&lying));
        assert_eq!(read, None);
        assert!(used.bytes <= (lying.len() / 4 * std::mem::size_of::<Vec<u8>>()) as u64, "{used:?}");
    }

    /// Reading a request in place allocates only what the host is given to own: its
    /// program, directory, stdin, each argument and each variable's name and value, and the
    /// two lists, each of the size it is.
    #[test]
    fn a_request_read_allocates_what_it_owns() {
        let bytes = written(&request());
        let (read, used) = blaze_alloc_count::measure(|| Request::read(&bytes));
        assert_eq!(read, Some(request()));
        assert_eq!(used.allocations, 3 + (1 + 2) + (1 + 2));
    }

    /// An answer is written into the buffer the host keeps for the run: warm, nothing is
    /// allocated.
    #[test]
    fn an_answer_written_warm_allocates_nothing() {
        let answers = [
            Response::Ran { status: 0, stdout: b"ok\n".to_vec(), stderr: b"warning: x\n".to_vec() },
            Response::Refused("`curl` is not a tool a script may run".into()),
            Response::NotStarted("cc: no such file".into()),
        ];
        let mut out = Vec::with_capacity(64);
        blaze_alloc_count::assert_at_most(0, "the host's answers written, warm", || {
            for answer in &answers {
                out.clear();
                answer.write(&mut out);
            }
        });
        assert_eq!(out[0], 2);
        assert_eq!(&out[5..], b"cc: no such file");
    }

    #[test]
    fn the_std_module_names_the_import_module() {
        assert!(STD_PROCESS.contains(&format!("wasm_import_module = \"{IMPORT_MODULE}\"")));
        assert!(STD_PROCESS.contains("link_name = \"run_tool\""));
        assert!(STD_PROCESS.contains("link_name = \"run_tool_response\""));
    }
}
