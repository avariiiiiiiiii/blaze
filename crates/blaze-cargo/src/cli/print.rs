//! What cargo prints of a build, from the daemon's responses to it: status lines and rustc's
//! diagnostics on stderr, the JSON messages on stdout, each unit's warnings and errors counted
//! as cargo's `JobQueue` and `on_stderr_line` count them (`core/compiler/job_queue/mod.rs`,
//! `core/compiler/mod.rs`).

use std::{
    collections::HashSet,
    io::{self, Write},
    path::Path,
    time::Duration,
};

use blaze_api::{Artifact, Diagnostic, Level, Response, UnitOf, Verb};

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
}

/// cargo's lines of a build, as its responses come.
pub struct Printer<'a> {
    shown: Shown,
    out: &'a mut dyn Write,
    err: &'a mut dyn Write,
    sources: json::Sources,
    /// The packages a status line was printed for.
    named: HashSet<String>,
    /// Each unit with diagnostics, in the order its first came.
    units: Vec<(UnitOf, Counted)>,
    /// Every diagnostic printed, as cargo's `DiagDedupe` keeps them.
    printed: HashSet<String>,
    built: Option<(Vec<Artifact>, String)>,
}

impl<'a> Printer<'a> {
    /// `root` is the checkout's, which the files spans name are under.
    pub fn new(shown: Shown, root: &Path, out: &'a mut dyn Write, err: &'a mut dyn Write) -> Printer<'a> {
        Printer {
            shown,
            out,
            err,
            sources: json::Sources::new(root),
            named: HashSet::new(),
            units: Vec::new(),
            printed: HashSet::new(),
            built: None,
        }
    }

    /// What the build placed, once it said.
    pub fn artifacts(&self) -> &[Artifact] {
        self.built.as_ref().map_or(&[], |(artifacts, _)| artifacts.as_slice())
    }

    pub fn err(&mut self) -> &mut dyn Write {
        &mut *self.err
    }

    pub fn shown(&self) -> Shown {
        self.shown
    }

    /// One response of the build.
    pub fn on(&mut self, response: &Response) -> io::Result<()> {
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
                if shows && self.named.insert(unit.package_id.clone()) && !self.shown.quiet {
                    let package = say::package(&unit.name, &unit.version, unit.source.as_deref());
                    self.err.write_all(say::status(word, &package, self.shown.color).as_bytes())?;
                }
            }
            Response::Diagnostic(diagnostic) => self.diagnostic(diagnostic)?,
            Response::BuildScript(ran) if self.shown.format.json() => {
                writeln!(self.out, "{}", json::build_script(ran))?;
            }
            Response::Built { artifacts, profile } => {
                if self.shown.format.json() {
                    for artifact in artifacts {
                        writeln!(self.out, "{}", json::artifact(artifact))?;
                    }
                }
                self.built = Some((artifacts.clone(), profile.clone()));
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
        if let Format::Json { ansi, render_diagnostics: false, .. } = self.shown.format {
            if let Some(unit) = &diagnostic.unit {
                writeln!(self.out, "{}", json::compiler_message(unit, diagnostic, ansi, &mut self.sources))?;
            }
        }
        let printed = match renders {
            true => self.printed.insert(diagnostic.rendered.clone()),
            false => true,
        };
        if renders && printed {
            self.err.write_all(diagnostic.rendered.as_bytes())?;
            if !diagnostic.rendered.ends_with('\n') {
                self.err.write_all(b"\n")?;
            }
        }
        if summary {
            return Ok(());
        }
        let Some(unit) = &diagnostic.unit else { return Ok(()) };
        let at = match self.units.iter().position(|(it, _)| it == unit) {
            Some(at) => at,
            None => {
                self.units.push((unit.clone(), Counted { warned: say::Warned { fixable: Some(0), ..Default::default() }, ..Default::default() }));
                self.units.len() - 1
            }
        };
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

    /// The build is over (`ok` is its `Done`'s), `took` after it began: each unit's summary,
    /// `Finished` and `build-finished`.
    pub fn finish(&mut self, ok: bool, took: Duration) -> io::Result<()> {
        let color = self.shown.color;
        for (unit, counted) in &self.units {
            let package = say::package_name(&unit.package_id);
            if counted.errors > 0 {
                let said = say::could_not_compile(package, &unit.target, unit.test, counted.errors, counted.warnings);
                self.err.write_all(say::error(&said, color).as_bytes())?;
                continue;
            }
            let local = unit.package_id.starts_with("path+");
            if let Some(said) = say::generated(package, &unit.target, unit.test, local, counted.warned)
                && !self.shown.quiet
            {
                self.err.write_all(say::warning(&said, color).as_bytes())?;
            }
        }
        if ok
            && !self.shown.quiet
            && let Some((_, profile)) = &self.built
        {
            self.err.write_all(say::status("Finished", &say::finished(profile, took), color).as_bytes())?;
        }
        if self.shown.format.json() {
            writeln!(self.out, "{}", json::build_finished(ok))?;
        }
        self.out.flush()?;
        self.err.flush()
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
