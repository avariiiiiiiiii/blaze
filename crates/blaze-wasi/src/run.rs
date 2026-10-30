//! A build script's module under Wasmtime: WASI preview 1 with the directories and environment
//! it is given and nothing else (no network, no other files), and `blaze.run_tool` answered by
//! the caller.

use std::{
    path::{Path, PathBuf},
    sync::OnceLock,
};

use anyhow::Context as _;
use wasmtime::{Caller, Engine, Linker, Module, Store};
use wasmtime_wasi::p1::WasiP1Ctx;
use wasmtime_wasi::p2::pipe::MemoryOutputPipe;
use wasmtime_wasi::{FsPerms, I32Exit, WasiCtxBuilder};

use crate::{IMPORT_MODULE, Request, Response};

/// A directory of the host the script sees, at `guest`.
pub struct Preopen {
    pub host: PathBuf,
    pub guest: String,
    pub writable: bool,
}

/// One run of a script's module.
pub struct Guest<'a> {
    pub module: &'a Path,
    /// Its arguments, the first its name.
    pub args: &'a [String],
    pub env: &'a [(String, String)],
    pub preopens: &'a [Preopen],
}

/// How a run ended.
pub struct Exited {
    /// What it passed to `proc_exit`, 0 when `main` returned.
    pub status: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    /// The trap that ended it, a panic's `unreachable` among them, with Wasmtime's backtrace.
    pub trap: Option<String>,
}

/// The most a script may print to each of stdout and stderr.
const MOST_PRINTED: usize = 256 << 20;

/// What answers a run's `blaze.run_tool`: called once per command the script runs, each a
/// process the caller starts, and of one type for every caller, so every run shares one linker.
type Tool = Box<dyn FnMut(Request) -> Response + Send>; // dyn: once a command run, the edge

struct State {
    wasi: WasiP1Ctx,
    tool: Tool,
    /// The last answer, encoded, until the script reads it: one buffer the whole run reuses.
    response: Vec<u8>,
}

/// The engine every run shares: compiling a module is most of a short run's time.
fn engine() -> &'static Engine {
    static ENGINE: OnceLock<Engine> = OnceLock::new();
    ENGINE.get_or_init(Engine::default)
}

/// Compiles the module at `path` for this host, or loads the machine code the store keeps of
/// the same bytes for this engine ([`code_key`]): Cranelift runs once per module and machine,
/// not once per run.
pub fn compile(path: &Path) -> anyhow::Result<Module> {
    // alloc: setup, once a run: the module's bytes, which its key is the hash of.
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?; // alloc: error path
    let store = blaze_store::toolchain().and_then(|_| blaze_store::Store::global());
    compile_in(&bytes, store).with_context(|| format!("compiling {}", path.display())) // alloc: error path
}

/// As [`compile`], of a module's `bytes`, with what `store` keeps.
fn compile_in(bytes: &[u8], store: Option<&blaze_store::Store>) -> anyhow::Result<Module> {
    let engine = engine();
    let key = code_key(bytes, engine);
    if let Some(code) = store.and_then(|it| it.get_lasting(&key)) {
        return kept::load(engine, &code).context(
            "the store's machine code of it, under a key of this engine's configuration, is refused",
        );
    }
    let module = Module::new(engine, bytes)?;
    if let Some(store) = store {
        store.insert_lasting(&key, &module.serialize()?);
        store.flush();
    }
    Ok(module)
}

/// What the store keeps a script module's machine code under: the hash of its bytes and what
/// Wasmtime says code must match to load (its version, its configuration, the target and the
/// CPU features `engine` compiles for), and nothing of where the module is.
fn code_key(bytes: &[u8], engine: &Engine) -> [u8; 32] {
    use std::hash::Hash as _;
    struct Parts<'a>(&'a mut blaze_ids::Hasher);
    impl std::hash::Hasher for Parts<'_> {
        fn finish(&self) -> u64 {
            0
        }
        fn write(&mut self, bytes: &[u8]) {
            self.0.part(bytes);
        }
    }
    let mut key = blaze_ids::Hasher::new("blaze v2 wasmtime machine code of a build script 1");
    key.part(bytes).part(std::env::consts::ARCH.as_bytes());
    engine.precompile_compatibility_hash().hash(&mut Parts(&mut key));
    key.finish().0
}

