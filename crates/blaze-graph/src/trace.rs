//! A build's inputs, kept so the build replays as a test (docs/driver.md, "The step
//! function"). A trace is lines of JSON: the machine's [`Config`], then each [`Input`] it took,
//! in order. Replayed on a new machine, a trace gives the outputs the build gave, since the
//! machine reads nothing but its inputs.

use std::fmt::Write as _;

use blaze_protocol::events::Event;
use serde::{Deserialize, Serialize};

use crate::machine::{Config, Error, Input, Machine, Output};

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Trace {
    pub config: Config,
    pub inputs: Vec<Input>,
}

impl Trace {
    pub fn write(&self) -> String {
        let mut text = String::new();
        for line in std::iter::once(serde_json::to_string(&self.config))
            .chain(self.inputs.iter().map(serde_json::to_string))
        {
            // Of types with no maps keyed by other than strings, and no floats.
            text.push_str(&line.unwrap_or_else(|why| unreachable!("{why}")));
            text.push('\n');
        }
        text
    }

    pub fn read(text: &str) -> Result<Trace, String> {
        let mut lines = text.lines().enumerate().filter(|(_, it)| !it.trim().is_empty());
        let (_, first) = lines.next().ok_or("a trace with no config line")?;
        let config = serde_json::from_str(first).map_err(|why| format!("line 1: {why}"))?;
        let inputs = lines
            .map(|(at, line)| {
                serde_json::from_str(line).map_err(|why| format!("line {}: {why}", at + 1))
            })
            .collect::<Result<_, _>>()?;
        Ok(Trace { config, inputs })
    }

    /// The outputs of the trace's inputs on a new machine.
    pub fn replay(&self) -> Result<Vec<Output>, Error> {
        let mut machine = Machine::new(self.config)?;
        let mut outputs = Vec::new();
        for input in &self.inputs {
            outputs.extend(machine.step(input.clone())?);
        }
        Ok(outputs)
    }
}

/// `outputs` a line each, for a person and for a test's expected file.
pub fn render(outputs: &[Output]) -> String {
    let mut text = String::new();
    for output in outputs {
        _ = match output {
            Output::Said(record) => match &record.event {
                Event::Node { id, kind, name, deps } => {
                    writeln!(text, "{} node #{id} {kind:?} {name} deps {deps:?}", record.at_ns)
                }
                Event::Moved { id, state } => {
                    writeln!(text, "{} #{id} {state:?}", record.at_ns)
                }
                Event::Critical { path } => {
                    writeln!(text, "{} critical {path:?}", record.at_ns)
                }
                other => writeln!(text, "{} {other:?}", record.at_ns),
            },
            Output::Run { id, pool } => writeln!(text, "  run {id} on {pool:?}"),
            Output::Finished { ok } => writeln!(text, "  finished ok={ok}"),
        };
    }
    text
}
