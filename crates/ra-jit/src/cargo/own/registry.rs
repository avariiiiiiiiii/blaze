//! `cargo search`, `owner`, `yank`, `login` and `logout`: a registry's web API asked with
//! blaze-registry's client, and its token kept as cargo's `cargo:token` provider keeps it, as
//! cargo 1.99.0's `commands/{search,owner,yank,login,logout}.rs`, `ops/registry` and
//! `util/auth` do. A credential provider other than `cargo:token` is refused: blaze runs none.

use std::fmt;
use std::io::IsTerminal as _;

use anyhow::{Context as _, anyhow, bail};
use blaze_manifest::config::{OptValue, PathAndArgs, Value};
use blaze_manifest::metadata::root_manifest;
use blaze_manifest::{Config, Context, Workspace};
use blaze_registry::SourceId;
use blaze_registry::api::{Api, check_token};

use super::super::say;
use super::args::{Args, Read, switch, valued};
use super::manifest::{USAGE, config, failed};

const COMMON: &str =
    "  -v, --verbose...               Use verbose output (-vv very verbose/build.rs output)
  -q, --quiet                    Do not print cargo log messages
      --color <WHEN>             Coloring [possible values: auto, always, never]
      --config <KEY=VALUE|PATH>  Override a configuration value
  -Z <FLAG>                      Unstable (nightly-only) flags to Cargo, see 'cargo -Z help' for
                                 details
  -h, --help                     Print help

Manifest Options:
      --locked   Assert that `Cargo.lock` will remain unchanged
      --offline  Run without accessing the network
      --frozen   Equivalent to specifying both --locked and --offline";

/// One of the commands: its name, usage line, help above [`COMMON`], and flags.
struct Command {
    name: &'static str,
    usage: &'static str,
    head: &'static str,
    /// Whether it takes words that are no flags, for clap's tip on an unknown flag.
    positional: bool,
}

const SEARCH: Command = Command {
    name: "search",
    usage: "cargo search [OPTIONS] [QUERY]...",
    head: "Search packages in the registry. Default registry is crates.io

Usage: cargo search [OPTIONS] [QUERY]...

Arguments:
  [QUERY]...  

Options:
      --limit <LIMIT>            Limit the number of results (default: 10, max: 100)
      --index <INDEX>            Registry index URL to search packages in
      --registry <REGISTRY>      Registry to search packages in
",
    positional: true,
};

const OWNER: Command = Command {
    name: "owner",
    usage: "cargo owner [OPTIONS] [CRATE]",
    head: "Manage the owners of a crate on the registry

Usage: cargo owner [OPTIONS] [CRATE]

Arguments:
  [CRATE]  

Options:
  -a, --add <LOGIN>              Name of a user or team to invite as an owner
  -r, --remove <LOGIN>           Name of a user or team to remove as an owner
  -l, --list                     List owners of a crate
      --index <INDEX>            Registry index URL to modify owners for
      --registry <REGISTRY>      Registry to modify owners for
      --token <TOKEN>            API token to use when authenticating
",
    positional: true,
};

const YANK: Command = Command {
    name: "yank",
    usage: "cargo yank [OPTIONS] [CRATE]",
    head: "Remove a pushed crate from the index

Usage: cargo yank [OPTIONS] [CRATE]

Arguments:
  [CRATE]  

Options:
      --version <VERSION>        The version to yank or un-yank
      --undo                     Undo a yank, putting a version back into the index
      --index <INDEX>            Registry index URL to yank from
      --registry <REGISTRY>      Registry to yank from
      --token <TOKEN>            API token to use when authenticating
",
    positional: true,
};

const LOGIN: Command = Command {
    name: "login",
    usage: "cargo login [OPTIONS] [-- [args]...]",
    head: "Log in to a registry.

Usage: cargo login [OPTIONS] [-- [args]...]

Arguments:
  [args]...  Additional arguments for the credential provider

Options:
      --registry <REGISTRY>      Registry to use
",
    positional: true,
};

const LOGOUT: Command = Command {
    name: "logout",
    usage: "cargo logout [OPTIONS]",
    head: "Remove an API token from the registry locally

Usage: cargo logout [OPTIONS]

Options:
      --registry <REGISTRY>      Registry to use
",
    positional: false,
};

