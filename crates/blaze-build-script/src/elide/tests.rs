//! Elision of real build scripts (a census of crates.io's and this tree's, each named by where
//! it is from, its source as published), of the forms it reads, and of each form it refuses.

use super::*;
use crate::{
    output::{self, Context, Output},
    unit::Package,
};

fn elided(source: &str) -> Option<String> {
    elided_bytes(source.as_bytes())
}

fn elided_bytes(source: &[u8]) -> Option<String> {
    let files = BTreeMap::from([(PathBuf::from("/p/foo/build.rs"), source.to_vec())]);
    elide(Path::new("/p/foo/build.rs"), &files).map(|it| String::from_utf8(it).unwrap())
}

/// Build scripts as published, and what elision says of each: what they print, or `None`.
const CENSUS: &[(&str, &str, Option<&str>)] = &[
    (
        "wasm-bindgen 0.2.100",
        "// Empty `build.rs` so that `[package] links = ...` works in `Cargo.toml`.
fn main() {
    println!(\"cargo:rerun-if-changed=build.rs\");
}
",
        Some("cargo:rerun-if-changed=build.rs\n"),
    ),
    (
        "rayon-core 1.13.0",
        "// We need a build script to use `link = \"rayon-core\"`.  But we're not
// *actually* linking to anything, just making sure that we're the only
// rayon-core in use.
fn main() {
    // we don't need to rebuild for anything else
    println!(\"cargo:rerun-if-changed=build.rs\");
}
",
        Some("cargo:rerun-if-changed=build.rs\n"),
    ),
    (
        "mockall_derive 0.15.0",
        "fn main() {
    // Avoid unnecessary re-building.
    println!(\"cargo:rerun-if-changed=build.rs\");

    // Quiet rustc's helpful lint.  reprocheck is defined in CI.
    println!(\"cargo:rustc-check-cfg=cfg(reprocheck)\");
}
",
        Some("cargo:rerun-if-changed=build.rs\ncargo:rustc-check-cfg=cfg(reprocheck)\n"),
    ),
    (
        "mockall 0.15.0",
        r#"fn main() {
    println!("cargo::rustc-check-cfg=cfg(target_os, values(\"multics\"))");
}
"#,
        Some("cargo::rustc-check-cfg=cfg(target_os, values(\"multics\"))\n"),
    ),
    (
        "dashu-int 0.6.2",
        r###"//! Declare the `force_bits` cfg that the test workflow sets via
//! `RUSTFLAGS=--cfg force_bits=...` (16/32/64) so the matrix can exercise
//! the alternative `Word` widths. Without this declaration, stable Rust's
//! `unexpected_cfgs` lint (added by default in 1.80) fires on the three
//! `#[cfg(force_bits = "...")]` sites in `src/arch/mod.rs`.
fn main() {
    // Single-colon `cargo:` form for portability. The directive itself
    // was stabilised in Rust 1.80; older cargo (down to our MSRV) just
    // silently ignores unknown `cargo:` directives, which is fine because
    // the `unexpected_cfgs` lint they'd suppress doesn't exist at older
    // versions either.
    println!(r#"cargo:rustc-check-cfg=cfg(force_bits, values("16", "32", "64"))"#);
}
"###,
        Some("cargo:rustc-check-cfg=cfg(force_bits, values(\"16\", \"32\", \"64\"))\n"),
    ),
    (
        "screencapturekit-sys 0.2.8",
        "fn main() {
    println!(\"cargo:rustc-link-lib=framework=CoreFoundation\");
    println!(\"cargo:rustc-link-lib=framework=CoreMedia\");
    println!(\"cargo:rustc-link-lib=framework=CoreVideo\");
    println!(\"cargo:rustc-link-lib=framework=Foundation\");
    println!(\"cargo:rustc-link-lib=framework=CoreGraphics\");
    println!(\"cargo:rustc-link-lib=framework=CoreImage\");
    println!(\"cargo:rustc-link-lib=framework=ImageIO\");
    println!(\"cargo:rustc-link-lib=framework=ScreenCaptureKit\");
}
",
        Some(
            "cargo:rustc-link-lib=framework=CoreFoundation\n\
             cargo:rustc-link-lib=framework=CoreMedia\n\
             cargo:rustc-link-lib=framework=CoreVideo\n\
             cargo:rustc-link-lib=framework=Foundation\n\
             cargo:rustc-link-lib=framework=CoreGraphics\n\
             cargo:rustc-link-lib=framework=CoreImage\n\
             cargo:rustc-link-lib=framework=ImageIO\n\
             cargo:rustc-link-lib=framework=ScreenCaptureKit\n",
        ),
    ),
    (
        "tango-bench 0.6.0",
        "fn main() {
    println!(\"cargo:rustc-link-arg-benches=-rdynamic\");
    println!(\"cargo:rerun-if-changed=build.rs\");
}
",
        Some("cargo:rustc-link-arg-benches=-rdynamic\ncargo:rerun-if-changed=build.rs\n"),
    ),
    (
        "decancer 3.3.3",
        "fn main() {
  println!(\"cargo:rerun-if-changed=bin/bidi.bin\");
  println!(\"cargo:rerun-if-changed=bin/codepoints.bin\");
}
",
        Some("cargo:rerun-if-changed=bin/bidi.bin\ncargo:rerun-if-changed=bin/codepoints.bin\n"),
    ),
    (
        "defmt-macros 1.2.0",
        "fn main() {
    println!(\"cargo:rerun-if-env-changed=DEFMT_LOG\");
}
",
        Some("cargo:rerun-if-env-changed=DEFMT_LOG\n"),
    ),
    (
        "biscuit-auth 6.0.0",
        "/*
 * Copyright (c) 2019 Geoffroy Couprie <contact@geoffroycouprie.com> and Contributors to the Eclipse Foundation.
 * SPDX-License-Identifier: Apache-2.0
 */
fn main() {
    println!(\"cargo:rerun-if-changed=src/format/schema.proto\");
    //prost_build::compile_protos(&[\"src/format/schema.proto\"], &[\"src/\"]).unwrap();
}
",
        Some("cargo:rerun-if-changed=src/format/schema.proto\n"),
    ),
    (
        "wit-bindgen-rust 0.62.0",
        "fn main() {
    println!(\"cargo:rerun-if-changed=build.rs\");
    // this build script is currently only here so OUT_DIR is set for testing.
}
",
        Some("cargo:rerun-if-changed=build.rs\n"),
    ),
    (
        "wayland-client 0.31.15",
        "fn main() {
    println!(\"cargo:rustc-check-cfg=cfg(coverage)\");
    println!(\"cargo:rustc-check-cfg=cfg(unstable_coverage)\");
}
",
        Some("cargo:rustc-check-cfg=cfg(coverage)\ncargo:rustc-check-cfg=cfg(unstable_coverage)\n"),
    ),
    (
        "num-bigint-dig 0.9.1",
        "fn main() {\n    println!(\"cargo:rustc-cfg=has_i128\");\n}\n",
        Some("cargo:rustc-cfg=has_i128\n"),
    ),
    ("embassy-time-driver 0.1.0", "fn main() {}\n", Some("")),
    ("blaze-manifest's fixtures/inherit/a", "fn main(){}", Some("")),
    (
        "proc-macro-srv's proc-macro-test/imp",
        "//! This teaches cargo about our cfg(rust_analyzer)

fn main() {
    println!(\"cargo:rustc-check-cfg=cfg(rust_analyzer)\");
}
",
        Some("cargo:rustc-check-cfg=cfg(rust_analyzer)\n"),
    ),
    (
        "harness/buildmodel/links/dep",
        "fn main() {\n    println!(\"cargo:answer=42\");\n    println!(\"cargo:rerun-if-changed=build.rs\");\n}\n",
        Some("cargo:answer=42\ncargo:rerun-if-changed=build.rs\n"),
    ),
    (
        "harness/buildmodel/reload_git_checkout/next/app",
        "fn main() {\n    println!(\"cargo:rustc-cfg=scripted\");\n    println!(\"cargo:rustc-env=BM_FROM_SCRIPT=first\");\n}\n",
        Some("cargo:rustc-cfg=scripted\ncargo:rustc-env=BM_FROM_SCRIPT=first\n"),
    ),
    // Refused, each for what it does besides printing constants.
    ("blaze-manifest's fixtures/discovery/crates/app: empty, no main", "", None),
    (
        "prettyplease 0.1.25: concat! and env!",
        "fn main() {
    println!(\"cargo:rerun-if-changed=build.rs\");

    println!(concat!(\"cargo:VERSION=\", env!(\"CARGO_PKG_VERSION\")));
}
",
        None,
    ),
    (
        "bollard-buildkit-proto 0.7.0: main returns a Result",
        "fn main() -> Result<(), Box<dyn std::error::Error>> {\n    Ok(())\n}\n",
        None,
    ),
    (
        "cpal 0.18.2: a use, a const, a captured argument and a branch",
        "use std::env;

const CPAL_ASIO_DIR: &str = \"CPAL_ASIO_DIR\";

fn main() {
    println!(\"cargo:rerun-if-env-changed={CPAL_ASIO_DIR}\");
    if env::var(CPAL_ASIO_DIR).is_ok() {
        println!(\"cargo:rustc-cfg=asio\");
    }
}
",
        None,
    ),
    (
        "libafl_bolts 0.16.1: attributes and another function",
        "#[rustversion::nightly]
fn nightly() {
    println!(\"cargo:rustc-cfg=nightly\");
}

#[rustversion::not(nightly)]
fn nightly() {}

fn main() {
    println!(\"cargo:rerun-if-changed=build.rs\");
    println!(\"cargo:rustc-check-cfg=cfg(nightly)\");
    nightly();
    #[cfg(target_env = \"musl\")]
    println!(\"cargo:rustc-link-lib=ucontext\");
}
",
        None,
    ),
    (
        "moka 0.12.10: a main behind a cfg",
        "#[cfg(rustver)]
fn main() {
    use rustc_version::version;
    let version = version().expect(\"Can't get the rustc version\");
    println!(
        \"cargo:rustc-env=RUSTC_SEMVER={}.{}\",
        version.major, version.minor
    );
}

#[cfg(not(rustver))]
fn main() {}
",
        None,
    ),
    (
        "inkwell 0.11.0: cfg!",
        "fn main() {
    if cfg!(all(not(target_os = \"windows\"), not(feature = \"no-libffi-linking\"))) {
        println!(\"cargo:rustc-link-lib=dylib=ffi\");
    }
}
",
        None,
    ),
    (
        "borsh 1.5.7: another macro",
        "use cfg_aliases::cfg_aliases;

fn main() {
    cfg_aliases! {
        hash_collections: { any(feature = \"hashbrown\", feature = \"std\") },
    }
}
",
        None,
    ),
    ("chrono-tz 0.10.0: a dependency's function", "fn main() {\n    chrono_tz_build::main();\n}\n", None),
    (
        "harness/buildmodel/opt_level_dev: reads the environment",
        "fn main() {
    for (name, var) in [(\"BUILD_PROFILE\", \"PROFILE\"), (\"BUILD_OPT\", \"OPT_LEVEL\"), (\"BUILD_DEBUG\", \"DEBUG\")] {
        println!(\"cargo:rustc-env={name}={}\", std::env::var(var).unwrap());
    }
}
",
        None,
    ),
    (
        "libm (compiler-builtins): a module",
        "mod configure;

use configure::{Config, Library};

fn main() {
    println!(\"cargo:rerun-if-changed=build.rs\");
    println!(\"cargo:rerun-if-changed=configure.rs\");

    let cfg = Config::from_env(Library::Libm);
    configure::emit(&cfg);
}
",
        None,
    ),
    (
        "oid-registry 0.8.1: include!",
        "use std::env;

include!(\"src/load.rs\");

fn main() -> Result<()> {
    println!(\"cargo:rerun-if-changed=assets/oid_db.txt\");

    let out_dir = env::var_os(\"OUT_DIR\").unwrap();
    let dest_path = Path::new(&out_dir).join(\"oid_db.rs\");

    let m = load_file(\"assets/oid_db.txt\")?;
    generate_file(&m, dest_path)?;

    Ok(())
}
",
        None,
    ),
    (
        "encoding_rs 0.8.42: a macro of a dependency",
        "fn main() {
    println!(\"cargo::rustc-check-cfg=cfg(slow_mm_packus_epi16)\");
    if rustversion::cfg!(all(since(1.96), before(1.99))) {
        println!(\"cargo:rustc-cfg=slow_mm_packus_epi16\");
    }
}
",
        None,
    ),
    (
        "proc-macro-srv: runs rustc",
        "//! Determine rustc version `proc-macro-srv` (and thus the sysroot ABI) is
//! build with and make it accessible at runtime for ABI selection.

use std::{env, process::Command};

fn main() {
    let rustc = env::var(\"RUSTC\").expect(\"proc-macro-srv's build script expects RUSTC to be set\");
    #[allow(clippy::disallowed_methods)]
    let output = Command::new(rustc).arg(\"--version\").output().expect(\"rustc --version must run\");
    let version_string = std::str::from_utf8(&output.stdout[..])
        .expect(\"rustc --version output must be UTF-8\")
        .trim();
    println!(\"cargo::rustc-env=RUSTC_VERSION={version_string}\");
}
",
        None,
    ),
];

#[test]
fn census() {
    for (from, source, expected) in CENSUS {
        assert_eq!(elided(source).as_deref(), *expected, "{from}");
    }
}

/// Each form elision refuses, alone in an otherwise inert script.
#[test]
fn every_rejection() {
    for source in [
        // Attributes, items and other ways to change `main`, `println!` or what is read.
        "#![allow(unused)] fn main() {}",
        "#[inline] fn main() {}",
        "fn main() { #[cfg(unix)] println!(\"cargo:rustc-cfg=u\"); }",
        "extern crate std; fn main() {}",
        "extern \"C\" fn main() {}",
        "use std::io; fn main() {}",
        "mod a; fn main() {}",
        "macro_rules! println { ($($t:tt)*) => {} } fn main() { println!(\"cargo:x=1\"); }",
        "fn main() { println!(\"cargo:x=1\"); } fn other() {}",
        "fn helper() {} fn main() {}",
        "fn main() {} ;",
        "#!/usr/bin/env run-cargo-script\nfn main() {}",
        // Another `main`.
        "fn main() -> () {}",
        "pub fn main() {}",
        "async fn main() {}",
        "unsafe fn main() {}",
        "const fn main() {}",
        "fn r#main() {}",
        "fn main(_: u8) {}",
        "fn main<T>() {}",
        "fn main() where {}",
        // Other statements, macros and paths.
        "fn main() { let x = 1; }",
        "fn main() { ; }",
        "fn main() { {} }",
        "fn main() { std::println!(\"cargo:x=1\"); }",
        "fn main() { ::std::println!(\"cargo:x=1\"); }",
        "fn main() { r#println!(\"cargo:x=1\"); }",
        "fn main() { eprintln!(\"cargo:x=1\"); }",
        "fn main() { format!(\"cargo:x=1\"); }",
        "fn main() { println(\"cargo:x=1\"); }",
        "fn main() { if cfg!(unix) { println!(\"cargo:rustc-cfg=u\"); } }",
        "fn main() { println!(include_str!(\"x\")); }",
        "fn main() { println!(\"cargo:rustc-env=A={}\", env!(\"X\")); }",
        "fn main() { println!(\"cargo:rustc-env=A={}\", x); }",
        "fn main() { println!(\"cargo:rustc-env=A={}\", main); }",
        "fn main() { println!(\"cargo:rustc-env=A={}\", (\"a\")); }",
        "fn main() { println!(\"cargo:rustc-env=A={}\", [\"a\"]); }",
        "fn main() { println!(\"cargo:rustc-env=A={}\", \"a\".len()); }",
        // Statements that need their `;`, and an empty statement after a braced macro.
        "fn main() { println!(\"cargo:x=1\") println!(\"cargo:y=1\"); }",
        "fn main() { println![\"cargo:x=1\"] println!(\"cargo:y=1\"); }",
        "fn main() { println!{\"cargo:x=1\"}; }",
        "fn main() { println!(\"cargo:x=1\"); ; }",
        "fn main() { println!(\"cargo:x=1\"]; }",
        // Format strings and arguments.
        "fn main() { println!(); }",
        "fn main() { print!(); }",
        "fn main() { println!(\"cargo:x={y}\"); }",
        "fn main() { println!(\"cargo:x={:?}\", \"a\"); }",
        "fn main() { println!(\"cargo:x={:}\", \"a\"); }",
        "fn main() { println!(\"cargo:x={0:}\", \"a\"); }",
        "fn main() { println!(\"cargo:x={:>4}\", \"a\"); }",
        "fn main() { println!(\"cargo:x={ }\", \"a\"); }",
        "fn main() { println!(\"cargo:x={00}\", \"a\"); }",
        "fn main() { println!(\"cargo:x={}\"); }",
        "fn main() { println!(\"cargo:x=\", \"a\"); }",
        "fn main() { println!(\"cargo:x={1}\", \"a\"); }",
        "fn main() { println!(\"cargo:x={0}\", \"a\", \"b\"); }",
        "fn main() { println!(\"cargo:x={}\", x = \"a\"); }",
        "fn main() { println!(\"cargo:x=}\"); }",
        "fn main() { println!(\"cargo:x={\"); }",
        "fn main() { println!(\"cargo:x={}\", \"a\",,); }",
        "fn main() { println!(\"cargo:x=1\" \"a\"); }",
        "fn main() { println!(\"cargo:x=1\";); }",
        r#"fn main() { println!("cargo:x=\u{7b}\u{7d}", 1); }"#,
        r#"fn main() { println!("cargo:x=\x7b}}"); }"#,
        "fn main() { println!(b\"cargo:x=1\"); }",
        "fn main() { println!(c\"cargo:x=1\"); }",
        "fn main() { println!('c'); }",
        "fn main() { println!(1); }",
        "fn main() { println!(\"cargo:x={}\", 3000000000); }",
        "fn main() { println!(\"cargo:x={}\", -1); }",
        "fn main() { println!(\"cargo:x={}\", 1u8); }",
        "fn main() { println!(\"cargo:x={}\", 0x10); }",
        "fn main() { println!(\"cargo:x={}\", 1.5); }",
        "fn main() { println!(\"cargo:x={}\", b'a'); }",
        "fn main() { println!(\"cargo:x={}\", 'a); }",
        "fn main() { println!(\"cargo:x={}\", r#true); }",
        "fn main() { println!(\"cargo:x={}\", None); }",
        "fn main() { println!(\"cargo:x=1\"suffix); }",
        // What is printed: a line that is not a directive, or output not ended.
        "fn main() { println!(\"hello\"); }",
        "fn main() { println!(\"cargo:x=1\\nhello\"); }",
        "fn main() { println!(\" cargo:x=1\"); }",
        "fn main() { print!(\"cargo:x=1\"); }",
        "fn main() { println!(\"cargo:x=1\\n\"); }",
        // Doc comments where rustc refuses them or reads them as attributes of a statement.
        "fn main() { /// doc\n println!(\"cargo:x=1\"); }",
        "fn main() { println!(\"cargo:x=1\"); /** doc */ }",
        "/// a\n//! b\nfn main() {}",
        "fn main() {}\n/// trailing",
        "fn main() {}\n//! inner, late",
        // Sources that do not lex or do not close.
        "fn main() { println!(\"cargo:x=1\"); /* unterminated }",
        "fn main() { println!(\"cargo:x=1); }",
        "fn main() { println!(\"cargo:x=1\");",
        "fn main() { println!(\"cargo:x=1\"); }}",
        "fn main() {\r println!(\"cargo:x=1\"); }",
        "fn main() { println!(\"cargo:x=\u{202e}1\"); }",
        "fn main() { println!(\"cargo:x=1\"); } // \u{2067}",
        "fn mäin() {}",
    ] {
        assert_eq!(elided(source), None, "{source:?}");
    }
    assert_eq!(elided_bytes(b"fn main() { println!(\"cargo:x=\xff\"); }"), None, "not UTF-8");
    let files = BTreeMap::from([(PathBuf::from("/p/foo/other.rs"), b"fn main() {}".to_vec())]);
    assert_eq!(elide(Path::new("/p/foo/build.rs"), &files), None, "no root");
}

/// The forms read, each printing what rustc's program would.
#[test]
fn what_each_form_prints() {
    for (source, printed) in [
        (
            r#"fn main() { println!("cargo:rustc-env=A={}-{}-{}-{}-{}", "s", 'c', 42, true, false); }"#,
            "cargo:rustc-env=A=s-c-42-true-false\n",
        ),
        (
            r#"fn main() { println!("cargo:rustc-env=A={1}{0}{1}{}", "x", "y"); }"#,
            "cargo:rustc-env=A=yxyx\n",
        ),
        (r#"fn main() { println!("cargo:x={}", 2147483647); }"#, "cargo:x=2147483647\n"),
        (r#"fn main() { println!("cargo:x={}", 1_000); }"#, "cargo:x=1000\n"),
        (r#"fn main() { println!("cargo:x={}", 007); }"#, "cargo:x=7\n"),
        (r#"fn main() { println!("cargo:x={}", "{}"); }"#, "cargo:x={}\n"),
        (r#"fn main() { println!("cargo:x={}", "\u{7b}"); }"#, "cargo:x={\n"),
        (r#"fn main() { println!("cargo:x={{}}"); }"#, "cargo:x={}\n"),
        (r#"fn main() { println!("cargo:x={{{}}}", "a"); }"#, "cargo:x={a}\n"),
        (r#"fn main() { println!("cargo:x={}", "a",); }"#, "cargo:x=a\n"),
        (r#"fn main() { println!("cargo:x=1",); }"#, "cargo:x=1\n"),
        (
            r#"fn main() { println!("cargo:rustc-env=A=\x41\u{42}\t\\\"\'\0"); }"#,
            "cargo:rustc-env=A=AB\t\\\"'\0\n",
        ),
        (r###"fn main() { println!(r##"cargo:x="#"##); }"###, "cargo:x=\"#\n"),
        (r###"fn main() { println!(r#"cargo:x={}"#, r"a\"); }"###, "cargo:x=a\\\n"),
        (r#"fn main() { println!(r"cargo:x={}\n", "a"); }"#, "cargo:x=a\\n\n"),
        (
            "fn main() { print!(\"cargo:a=1\\n\"); print!(\"cargo:b=2\\ncargo:c=\"); print!(\"3\\n\") }",
            "cargo:a=1\ncargo:b=2\ncargo:c=3\n",
        ),
        (
            "fn main() { println!{\"cargo:a=1\"} println![\"cargo:b=2\"]; println![\"cargo:c=3\"] }",
            "cargo:a=1\ncargo:b=2\ncargo:c=3\n",
        ),
        ("fn main() { println!(\"cargo:a=1\") }", "cargo:a=1\n"),
        (
            "fn main ( ) { println /* a comment */ ! ( \"cargo:a=1\" ) ; }",
            "cargo:a=1\n",
        ),
        ("fn main() { println!(\"cargo:a=\\\n      1\"); }", "cargo:a=1\n"),
        ("fn main() { println!(\"cargo:a=1\ncargo:b=2\"); }", "cargo:a=1\ncargo:b=2\n"),
        ("fn main() { println!(\"cargo:a=é\u{301}✓\"); }", "cargo:a=é\u{301}✓\n"),
        // rustc reads a file with `\r\n` as with `\n`, string literals too.
        (
            "fn main() {\r\n    println!(\"cargo:a=1\r\ncargo:b=2\");\r\n}\r\n",
            "cargo:a=1\ncargo:b=2\n",
        ),
        ("\u{feff}fn main() { println!(\"cargo:a=1\"); }", "cargo:a=1\n"),
        ("/* a /* nested */ comment */ fn main() {}", ""),
        ("/** outer */ /// outer too\nfn main() {}", ""),
        ("//! inner\n/*! inner too */\n/// outer\nfn main() {}", ""),
        ("/**/ /***/ //// plain\nfn main() {}", ""),
        ("fn main() { println!(\"cargo:a=1\"); } // done\n", "cargo:a=1\n"),
    ] {
        assert_eq!(elided(source).as_deref(), Some(printed), "{source:?}");
    }
}

/// What is elided goes through cargo's reading of it as a run's output does.
#[test]
fn read_as_a_runs_output() {
    let package = Package { name: "foo".into(), version: "0.1.0".into(), ..Package::default() };
    let out = Path::new("/p/foo/target/debug/build/foo-1/out");
    let cx = Context {
        package: &package,
        out_dir_when_generated: out,
        out_dir: out,
        nightly_features_allowed: false,
        rustc_bootstrap: None,
    };
    let printed = elided(
        "fn main() {
    println!(\"cargo:rerun-if-changed=build.rs\");
    println!(\"cargo::rustc-check-cfg=cfg(foo)\");
    println!(\"cargo:rustc-cfg=foo\");
    println!(\"cargo:links=1\");
}",
    )
    .unwrap();
    let output = output::parse(printed.as_bytes(), &cx).unwrap();
    assert_eq!(output, Output {
        cfgs: vec!["foo".into()],
        check_cfgs: vec!["cfg(foo)".into()],
        metadata: vec![("links".into(), "1".into())],
        rerun_if_changed: vec!["build.rs".into()],
        ..Output::default()
    });
}

#[test]
fn verify_names_the_first_line_that_differs() {
    assert_eq!(verify(b"cargo:a=1\ncargo:b=2\n", b"cargo:a=1\ncargo:b=2\n"), Ok(()));
    assert_eq!(verify(b"", b""), Ok(()));
    assert_eq!(
        verify(b"cargo:a=1\ncargo:b=2\n", b"cargo:a=1\ncargo:b=3\n").unwrap_err().0,
        "the elided build script's output differs from its run's at line 2: elision says \
         \"cargo:b=2\\n\", the run printed \"cargo:b=3\\n\""
    );
    assert_eq!(
        verify(b"cargo:a=1\n", b"cargo:a=1\ncargo:b=2\n").unwrap_err().0,
        "the elided build script's output differs from its run's at line 2: elision says \
         nothing, the run printed \"cargo:b=2\\n\""
    );
    assert_eq!(
        verify(b"cargo:a=1\n", b"cargo:a=1").unwrap_err().0,
        "the elided build script's output differs from its run's at line 1: elision says \
         \"cargo:a=1\\n\", the run printed \"cargo:a=1\""
    );
}

#[test]
fn flags_that_leave_elision_true() {
    let flags = |it: &[&str]| it.iter().map(|it| (*it).to_owned()).collect::<Vec<_>>();
    for allowed in [
        flags(&[]),
        flags(&["-C", "opt-level=3", "-Cdebuginfo=2", "-Copt-level=s"]),
        flags(&["--cfg", "foo", "--cfg", "feature=\"std\"", "--cfg=bar"]),
        flags(&["-C", "debuginfo=line-tables-only"]),
    ] {
        assert!(flags_allow(&allowed), "{allowed:?}");
    }
    for refused in [
        flags(&["-D", "warnings"]),
        flags(&["-Dwarnings"]),
        flags(&["-Dmissing-docs"]),
        flags(&["-Z", "crate-attr=no_std"]),
        flags(&["-C", "link-arg=-fuse-ld=mold"]),
        flags(&["-C", "target-cpu=native"]),
        flags(&["-C", "opt-level=4"]),
        flags(&["-C"]),
        flags(&["--cfg"]),
        flags(&["--cfg", "a b"]),
        flags(&["--cfg", "x=\"a\\\"b\""]),
        flags(&["--cfg", "x=unquoted"]),
        flags(&["--check-cfg", "cfg(foo)"]),
        flags(&["--extern", "std=/x"]),
        flags(&["--edition", "2015"]),
    ] {
        assert!(!flags_allow(&refused), "{refused:?}");
    }
}
