//! What cargo prints of a build, from the daemon's responses to it: status lines and rustc's
//! diagnostics on stderr, the JSON messages on stdout, each unit's warnings and errors counted
//! as cargo's `JobQueue` and `on_stderr_line` count them (`core/compiler/job_queue/mod.rs`,
//! `core/compiler/mod.rs`).
//!
//! Per response: one write to its stream, built in a buffer the printer reuses; a package or a
//! unit seen before costs one Fx lookup and allocates nothing, and one seen first keeps the
//! response's own strings, moved, never copied; a diagnostic is deduplicated by a 64-bit hash of
//! its text, as cargo's `DiagDedupe` does, never a copy of it. Once the printer's tables have
//! grown to the build's size, printing a response allocates nothing.
//!
//! `--message-format=short` prints as `human` does (cargo's `on_stderr_line_inner` takes both
//! one way): the daemon renders each diagnostic in rustc's short form, `file:line:col: level:
//! message`, from the `--message-format` in `Cargo::args`, and cargo's own lines are the same.

use std::{
    hash::{BuildHasher as _, Hash as _, Hasher as _},
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

/// One unit with diagnostics, its counts, and the next unit of the same hash.
struct Seen {
    unit: UnitOf,
    counted: Counted,
    next: u32,
}

/// No next unit in a hash's chain.
const END: u32 = u32::MAX;

/// What the tables are made for before the first response: a build of a few hundred units
/// fits, so none of them grows while it prints.
const PACKAGES: usize = 256;
const DIAGNOSTICS: usize = 1024;

/// cargo's lines of a build, as its responses come.
pub struct Printer<'a, O: Write, E: Write> {
    shown: Shown,
    out: &'a mut O,
    err: &'a mut E,
    /// Each line or batch of lines is built here, then written in one call.
    line: String,
    sources: json::Sources,
    /// The packages a status line was printed for, each the id its `Unit` response brought.
    named: FxHashSet<String>,
    /// Each unit with diagnostics, in the order its first came.
    units: Vec<Seen>,
    /// The first unit in `units` of each hash of a unit ([`unit_hash`]); the rest chain on.
    by_hash: FxHashMap<u64, u32>,
    /// The hash of every diagnostic printed, as cargo's `DiagDedupe` keeps them.
    printed: FxHashSet<u64>,
    built: Option<Built>,
}

/// What tells units apart, hashed: their package, target and mode.
fn unit_hash(unit: &UnitOf) -> u64 {
    let mut hasher = FxBuildHasher.build_hasher();
    unit.package_id.hash(&mut hasher);
    unit.target.name.hash(&mut hasher);
    unit.target.kind.hash(&mut hasher);
    unit.test.hash(&mut hasher);
    hasher.finish()
}

