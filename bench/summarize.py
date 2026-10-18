"""Summarize a bench/run_all.sh output directory as markdown.

Usage: python3 -I bench/summarize.py bench/out/<run> > summary.md

For every benchmark it prints the steady-state metric(s) as median [min-max] over all samples of all trials for
Java and Rust, the Rust speedup (>1 = Rust faster; computed from medians, inverted for lower-is-better metrics),
and per-process wall / CPU (user+sys) / peak RSS from `/usr/bin/time -l`. Warmup rounds are dropped as described
per benchmark (see WARMUP notes in the output).
"""
import os
import re
import statistics
import sys
from collections import OrderedDict

ANSI = re.compile(r"\x1b\[[0-9;]*m")
SUFFIX = {"": 1, "k": 1e3, "K": 1e3, "M": 1e6, "B": 1e9, "G": 1e9, "T": 1e12}


def num(s):
    """'1.2B' / '776M' / '45k' / '12,345' -> float"""
    s = s.strip().replace(",", "")
    m = re.fullmatch(r"([0-9.]+(?:[eE][-+]?\d+)?)([kKMBGT]?)", s)
    if not m:
        raise ValueError(s)
    return float(m.group(1)) * SUFFIX[m.group(2)]


def dur(s):
    """planetiler duration '1h2m3s' / '1m5s' / '0.5s' / '300ms' -> seconds"""
    s = s.strip()
    if s.endswith("ms") and re.fullmatch(r"[0-9.]+ms", s):
        return float(s[:-2]) / 1000
    total = 0.0
    for v, u in re.findall(r"([0-9.]+)([hms])", s):
        total += float(v) * {"h": 3600, "m": 60, "s": 1}[u]
    return total


def iso_dur(s):
    """Java Duration.toString without PT: '0.302S', '1M2.5S', '302MS'? -> seconds"""
    s = s.strip().upper()
    total = 0.0
    for v, u in re.findall(r"([0-9.]+)(H|M(?!S)|MS|S)", s):
        total += float(v) * {"H": 3600, "M": 60, "S": 1, "MS": 0.001}[u]
    return total


def read_log(path):
    out = []
    with open(path, errors="replace") as f:
        for line in f:
            line = ANSI.sub("", line.rstrip("\n"))
            m = re.match(r"(\d+\.\d+) (.*)", line)
            if m:
                out.append((float(m.group(1)), m.group(2)))
            else:
                out.append((None, line))
    return out


def read_time(path):
    res = {}
    try:
        txt = open(path).read()
    except OSError:
        return res
    m = re.search(r"([0-9.]+) real\s+([0-9.]+) user\s+([0-9.]+) sys", txt)
    if m:
        res["wall"] = float(m.group(1))
        res["cpu"] = float(m.group(2)) + float(m.group(3))
    m = re.search(r"(\d+)\s+maximum resident set size", txt)
    if m:
        res["rss"] = int(m.group(1))
    return res


def read_meta(path):
    res = {}
    try:
        for line in open(path):
            k, _, v = line.partition(": ")
            res[k.strip()] = v.strip()
    except OSError:
        pass
    return res


class Metric:
    def __init__(self, name, higher_better=True, unit=""):
        self.name, self.higher_better, self.unit = name, higher_better, unit
        self.values = {"java": [], "rust": []}


class Bench:
    def __init__(self):
        self.metrics = OrderedDict()
        self.notes = []

    def add(self, side, name, value, higher_better=True, unit=""):
        if name not in self.metrics:
            self.metrics[name] = Metric(name, higher_better, unit)
        self.metrics[name].values[side].append(value)


# ------------------------------------------------------------------ per-benchmark parsers
# each takes (bench, side, lines) and adds steady-state samples


def rounds_rate(regex, name, skip=1):
    def parse(b, side, lines):
        vals = [num(m.group(1)) for _, t in lines for m in [re.search(regex, t)] if m]
        for v in vals[skip:]:
            b.add(side, name, v, True, "/s")

    return parse