impl Command {
    /// The flags, or the status clap ends with: help printed, or the command line refused.
    fn read(&self, typed: &[String], flags: &[super::args::Flag]) -> Result<Read, i32> {
        let read = match Args::new(typed).read(flags) {
            Ok(read) => read,
            Err(why) => {
                let unknown =
                    typed.iter().find(|it| why == format!("unexpected argument '{it}' found"));
                let Some(word) = unknown.filter(|it| it.starts_with("--")) else {
                    return Err(self.refuse(&why, self.usage));
                };
                let flag = word.split_once('=').map_or(word.as_str(), |it| it.0);
                if let Some(similar) = similar(&flag[2..], flags) {
                    let positional = self.positional_usage(&Read::default());
                    let usage = format!("cargo {} {similar}{positional}", self.name);
                    let similar = similar.split(' ').next().unwrap_or_default();
                    let why = format!("{why}\n\n  tip: a similar argument exists: '{similar}'");
                    return Err(self.refuse(&why, &usage));
                }
                let tip = match self.positional {
                    true => format!("\n\n  tip: to pass '{flag}' as a value, use '-- {flag}'"),
                    false => String::new(),
                };
                return Err(self.refuse(&format!("{why}{tip}"), self.usage));
            }
        };
        if read.has("--help") {
            println!(
                "{}{COMMON}\n\nRun `cargo help {}` for more detailed information.",
                self.head, self.name
            );
            return Err(0);
        }
        if read.has("--registry") && read.has("--index") {
            let positional = self.positional_usage(&read);
            let usage = format!("cargo {} --registry <REGISTRY>{positional}", self.name);
            return Err(self.refuse(
                "the argument '--registry <REGISTRY>' cannot be used with '--index <INDEX>'",
                &usage,
            ));
        }
        Ok(read)
    }

    /// The words that are no flags as clap's usage of the arguments given shows them.
    fn positional_usage(&self, read: &Read) -> &'static str {
        match self.name {
            "search" => " [QUERY]...",
            _ if read.positional.is_empty() => " [CRATE]",
            _ => " <CRATE>",
        }
    }

    fn refuse(&self, why: &str, usage: &str) -> i32 {
        say::error(why);
        eprintln!("\nUsage: {usage}\n\nFor more information, try '--help'.");
        USAGE
    }
}

/// The long flags every command takes, with whether each takes a value, as clap knows them.
const GLOBAL_LONGS: &[(&str, bool)] = &[
    ("verbose", false),
    ("quiet", false),
    ("color", true),
    ("config", true),
    ("frozen", false),
    ("locked", false),
    ("offline", false),
    ("help", false),
];

/// clap's `did_you_mean_flag`: the long flag most like `typed` by Jaro similarity, if any is
/// more than 0.7 alike, as clap's usage shows it (`--limit <LIMIT>`).
fn similar(typed: &str, flags: &[super::args::Flag]) -> Option<String> {
    let own = flags.iter().filter_map(|it| {
        let long = it.names.iter().find(|it| it.starts_with("--"))?;
        Some((&long[2..], it.valued))
    });
    let mut found: Vec<(f64, &str, bool)> = own
        .chain(GLOBAL_LONGS.iter().copied())
        .map(|(long, valued)| (jaro(typed, long), long, valued))
        .filter(|(confidence, ..)| *confidence > 0.7)
        .collect();
    found.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    let (_, long, valued) = found.pop()?;
    Some(match valued {
        true => format!("--{long} <{}>", long.to_uppercase()),
        false => format!("--{long}"),
    })
}

/// strsim's Jaro similarity of two strings, by characters.
fn jaro(a: &str, b: &str) -> f64 {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let window = (a.len().max(b.len()) / 2).saturating_sub(1);
    let mut taken = vec![false; b.len()];
    let mut matched_a = Vec::new();
    for (i, ca) in a.iter().enumerate() {
        let lo = i.saturating_sub(window);
        let hi = (i + window + 1).min(b.len());
        if let Some(j) = (lo..hi).find(|&j| !taken[j] && b[j] == *ca) {
            taken[j] = true;
            matched_a.push(*ca);
        }
    }
    let m = matched_a.len();
    if m == 0 {
        return 0.0;
    }
    let matched_b = b.iter().zip(&taken).filter(|(_, t)| **t).map(|(c, _)| *c);
    let transpositions = matched_a.iter().zip(matched_b).filter(|(x, y)| **x != *y).count() / 2;
    let m = m as f64;
    (m / a.len() as f64 + m / b.len() as f64 + (m - transpositions as f64) / m) / 3.0
}

/// `--registry` or `--index`, or `registry.default`.
enum RegistryOrIndex {
    Registry(String),
    Index(String),
}

/// `--registry`, `--index`, or the `registry.default` configured, as cargo's
/// `registry_or_index`.
fn registry_or_index(read: &Read, config: &Config) -> anyhow::Result<Option<RegistryOrIndex>> {
    Ok(match (read.value("--registry"), read.value("--index")) {
        (None, None) => config.default_registry()?.map(RegistryOrIndex::Registry),
        (None, Some(index)) => {
            url::Url::parse(&index).map_err(|e| anyhow!("invalid url `{index}`: {e}"))?;
            Some(RegistryOrIndex::Index(index))
        }
        (Some(registry), _) => {
            cargo_util_schemas::manifest::RegistryName::new(&registry)?;
            Some(RegistryOrIndex::Registry(registry))
        }
    })
}

/// The registry a command asks, as cargo's `RegistrySourceIds`.
struct Target {
    /// What its token is kept under: crates.io's git index for crates.io.
    original: SourceId,
    /// What is asked: crates.io's sparse index for crates.io, else `original`.
    replacement: SourceId,
    /// Its name: `crates-io`, or `--registry`'s; none for `--index`.
    name: Option<String>,
}

