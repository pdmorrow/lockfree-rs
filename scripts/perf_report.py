#!/usr/bin/env python3
"""Turn the counter CSV that `scripts/perf.sh` writes into something readable.

Two outputs, because they answer different questions. The terminal
table is what you look at while iterating -- it is the numbers, and it
appears the moment the sweep finishes. The HTML report is what you keep:
the same numbers, plus the shape of them, which is the part that
actually carries the argument. "MCS costs more per handoff at two
threads and stops caring after that" is a sentence about a curve, and
no column of figures makes it in one glance.

Kept as its own file rather than a heredoc inside `perf.sh` -- which is
where `coverage.sh` puts its python -- because an SVG generator is a
few hundred lines and bash is a poor host for those. Stdlib only, so
there is nothing to install: matplotlib would be a nicer way to write
this, but not a nicer way to run it on a machine that has just been
handed a repository.

Usage (normally via scripts/perf.sh):
    python3 scripts/perf_report.py target/perf/counters.csv \
        --out target/perf/report.html --events 'transfer=...;cycles=...'
"""

import argparse
import csv
import html
import math
import os
from collections import defaultdict

# --- palette ---------------------------------------------------------
#
# Three categorical hues, in a fixed order, so a series keeps its
# colour when another one is added or dropped. The dark column is the
# same three hues re-stepped for a dark surface rather than an
# automatic inversion, which would push them out of the lightness band
# the separation was checked at.
SERIES_LIGHT = ["#2a78d6", "#eb6834", "#1baf7a"]
SERIES_DARK = ["#3987e5", "#d95926", "#199e70"]

# Which series gets which slot, pinned by name. The contended sweep and
# the padding sweep share the palette but not the series, and neither
# should ever depend on iteration order for its colours.
SLOT = {
    "spinlock": 0,
    "mcs": 1,
    "mutex": 2,
    "packed": 0,
    "pad64": 1,
    "pad128": 2,
}

LABEL = {
    "spinlock": "Spinlock",
    "mcs": "McsSpinlock",
    "mutex": "std Mutex",
    "packed": "unpadded",
    "pad64": "64-byte aligned",
    "pad128": "128-byte aligned",
}


def parse_args():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("csv")
    p.add_argument("--out", default="target/perf/report.html")
    p.add_argument("--events", default="", help="role=event;... as perf.sh chose them")
    return p.parse_args()


def load(path):
    """CSV rows -> {(scenario, lock, threads): {metric: value}}.

    Repeats of the same point are averaged. Not a median, and not a
    best-of: a counter is not a timing, and the run where the scheduler
    moved a thread between cores is a real observation of this machine
    rather than noise to be discarded. If the spread matters, run the
    sweep twice and compare the reports.
    """
    runs = defaultdict(lambda: defaultdict(list))

    with open(path, newline="") as f:
        for row in csv.DictReader(f):
            key = (row["scenario"], row["lock"], int(row["threads"]))
            runs[key][row["event"]].append(float(row["count"]))
            runs[key]["_acquisitions"].append(float(row["acquisitions"]))
            runs[key]["_seconds"].append(float(row["seconds"]))

    return {
        key: {name: sum(vals) / len(vals) for name, vals in events.items()}
        for key, events in runs.items()
    }


def derive(data, roles):
    """Per-acquisition figures, which are the only comparable ones.

    Totals are not: a slower lock runs the same acquisition count over
    a longer wall clock and accumulates more of everything, so a table
    of totals ranks locks by how slow they are no matter what is being
    counted. Dividing by the acquisition count -- which the harness
    fixes in advance and reports -- gives the cost of one handoff,
    which is the quantity the two implementations actually differ in.
    """
    out = {}

    for key, events in data.items():
        acq = events["_acquisitions"]
        seconds = events["_seconds"]
        row = {"acquisitions": acq, "seconds": seconds, "rate": acq / seconds / 1e6}

        for role, event in roles.items():
            if event in events:
                row[role] = events[event] / acq

        out[key] = row

    return out


