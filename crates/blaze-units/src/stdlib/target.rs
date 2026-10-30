//! What the pinned nightly's rustc says of each target blaze builds for, without running it
//! (docs/no-rustc.md, step F): the cfgs `rustc --print cfg` prints, with those its arguments
//! decide, the default panic strategy, whether the target has std, and the names of its files.
//! The data is what that rustc printed (`rustc.json`, written by `harness/answers.py --write`
//! when the pin moves); the cfgs that follow from the arguments are added here as rustc adds
//! them (`rustc_session::config::cfg::default_configuration`, `rustc_codegen_ssa`'s
//! `target_features`). A triple the data does not have is an error naming it, never a guess.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::OnceLock,
};

use blaze_resolve::Cfg;
use serde::Deserialize;

const DATA: &str = include_str!("rustc.json");

#[derive(Deserialize)]
struct Release {
    /// As the first line of `rustc -V` has it, after `rustc `.
    version: String,
    /// What `rustc -vV` prints, with `{host}` for the host's triple.
    verbose: String,
    targets: BTreeMap<String, Target>,
}

#[derive(Deserialize)]
struct Target {
    /// The cfgs of the target, but those the arguments decide.
    cfg: Vec<String>,
    /// The cfgs printed besides those when unstable features are allowed, as on blaze's
    /// nightly they always are.
    nightly_cfg: Vec<String>,
    /// The strategy of `-C panic` when none is given.
    panic: String,
    split_debuginfo: Vec<String>,
    /// The name of the file of each kind of crate named `___`; `None` for a kind the target
    /// does not have.
    file_names: BTreeMap<String, Option<String>>,
    /// By the host rustc ran on.
    target_spec_json: BTreeMap<String, String>,
    /// The `target_feature`s each feature rustc has turns on beside the target's own, with
    /// itself.
    #[serde(default)]
    features: BTreeMap<String, Vec<String>>,
    /// The same of the features only LLVM names, which `target-cpu=native` turns on.
    #[serde(default)]
    llvm_features: BTreeMap<String, Vec<String>>,
    /// Of each feature the target has, the features turning it off turns off: itself and
    /// those that need it.
    #[serde(default)]
    features_off: BTreeMap<String, Vec<String>>,
    /// Each CPU of `-C target-cpu`, by the index of its `target_feature`s in `cpu_features`.
    #[serde(default)]
    cpus: BTreeMap<String, usize>,
    #[serde(default)]
    cpu_features: Vec<Vec<String>>,
    /// The cfgs besides `target_feature` a feature has: wasm's `atomics`, `target_has_threads`.
    #[serde(default)]
    feature_cfgs: BTreeMap<String, Vec<String>>,
    /// The other features rustc or LLVM lists of the target (`--print target-features`): one
    /// of none of these turns nothing on or off, and rustc warns of it.
    #[serde(default)]
    other_features: Vec<String>,
}

#[derive(Deserialize)]
struct Spec {
    #[serde(default, rename = "crt-static-respected")]
    crt_static_respected: bool,
    #[serde(default)]
    metadata: SpecMetadata,
}

#[derive(Deserialize, Default)]
struct SpecMetadata {
    #[serde(default)]
    std: Option<bool>,
}

fn release() -> &'static Release {
    static READ: OnceLock<Release> = OnceLock::new();
    READ.get_or_init(|| serde_json::from_str(DATA).expect("stdlib/rustc.json is what harness/answers.py writes"))
}

fn target(triple: &str) -> Result<&'static Target, String> {
    release().targets.get(triple).ok_or_else(|| {
        let known: Vec<&str> = release().targets.keys().map(String::as_str).collect();
        format!(
            "blaze has no data of target `{triple}` (it knows {}): harness/answers.py records each \
             target blaze builds for, and blaze runs no rustc to ask (docs/no-rustc.md)",
            known.join(", ")
        )
    })
}

impl Target {
    /// The spec every recorded host prints, for what the hosts agree on.
    fn spec(&self, triple: &str) -> Result<Spec, String> {
        let text = self
            .target_spec_json
            .values()
            .next()
            .ok_or_else(|| format!("blaze recorded no target spec of `{triple}`"))?;
        serde_json::from_str(text).map_err(|error| format!("the recorded target spec of `{triple}`: {error}"))
    }

    /// Whether rustc or LLVM lists `name` as a feature of the target.
    fn lists(&self, name: &str) -> bool {
        self.features.contains_key(name)
            || self.llvm_features.contains_key(name)
            || self.features_off.contains_key(name)
            || self.other_features.iter().any(|it| it == name)
    }

