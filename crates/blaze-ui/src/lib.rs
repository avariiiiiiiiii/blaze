//! How a build is shown, from its records ([`blaze_protocol::events`]) alone: a renderer
//! is a consumer of what the daemon publishes, so the same records give the same text
//! whichever terminal attaches, and a recorded build replays into it.
//!
//! - [`Live`], on a terminal: a region at the bottom that is redrawn in place (not a full
//!   screen), with the step under way, the crates being built and their counts of
//!   functions, each with how long it has taken; what is done scrolls above it as lines
//!   that stay. It is never left unchanged for more than a tick. At the end it is cleared
//!   and one summary line takes its place.
//! - [`Plain`], elsewhere (CI, a pipe): one line per step and crate, as cargo writes them,
//!   and a line every [`HEARTBEAT_NS`] that nothing else was said.

mod live;
mod plain;
#[cfg(test)]
mod tests;

pub use live::Live;
pub use plain::Plain;

use blaze_protocol::events::{Cause, CrateState, Event, Fns, Level, Record, Step, Summary};

/// The longest a plain rendering says nothing while a step is under way.
pub const HEARTBEAT_NS: u64 = 15_000_000_000;

/// A build as its records so far have it.
#[derive(Default, Debug)]
pub struct Build {
    pub command: Option<String>,
    /// Every step begun, in order.
    pub steps: Vec<Stepped>,
    /// Every crate the build said anything of, in the order it first did.
    pub crates: Vec<Crated>,
    /// What was compiled again and why: name, crate, cause.
    pub rebuilt: Vec<(String, String, Cause)>,
    /// How many more were compiled again than `rebuilt` names.
    pub unlisted: u64,
    pub refused: Vec<(String, String)>,
    pub finished: Option<Finished>,
    /// The time of the last record.
    pub last_ns: u64,
}

#[derive(Debug, Clone)]
pub struct Stepped {
    pub step: Step,
    pub of: Option<String>,
    pub began_ns: u64,
    pub ended_ns: Option<u64>,
}

impl Stepped {
    /// What the step is of: its crate or program, or the step's own word for the whole.
    pub fn what(&self) -> &str {
        self.of.as_deref().unwrap_or(self.step.whole())
    }
}

#[derive(Debug, Clone)]
pub struct Crated {
    pub name: String,
    pub state: CrateState,
    pub fns: Fns,
    pub began_ns: u64,
    pub ended_ns: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct Finished {
    pub ok: bool,
    pub summary: Summary,
    pub at_ns: u64,
}

/// What a record changed, for a renderer to say.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    Nothing,
    /// The step at this index of [`Build::steps`] began, or ended.
    Began(usize),
    Ended(usize),
    /// The crate at this index of [`Build::crates`] is in another state, or further.
    Crate {
        at: usize,
        moved: bool,
    },
    Said(Level, String),
    Refused(usize),
    Rebuilt(usize),
    Finished,
}

impl Build {
    pub fn apply(&mut self, record: &Record) -> Change {
        let at = record.at_ns;
        self.last_ns = self.last_ns.max(at);
        match &record.event {
            Event::Began { command, .. } => {
                self.command = Some(command.clone());
                Change::Nothing
            }
            Event::Started { step, of } => {
                let of = of.clone();
                self.steps.push(Stepped { step: *step, of, began_ns: at, ended_ns: None });
                Change::Began(self.steps.len() - 1)
            }
            Event::Ended { step, of } => {
                let open = self
                    .steps
                    .iter()
                    .rposition(|it| it.step == *step && it.of == *of && it.ended_ns.is_none());
                match open {
                    Some(index) => {
                        self.steps[index].ended_ns = Some(at);
                        Change::Ended(index)
                    }
                    None => Change::Nothing,
                }
            }
            Event::Crate { name, state, fns } => {
                let done =
                    matches!(state, CrateState::UpToDate | CrateState::Built | CrateState::Failed);
                let (index, new) = match self.crates.iter().position(|it| it.name == *name) {
                    Some(index) => (index, false),
                    None => {
                        self.crates.push(Crated {
                            name: name.clone(),
                            state: *state,
                            fns: *fns,
                            began_ns: at,
                            ended_ns: None,
                        });
                        (self.crates.len() - 1, true)
                    }
                };
                let krate = &mut self.crates[index];
                let moved = new || krate.state != *state || krate.ended_ns.is_none() && done;
                if krate.state == CrateState::Waiting && *state != CrateState::Waiting {
                    krate.began_ns = at;
                }
                krate.state = *state;
                krate.fns = *fns;
                // Compiled again in the same build: a patch that is built in full after all.
                if *state == CrateState::Running {
                    krate.ended_ns = None;
                }
                if done && krate.ended_ns.is_none() {
                    krate.ended_ns = Some(at);
                }
                Change::Crate { at: index, moved }
            }
            Event::Rebuilt { name, krate, cause } => {
                self.rebuilt.push((name.clone(), krate.clone(), cause.clone()));
                Change::Rebuilt(self.rebuilt.len() - 1)
            }
            Event::Unlisted { count } => {
                self.unlisted += count;
                Change::Nothing
            }
            Event::Refused { name, why } => {
                self.refused.push((name.clone(), why.clone()));
                Change::Refused(self.refused.len() - 1)
            }
            Event::Said { level, text } => Change::Said(*level, text.clone()),
            Event::Finished { ok, summary } => {
                self.finished = Some(Finished { ok: *ok, summary: summary.clone(), at_ns: at });
                Change::Finished
            }
        }
    }