# --- SVG -------------------------------------------------------------
#
# Written by hand because the charts here are two shapes and both are
# simple, and because a dependency-free script is one that still runs
# in three years. The conventions: an ordinal x axis (the thread counts
# are a set of configurations, not a continuous variable, and plotting
# 2, 4, 8, 12 to scale would crowd three of them into the left third),
# a y axis that always starts at zero, recessive gridlines, 2px lines,
# 9px markers, and a direct label at the end of every series so colour
# is never the only thing carrying identity.

W, H = 760, 330
PAD_L, PAD_R, PAD_T, PAD_B = 62, 118, 20, 46


def nice_ceiling(value):
    """A round number at or above `value`, for the top of the axis."""
    if value <= 0:
        return 1.0
    magnitude = 10 ** math.floor(math.log10(value))
    for step in (1, 1.5, 2, 2.5, 3, 4, 5, 7.5, 10):
        if step * magnitude >= value:
            return step * magnitude
    return 10 * magnitude


def axes(xs, ymax, ylabel, fmt):
    """The frame both chart types draw inside: grid, ticks, labels."""
    parts = []
    plot_w = W - PAD_L - PAD_R
    plot_h = H - PAD_T - PAD_B

    for i in range(6):
        value = ymax * i / 5
        y = PAD_T + plot_h - plot_h * i / 5
        parts.append(
            f'<line class="grid" x1="{PAD_L}" y1="{y:.1f}" '
            f'x2="{PAD_L + plot_w}" y2="{y:.1f}"/>'
        )
        parts.append(
            f'<text class="tick" x="{PAD_L - 10}" y="{y + 4:.1f}" '
            f'text-anchor="end">{fmt(value)}</text>'
        )

    for i, x in enumerate(xs):
        cx = PAD_L + plot_w * (i + 0.5) / len(xs)
        parts.append(
            f'<text class="tick" x="{cx:.1f}" y="{PAD_T + plot_h + 20}" '
            f'text-anchor="middle">{x}</text>'
        )

    parts.append(
        f'<text class="axis-title" x="{PAD_L + plot_w / 2:.1f}" '
        f'y="{H - 8}" text-anchor="middle">threads</text>'
    )
    parts.append(
        f'<text class="axis-title" x="{PAD_L - 46}" y="{PAD_T + plot_h / 2:.1f}" '
        f'text-anchor="middle" transform="rotate(-90 {PAD_L - 46} '
        f'{PAD_T + plot_h / 2:.1f})">{html.escape(ylabel)}</text>'
    )

    return parts


def line_chart(xs, series, ylabel, fmt):
    """One line per series over an ordinal x axis.

    Lines rather than grouped bars because the question is the *shape*:
    whether the cost per handoff climbs as threads are added or stays
    where it is. A bar chart states each value and leaves the reader to
    infer the trend; a line states the trend, which is the finding.
    """
    plot_w = W - PAD_L - PAD_R
    plot_h = H - PAD_T - PAD_B

    ymax = nice_ceiling(max(max(v for v in ys if v is not None) for _, ys in series))
    parts = axes(xs, ymax, ylabel, fmt)
    labels = []

    def px(i):
        return PAD_L + plot_w * (i + 0.5) / len(xs)

    def py(v):
        return PAD_T + plot_h - plot_h * (v / ymax)

    for name, ys in series:
        colour = f"var(--series-{SLOT[name] + 1})"
        points = [(px(i), py(v)) for i, v in enumerate(ys) if v is not None]

        path = " ".join(
            f"{'M' if i == 0 else 'L'}{x:.1f},{y:.1f}"
            for i, (x, y) in enumerate(points)
        )
        parts.append(f'<path d="{path}" fill="none" stroke="{colour}" stroke-width="2"/>')

        for (x, y), v in zip(points, [v for v in ys if v is not None]):
            # The 2px surface ring is what keeps two series that cross
            # from merging into one blob at the crossing point.
            parts.append(
                f'<circle cx="{x:.1f}" cy="{y:.1f}" r="4.5" fill="{colour}" '
                f'stroke="var(--surface)" stroke-width="2">'
                f"<title>{html.escape(LABEL[name])}: {fmt(v)}</title></circle>"
            )

        labels.append((points[-1][0], points[-1][1], name, colour))

    # Direct labels are the secondary encoding that keeps identity off
    # colour alone, so two of them landing on the same pixel row is not
    # a cosmetic problem -- it is the encoding failing. Series that end
    # at the same value do exactly that (two locks that both never
    # context-switch, say), so the labels are laid out top to bottom
    # with a minimum gap and pushed down where they would overlap.
    labels.sort(key=lambda l: l[1])
    previous = -1e9
    for lx, ly, name, colour in labels:
        ly = max(ly, previous + 15)
        previous = ly
        parts.append(
            f'<text class="direct-label" x="{lx + 12:.1f}" y="{ly + 4:.1f}" '
            f'fill="{colour}">{html.escape(LABEL[name])}</text>'
        )

    return svg(parts)


