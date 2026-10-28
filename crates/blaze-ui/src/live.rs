//! The terminal's rendering: a region redrawn in place below the lines that stay.

use blaze_protocol::events::{CrateState, Record};

use crate::{Build, CYAN, Change, DIM, GREEN, fresh, paint, staying, summary_line, took};

/// The most crates the region lists by name; the rest under way are counted.
const SHOWN: usize = 6;

/// Clears the region and draws it again as records come and time passes. What it returns is
/// written to the terminal as it is.
pub struct Live {
    build: Build,
    /// How many lines of the region are on the screen.
    drawn: usize,
    width: usize,
    color: bool,
}

impl Live {
    pub fn new(width: usize, color: bool) -> Live {
        Live { build: Build::default(), drawn: 0, width: width.max(20), color }
    }

    pub fn build(&self) -> &Build {
        &self.build
    }

    pub fn resize(&mut self, width: usize) {
        self.width = width.max(20);
    }

    /// What to write for `record` at `now_ns`: the lines it adds that stay, and the region
    /// again below them; at the build's end, the summary in the region's place.
    pub fn record(&mut self, record: &Record, now_ns: u64) -> String {
        let change = self.build.apply(record);
        let lines = staying(&self.build, &change, self.color, true);
        match change {
            Change::Finished => self.finish_with(&lines),
            _ => self.redraw(&lines, now_ns),
        }
    }

    /// The region drawn again for the time that passed.
    pub fn tick(&mut self, now_ns: u64) -> String {
        self.redraw(&[], now_ns)
    }

    /// The region cleared, and the summary in its place when the build finished.
    pub fn finish(&mut self) -> String {
        self.finish_with(&[])
    }

    fn finish_with(&mut self, lines: &[String]) -> String {
        let mut out = self.cleared();
        for line in lines {
            out.push_str(line);
            out.push('\n');
        }
        if let Some(finished) = self.build.finished.as_ref().filter(|it| !fresh(it)) {
            out.push_str(&summary_line(finished, self.color));
            out.push('\n');
        }
        out
    }

    fn cleared(&mut self) -> String {
        let drawn = std::mem::take(&mut self.drawn);
        match drawn {
            0 => String::new(),
            _ => format!("\r\x1b[{drawn}A\x1b[J"),
        }
    }

    fn redraw(&mut self, lines: &[String], now_ns: u64) -> String {
        let mut out = self.cleared();
        for line in lines {
            out.push_str(line);
            out.push('\n');
        }
        let region = self.region(now_ns);
        self.drawn = region.len();
        for line in region {
            out.push_str(&line);
            out.push('\n');
        }
        out
    }

    /// The region at `now_ns`: what is under way, with the time it has taken, and a count of
    /// what is done. Each line fits the width, so the region is as many rows as lines.
    fn region(&self, now_ns: u64) -> Vec<String> {
        let build = &self.build;
        if build.finished.is_some() {
            return Vec::new();
        }
        let since = |began: u64| took(now_ns.saturating_sub(began));
        let mut totals = Vec::new();
        let crates = build.crates.len();
        if crates > 0 {
            let done = build.crates.iter().filter(|it| it.ended_ns.is_some()).count();
            totals.push(format!("{done}/{crates} crates"));
        }
        let fns = build.fns();
        if fns.total > 0 {
            totals.push(format!("{}/{} fns", fns.done, fns.total));
        }
        let (word, what) = match build.running().last() {
            Some(step) => (step.step.word(), format!("{} {}", step.what(), since(step.began_ns))),
            None if build.steps.is_empty() => ("Waiting", format!("for the daemon {}", since(0))),
            None => ("Finishing", since(0)),
        };
        let mut what = what;
        for total in &totals {
            what.push_str(" \u{b7} ");
            what.push_str(total);
        }
        let mut lines = vec![self.fit(GREEN, &format!("{word:>12}"), &what)];
        let mut running: Vec<_> =
            build.crates.iter().filter(|it| it.state == CrateState::Running).collect();
        // The longest under way first: what the build waits on.
        running.sort_by_key(|it| it.began_ns);
        let shown = running.len().min(SHOWN);
        for (at, krate) in running.iter().take(shown).enumerate() {
            let branch = match at + 1 == shown && running.len() == shown {
                true => "\u{2514}\u{2500}",
                false => "\u{251c}\u{2500}",
            };
            // A compile under way knows how many it did, and not yet of how many.
            let fns = match (krate.fns.total, krate.fns.done) {
                (0, 0) => String::new(),
                (0, done) => format!("  {done} fns"),
                (total, done) => format!("  {done}/{total} fns"),
            };
            let rest = format!("{}{fns}  {}", krate.name, since(krate.began_ns));
            lines.push(self.fit(CYAN, &format!("{:>14}", branch), &rest));
        }
        if running.len() > shown {
            let more = format!("{} more under way", running.len() - shown);
            lines.push(self.fit(CYAN, &format!("{:>14}", "\u{2514}\u{2500}"), &more));
        }
        let up_to_date = build.crates.iter().filter(|it| it.state == CrateState::UpToDate).count();
        let built = build.crates.iter().filter(|it| it.state == CrateState::Built).count();
        if up_to_date + built > 0 {
            let done = format!("{built} built, {up_to_date} up to date");
            lines.push(self.fit(DIM, &format!("{:>12}", ""), &done));
        }
        lines
    }

    /// `word` painted and `rest` after it, cut to the width: a line that wrapped would be two
    /// rows, and the region would no longer be cleared whole.
    fn fit(&self, color: &str, word: &str, rest: &str) -> String {
        let room = self.width.saturating_sub(word.chars().count() + 2);
        let rest: String = match rest.chars().count() > room {
            true => rest.chars().take(room.saturating_sub(1)).chain(['\u{2026}']).collect(),
            false => rest.to_owned(),
        };
        format!("{} {rest}", paint(color, word, self.color))
    }
}
