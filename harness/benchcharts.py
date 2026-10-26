"""benchcharts.py: the benchmark charts of docs/benchmarks.md, as SVG, from the results benchreport.py writes.

benchreport.py is the one caller: every time it writes a table it writes the
chart of the same numbers next to it, so a chart is never older than its table. Each chart is
two files, `NAME-light.svg` and `NAME-dark.svg` under docs/benchmarks/charts, which the page
shows with `<picture>` by the reader's colour scheme. Hand-written SVG, standard library only:
nothing to pin, and a change of numbers is a readable diff. The two files of a chart differ only
in their `<style>`, and the same numbers always give the same bytes.

Every chart is the same chart (`compare`): ra-jit against cargo, as how many times better
ra-jit is, on a log axis around a line at 1×, the same as cargo. Right of the line ra-jit is
better, left of it worse, in every chart, and the axis says so under it.

A chart is a picture, not a paragraph: a short title, the legend, the bars with their factors,
the axis and the 1× line, and nothing else. What it shows, how to read it and on which machines
is Markdown next to it (`says`, and benchreport.py's machine lines), so it reads as text, wraps
on a phone and is found by search.

  edits     how many times faster than incremental `cargo build` an edit builds, per project and
            platform: the median over the kinds of edit, the line from the slowest kind to the fastest
  memory    the most memory the work of an edit held at once, ra-jit's (the daemon, its workers,
            the command) against cargo's (cargo, rustc, the linker)
  disk      what each side keeps on disk once the edits are done: cargo's target directory against
            ra-jit's target directory, store and home
  runtime   how long the program ra-jit's LLVM built takes against `cargo build --release`'s

Each row has a bar per platform (benchplatforms.PLATFORMS, in that order), and a platform with
no numbers for the row has a quiet dash in place of its bar. The first row is each platform's
geometric mean over the rows it measured, its factors in words ("10× faster"); the other rows
say the word only when the chart has bars on both sides of the line. Each platform is its own
machine: a bar is comparable with the bars of its own colour, which the text under it says.

The colours: a hue per platform, the same in every chart (blue, orange, teal: Okabe and Ito's
palette, apart under the common colour blindnesses), each tuned for the page it is drawn on. The
rest is GitHub's own greys, so a chart sits on a GitHub page like its tables do.
"""
import math, os

from benchplatforms import NAMES

HERE = os.path.dirname(os.path.abspath(__file__))
CHARTS = os.path.join(os.path.dirname(HERE), "docs/benchmarks/charts")
FONT = "-apple-system, BlinkMacSystemFont, 'Segoe UI', 'Noto Sans', Helvetica, Arial, sans-serif"
# By scheme: a colour per platform (in NAMES' order), text, quieter text, grid lines, the 1× line,
# and the band behind every other row. The greys are GitHub's (Primer's fg.default, fg.muted,
# border.muted, canvas.subtle) for its light and dark pages.
SCHEMES = {
    "light": dict(platforms=("#1f6fd0", "#d8700a", "#12a07a"), ink="#1f2328", dim="#59636e", grid="#e3e7eb",
                  rule="#59636e", band="#f6f8fa"),
    "dark": dict(platforms=("#5aa9f5", "#f0913a", "#34c69a"), ink="#f0f6fc", dim="#9198a1", grid="#262c36",
                 rule="#9198a1", band="#151b23"),
}
WIDTH, PAD, BAR, GAP, ROW_PAD = 760, 16, 12, 3, 22
# In place of the bar of a platform that has not measured a row.
NOT_MEASURED = "–"


def esc(text):
    return str(text).replace("&", "&amp;").replace("<", "&lt;").replace(">", "&gt;")


def text_width(text, size):
    """About how wide `text` is in the sans-serif fonts above: enough to lay out a column."""
    narrow = sum(c in "ijlt.,:;|!' ()1" for c in text)
    return size * (0.58 * (len(text) - narrow) + 0.3 * narrow)


def n(v):
    """A coordinate, to a tenth of a pixel."""
    return f"{v:.1f}".removesuffix(".0")


class Log:
    """A logarithmic axis from `lo` to `hi` onto pixels `x0` to `x1`."""

    def __init__(self, lo, hi, x0, x1):
        self.lo, self.hi, self.x0, self.x1 = math.log10(lo), math.log10(hi), x0, x1

    def __call__(self, v):
        return self.x0 + (math.log10(v) - self.lo) / (self.hi - self.lo) * (self.x1 - self.x0)


