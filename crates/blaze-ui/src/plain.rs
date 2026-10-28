//! The rendering for what is not a terminal: lines, each said once.

use blaze_protocol::events::Record;

use crate::{Build, GREEN, HEARTBEAT_NS, paint, staying, took};

pub struct Plain {
    build: Build,
    /// When a line was last written.
    said_ns: u64,
    color: bool,
    errors_said: bool,
}

impl Plain {
    pub fn new(color: bool) -> Plain {
        Plain { build: Build::default(), said_ns: 0, color, errors_said: false }
    }

    /// For the command that asked for the build, which says its errors in full after it: a
    /// failed build has no last line of its own ([`crate::last_line`]).
    pub fn errors_said(self) -> Plain {
        Plain { errors_said: true, ..self }
    }

    pub fn build(&self) -> &Build {
        &self.build
    }

    /// The lines `record` adds, each with its end.
    pub fn record(&mut self, record: &Record) -> String {
        let change = self.build.apply(record);
        let lines = staying(&self.build, &change, self.color, false, self.errors_said);
        if !lines.is_empty() {
            self.said_ns = record.at_ns;
        }
        lines.into_iter().map(|it| it + "\n").collect()
    }

    /// A line saying the step under way still is, when nothing was said for
    /// [`HEARTBEAT_NS`]: a log that stops for a minute looks like a hung build.
    pub fn tick(&mut self, now_ns: u64) -> String {
        if self.build.finished.is_some() || now_ns.saturating_sub(self.said_ns) < HEARTBEAT_NS {
            return String::new();
        }
        let Some(step) = self.build.running().last() else { return String::new() };
        self.said_ns = now_ns;
        let word = paint(GREEN, &format!("{:>12}", step.step.word()), self.color);
        format!("{word} {} ({})\n", step.what(), took(now_ns.saturating_sub(step.began_ns)))
    }
}
