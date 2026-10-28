//! Charts (content/schema.flecs, "Charts"): `BarChart` and `LineChart` drawn as inline SVG at build time, and the
//! `Picker` that shows one of several panels.
//!
//! The SVG has no viewBox: its height is in pixels and every x is a percentage of its width, so the text stays the
//! page's size at any width (a phone does not shrink it) and the marks stretch. A line chart's paths, which take no
//! percentages, sit in a nested SVG whose viewBox is stretched along x only (`preserveAspectRatio="none"`), with
//! strokes that do not scale. Colours are the subjects' slots (`--series-N` in styles/tokens.css, light and dark);
//! text is in the ink tokens, never a series colour. Each chart carries its numbers as a table under it, which is
//! also what client/chart.ts reads for the tip under the pointer.

use crate::inline::esc;
use crate::js;
use crate::world::{Entity, Id, Scalar, World};

/// A subject (`Subject` on the entity a `Plots` pair names): its label, colour slot and dashing.
#[derive(Clone, Debug, PartialEq)]
struct Subject {
    path: String,
    label: String,
    slot: i64,
    dashed: bool,
}

/// The colour slots styles/tokens.css defines.
const SLOTS: i64 = 4;

fn subject(world: &World, e: &Entity) -> Result<Subject, String> {
    let target = e
        .targets("Plots")
        .and_then(|t| t.first())
        .ok_or_else(|| format!("{}: a chart value needs (Plots, <subject>)", e.path))?;
    subject_at(world, target, &e.path)
}

fn subject_at(world: &World, path: &str, wher: &str) -> Result<Subject, String> {
    let s = world
        .lookup(path)
        .map(|id| world.get(id))
        .ok_or_else(|| format!("{wher}: {path} names nothing"))?;
    let v = s.value("Subject").ok_or_else(|| format!("{wher}: {path} is not a Subject"))?;
    let slot = match v.get("slot") {
        Some(Scalar::Int(i)) => *i,
        Some(Scalar::Float(f)) => *f as i64,
        _ => 0,
    };
    if !(1..=SLOTS).contains(&slot) {
        return Err(format!("{path}: Subject slot {slot} is not 1 to {SLOTS}"));
    }
    Ok(Subject {
        path: path.to_string(),
        label: js::string(v.get("label")),
        slot,
        dashed: matches!(v.get("dashed"), Some(Scalar::Bool(true))),
    })
}

/// The units a chart may be in.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Unit {
    Seconds,
    Millis,
    Megabytes,
}

impl Unit {
    fn of(e: &Entity, comp: &str) -> Result<Unit, String> {
        match js::string(e.value(comp).and_then(|v| v.get("unit"))).as_str() {
            "s" => Ok(Unit::Seconds),
            "ms" => Ok(Unit::Millis),
            "MB" => Ok(Unit::Megabytes),
            u => Err(format!("{}: unit {u:?} is not s, ms or MB", e.path)),
        }
    }

    /// A value as the reader reads it: `69 ms`, `1.18 s`, `531 MB`, `3.53 GB`.
    fn format(self, v: f64) -> String {
        match self {
            Unit::Seconds if v < 1.0 => format!("{} ms", (v * 1000.0).round()),
            Unit::Seconds => format!("{v:.2} s"),
            Unit::Millis if v >= 1000.0 => Unit::Seconds.format(v / 1000.0),
            Unit::Millis if v < 10.0 => format!("{v:.1} ms"),
            Unit::Millis => format!("{} ms", v.round()),
            Unit::Megabytes if v >= 1000.0 => format!("{:.2} GB", v / 1000.0),
            Unit::Megabytes => format!("{} MB", v.round()),
        }
    }

    /// An axis label: as short as the value allows.
    fn tick(self, v: f64) -> String {
        let trim = |x: f64| js::number(format!("{x:.3}").parse::<f64>().unwrap_or(x));
        match self {
            _ if v == 0.0 => "0".into(),
            Unit::Seconds if v < 1.0 => format!("{} ms", trim(v * 1000.0)),
            Unit::Seconds => format!("{} s", trim(v)),
            Unit::Millis if v >= 1000.0 => format!("{} s", trim(v / 1000.0)),
            Unit::Millis => format!("{} ms", trim(v)),
            Unit::Megabytes if v >= 1000.0 => format!("{} GB", trim(v / 1000.0)),
            Unit::Megabytes => format!("{} MB", trim(v)),
        }
    }
}

