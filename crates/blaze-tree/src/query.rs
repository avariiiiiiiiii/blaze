//! `cargo tree` as a query of blaze's database: [`Tree`] resolves the view's workspace through
//! blaze-units' load queries ([`resolve_view`]) and prints it as [`crate::print_tree`] prints
//! blaze-metadata's.

use blaze_format::SchemaHash;
use blaze_ids::ContentId;
use blaze_query::{Cancelled, Cx, Database, Query, QueryKind};
use blaze_units::load::{ResolveAsk, Resolving, ViewResolved, load_error, platform_cfg, resolve_view};
use blaze_units::query::{Cbor, Fetch, LoadError, Versioned, key_input, producer};
use rustc_hash::FxHashSet;
use serde::{Deserialize, Serialize};

use crate::{DepKind, DisplayDepth, EdgeKind, Input, Kinds, Options, Packages, Platformed, Prefix, Target};

/// `cargo tree`'s command line, as cargo reads it: everything of [`Options`] and the
/// platforms. The rest of the invocation (`--manifest-path`, `--config`, `--locked`,
/// `--offline`) is the view's.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct TreeAsk {
    /// `-p`, `--workspace`, `--exclude` and the features. Whether dev-dependencies are
    /// resolved is `edges`'.
    pub resolve: ResolveAsk,
    /// `--target`, as given: `all` alone for every platform, none for the host.
    pub targets: Vec<String>,
    pub edges: Vec<Edge>,
    pub invert: Vec<String>,
    pub prune: Vec<String>,
    pub prefix: TreePrefix,
    pub no_dedupe: bool,
    pub duplicates: bool,
    pub format: String,
    pub depth: TreeDepth,
    pub no_proc_macro: bool,
    pub public: bool,
    pub unicode: bool,
    pub color: bool,
}

/// One of [`Options::edge_kinds`].
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub enum Edge {
    Normal,
    Build,
    Dev,
    Features,
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub enum TreePrefix {
    None,
    Indent,
    Depth,
}

/// `--depth`: a number (`u32::MAX` when not given) or `workspace`.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub enum TreeDepth {
    Max(u32),
    Workspace,
}

/// What `cargo tree` came to: its output and warnings, what it would read that no mount
/// covers yet, or cargo's error.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub enum Printed {
    Ready { out: String, warnings: Vec<String> },
    Needs(Vec<Fetch>),
    Failed(LoadError),
}

impl Versioned for Printed {
    const SCHEMA: SchemaHash = blaze_format::schema("blaze v2 cargo tree printed 1");
}

impl TreeAsk {
    fn options(&self) -> Options {
        let edge_kinds: FxHashSet<EdgeKind> = self
            .edges
            .iter()
            .map(|it| match it {
                Edge::Normal => EdgeKind::Dep(DepKind::Normal),
                Edge::Build => EdgeKind::Dep(DepKind::Build),
                Edge::Dev => EdgeKind::Dep(DepKind::Development),
                Edge::Features => EdgeKind::Feature,
            })
            .collect();
        let ask = &self.resolve;
        Options {
            packages: match (ask.workspace, ask.exclude.is_empty(), ask.packages.is_empty()) {
                (false, true, true) => Packages::Default,
                (false, true, false) => Packages::Packages(ask.packages.clone()),
                (true, true, _) => Packages::All(ask.packages.clone()),
                (_, false, _) => Packages::OptOut(ask.exclude.clone()),
            },
            target: Target::from_cli(self.targets.clone()),
            graph_features: edge_kinds.contains(&EdgeKind::Feature),
            edge_kinds,
            invert: self.invert.clone(),
            pkgs_to_prune: self.prune.clone(),
            prefix: match self.prefix {
                TreePrefix::None => Prefix::None,
                TreePrefix::Indent => Prefix::Indent,
                TreePrefix::Depth => Prefix::Depth,
            },
            no_dedupe: self.no_dedupe,
            duplicates: self.duplicates,
            format: self.format.clone(),
            display_depth: match self.depth {
                TreeDepth::Max(depth) => DisplayDepth::MaxDisplayDepth(depth),
                TreeDepth::Workspace => DisplayDepth::Workspace,
            },
            no_proc_macro: self.no_proc_macro,
            public: self.public,
            unicode: self.unicode,
            color: self.color,
        }
    }
}

/// `cargo tree` of the view's workspace.
pub struct Tree;

impl Query for Tree {
    const KIND: QueryKind = QueryKind(0x601c);
    const NAME: &'static str = "cargo tree";
    const PRODUCER: ContentId = producer("blaze v2 cargo tree 1");
    type Key = TreeAsk;
    type Value = Cbor<Printed>;

    fn input(key: &TreeAsk) -> ContentId {
        key_input("blaze v2 cargo tree key 1", key)
    }

    fn execute(cx: &mut Cx<'_>, ask: &TreeAsk) -> Result<Cbor<Printed>, Cancelled> {
        let opts = ask.options();
        if opts.graph_features && opts.duplicates {
            let what = "the `-e features` flag does not support `--duplicates`".to_owned();
            return Ok(Cbor(Printed::Failed(LoadError { what, at: None })));
        }
        let resolve = ResolveAsk { dev_units: ask.edges.contains(&Edge::Dev), ..ask.resolve.clone() };
        Ok(Cbor(match resolve_view(cx, &resolve, |view| print_view(view, &opts))? {
            Resolving::Done((out, warnings)) => Printed::Ready { out, warnings },
            Resolving::Needs(fetches) => Printed::Needs(fetches),
            Resolving::Failed(error) => Printed::Failed(error),
        }))
    }
}

fn print_view(view: &ViewResolved<'_>, opts: &Options) -> Result<(String, Vec<String>), LoadError> {
    let targets: &[String] = match &opts.target {
        Target::Specific(triples) => triples,
        Target::Host | Target::All => &[],
    };
    let platformed = |triple: &str, target: Option<&str>| -> Result<Platformed, LoadError> {
        let cfg = platform_cfg(view.config, triple, view.host, target)?;
        Ok(Platformed { triple: triple.to_owned(), cfg })
    };
    let kinds = Kinds {
        host: platformed(view.host, targets.first().map(String::as_str))?,
        targets: targets.iter().map(|it| platformed(it, Some(it))).collect::<Result<_, _>>()?,
    };
    let input = Input {
        resolve: view.resolve,
        members: view.members,
        member_ids: view.member_ids,
        specs: view.specs,
        cli_features: view.cli_features,
        behavior: view.behavior,
        unification: view.unification,
        packages: view.reads,
    };
    let mut out = String::new();
    let mut warnings = view.warnings.to_vec();
    crate::print_resolved(&input, opts, &kinds, &mut out, &mut |it| warnings.push(it.to_owned()))
        .map_err(|error| load_error(&error, None))?;
    Ok((out, warnings))
}

/// Registers [`Tree`]; the load's queries it reads are blaze-units' (`blaze_units::load::register`).
pub fn register(db: &mut Database) {
    db.register::<Tree>();
}

#[cfg(test)]
mod tests;
