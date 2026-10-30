//! The flags cargo gives rustc: a platform's rustflags (`target_info.rs`'s `TargetInfo::new`
//! and `extra_args`), the `--cfg`s (`features_args`), the `--check-cfg`s (`check_cfg_args`) and
//! `[lints]` (`parser/mod.rs`'s `lints_to_rustflags`).

use std::cmp::Reverse;

use blaze_manifest::Config;
use blaze_manifest::config::{CompileKind, Flags};
use blaze_manifest::manifest::Manifest;
use blaze_resolve::Cfg;
use cargo_util_schemas::manifest::TomlLintLevel;

use crate::load::load_error;
use crate::query::LoadError;
use crate::stdlib::target::{self as platform, CfgArgs};

fn error(what: String) -> LoadError {
    LoadError { what, at: None }
}

/// What cargo's `TargetInfo` has of one kind of unit.
pub(super) struct KindInfo {
    /// The triple, the host's for the host kind.
    pub triple: String,
    /// cargo's `TargetInfo::rustflags`: every unit of the kind has them (`Unit::rustflags`).
    pub rustflags: Vec<String>,
    /// `rustc --print cfg` of the kind with them, as lines.
    pub cfg_lines: Vec<String>,
    pub cfgs: Vec<Cfg>,
}

/// cargo's `TargetInfo::new` for `kind` (`None` the host) in a build for `target`: the
/// rustflags of `extra_args`, then once more with the cfg they give for `[target.'cfg(..)']`,
/// up to the second turn as cargo stops.
pub(super) fn kind_info(config: &Config, host: &str, target: Option<&str>, kind: Option<&str>) -> Result<KindInfo, LoadError> {
    let requested = [match target {
        Some(triple) => CompileKind::Target(triple.to_owned()),
        None => CompileKind::Host,
    }];
    let compile_kind = match kind {
        Some(triple) => CompileKind::Target(triple.to_owned()),
        None => CompileKind::Host,
    };
    let triple = kind.unwrap_or(host);
    let extra = |cfg: Option<&[Cfg]>| {
        config.extra_args(&requested, host, cfg, &compile_kind, Flags::Rust).map_err(|e| load_error(&e, None))
    };
    let mut rustflags = extra(None)?;
    let mut turn = 0;
    loop {
        let args = CfgArgs::from_flags(&rustflags).map_err(error)?;
        let cfg_lines = platform::target_cfg(triple, &CfgArgs { proc_macro: true, ..args }).map_err(error)?;
        let cfgs = cfg_lines
            .iter()
            .map(|line| line.parse::<Cfg>().map_err(|e| error(format!("the cfg `{line}` of `{triple}`: {e}"))))
            .collect::<Result<Vec<_>, _>>()?;
        let next = extra(Some(&cfgs))?;
        if next != rustflags && turn == 0 {
            turn += 1;
            rustflags = next;
            continue;
        }
        return Ok(KindInfo { triple: triple.to_owned(), rustflags, cfg_lines, cfgs });
    }
}

/// Each `--cfg` of `flags`, as `--cfg x` or `--cfg=x`.
fn push_cfgs(flags: &[String], into: &mut Vec<String>) {
    let mut at = 0;
    while at < flags.len() {
        let flag = &flags[at];
        if flag == "--cfg" {
            if let Some(value) = flags.get(at + 1) {
                into.push(value.clone());
            }
            at += 2;
            continue;
        }
        if let Some(value) = flag.strip_prefix("--cfg=") {
            into.push(value.to_owned());
        }
        at += 1;
    }
}

/// The `--cfg`s of a compile in cargo's order: the profile's rustflags' (`build_base_args`),
/// the features (`features_args`), the kind's rustflags' (`prepare_rustc`).
pub(super) fn cfgs(profile_rustflags: &[String], features: &[String], rustflags: &[String]) -> Vec<String> {
    let mut out = Vec::with_capacity(features.len());
    push_cfgs(profile_rustflags, &mut out);
    out.extend(features.iter().map(|feature| format!("feature=\"{feature}\"")));
    push_cfgs(rustflags, &mut out);
    out
}

/// cargo's `check_cfg_args`: what follows each `--check-cfg`, the package's features (its
/// summary's, the implicit ones of optional dependencies too) sorted by name.
pub(super) fn check_cfg(manifest: &Manifest) -> Vec<String> {
    let mut feature = String::with_capacity(manifest.features.len() * 7 + 25);
    feature.push_str("cfg(feature, values(");
    for (at, name) in manifest.features.keys().enumerate() {
        if at != 0 {
            feature.push_str(", ");
        }
        feature.push('"');
        feature.push_str(name);
        feature.push('"');
    }
    feature.push_str("))");
    vec!["cfg(docsrs,test)".to_owned(), feature]
}

/// cargo's `lints_to_rustflags` of the package's `[lints]` (inherited already): the level
/// flags sorted by priority, then by name backwards, then by flag; and what follows each
/// `--check-cfg` of `[lints.rust.unexpected_cfgs] check-cfg`.
pub(super) fn lints(manifest: &Manifest) -> Result<(Vec<String>, Vec<String>), LoadError> {
    let name = &manifest.package_id.name;
    let lints = manifest
        .normalized_toml
        .normalized_lints()
        .map_err(|_| error(format!("the `[lints]` of `{name}` were not inherited from the workspace")))?;
    let Some(lints) = lints else { return Ok((Vec::new(), Vec::new())) };
    let mut flags: Vec<(i8, Reverse<&str>, String)> = Vec::new();
    for (tool, tool_lints) in lints {
        if tool == "cargo" {
            continue;
        }
        for (lint, config) in tool_lints {
            let flag = match config.level() {
                TomlLintLevel::Forbid => "--forbid",
                TomlLintLevel::Deny => "--deny",
                TomlLintLevel::Warn => "--warn",
                TomlLintLevel::Allow => "--allow",
            };
            let option = if tool == "rust" { format!("{flag}={lint}") } else { format!("{flag}={tool}::{lint}") };
            flags.push((config.priority(), Reverse(lint.as_str()), option));
        }
    }
    flags.sort();
    let flags = flags.into_iter().map(|(_, _, option)| option).collect();
    let mut check_cfg = Vec::new();
    let custom = lints
        .get("rust")
        .and_then(|rust| rust.get("unexpected_cfgs"))
        .and_then(|lint| lint.config())
        .and_then(|config| config.get("check-cfg"));
    if let Some(value) = custom {
        let list = toml::Value::try_into::<Vec<String>>(value.clone())
            .map_err(|_| error("`lints.rust.unexpected_cfgs.check-cfg` must be a list of string".to_owned()))?;
        check_cfg.extend(list);
    }
    Ok((flags, check_cfg))
}