/// 1, 2, 2.5 or 5 times a power of ten, at least `rough`.
fn nice_step(rough: f64) -> f64 {
    let power = 10f64.powf(rough.log10().floor());
    let f = rough / power;
    let k = if f <= 1.0 {
        1.0
    } else if f <= 2.0 {
        2.0
    } else if f <= 2.5 {
        2.5
    } else if f <= 5.0 {
        5.0
    } else {
        10.0
    };
    k * power
}

/// The axis: zero to a round number at or above the largest value, and its ticks (about four steps).
fn axis(max: f64) -> (f64, Vec<f64>) {
    if !(max > 0.0) {
        return (1.0, vec![0.0, 1.0]);
    }
    let step = nice_step(max / 4.0);
    let top = (max / step - 1e-9).ceil().max(1.0) * step;
    let ticks = (0..=((top / step).round() as usize)).map(|i| i as f64 * step).collect();
    (top, ticks)
}

fn pct(x: f64) -> String {
    format!("{}%", js::number((x * 100.0).round() / 100.0))
}

fn f1(v: f64) -> String {
    js::number((v * 10.0).round() / 10.0)
}

/// `cargo / blaze` as a reader says it: `11× faster`, `2.1× slower`, `about the same`.
fn speedup(baseline: f64, lead: f64) -> Option<String> {
    if !(baseline > 0.0 && lead > 0.0) {
        return None;
    }
    let times = |r: f64| if r >= 10.0 { format!("{r:.0}×") } else { format!("{r:.1}×") };
    let r = baseline / lead;
    Some(if r >= 1.05 {
        format!("{} faster", times(r))
    } else if r <= 1.0 / 1.05 {
        format!("{} slower", times(1.0 / r))
    } else {
        "about the same".into()
    })
}

/// The key beside a subject's name: a bar swatch, or a line (dashed or not).
fn key(s: &Subject, line: bool) -> String {
    let dash = if s.dashed { " dashed" } else { "" };
    let mark = if line {
        format!("<line x1=\"1\" y1=\"5\" x2=\"17\" y2=\"5\" class=\"stroke s{}{dash}\" />", s.slot)
    } else {
        format!("<rect x=\"0\" y=\"1\" width=\"12\" height=\"8\" rx=\"2\" class=\"fill s{}\" />", s.slot)
    };
    format!("<svg class=\"key\" width=\"18\" height=\"10\" aria-hidden=\"true\">{mark}</svg>")
}

/// The caption: title, note, and a legend when the chart compares two subjects or more.
fn caption(title: &str, note: &str, subjects: &[Subject], line: bool) -> String {
    let legend = if subjects.len() > 1 {
        let items: String = subjects
            .iter()
            .map(|s| format!("<li>{}{}</li>", key(s, line), esc(&s.label)))
            .collect();
        format!("<ul class=\"legend\">{items}</ul>")
    } else {
        String::new()
    };
    let note = if note.is_empty() { String::new() } else { format!("<p class=\"note\">{note}</p>") };
    format!("<figcaption><span class=\"title\">{}</span>{note}{legend}</figcaption>", esc(title))
}

/// The numbers, in a fold under the chart.
fn table(head: &[String], rows: &[Vec<String>]) -> String {
    let th: String = head.iter().map(|h| format!("<th scope=\"col\">{h}</th>")).collect();
    let body: String = rows
        .iter()
        .map(|r| {
            let (first, rest) = r.split_first().expect("a row has its label");
            let tds: String = rest.iter().map(|c| format!("<td class=\"num\">{c}</td>")).collect();
            format!("<tr><th scope=\"row\">{first}</th>{tds}</tr>")
        })
        .collect();
    format!(
        "<details class=\"numbers\"><summary>Numbers</summary><div class=\"ref\"><table><thead><tr>{th}</tr></thead><tbody>{body}</tbody></table></div></details>"
    )
}

fn notes_html(notes: &[String]) -> String {
    if notes.is_empty() {
        return String::new();
    }
    let items: Vec<String> =
        notes.iter().enumerate().map(|(i, n)| format!("{} {}", mark(i), esc(n))).collect();
    format!("<p class=\"note\">{}</p>", items.join("; "))
}

fn mark(i: usize) -> &'static str {
    ["*", "†", "‡", "§"].get(i).copied().unwrap_or("¶")
}

