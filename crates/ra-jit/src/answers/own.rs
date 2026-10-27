//! ra-jit's own answers to rustc's questions, from what it knows of a release of rustc and of
//! the targets that release has: no rustc runs. The data of each release is what that rustc
//! printed for every target ra-jit has (`harness/answers.py --write`), and the cfgs that follow
//! from the arguments are added here, as rustc adds them. A question this does not understand
//! whole is answered `None`, and rustc is asked.

use std::{
    collections::BTreeMap,
    ffi::OsString,
    path::{Path, PathBuf},
    sync::OnceLock,
};

use serde_derive::Deserialize;

/// What the rustc of the toolchain in `harness/toolchain` says (`harness/answers.py --write`
/// writes it when the toolchain moves).
const RELEASES: [&str; 1] = [include_str!("rustc.json")];

#[derive(Deserialize)]
struct Release {
    /// As the first line of `-V` has it, after `rustc `.
    version: String,
    /// What `-vV` prints, with `{host}` for the host's tuple.
    verbose: String,
    targets: BTreeMap<String, Target>,
}

#[derive(Deserialize)]
struct Target {
    /// The cfgs of the target, but those the arguments decide.
    cfg: Vec<String>,
    /// The cfgs printed besides those when unstable features are allowed.
    nightly_cfg: Vec<String>,
    /// The strategy of `-C panic` when none is given.
    panic: String,
    split_debuginfo: Vec<String>,
    /// The name of the file of each kind of crate named `___`; `None` for a kind the target
    /// does not have.
    file_names: BTreeMap<String, Option<String>>,
    /// By the host rustc runs on, whose build decides some of it.
    target_spec_json: BTreeMap<String, String>,
}

fn releases() -> &'static [Release] {
    static RELEASES_READ: OnceLock<Vec<Release>> = OnceLock::new();
    RELEASES_READ.get_or_init(|| {
        RELEASES
            .iter()
            .map(|it| serde_json::from_str(it).expect("the answers of a release"))
            .collect()
    })
}

/// The toolchain `rustc` is of, as far as ra-jit can tell without running it.
struct Toolchain {
    sysroot: PathBuf,
    host: String,
    release: &'static Release,
}

impl Toolchain {
    /// From rustup's records in the sysroot: the release of rustc and the host it runs on.
    /// rustc finds its sysroot by where its library is, which is beside it.
    fn of(rustc: &Path) -> Option<Toolchain> {
        let sysroot = rustc.parent()?.parent()?.to_owned();
        let lib = sysroot.join("lib");
        let has_driver = std::fs::read_dir(&lib).ok()?.flatten().any(|it| {
            it.file_name().to_str().is_some_and(|name| name.starts_with("librustc_driver-"))
        });
        if !has_driver {
            return None;
        }
        let rustlib = lib.join("rustlib");
        let host = std::fs::read_dir(&rustlib).ok()?.flatten().find_map(|it| {
            it.file_name().to_str()?.strip_prefix("manifest-rustc-").map(str::to_owned)
        })?;
        let manifest =
            std::fs::read_to_string(rustlib.join("multirust-channel-manifest.toml")).ok()?;
        let (_, rest) = manifest.split_once("\n[pkg.rustc]\nversion = \"")?;
        let (version, _) = rest.split_once('"')?;
        let release = releases().iter().find(|it| it.version == version)?;
        Some(Toolchain { sysroot, host, release })
    }
}

/// How cfgs follow from `-C` and `--cfg`.
#[derive(Default)]
struct Given<'a> {
    target: Option<&'a str>,
    opt_level: Option<&'a str>,
    debug_assertions: Option<bool>,
    overflow_checks: Option<bool>,
    panic: Option<&'a str>,
    cfgs: Vec<&'a str>,
    crate_name: Option<&'a str>,
    crate_types: Vec<&'a str>,
    prints: Vec<&'a str>,
    unstable_options: bool,
    from_stdin: bool,
    extra_filename: bool,
}