    /// The `target_feature`s with `cpu`'s and then `features` turned on (`true`) and off in
    /// order, starting from `own`, the target's: rustc's `cfg_target_feature`.
    fn tuned(
        &self,
        triple: &str,
        own: Vec<String>,
        cpu: Option<(&str, Option<&str>)>,
        features: &[(bool, String)],
    ) -> Result<Vec<String>, String> {
        let mut have: BTreeSet<String> = match cpu {
            Some((name, _)) => {
                let index = self
                    .cpus
                    .get(name)
                    .ok_or_else(|| format!("`-C target-cpu={name}` is no CPU blaze knows of target `{triple}`"))?;
                self.cpu_features
                    .get(*index)
                    .ok_or_else(|| format!("blaze's data of target `{triple}` lists no features of CPU `{name}`"))?
                    .iter()
                    .cloned()
                    .collect()
            }
            None => own.iter().cloned().collect(),
        };
        // `target-cpu=native`: LLVM's features of the host, which name what rustc does not too.
        let host: Vec<(bool, String)> = cpu
            .and_then(|(_, host)| host)
            .map(|it| {
                it.split(',')
                    .filter_map(|it| Some((it.starts_with('+'), it.get(1..)?.to_owned())))
                    .collect()
            })
            .unwrap_or_default();
        let on = |name: &str| self.features.get(name).or_else(|| self.llvm_features.get(name));
        let mut apply = |enable: bool, name: &str, from_host: bool| -> Result<(), String> {
            match (enable, on(name)) {
                (true, Some(turned)) => {
                    have.extend(turned.iter().cloned());
                    // The target's own features it needs, should they be off.
                    have.extend(
                        self.features_off
                            .iter()
                            .filter(|(_, off)| off.iter().any(|it| it == name))
                            .map(|(it, _)| it.clone()),
                    );
                }
                (false, _) if self.features_off.contains_key(name) => {
                    for it in &self.features_off[name] {
                        have.remove(it);
                    }
                }
                (false, Some(turned)) if !own.iter().any(|it| it == name) => {
                    // Off with it go the features that turn it on.
                    let needs = |other: &Vec<String>| turned.iter().all(|it| other.contains(it));
                    if !turned.is_empty() {
                        have.retain(|it| !self.features.get(it).is_some_and(needs));
                    }
                }
                // A feature only LLVM has, of no `target_feature`, changes none of them.
                _ if from_host => {}
                _ => {
                    let sign = if enable { '+' } else { '-' };
                    return Err(format!(
                        "blaze's data of target `{triple}` does not say what `-C target-feature={sign}{name}` \
                         turns {}",
                        if enable { "on" } else { "off" }
                    ));
                }
            }
            Ok(())
        };
        for (enable, name) in &host {
            apply(*enable, name, true)?;
        }
        for (enable, name) in features {
            if !self.lists(name) {
                if self.other_features.is_empty() {
                    return Err(format!(
                        "blaze's data of target `{triple}` does not list its features, so not whether \
                         `{name}` is one"
                    ));
                }
                // rustc warns of a feature it does not know and passes it to LLVM: no cfg.
                continue;
            }
            apply(*enable, name, false)?;
        }
        Ok(have.into_iter().collect())
    }
}

/// The host's CPU as LLVM names it, for `-C target-cpu=native`: what the daemon reads of the
/// machine it runs on (`hir_ty_codegen::host_cpu`), never a query.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct HostCpu {
    /// The architecture of the host's triple (what is before its first `-`).
    pub arch: String,
    pub name: String,
    /// LLVM's features of it, `+a,-b,...`.
    pub features: String,
}

/// The arguments of a rustc that decide cfgs: the flags of `-C`, `--cfg` and the crate types,
/// as [`CfgArgs::from_flags`] reads them from a command line (cargo's rustflags).
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct CfgArgs {
    pub opt_level: Option<String>,
    pub debug_assertions: Option<bool>,
    pub overflow_checks: Option<bool>,
    pub panic: Option<String>,
    pub target_cpu: Option<String>,
    /// For `target_cpu` `native`.
    pub host_cpu: Option<HostCpu>,
    /// `-C target-feature`'s, on (`true`) or off, in order.
    pub target_features: Vec<(bool, String)>,
    /// `--cfg`'s, as given (`foo`, `feature="x"`).
    pub cfgs: Vec<String>,
    /// Whether `proc-macro` is among the crate types: cargo's probe of a target
    /// (`TargetInfo::new`) has it, so cargo's cfgs of a target have `proc_macro`.
    pub proc_macro: bool,
}

