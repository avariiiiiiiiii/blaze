//! What cargo prints of a build, from the daemon's responses to it: status lines and rustc's
//! diagnostics on stderr, the JSON messages on stdout, each unit's warnings and errors counted
//! as cargo's `JobQueue` and `on_stderr_line` count them (`core/compiler/job_queue/mod.rs`,
//! `core/compiler/mod.rs`).
//!
//! Per response: one write to its stream, built in a buffer the printer reuses; a package or a
//! unit seen before costs one Fx lookup by `&str` and allocates nothing; a diagnostic is
//! deduplicated by a 64-bit hash of its text, as cargo's `DiagDedupe` does, never a copy of it.

use std::{
    hash::BuildHasher as _,
    io::{self, Write},
    path::Path,
    time::Duration,
};

use blaze_api::{Artifact, Diagnostic, FetchVerb, Level, Response, UnitOf, Verb};
use rustc_hash::{FxBuildHasher, FxHashMap, FxHashSet};

use super::{json, say};

/// cargo's `MessageFormat`.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Format {
    #[default]
    Human,
    Short,
    Json {
        short: bool,
        ansi: bool,
        render_diagnostics: bool,
    },
}

impl Format {
    pub fn json(self) -> bool {
        matches!(self, Format::Json { .. })
    }

    /// Whether diagnostics are printed as rustc rendered them, on stderr.
    fn renders(self) -> bool {
        !matches!(self, Format::Json { render_diagnostics: false, .. })
    }
}

/// cargo's `ArgMatchesExt::message_format`: each `--message-format`, split at commas.
pub fn format(values: &[String]) -> Result<Format, String> {
    const TWO: &str = "cannot specify two kinds of `message-format` arguments";
    let json = Format::Json { short: false, ansi: false, render_diagnostics: false };
    let mut format = None;
    for value in values.iter().flat_map(|it| it.split(',')) {
        let value = value.to_ascii_lowercase();
        let only = |format: Option<Format>, it: Format| match format {
            Some(_) => Err(TWO.to_owned()),
            None => Ok(Some(it)),
        };
        let json_with = |format: &mut Option<Format>, set: fn(&mut bool, &mut bool, &mut bool)| {
            let mut it = format.unwrap_or(json);
            match &mut it {
                Format::Json { short, ansi, render_diagnostics } => set(short, ansi, render_diagnostics),
                _ => return Err(TWO.to_owned()),
            }
            *format = Some(it);
            Ok(())
        };
        match value.as_str() {
            "json" => format = only(format, json)?,
            "human" => format = only(format, Format::Human)?,
            "short" => format = only(format, Format::Short)?,
            "json-render-diagnostics" => json_with(&mut format, |_, _, it| *it = true)?,
            "json-diagnostic-short" => json_with(&mut format, |it, _, _| *it = true)?,
            "json-diagnostic-rendered-ansi" => json_with(&mut format, |_, it, _| *it = true)?,
            other => return Err(format!("invalid message format specifier: `{other}`")),
        }
    }
    Ok(format.unwrap_or_default())
}

/// How the command was asked to speak.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Shown {
    pub format: Format,
    pub quiet: bool,
    pub verbose: u32,
    /// stderr is colored.
    pub color: bool,
}

/// What one unit's diagnostics came to.
#[derive(Default)]
struct Counted {
    errors: u32,
    /// Every warning, its duplicates too (cargo's `errors_seen`, `warnings_seen`).
    warnings: u32,
    warned: say::Warned,
    /// Its summary was printed when it ended ([`Response::UnitEnded`]), as cargo prints it.
    ended: bool,
}

/// What the build placed, once it said ([`Response::Built`]).
struct Built {
    artifacts: Vec<Artifact>,
    profile: String,
    members: u32,
}

/// cargo's lines of a build, as its responses come.
pub struct Printer<'a, O: Write, E: Write> {
    shown: Shown,
    out: &'a mut O,
    err: &'a mut E,
    /// Each line or batch of lines is built here, then written in one call.
    line: String,
    sources: json::Sources,
    /// The packages a status line was printed for.
    named: FxHashSet<Box<str>>,
    /// Each unit with diagnostics, in the order its first came, and its counts.
    units: Vec<(UnitOf, Counted)>,
    /// The units of each package, by index into `units`: a package has few.
    of_package: FxHashMap<Box<str>, Vec<u32>>,
    /// The hash of every diagnostic printed, as cargo's `DiagDedupe` keeps them.
    printed: FxHashSet<u64>,
    built: Option<Built>,
}