def bar_chart(xs, series, ylabel, fmt):
    """Grouped bars: a value per (x, series) with no trend to read.

    The padding sweep is the case for this. Its interesting result is
    a ratio between three variants at the same thread count, not a
    curve, and bars put the three next to each other where the ratio
    is what the eye measures.
    """
    plot_w = W - PAD_L - PAD_R
    plot_h = H - PAD_T - PAD_B

    ymax = nice_ceiling(max(max(v for v in ys if v is not None) for _, ys in series))
    parts = axes(xs, ymax, ylabel, fmt)

    group_w = plot_w / len(xs)
    # 2px of surface between adjacent fills, so two bars of similar
    # height never read as one wide bar.
    bar_w = (group_w * 0.72) / len(series) - 2

    for si, (name, ys) in enumerate(series):
        colour = f"var(--series-{SLOT[name] + 1})"

        for i, v in enumerate(ys):
            if v is None:
                continue
            x = (
                PAD_L + group_w * i + group_w * 0.14
                + si * (bar_w + 2)
            )
            h = plot_h * (v / ymax)
            y = PAD_T + plot_h - h
            parts.append(
                f'<rect x="{x:.1f}" y="{y:.1f}" width="{bar_w:.1f}" '
                f'height="{max(h, 1):.1f}" rx="3" fill="{colour}">'
                f"<title>{html.escape(LABEL[name])}: {fmt(v)}</title></rect>"
            )

    legend = []
    for si, (name, _) in enumerate(series):
        y = PAD_T + 6 + si * 22
        legend.append(
            f'<rect x="{W - PAD_R + 12}" y="{y}" width="11" height="11" rx="2.5" '
            f'fill="var(--series-{SLOT[name] + 1})"/>'
        )
        legend.append(
            f'<text class="direct-label" x="{W - PAD_R + 30}" y="{y + 10}">'
            f"{html.escape(LABEL[name])}</text>"
        )

    return svg(parts + legend)


def svg(parts):
    return (
        f'<svg viewBox="0 0 {W} {H}" role="img" '
        f'preserveAspectRatio="xMidYMid meet">' + "".join(parts) + "</svg>"
    )


def table(xs, series, fmt, unit):
    """The chart's numbers, spelled out.

    Not a fallback. Two of the three hues sit below 3:1 against a light
    surface, and the rule for that is relief: the exact values have to
    be available as text somewhere, not only as a position on an axis.
    """
    head = "".join(f"<th>{x}</th>" for x in xs)
    rows = "".join(
        "<tr><th>{}</th>{}</tr>".format(
            html.escape(LABEL[name]),
            "".join(
                f"<td>{fmt(v) if v is not None else '&mdash;'}</td>" for v in ys
            ),
        )
        for name, ys in series
    )
    return (
        f'<table><caption>{html.escape(unit)}</caption>'
        f"<thead><tr><th>threads</th>{head}</tr></thead>"
        f"<tbody>{rows}</tbody></table>"
    )


def scaled(series, factor):
    """Rescale a series so its values have significant digits to show.

    Context switches per acquisition are a few parts in a hundred
    thousand, and every row of that table formats to `0.00` -- a chart
    whose axis reads zero everywhere, which is worse than no chart. The
    fix is a unit the numbers fit in, stated in the axis label.
    """
    return [
        (name, [None if v is None else v * factor for v in ys])
        for name, ys in series
    ]


def figure(kind, title, blurb, xs, series, ylabel, fmt, unit):
    draw = line_chart if kind == "line" else bar_chart
    return f"""
    <figure>
      <h3>{html.escape(title)}</h3>
      <p>{blurb}</p>
      <div class="chart">{draw(xs, series, ylabel, fmt)}</div>
      <div class="scroll">{table(xs, series, fmt, unit)}</div>
    </figure>"""