/// The codegen options that decide no cfg.
const NO_CFG_CODEGEN: [&str; 20] = [
    "embed-bitcode",
    "debuginfo",
    "metadata",
    "extra-filename",
    "incremental",
    "strip",
    "codegen-units",
    "prefer-dynamic",
    "lto",
    "linker",
    "linker-features",
    "link-arg",
    "link-args",
    "split-debuginfo",
    "link-dead-code",
    "rpath",
    "force-frame-pointers",
    "force-unwind-tables",
    "symbol-mangling-version",
    "save-temps",
];

/// The flags that take a value and decide no cfg.
const NO_CFG_VALUED: [&str; 22] = [
    "--edition",
    "--error-format",
    "--json",
    "--emit",
    "--out-dir",
    "-o",
    "-L",
    "-l",
    "--extern",
    "--check-cfg",
    "--cap-lints",
    "--color",
    "--diagnostic-width",
    "--remap-path-prefix",
    "--crate-name",
    "-W",
    "-A",
    "-D",
    "-F",
    "--warn",
    "--allow",
    "--deny",
];

const NO_CFG_LINTS: [&str; 2] = ["--forbid", "--force-warn"];

fn yes(flag: &str, value: Option<&str>) -> Result<bool, String> {
    match value {
        None | Some("y" | "yes" | "on" | "true") => Ok(true),
        Some("n" | "no" | "off" | "false") => Ok(false),
        Some(other) => Err(format!("`-C {flag}={other}`: rustc takes a boolean (`y`, `yes`, `on`, `true`, `n`, `no`, `off`, `false`)")),
    }
}

impl CfgArgs {
    /// The arguments of `flags`, a rustc command line (cargo's rustflags for a target). A flag
    /// that may decide a cfg and is not read here is an error naming it.
    pub fn from_flags(flags: &[String]) -> Result<CfgArgs, String> {
        let mut args = CfgArgs::default();
        let mut words = flags.iter().map(String::as_str);
        while let Some(word) = words.next() {
            let (flag, joined) = match word.split_once('=') {
                Some((flag, value)) if flag.starts_with("--") => (flag, Some(value)),
                _ => match word.as_bytes() {
                    [b'-', b'C' | b'Z' | b'W' | b'A' | b'D' | b'F' | b'L' | b'l', _, ..] => (&word[..2], Some(&word[2..])),
                    _ => (word, None),
                },
            };
            let mut value = || {
                joined
                    .or_else(|| words.next())
                    .ok_or_else(|| format!("rustc's `{flag}` takes a value, and the flags end after it"))
            };
            match flag {
                "--cfg" => args.cfgs.push(value()?.to_owned()),
                "--crate-type" => args.proc_macro |= value()?.split(',').any(|it| it == "proc-macro"),
                "-O" => args.opt_level = Some("2".to_owned()),
                "-g" => {}
                "-C" | "--codegen" => {
                    let option = value()?;
                    let (name, value) = match option.split_once('=') {
                        Some((name, value)) => (name, Some(value)),
                        None => (option, None),
                    };
                    let name = name.replace('_', "-");
                    let needed = |value: Option<&str>| {
                        value.map(str::to_owned).ok_or_else(|| format!("rustc's `-C {name}` takes a value"))
                    };
                    match name.as_str() {
                        "opt-level" => args.opt_level = Some(needed(value)?),
                        "debug-assertions" => args.debug_assertions = Some(yes(&name, value)?),
                        "overflow-checks" => args.overflow_checks = Some(yes(&name, value)?),
                        "panic" => args.panic = Some(needed(value)?),
                        "target-cpu" => args.target_cpu = Some(needed(value)?),
                        "target-feature" => {
                            for feature in needed(value)?.split(',').filter(|it| !it.is_empty()) {
                                let enable = match feature.as_bytes()[0] {
                                    b'+' => true,
                                    b'-' => false,
                                    _ => {
                                        return Err(format!(
                                            "`-C target-feature={feature}`: each feature starts with `+` or `-`"
                                        ));
                                    }
                                };
                                args.target_features.push((enable, feature[1..].to_owned()));
                            }
                        }
                        other if NO_CFG_CODEGEN.contains(&other) => {}
                        other => return Err(format!("blaze does not know which cfgs `-C {other}` decides")),
                    }
                }
                "-Z" => {
                    let option = value()?;
                    let name = option.split_once('=').map_or(option, |(name, _)| name);
                    match name {
                        "unstable-options" | "threads" | "macro-backtrace" | "time-passes" | "share-generics"
                        | "external-clangrt" | "linker-features" => {}
                        other => return Err(format!("blaze does not know which cfgs `-Z {other}` decides")),
                    }
                }
                _ if NO_CFG_VALUED.contains(&flag) || NO_CFG_LINTS.contains(&flag) => {
                    value()?;
                }
                _ if !flag.starts_with('-') => {}
                other => return Err(format!("blaze does not know which cfgs rustc's `{other}` decides")),
            }
        }
        Ok(args)
    }