impl<'a, O: Write, E: Write> Printer<'a, O, E> {
    /// `root` is the checkout's, which the files spans name are under.
    pub fn new(shown: Shown, root: &Path, out: &'a mut O, err: &'a mut E) -> Printer<'a, O, E> {
        Printer {
            shown,
            out,
            err,
            line: String::with_capacity(4096),
            sources: json::Sources::new(root),
            named: FxHashSet::default(),
            units: Vec::new(),
            of_package: FxHashMap::default(),
            printed: FxHashSet::default(),
            built: None,
        }
    }

    /// What the build placed, once it said, taken out of the printer.
    pub fn take_artifacts(&mut self) -> Vec<Artifact> {
        self.built.as_mut().map_or_else(Vec::new, |built| std::mem::take(&mut built.artifacts))
    }

    /// The workspace's member count the build said; 1 before it did.
    pub fn members(&self) -> u32 {
        self.built.as_ref().map_or(1, |built| built.members)
    }

    pub fn err(&mut self) -> &mut E {
        self.err
    }

    /// stdout and stderr, for what follows the build.
    pub fn streams(&mut self) -> (&mut O, &mut E) {
        (self.out, self.err)
    }

    pub fn shown(&self) -> Shown {
        self.shown
    }

    /// One response of the build.
    pub fn on(&mut self, response: Response) -> io::Result<()> {
        match response {
            Response::Unit(unit) => {
                let word = match unit.verb {
                    Verb::Compiling => "Compiling",
                    Verb::Checking => "Checking",
                    Verb::Fresh => "Fresh",
                };
                // A fresh unit names its package only where it is shown: cargo marks one fresh
                // only as the package's last unit, and the units of a package come in no order
                // the client knows.
                let shows = match unit.verb {
                    Verb::Fresh => self.shown.verbose > 0,
                    Verb::Compiling | Verb::Checking => true,
                };
                if !shows || self.named.contains(unit.package_id.as_str()) {
                    return Ok(());
                }
                self.named.insert(unit.package_id.into_boxed_str());
                if self.shown.quiet {
                    return Ok(());
                }
                self.line.clear();
                say::status_word(&mut self.line, word, self.shown.color);
                self.line.push_str(&unit.name);
                self.line.push_str(" v");
                self.line.push_str(&unit.version);
                if let Some(source) = &unit.source {
                    self.line.push(' ');
                    self.line.push_str(source);
                }
                self.line.push('\n');
                self.err.write_all(self.line.as_bytes())?;
            }
            // cargo's `Updating crates.io index`, `Downloaded serde v1.0.219`.
            Response::Fetched { verb, what } if !self.shown.quiet => {
                let word = match verb {
                    FetchVerb::Updating => "Updating",
                    FetchVerb::Downloaded => "Downloaded",
                };
                self.line.clear();
                say::status_word(&mut self.line, word, self.shown.color);
                self.line.push_str(&what);
                self.line.push('\n');
                self.err.write_all(self.line.as_bytes())?;
            }
            Response::Diagnostic(diagnostic) => self.diagnostic(&diagnostic)?,
            Response::BuildScript(ran) if self.shown.format.json() => {
                self.line.clear();
                json::build_script(&mut self.line, &ran);
                self.line.push('\n');
                self.out.write_all(self.line.as_bytes())?;
            }
            Response::Built { artifacts, profile, members } => {
                if self.shown.format.json() {
                    self.line.clear();
                    for artifact in &artifacts {
                        json::artifact(&mut self.line, artifact);
                        self.line.push('\n');
                    }
                    self.out.write_all(self.line.as_bytes())?;
                }
                self.built = Some(Built { artifacts, profile, members });
            }
            // cargo prints a unit's "generated N warnings" or "could not compile" as it ends.
            Response::UnitEnded(unit) => {
                let at = self.unit(&unit);
                let (unit, counted) = &mut self.units[at];
                if counted.ended {
                    return Ok(());
                }
                counted.ended = true;
                self.line.clear();
                summary(&mut self.line, unit, counted, self.shown);
                self.err.write_all(self.line.as_bytes())?;
            }
            // blaze's own, for the proof harness: what the request did.
            Response::Work(worked) if self.shown.format.json() => {
                self.out.write_all(blaze_ui::api::work_json(&worked).as_bytes())?;
            }
            Response::Error(error) => {
                self.err.write_all(say::error(&error.what, self.shown.color).as_bytes())?;
            }
            _ => {}
        }
        Ok(())
    }

    fn diagnostic(&mut self, diagnostic: &Diagnostic) -> io::Result<()> {
        let message = &diagnostic.message;
        // rustc's own summary, which cargo replaces with its own.
        let summary = message.starts_with("aborting due to")
            || message.ends_with("warning emitted")
            || message.ends_with("warnings emitted");
        let renders = self.shown.format.renders();
        if summary && renders {
            return Ok(());
        }
        if let Format::Json { ansi, render_diagnostics: false, .. } = self.shown.format
            && let Some(unit) = &diagnostic.unit
        {
            self.line.clear();
            json::compiler_message(&mut self.line, unit, diagnostic, ansi, &mut self.sources);
            self.line.push('\n');
            self.out.write_all(self.line.as_bytes())?;
        }
        let printed = match renders {
            true => self.printed.insert(FxBuildHasher.hash_one(diagnostic.rendered.as_str())),
            false => true,
        };
        if renders && printed {
            match diagnostic.rendered.ends_with('\n') {
                true => self.err.write_all(diagnostic.rendered.as_bytes())?,
                false => {
                    self.line.clear();
                    self.line.push_str(&diagnostic.rendered);
                    self.line.push('\n');
                    self.err.write_all(self.line.as_bytes())?;
                }
            }
        }
        if summary {
            return Ok(());
        }
        let Some(unit) = &diagnostic.unit else { return Ok(()) };
        let at = self.unit(unit);
        let counted = &mut self.units[at].1;
        match diagnostic.level {
            Level::Warning => {
                counted.warnings += 1;
                if renders {
                    counted.warned.total += 1;
                    match printed {
                        false => counted.warned.duplicates += 1,
                        true if fixable(diagnostic) => {
                            counted.warned.fixable = counted.warned.fixable.map(|it| it + 1);
                        }
                        true => {}
                    }
                }
            }
            Level::Error => {
                counted.errors += 1;
                counted.warned.fixable = None;
            }
            Level::Note | Level::Help | Level::FailureNote => {}
        }
        Ok(())
    }

    /// The index of `unit` in `units`, added the first time it comes.
    fn unit(&mut self, unit: &UnitOf) -> usize {
        if let Some(indices) = self.of_package.get(unit.package_id.as_str())
            && let Some(&at) = indices.iter().find(|&&at| self.units[at as usize].0 == *unit)
        {
            return at as usize;
        }
        let at = self.units.len();
        let counted = Counted { warned: say::Warned { fixable: Some(0), ..say::Warned::default() }, ..Counted::default() };
        self.units.push((unit.clone(), counted));
        match self.of_package.get_mut(unit.package_id.as_str()) {
            Some(indices) => indices.push(at as u32),
            None => {
                self.of_package.insert(unit.package_id.as_str().into(), vec![at as u32]);
            }
        }
        at
    }

    /// The build is over (`ok` is its `Done`'s), `took` after it began: each unit's summary,
    /// `Finished` and `build-finished`.
    pub fn finish(&mut self, ok: bool, took: Duration) -> io::Result<()> {
        let color = self.shown.color;
        self.line.clear();
        for (unit, counted) in &self.units {
            if !counted.ended {
                summary(&mut self.line, unit, counted, self.shown);
            }
        }
        if ok
            && !self.shown.quiet
            && let Some(built) = &self.built
        {
            say::status_word(&mut self.line, "Finished", color);
            self.line.push_str(&say::finished(&built.profile, took));
            self.line.push('\n');
        }
        self.err.write_all(self.line.as_bytes())?;
        if self.shown.format.json() {
            self.line.clear();
            json::build_finished(&mut self.line, ok);
            self.line.push('\n');
            self.out.write_all(self.line.as_bytes())?;
        }
        self.out.flush()?;
        self.err.flush()
    }
}

/// A unit's "could not compile" or "generated N warnings", into `line`.
fn summary(line: &mut String, unit: &UnitOf, counted: &Counted, shown: Shown) {
    let package = say::package_name(&unit.package_id);
    if counted.errors > 0 {
        let said = say::could_not_compile(package, &unit.target, unit.test, counted.errors, counted.warnings);
        line.push_str(&say::error(&said, shown.color));
        return;
    }
    let local = unit.package_id.starts_with("path+");
    if let Some(said) = say::generated(package, &unit.target, unit.test, local, counted.warned)
        && !shown.quiet
    {
        line.push_str(&say::warning(&said, shown.color));
    }
}

/// cargo's `machine_applicable`: a child of the diagnostic suggests a fix rustfix may apply.
fn fixable(diagnostic: &Diagnostic) -> bool {
    diagnostic
        .children
        .iter()
        .any(|child| child.spans.iter().any(|span| span.applicability.as_deref() == Some("MachineApplicable")))
}

#[cfg(test)]
mod tests;
