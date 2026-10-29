//! `blaze-v2-proof '{"incremental": "a.cbor", "scratch": "b.cbor", "before": "p.cbor"}'`: the
//! oracle's comparison of two [`Trace`]s (deterministic CBOR files), as JSON on stdout. Its
//! one argument is a JSON value inline, the path of a file holding one, or `-` for stdin;
//! `before`, the trace of the build before the incremental one, adds what it reran for
//! nothing. Exits 1 when anything is stale, 2 when it could not compare.

use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
    process::ExitCode,
};

use blaze_query::QueryKey;
use blaze_v2_proof::{Stale, Trace, over, under};
use serde::{Deserialize, Serialize};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    incremental: PathBuf,
    scratch: PathBuf,
    #[serde(default)]
    before: Option<PathBuf>,
}

#[derive(Serialize)]
struct Found {
    what: &'static str,
    query: String,
    culprit: bool,
    detail: String,
}

#[derive(Serialize)]
struct Output {
    stale: Vec<Found>,
    culprits: usize,
    work: Option<blaze_v2_proof::Work>,
    needless: Vec<String>,
}

fn name(key: &QueryKey) -> String {
    let hex: String = key.input.0[..8].iter().map(|b| format!("{b:02x}")).collect();
    format!("{:04x}:{hex}", key.kind.0)
}

fn found(stale: &Stale) -> Found {
    let (what, detail) = match stale {
        Stale::Unverified { read, saw, now, .. } => (
            "unverified",
            format!("kept though it read {} at {saw:?}, which is now {now:?}", name(read)),
        ),
        Stale::UnrecordedRead { incremental, scratch, .. } => (
            "unrecorded-read",
            format!("read the same as from scratch; {incremental:?} against {scratch:?}"),
        ),
        Stale::Absent { .. } => ("absent", "the build from scratch has it, the incremental build not".to_owned()),
        Stale::Downstream { incremental, scratch, .. } => {
            ("downstream", format!("read stale results; {incremental:?} against {scratch:?}"))
        }
    };
    Found { what, query: name(&stale.query()), culprit: stale.is_culprit(), detail }
}

fn read_input(arg: &str) -> Result<Input, String> {
    let text = if arg == "-" {
        let mut text = String::new();
        std::io::stdin().read_to_string(&mut text).map_err(|e| format!("stdin: {e}"))?;
        text
    } else if arg.trim_start().starts_with('{') {
        arg.to_owned()
    } else {
        fs::read_to_string(arg).map_err(|e| format!("{arg}: {e}"))?
    };
    serde_json::from_str(&text).map_err(|e| format!("the input: {e}"))
}

fn trace(path: &Path) -> Result<Trace, String> {
    let bytes = fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    blaze_protocol::cbor::from_slice(&bytes).map_err(|e| format!("{}: {e}", path.display()))
}

fn compare(input: &Input) -> Result<Output, String> {
    let incremental = trace(&input.incremental)?;
    let scratch = trace(&input.scratch)?;
    let stale: Vec<Found> = under(&incremental, &scratch, &[]).iter().map(found).collect();
    let (work, needless) = match &input.before {
        Some(before) => {
            let (work, needless) = over(&trace(before)?, &incremental);
            (Some(work), needless.iter().map(name).collect())
        }
        None => (None, Vec::new()),
    };
    let culprits = stale.iter().filter(|f| f.culprit).count();
    Ok(Output { stale, culprits, work, needless })
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [arg] = args.as_slice() else {
        eprintln!("usage: blaze-v2-proof JSON (inline, a file's path, or -): {{\"incremental\", \"scratch\", \"before\"?}}");
        return ExitCode::from(2);
    };
    match read_input(arg).and_then(|input| compare(&input)) {
        Ok(output) => {
            let stale = !output.stale.is_empty();
            println!("{}", serde_json::to_string_pretty(&output).expect("the output is JSON"));
            if stale { ExitCode::from(1) } else { ExitCode::SUCCESS }
        }
        Err(e) => {
            eprintln!("blaze-v2-proof: {e}");
            ExitCode::from(2)
        }
    }
}