    fn debug_assertions(&self) -> bool {
        self.debug_assertions.unwrap_or(matches!(self.opt_level.as_deref(), None | Some("0")))
    }
}

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

/// Whether `cfg` is `name` or `name="value"` as rustc prints it back.
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
        inner.is_some_and(|it| it.chars().all(|c| c.is_ascii_graphic() && c != '"' && c != '\\' || c == ' '))
    });
    plain_name && !builtin && plain_value
}

fn arch(triple: &str) -> &str {
    triple.split('-').next().unwrap_or(triple)
}

/// The cfgs `rustc --print cfg --target <triple> <args>` prints, one per line as rustc prints
/// them, sorted.
pub fn target_cfg(triple: &str, args: &CfgArgs) -> Result<Vec<String>, String> {
    let target = target(triple)?;
    let mut cfgs: Vec<String> = target.cfg.iter().chain(&target.nightly_cfg).cloned().collect();
    let static_default = cfgs.iter().any(|it| it == "target_feature=\"crt-static\"");
    let as_it_is = args
        .target_features
        .iter()
        .all(|(enable, name)| name == "crt-static" && *enable == static_default);
    if args.target_cpu.is_some() || !as_it_is {
        let cpu = match args.target_cpu.as_deref() {
            Some("native") => match &args.host_cpu {
                Some(host) if host.arch == arch(triple) => Some((host.name.as_str(), Some(host.features.as_str()))),
                Some(host) => {
                    return Err(format!(
                        "`-C target-cpu=native` for target `{triple}` on a {} host: rustc's native CPU \
                         is the host's, of another architecture",
                        host.arch
                    ));
                }
                None => {
                    return Err(format!(
                        "`-C target-cpu=native` for target `{triple}`: the host's CPU was not given"
                    ));
                }
            },
            Some(name) => Some((name, None)),
            None => None,
        };
        let feature = |it: &String| Some(it.strip_prefix("target_feature=\"")?.strip_suffix('"')?.to_owned());
        let own: Vec<String> = cfgs.iter().filter_map(feature).collect();
        let tuned = target.tuned(triple, own, cpu, &args.target_features)?;
        cfgs.retain(|it| feature(it).is_none());
        for name in &tuned {
            cfgs.extend(target.feature_cfgs.get(name).into_iter().flatten().cloned());
        }
        cfgs.extend(tuned.iter().map(|it| format!("target_feature=\"{it}\"")));
    }
    let debug_assertions = args.debug_assertions();
    if debug_assertions {
        cfgs.push("debug_assertions".to_owned());
        cfgs.push("ub_checks".to_owned());
    }
    if args.overflow_checks.unwrap_or(debug_assertions) {
        cfgs.push("overflow_checks".to_owned());
    }
    let panic = args.panic.as_deref().unwrap_or(&target.panic);
    if panic != target.panic && panic != "abort" {
        return Err(format!(
            "`-C panic={panic}` for target `{triple}`, whose strategy is `{}`: blaze knows only `abort` \
             besides a target's own",
            target.panic
        ));
    }
    cfgs.push(format!("panic=\"{panic}\""));
    if args.proc_macro {
        cfgs.push("proc_macro".to_owned());
        // A proc macro is a dynamic library, which a target that respects `crt-static` builds
        // without it unless it is asked for (rustc's `Session::crt_static`).
        if !args.target_features.iter().any(|(_, name)| name == "crt-static")
            && target.spec(triple)?.crt_static_respected
        {
            cfgs.retain(|it| it != "target_feature=\"crt-static\"");
        }
    }
    for cfg in &args.cfgs {
        if !is_plain_cfg(cfg) {
            return Err(format!(
                "`--cfg {cfg}`: blaze passes on only a plain name or `name=\"value\"` of no cfg rustc sets"
            ));
        }
        cfgs.push(cfg.clone());
    }
    cfgs.sort();
    cfgs.dedup();
    Ok(cfgs)
}

