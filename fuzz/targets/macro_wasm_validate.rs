//! Feeds `blaze_macro_wasm::Wasm::at` the input as the bytes of a proc-macro module: Wasmtime
//! validates it, Cranelift compiles it, its machine code is written beside it, and it is linked
//! against the host's functions and WASI (`add_host`, `wasi::add`, `wasi::rest`) into an
//! `InstancePre`. Nothing of the module runs: a module may loop forever, as a native proc
//! macro may, so its list and expansions are not called here (`macro_wasm_host` runs a module
//! that ends).
//!
//! Invariants, each checked below (a violation panics, which the engine reports as a crash):
//! - loading never panics, whatever the bytes: a module that is not one, or that imports what
//!   the host has not, is an `Err`;
//! - it asks for at most 4 MiB plus 256 bytes for every byte of the module at once: Wasmtime
//!   keeps tables of under a few hundred bytes per function, type or import, each declared by at
//!   least a byte, and the machine code it writes is a few times the module's code in sections
//!   aligned to pages.
//!
//! The module is a file in this process's temporary directory, cleared at each run; a module
//! of bytes loaded once is not compiled again in the process (`Wasm::at`'s cache, by digest).

#![forbid(unsafe_code)]

#[allow(dead_code)]
#[path = "macro_wasm_guest.rs"]
mod guest;

use std::{
    path::PathBuf,
    sync::{Mutex, OnceLock},
    time::Duration,
};

use blaze_fuzzer::Target;
use blaze_macro_wasm::Wasm;

const TARGET: Target = Target {
    name: "macro_wasm_validate",
    check,
    seeds,
    dictionary: &[
        b"\0asm\x01\0\0\0",
        b"blaze_proc_macro",
        b"wasi_snapshot_preview1",
        b"blaze_proc_macros",
        b"blaze_proc_macro_alloc",
        b"blaze_proc_macro_list",
        b"blaze_proc_macro_expand",
        b"memory",
        b"_initialize",
        b"\x60\x00\x00",
        b"\x60\x01\x7f\x01\x7f",
        b"\x7e",
        b"\xff\xff\xff\xff\x0f",
    ],
    max_len: 1 << 16,
    budget: Duration::from_secs(900),
};

/// The temporary directory, and one load at a time, as they share it.
fn dir() -> &'static Mutex<PathBuf> {
    static DIR: OnceLock<Mutex<PathBuf>> = OnceLock::new();
    DIR.get_or_init(|| {
        let dir = std::env::temp_dir()
            .join(format!("blaze-fuzz-{}-{}", TARGET.name, std::process::id()));
        // Wasmtime's engine is made by the first load: made here, it is not what a check sees.
        clear(&dir);
        let warm = dir.join("warm.wasm");
        std::fs::write(&warm, b"\0asm\x01\0\0\0").unwrap_or_else(|e| panic!("{}: {e}", warm.display()));
        Wasm::at(&warm, None).expect("the empty module loads");
        Mutex::new(dir)
    })
}

fn clear(dir: &std::path::Path) {
    _ = std::fs::remove_dir_all(dir);
    std::fs::create_dir_all(dir).unwrap_or_else(|e| panic!("{}: {e}", dir.display()));
}

fn check(data: &[u8]) {
    let dir = dir().lock().unwrap_or_else(|e| e.into_inner());
    clear(&dir);
    let path = dir.join("module.wasm");
    std::fs::write(&path, data).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let (largest, _) = blaze_fuzz::largest_allocation(|| Wasm::at(&path, None));
    let bound = (4 << 20) + 256 * data.len();
    assert!(
        largest <= bound,
        "loading a module of {} bytes asked for {largest} bytes at once, past {bound}",
        data.len()
    );
}

/// The modules the other targets run, which link, and a module of each section a proc-macro
/// module has.
fn seeds() -> Vec<Vec<u8>> {
    let header = b"\0asm\x01\0\0\0".to_vec();
    let with = |sections: &[u8]| [header.as_slice(), sections].concat();
    let list = "0\tSerialize\tserde\n1\ttwice\t\n2\tsql\t\n";
    let mut custom = vec![0, (1 + 17 + list.len()) as u8, 17];
    custom.extend_from_slice(b"blaze_proc_macros");
    custom.extend_from_slice(list.as_bytes());
    vec![
        guest::module(guest::Kind::Interpreter),
        guest::module(guest::Kind::Echo),
        header.clone(),
        with(&custom),
        // `(func (export "f"))` of type `() -> ()`.
        with(&[1, 4, 1, 0x60, 0, 0, 3, 2, 1, 0, 7, 5, 1, 1, b'f', 0, 0, 10, 4, 1, 2, 0, 0x0b]),
        // An import of `env.f`, which the host has not.
        with(&[1, 4, 1, 0x60, 0, 0, 2, 9, 1, 3, b'e', b'n', b'v', 1, b'f', 0, 0]),
        // An import of `blaze_proc_macro.output` of the wrong type.
        with(&[
            1, 4, 1, 0x60, 0, 0, 2, 27, 1, 16, b'b', b'l', b'a', b'z', b'e', b'_', b'p', b'r',
            b'o', b'c', b'_', b'm', b'a', b'c', b'r', b'o', 6, b'o', b'u', b't', b'p', b'u', b't',
            0, 0,
        ]),
        // A memory of 65536 pages, the most wasm32 has.
        with(&[5, 5, 1, 0, 0x80, 0x80, 0x04]),
    ]
}

#[cfg_attr(test, allow(dead_code))]
fn main() -> std::process::ExitCode {
    blaze_fuzzer::main(&TARGET)
}

#[cfg(test)]
mod tests {
    #[test]
    fn seeds() {
        blaze_fuzzer::seeds(&super::TARGET);
    }

    #[test]
    fn the_modules_the_other_targets_run_load() {
        for kind in [super::guest::Kind::Interpreter, super::guest::Kind::Echo] {
            let dir = super::dir().lock().unwrap_or_else(|e| e.into_inner());
            let path = dir.join(format!("{kind:?}.wasm"));
            std::fs::write(&path, super::guest::module(kind)).unwrap();
            let wasm = blaze_macro_wasm::Wasm::at(&path, None).unwrap();
            let macros = wasm.macros().unwrap();
            assert_eq!(macros.len(), 1);
            assert_eq!(macros[0].name, "fuzz");
        }
    }
}
