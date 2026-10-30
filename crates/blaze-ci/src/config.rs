//! The service's one input: a JSON file (rule scripts-take-typed-input), read into structs that
//! refuse unknown fields. One file serves both machines: `blaze-ci serve` reads all of it,
//! `blaze-ci standby` its `standby` part.
//!
//! Secrets (the App's key, the webhook's secret, the primary's token) are never in the JSON: it
//! names their files, read only when needed ([`Secret`]), never printed.

use std::{
    collections::BTreeSet,
    fmt,
    net::SocketAddr,
    path::{Path, PathBuf},
};

use blaze_api::{Request, Targets, ViewRef};
use serde::Deserialize;

use crate::oracle;

#[derive(Deserialize, Clone, PartialEq, Eq, Debug)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// `owner/name`: deliveries for any other repository are refused.
    pub repo: String,
    pub app: AppConfig,
    pub webhook: WebhookConfig,
    pub daemon: DaemonConfig,
    pub git: GitConfig,
    /// The branch whose green runs are committed to the head `ci/<branch>` and snapshotted.
    pub default_branch: String,
    /// What runs on every push and pull request.
    pub checks: Vec<Check>,
    /// What runs on a tag, one release at a time.
    pub release_checks: Vec<Check>,
    pub oracle: OracleConfig,
    pub snapshot: SnapshotConfig,
    pub standby: StandbyConfig,
    /// The service's own state (each ref's checkout, the snapshots), rewritten after each
    /// change; absent on the first start.
    pub state: PathBuf,
}

#[derive(Deserialize, Clone, PartialEq, Eq, Debug)]
#[serde(deny_unknown_fields)]
pub struct AppConfig {
    pub id: u64,
    pub installation: u64,
    /// The App's private key, PEM.
    pub key_file: PathBuf,
    /// The API's host, `api.github.com`.
    pub api: String,
}

#[derive(Deserialize, Clone, PartialEq, Eq, Debug)]
#[serde(deny_unknown_fields)]
pub struct WebhookConfig {
    /// Where the listener binds, behind the VM's proxy.
    pub listen: SocketAddr,
    pub secret_file: PathBuf,
}

#[derive(Deserialize, Clone, PartialEq, Eq, Debug)]
#[serde(deny_unknown_fields)]
pub struct DaemonConfig {
    /// The warm daemon's agent API socket.
    pub socket: PathBuf,
}

#[derive(Deserialize, Clone, PartialEq, Eq, Debug)]
#[serde(deny_unknown_fields)]
pub struct GitConfig {
    /// The VM's clone of the repository.
    pub dir: PathBuf,
    pub remote: String,
}

#[derive(Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
pub enum CheckKind {
    Check,
    Build,
    Test,
}

/// One check run per event.
#[derive(Deserialize, Clone, PartialEq, Eq, Debug)]
#[serde(deny_unknown_fields)]
pub struct Check {
    /// The check run's name on GitHub, which required checks name.
    pub name: String,
    pub kind: CheckKind,
    pub targets: TargetsConfig,
    /// For a test check only: test names to run.
    #[serde(default)]
    pub filter: Vec<String>,
    /// For a test check only: every test, not only those the change reached.
    #[serde(default)]
    pub all: bool,
}

impl Check {
    /// The request this check sends for `view`.
    pub fn request(&self, view: ViewRef) -> Request {
        let targets = self.targets.to_targets();
        match self.kind {
            CheckKind::Check => Request::Check { view, targets },
            CheckKind::Build => Request::Build { view, targets },
            CheckKind::Test => {
                Request::Test {
                    view,
                    targets,
                    filter: self.filter.clone(),
                    all: self.all,
                    args: Vec::new(),
                    ub: None,
                }
            }
        }
    }
}

/// [`blaze_api::Targets`] as the file spells it.
#[derive(Deserialize, Clone, PartialEq, Eq, Debug)]
#[serde(deny_unknown_fields)]
pub struct TargetsConfig {
    /// Empty for the workspace's default members.
    #[serde(default)]
    pub packages: Vec<String>,
    pub profile: String,
    /// `None` for the machine's.
    #[serde(default)]
    pub target: Option<String>,
    #[serde(default)]
    pub features: Vec<String>,
    #[serde(default)]
    pub all_features: bool,
    #[serde(default)]
    pub no_default_features: bool,
}

impl TargetsConfig {
    pub fn to_targets(&self) -> Targets {
        Targets {
            packages: self.packages.clone(),
            profile: self.profile.clone(),
            target: self.target.clone(),
            features: self.features.clone(),
            all_features: self.all_features,
            no_default_features: self.no_default_features,
            cargo: None,
        }
    }
}

#[derive(Deserialize, Clone, PartialEq, Eq, Debug)]
#[serde(deny_unknown_fields)]
pub struct OracleConfig {
    /// One view in this many is built again from nothing ([`crate::oracle::sampled`]); 0 for
    /// none.
    pub one_in: u64,
    /// What a sampled view is verified for.
    pub targets: TargetsConfig,
}