impl Target {
    /// cargo's `display_registry_name`.
    fn shown(&self) -> String {
        match &self.name {
            Some(name) => name.clone(),
            None => self.original.url().to_string(),
        }
    }

    /// cargo's `SourceId` as it says it: "registry `<name>`".
    fn described(&self) -> String {
        format!("registry `{}`", self.shown())
    }

    /// cargo's `display_index`.
    fn index(&self) -> String {
        match self.original.is_crates_io() {
            true => "crates.io index".to_owned(),
            false => format!("`{}` index", self.shown()),
        }
    }
}

/// cargo's `get_source_id`: the registry named, with only crates.io's own replacement (its
/// sparse index); with none named, a `[source]` replacing crates.io is an error.
fn target(ctx: &Context, asked: Option<&RegistryOrIndex>) -> anyhow::Result<Target> {
    let crates_io =
        || (SourceId::crates_io_git(), Some(blaze_registry::CRATES_IO_REGISTRY.to_owned()));
    let (original, name) = match asked {
        None => crates_io(),
        Some(RegistryOrIndex::Index(url)) => (SourceId::for_registry(url)?, None),
        Some(RegistryOrIndex::Registry(name)) if name == blaze_registry::CRATES_IO_REGISTRY => {
            crates_io()
        }
        Some(RegistryOrIndex::Registry(name)) => {
            let index = ctx.config.registry_index(name)?;
            (SourceId::for_registry(index.as_str())?, Some(name.clone()))
        }
    };
    let builtin =
        blaze_metadata::registry_sources(ctx, false)?.map().replacement(&original, false)?;
    let replaced =
        blaze_metadata::registry_sources(ctx, true)?.map().replacement(&original, false)?;
    if asked.is_none() && replaced != builtin {
        let shown = match replaced_by(&ctx.config)? {
            Some((name, true)) => bail!(
                "crates-io is replaced with remote registry {name};\ninclude `--registry {name}` or `--registry crates-io`"
            ),
            Some((name, false)) if replaced.is_remote_registry() => format!("registry `{name}`"),
            _ => replaced.to_string(),
        };
        bail!(
            "crates-io is replaced with non-remote-registry source {shown};\ninclude `--registry crates-io` to use crates.io"
        );
    }
    Ok(Target { original, replacement: builtin, name })
}

/// The last name of crates.io's chain of `replace-with`, and whether it is a `[registries]`
/// name rather than a `[source]` one: cargo names a replacement by it.
fn replaced_by(config: &Config) -> anyhow::Result<Option<(String, bool)>> {
    let mut name = blaze_registry::CRATES_IO_REGISTRY.to_owned();
    let mut last = None;
    while let Some(with) = config.get_string(&format!("source.{name}.replace-with"))? {
        name = with.val;
        let mut is_source = false;
        for key in ["registry", "directory", "local-registry", "git"] {
            is_source |= config.get_string(&format!("source.{name}.{key}"))?.is_some();
        }
        last = Some((name.clone(), !is_source));
        if !is_source
            || last.as_ref().is_some_and(|(it, _)| it == blaze_registry::CRATES_IO_REGISTRY)
        {
            break;
        }
    }
    Ok(last)
}

/// The `[registries.<name>]` whose index is `id`'s.
fn registry_named(config: &Config, id: &SourceId) -> anyhow::Result<Option<String>> {
    #[derive(serde::Deserialize)]
    struct Index {
        index: Option<String>,
    }
    let registries = config
        .get::<Option<std::collections::BTreeMap<String, Index>>>("registries")
        .ok()
        .flatten();
    Ok(registries.into_iter().flatten().find_map(|(name, it)| {
        let it = SourceId::for_registry(it.index.as_deref()?).ok()?;
        (it.canonical_url() == id.canonical_url()).then_some(name)
    }))
}

/// What a command does with the registry, for the token it needs.
#[derive(Clone, Copy, PartialEq)]
enum Operation {
    /// A search: a token only when the registry wants one for everything.
    Read,
    /// Owners, yanks: a token always.
    Write,
}

/// The run's flags every command reads.
struct Run {
    ctx: Context,
    quiet: bool,
    verbose: bool,
}

impl Run {
    fn status(&self, word: &str, rest: &str) {
        say::status(self.quiet, word, rest);
    }

    fn warn(&self, text: &str) {
        if !self.quiet {
            say::warning(text);
        }
    }

    fn note(&self, text: &str) {
        if !self.quiet {
            say::shell_note(text);
        }
    }
}