def stats_line(key, name):
    def parse(b, side, lines):
        for _, t in lines:
            m = re.search(key + r".*average=([0-9.]+), max=([0-9.]+)", t)
            if m:
                b.add(side, name + " (average of reps)", float(m.group(1)), True, "/s")
                b.add(side, name + " (best rep)", float(m.group(2)), True, "/s")

    return parse


def p_long_long_map(b, side, lines):
    for _, t in lines:
        parts = t.split("\t")
        if len(parts) == 9 and parts[7].endswith("/s"):
            b.add(side, "write rate", num(parts[7][:-2]), True, "/s")
            b.add(side, "read rate (8 readers)", num(parts[8][:-2]), True, "/s")


def p_vector_tile(b, side, lines):
    for _, t in lines:
        m = re.search(r"encodeGeometry: ([0-9.]+) features/sec", t)
        if m:
            b.add(side, "encodeGeometry", float(m.group(1)), True, "features/s")
        m = re.search(r"tile encode: +([0-9.]+) tiles/sec", t)
        if m:
            b.add(side, "3-layer tile encode", float(m.group(1)), True, "tiles/s")
        m = re.search(r"tile fnv1a64: +([0-9a-f]+)", t)
        if m:
            note = f"{side} tile hash {m.group(1)}"
            if note not in b.notes:
                b.notes.append(note)


TIMER = re.compile(r"^([0-9.hms]+) cpu:([0-9.hms]+) avg:[0-9.]+(?: \(wall_ms=([0-9.]+) cpu_ms=([0-9.]+)\))?$")


def p_osm_read(b, side, lines):
    passes = [(ts, m) for ts, t in lines for m in [TIMER.match(t)] if m]
    # discard passes 1-2 (warmup); Java prints coarse durations, so derive its wall time from the line timestamps
    for i in range(2, len(passes)):
        ts, m = passes[i]
        if m.group(3):
            wall, cpu = float(m.group(3)) / 1000, float(m.group(4)) / 1000
        else:
            wall, cpu = ts - passes[i - 1][0], dur(m.group(2))
        b.add(side, "pass1 wall", wall, False, "s")
        b.add(side, "pass1 CPU" + (" (Java: 1s resolution)" if side == "java" else ""), cpu, False, "s")


def p_osm_reader(b, side, lines):
    res = [m for _, t in lines for m in [re.search(
        r"RESULT pass1: .*?\(([0-9.]+)s wall ([0-9.]+)s cpu\) pass2: .*?\(([0-9.]+)s wall ([0-9.]+)s cpu\) features=(\d+)",
        t)] if m]
    for m in res[1:]:  # first iteration of each process discarded (JIT warmup / page cache)
        b.add(side, "pass1 wall", float(m.group(1)), False, "s")
        b.add(side, "pass1 CPU", float(m.group(2)), False, "s")
        b.add(side, "pass2 wall", float(m.group(3)), False, "s")
        b.add(side, "pass2 CPU", float(m.group(4)), False, "s")
    if res:
        note = f"{side} features={res[-1].group(5)}"
        if note not in b.notes:
            b.notes.append(note)


def p_tile_archive_writer(b, side, lines):
    for _, t in lines:
        m = re.search(r"archive round (\d+): ([0-9.]+)s wall ([0-9.]+)s cpu, (\d+) tiles, (\d+) bytes", t)
        if m and int(m.group(1)) > 0:  # round 0 discarded
            b.add(side, "archive wall", float(m.group(2)), False, "s")
            b.add(side, "archive CPU", float(m.group(3)), False, "s")
            note = f"{side} tiles={m.group(4)} bytes={m.group(5)}"
            if note not in b.notes:
                b.notes.append(note)
        m = re.search(r"render: \d+ rendered features in ([0-9.]+)s wall ([0-9.]+)s cpu", t)
        if m:
            b.add(side, "render (setup, 1 per process) wall", float(m.group(1)), False, "s")
        m = re.search(r"^sort: ([0-9.]+)s wall", t)
        if m:
            b.add(side, "sort (setup, 1 per process) wall", float(m.group(1)), False, "s")