/// The flags that take a value and change no answer.
const IGNORED_VALUED: [&str; 12] = [
    "--edition",
    "--error-format",
    "--json",
    "--emit",
    "--out-dir",
    "-L",
    "--extern",
    "--check-cfg",
    "--cap-lints",
    "--color",
    "--diagnostic-width",
    "--remap-path-prefix",
];

/// The lint flags, whose value changes no answer.
const LINTS: [&str; 10] = [
    "-W",
    "-A",
    "-D",
    "-F",
    "--warn",
    "--allow",
    "--deny",
    "--forbid",
    "--force-warn",
    "--cap-lints",
];

/// The codegen options that change no answer.
const IGNORED_CODEGEN: [&str; 16] = [
    "embed-bitcode",
    "debuginfo",
    "metadata",
    "incremental",
    "strip",
    "codegen-units",
    "prefer-dynamic",
    "lto",
    "linker",
    "link-arg",
    "link-args",
    "split-debuginfo",
    "link-dead-code",
    "rpath",
    "force-frame-pointers",
    "force-unwind-tables",
];

fn yes(value: Option<&str>) -> Option<bool> {
    match value {
        None | Some("y" | "yes" | "on" | "true") => Some(true),
        Some("n" | "no" | "off" | "false") => Some(false),
        Some(_) => None,
    }
}

impl<'a> Given<'a> {
    fn of(args: &'a [&'a str]) -> Option<Given<'a>> {
        let mut given = Given::default();
        let mut words = args.iter().copied();
        while let Some(word) = words.next() {
            let (flag, joined) = match word.split_once('=') {
                Some((flag, value)) if flag.starts_with("--") => (flag, Some(value)),
                _ => match word.as_bytes() {
                    // `-Cname=value`, `-Zname`, `-Wlint`, `-Ldir`.
                    [b'-', b'C' | b'Z' | b'W' | b'A' | b'D' | b'F' | b'L', _, ..] => {
                        (&word[..2], Some(&word[2..]))
                    }
                    _ => (word, None),
                },
            };
            let mut value = || joined.or_else(|| words.next());
            match flag {
                "-" => given.from_stdin = true,
                "--print" => given.prints.push(value()?),
                "--target" => given.target = Some(value()?),
                "--crate-name" => given.crate_name = Some(value()?),
                "--crate-type" => given.crate_types.extend(value()?.split(',')),
                "--cfg" => given.cfgs.push(value()?),
                "-O" => given.opt_level = Some("2"),
                "-g" => {}
                "-C" | "--codegen" => {
                    let option = value()?;
                    let (name, value) = match option.split_once('=') {
                        Some((name, value)) => (name, Some(value)),
                        None => (option, None),
                    };
                    match name.replace('_', "-").as_str() {
                        "opt-level" => given.opt_level = Some(value?),
                        "debug-assertions" => given.debug_assertions = Some(yes(value)?),
                        "overflow-checks" => given.overflow_checks = Some(yes(value)?),
                        "panic" => given.panic = Some(value?),
                        "extra-filename" => given.extra_filename = true,
                        name if IGNORED_CODEGEN.contains(&name) => {}
                        _ => return None,
                    }
                }
                "-Z" => match crate::unstable::flag(value()?) {
                    flag if flag.name == "unstable-options" => given.unstable_options = true,
                    // One that changes only how rustc works changes no answer.
                    flag if matches!(flag.treatment(), crate::unstable::Treatment::Ignored(_)) => {}
                    _ => return None,
                },
                _ if LINTS.contains(&flag) || IGNORED_VALUED.contains(&flag) => {
                    value()?;
                }
                // An input rustc does not read for what it is asked.
                _ if !flag.starts_with('-') => {}
                _ => return None,
            }
        }
        Some(given)
    }

    fn debug_assertions(&self) -> bool {
        self.debug_assertions.unwrap_or(matches!(self.opt_level, None | Some("0")))
    }
}