/// cargo's `ops::registry::registry`: the registry's `config.json` (asked again when
/// `force_update`), its API, and the token `operation` needs.
fn registry(
    run: &Run,
    target: &Target,
    from_command_line: Option<&str>,
    asked: Option<&RegistryOrIndex>,
    force_update: bool,
    operation: Option<Operation>,
) -> anyhow::Result<Api> {
    let is_index = matches!(asked, Some(RegistryOrIndex::Index(_)));
    if is_index && operation.is_some() && from_command_line.is_none() {
        bail!("command-line argument --index requires --token to be specified");
    }
    let sources = blaze_metadata::registry_sources(&run.ctx, false)?;
    let registry = sources.registry(&target.replacement)?;
    if force_update {
        registry.invalidate();
    }
    if !registry.config_is_cached() {
        run.status("Updating", &target.index());
    }
    let config = registry
        .config()
        .with_context(|| format!("failed to update {}", target.described()))?
        .with_context(|| format!("{} has no config.json", target.described()))?;
    let Some(api) = config.api else {
        bail!("{} does not support API commands", target.described());
    };
    let token = match operation.is_some() || config.auth_required {
        true => Some(token(run, target, from_command_line)?),
        false => None,
    };
    let http = blaze_registry::http::HyperHttp::shared()?;
    Ok(Api::new(api, token, http, config.auth_required))
}

/// A registry's credentials as `[registry]` (crates.io) or `[registries.<name>]` has them,
/// with `credentials.toml` and the variables over the files.
struct Credentials {
    token: OptValue<String>,
    provider: Option<PathAndArgs>,
}

/// cargo's `registry_credential_config_raw`: the table of the registry's name, the name
/// found by its index URL among `CARGO_REGISTRIES_<NAME>_INDEX` and then `[registries]`.
fn credentials(run: &Run, target: &Target) -> anyhow::Result<Option<Credentials>> {
    let config = &run.ctx.config;
    config.load_credentials()?;
    for warning in config.take_warnings() {
        run.warn(&warning);
    }
    if !target.original.is_remote_registry() {
        bail!(
            "{} does not support API commands.\nCheck for a source-replacement in .cargo/config.",
            target.described()
        );
    }
    let table = if target.original.is_crates_io() {
        config.check_registry_index_not_set()?;
        "registry".to_owned()
    } else {
        let index = target.original.canonical_url();
        let mut names: Vec<String> = config
            .env_vars()
            .filter_map(|(k, v)| {
                Some((k.strip_prefix("CARGO_REGISTRIES_")?.strip_suffix("_INDEX")?, v))
            })
            .filter(|(_, v)| SourceId::for_registry(v).is_ok_and(|it| it.canonical_url() == index))
            .map(|(k, _)| k.to_lowercase())
            .collect();
        if names.is_empty() {
            names.extend(registry_named(config, &target.original)?);
        }
        names.sort();
        let name = match names.len() {
            0 => return Ok(None),
            1 => names.pop().expect("one"),
            _ => bail!(
                "multiple registries are configured with the same index url '{}': {}",
                target.original.url(),
                names.join(", ")
            ),
        };
        if Some(&name) != target.name.as_ref() {
            run.note(&format!(
                "name of alternative registry `{}` set to `{name}`",
                target.original.url()
            ));
        }
        format!("registries.{name}")
    };
    let token = config.get::<OptValue<String>>(&format!("{table}.token"))?;
    let provider = config.get::<Option<PathAndArgs>>(&format!("{table}.credential-provider"))?;
    Ok(Some(Credentials { token, provider }))
}

const TOKEN: &str = "cargo:token";

/// cargo's `credential_provider`: the providers to ask in turn, each a program and its
/// arguments, with cargo's warnings of a token that none of them reads.
fn providers(
    run: &Run,
    target: &Target,
    found: Option<&Credentials>,
    warn: bool,
) -> anyhow::Result<Vec<Vec<String>>> {
    let config = &run.ctx.config;
    let resolve = |it: PathAndArgs| -> anyhow::Result<Vec<String>> {
        let name = it.path.raw_value().to_owned();
        if it.args.is_empty()
            && let Some(alias) =
                config.get::<Option<PathAndArgs>>(&format!("credential-alias.{name}"))?
            && !BUILT_IN.contains(&name.as_str())
        {
            let program = alias.path.resolve_program(config.cwd()).display().to_string();
            return Ok(std::iter::once(program).chain(alias.args).collect());
        }
        let program = it.path.resolve_program(config.cwd()).display().to_string();
        Ok(std::iter::once(program).chain(it.args).collect())
    };
    let global =
        config.get::<Option<Vec<Value<String>>>>("registry.global-credential-providers")?;
    let providers: Vec<Vec<String>> = match global.filter(|it| !it.is_empty()) {
        Some(list) => list
            .iter()
            .rev()
            .map(|it| resolve(PathAndArgs::from_whitespace_separated_string(it)))
            .collect::<anyhow::Result<_>>()?,
        None => vec![vec![TOKEN.to_owned()]],
    };
    let Some(found) = found else { return Ok(providers) };
    if let Some(provider) = &found.provider {
        let provider = resolve(provider.clone())?;
        if let Some(token) = &found.token
            && provider[0] != TOKEN
            && warn
        {
            run.warn(&format!(
                "{} has a token configured in {} that will be ignored because this registry is \
                 configured to use credential-provider `{}`",
                target.described(),
                token.definition,
                provider[0]
            ));
        }
        return Ok(vec![provider]);
    }
    if let Some(token) = &found.token
        && !providers.iter().any(|it| it[0] == TOKEN)
        && warn
    {
        run.warn(&format!(
            "{} has a token configured in {} that will be ignored because the `cargo:token` \
             credential provider is not listed in `registry.global-credential-providers`",
            target.described(),
            token.definition
        ));
    }
    Ok(providers)
}

