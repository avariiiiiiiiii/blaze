use blaze_protocol::events::{Cause, CrateState, Event, Fns, Record, Step, Summary};

use crate::{Live, Plain, summary_line, took};

const S: u64 = 1_000_000_000;
const MS: u64 = 1_000_000;

fn at(at_ns: u64, event: Event) -> Record {
    Record { at_ns, event }
}

fn anim(state: CrateState, done: u64, kept: u64) -> Event {
    Event::Crate { name: "anim".into(), state, fns: Fns { total: 5353, done, kept } }
}

/// Andrew's `blaze run` of anim from cold (04:51Z): 38 s loading, 3 s resolving, 33 s
/// compiling.
fn cold() -> Vec<Record> {
    vec![
        at(0, Event::Began { build: 1, command: "blaze run".into(), root: "/w/anim".into() }),
        at(0, Event::Started { step: Step::LoadWorkspace, of: None }),
        at(38_300 * MS, Event::Ended { step: Step::LoadWorkspace, of: None }),
        at(38_300 * MS, Event::Started { step: Step::Resolve, of: None }),
        at(41_500 * MS, Event::Ended { step: Step::Resolve, of: None }),
        at(41_500 * MS, Event::Started { step: Step::Codegen, of: None }),
        at(41_500 * MS, anim(CrateState::Running, 0, 0)),
        at(50 * S, anim(CrateState::Running, 1200, 2)),
        at(74_800 * MS, anim(CrateState::Built, 5353, 2)),
        at(74_800 * MS, Event::Ended { step: Step::Codegen, of: None }),
        at(74_800 * MS, Event::Started { step: Step::Link, of: Some("anim".into()) }),
        at(74_900 * MS, Event::Ended { step: Step::Link, of: Some("anim".into()) }),
        at(
            74_900 * MS,
            Event::Finished {
                ok: true,
                summary: Summary {
                    took_ns: 74_900 * MS,
                    crates: 1,
                    fns: Fns { total: 5353, done: 5353, kept: 2 },
                    ..Summary::default()
                },
            },
        ),
    ]
}

/// An edit of `render`'s body, which `draw` and `tick` call by its code.
fn edit() -> Vec<Record> {
    let rebuilt = |name: &str, cause: Cause| Event::Rebuilt {
        name: name.into(),
        krate: "anim".into(),
        cause,
    };
    let callee = || Cause::Callee { name: "render".into() };
    vec![
        at(0, Event::Started { step: Step::Codegen, of: None }),
        at(20 * MS, anim(CrateState::Running, 0, 0)),
        at(90 * MS, rebuilt("render", Cause::Body)),
        at(120 * MS, rebuilt("draw", callee())),
        at(150 * MS, rebuilt("tick", callee())),
        at(180 * MS, anim(CrateState::Built, 5353, 5350)),
        at(180 * MS, Event::Ended { step: Step::Codegen, of: None }),
        at(
            210 * MS,
            Event::Finished {
                ok: true,
                summary: Summary {
                    took_ns: 210 * MS,
                    crates: 1,
                    fns: Fns { total: 5353, done: 5353, kept: 5350 },
                    rebuilt: vec![
                        ("render".into(), Cause::Body),
                        ("draw".into(), callee()),
                        ("tick".into(), callee()),
                    ],
                    more: 0,
                    error: None,
                },
            },
        ),
    ]
}

#[test]
fn durations_read_as_people_say_them() {
    assert_eq!(took(210 * MS), "0.21s");
    assert_eq!(took(3_200 * MS), "3.20s");
    assert_eq!(took(33_300 * MS), "33.3s");
    assert_eq!(took(77 * S), "1m17s");
}

#[test]
fn a_cold_build_off_a_terminal_is_a_line_per_step() {
    let mut plain = Plain::new(false);
    let mut out = String::new();
    for record in cold() {
        // What a log shows while the 33 s of compiling go by.
        if record.at_ns == 74_800 * MS {
            out.push_str(&plain.tick(60 * S));
            out.push_str(&plain.tick(61 * S));
        }
        out.push_str(&plain.record(&record));
    }
    let expected = [
        "     Loading workspace",
        "      Loaded workspace in 38.3s",
        "   Resolving names",
        "    Resolved names in 3.20s",
        "   Compiling functions",
        "   Compiling anim",
        "   Compiling functions (18.5s)",
        "    Compiled anim: 5351 fns compiled, 2 kept in 33.3s",
        "     Linking anim",
        "    Compiled 5351 fns in 1 crate, 2 kept; 1m14s",
    ];
    assert_eq!(out, expected.map(|it| format!("{it}\n")).concat());
}