    /// The steps under way, the one begun last at the end.
    pub fn running(&self) -> impl Iterator<Item = &Stepped> {
        self.steps.iter().filter(|it| it.ended_ns.is_none())
    }

    /// What the records so far come to, for the last line of a build that took `took_ns`:
    /// the first few rebuilt, and a count of the rest.
    pub fn summary(&self, took_ns: u64, error: Option<String>) -> Summary {
        const LISTED: usize = 8;
        let rebuilt = self.rebuilt.iter().take(LISTED);
        let rebuilt = rebuilt.map(|(name, _, cause)| (name.clone(), cause.clone())).collect();
        let more = self.rebuilt.len().saturating_sub(LISTED) as u64 + self.unlisted;
        let crates = self.crates.len() as u64;
        Summary { took_ns, crates, fns: self.fns(), rebuilt, more, error }
    }

    /// The functions of every crate together.
    pub fn fns(&self) -> Fns {
        self.crates.iter().fold(Fns::default(), |all, it| Fns {
            total: all.total + it.fns.total,
            done: all.done + it.fns.done,
            kept: all.kept + it.fns.kept,
        })
    }
}

/// A duration as a person reads it: `0.21s`, `33.2s`, `1m17s`.
pub fn took(ns: u64) -> String {
    let ms = ns / 1_000_000;
    match ms {
        0..=9_999 => format!("{}.{:02}s", ms / 1000, ms % 1000 / 10),
        10_000..=59_999 => format!("{}.{}s", ms / 1000, ms % 1000 / 100),
        _ => format!("{}m{:02}s", ms / 60_000, ms % 60_000 / 1000),
    }
}

/// Whether a build did nothing and failed at nothing: it has no last line of its own, as cargo
/// says nothing of a fresh build but its `Finished`.
pub fn fresh(finished: &Finished) -> bool {
    let summary = &finished.summary;
    finished.ok && summary.rebuilt.is_empty() && summary.fns.compiled() == 0
}

/// The last line of a build: what it rebuilt and why, or why it failed, and how long it took.
/// Not cargo's `Finished`, which `blaze` says after it.
pub fn summary_line(finished: &Finished, color: bool) -> String {
    let summary = &finished.summary;
    let took = took(if summary.took_ns > 0 { summary.took_ns } else { finished.at_ns });
    if !finished.ok {
        let why = summary.error.as_deref().unwrap_or("the build failed");
        return format!("{}: {why} (after {took})", paint(RED, "error", color));
    }
    let compiled = summary.fns.compiled();
    let (word, what) = if !summary.rebuilt.is_empty() {
        let listed = summary.rebuilt.len() as u64 + summary.more;
        let mut said = format!("{listed} {}: {}", plural(listed, "fn"), causes(&summary.rebuilt));
        if summary.more > 0 {
            said.push_str(&format!(" and {} more", summary.more));
        }
        ("Rebuilt", said)
    } else if compiled > 0 {
        let kept = summary.fns.kept;
        let crates = summary.crates;
        let said = format!(
            "{compiled} {} in {crates} {}, {kept} kept",
            plural(compiled, "fn"),
            plural(crates, "crate")
        );
        ("Compiled", said)
    } else {
        ("Fresh", "nothing to rebuild".to_owned())
    };
    format!("{} {what}; {took}", paint(GREEN, &format!("{word:>12}"), color))
}