/// cargo's built-in providers, which an alias cannot shadow.
const BUILT_IN: &[&str] = &[
    "cargo:token",
    "cargo:paseto",
    "cargo:token-from-stdout",
    "cargo:wincred",
    "cargo:macos-keychain",
    "cargo:libsecret",
];

/// What the providers are asked, as cargo's `Action`.
enum Action<'a> {
    Get,
    Login { token: Option<String>, login_url: Option<&'a str> },
    Logout,
}

impl fmt::Display for Action<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Action::Get => "get",
            Action::Login { .. } => "login",
            Action::Logout => "logout",
        })
    }
}

/// What a provider answered.
enum Answer {
    /// It has no token: none to give, or none to remove.
    NotFound,
    Token(String),
    Done,
}

/// cargo's `credential_action` with `cargo:token` the only provider blaze runs.
fn credential_action(run: &Run, target: &Target, action: Action<'_>) -> anyhow::Result<Answer> {
    let found = credentials(run, target)?;
    let providers = providers(run, target, found.as_ref(), true)?;
    let mut not_found = false;
    for provider in providers {
        if run.verbose {
            run.status(
                "Credential",
                &format!("{} {action} {}", provider.join(" "), target.shown()),
            );
        }
        if provider[0] != TOKEN {
            bail!(
                "credential provider `{}` is not one blaze runs: blaze keeps tokens only as \
                 `cargo:token` does, in its configuration and `credentials.toml`",
                provider.join(" ")
            );
        }
        let previous = found.as_ref().and_then(|it| it.token.as_ref()).map(|it| it.val.clone());
        let done = cargo_token(run, target, &action, previous).with_context(|| {
            format!("credential provider `{}` failed action `{action}`", provider.join(" "))
        })?;
        match done {
            Answer::NotFound => not_found = true,
            answer => return Ok(answer),
        }
    }
    match not_found {
        true => Ok(Answer::NotFound),
        false => bail!("no credential providers could handle the request"),
    }
}

/// cargo's `TokenCredential`.
fn cargo_token(
    run: &Run,
    target: &Target,
    action: &Action<'_>,
    previous: Option<String>,
) -> anyhow::Result<Answer> {
    let alternative = (!target.original.is_crates_io()).then(|| target.name.as_deref()).flatten();
    let name = target.shown();
    match action {
        Action::Get => Ok(previous.map_or(Answer::NotFound, Answer::Token)),
        Action::Login { token, login_url } => {
            let token = match token {
                Some(token) => token.clone(),
                None => {
                    match (login_url, &target.name) {
                        (Some(url), _) => eprintln!("please paste the token found on {url} below"),
                        (None, Some(name)) => eprintln!("please paste the token for {name} below"),
                        (None, None) => {
                            eprintln!("please paste the token for {} below", target.original.url())
                        }
                    }
                    read_line()?
                }
            };
            let token = token.replace("cargo login", "").trim().to_owned();
            check_token(&token)?;
            if !target.original.is_crates_io() && alternative.is_none() {
                bail!("can't save credentials for anonymous registry");
            }
            run.ctx.config.save_credentials(Some(&token), alternative)?;
            run.status("Login", &format!("token for `{name}` saved"));
            Ok(Answer::Done)
        }
        Action::Logout => {
            if previous.is_none() {
                return Ok(Answer::NotFound);
            }
            run.ctx.config.save_credentials(None, alternative)?;
            run.status(
                "Logout",
                &format!("token for `{name}` has been removed from local storage"),
            );
            let location = match target.original.is_crates_io() {
                true => "<https://crates.io/me>".to_owned(),
                false => format!("the `{name}` website"),
            };
            eprintln!(
                "note: This does not revoke the token on the registry server.\n    If you need to \
                 revoke the token, visit {location} and follow the instructions there."
            );
            Ok(Answer::Done)
        }
    }
}

fn read_line() -> anyhow::Result<String> {
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(line.trim().to_owned())
}

/// cargo's `auth_token`: the token given on the command line, else the providers'.
fn token(run: &Run, target: &Target, from_command_line: Option<&str>) -> anyhow::Result<String> {
    if let Some(token) = from_command_line {
        return Ok(token.to_owned());
    }
    match credential_action(run, target, Action::Get)? {
        Answer::Token(token) => Ok(token),
        _ => Err(AuthorizationError::new(run, target)?.into()),
    }
}

/// A registry that wants a token there is none of, as cargo says it.
#[derive(Debug)]
struct AuthorizationError {
    crates_io: bool,
    /// `--registry`'s name: none for `--index`.
    name: Option<String>,
    url: String,
    default_registry: bool,
    /// Whether `cargo:token` is among the providers, so that `cargo login` would help.
    cargo_token: bool,
}