#[test]
fn an_edit_says_what_it_rebuilt_and_why() {
    let mut plain = Plain::new(false);
    let out: String = edit().iter().map(|it| plain.record(it)).collect();
    assert_eq!(
        out,
        "   Compiling functions\n   Compiling anim\n     Rebuilt 3 fns: render (body edited), draw, \
         tick (calls render); 0.21s\n"
    );
    let finished = plain.build().finished.clone().unwrap();
    assert!(summary_line(&finished, true).contains("\x1b["));
}

#[test]
fn a_fresh_build_says_nothing_of_its_own() {
    let records = [
        at(0, Event::Began { build: 1, command: "build".into(), root: "/w/anim".into() }),
        at(MS, Event::Finished { ok: true, summary: Summary::default() }),
    ];
    let mut plain = Plain::new(false);
    assert_eq!(records.iter().map(|it| plain.record(it)).collect::<String>(), "");
    let mut live = Live::new(80, false);
    let drawn: String = records.iter().map(|it| live.record(it, MS)).collect();
    assert!(!drawn.contains("Fresh"), "{drawn:?}");
}

/// On a terminal: the region is redrawn in place, each of its lines fits the width, each
/// redraw clears exactly what was drawn, and at the end the summary is all that is left
/// of it.
#[test]
fn the_live_region_is_redrawn_in_place_and_cleared() {
    let width = 60;
    let mut live = Live::new(width, false);
    let mut drawn = 0;
    let mut screens = Vec::new();
    let mut time = 0;
    for record in cold() {
        // Ticks every 100 ms between records: never more than that without a redraw.
        while time + 100 * MS < record.at_ns {
            time += 100 * MS;
            screens.push((time, live.tick(time)));
        }
        screens.push((record.at_ns, live.record(&record, record.at_ns)));
    }
    for (time, screen) in &screens {
        let mut rest = screen.as_str();
        if drawn > 0 {
            let clear = format!("\r\x1b[{drawn}A\x1b[J");
            rest = rest.strip_prefix(&clear).unwrap_or_else(|| panic!("{time}: {screen:?}"));
        }
        assert!(!rest.contains('\x1b'), "{time}: {rest:?}");
        let lines: Vec<&str> = rest.lines().collect();
        for line in &lines {
            assert!(line.chars().count() <= width, "{time}: {line:?}");
        }
        drawn = if live_finished(rest) { 0 } else { region_len(&lines) };
    }
    let last = &screens.last().unwrap().1;
    assert!(last.ends_with("    Compiled 5351 fns in 1 crate, 2 kept; 1m14s\n"), "{last:?}");
    // In the middle of the compile: the step, the crate under way, its count.
    let middle = &screens.iter().find(|it| it.0 == 60 * S).unwrap().1;
    let region: Vec<&str> = middle.split("\x1b[J").last().unwrap().lines().collect();
    assert_eq!(
        region,
        [
            "   Compiling functions 18.5s \u{b7} 0/1 crates \u{b7} 1200/5353 fns",
            "            \u{2514}\u{2500} anim  1200/5353 fns  18.5s",
        ]
    );
}

/// The lines that stay are those before the region: the region is its header, the crates
/// under way and the count of those done. A test of the renderer's own text, with no color.
fn region_len(lines: &[&str]) -> usize {
    let header = lines.iter().rposition(|it| {
        it.starts_with("   Compiling functions")
            || it.starts_with("     Loading workspace ")
            || it.starts_with("   Resolving names ")
            || it.starts_with("     Linking anim ")
            || it.starts_with("     Waiting ")
            || it.starts_with("   Finishing ")
    });
    header.map_or(0, |at| lines.len() - at)
}

fn live_finished(rest: &str) -> bool {
    rest.contains("    Compiled 5351 fns in 1 crate")
}