#[derive(Deserialize, Clone, PartialEq, Eq, Debug)]
#[serde(deny_unknown_fields)]
pub struct SnapshotConfig {
    /// The least time between two snapshots.
    pub every_secs: u64,
    /// Takes a snapshot of this VM; `{name}` is replaced by the snapshot's name. Its stdout's
    /// last nonblank line is the snapshot's id.
    pub command: Vec<String>,
    /// Removes a snapshot; `{id}` is replaced by its id.
    pub remove: Vec<String>,
    /// How many snapshots are kept; older ones are removed.
    pub keep: usize,
}

#[derive(Deserialize, Clone, PartialEq, Eq, Debug)]
#[serde(deny_unknown_fields)]
pub struct StandbyConfig {
    /// The primary's agent API over HTTP.
    pub primary: SocketAddr,
    /// The bearer token of the primary's HTTP API.
    pub token_file: PathBuf,
    pub every_secs: u64,
    /// Failed polls in a row after which the standby takes over.
    pub misses: u32,
    /// Prints the newest snapshot's id as its stdout's last nonblank line.
    pub latest: Vec<String>,
    /// Restores snapshot `{id}` here.
    pub restore: Vec<String>,
    /// Makes the restored VM the primary (moves its address, its webhook).
    pub promote: Vec<String>,
}

/// Why the file is refused.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ConfigError {
    pub what: String,
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.what)
    }
}

impl std::error::Error for ConfigError {}

impl Config {
    pub fn read(path: &Path) -> Result<Config, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|error| ConfigError {
            what: format!("could not read {}: {error}", path.display()),
        })?;
        Config::parse(&text).map_err(|error| ConfigError {
            what: format!("{}: {}", path.display(), error.what),
        })
    }

    pub fn parse(text: &str) -> Result<Config, ConfigError> {
        let config: Config = serde_json::from_str(text)
            .map_err(|error| ConfigError { what: error.to_string() })?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<(), ConfigError> {
        let refuse = |what: String| Err(ConfigError { what });
        if self.repo.split('/').filter(|part| !part.is_empty()).count() != 2
            || self.repo.matches('/').count() != 1
        {
            return refuse(format!("repo {:?} is not owner/name", self.repo));
        }
        if self.default_branch.is_empty() {
            return refuse("default_branch is empty".to_owned());
        }
        for (list, checks) in [("checks", &self.checks), ("release_checks", &self.release_checks)] {
            let mut names = BTreeSet::new();
            for check in checks {
                if check.name.is_empty() {
                    return refuse(format!("a check of {list} has no name"));
                }
                if check.name == oracle::CHECK {
                    return refuse(format!(
                        "check {} of {list} has the oracle's name, which the service posts",
                        check.name
                    ));
                }
                if !names.insert(check.name.as_str()) {
                    return refuse(format!("check {} is named twice in {list}", check.name));
                }
                if check.kind != CheckKind::Test && (!check.filter.is_empty() || check.all) {
                    return refuse(format!(
                        "check {} of {list} has a test filter or `all`, and is not a test",
                        check.name
                    ));
                }
            }
        }
        if self.checks.is_empty() {
            return refuse("checks is empty: a push would post nothing".to_owned());
        }
        if self.release_checks.is_empty() {
            return refuse("release_checks is empty: a tag would post nothing".to_owned());
        }
        let snapshot = &self.snapshot;
        if snapshot.command.is_empty() {
            return refuse("snapshot.command is empty".to_owned());
        }
        if snapshot.remove.is_empty() {
            return refuse("snapshot.remove is empty".to_owned());
        }
        if snapshot.keep == 0 {
            return refuse("snapshot.keep is 0: the snapshot just taken would be removed".to_owned());
        }
        let standby = &self.standby;
        for (name, argv) in
            [("latest", &standby.latest), ("restore", &standby.restore), ("promote", &standby.promote)]
        {
            if argv.is_empty() {
                return refuse(format!("standby.{name} is empty"));
            }
        }
        if standby.every_secs == 0 {
            return refuse("standby.every_secs is 0".to_owned());
        }
        if standby.misses == 0 {
            return refuse("standby.misses is 0: the standby would take over at once".to_owned());
        }
        Ok(())
    }
}

/// The bytes of a secret's file, without a trailing newline. Debug never shows them.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(Vec<u8>);

impl Secret {
    pub fn read(path: &Path) -> Result<Secret, ConfigError> {
        let mut bytes = std::fs::read(path).map_err(|error| ConfigError {
            what: format!("could not read the secret file {}: {error}", path.display()),
        })?;
        while bytes.last().is_some_and(|byte| *byte == b'\n' || *byte == b'\r') {
            bytes.pop();
        }
        if bytes.is_empty() {
            return Err(ConfigError { what: format!("the secret file {} is empty", path.display()) });
        }
        Ok(Secret(bytes))
    }