impl AuthorizationError {
    fn new(run: &Run, target: &Target) -> anyhow::Result<AuthorizationError> {
        let found = credentials(run, target)?;
        let providers = providers(run, target, found.as_ref(), false)?;
        Ok(AuthorizationError {
            crates_io: target.original.is_crates_io(),
            name: target.name.clone(),
            url: target.original.url().to_string(),
            default_registry: run.ctx.config.default_registry()?.is_some(),
            cargo_token: providers.iter().any(|it| it[0] == TOKEN),
        })
    }
}

impl std::error::Error for AuthorizationError {}

impl fmt::Display for AuthorizationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.crates_io {
            let args = if self.default_registry { " --registry crates-io" } else { "" };
            write!(f, "no token found, please run `cargo login{args}`")?;
            if self.cargo_token {
                write!(f, "\nor use environment variable CARGO_REGISTRY_TOKEN")?;
            }
            Ok(())
        } else if let Some(name) = &self.name {
            write!(f, "no token found for `{name}`")?;
            match self.cargo_token {
                true => write!(
                    f,
                    ", please run `cargo login --registry {name}`\nor use environment variable {}",
                    blaze_manifest::config::env_key(&format!("registries.{name}.token"))
                ),
                false => {
                    write!(f, "\nYou may need to log in using this registry's credential provider")
                }
            }
        } else {
            write!(
                f,
                "no token found for `{url}`\nconsider setting up an alternate registry in Cargo's \
                 configuration\nas described by \
                 https://doc.rust-lang.org/cargo/reference/registries.html\n\n[registries]\n\
                 my-registry = {{ index = \"{url}\" }}\n",
                url = self.url
            )
        }
    }
}

/// The run of one command, its `--config`s merged.
fn start(name: &str, read: &Read) -> anyhow::Result<Run> {
    Ok(Run {
        ctx: Context::new(config(name, read)?),
        quiet: read.has("--quiet"),
        verbose: read.has("--verbose"),
    })
}

fn ended(done: anyhow::Result<()>) -> anyhow::Result<i32> {
    Ok(match done {
        Ok(()) => 0,
        Err(err) => failed(&err),
    })
}

/// `cargo search [QUERY]...`.
pub(super) fn search(typed: &[String]) -> anyhow::Result<i32> {
    let flags = [valued(&["--limit"]), valued(&["--index"]), valued(&["--registry"])];
    let read = match SEARCH.read(typed, &flags) {
        Ok(read) => read,
        Err(code) => return Ok(code),
    };
    let run = start("search", &read)?;
    let asked = match registry_or_index(&read, &run.ctx.config) {
        Ok(it) => it,
        Err(err) => return Ok(failed(&err)),
    };
    let limit = match read.value("--limit") {
        Some(limit) => match limit.parse::<u32>() {
            Ok(limit) => limit,
            Err(_) => {
                say::error_unended(&format!(
                    "invalid value: could not parse `{limit}` as a number"
                ));
                return Ok(USAGE);
            }
        },
        None => 10,
    };
    let query = read.positional.join("+");
    ended(searched(&run, &query, asked.as_ref(), limit.min(100)))
}

fn searched(
    run: &Run,
    query: &str,
    asked: Option<&RegistryOrIndex>,
    limit: u32,
) -> anyhow::Result<()> {
    let target = target(&run.ctx, asked)?;
    let api = registry(run, &target, None, asked, false, None)?;
    let (crates, total) = api.search(query, limit).with_context(|| {
        format!("failed to retrieve search results from the registry at {}", api.host())
    })?;
    let names: Vec<String> =
        crates.iter().map(|it| format!("{} = \"{}\"", it.name, it.max_version)).collect();
    let margin = names.iter().map(String::len).max().unwrap_or_default() + 4;
    let length = 80.max(128usize.saturating_sub(margin));
    let colored = say::colored_out();
    let mut out = String::new();
    for (name, it) in names.into_iter().zip(&crates) {
        let line = match &it.description {
            Some(description) => {
                let description = truncate_with_ellipsis(&description.replace('\n', " "), length);
                format!("{name: <margin$}# {description}")
            }
            None => name,
        };
        let mut fragments = line.split(query).peekable();
        while let Some(fragment) = fragments.next() {
            out.push_str(fragment);
            if fragments.peek().is_some() {
                match colored {
                    true => out.push_str(&format!("\x1b[1m\x1b[92m{query}\x1b[0m")),
                    false => out.push_str(query),
                }
            }
        }
        out.push('\n');
    }
    if total > limit && limit < 100 {
        out.push_str(&format!(
            "... and {} crates more (use --limit N to see more)\n",
            total - limit
        ));
    } else if total > limit && limit >= 100 {
        let extra = match target.original.is_crates_io() {
            true => {
                let url = url::Url::parse_with_params("https://crates.io/search", &[("q", query)])?;
                format!(" (go to {url} to see more)")
            }
            false => String::new(),
        };
        out.push_str(&format!("... and {} crates more{extra}\n", total - limit));
    }
    print!("{out}");
    if total > 0 {
        let literal = match say::colored() {
            true => "\x1b[1m\x1b[96mcargo info <name>\x1b[0m",
            false => "cargo info <name>",
        };
        run.note(&format!("to learn more about a package, run `{literal}`"));
    }
    Ok(())
}

