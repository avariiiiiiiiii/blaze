//! Feeds a proc-macro module's calls of the host: a fixed module (`macro_wasm_guest.rs`,
//! [`guest::Kind::Interpreter`]), compiled and linked once through `Wasm::at`, reads the input
//! from its package through the host's WASI and runs it as a program of host calls, each with
//! the fuzzer's pointers, lengths and numbers, in `_initialize` or in the expansion, in a
//! `Session` or a fresh instance (`Wasm::expand`).
//!
//! Invariants, each checked below (a violation panics, which the engine reports as a crash):
//! - no host function panics, whatever the module passes it: one given a pointer and a length
//!   outside the module's memory returns an error or an errno, and one inside reads and writes
//!   only there (an out-of-bounds slice in the host is a panic, which this sees);
//! - no expansion asks for more memory at once than `blaze_fuzz::allowed(len)` plus 256 bytes
//!   for every byte of the module's memory: whatever the host reads of the module is in its
//!   memory, and the host makes trees of a few hundred bytes a byte at most, so a larger
//!   allocation is a length the host believed before checking it.

#![forbid(unsafe_code)]

#[allow(dead_code)]
#[path = "macro_wasm_guest.rs"]
mod guest;

use std::{sync::OnceLock, time::Duration};

use blaze_fuzzer::Target;
use guest::{ARGS, ATTR, FRESH, Host, IMPORTS, IN_INITIALIZE, IOV_AT, Kind, MEMORY, PATH, PATH_AT};

const TARGET: Target = Target {
    name: "macro_wasm_host",
    check,
    seeds,
    dictionary: &[
        // `BUF`, where the input is in the module's memory, and the end of the memory.
        b"\x00\x80\x00\x00",
        b"\x00\x00\x04\x00",
        b"\xfc\xff\x03\x00",
        b"\xff\xff\xff\xff",
        b"\x00\x00\x00\x80",
        b"/blaze/package",
        b"input",
        b"../",
        b"r#\"",
    ],
    max_len: 1 << 16,
    budget: Duration::from_secs(600),
};

fn host() -> &'static Host {
    static HOST: OnceLock<Host> = OnceLock::new();
    HOST.get_or_init(|| Host::new(TARGET.name, Kind::Interpreter))
}

fn check(data: &[u8]) {
    let phase = data.first().copied().unwrap_or(0);
    let (largest, _) = host().run(data, phase);
    let bound = blaze_fuzz::allowed(data.len()) + 256 * MEMORY;
    assert!(
        largest <= bound,
        "an expansion asked for {largest} bytes at once, past {bound}: a length the module gave \
         was believed"
    );
}

/// The args of a record: `args` then zeros.
fn args(args: &[i32]) -> [i32; ARGS] {
    let mut all = [0; ARGS];
    all[..args.len()].copy_from_slice(args);
    all
}

/// The bytes of a stream as the module writes it: one ident, `foo`, of the call site.
fn foo() -> Vec<u8> {
    let mut out = 1u32.to_le_bytes().to_vec();
    out.push(1);
    out.extend_from_slice(&2u32.to_le_bytes());
    out.push(0);
    out.extend_from_slice(&3u32.to_le_bytes());
    out.extend_from_slice(b"foo");
    out
}

fn iov(ptr: i32, len: i32) -> Vec<u8> {
    let mut out = ptr.to_le_bytes().to_vec();
    out.extend_from_slice(&len.to_le_bytes());
    out
}

