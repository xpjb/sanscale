#!/usr/bin/env python3
"""Baseline dashboard or before/after comparison. Python stdlib only.

Never use instrumented durations as production timing. Work reports are optional
companions, matched by case name, parameters, corpus, and environment.
"""
import argparse
import html
import json
import math
from pathlib import Path
import statistics
import sys


def load(path, mode):
    data = json.loads(Path(path).read_text())
    if data.get("schema") != 1 or data.get("mode") != mode:
        raise ValueError(f"{path}: expected schema 1, mode {mode!r}")
    names = [r["name"] for r in data["results"]]
    if len(names) != len(set(names)):
        raise ValueError(f"{path}: duplicate scenario names")
    for row in data["results"]:
        if row["units"] <= 0 or not row["samples"]:
            raise ValueError(f"{path}: empty/invalid samples in {row['name']}")
    return data


def percentile(values, p):
    values = sorted(values)
    return values[max(0, math.ceil(len(values) * p) - 1)]


def distribution(row, field="total_ns"):
    vals = [s[field] / row["units"] for s in row["samples"] if s.get(field) is not None]
    if not vals:
        return None
    return {"min": min(vals), "p50": statistics.median(vals),
            "p95": percentile(vals, .95), "max": max(vals), "n": len(vals)}


def median_fields(row, category):
    if row is None:
        return {}
    samples = [s[category] for s in row["samples"] if s.get(category) is not None]
    keys = sorted({k for s in samples for k in s})
    return {k: statistics.median([s[k] for s in samples if k in s]) / row["units"] for k in keys}


def environmental_differences(before, after):
    differences = []
    for key in ("suite_version", "tier", "warmup", "corpora"):
        if before.get(key) != after.get(key):
            differences.append(key)
    a, b = before["metadata"], after["metadata"]
    for key in ("rustc", "os", "arch", "cpu", "kernel", "debug_assertions", "rustflags", "lock_sha256", "gpu"):
        if a.get(key) != b.get(key):
            differences.append("metadata." + key)
    # Font paths may differ; actual bytes and collection face must not.
    fonts = lambda d: [(f["role"], f["face_index"], f["sha256"]) for f in d["fonts"]]
    if fonts(a) != fonts(b):
        differences.append("font contents/face indices")
    return differences


def index(data):
    return {r["name"]: r for r in data["results"]} if data else {}


def compatible_rows(a, b):
    # Several fixtures evolve by sample index (fresh widths, clipping, cache
    # pressure). Different sample counts would cover different input histories.
    return (a["params"] == b["params"] and a["units"] == b["units"]
            and len(a["samples"]) == len(b["samples"]))


def checked_work(timing, work):
    if work is None:
        return {}
    diff = environmental_differences(timing, work)
    if diff:
        raise ValueError("timing/work companion mismatch: " + ", ".join(diff))
    # Work must describe this source/harness revision, not a different implementation.
    for key in ("source_sha256", "shader_sha256", "benchmark_sha256", "manifest_sha256", "fingerprint_scope"):
        if timing["metadata"].get(key) != work["metadata"].get(key):
            raise ValueError(f"timing/work companion mismatch: {key}")
    rows = index(work)
    for name, row in index(timing).items():
        if name not in rows or not compatible_rows(row, rows[name]):
            raise ValueError(f"timing/work companion lacks matching case {name}")
    return rows


def field_ranges(row, category):
    if row is None:
        return {}
    samples = [s[category] for s in row["samples"] if s.get(category) is not None]
    keys = sorted({k for s in samples for k in s})
    result = {}
    for key in keys:
        values = [s[key] / row["units"] for s in samples if key in s]
        result[key] = (statistics.median(values), min(values), max(values))
    return result


def counter_table(before, after=None, category="work"):
    a, b = field_ranges(before, category), field_ranges(after, category)
    lines = []
    def fmt(v):
        if v is None:
            return "—"
        number = lambda x: f"{x:,.3f}".rstrip("0").rstrip(".")
        median, low, high = v
        return number(median) + (f" [{number(low)} … {number(high)}]" if low != high else "")
    for key in sorted(a.keys() | b.keys()):
        old, new = a.get(key), b.get(key)
        # A rare eviction/rasterization must not disappear just because p50 is zero.
        if all(v is None or v == (0, 0, 0) for v in (old, new)):
            continue
        lines.append(f"<tr><td>{html.escape(key)}</td><td>{fmt(old)}</td>" +
                     (f"<td>{fmt(new)}</td>" if after else "") + "</tr>")
    return "<table><tr><th>" + category + "/op: median [min … max]</th><th>Baseline</th>" + ("<th>Candidate</th>" if after else "") + "</tr>" + "".join(lines) + "</table>"


