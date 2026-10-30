//! The view's workspace resolved, for a query of another crate that prints from the resolve
//! rather than building (blaze-tree's `cargo tree`): the same queries and the same resolve a
//! build's [`crate::query::Plan`] makes, handed over before the unit graph.

use std::path::Path;

use blaze_manifest::manifest::Manifest;
use blaze_manifest::{Config, Workspace};
use blaze_query::{Cancelled, Cx};
use blaze_manifest::config::schema::FeatureUnification;
use blaze_resolve::resolver::{CliFeatures, Members, ResolveBehavior};
use blaze_resolve::{Cfg, PackageId, PackageIdSpec, Resolve, Summary};
use rustc_hash::FxHashMap;
use serde::{Deserialize, Serialize};

use super::files::{Mounts, Snap};
use crate::read::Read;
use super::invocation::Asking;
use super::manifest::{WorkspaceQuery, replay};
use super::{Select, Staged, unmounted_error, view_spec};
use crate::query::{Fetch, LoadError};
use crate::stdlib::target::{self as platform, CfgArgs};

/// What the workspace is resolved for: `-p`, `--workspace`, `--exclude`, the features, and
/// whether the dev-dependencies are.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug, Default)]
pub struct ResolveAsk {
    pub packages: Vec<String>,
    pub workspace: bool,
    pub exclude: Vec<String>,
    pub features: Vec<String>,
    pub all_features: bool,
    pub no_default_features: bool,
    pub dev_units: bool,
}

/// The view's workspace resolved, as cargo's `resolve_ws_with_opts` leaves it.
pub struct ViewResolved<'a> {
    pub ws: &'a Workspace<'a, Snap>,
    pub config: &'a Config<Snap>,
    /// The resolve of the packages selected, guided by the lock file.
    pub resolve: &'a Resolve,
    pub members: Members<'a>,
    /// The members' ids, in the workspace's order.
    pub member_ids: &'a [PackageId],
    /// The packages selected.
    pub specs: &'a [PackageIdSpec],
    pub cli_features: &'a CliFeatures,
    pub behavior: ResolveBehavior,
    pub unification: FeatureUnification,
    /// What was read of every package of the resolve, and its manifest and where that is.
    pub reads: &'a FxHashMap<PackageId, Read>,
    pub manifests: &'a FxHashMap<PackageId, (&'a Path, &'a Manifest)>,
    /// The machine the build runs on.
    pub host: &'a str,
    /// What cargo warns of while reading the workspace and selecting its packages.
    pub warnings: &'a [String],
}

/// What resolving the view came to.
pub enum Resolving<R> {
    Done(R),
    /// What it would read that no mount covers yet, as [`crate::query::Planned::Needs`].
    Needs(Vec<Fetch>),
    Failed(LoadError),
}

/// Resolves the view's workspace as `ask` says (the view's command line for the rest: its
/// `--locked`, `--offline`, `--config`, `--manifest-path`) and hands it to `then`. `then`
/// may run more than once: only its last run's answer is the one returned.
pub fn resolve_view<R>(
    cx: &mut Cx<'_>,
    ask: &ResolveAsk,
    mut then: impl FnMut(&ViewResolved<'_>) -> Result<R, LoadError>,
) -> Result<Resolving<R>, Cancelled> {
    let asked = match cx.get::<Asking>(&())?.get() {
        Ok(asked) => asked,
        Err(error) => return Ok(Resolving::Failed(error)),
    };
    let library = cx.get::<Mounts>(&())?.get(&asked.library);
    let mut spec = match view_spec(&asked, library) {
        Ok(spec) => spec,
        Err(error) => return Ok(Resolving::Failed(error)),
    };
    spec.select = match (ask.workspace, ask.exclude.is_empty(), ask.packages.is_empty()) {
        (false, true, true) => Select::Default,
        (false, true, false) => Select::Packages(ask.packages.clone()),
        (false, false, _) => {
            let what = "--exclude can only be used together with --workspace".to_owned();
            return Ok(Resolving::Failed(LoadError { what, at: None }));
        }
        (true, true, _) => Select::All,
        (true, false, _) => Select::OptOut(ask.exclude.clone()),
    };
    spec.features.clone_from(&ask.features);
    spec.all_features = ask.all_features;
    spec.no_default_features = ask.no_default_features;
    spec.intent = crate::Intent::Build;
    spec.filter = match ask.dev_units {
        true => crate::Filter::all_targets(),
        false => crate::Filter::Default { required_features_filterable: true },
    };
    let mut read = match cx.get::<WorkspaceQuery>(&spec.workspace)?.get() {
        Ok(read) => read,
        Err(error) => return Ok(Resolving::Failed(error)),
    };
    let base = std::mem::take(&mut read.snapshot);
    let (outcome, used) = replay(cx, &read, base, &spec.workspace.config, |cx, ctx, ws, files, reads| {
        super::resolve::resolved(cx, ctx, ws, &files, reads, &spec, |_, resolved| {
            let member_summaries: Vec<&Summary> =
                resolved.member_ids.iter().map(|id| &resolved.reads[id].summary).collect();
            let warnings: Vec<String> = read.warnings.iter().chain(&resolved.warnings).cloned().collect();
            let view = ViewResolved {
                ws,
                config: &ctx.config,
                resolve: &resolved.resolve,
                members: Members {
                    members: &member_summaries,
                    current: resolved.current,
                    new_cli_feature_behavior: resolved.new_cli_feature_behavior,
                },
                member_ids: &resolved.member_ids,
                specs: &resolved.specs,
                cli_features: &resolved.cli_features,
                behavior: resolved.behavior,
                unification: resolved.unification,
                reads: &resolved.reads,
                manifests: &resolved.manifests,
                host: &spec.host,
                warnings: &warnings,
            };
            Ok(match then(&view) {
                Ok(it) => Staged::Done(it),
                Err(error) => Staged::Failed(error),
            })
        })
    })?;
    if let Some(at) = used.unmounted() {
        return Ok(Resolving::Failed(unmounted_error(at)));
    }
    Ok(match outcome {
        Ok(Staged::Done(it)) => Resolving::Done(it),
        Ok(Staged::Needs(fetches)) => Resolving::Needs(fetches),
        Ok(Staged::Failed(error)) | Err(error) => Resolving::Failed(error),
    })
}

/// `triple`'s cfg as cargo reads `[target.'cfg(..)']` for a build on `host` (with `target`
/// the build's `--target`): its probe with the configuration's rustflags for it and
/// `--crate-type proc-macro`.
pub fn platform_cfg(config: &Config<Snap>, triple: &str, host: &str, target: Option<&str>) -> Result<Vec<Cfg>, LoadError> {
    let error = |what: String| LoadError { what, at: None };
    let rustflags = super::plan::platform_rustflags(config, triple, host, target)?;
    let args = CfgArgs::from_flags(&rustflags).map_err(error)?;
    platform::target_cfgs(triple, &CfgArgs { proc_macro: true, ..args }).map_err(error)
}