/// The variables rustc reads for a question that ra-jit answers only when unset.
const UNSET: [&str; 8] = [
    "RUSTC_OVERRIDE_VERSION_STRING",
    "RUST_TARGET_PATH",
    "SDKROOT",
    "MACOSX_DEPLOYMENT_TARGET",
    "IPHONEOS_DEPLOYMENT_TARGET",
    "TVOS_DEPLOYMENT_TARGET",
    "WATCHOS_DEPLOYMENT_TARGET",
    "XROS_DEPLOYMENT_TARGET",
];

/// The cfgs rustc sets itself, which it refuses as `--cfg`, by name or by what it starts with.
const BUILTIN_CFGS: [&str; 13] = [
    "debug_assertions",
    "overflow_checks",
    "ub_checks",
    "contract_checks",
    "fmt_debug",
    "relocation_model",
    "panic",
    "unix",
    "windows",
    "test",
    "emscripten_wasm_eh",
    "sanitize",
    "sanitizer_",
];

/// Whether `cfg` is `name` or `name="value"` as rustc prints it back: a plain name of no
/// builtin cfg, a value with nothing to escape.
fn is_plain_cfg(cfg: &str) -> bool {
    let (name, value) = match cfg.split_once('=') {
        Some((name, value)) => (name, Some(value)),
        None => (cfg, None),
    };
    let plain_name = name.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_');
    let builtin = name.starts_with("target_")
        || BUILTIN_CFGS.iter().any(|it| name == *it || (it.ends_with('_') && name.starts_with(it)));
    let plain_value = value.is_none_or(|value| {
        let inner = value.strip_prefix('"').and_then(|it| it.strip_suffix('"'));
        inner.is_some_and(|it| {
            it.chars().all(|c| c.is_ascii_graphic() && c != '"' && c != '\\' || c == ' ')
        })
    });
    plain_name && !builtin && plain_value
}