def legend(roles):
    """One line per column, naming the counter it came from.

    The counter names are worth printing rather than just the
    interpretation: a reader who wants to check what a column really
    measures needs the event name to look it up in the vendor's
    optimisation manual, and the name differs per CPU.
    """
    def event(role):
        return roles.get(role, "-")

    lines = [
        "    lines/acq    cache lines fetched from ANOTHER core or the shared",
        "                 L3 -- the coherence traffic, and the headline number.",
        f"                 [{event('transfer')}]",
        "    local/acq    fills served from this core's own L2: the cheap ones,",
        "                 where no line moved between cores.",
        f"                 [{event('local_fill')}]",
        "    cycles/acq   core-cycles per critical section, summed over EVERY",
        "                 thread -- what the lock costs the machine, not the",
        "                 latency of one acquisition. A spinlock's figure is",
        "                 mostly its waiters spinning, which is the point.",
        f"                 [{event('cycles')}]",
        "    nonspec/acq  locked read-modify-writes the core could not execute",
        "                 speculatively. NOT a count of every atomic: the",
        "                 speculated ones land elsewhere, so a lock whose CAS",
        "                 usually succeeds first time reads near zero. Read it",
        "                 as contention on the atomic itself.",
        f"                 [{event('atomics')}]",
        "    M acq/s      millions of critical sections per second, machine-wide.",
        "",
        "    Every counter column is the raw total divided by the acquisitions",
        "    it was counted over, which the harness fixes in advance. Totals",
        "    would rank the locks by how slow they are, whatever is counted.",
    ]
    return lines


def cpu_model():
    try:
        with open("/proc/cpuinfo") as f:
            for line in f:
                if line.startswith("model name"):
                    return line.split(":", 1)[1].strip()
    except OSError:
        pass
    return "unknown CPU"