    pub fn expose(&self) -> &[u8] {
        &self.0
    }

    /// The secret as text (a PEM key, a token).
    pub fn text(&self, path: &Path) -> Result<String, ConfigError> {
        String::from_utf8(self.0.clone()).map_err(|_| ConfigError {
            what: format!("the secret file {} is not UTF-8", path.display()),
        })
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(..)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE: &str = r#"{
        "repo": "indexable-inc/blaze",
        "app": {
            "id": 123,
            "installation": 456,
            "key_file": "/etc/blaze-ci/app.pem",
            "api": "api.github.com"
        },
        "webhook": { "listen": "127.0.0.1:8080", "secret_file": "/etc/blaze-ci/webhook" },
        "daemon": { "socket": "/run/blaze/api.sock" },
        "git": { "dir": "/var/lib/blaze-ci/repo", "remote": "https://github.com/indexable-inc/blaze" },
        "default_branch": "main",
        "checks": [
            { "name": "blaze/check", "kind": "check", "targets": { "profile": "dev" } },
            {
                "name": "blaze/test",
                "kind": "test",
                "targets": { "packages": ["blaze-ci"], "profile": "dev", "features": ["x"] },
                "filter": ["service"]
            }
        ],
        "release_checks": [
            {
                "name": "blaze/release",
                "kind": "build",
                "targets": { "profile": "release", "target": "x86_64-unknown-linux-gnu" }
            }
        ],
        "oracle": { "one_in": 50, "targets": { "profile": "dev" } },
        "snapshot": {
            "every_secs": 3600,
            "command": ["ix", "snapshot", "{name}"],
            "remove": ["ix", "snapshot", "rm", "{id}"],
            "keep": 3
        },
        "standby": {
            "primary": "10.0.0.2:7070",
            "token_file": "/etc/blaze-ci/token",
            "every_secs": 10,
            "misses": 6,
            "latest": ["ix", "snapshot", "latest"],
            "restore": ["ix", "restore", "{id}"],
            "promote": ["ix", "promote"]
        },
        "state": "/var/lib/blaze-ci/state.json"
    }"#;

    #[test]
    fn a_full_example_parses() {
        let config = Config::parse(EXAMPLE).unwrap();
        assert_eq!(config.repo, "indexable-inc/blaze");
        assert_eq!(config.app.installation, 456);
        assert_eq!(config.webhook.listen, "127.0.0.1:8080".parse().unwrap());
        assert_eq!(config.checks.len(), 2);
        assert_eq!(config.checks[1].kind, CheckKind::Test);
        assert_eq!(config.checks[1].filter, ["service"]);
        assert_eq!(config.release_checks[0].targets.target.as_deref(), Some("x86_64-unknown-linux-gnu"));
        assert_eq!(config.oracle.one_in, 50);
        assert_eq!(config.snapshot.keep, 3);
        assert_eq!(config.standby.misses, 6);
        let request = config.checks[1].request(ViewRef::Head("h".to_owned()));
        let Request::Test { targets, filter, all, .. } = request else { panic!("{request:?}") };
        assert_eq!(targets.packages, ["blaze-ci"]);
        assert_eq!(filter, ["service"]);
        assert!(!all);
    }

    #[test]
    fn an_unknown_field_is_refused_naming_it() {
        let text = EXAMPLE.replace("\"keep\": 3", "\"keep\": 3, \"kept\": 4");
        let error = Config::parse(&text).unwrap_err();
        assert!(error.what.contains("kept"), "{error}");
        let text = EXAMPLE.replace("\"api\": \"api.github.com\"", "\"api\": \"x\", \"token\": \"t\"");
        let error = Config::parse(&text).unwrap_err();
        assert!(error.what.contains("token"), "{error}");
    }

    #[test]
    fn a_filter_on_a_check_that_is_not_a_test_is_refused() {
        let text = EXAMPLE.replace(
            r#""kind": "check", "targets": { "profile": "dev" } }"#,
            r#""kind": "check", "targets": { "profile": "dev" }, "all": true }"#,
        );
        let error = Config::parse(&text).unwrap_err();
        assert!(error.what.contains("blaze/check"), "{error}");
    }

    #[test]
    fn the_oracles_name_is_refused_for_a_check() {
        let text = EXAMPLE.replace("blaze/release", oracle::CHECK);
        assert!(Config::parse(&text).unwrap_err().what.contains(oracle::CHECK));
    }

    #[test]
    fn a_secret_hides_in_debug_and_loses_its_newline() {
        let path = std::env::temp_dir().join(format!("blaze-ci-secret-{}", std::process::id()));
        std::fs::write(&path, "hunter2\n").unwrap();
        let secret = Secret::read(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert_eq!(secret.expose(), b"hunter2");
        assert!(!format!("{secret:?}").contains("hunter2"));
    }
}