/// cargo's `truncate_with_ellipsis`: at most `max` characters, the last an ellipsis when cut.
fn truncate_with_ellipsis(text: &str, max: usize) -> String {
    let mut chars = text.chars();
    let mut prefix: String = chars.by_ref().take(max - 1).collect();
    if chars.next().is_some() {
        prefix.push('…');
    }
    prefix
}

/// The package of the working directory, for `owner` and `yank` with none named.
fn current_package(run: &Run) -> anyhow::Result<String> {
    let root = root_manifest(None, &run.ctx)?;
    let ws = Workspace::new(&root, &run.ctx)?;
    let current = ws.current_manifest().to_owned();
    match ws.members().find(|(path, _)| *path == current) {
        Some((_, manifest)) => Ok(manifest.package_id.name.clone()),
        None => bail!(
            "manifest path `{}` is a virtual manifest, but this command requires running against an actual package in this workspace",
            current.display()
        ),
    }
}

/// `cargo owner [CRATE]`.
pub(super) fn owner(typed: &[String]) -> anyhow::Result<i32> {
    let flags = [
        valued(&["-a", "--add"]),
        valued(&["-r", "--remove"]),
        switch(&["-l", "--list"]),
        valued(&["--index"]),
        valued(&["--registry"]),
        valued(&["--token"]),
    ];
    let read = match OWNER.read(typed, &flags) {
        Ok(read) => read,
        Err(code) => return Ok(code),
    };
    if let Some(extra) = read.positional.get(1) {
        return Ok(OWNER.refuse(&format!("unexpected argument '{extra}' found"), OWNER.usage));
    }
    let run = start("owner", &read)?;
    ended((|| {
        let asked = registry_or_index(&read, &run.ctx.config)?;
        let name = match read.positional.first() {
            Some(name) => name.clone(),
            None => current_package(&run)?,
        };
        let target = target(&run.ctx, asked.as_ref())?;
        let token = read.value("--token");
        let api = registry(
            &run,
            &target,
            token.as_deref(),
            asked.as_ref(),
            true,
            Some(Operation::Write),
        )?;
        let host = api.host().to_owned();
        let to_add = read.values("--add");
        if !to_add.is_empty() {
            let users: Vec<&str> = to_add.iter().map(String::as_str).collect();
            let msg = api.add_owners(&name, &users).with_context(|| {
                format!("failed to invite owners to crate `{name}` on registry at {host}")
            })?;
            run.status("Owner", &msg);
        }
        let to_remove = read.values("--remove");
        if !to_remove.is_empty() {
            let users: Vec<&str> = to_remove.iter().map(String::as_str).collect();
            run.status("Owner", &format!("removing {users:?} from crate {name}"));
            api.remove_owners(&name, &users).with_context(|| {
                format!("failed to remove owners from crate `{name}` on registry at {host}")
            })?;
        }
        if read.has("--list") {
            let owners = api.list_owners(&name).with_context(|| {
                format!("failed to list owners of crate `{name}` on registry at {host}")
            })?;
            let mut out = String::new();
            for it in owners {
                out.push_str(&it.login);
                match (it.name, it.email) {
                    (Some(name), Some(email)) => out.push_str(&format!(" ({name} <{email}>)\n")),
                    (Some(it), None) | (None, Some(it)) => out.push_str(&format!(" ({it})\n")),
                    (None, None) => out.push('\n'),
                }
            }
            print!("{out}");
        }
        Ok(())
    })())
}

/// `cargo yank [CRATE] --version <VERSION> [--undo]`.
pub(super) fn yank(typed: &[String]) -> anyhow::Result<i32> {
    let flags = [
        valued(&["--version", "--vers"]),
        switch(&["--undo"]),
        valued(&["--index"]),
        valued(&["--registry"]),
        valued(&["--token"]),
    ];
    let read = match YANK.read(typed, &flags) {
        Ok(read) => read,
        Err(code) => return Ok(code),
    };
    if let Some(extra) = read.positional.get(1) {
        return Ok(YANK.refuse(&format!("unexpected argument '{extra}' found"), YANK.usage));
    }
    let run = start("yank", &read)?;
    ended((|| {
        let (krate, version) = resolve_crate(read.positional.first(), read.value("--version"))?;
        let Some(version) = version else { bail!("`--version` is required") };
        let asked = registry_or_index(&read, &run.ctx.config)?;
        let name = match krate {
            Some(name) => name,
            None => current_package(&run)?,
        };
        let target = target(&run.ctx, asked.as_ref())?;
        let token = read.value("--token");
        let api = registry(
            &run,
            &target,
            token.as_deref(),
            asked.as_ref(),
            true,
            Some(Operation::Write),
        )?;
        let spec = format!("{name}@{version}");
        if read.has("--undo") {
            run.status("Unyank", &spec);
            api.unyank(&name, &version).with_context(|| {
                format!("failed to undo a yank from the registry at {}", api.host())
            })?;
        } else {
            run.status("Yank", &spec);
            api.yank(&name, &version)
                .with_context(|| format!("failed to yank from the registry at {}", api.host()))?;
        }
        Ok(())
    })())
}