/// What `rustc` (the toolchain's executable) prints for `args` with `stdin`, with the
/// variables `var` reads; `None` for what this does not answer.
pub(super) fn answer(
    rustc: &Path,
    args: &[OsString],
    stdin: &[u8],
    var: &dyn Fn(&str) -> Option<OsString>,
) -> Option<Vec<u8>> {
    if UNSET.iter().any(|it| var(it).is_some()) {
        return None;
    }
    let args = args.iter().map(|it| it.to_str()).collect::<Option<Vec<_>>>()?;
    // A driver of rustc's (clippy's) may answer otherwise.
    if rustc.file_stem()? != "rustc" {
        return None;
    }
    let toolchain = Toolchain::of(rustc)?;
    let release = toolchain.release;
    // Whether unstable features are allowed: always on a nightly (the toolchain ra-jit is
    // pinned to), unless `RUSTC_BOOTSTRAP=-1` makes it act as a stable release; a list of
    // crates is not answered.
    let nightly = match var("RUSTC_BOOTSTRAP") {
        None => release.version.contains("-nightly") || release.version.contains("-dev"),
        Some(it) if it == "1" => true,
        Some(it) if it == "-1" => false,
        Some(_) => return None,
    };
    if !args.is_empty()
        && args.iter().all(|it| matches!(*it, "-V" | "-v" | "-vV" | "--version" | "--verbose"))
    {
        let verbose = args.iter().any(|it| matches!(*it, "-v" | "-vV" | "--verbose"));
        let text = if verbose {
            release.verbose.replace("{host}", &toolchain.host)
        } else {
            format!("rustc {}\n", release.version)
        };
        return Some(text.into_bytes());
    }
    let given = Given::of(&args)?;
    if given.prints.is_empty() || (given.from_stdin && !stdin.trim_ascii().is_empty()) {
        return None;
    }
    let tuple = given.target.unwrap_or(&toolchain.host);
    let target = release.targets.get(tuple)?;
    let mut out = String::new();
    for print in &given.prints {
        match *print {
            "sysroot" => out += &format!("{}\n", toolchain.sysroot.to_str()?),
            "target-libdir" => {
                let dir = toolchain.sysroot.join("lib/rustlib").join(tuple).join("lib");
                out += &format!("{}\n", dir.to_str()?);
            }
            "host-tuple" => out += &format!("{}\n", toolchain.host),
            "split-debuginfo" => {
                target.split_debuginfo.iter().for_each(|it| out += &format!("{it}\n"))
            }
            "crate-name" => out += &format!("{}\n", given.crate_name.unwrap_or("rust_out")),
            "file-names" => {
                if given.extra_filename || !given.from_stdin {
                    return None;
                }
                let name = given.crate_name.unwrap_or("rust_out");
                let mut kinds: Vec<&str> = Vec::new();
                for kind in
                    if given.crate_types.is_empty() { &["bin"][..] } else { &given.crate_types }
                {
                    if !kinds.contains(kind) {
                        kinds.push(kind);
                    }
                }
                // `lib` is an `rlib`, which rustc may not name twice.
                if kinds.contains(&"lib") && kinds.contains(&"rlib") {
                    return None;
                }
                for kind in kinds {
                    let file = target.file_names.get(kind)?.as_deref()?;
                    out += &format!("{}\n", file.replace("___", name));
                }
            }
            "cfg" => {
                let mut cfgs: Vec<String> = target.cfg.clone();
                if nightly {
                    cfgs.extend(target.nightly_cfg.iter().cloned());
                }
                let debug_assertions = given.debug_assertions();
                if debug_assertions {
                    cfgs.push("debug_assertions".to_owned());
                }
                if nightly && given.overflow_checks.unwrap_or(debug_assertions) {
                    cfgs.push("overflow_checks".to_owned());
                }
                if nightly && debug_assertions {
                    cfgs.push("ub_checks".to_owned());
                }
                let panic = given.panic.unwrap_or(&target.panic);
                if panic != target.panic && panic != "abort" {
                    return None;
                }
                cfgs.push(format!("panic=\"{panic}\""));
                if given.crate_types.contains(&"proc-macro") {
                    cfgs.push("proc_macro".to_owned());
                }
                for cfg in &given.cfgs {
                    if !is_plain_cfg(cfg) {
                        return None;
                    }
                    cfgs.push((*cfg).to_owned());
                }
                cfgs.sort();
                cfgs.dedup();
                cfgs.iter().for_each(|it| out += &format!("{it}\n"));
            }
            "target-spec-json" if given.unstable_options && nightly => {
                out += target.target_spec_json.get(&toolchain.host)?;
            }
            _ => return None,
        }
    }
    Some(out.into_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cfgs_given() {
        assert!(is_plain_cfg("foo"));
        assert!(is_plain_cfg("feature=\"serde derive\""));
        assert!(!is_plain_cfg("feature=serde"));
        assert!(!is_plain_cfg("feature=\"a\\\"b\""));
        assert!(!is_plain_cfg("debug_assertions"));
        assert!(!is_plain_cfg("target_os=\"linux\""));
        assert!(!is_plain_cfg("sanitizer_cfi_generalize_pointers"));
        assert!(!is_plain_cfg("9lives"));
    }

    #[test]
    fn arguments_read() {
        let args =
            ["-Copt-level=3", "-C", "panic=abort", "--print=cfg", "--cfg", "foo", "-Wwarnings"];
        let given = Given::of(&args).unwrap();
        assert_eq!(given.opt_level, Some("3"));
        assert_eq!(given.panic, Some("abort"));
        assert_eq!(given.prints, ["cfg"]);
        assert_eq!(given.cfgs, ["foo"]);
        assert!(!given.debug_assertions());
        assert!(Given::of(&["-O"]).is_some_and(|it| !it.debug_assertions()));
        assert!(Given::of(&["-C", "opt-level=0"]).is_some_and(|it| it.debug_assertions()));
        // What may change the target's features is rustc's to answer.
        assert!(Given::of(&["-C", "target-cpu=native", "--print", "cfg"]).is_none());
        assert!(Given::of(&["-Ctarget-feature=+avx2", "--print", "cfg"]).is_none());
        assert!(Given::of(&["-Z", "ub-checks", "--print", "cfg"]).is_none());
        assert!(Given::of(&["-Zthreads=8", "--print", "cfg"]).is_some());
    }
}