def p_line_splitter(b, side, lines):
    rows = [t for _, t in lines if t.startswith("reused:")]
    cols = ["reused 10pt", "reused 100pt", "reused 1000pt", "!reused 10pt", "!reused 100pt", "!reused 1000pt",
            "cacheable 10pt", "cacheable 100pt", "cacheable 1000pt"]
    for row in rows[2:]:  # first 2 of 10 rounds discarded
        parts = row.split("\t")
        vals = [parts[i] for i in (1, 2, 3, 5, 6, 7, 9, 10, 11)]
        for c, v in zip(cols, vals):
            b.add(side, c, float(v), False, "ms")


def p_line_merge(b, side, lines):
    rows = [t.split("\t") for _, t in lines if re.match(r"^ *(JTS|loop\()", t)]
    names = ["200433 lines (time)", "239823 lines (time)", "1759 point line", "50x2", "10x10", "2x50"]
    for i, row in enumerate(rows):
        if i < 4:  # round 1 (4 rows) discarded
            continue
        alg = row[0].strip()
        b.add(side, f"{alg} {names[0]}", iso_dur(row[1]) * 1000, False, "ms")
        b.add(side, f"{alg} {names[1]}", iso_dur(row[2]) * 1000, False, "ms")
        for n, v in zip(names[2:], row[4:8]):
            b.add(side, f"{alg} {n}", num(v), True, "/s")


def p_simplify(b, side, lines):
    rows = [t.split("\t") for _, t in lines if re.match(r"^ *(DP|JTS VW|geo VW|VW)\(", t)]
    per_round = 11
    for i, row in enumerate(rows):
        if i < per_round:  # round 1 discarded
            continue
        alg = re.sub(r"^(JTS|geo) VW", "JTS/geo VW", row[0].strip())
        for n, v in zip(["2pt", "10pt", "50pt", "100pt", "10kpt"], row[1:6]):
            b.add(side, f"{alg} {n}", num(v), True, "/s")


def p_simple_readers(b, side, lines):
    for _, t in lines:
        m = re.search(r"^(water|natural_earth): (open(?: \(unzip\))?|total) in ([0-9.]+)s|^(water|natural_earth): total ([0-9.]+)s", t)
        if m:
            if m.group(1):
                b.add(side, f"{m.group(1)} {m.group(2)}", float(m.group(3)), False, "s")
            else:
                b.add(side, f"{m.group(4)} total", float(m.group(5)), False, "s")


E2E_STAGES = ["overall", "lake_centerlines", "water_polygons", "natural_earth", "osm_pass1", "osm_pass2", "ne_lakes",
              "boundaries", "agg_stop", "sort", "archive"]


def p_e2e(b, side, lines):
    seen = []
    for _, t in lines:
        m = re.search(r"\t(\w+) +([0-9.hms]+) cpu:([0-9.hms]+) avg:", t)
        if m and m.group(1) in E2E_STAGES and m.group(1) not in seen:
            seen.append(m.group(1))
            b.add(side, f"{m.group(1)} wall", dur(m.group(2)), False, "s")
            b.add(side, f"{m.group(1)} CPU", dur(m.group(3)), False, "s")
    # precise stage wall times from the harness's line timestamps: time from a stage's "Starting..." line to the
    # next stage's "Starting..." (or the end of the run). Java tags lines with [stage]; Rust's stages start in the
    # order the summary lists them (overall excluded).
    starts = []
    end = None
    for ts, t in lines:
        if ts is None:
            continue
        if t.rstrip().endswith("Starting..."):
            m = re.search(r"\[(\w+)\] - Starting", t)
            starts.append((m.group(1) if m else None, ts))
        if "FINISHED!" in t:
            end = ts
    if not starts or end is None:
        return
    order = [st for st in seen if st != "overall"]
    if all(n is None for n, _ in starts):
        if len(starts) != len(order):
            b.notes.append(f"{side}: {len(starts)} stage starts vs {len(order)} stages; no precise stage times")
            return
        starts = [(order[i], ts) for i, (_, ts) in enumerate(starts)]
    for i, (name, ts) in enumerate(starts):
        nxt = starts[i + 1][1] if i + 1 < len(starts) else end
        if name in E2E_STAGES:
            b.add(side, f"{name} wall (log timestamps)", nxt - ts, False, "s")