def main():
    args = parse_args()

    roles = {}
    for pair in args.events.split(";"):
        if "=" in pair:
            role, event = pair.split("=", 1)
            roles[role] = event

    rows = derive(load(args.csv), roles)

    scenarios = []
    for scenario in ("contended", "disjoint"):
        keys = [k for k in rows if k[0] == scenario]
        if keys:
            xs = sorted({k[2] for k in keys})
            locks = sorted({k[1] for k in keys}, key=lambda n: SLOT.get(n, 99))
            scenarios.append((scenario, xs, locks))

    if not scenarios:
        raise SystemExit("no rows in the CSV")

    # --- terminal ----------------------------------------------------
    #
    # A legend, because the column heads have to fit in a terminal and
    # what fits is not self-explanatory. Two of them are actively
    # misleading without it: `cycles/acq` counts every thread, so it is
    # what the lock costs the machine rather than what it costs the
    # caller, and `nonspec/acq` is not a census of atomic operations --
    # see the note under it.
    print("==> columns")
    for line in legend(roles):
        print(line)
    print()

    for scenario, xs, locks in scenarios:
        print(f"==> {scenario}: per acquisition, averaged over the run")
        header = f"    {'lock':<10}{'threads':>8}{'lines/acq':>11}{'local/acq':>11}"
        header += f"{'cycles/acq':>12}{'nonspec/acq':>13}{'M acq/s':>10}"
        print(header)

        for lock in locks:
            for x in xs:
                r = rows.get((scenario, lock, x))
                if r is None:
                    continue
                print(
                    f"    {lock:<10}{x:>8}"
                    f"{r.get('transfer', float('nan')):>11.2f}"
                    f"{r.get('local_fill', float('nan')):>11.2f}"
                    f"{r.get('cycles', float('nan')):>12.0f}"
                    f"{r.get('atomics', float('nan')):>13.2f}"
                    f"{r['rate']:>10.1f}"
                )
        print()

    # --- html --------------------------------------------------------
    two = lambda v: f"{v:.2f}"
    zero = lambda v: f"{v:,.0f}"
    one = lambda v: f"{v:.1f}"

    figures = []

    for scenario, xs, locks in scenarios:
        def series(role):
            return [
                (lock, [
                    rows.get((scenario, lock, x), {}).get(role) for x in xs
                ])
                for lock in locks
                if any(role in rows.get((scenario, lock, x), {}) for x in xs)
            ]

        if scenario == "contended":
            figures.append("<h2>One lock, N threads</h2>")
            figures.append(
                "<p class=\"lede\">Every thread wants the same lock and does "
                "nothing else, so the workload is entirely serial and the only "
                "question left is what one handoff costs. The first chart is the "
                "one a stopwatch cannot draw.</p>"
            )

            if series("transfer"):
                figures.append(figure(
                    "line",
                    "Cache lines taken from another core, per acquisition",
                    "A line that was live in some other core's cache and had to be "
                    "moved to this one. The barging lock has every waiter spinning "
                    "on the same word, so a release invalidates all of them at once "
                    "and the cost per handoff climbs with the thread count. The "
                    "queued lock gives each waiter a line nobody else writes: its "
                    "constant is higher &mdash; it touches the tail pointer, its "
                    "predecessor's node and its own flag &mdash; but it is a "
                    "constant, and that is the whole trade.",
                    xs, series("transfer"),
                    "lines / acquisition", two, "cache lines pulled from another core, per acquisition",
                ))

            figures.append(figure(
                "line", "Cycles per acquisition",
                "The same runs, timed. Core-cycles summed over every thread, so "
                "this is what the lock costs the machine rather than the "
                "latency of one acquisition &mdash; most of a spinlock's figure "
                "is its waiters spinning. It is the curve the criterion "
                "benchmarks report, and on its own it says the queued lock wins "
                "past four threads without saying why. Read it against the "
                "chart above.",
                xs, series("cycles"), "cycles / acquisition", zero,
                "cycles per acquisition",
            ))

            if series("ctxsw"):
                figures.append(figure(
                    "line", "Context switches per acquisition",
                    "The axis that separates a spinlock from a Mutex more sharply "
                    "than any cache counter. A spinlock never leaves the CPU; a "
                    "Mutex parks on a futex the moment it cannot get in, trading "
                    "the burned core for a trip through the scheduler. That trade "
                    "is why the Mutex column above looks so good here and why the "
                    "result does not generalise: this benchmark hands every "
                    "waiting thread's core straight back to the one thread making "
                    "progress, which is the best case for parking and the worst "
                    "case for spinning.",
                    xs, scaled(series("ctxsw"), 1000),
                    "switches / 1000 acquisitions", two,
                    "context switches per thousand acquisitions",
                ))

        else:
            figures.append("<h2>N locks, N threads: what the padding buys</h2>")
            figures.append(
                "<p class=\"lede\">The control experiment. Every thread has a lock "
                "of its own and never waits for anybody, so a machine without "
                "caches would show three flat lines at the same height. The three "
                "variants run identical code and differ only in "
                "<code>repr(align)</code> &mdash; so whatever separates them is "
                "the hardware noticing where the flags landed, and nothing "
                "else.</p>"
            )

            if series("transfer"):
                figures.append(figure(
                    "bar",
                    "Cache lines taken from another core, per acquisition",
                    "Contention that exists nowhere in the program. Unpadded, "
                    "several flags share one 64-byte line, and each thread's "
                    "compare-exchange invalidates the line its neighbours are "
                    "using: false sharing, at the full price of the real thing. "
                    "The gap between the 64- and 128-byte columns is the part "
                    "that is architecture-specific &mdash; on AMD there is no "
                    "adjacent-line prefetcher to defeat, so 64 is already enough "
                    "and 128 is insurance against the Intel case.",
                    xs, series("transfer"), "lines / acquisition", two,
                    "cache lines pulled from another core, per acquisition",
                ))

            figures.append(figure(
                "bar", "Cycles per acquisition",
                "What that costs, in core-cycles summed over every thread. An "
                "uncontended lock should be a handful of them; anything above "
                "that is being spent on a coherence protocol resolving a "
                "conflict the program does not have.",
                xs, series("cycles"), "cycles / acquisition", zero,
                "cycles per acquisition",
            ))

    counters = "".join(
        f"<tr><td><code>{html.escape(role)}</code></td>"
        f"<td><code>{html.escape(event)}</code></td></tr>"
        for role, event in roles.items()
    )

    doc = TEMPLATE.format(
        cpu=html.escape(cpu_model()),
        nproc=os.cpu_count(),
        counters=counters,
        figures="".join(figures),
        series_light="".join(
            f"--series-{i + 1}: {c};" for i, c in enumerate(SERIES_LIGHT)
        ),
        series_dark="".join(
            f"--series-{i + 1}: {c};" for i, c in enumerate(SERIES_DARK)
        ),
    )

    os.makedirs(os.path.dirname(args.out) or ".", exist_ok=True)
    with open(args.out, "w") as f:
        f.write(doc)