/// The notes of a chart's values, numbered in order of first use.
#[derive(Default)]
struct Notes(Vec<String>);

impl Notes {
    fn mark(&mut self, note: &str) -> &'static str {
        if note.is_empty() {
            return "";
        }
        let i = match self.0.iter().position(|n| n == note) {
            Some(i) => i,
            None => {
                self.0.push(note.to_string());
                self.0.len() - 1
            }
        };
        mark(i)
    }
}

/// Bar geometry, in pixels.
const LABEL_H: f64 = 20.0;
const BAR_H: f64 = 10.0;
const BAR_GAP: f64 = 3.0;
const ROW_GAP: f64 = 14.0;
const AXIS_H: f64 = 22.0;
/// The share of the width the bars may take; the rest is for the value after the longest.
const BAR_SPAN: f64 = 0.8;

/// A `BarChart`: one group per `Row`, a bar per subject in it, each labelled with its value, and the row's speedup
/// over the chart's `Baseline` on the right of its label.
pub fn bar_chart(world: &World, n: Id, id: &str, note: &str) -> Result<String, String> {
    let e = world.get(n);
    let v = e.value("BarChart").expect("a BarChart");
    let title = js::string(v.get("title"));
    let unit = Unit::of(e, "BarChart")?;
    let baseline = match e.targets("Baseline").and_then(|t| t.first()) {
        Some(p) => Some(subject_at(world, p, &e.path)?),
        None => None,
    };

    struct Row {
        label: String,
        bars: Vec<(Subject, f64, String)>,
    }
    let mut rows = Vec::new();
    let mut subjects: Vec<Subject> = Vec::new();
    for &r in &e.children {
        let re = world.get(r);
        let label = js::string(
            re.value("Row")
                .ok_or_else(|| format!("{}: a BarChart's children are Rows", re.path))?
                .get("label"),
        );
        let mut bars = Vec::new();
        for &b in &re.children {
            let be = world.get(b);
            let bv = be.value("Bar").ok_or_else(|| format!("{}: a Row's children are Bars", be.path))?;
            let value = js::to_number(bv.get("value"));
            if !value.is_finite() || value < 0.0 {
                return Err(format!("{}: a Bar's value must be zero or more", be.path));
            }
            let s = subject(world, be)?;
            if bars.iter().any(|(x, _, _): &(Subject, f64, String)| x.path == s.path) {
                return Err(format!("{}: two Bars for {} in one Row", be.path, s.path));
            }
            if !subjects.iter().any(|x| x.path == s.path) {
                subjects.push(s.clone());
            }
            bars.push((s, value, js::string(bv.get("note"))));
        }
        rows.push(Row { label, bars });
    }
    if rows.is_empty() {
        return Err(format!("{}: a BarChart with no Rows", e.path));
    }
    // Every row draws its bars in the legend's order, so a colour sits in the same place in every group.
    for r in &mut rows {
        r.bars.sort_by_key(|(s, _, _)| subjects.iter().position(|x| x.path == s.path));
    }
    let max = rows.iter().flat_map(|r| r.bars.iter().map(|b| b.1)).fold(0.0, f64::max);
    let (top, ticks) = axis(max);
    let x = |v: f64| v / top * BAR_SPAN;
    let ratio = |r: &Row| {
        let b = baseline.as_ref()?;
        let base = r.bars.iter().find(|(s, _, _)| s.path == b.path)?.1;
        let lead = r.bars.iter().find(|(s, _, _)| s.path != b.path)?.1;
        speedup(base, lead)
    };

    let mut notes = Notes::default();
    let mut body = String::new();
    let mut y = 0.0;
    let mut table_rows = Vec::new();
    for (i, r) in rows.iter().enumerate() {
        let h = LABEL_H + r.bars.len() as f64 * (BAR_H + BAR_GAP) - BAR_GAP;
        let speed = ratio(r);
        let mut g = format!(
            "<rect class=\"hit\" x=\"0\" y=\"{}\" width=\"100%\" height=\"{}\" rx=\"4\" />",
            f1(y - 4.0),
            f1(h + 8.0)
        );
        g += &format!("<text class=\"label\" x=\"0\" y=\"{}\">{}</text>", f1(y + 13.0), esc(&r.label));
        if let Some(sp) = &speed {
            g += &format!(
                "<text class=\"ratio\" x=\"100%\" y=\"{}\" text-anchor=\"end\">{}</text>",
                f1(y + 13.0),
                esc(sp)
            );
        }
        let mut cells = vec![esc(&r.label)];
        for s in &subjects {
            cells.push(match r.bars.iter().find(|(b, _, _)| b.path == s.path) {
                Some((_, v, note)) => format!("{}{}", esc(&unit.format(*v)), notes.mark(note)),
                None => "–".into(),
            });
        }
        if baseline.is_some() {
            cells.push(speed.as_deref().map(esc).unwrap_or_else(|| "–".into()));
        }
        table_rows.push(cells);
        for (k, (s, v, note)) in r.bars.iter().enumerate() {
            let by = y + LABEL_H + k as f64 * (BAR_H + BAR_GAP);
            // A bar too short to see still shows where it starts.
            g += &format!(
                "<rect class=\"fill s{}\" x=\"0\" y=\"{}\" width=\"{}\" height=\"{BAR_H}\" rx=\"2\" />",
                s.slot,
                f1(by),
                pct(x(*v).max(0.002))
            );
            g += &format!(
                "<text class=\"value\" x=\"{}\" dx=\"6\" y=\"{}\" dy=\"0.35em\">{}{}</text>",
                pct(x(*v)),
                f1(by + BAR_H / 2.0),
                esc(&unit.format(*v)),
                notes.mark(note)
            );
        }
        body += &format!("<g class=\"row\" data-row=\"{i}\">{g}</g>");
        y += h + ROW_GAP;
    }
    let plot_h = y - ROW_GAP;
    let mut grid = String::new();
    for t in &ticks {
        let tx = pct(x(*t));
        grid += &format!(
            "<line class=\"grid{}\" x1=\"{tx}\" x2=\"{tx}\" y1=\"{}\" y2=\"{}\" />",
            if *t == 0.0 { " zero" } else { "" },
            f1(LABEL_H - 4.0),
            f1(plot_h + 4.0)
        );
        grid += &format!(
            "<text class=\"tick\" x=\"{tx}\" y=\"{}\" text-anchor=\"{}\">{}</text>",
            f1(plot_h + AXIS_H - 4.0),
            if *t == 0.0 { "start" } else { "middle" },
            esc(&unit.tick(*t))
        );
    }
    let height = plot_h + AXIS_H;
    let names: Vec<&str> = subjects.iter().map(|s| s.label.as_str()).collect();
    let label = format!(
        "{title}: bars for {} in each of {} rows; the numbers are in the table below.",
        names.join(" and "),
        rows.len()
    );
    let mut head = vec![String::new()];
    head.extend(subjects.iter().map(|s| esc(&s.label)));
    if let Some(b) = &baseline {
        head.push(format!("vs {}", esc(&b.label)));
    }
    Ok(format!(
        "<figure class=\"chart bars\" id=\"{}\">{}<svg class=\"plot\" width=\"100%\" height=\"{}\" role=\"img\" aria-label=\"{}\">{grid}{body}</svg>{}{}</figure>",
        esc(id),
        caption(&title, note, &subjects, false),
        f1(height),
        esc(&label),
        notes_html(&notes.0),
        table(&head, &table_rows)
    ))
}