def factors(span):
    """The round factors past 1× an axis spanning `span` (its high ÷ its low) has room to mark,
    each side of the 1× line, up to 10⁶."""
    marks = (1, 2, 5) if span >= 30 else (1, 1.5, 2, 3, 5, 7) if span >= 6 else (1, 1.5, 2, 3, 4, 5, 7)
    return [round(10 ** d * m, 9) for d in range(7) for m in marks if 10 ** d * m > 1]


def geomean(values):
    return math.exp(sum(math.log(v) for v in values) / len(values))


def style(c):
    platforms = "".join(f".p{i}{{fill:{v}}}.w{i}{{stroke:{v}}}" for i, v in enumerate(c["platforms"]))
    return (f"<style>text{{font-family:{FONT};fill:{c['ink']}}}.d{{fill:{c['dim']}}}.b{{font-weight:600}}"
            f".v{{font-variant-numeric:tabular-nums}}.g{{stroke:{c['grid']}}}.r{{stroke:{c['rule']};stroke-width:1.25}}"
            f".z{{fill:{c['band']}}}.o{{fill:none;stroke-width:1.25}}{platforms}</style>")


def cell(better, low=None, high=None, detail="", rough=False):
    """One platform's bar of a row: how many times better ra-jit is (over 1, better; under 1,
    worse), the range of it from `low` to `high` when there is one, what to add after the factor,
    and whether the runs varied too much to read it closely (≈)."""
    return better, low, high, detail, rough