def render(before, after=None, before_work=None, after_work=None, threshold=10, min_delta_us=1):
    a, b = index(before), index(after)
    wa, wb = checked_work(before, before_work), checked_work(after, after_work) if after else {}
    warnings = []
    if before["metadata"].get("debug_assertions"):
        warnings.append("Debug build: not a production latency baseline.")
    if after and before["metadata"].get("benchmark_sha256") != after["metadata"].get("benchmark_sha256"):
        warnings.append("Benchmark source changed. Review common cases; matching names do not prove identical measurement code.")
    if any(len(r["samples"]) < 20 for r in [*a.values(), *b.values()]):
        warnings.append("Fewer than 20 samples in some cases: p95 is a descriptive order statistic, not a reliable tail estimate.")
    gpu = before["metadata"].get("gpu")
    if gpu and gpu.get("device_type") == "Cpu":
        warnings.append("Software GPU results; not hardware GPU performance.")
    added = sorted(b.keys() - a.keys()) if after else []
    removed = sorted(a.keys() - b.keys()) if after else []
    names = sorted(a.keys() & b.keys()) if after else sorted(a)
    if after and not names:
        raise ValueError("no common scenarios to compare")
    rows, console, regressions = [], [], []
    for name in names:
        old, new = a[name], b.get(name)
        if new and not compatible_rows(old, new):
            raise ValueError(f"case inputs/units changed (including sample count): {name}")
        x, y = distribution(old), distribution(new) if new else None
        ratio = y["p50"] / x["p50"] if y and x["p50"] else None
        regressed = bool(y and ratio is not None and ratio > 1 + threshold/100 and (y["p50"]-x["p50"])/1000 >= min_delta_us)
        if regressed:
            regressions.append(name)
        status = "regression" if regressed else ""
        ratio_s = f"{ratio:.3f}×" if ratio is not None else "—"
        console.append(f"{name}: {x['p50']/1000:.3f} us" + (f" → {y['p50']/1000:.3f} us ({ratio_s})" if y else ""))
        maximum = max(x["p50"], y["p50"] if y else 0, 1)
        bars = f'<div class="bar before" style="width:{100*x["p50"]/maximum:.2f}%"></div>'
        if y:
            bars += f'<div class="bar after" style="width:{100*y["p50"]/maximum:.2f}%"></div>'
        details = '<pre>' + html.escape(json.dumps(old["params"], indent=2)) + '</pre>'
        details += f'<p>Total/op: min {x["min"]/1000:.3f}, max {x["max"]/1000:.3f} us' + (f' → min {y["min"]/1000:.3f}, max {y["max"]/1000:.3f} us' if y else '') + '</p>'

        for field in ("prepare_ns", "encode_ns", "submit_ns", "wait_ns", "gpu_pass_ns"):
            p, q = distribution(old, field), distribution(new, field) if new else None
            if p:
                details += f'<p>{field}: {p["p50"]/1000:.3f} us p50' + (f' → {q["p50"]/1000:.3f} us' if q else '') + '</p>'
        if name in wa:
            details += counter_table(wa[name], wb.get(name))
            details += counter_table(wa[name], wb.get(name), "memory")
            work_only = lambda row: [{"work":s["work"],"memory":s.get("memory")} for s in row["samples"]] if row else None
            details += '<details><summary>Raw work/allocation samples (separate run)</summary><pre>' + html.escape(json.dumps({"baseline":work_only(wa[name]),"candidate":work_only(wb.get(name))},indent=2)) + '</pre></details>'

        details += '<details><summary>Raw timing samples</summary><pre>' + html.escape(json.dumps({"baseline":old["samples"],"candidate":new["samples"] if new else None},indent=2)) + '</pre></details>'
        rows.append(f'<tr class="{status}"><td><details><summary>{html.escape(name)}</summary>{details}</details></td>' +
                    f'<td>{x["p50"]/1000:.3f}</td><td>{x["p95"]/1000:.3f}</td>' +
                    (f'<td>{y["p50"]/1000:.3f}</td><td>{y["p95"]/1000:.3f}</td><td>{ratio_s}</td>' if y else '') +
                    f'<td class="bars">{bars}</td></tr>')
    heading = "Sanscale performance comparison" if after else "Sanscale performance baseline"
    notes = "".join(f'<li>{html.escape(w)}</li>' for w in warnings)
    pending = "".join(f'<li>{html.escape(p)}</li>' for p in before.get("pending_features", []))
    meta = html.escape(json.dumps({"baseline":before["metadata"],"candidate":after["metadata"] if after else None},indent=2))
    result = f'''<!doctype html><meta charset="utf-8"><title>{heading}</title>
<style>body{{font:14px system-ui;margin:2em;color:#ddd;background:#14171c}}table{{border-collapse:collapse;width:100%}}th,td{{padding:.5em;border-bottom:1px solid #39404b;text-align:right;vertical-align:top}}td:first-child,th:first-child{{text-align:left}}summary{{cursor:pointer}}pre{{white-space:pre-wrap;font-size:12px}}.bar{{height:7px;margin:3px 0}}.before{{background:#6ab0ed}}.after{{background:#eab66b}}.bars{{min-width:110px}}.regression{{background:#50252a}}details table{{width:auto}}li{{margin:.3em}}a{{color:#8dc9ff}}</style>
<h1>{heading}</h1><p>Tier: {html.escape(before['tier'])}. Values are microseconds per operation. Bars are independently scaled per row: blue baseline, orange candidate.</p>
<p>Uninstrumented timings only. Counter/allocation details come from separate instrumented runs. GPU pass timestamps exclude atlas uploads; CPU fence wait is not GPU time. Nested phase timings must not be added to total.</p><ul>{notes}</ul>
<table><thead><tr><th>Case (expand for inputs, phases and work)</th><th>Base p50</th><th>Base p95</th>{'<th>New p50</th><th>New p95</th><th>Ratio</th>' if after else ''}<th>Relative p50</th></tr></thead><tbody>{''.join(rows)}</tbody></table>
<p>Timing flags: &gt;{threshold}% and at least {min_delta_us} us p50 increase. These are investigation prompts, not proof of a regression. Repeat runs before drawing conclusions.</p>
<h2>Unpaired cases</h2><pre>{html.escape(json.dumps({'added':added,'removed':removed},indent=2))}</pre>
<h2>Not yet implemented / not measured</h2><ul>{pending}</ul>
<details><summary>Environment, fonts, binary and source fingerprints</summary><pre>{meta}</pre></details>
'''
    return result, console, regressions


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("before")
    p.add_argument("after", nargs="?")
    p.add_argument("--before-work")
    p.add_argument("--after-work")
    p.add_argument("--html", default="perf-results/report.html")
    p.add_argument("--threshold", type=float, default=10)
    p.add_argument("--min-delta-us", type=float, default=1)
    p.add_argument("--allow-environment-mismatch", action="store_true")
    p.add_argument("--fail", action="store_true", help="opt-in nonzero status for flagged timings; noisy, not for unattended shared-host CI")
    args = p.parse_args()
    try:
        before = load(args.before, "timing")
        after = load(args.after, "timing") if args.after else None
        bw = load(args.before_work, "work") if args.before_work else None
        aw = load(args.after_work, "work") if args.after_work else None
        if aw and not after:
            raise ValueError("--after-work requires a candidate timing file")
        if after:
            differences = environmental_differences(before, after)
            if differences:
                message = "INCOMPARABLE ENVIRONMENTS: " + ", ".join(differences)
                if not args.allow_environment_mismatch:
                    raise ValueError(message + "; inspect or explicitly override")
                print(message, file=sys.stderr)
        page, lines, regressions = render(before, after, bw, aw, args.threshold, args.min_delta_us)
        if after and environmental_differences(before, after):
            page = page.replace("<h1>", "<p><strong>WARNING: environment mismatch explicitly allowed; timings are not controlled comparisons.</strong></p><h1>", 1)
        dest = Path(args.html)
        dest.parent.mkdir(parents=True, exist_ok=True)
        dest.write_text(page)
        print("\n".join(lines))
        print(f"\nWrote {dest}; {len(regressions)} timing flag(s)")
        return 1 if args.fail and regressions else 0
    except (ValueError, KeyError, OSError) as e:
        print(f"error: {e}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main())