/// The word a step is said done with.
fn done_word(step: Step) -> &'static str {
    match step {
        Step::LoadWorkspace => "Loaded",
        Step::Fetch => "Fetched",
        Step::Resolve => "Resolved",
        Step::FrontEnd | Step::Check => "Checked",
        Step::Codegen | Step::Std => "Compiled",
        Step::Link => "Linked",
        Step::Run => "Ran",
        Step::Test => "Tested",
    }
}

/// A step or crate done sooner than this is not said done in lines that stay, but for a
/// crate built on a terminal: cargo says each.
const NEWS_NS: u64 = 500_000_000;

/// The lines `change` adds that stay: on a terminal (`live`) above the region, elsewhere all
/// there is.
fn staying(build: &Build, change: &Change, color: bool, live: bool) -> Vec<String> {
    let status =
        |word: &str, rest: &str| format!("{} {rest}", paint(GREEN, &format!("{word:>12}"), color));
    match change {
        Change::Began(at) if !live => {
            let stepped = &build.steps[*at];
            vec![status(stepped.step.word(), stepped.what())]
        }
        Change::Ended(at) => {
            let stepped = &build.steps[*at];
            let took_ns = stepped.ended_ns.unwrap_or(stepped.began_ns) - stepped.began_ns;
            // The crates say how their compiling went.
            let said_by_crates = stepped.step == Step::Codegen && !build.crates.is_empty();
            match took_ns >= NEWS_NS && !said_by_crates {
                true => {
                    let rest = format!("{} in {}", stepped.what(), took(took_ns));
                    vec![status(done_word(stepped.step), &rest)]
                }
                false => Vec::new(),
            }
        }
        Change::Crate { at, moved: true } => {
            let krate = &build.crates[*at];
            let took_ns = krate.ended_ns.unwrap_or(krate.began_ns) - krate.began_ns;
            match krate.state {
                // The step of compiling it said so already.
                CrateState::Running
                    if !live
                        && !build.running().any(|it| {
                            it.step == Step::Codegen && it.of.as_deref() == Some(&krate.name)
                        }) =>
                {
                    vec![status("Compiling", &krate.name)]
                }
                CrateState::Built if live || took_ns >= NEWS_NS => {
                    let fns = match krate.fns.total {
                        0 => String::new(),
                        _ => format!(
                            ": {} fns compiled, {} kept",
                            krate.fns.compiled(),
                            krate.fns.kept
                        ),
                    };
                    vec![status("Compiled", &format!("{}{fns} in {}", krate.name, took(took_ns)))]
                }
                _ => Vec::new(),
            }
        }
        Change::Said(level, text) => vec![said_line(*level, text, color)],
        Change::Refused(at) => {
            let (name, why) = &build.refused[*at];
            vec![said_line(Level::Error, &format!("blaze does not build `{name}`: {why}"), color)]
        }
        Change::Finished if !live => build
            .finished
            .iter()
            .filter(|it| !fresh(it))
            .map(|it| summary_line(it, color))
            .collect(),
        _ => Vec::new(),
    }
}

/// `render (body edited), draw, tick (calls render)`: names with the same cause one after
/// another say it once, after the last of them.
fn causes(rebuilt: &[(String, Cause)]) -> String {
    let mut said = String::new();
    for (at, (name, cause)) in rebuilt.iter().enumerate() {
        if at > 0 {
            said.push_str(", ");
        }
        said.push_str(name);
        if rebuilt.get(at + 1).is_none_or(|next| next.1 != *cause) {
            said.push_str(&format!(" ({})", cause.short()));
        }
    }
    said
}

fn plural(n: u64, word: &str) -> String {
    if n == 1 { word.to_owned() } else { format!("{word}s") }
}

const GREEN: &str = "\x1b[1m\x1b[92m";
const CYAN: &str = "\x1b[1m\x1b[96m";
const YELLOW: &str = "\x1b[1m\x1b[93m";
const RED: &str = "\x1b[1m\x1b[91m";
const DIM: &str = "\x1b[2m";
const RESET: &str = "\x1b[0m";

fn paint(color: &str, text: &str, colored: bool) -> String {
    if colored { format!("{color}{text}{RESET}") } else { text.to_owned() }
}

/// A line of `level`, as cargo writes them.
fn said_line(level: Level, text: &str, color: bool) -> String {
    let (word, color_of) = match level {
        Level::Note => ("note", GREEN),
        Level::Warning => ("warning", YELLOW),
        Level::Error => ("error", RED),
    };
    format!("{}: {text}", paint(color_of, word, color))
}