def compare(name, title, rows, everything, words, same, hints, digits=1):
    """Write chart `name`: `rows` is (label, {platform: cell(...)}), `everything` what the
    geometric-mean row stands for ("All projects"), `words` (better, worse) after a factor, `same`
    what the 1× line is, `hints` (left, right) what each side of it means. A row's label is drawn
    up to its first space ("ripgrep 15.1.0" as "ripgrep"). Returns every row in words, with its
    whole label and each detail, for the chart's alt text."""
    measured = [k for k in NAMES if any(k in by for _, by in rows)]
    means = {k: geomean([by[k][0] for _, by in rows if k in by]) for k in measured}
    sides = {by[k][0] > 1 for _, by in rows for k in by if round(by[k][0], digits + 1) != 1}
    mixed = len(sides) > 1

    def said(better, rough=False, detail="", worded=True):
        m = better if better >= 1 else 1 / better
        text = f"{m:.0f}×" if digits == 1 and m >= 9.95 else f"{m:.{digits}f}×"
        word = "" if float(text[:-1]) == 1 or not worded else f" {words[0] if better > 1 else words[1]}"
        return ("≈" if rough else "") + text + word + (f" · {detail}" if detail else "")

    # Each bar: (platform, value, low, high, its label on the chart, it in words for the alt text).
    lines = [((everything, "geometric mean"), True, [(NAMES.index(k), means[k], None, None, said(means[k]), said(means[k]))
                                                   for k in measured])]
    lines += [((label, None), False, [(NAMES.index(k), by[k][0], by[k][1], by[k][2], said(by[k][0], by[k][4], worded=mixed),
                                       said(by[k][0], by[k][4], by[k][3]))
                                      if k in by else (NAMES.index(k), None, None, None, NOT_MEASURED, None) for k in measured])
              for label, by in rows]
    shown = lambda main: main if main == everything else main.split(" ")[0]

    values = [x for _, _, bars in lines for _, v, low, high, *_ in bars for x in (v, low, high) if x is not None]
    lo, hi = min(min(values) / 1.2, 0.5), max(max(values) * 1.2, 2)
    marks = factors(hi / lo)
    lo, hi = 1 / next(m for m in marks if m >= round(1 / lo, 9)), next(m for m in marks if m >= round(hi, 9))
    ticks = [1 / m for m in reversed(marks) if m <= round(1 / lo, 9)] + [1] + [m for m in marks if m <= hi]
    if hi / lo >= 30:
        # A wide axis keeps its grid to the powers of ten: fewer lines, the same span.
        ticks = [v for v in ticks if abs(math.log10(v) - round(math.log10(v))) < 1e-9]

    label_w = max(max(text_width(shown(main), 13.5), text_width(sub or "", 11)) for (main, sub), _, _ in lines)
    value_w = max(text_width(text, 12.5) for _, _, bars in lines for *_, text, _ in bars)
    x0, x1 = PAD + label_w + 16, WIDTH - PAD - value_w - 16
    scale = Log(lo, hi, x0, x1)
    one = scale(1)

    body, y = [f'<text x="{PAD}" y="30" font-size="19" class="b">{esc(title)}</text>'], 30
    at, y = PAD, y + 30
    for k in measured:
        width = 18 + text_width(k, 12.5)
        if at > PAD and at + width > WIDTH - PAD:
            at, y = PAD, y + 22
        body.append(f'<rect x="{at}" y="{y - 10}" width="12" height="12" rx="3" class="p{NAMES.index(k)}"/>')
        body.append(f'<text x="{n(at + 18)}" y="{y}" font-size="12.5">{esc(k)}</text>')
        at += width + 22

    top = y + 40
    heights = [len(bars) * BAR + (len(bars) - 1) * GAP + ROW_PAD for _, _, bars in lines]
    bottom = top + sum(heights)
    grid, marks_, labels = [], [], []
    for v in ticks:
        x = n(scale(v))
        grid.append(f'<line x1="{x}" y1="{top}" x2="{x}" y2="{bottom}" class="g"/>')
        labels.append(f'<text x="{x}" y="{bottom + 17}" font-size="11.5" text-anchor="middle" class="d v">'
                      f'{(v if v >= 1 else 1 / v):g}×</text>')
    row_y = top
    for i, (((main, sub), bold, bars), h) in enumerate(zip(lines, heights)):
        if i and i % 2 == 0:
            grid.insert(0, f'<rect x="{PAD}" y="{row_y}" width="{WIDTH - 2 * PAD}" height="{h}" class="z"/>')
        if i == 1:
            grid.append(f'<line x1="{PAD}" y1="{row_y}" x2="{WIDTH - PAD}" y2="{row_y}" class="r"/>')
        weight, strong = (" b", ' class="b"') if bold else ("", "")
        mid = row_y + h / 2
        labels.append(f'<text x="{n(x0 - 12)}" y="{n(mid + (-2 if sub else 4.5))}" font-size="13.5" text-anchor="end"{strong}>{esc(shown(main))}</text>')
        if sub:
            labels.append(f'<text x="{n(x0 - 12)}" y="{n(mid + 12)}" font-size="11" text-anchor="end" class="d">{esc(sub)}</text>')
        by = row_y + ROW_PAD / 2
        for platform, value, low, high, text, _ in bars:
            if value is None:
                labels.append(f'<text x="{n(one + 6)}" y="{n(by + BAR - 2)}" font-size="12.5" class="d">{esc(text)}</text>')
                by += BAR + GAP
                continue
            a, b = sorted((one, scale(min(max(value, lo), hi))))
            marks_.append(f'<rect x="{n(a)}" y="{n(by)}" width="{n(max(b - a, 2))}" height="{BAR}" rx="2" class="p{platform}"/>')
            # The label always reads rightwards: after a bar to the right, or just past the 1× line
            # for a bar to the left, where its row is empty.
            tx = b if value >= 1 else one
            if low is not None and high is not None:
                l, r = scale(max(low, lo)), scale(min(high, hi))
                marks_.append(f'<path d="M{n(l)} {n(by + BAR / 2)}H{n(r)}M{n(l)} {n(by + 2)}v8M{n(r)} {n(by + 2)}v8" class="o w{platform}"/>')
                tx = max(tx, r)
            labels.append(f'<text x="{n(tx + 6)}" y="{n(by + BAR - 2)}" font-size="12.5" class="v{weight}">{esc(text)}</text>')
            by += BAR + GAP
        row_y += h
    marks_.append(f'<line x1="{n(one)}" y1="{top - 8}" x2="{n(one)}" y2="{bottom}" class="r"/>')
    labels.append(f'<text x="{n(one)}" y="{top - 13}" font-size="11.5" text-anchor="middle" class="d">{esc(same)}</text>')
    left, right = f"← {hints[0]}", f"{hints[1]} →"
    # Under the row labels is empty, so the left hint may reach into it: with nothing worse than
    # 1× the line is near the plot's left edge, and the two hints must not overlap there.
    lx = max(one - 8, PAD + text_width(left, 12))
    rx = max(min(one + 8, WIDTH - PAD - text_width(right, 12)), lx + 16)
    labels.append(f'<text x="{n(lx)}" y="{bottom + 40}" font-size="12" text-anchor="end" class="d">{esc(left)}</text>')
    labels.append(f'<text x="{n(rx)}" y="{bottom + 40}" font-size="12" class="d">{esc(right)}</text>')
    height = bottom + 60

    shapes = "\n".join(body + grid + marks_ + labels)
    os.makedirs(CHARTS, exist_ok=True)
    for scheme, c in SCHEMES.items():
        with open(os.path.join(CHARTS, f"{name}-{scheme}.svg"), "w") as f:
            f.write(f'<svg xmlns="http://www.w3.org/2000/svg" width="{WIDTH}" height="{height}" viewBox="0 0 {WIDTH} {height}" '
                    f'role="img" aria-label="{esc(title)}">\n<title>{esc(title)}</title>\n{style(c)}\n{shapes}\n</svg>\n')
    return "; ".join(f"{main}{' (' + sub + ')' if sub else ''}: " + ", ".join(f"{NAMES[k]} {text}" for k, v, *_, text in bars if v is not None)
                     for (main, sub), _, bars in lines)