/// Machine code from the store, made a module.
#[allow(unsafe_code)]
mod kept {
    use wasmtime::{Engine, Module};

    pub(super) fn load(engine: &Engine, code: &[u8]) -> wasmtime::Result<Module> {
        // SAFETY: `code` is what the store keeps under a `code_key` of the module's bytes and
        // of `engine`'s compatibility hash, checked against its checksum as it was read
        // (`Store::get_lasting`), and only `compile_in` puts under such a key, the `serialize`
        // of a module an engine of that hash compiled; Wasmtime refuses code of another
        // version, configuration or CPU. Other bytes would run as machine code.
        unsafe { Module::deserialize(engine, code) }
    }
}

/// WASI preview 1 and `blaze.run_tool`, linked once a process: a run only instantiates.
fn linker() -> wasmtime::Result<&'static Linker<State>> {
    static LINKER: OnceLock<Linker<State>> = OnceLock::new();
    if let Some(linker) = LINKER.get() {
        return Ok(linker);
    }
    let linked = linked()?;
    Ok(LINKER.get_or_init(|| linked))
}

fn linked() -> wasmtime::Result<Linker<State>> {
    let mut linker: Linker<State> = Linker::new(engine());
    wasmtime_wasi::p1::add_to_linker_sync(&mut linker, |it: &mut State| &mut it.wasi)?;
    linker.func_wrap(
        IMPORT_MODULE,
        "run_tool",
        |mut caller: Caller<'_, State>, at: u32, len: u32| -> wasmtime::Result<i64> {
            let memory = memory(&mut caller)?;
            // Read in place in the script's memory: a request past its end is none.
            let bytes = memory.data(&caller).get(at as usize..).and_then(|it| it.get(..len as usize));
            let Some(request) = bytes.and_then(Request::read) else { return Ok(-1) };
            let State { tool, response, .. } = caller.data_mut();
            let answer = tool(request);
            response.clear();
            answer.write(response);
            Ok(response.len() as i64)
        },
    )?;
    linker.func_wrap(
        IMPORT_MODULE,
        "run_tool_response",
        |mut caller: Caller<'_, State>, at: u32, len: u32| -> wasmtime::Result<()> {
            let memory = memory(&mut caller)?;
            let mut response = std::mem::take(&mut caller.data_mut().response);
            if response.len() != len as usize {
                return Err(wasmtime::Error::msg("run_tool_response: a wrong length"));
            }
            memory.write(&mut caller, at as usize, &response)?;
            // Given back empty, its capacity kept for the next answer.
            response.clear();
            caller.data_mut().response = response;
            Ok(())
        },
    )?;
    Ok(linker)
}