/// Line geometry, in pixels.
const LINE_H: f64 = 156.0;
const TOP: f64 = 14.0;
const BOTTOM: f64 = 22.0;
/// The share of the width at each side kept clear, so the first and last points are whole.
const EDGE: f64 = 0.02;
/// Every point gets a dot up to this many runs; past it only the last does.
const DOTS_UP_TO: usize = 24;

/// One number per run, `-` (or anything not a number) where there is none.
fn numbers(s: &str, n: usize, wher: &str) -> Result<Vec<Option<f64>>, String> {
    if s.trim().is_empty() {
        return Ok(Vec::new());
    }
    let v: Vec<Option<f64>> = s
        .split_whitespace()
        .map(|t| t.parse::<f64>().ok().filter(|v| v.is_finite() && *v >= 0.0))
        .collect();
    if v.len() != n {
        return Err(format!("{wher}: {} values for {n} runs", v.len()));
    }
    Ok(v)
}

/// Runs of consecutive indices where `ok(i)`: a missing run breaks the line.
fn stretches(n: usize, ok: impl Fn(usize) -> bool) -> Vec<Vec<usize>> {
    let mut out = Vec::new();
    let mut cur = Vec::new();
    for i in 0..n {
        if ok(i) {
            cur.push(i);
        } else if !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

const MONTHS: [&str; 12] =
    ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

/// `Oct 20` from an ISO date, as it was written (UTC).
fn day(iso: &str) -> String {
    let (m, d) = (iso.get(5..7).and_then(|m| m.parse::<usize>().ok()), iso.get(8..10));
    match (m, d.and_then(|d| d.parse::<u32>().ok())) {
        (Some(m @ 1..=12), Some(d)) => format!("{} {d}", MONTHS[m - 1]),
        _ => iso.to_string(),
    }
}

/// A `LineChart`: a line per `Series` over the chart's `Runs`, with its band when it has one.
pub fn line_chart(world: &World, n: Id, id: &str, note: &str) -> Result<String, String> {
    let e = world.get(n);
    let v = e.value("LineChart").expect("a LineChart");
    let title = js::string(v.get("title"));
    let unit = Unit::of(e, "LineChart")?;
    let runs = e.value("Runs").ok_or_else(|| format!("{}: a LineChart needs Runs", e.path))?;
    let dates: Vec<String> =
        js::string(runs.get("dates")).split_whitespace().map(str::to_string).collect();
    let commits: Vec<String> =
        js::string(runs.get("commits")).split_whitespace().map(str::to_string).collect();
    let n_runs = dates.len();
    if n_runs == 0 || commits.len() != n_runs {
        return Err(format!(
            "{}: Runs has {n_runs} dates and {} commits",
            e.path,
            commits.len()
        ));
    }

    struct Line {
        s: Subject,
        values: Vec<Option<f64>>,
        band: Option<(Vec<Option<f64>>, Vec<Option<f64>>)>,
    }
    let mut lines: Vec<Line> = Vec::new();
    for &c in &e.children {
        let ce = world.get(c);
        let sv = ce
            .value("Series")
            .ok_or_else(|| format!("{}: a LineChart's children are Series", ce.path))?;
        let s = subject(world, ce)?;
        if lines.iter().any(|l| l.s.path == s.path) {
            return Err(format!("{}: two Series for {}", ce.path, s.path));
        }
        let values = numbers(&js::string(sv.get("values")), n_runs, &ce.path)?;
        let (lo, hi) = (
            numbers(&js::string(sv.get("lo")), n_runs, &ce.path)?,
            numbers(&js::string(sv.get("hi")), n_runs, &ce.path)?,
        );
        if values.is_empty() {
            return Err(format!("{}: a Series with no values", ce.path));
        }
        let band = if lo.is_empty() || hi.is_empty() { None } else { Some((lo, hi)) };
        lines.push(Line { s, values, band });
    }
    if lines.is_empty() {
        return Err(format!("{}: a LineChart with no Series", e.path));
    }
    let max = lines
        .iter()
        .flat_map(|l| {
            l.values.iter().chain(l.band.iter().flat_map(|(_, hi)| hi.iter())).flatten().copied()
        })
        .fold(0.0, f64::max);
    let (top, ticks) = axis(max);
    let ph = LINE_H - TOP - BOTTOM;
    // x as a share of the width; y in pixels inside the plot.
    let x = |i: usize| {
        if n_runs <= 1 { 0.5 } else { EDGE + i as f64 / (n_runs - 1) as f64 * (1.0 - 2.0 * EDGE) }
    };
    let y = |v: f64| ph - (v / top).clamp(0.0, 1.0) * ph;
    // In the stretched inner SVG, x runs 0 to 1000.
    let ix = |i: usize| f1(x(i) * 1000.0);

    let mut grid = String::new();
    for t in &ticks {
        let ty = TOP + y(*t);
        grid += &format!(
            "<line class=\"grid{}\" x1=\"0\" x2=\"100%\" y1=\"{}\" y2=\"{}\" />",
            if *t == 0.0 { " zero" } else { "" },
            f1(ty),
            f1(ty)
        );
        if *t != 0.0 {
            grid += &format!(
                "<text class=\"tick\" x=\"0\" y=\"{}\">{}</text>",
                f1(ty - 4.0),
                esc(&unit.tick(*t))
            );
        }
    }
    let base = f1(LINE_H - 6.0);
    if n_runs > 1 {
        grid += &format!(
            "<text class=\"tick\" x=\"{}\" y=\"{base}\">{}</text><text class=\"tick\" x=\"{}\" y=\"{base}\" text-anchor=\"end\">{}</text>",
            pct(x(0)),
            esc(&day(&dates[0])),
            pct(x(n_runs - 1)),
            esc(&day(&dates[n_runs - 1]))
        );
    } else {
        grid += &format!(
            "<text class=\"tick\" x=\"50%\" y=\"{base}\" text-anchor=\"middle\">{}</text>",
            esc(&day(&dates[0]))
        );
    }

    // Bands first, then the lines, the first subject last so that it is on top.
    let mut inner = String::new();
    for l in lines.iter().rev() {
        let Some((lo, hi)) = &l.band else { continue };
        for st in stretches(n_runs, |i| lo[i].is_some() && hi[i].is_some()) {
            let (a, b) = (st[0], st[st.len() - 1]);
            // One run alone is a whisker, drawn with the dots below.
            if a == b {
                continue;
            }
            let upper: Vec<String> =
                st.iter().map(|&i| format!("{},{}", ix(i), f1(y(hi[i].unwrap_or(0.0))))).collect();
            let lower: Vec<String> = st
                .iter()
                .rev()
                .map(|&i| format!("{},{}", ix(i), f1(y(lo[i].unwrap_or(0.0)))))
                .collect();
            inner += &format!(
                "<path class=\"band s{}\" d=\"M{}L{}Z\" />",
                l.s.slot,
                upper.join("L"),
                lower.join("L")
            );
        }
    }
    for l in lines.iter().rev() {
        for st in stretches(n_runs, |i| l.values[i].is_some()) {
            if st.len() < 2 {
                continue;
            }
            let d: Vec<String> = st
                .iter()
                .map(|&i| format!("{},{}", ix(i), f1(y(l.values[i].unwrap_or(0.0)))))
                .collect();
            inner += &format!(
                "<path class=\"stroke s{}{}\" d=\"M{}\" vector-effect=\"non-scaling-stroke\" />",
                l.s.slot,
                if l.s.dashed { " dashed" } else { "" },
                d.join("L")
            );
        }
    }
    let mut dots = String::new();
    for l in lines.iter().rev() {
        if let Some((lo, hi)) = &l.band {
            for i in 0..n_runs {
                let alone = (i == 0 || lo[i - 1].is_none() || hi[i - 1].is_none())
                    && (i + 1 == n_runs || lo[i + 1].is_none() || hi[i + 1].is_none());
                if let (true, Some(a), Some(b)) = (alone, lo[i], hi[i]) {
                    dots += &format!(
                        "<line class=\"whisker s{}\" x1=\"{p}\" x2=\"{p}\" y1=\"{}\" y2=\"{}\" />",
                        l.s.slot,
                        f1(TOP + y(b)),
                        f1(TOP + y(a)),
                        p = pct(x(i))
                    );
                }
            }
        }
        for (i, v) in l.values.iter().enumerate() {
            let Some(v) = v else { continue };
            if n_runs > DOTS_UP_TO && i + 1 != n_runs {
                continue;
            }
            dots += &format!(
                "<circle class=\"dot s{}\" cx=\"{}\" cy=\"{}\" r=\"4\" />",
                l.s.slot,
                pct(x(i)),
                f1(TOP + y(*v))
            );
        }
    }

    let subjects: Vec<Subject> = lines.iter().map(|l| l.s.clone()).collect();
    let mut head = vec!["Run".to_string()];
    head.extend(subjects.iter().map(|s| esc(&s.label)));
    let rows: Vec<Vec<String>> = (0..n_runs)
        .rev()
        .map(|i| {
            let short: String = commits[i].chars().take(10).collect();
            let run = if commits[i] == "-" {
                esc(&day(&dates[i]))
            } else {
                format!(
                    "{} <a href=\"https://github.com/indexable-inc/blaze/commit/{}\"><code>{}</code></a>",
                    esc(&day(&dates[i])),
                    esc(&commits[i]),
                    esc(&short)
                )
            };
            let mut r = vec![run];
            for l in &lines {
                let band = l.band.as_ref().and_then(|(lo, hi)| Some((lo[i]?, hi[i]?)));
                r.push(match (l.values[i], band) {
                    (Some(v), Some((a, b))) => format!(
                        "{} <span class=\"dim\">({} to {})</span>",
                        esc(&unit.format(v)),
                        esc(&unit.format(a)),
                        esc(&unit.format(b))
                    ),
                    (Some(v), None) => esc(&unit.format(v)),
                    (None, _) => "–".into(),
                });
            }
            r
        })
        .collect();
    let names: Vec<&str> = subjects.iter().map(|s| s.label.as_str()).collect();
    let label = format!(
        "{title}: {} over {n_runs} run{}, from {} to {}; the numbers are in the table below.",
        names.join(" and "),
        if n_runs == 1 { "" } else { "s" },
        day(&dates[0]),
        day(&dates[n_runs - 1])
    );
    Ok(format!(
        "<figure class=\"chart lines\" id=\"{}\">{}<svg class=\"plot\" width=\"100%\" height=\"{LINE_H}\" role=\"img\" aria-label=\"{}\" data-runs=\"{n_runs}\" data-edge=\"{EDGE}\">{grid}<svg y=\"{TOP}\" width=\"100%\" height=\"{}\" viewBox=\"0 0 1000 {}\" preserveAspectRatio=\"none\" overflow=\"visible\">{inner}</svg><line class=\"guide\" x1=\"0\" x2=\"0\" y1=\"{TOP}\" y2=\"{}\" />{dots}</svg>{}</figure>",
        esc(id),
        caption(&title, note, &subjects, true),
        esc(&label),
        f1(ph),
        f1(ph),
        f1(TOP + ph),
        table(&head, &rows)
    ))
}

/// The most Picks a Picker may have: styles/site.css shows the panel at the same place as the checked radio button,
/// with a rule per place.
pub const MAX_PICKS: usize = 16;

/// A `Picker`'s buttons: radio inputs named for the picker, so it needs no JS; the panels follow (render.rs).
pub fn picker_buttons(world: &World, n: Id, id: &str, picks: &[Id]) -> Result<String, String> {
    let e = world.get(n);
    let label = js::string(e.value("Picker").and_then(|v| v.get("label")));
    if picks.is_empty() || picks.len() > MAX_PICKS {
        return Err(format!("{}: a Picker has 1 to {MAX_PICKS} Picks, not {}", e.path, picks.len()));
    }
    let chosen = picks.iter().position(|&p| world.get(p).has_tag("Picked")).unwrap_or(0);
    let mut buttons = String::new();
    for (i, &p) in picks.iter().enumerate() {
        let pl = js::string(world.get(p).value("Pick").and_then(|v| v.get("label")));
        buttons += &format!(
            "<label><input type=\"radio\" name=\"pick-{}\" value=\"{i}\"{} /><span>{}</span></label>",
            esc(id),
            if i == chosen { " checked" } else { "" },
            esc(&pl)
        );
    }
    Ok(format!(
        "<fieldset class=\"picks\"><legend class=\"sr\">{}</legend>{buttons}</fieldset>",
        esc(&label)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_like_the_readme() {
        assert_eq!(Unit::Seconds.format(0.069), "69 ms");
        assert_eq!(Unit::Seconds.format(1.181), "1.18 s");
        assert_eq!(Unit::Millis.format(5.25), "5.3 ms");
        assert_eq!(Unit::Millis.format(1526.27), "1.53 s");
        assert_eq!(Unit::Megabytes.format(3532.0), "3.53 GB");
        assert_eq!(Unit::Megabytes.format(531.4), "531 MB");
        assert_eq!(Unit::Seconds.tick(0.25), "250 ms");
        assert_eq!(Unit::Megabytes.tick(2500.0), "2.5 GB");
    }

    #[test]
    fn axes_start_at_zero_and_end_round() {
        let (top, ticks) = axis(0.787);
        assert!((top - 0.8).abs() < 1e-9 && ticks.len() == 5 && ticks[0] == 0.0);
        assert_eq!(axis(8.0).0, 8.0);
        assert_eq!(axis(0.0), (1.0, vec![0.0, 1.0]));
    }

    #[test]
    fn speedups() {
        assert_eq!(speedup(0.787, 0.069).as_deref(), Some("11× faster"));
        assert_eq!(speedup(23.7, 105.7).as_deref(), Some("4.5× slower"));
        assert_eq!(speedup(1.0, 1.02).as_deref(), Some("about the same"));
        assert_eq!(speedup(0.0, 1.0), None);
    }
}