# What each chart shows, as Markdown bullets for under it (the chart itself carries no prose).
# `colours` is how to read its colours: which machines, or where they are named.
SAYS = {
    "edits": ["**Bar:** how many times faster blaze builds an edit than incremental `cargo build`, both warm. "
              "It is the median over the kinds of edit.",
              "**Thin line:** from the slowest kind of edit to the fastest."],
    "memory": ["**Bar:** how many times more (left) or less (right) memory blaze held at its peak while an edit built, "
               "against cargo.",
               "**blaze's side:** its daemon (which keeps the project loaded between edits), its workers and the command. "
               "**cargo's:** cargo, rustc and the linker."],
    "disk": ["**Bar:** how many times more (left) or less (right) disk blaze keeps once the edits are done, against cargo.",
             "**blaze's side:** its target directory, function store and home. **cargo's:** its target directory."],
    "runtime": ["**Bar:** how long each benchmark takes built by blaze's LLVM against built by `cargo build --release`, "
                "on the same machine."],
}
def says(name, colours, same="cargo"):
    """The Markdown under chart `name`: what it shows, the 1× line (the same as `same`) and its colours."""
    line = f"**1× line:** the same as {same}. Right of it blaze wins, left of it {same}. The scale is logarithmic."
    return "\n".join(f"- {x}" for x in [*SAYS[name], line, f"**Colours:** {colours}"])


def picture(name, alt, prefix="docs/benchmarks/charts"):
    """The markup for a chart: the dark file for a dark scheme, the light one otherwise."""
    return (f'<picture><source media="(prefers-color-scheme: dark)" srcset="{prefix}/{name}-dark.svg">'
            f'<img alt="{esc(alt)}" src="{prefix}/{name}-light.svg" width="{WIDTH}"></picture>')


def gigabytes(mb):
    return f"{mb / 1000:.1f} GB" if mb >= 1000 else f"{mb:.0f} MB"


def median(values):
    r = sorted(values)
    return r[len(r) // 2] if len(r) % 2 else math.sqrt(r[len(r) // 2 - 1] * r[len(r) // 2])


def edits(rows):
    """`rows`: (project, {platform: [cargo seconds / ra-jit seconds of each kind of edit]}).
    Returns the chart's alt text."""
    rows = [(p, {k: sorted(v) for k, v in by.items() if v}) for p, by in rows]
    rows = [(p, {k: cell(median(r), r[0], r[-1]) for k, r in by.items()}) for p, by in rows if by]
    if not rows:
        return None
    return compare("edits", "How fast an edit builds, against cargo", rows, "All projects", ("faster", "slower"),
                   "same as cargo", ("slower", "faster"))


def against(rows):
    """(project, {platform: (cargo's MB, ra-jit's MB)}) as cells: how many times less ra-jit uses."""
    rows = [(p, {k: cell(a / b, detail=f"{gigabytes(b)} vs cargo's {gigabytes(a)}") for k, (a, b) in by.items() if a and b}) for p, by in rows]
    return [(p, by) for p, by in rows if by]


def memory(rows):
    """`rows`: (project, {platform: (cargo's peak MB, ra-jit's peak MB)}) during an edit."""
    rows = against(rows)
    if not rows:
        return None
    return compare("memory", "Memory while an edit builds, against cargo", rows, "All projects", ("less", "more"),
                   "same as cargo", ("more memory", "less memory"))


def disk(rows):
    """`rows`: (project, {platform: (cargo's MB on disk, ra-jit's MB on disk)}) once the edits are done."""
    rows = against(rows)
    if not rows:
        return None
    return compare("disk", "Disk a build keeps, against cargo", rows, "All projects", ("less", "more"),
                   "same as cargo", ("more disk", "less disk"))


def runtime(rows, rough=()):
    """`rows`: (benchmark, {platform: ra-jit's time / release's}); `rough`: the (benchmark, platform)
    whose runs varied too much to read closely, labelled ≈."""
    rows = [(b, {k: cell(1 / v, rough=(b, k) in rough) for k, v in by.items() if v}) for b, by in rows]
    rows = [(b, by) for b, by in rows if by]
    if not rows:
        return None
    return compare("runtime", "How fast the built programs run, against release", rows, "All benchmarks",
                   ("faster", "slower"), "same as release", ("slower", "faster"), digits=2)