/// [`target_cfg`] as cfgs.
pub fn target_cfgs(triple: &str, args: &CfgArgs) -> Result<Vec<Cfg>, String> {
    target_cfg(triple, args)?
        .iter()
        .map(|it| it.parse::<Cfg>().map_err(|error| format!("cfg `{it}` of target `{triple}`: {error}")))
        .collect()
}

/// The `CARGO_CFG_*` variables cargo sets for a build script's run from `cfgs`, its target's
/// [`target_cfg`] (cargo's `custom_build.rs`, `build_work`): one per name, the values of a
/// name joined by `,`, `debug_assertions` left out (cargo asks rustc with no profile, so it is
/// always there and says nothing). Sorted by name.
pub fn cargo_cfg_env(cfgs: &[String]) -> Vec<(String, String)> {
    let mut by_name: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for cfg in cfgs {
        match cfg.split_once('=') {
            Some((name, value)) => {
                let value = value.strip_prefix('"').and_then(|it| it.strip_suffix('"')).unwrap_or(value);
                by_name.entry(name.to_owned()).or_default().push(value.to_owned());
            }
            None => {
                by_name.entry(cfg.clone()).or_default();
            }
        }
    }
    by_name
        .into_iter()
        .filter(|(name, _)| name != "debug_assertions")
        .map(|(name, values)| (format!("CARGO_CFG_{}", name.to_uppercase().replace('-', "_")), values.join(",")))
        .collect()
}

/// What `-Zbuild-std` and a build need of a target beyond its cfgs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TargetFacts {
    /// The strategy of `-C panic` when none is given: `unwind` or `abort`.
    pub panic: String,
    /// Whether the target has std (its spec's `metadata.std`); without, a build asks `core`
    /// and `alloc` alone.
    pub has_std: bool,
    /// Whether the target has no libc's `memcpy` and the rest, so `-Zbuild-std` users turn on
    /// `compiler-builtins-mem` (a target without std). compiler_builtins' own build script turns
    /// its `mem` on for wasm without WASI, SGX, `-none`, nvptx, UEFI and xous whatever the
    /// features say.
    pub needs_builtins_mem: bool,
    /// `--print split-debuginfo`'s, the first the default.
    pub split_debuginfo: Vec<String>,
}

pub fn target_facts(triple: &str) -> Result<TargetFacts, String> {
    let target = target(triple)?;
    let spec = target.spec(triple)?;
    let has_std = spec
        .metadata
        .std
        .ok_or_else(|| format!("the recorded target spec of `{triple}` does not say whether it has std"))?;
    Ok(TargetFacts {
        panic: target.panic.clone(),
        has_std,
        needs_builtins_mem: !has_std,
        split_debuginfo: target.split_debuginfo.clone(),
    })
}

/// The name of the file rustc writes for a crate `name` of `crate_type` (`bin`, `lib`, `rlib`,
/// `dylib`, `cdylib`, `staticlib`, `proc-macro`) for `triple`, as `--print file-names` prints
/// it; `None` for a crate type the target does not have.
pub fn file_name(triple: &str, crate_type: &str, name: &str) -> Result<Option<String>, String> {
    let target = target(triple)?;
    let file = target
        .file_names
        .get(crate_type)
        .ok_or_else(|| format!("blaze does not know rustc's file of a `{crate_type}` crate for target `{triple}`"))?;
    Ok(file.as_ref().map(|it| it.replace("___", name)))
}

/// Every triple blaze has data of, sorted.
pub fn triples() -> impl Iterator<Item = &'static str> {
    release().targets.keys().map(String::as_str)
}

/// The version of blaze's rustc as `rustc -V` prints it after `rustc `
/// (`1.101.0-nightly (16d1ffac1 2026-10-26)`): cargo's `-C metadata` hashes it.
pub fn rustc_version() -> &'static str {
    &release().version
}

/// What `rustc -vV` prints, on `host`.
pub fn rustc_verbose_version(host: &str) -> String {
    release().verbose.replace("{host}", host)
}

/// The commit of rust-lang/rust blaze's rustc is (`commit-hash` of `rustc -vV`): the one
/// `library/UPSTREAM` names.
pub fn rustc_commit() -> &'static str {
    release()
        .verbose
        .lines()
        .find_map(|line| line.strip_prefix("commit-hash: "))
        .expect("`rustc -vV` of blaze's release names its commit")
}