PARSERS = {
    "tile_coord": (rounds_rate(r"took .*\(([0-9.]+[kMBG]?)/s\)", "z0-z14 encode+decode"), "3 rounds, round 1 dropped"),
    "var_int": (rounds_rate(r"varints took .*\(([0-9.]+[kMBG]?)/s\)", "100M varints encode+decode"),
                "3 rounds, round 1 dropped"),
    "pmtiles": (rounds_rate(r"entries took .*\(([0-9.]+[kMBG]?)/s\)", "60M dir entries encode+decode"),
                "3 rounds, round 1 dropped"),
    "mbtiles_writer": (stats_line("tileWritesPerSecondsStats", "tile writes/s"),
                       "10 reps of 1M tiles; the program reports only aggregate stats (Java's include JIT warmup in rep 1)"),
    "mbtiles_read": (stats_line("readOperationsPerSecondStats", "tile reads/s"),
                     "3 pre-warm reps dropped by the program, 10 measured reps of 500k random reads"),
    "long_long_map_sparsearray_ram": (p_long_long_map, "single write pass of 100M entries, then 30s of reads"),
    "long_long_map_sparsearray_mmap": (p_long_long_map, "single write pass of 100M entries, then 30s of reads"),
    "long_long_map_array_mmap": (p_long_long_map, "single write pass of 100M entries, then 30s of reads"),
    "vector_tile": (p_vector_tile, "program warms up with iterations/4 tile encodes first"),
    "osm_read": (p_osm_read, "6 passes per process, passes 1-2 dropped; Java wall from line timestamps"),
    "osm_reader_null": (p_osm_reader, "3 iterations per process, iteration 1 dropped"),
    "osm_reader_emit": (p_osm_reader, "3 iterations per process, iteration 1 dropped"),
    "tile_archive_writer": (p_tile_archive_writer, "5 archive rounds per process, round 0 dropped"),
    "line_splitter": (p_line_splitter, "10 rounds, rounds 1-2 dropped; ms per run, lower is better"),
    "line_merge": (p_line_merge, "4 rounds (Java stopped after 4 of its 10), round 1 dropped"),
    "simplify": (p_simplify, "4 rounds (Java stopped after 4 of its 10), round 1 dropped; "
                             "'JTS/geo VW' rows compare JTS VWSimplifier (Java) with geo's SimplifyVw (Rust)"),
    "simple_readers": (p_simple_readers, "Rust only; 1 iteration per process"),
    "e2e_monaco": (p_e2e, "whole process per trial; stage times from the planetiler summary (rounded by its formatter)"),
    "e2e_switzerland": (p_e2e, "whole process per trial; stage times from the planetiler summary"),
}


def fmt(v, unit):
    if unit in ("s",):
        return f"{v:.3g}s" if v < 10 else f"{v:.1f}s"
    if unit == "ms":
        return f"{v:.3g}ms"
    a = abs(v)
    for div, suf in ((1e9, "B"), (1e6, "M"), (1e3, "k")):
        if a >= div:
            return f"{v / div:.3g}{suf}"
    return f"{v:.3g}"


def cell(vals, unit):
    if not vals:
        return "-"
    med = statistics.median(vals)
    if len(vals) == 1:
        return fmt(med, unit)
    return f"{fmt(med, unit)} [{fmt(min(vals), unit)}-{fmt(max(vals), unit)}]"


def gb(v):
    return f"{v:.2f}GB" if v >= 1 else f"{v * 1000:.0f}MB"


def cell_bytes(vals):
    if not vals:
        return "-"
    med = statistics.median(vals)
    return f"{gb(med)} [{gb(min(vals))}-{gb(max(vals))}]" if len(vals) > 1 else gb(med)