/// `name@version` taken apart, and the version checked, as cargo's `resolve_crate`.
fn resolve_crate(
    krate: Option<&String>,
    version: Option<String>,
) -> anyhow::Result<(Option<String>, Option<String>)> {
    let (mut krate, mut version) = (krate.cloned(), version);
    if let Some((k, v)) = krate.as_deref().and_then(|it| it.split_once('@')) {
        if version.is_some() {
            bail!("cannot specify both `@{v}` and `--version`");
        }
        if k.is_empty() {
            bail!("missing crate name for `@{v}`");
        }
        let (k, v) = (k.to_owned(), v.to_owned());
        krate = Some(k);
        version = Some(v);
    }
    if let Some(version) = &version
        && semver::Version::parse(version).is_err()
    {
        let err = semver::Version::parse(version).unwrap_err();
        let why = match version.strip_prefix('v') {
            Some(stripped) => format!(
                "the version provided, `{version}` is not a valid SemVer version\n\nhelp: try changing the version to `{stripped}`"
            ),
            None => format!("invalid version `{version}`"),
        };
        return Err(anyhow::Error::new(err).context(why));
    }
    Ok((krate, version))
}

/// `cargo login [-- [args]...]`, with the token from standard input (or, deprecated, the
/// one word given).
pub(super) fn login(typed: &[String]) -> anyhow::Result<i32> {
    let flags = [valued(&["--registry"])];
    let (before, _provider_args) = match typed.iter().position(|it| it == "--") {
        Some(at) => (&typed[..at], &typed[at + 1..]),
        None => (typed, &typed[typed.len()..]),
    };
    let read = match LOGIN.read(before, &flags) {
        Ok(read) => read,
        Err(code) => return Ok(code),
    };
    if let Some(extra) = read.positional.get(1) {
        return Ok(LOGIN.refuse(&format!("unexpected argument '{extra}' found"), LOGIN.usage));
    }
    let run = start("login", &read)?;
    ended((|| {
        let asked = registry_or_index(&read, &run.ctx.config)?;
        let from_command_line = read.positional.first().cloned();
        if from_command_line.is_some() {
            run.warn(
                "`cargo login <token>` is deprecated in favor of reading `<token>` from stdin",
            );
        }
        let target = target(&run.ctx, asked.as_ref())?;
        let login_url = match registry(
            &run,
            &target,
            from_command_line.as_deref(),
            asked.as_ref(),
            false,
            None,
        ) {
            Ok(api) => Some(format!("{}/me", api.host())),
            Err(err) if err.is::<AuthorizationError>() => None,
            Err(err) => return Err(err),
        };
        let token = from_command_line.or_else(|| {
            if std::io::stdin().is_terminal() {
                return None;
            }
            read_line().ok().filter(|it| !it.is_empty())
        });
        let action = Action::Login { token, login_url: login_url.as_deref() };
        credential_action(&run, &target, action)?;
        Ok(())
    })())
}

/// `cargo logout`.
pub(super) fn logout(typed: &[String]) -> anyhow::Result<i32> {
    let flags = [valued(&["--registry"])];
    let read = match LOGOUT.read(typed, &flags) {
        Ok(read) => read,
        Err(code) => return Ok(code),
    };
    if let Some(extra) = read.positional.first() {
        return Ok(LOGOUT.refuse(&format!("unexpected argument '{extra}' found"), LOGOUT.usage));
    }
    let run = start("logout", &read)?;
    ended((|| {
        let asked = registry_or_index(&read, &run.ctx.config)?;
        let target = target(&run.ctx, asked.as_ref())?;
        if let Answer::NotFound = credential_action(&run, &target, Action::Logout)? {
            run.status("Logout", &format!("not currently logged in to `{}`", target.shown()));
        }
        Ok(())
    })())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptions_cut_as_cargo_cuts_them() {
        assert_eq!(truncate_with_ellipsis("abcdef", 4), "abc…");
        assert_eq!(truncate_with_ellipsis("abcd", 4), "abc…");
        assert_eq!(truncate_with_ellipsis("abc", 4), "abc");
        assert_eq!(truncate_with_ellipsis("ééééé", 3), "éé…");
    }

    #[test]
    fn versions_with_the_crate() {
        let it =
            |k: &str, v: Option<&str>| resolve_crate(Some(&k.to_owned()), v.map(str::to_owned));
        assert_eq!(it("a@1.0.0", None).unwrap(), (Some("a".into()), Some("1.0.0".into())));
        assert!(
            it("a@1.0.0", Some("1.0.0")).unwrap_err().to_string().contains("cannot specify both")
        );
        assert!(it("@1.0.0", None).unwrap_err().to_string().contains("missing crate name"));
        assert!(format!("{:#}", it("a", Some("v1.0.0")).unwrap_err()).contains("try changing"));
    }
}