impl<'a, O: Write, E: Write> Printer<'a, O, E> {
    /// `root` is the checkout's, which the files spans name are under.
    pub fn new(shown: Shown, root: &Path, out: &'a mut O, err: &'a mut E) -> Printer<'a, O, E> {
        let fx = FxBuildHasher;
        Printer {
            shown,
            out,
            err,
            line: String::with_capacity(16 * 1024),
            sources: json::Sources::new(root),
            named: FxHashSet::with_capacity_and_hasher(PACKAGES, fx),
            units: Vec::with_capacity(PACKAGES),
            by_hash: FxHashMap::with_capacity_and_hasher(PACKAGES, fx),
            printed: FxHashSet::with_capacity_and_hasher(DIAGNOSTICS, fx),
            built: None,
        }
    }

    /// What the build placed, once it said, taken out of the printer.
    pub fn take_artifacts(&mut self) -> Vec<Artifact> {
        // alloc: none; an empty `Vec` holds no memory.
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
                if !self.shown.quiet {
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
                self.named.insert(unit.package_id);
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
            Response::Diagnostic(diagnostic) => self.diagnostic(diagnostic)?,
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
                let at = self.unit(unit);
                let seen = &mut self.units[at];
                if seen.counted.ended {
                    return Ok(());
                }
                seen.counted.ended = true;
                self.line.clear();
                summary(&mut self.line, &seen.unit, &seen.counted, self.shown);
                self.err.write_all(self.line.as_bytes())?;
            }
            // blaze's own, for the proof harness: what the request did.
            Response::Work(worked) if self.shown.format.json() => {
                // alloc: one line a request, at its end.
                self.out.write_all(blaze_ui::api::work_json(&worked).as_bytes())?;
            }
            Response::Error(error) => {
                self.line.clear();
                say::error_into(&mut self.line, &error.what, self.shown.color);
                self.err.write_all(self.line.as_bytes())?;
            }
            _ => {}
        }
        Ok(())
    }

    fn diagnostic(&mut self, mut diagnostic: Diagnostic) -> io::Result<()> {
        let message = &diagnostic.message;
        // rustc's own summary, which cargo skips in every format and replaces with its own.
        if message.starts_with("aborting due to") || message.ends_with("warning emitted") || message.ends_with("warnings emitted") {
            return Ok(());
        }
        let renders = self.shown.format.renders();
        if let Format::Json { ansi, render_diagnostics: false, .. } = self.shown.format
            && let Some(unit) = &diagnostic.unit
        {
            self.line.clear();
            json::compiler_message(&mut self.line, unit, &diagnostic, ansi, &mut self.sources);
            self.line.push('\n');
            self.out.write_all(self.line.as_bytes())?;
        }
        // cargo takes one newline off the end, then writes the text and a newline.
        let rendered = diagnostic.rendered.strip_suffix('\n').unwrap_or(&diagnostic.rendered);
        let printed = match renders {
            true => self.printed.insert(FxBuildHasher.hash_one(rendered)),
            false => true,
        };
        if renders && printed {
            self.line.clear();
            self.line.push_str(rendered);
            self.line.push('\n');
            self.err.write_all(self.line.as_bytes())?;
        }
        let Some(unit) = diagnostic.unit.take() else { return Ok(()) };
        let fixable = fixable(&diagnostic);
        let at = self.unit(unit);
        let counted = &mut self.units[at].counted;
        match diagnostic.level {
            Level::Warning => {
                counted.warnings += 1;
                if renders {
                    counted.warned.total += 1;
                    match printed {
                        false => counted.warned.duplicates += 1,
                        true if fixable => {
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

    /// The index of `unit` in `units`, which keeps it the first time it comes.
    fn unit(&mut self, unit: UnitOf) -> usize {
        let hash = unit_hash(&unit);
        let head = self.by_hash.get(&hash).copied().unwrap_or(END);
        let mut at = head;
        while at != END {
            let seen = &self.units[at as usize];
            if seen.unit == unit {
                return at as usize;
            }
            at = seen.next;
        }
        let at = self.units.len();
        let counted = Counted { warned: say::Warned { fixable: Some(0), ..say::Warned::default() }, ..Counted::default() };
        self.units.push(Seen { unit, counted, next: head });
        self.by_hash.insert(hash, at as u32);
        at
    }

    /// The build is over (`ok` is its `Done`'s), `took` after it began: each unit's summary,
    /// `Finished` and `build-finished`.
    pub fn finish(&mut self, ok: bool, took: Duration) -> io::Result<()> {
        let color = self.shown.color;
        self.line.clear();
        for seen in &self.units {
            if !seen.counted.ended {
                summary(&mut self.line, &seen.unit, &seen.counted, self.shown);
            }
        }
        if ok
            && !self.shown.quiet
            && let Some(built) = &self.built
        {
            say::status_word(&mut self.line, "Finished", color);
            say::finished_into(&mut self.line, &built.profile, took);
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

/// A unit's "generated N warnings" (`JobQueue::report_warning_count` as the unit finishes, a
/// failed one too, silent under `-q`), then its "could not compile" (the unit's error, which
/// `-q` does not silence), into `line`.
fn summary(line: &mut String, unit: &UnitOf, counted: &Counted, shown: Shown) {
    let package = say::package_name(&unit.package_id);
    let local = unit.package_id.starts_with("path+");
    if !shown.quiet && counted.warned.total > 0 {
        say::leveled_with(line, say::WARN, "warning", shown.color, |line| {
            say::generated_into(line, package, &unit.target, unit.test, local, counted.warned);
        });
    }
    if counted.errors > 0 {
        say::leveled_with(line, say::ERROR, "error", shown.color, |line| {
            say::could_not_compile_into(line, package, &unit.target, unit.test, counted.errors, counted.warnings);
        });
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