def speedup(m):
    j, r = m.values["java"], m.values["rust"]
    if not j or not r:
        return "-"
    jm, rm = statistics.median(j), statistics.median(r)
    if jm == 0 or rm == 0:
        return "-"
    s = rm / jm if m.higher_better else jm / rm
    return f"**{s:.2f}x**" if s < 1 else f"{s:.2f}x"


def main(out):
    benches = sorted(d for d in os.listdir(out) if os.path.isdir(os.path.join(out, d)))
    order = list(PARSERS)
    benches.sort(key=lambda d: order.index(d) if d in order else 999)
    try:
        print("```\n" + open(os.path.join(out, "env.txt")).read().strip() + "\n```\n")
    except OSError:
        pass
    for name in benches:
        d = os.path.join(out, name)
        parser, warm = PARSERS.get(name, (None, ""))
        b = Bench()
        procs = {"java": [], "rust": []}
        loads, notquiet, killed, failed = [], 0, [], []
        trials = sorted({f.rsplit(".", 1)[0] for f in os.listdir(d) if f.endswith(".log")},
                        key=lambda s: (int(s.split(".")[1]), s))
        for trial in trials:
            side = trial.split(".")[0]
            lines = read_log(os.path.join(d, trial + ".log"))
            if parser:
                try:
                    parser(b, side, lines)
                except Exception as e:  # keep going; report it
                    b.notes.append(f"parse error in {trial}: {e!r}")
            procs[side].append(read_time(os.path.join(d, trial + ".time")))
            meta = read_meta(os.path.join(d, trial + ".meta"))
            m = re.search(r"load averages?: ([0-9.]+)", meta.get("uptime", ""))
            if m:
                loads.append(float(m.group(1)))
            if meta.get("quiet", "").startswith("NOT QUIET"):
                notquiet += 1
            if "killed" in meta:
                killed.append(trial)
            elif meta.get("exit", "0") != "0":
                failed.append(f"{trial} (exit {meta.get('exit')})")
            if "output_bytes" in meta:
                b.add(side, "output mbtiles size", float(meta["output_bytes"]) / 1e6, False, "MB")
        print(f"### {name}\n")
        print(f"Trials: {len(trials)} ({', '.join(trials)}). Steady state: {warm}.")
        if loads:
            print(f"Load avg (1 min) at trial start: {min(loads):.2f}-{max(loads):.2f}"
                  + (f"; **{notquiet} trial(s) started on a busy machine**" if notquiet else "") + ".")
        if killed:
            print(f"Stopped by the harness after enough rounds: {', '.join(killed)}.")
        if failed:
            print(f"**Failed:** {', '.join(failed)}.")
        for n in b.notes:
            print(f"- {n}")
        print()
        print("| Metric | Java median [min-max] | Rust median [min-max] | Rust speedup |")
        print("|---|---|---|---|")
        for m in b.metrics.values():
            unit = m.unit if m.unit != "MB" else ""
            label = m.name + ("" if m.higher_better else " (lower is better)")
            if m.unit == "MB":
                print(f"| {m.name} | {cell(m.values['java'], '')}MB | {cell(m.values['rust'], '')}MB | - |")
            else:
                print(f"| {label} | {cell(m.values['java'], unit)} | {cell(m.values['rust'], unit)} | {speedup(m)} |")
        for key, label, unit in (("wall", "process wall", "s"), ("cpu", "process CPU (user+sys)", "s"),
                                 ("rss", "process peak RSS", "GB")):
            jv = [p[key] for p in procs["java"] if key in p]
            rv = [p[key] for p in procs["rust"] if key in p]
            if unit == "GB":
                jv, rv = [v / 1e9 for v in jv], [v / 1e9 for v in rv]
                m = Metric(label, False, "")
                m.values = {"java": jv, "rust": rv}
                jc = cell_bytes(jv)
                rc = cell_bytes(rv)
                print(f"| {label} | {jc} | {rc} | {speedup(m)} |")
            else:
                m = Metric(label, False, unit)
                m.values = {"java": jv, "rust": rv}
                print(f"| {label} | {cell(jv, unit)} | {cell(rv, unit)} | {speedup(m)} |")
        print()


if __name__ == "__main__":
    main(sys.argv[1])