/// Programs that call every function of the host's as a module of syn and quote would, and
/// each with pointers at the edges of the module's memory.
fn seeds() -> Vec<Vec<u8>> {
    let mut seeds = Vec::new();
    let one = |phase: u8, data: &[u8]| {
        guest::program(phase, &[("output", args(&[guest::data_at(1), data.len() as i32]))], data)
    };
    for phase in [0, FRESH, ATTR, FRESH | ATTR, IN_INITIALIZE, IN_INITIALIZE | FRESH] {
        seeds.push(one(phase, &foo()));
    }

    let spans: &[(&str, [i32; ARGS])] = &[
        ("def_site", args(&[])),
        ("call_site", args(&[])),
        ("mixed_site", args(&[])),
        ("span_join", args(&[1, 2])),
        ("span_join", args(&[2, 0])),
        ("span_resolved_at", args(&[1, 3])),
        ("span_located_at", args(&[2, 1])),
        ("span_parent", args(&[1])),
        ("span_source", args(&[2])),
        ("span_start", args(&[1])),
        ("span_end", args(&[2])),
        ("span_subspan", args(&[2, 0, 0])),
        ("span_subspan", args(&[2, 1, 7])),
        ("span_subspan", args(&[2, -1, -1])),
        ("span_byte_range", args(&[2, 0x300])),
        ("span_line", args(&[1])),
        ("span_column", args(&[3])),
        ("span_debug", args(&[3])),
        ("take", args(&[0x400])),
        ("span_file", args(&[1])),
        ("take", args(&[0x400])),
        ("span_local_file", args(&[1])),
        ("span_source_text", args(&[2])),
        ("track_path", args(&[PATH_AT, PATH.len() as i32])),
        ("track_env_var", args(&[PATH_AT, PATH.len() as i32])),
        ("diagnostic", args(&[0, PATH_AT, PATH.len() as i32, 0])),
        ("retained", args(&[16])),
    ];
    for phase in [0, FRESH, IN_INITIALIZE] {
        seeds.push(guest::program(phase, spans, &[]));
    }

    let text = b"fn f<'a>(x: &'a u8) -> r#u8 { x.0 + -1i32 } /// doc";
    let literal = b"-1.5f32";
    let at = guest::data_at(6);
    let lexing: &[(&str, [i32; ARGS])] = &[
        ("ts_from_str", args(&[at, text.len() as i32])),
        ("take", args(&[0x400])),
        ("literal_from_str", args(&[at + text.len() as i32, literal.len() as i32])),
        ("take", args(&[0x400])),
        ("ts_from_str", args(&[at, 1])),
        ("panicked", args(&[at, 2])),
    ];
    let mut data = text.to_vec();
    data.extend_from_slice(literal);
    seeds.push(guest::program(0, lexing, &data));

    // fd 4 and 5 are `input`, opened by `_initialize` and the expansion; 6 is the next.
    let at = guest::data_at(24);
    let mut data = iov(0x2000, 16);
    data.extend_from_slice(b"../input\0src");
    let files: &[(&str, [i32; ARGS])] = &[
        ("path_open", args(&[3, 0, PATH_AT, PATH.len() as i32, 0, 0, 0, 0, 0x300])),
        ("fd_read", args(&[6, at, 1, 0x304])),
        ("fd_seek", args(&[6, 2, 1, 0x308])),
        ("fd_tell", args(&[6, 0x310])),
        ("fd_pread", args(&[6, at, 1, 3, 0x318])),
        ("fd_read", args(&[4, IOV_AT, 1, 0x31c])),
        ("fd_filestat_get", args(&[6, 0x320])),
        ("fd_fdstat_get", args(&[6, 0x360])),
        ("fd_fdstat_get", args(&[3, 0x360])),
        ("fd_close", args(&[6])),
        ("fd_close", args(&[3])),
        ("fd_close", args(&[6])),
        ("fd_prestat_get", args(&[3, 0x300])),
        ("fd_prestat_dir_name", args(&[3, 0x380, 14])),
        ("path_filestat_get", args(&[3, 0, PATH_AT, PATH.len() as i32, 0x3c0])),
        ("fd_readdir", args(&[3, 0x1000, 512, 0, 0x300])),
        ("fd_readdir", args(&[3, 0x1000, 512, 1, 0x300])),
        ("path_open", args(&[3, 0, at + 8, 8, 0, 0, 0, 0, 0x300])),
        ("path_open", args(&[3, 0, at + 8, 9, 0, 0, 0, 0, 0x300])),
        ("path_open", args(&[3, 0, at + 17, 3, 2, 0, 0, 0, 0x300])),
        ("path_open", args(&[3, 0, PATH_AT, PATH.len() as i32, 1, 64, 0, 0, 0x300])),
        ("path_readlink", args(&[3, PATH_AT, 5, 0x300, 16, 0x310])),
        ("fd_sync", args(&[3])),
        ("poll_oneoff", args(&[0x300, 0x400, 1, 0x500])),
    ];
    seeds.push(guest::program(0, files, &data));

    let at = guest::data_at(14);
    let mut data = iov(at + 16, 5);
    data.extend_from_slice(&iov(at + 21, 3));
    data.extend_from_slice(b"hello, \n");
    let system: &[(&str, [i32; ARGS])] = &[
        ("args_sizes_get", args(&[0x300, 0x304])),
        ("args_get", args(&[0x310, 0x320])),
        ("environ_sizes_get", args(&[0x300, 0x304])),
        ("environ_get", args(&[0x400, 0x500])),
        ("clock_res_get", args(&[0, 0x300])),
        ("clock_time_get", args(&[0, 1, 0x300])),
        ("clock_time_get", args(&[1, 1, 0x300])),
        ("random_get", args(&[0x300, 32])),
        ("sched_yield", args(&[])),
        ("fd_write", args(&[1, at, 2, 0x300])),
        ("fd_write", args(&[2, at, 1, 0x300])),
        ("fd_write", args(&[3, at, 1, 0x300])),
        ("fd_fdstat_set_flags", args(&[1, 0])),
        ("proc_exit", args(&[3])),
    ];
    seeds.push(guest::program(0, system, &data));

    let memory: &[(&str, [i32; ARGS])] = &[
        ("store32", args(&[0x300, 7])),
        ("fill", args(&[0x400, 0xab, 64])),
        ("copy", args(&[0x500, 0x400, 64])),
        ("panicked", args(&[0x500, 8])),
        ("copy", args(&[MEMORY as i32 - 8, 0x400, 64])),
    ];
    seeds.push(guest::program(0, memory, &[]));

    // Every function with every argument at the end of the memory, and with every argument -1
    // but where the host is known to believe the length ([`check`]'s corpus has those).
    let end = MEMORY as i32 - 4;
    let believed = ["output", "panicked", "ts_from_str", "literal_from_str", "random_get"];
    for &(_, name, ..) in IMPORTS {
        seeds.push(guest::program(0, &[(name, [end; ARGS])], &[]));
        if !believed.contains(&name) {
            seeds.push(guest::program(0, &[(name, [-1; ARGS])], &[]));
        }
    }
    seeds
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
    fn a_program_that_writes_a_stream_expands_to_it() {
        let input = super::guest::program(
            0,
            &[("output", super::args(&[super::guest::data_at(1), super::foo().len() as i32]))],
            &super::foo(),
        );
        let (_, output) = super::host().run(&input, 0);
        let want = blaze_macro_wasm::tokens("foo", super::guest::site(1)).unwrap();
        assert_eq!(output.expect("the module wrote a stream").to_string(), want.to_string());
    }
}
