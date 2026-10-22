#!/usr/bin/env python3
# sorts calliope conformance reports into per-vector outcomes and compares them with the committed baseline
import html
import json
import sys
from pathlib import Path

USAGE = """usage:
  conformance-gate.py table <out.json> <backend>=<report.json>...
  conformance-gate.py differing <baseline.json> <table.json>
  conformance-gate.py verdict <baseline.json> <table.json> <retry.json> <summary.md> <title>
  conformance-gate.py page <out.html> <summary.md> [<table.json> <retry.json>]"""

# outcomes that fail the gate when a vector newly lands on them
FAILING = {"silent", "crash", "timeout", "device-lost", "harness", "missing"}
DETAIL_LIMIT = 200


def last_line(path):
    lines = [line.strip() for line in path.read_text(errors="replace").splitlines() if line.strip()]
    return lines[-1][:DETAIL_LIMIT] if lines else "no stderr"


def outcome(scenario):
    run = next(r for r in scenario["runs"] if r["engine"] == "g2g")
    status = run["status"]
    if status == "ok":
        expected = scenario["golden_expected"]
        if (run.get("output_md5") or "").lower() == expected.lower():
            return {"outcome": "pass"}
        if not run.get("output_len"):
            return {"outcome": "silent", "detail": "exit 0 with empty output"}
        return {"outcome": "silent", "detail": "exit 0 with an output md5 that differs"}
    if status == "timeout":
        return {"outcome": "timeout"}
    if "signaled" in status:
        return {"outcome": "crash", "detail": f"signal {status['signaled']['signal']}"}
    if "error" in status:
        return {"outcome": "harness", "detail": status["error"]["message"][:DETAIL_LIMIT]}
    stderr = Path(run["log_dir"]) / "stderr.log"
    if "DEVICE_LOST" in stderr.read_text(errors="replace"):
        return {"outcome": "device-lost", "detail": last_line(stderr)}
    return {"outcome": "refused", "detail": last_line(stderr)}


def table(out_path, pairs):
    result = {}
    for pair in pairs:
        backend, report_path = pair.split("=", 1)
        report = json.loads(Path(report_path).read_text())
        result[backend] = {s["scenario"]: outcome(s) for s in report["scenarios"]}
    Path(out_path).write_text(json.dumps(result, indent=1, sort_keys=True) + "\n")


def load(path):
    return json.loads(Path(path).read_text())


def outcome_of(table, backend, vector):
    return table.get(backend, {}).get(vector, {"outcome": "missing"})


def differing(baseline, current):
    for backend, vectors in sorted(baseline.items()):
        for vector, expected in sorted(vectors.items()):
            if outcome_of(current, backend, vector)["outcome"] != expected["outcome"]:
                yield backend, vector


def describe(backend, vector, before, after):
    detail = f" ({after['detail']})" if "detail" in after else ""
    return f"- {backend} `{vector}`: {before['outcome']} -> {after['outcome']}{detail}"


def verdict(baseline, current, retry, summary_path, title):
    regressions, improvements, changes, flakes = [], [], [], []
    for backend, vector in differing(baseline, current):
        before = baseline[backend][vector]
        first = outcome_of(current, backend, vector)
        after = outcome_of(retry, backend, vector)
        if after["outcome"] == before["outcome"]:
            flakes.append(describe(backend, vector, before, first) + ", the rerun matched the baseline")
            continue
        line = describe(backend, vector, before, after)
        if before["outcome"] == "pass" or after["outcome"] in FAILING:
            regressions.append(line)
        elif after["outcome"] == "pass":
            improvements.append(line)
        else:
            changes.append(line)
    added = [
        f"- {backend} `{vector}`: {result['outcome']}"
        for backend, vectors in sorted(current.items())
        for vector, result in sorted(vectors.items())
        if vector not in baseline.get(backend, {})
    ]
    lines = [title, ""]
    for backend, vectors in sorted(current.items()):
        passed = sum(r["outcome"] == "pass" for r in vectors.values())
        baseline_passed = sum(r["outcome"] == "pass" for r in baseline.get(backend, {}).values())
        lines.append(f"- {backend}: {passed} / {len(vectors)} pass, baseline {baseline_passed}")
    for heading, entries in [
        ("Regressions", regressions),
        ("Flakes", flakes),
        ("Improvements, accept them into the baseline", improvements),
        ("Other outcome changes", changes),
        ("Not in the baseline", added),
    ]:
        if entries:
            lines += ["", f"## {heading}", "", *entries]
    Path(summary_path).write_text("\n".join(lines) + "\n")
    return 1 if regressions else 0


def page(out_path, summary_path, current, retry):
    rows = []
    for backend, vectors in sorted(current.items()):
        for vector in sorted(vectors):
            result = retry.get(backend, {}).get(vector, vectors[vector])
            detail = html.escape(result.get("detail", ""), quote=True)
            rows.append(
                f"<tr><td>{html.escape(backend)}</td><td>{html.escape(vector)}</td>"
                f'<td class="{result["outcome"]}" title="{detail}">{result["outcome"]}</td></tr>'
            )
    summary = html.escape(Path(summary_path).read_text())
    Path(out_path).write_text(f"""<!doctype html>
<meta charset="utf-8">
<title>g2g GPU decoder conformance</title>
<style>
body {{ font-family: sans-serif; margin: 2em; }}
td {{ padding: 0 1em 0 0; }}
.pass {{ color: #2e9e44; }}
.refused {{ color: #888; }}
.silent, .crash, .timeout, .device-lost, .harness, .missing {{ color: #c62828; font-weight: bold; }}
</style>
<h1>g2g GPU decoder conformance</h1>
<pre>{summary}</pre>
<table>
<tr><th>decoder</th><th>vector</th><th>outcome</th></tr>
{chr(10).join(rows)}
</table>
""")


def main(args):
    if args[:1] == ["table"] and len(args) >= 3:
        table(args[1], args[2:])
        return 0
    if args[:1] == ["differing"] and len(args) == 3:
        for backend, vector in differing(load(args[1]), load(args[2])):
            print(backend, vector)
        return 0
    if args[:1] == ["verdict"] and len(args) == 6:
        return verdict(load(args[1]), load(args[2]), load(args[3]), args[4], args[5])
    if args[:1] == ["page"] and len(args) == 3:
        page(args[1], args[2], {}, {})
        return 0
    if args[:1] == ["page"] and len(args) == 5:
        page(args[1], args[2], load(args[3]), load(args[4]))
        return 0
    raise SystemExit(USAGE)


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
