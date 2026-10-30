//! Feeds the host's reader of the trees a proc-macro module writes (`wire::read`, tt's columns
//! as `Tree::write_wire` lays them out, then the package's paths put back): a fixed module
//! (`macro_wasm_guest.rs`, [`guest::Kind::Echo`]), compiled and linked once through
//! `Wasm::at`, reads the input from its package and gives it back as its expansion's output,
//! which a `Session` decodes. Each span part the bytes name is `{file_id: index, ast_id: 0,
//! ctx: 0}`, an index of the expansion's parts: the def, call and mixed sites first, which are
//! one part here.
//!
//! Invariants, each checked below (a violation panics, which the engine reports as a crash):
//! - decoding never panics, whatever the module wrote;
//! - it asks for at most `blaze_fuzz::allowed(len)` bytes at once: a count or a length is
//!   not believed before the bytes it promises are there;
//! - what it decodes is no bigger than what was written: every token tree is a node of 2 words
//!   and a span of 2, so a decoded tree has at most one token tree for every 16 bytes.

#![forbid(unsafe_code)]

#[allow(dead_code)]
#[path = "macro_wasm_guest.rs"]
mod guest;

use std::{sync::OnceLock, time::Duration};

use blaze_expand::bridge::wire::{self, Parts};
use blaze_fuzzer::Target;
use guest::{CAP, Host, Kind, site};

const TARGET: Target = Target {
    name: "macro_wasm_wire",
    check,
    seeds,
    dictionary: &[
        // tt's `MAGIC`, the first word.
        b"tt\x01\x00",
        b"\x01\x00\x00\x00",
        b"\x02\x00\x00\x00",
        // A part row naming the first part.
        b"\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00",
        // `WIDE`, a span's `len_part` saying it is in the wide spans.
        b"\xff\xff\xff\xff",
        // `NARROW`, the length or part only a wide span may have.
        b"\xff\xff\x00\x00",
        b"/blaze/package",
    ],
    max_len: 1 << 16,
    budget: Duration::from_secs(600),
};

fn host() -> &'static Host {
    static HOST: OnceLock<Host> = OnceLock::new();
    HOST.get_or_init(|| Host::new(TARGET.name, Kind::Echo))
}

fn check(data: &[u8]) {
    let (largest, output) = host().run(data, 0);
    let bound = blaze_fuzz::allowed(data.len());
    assert!(
        largest <= bound,
        "decoding {} bytes asked for {largest} bytes at once, past {bound}: a count or a length \
         was believed",
        data.len()
    );
    if let Ok(top) = output {
        let written = data.len().min(CAP as usize);
        let trees = top.token_trees().len();
        assert!(
            trees * 16 <= written,
            "{written} bytes decoded to {trees} token trees, more than they can hold"
        );
    }
}

/// The bytes a module writes for the tokens of `text`: tt's columns with each part named by its
/// index among the expansion's parts, the sites' first, as the host names them.
fn written(text: &str) -> Vec<u8> {
    let tree = blaze_macro_wasm::tokens(text, site(1))
        .unwrap_or_else(|e| panic!("a seed's text lexes: {e}: {text:?}"));
    let mut parts = Parts::default();
    for at in [0, 1, 2] {
        parts.pack(site(at));
    }
    let mut out = Vec::new();
    wire::relative(&tree, &mut parts, &mut out);
    out
}

/// Trees as syn and quote make them: of every kind of tree, nested, long.
fn seeds() -> Vec<Vec<u8>> {
    // 24 bytes a group (its node, its span and its closing span), under `max_len`.
    let deep = format!("{}x{}", "(".repeat(2000), ")".repeat(2000));
    let long = (0..3000).map(|at| ["an_ident", "+", "17u64"][at % 3]).collect::<Vec<_>>().join(" ");
    [
        "",
        "foo",
        "#[derive(r#Debug)] struct S { a :: \"s\" r##\"raw\"## -1i8 2.5f32 };",
        "'x' b'c' 7 1.5 \"s\" r#\"r\"# b\"b\" br\"rb\" c\"c\" cr###\"rc\"### \"s\"suffix",
        // A path the module made of its package, which the host puts its own in place of.
        "\"/blaze/package/schema.json\" [r#\"/blaze/package\"#]",
        deep.as_str(),
        long.as_str(),
    ]
    .into_iter()
    .map(written)
    .collect()
}

#[cfg_attr(test, allow(dead_code))]
fn main() -> std::process::ExitCode {
    blaze_fuzzer::main(&TARGET)
}

#[cfg(test)]
mod tests {
    use super::{guest::site, host, written};

    fn decoded(bytes: &[u8]) -> String {
        let (_, output) = host().run(bytes, 0);
        output.expect("the module wrote a tree").to_string()
    }

    fn lexed(text: &str) -> String {
        blaze_macro_wasm::tokens(text, site(1)).unwrap().to_string()
    }

    #[test]
    fn seeds() {
        blaze_fuzzer::seeds(&super::TARGET);
    }

    #[test]
    fn a_tree_the_module_wrote_is_the_tokens_it_says() {
        for text in ["foo + \"s\"", "-1i8 -1.5", "a (b [c]) \"d\" 1u8"] {
            assert_eq!(decoded(&written(text)), lexed(text));
        }
    }

    #[test]
    fn a_part_the_expansion_lacks_is_refused() {
        let mut bytes = written("foo");
        let at = blaze_expand::bridge::wire::sections(&bytes).expect("one tree").parts;
        bytes[at..at + 4].copy_from_slice(&7u32.to_le_bytes());
        let (_, output) = host().run(&bytes, 0);
        assert!(output.is_err(), "a part index past the expansion's parts is read");
    }

    #[test]
    fn a_path_of_the_package_is_the_package_on_this_machine() {
        let root = host().package().to_string_lossy().into_owned();
        assert_eq!(
            decoded(&written("\"/blaze/package/schema.json\"")),
            lexed(&format!("\"{root}/schema.json\""))
        );
    }
}