TEMPLATE = """<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>spinlock-rs &middot; cache behaviour</title>
<style>
  :root {{
    color-scheme: light dark;
    --surface: #fcfcfb;
    --surface-2: #f4f3f0;
    --text: #0b0b0b;
    --text-2: #52514e;
    --muted: #8a8985;
    --rule: #e2e1dc;
    {series_light}
  }}
  @media (prefers-color-scheme: dark) {{
    :root {{
      --surface: #1a1a19;
      --surface-2: #222221;
      --text: #ffffff;
      --text-2: #c3c2b7;
      --muted: #86857c;
      --rule: #35352f;
      {series_dark}
    }}
  }}
  * {{ box-sizing: border-box; }}
  body {{
    margin: 0; padding: 3rem 1.5rem 5rem;
    background: var(--surface); color: var(--text);
    font: 16px/1.6 ui-sans-serif, system-ui, -apple-system, "Segoe UI", sans-serif;
  }}
  main {{ max-width: 60rem; margin: 0 auto; }}
  h1 {{ font-size: 1.8rem; margin: 0 0 .3rem; letter-spacing: -.02em; }}
  h2 {{
    font-size: 1.25rem; margin: 3.5rem 0 .5rem;
    padding-top: 1.5rem; border-top: 1px solid var(--rule);
  }}
  h3 {{ font-size: 1rem; margin: 0 0 .4rem; }}
  p {{ margin: 0 0 1rem; color: var(--text-2); }}
  p.lede {{ color: var(--text-2); max-width: 46rem; }}
  figure {{ margin: 2rem 0 0; }}
  figure p {{ font-size: .9rem; max-width: 46rem; }}
  .chart {{ margin: .5rem 0 1rem; }}
  .chart svg {{ width: 100%; height: auto; display: block; }}
  .grid {{ stroke: var(--rule); stroke-width: 1; }}
  .tick {{ fill: var(--muted); font: 12px ui-monospace, monospace; }}
  .axis-title {{ fill: var(--text-2); font: 12px ui-sans-serif, system-ui, sans-serif; }}
  .direct-label {{ font: 12px ui-sans-serif, system-ui, sans-serif; fill: var(--text-2); }}
  .scroll {{ overflow-x: auto; }}
  table {{
    border-collapse: collapse; font: 13px ui-monospace, monospace;
    width: 100%; margin-bottom: .5rem;
  }}
  caption {{
    caption-side: top; text-align: left; color: var(--muted);
    font: 12px ui-sans-serif, system-ui, sans-serif; padding-bottom: .35rem;
  }}
  th, td {{ padding: .3rem .7rem; text-align: right; border-bottom: 1px solid var(--rule); }}
  thead th, tbody th {{ text-align: left; color: var(--text-2); font-weight: 500; }}
  td {{ font-variant-numeric: tabular-nums; }}
  code {{ font: 13px ui-monospace, monospace; color: var(--text-2); }}
  .meta {{ color: var(--muted); font-size: .85rem; }}
  .meta table {{ width: auto; margin-top: .5rem; }}
  .meta td, .meta th {{ text-align: left; border: 0; padding: .1rem .9rem .1rem 0; }}
</style>
</head>
<body>
<main>
  <h1>What the locks do to the cache</h1>
  <p>Hardware counters from <code>scripts/perf.sh</code>, one process per
  point, counting only the critical-section loop. Every figure is
  <em>per acquisition</em>: totals rank the locks by how slow they are,
  whatever is being counted.</p>

  <div class="meta">
    <div>{cpu} &middot; {nproc} logical CPUs</div>
    <table>
      <tr><th>role</th><th>counter</th></tr>
      {counters}
    </table>
  </div>

  {figures}
</main>
</body>
</html>
"""


if __name__ == "__main__":
    main()