/// Runs `guest`, `module` being its module compiled ([`compile`]), with each `blaze.run_tool`
/// answered by `tool`.
pub fn run<T>(guest: &Guest<'_>, module: &Module, tool: T) -> anyhow::Result<Exited>
where
    T: FnMut(Request) -> Response + Send + 'static,
{
    let stdout = MemoryOutputPipe::new(MOST_PRINTED);
    let stderr = MemoryOutputPipe::new(MOST_PRINTED);
    let mut wasi = WasiCtxBuilder::new();
    // clone: setup: a handle to each pipe for the script, the other taken back as it ends
    wasi.args(guest.args).envs(guest.env).stdout(stdout.clone()).stderr(stderr.clone());
    wasi.allow_blocking_current_thread(true);
    wasi.allow_tcp(false).allow_udp(false).allow_ip_name_lookup(false);
    for preopen in guest.preopens {
        let perms = if preopen.writable { FsPerms::ReadWrite } else { FsPerms::ReadOnly };
        wasi.preopened_dir(&preopen.host, &preopen.guest, perms)
            .map_err(anyhow::Error::from)
            .with_context(|| format!("opening {} for the script", preopen.host.display()))?; // alloc: error path
    }
    // alloc: setup, once a run: the caller's answerer, boxed.
    let tool: Tool = Box::new(tool);
    let state = State { wasi: wasi.build_p1(), tool, response: Vec::new() }; // alloc: empty, allocates nothing
    let mut store = Store::new(engine(), state);
    let instance = linker()?.instantiate(&mut store, module)?;
    let start = instance.get_typed_func::<(), ()>(&mut store, "_start")?;
    let (status, trap) = match start.call(&mut store, ()) {
        Ok(()) => (0, None),
        Err(error) => match error.downcast_ref::<I32Exit>() {
            Some(exit) => (exit.0, None),
            None => (1, Some(format!("{error:?}"))), // alloc: error path: the trap that ended the run
        },
    };
    drop(store);
    Ok(Exited { status, stdout: printed(stdout)?, stderr: printed(stderr)?, trap })
}

/// What the script printed to `pipe`, moved out of it: the store, which held its other
/// handle, is dropped.
fn printed(pipe: MemoryOutputPipe) -> anyhow::Result<Vec<u8>> {
    let printed = pipe.try_into_inner().context("a script's output is still held after its store is dropped")?;
    Ok(Vec::from(printed))
}

fn memory<D: 'static>(caller: &mut Caller<'_, D>) -> wasmtime::Result<wasmtime::Memory> {
    caller
        .get_export("memory")
        .and_then(|it| it.into_memory())
        .ok_or_else(|| wasmtime::Error::msg("the script exports no memory"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A script whose `_start` ends the run with status 7 (`proc_exit(7)`).
    fn exits_7() -> Vec<u8> {
        fn section(id: u8, body: &[u8]) -> Vec<u8> {
            let mut out = vec![id, body.len() as u8];
            out.extend_from_slice(body);
            out
        }
        let mut module = b"\0asm\x01\0\0\0".to_vec();
        // 0: `(i32) -> ()`, 1: `() -> ()`.
        module.extend(section(1, &[2, 0x60, 1, 0x7f, 0, 0x60, 0, 0]));
        let mut import = vec![1, 22];
        import.extend_from_slice(b"wasi_snapshot_preview1");
        import.push(9);
        import.extend_from_slice(b"proc_exit");
        import.extend([0, 0]);
        module.extend(section(2, &import));
        module.extend(section(3, &[1, 1]));
        module.extend(section(5, &[1, 0, 1]));
        let mut exports = vec![2, 6];
        exports.extend_from_slice(b"_start");
        exports.extend([0, 1, 6]);
        exports.extend_from_slice(b"memory");
        exports.extend([2, 0]);
        module.extend(section(7, &exports));
        // `proc_exit(7)`.
        module.extend(section(10, &[1, 6, 0, 0x41, 7, 0x10, 0, 0x0b]));
        module
    }

    fn ran(module: &Module) -> Exited {
        let guest = Guest { module: Path::new("exits.wasm"), args: &[], env: &[], preopens: &[] };
        run(&guest, module, |_| Response::Refused("no tools".into())).unwrap()
    }

    /// The first compile keeps the machine code; the next load is of that code, not another
    /// compile, and runs as the compiled module does. Every run shares one linker.
    #[test]
    fn a_scripts_machine_code_is_compiled_once_and_loaded_from_the_store_after() {
        let dir = std::env::temp_dir().join(format!("blaze-wasi-kept-{}", std::process::id()));
        _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        blaze_store::compiles_against("blaze-wasi tests");
        let store = blaze_store::Store::open_at(&dir.join("store")).unwrap();
        let bytes = exits_7();
        let key = code_key(&bytes, engine());
        assert!(!store.has_lasting(&key));
        let compiled = compile_in(&bytes, Some(&store)).unwrap();
        store.seal();
        let code = store.get_lasting(&key).expect("the compile kept its machine code");
        assert_eq!(code, compiled.serialize().unwrap());
        let loaded = compile_in(&bytes, Some(&store)).unwrap();
        for module in [&compiled, &loaded, &compile_in(&bytes, None).unwrap()] {
            let exited = ran(module);
            assert_eq!((exited.status, exited.trap), (7, None));
        }
        assert!(std::ptr::eq(linker().unwrap(), linker().unwrap()), "one linker for every run");
        let longer: [&[u8]; 2] = [&bytes, b"\0\0"];
        let other = code_key(&longer.concat(), engine());
        assert_ne!(other, key, "a module's key is of its bytes");
        _ = std::fs::remove_dir_all(&dir);
    }
}
