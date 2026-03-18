#!/usr/bin/env python3
"""Analyze benchmark session data and produce a regression report.

Parses session.jsonl files from bench/results/sessions/{task}/{mode}_run{N}/,
compares CLI (wonk) vs baseline runs, and identifies token regressions.

Usage:
    python3 bench/analyze_sessions.py                            # analyze all
    python3 bench/analyze_sessions.py --task httpx_request_flow  # analyze one task
    python3 bench/analyze_sessions.py --threshold 1.2            # only show >1.2x
    python3 bench/analyze_sessions.py -o report.md               # write to file
"""

import argparse
import json
import os
import sys
from collections import defaultdict
from pathlib import Path
from statistics import median


# ---------------------------------------------------------------------------
# Data structures
# ---------------------------------------------------------------------------

class APICall:
    """One logical API request (aggregated across streaming entries)."""
    __slots__ = (
        "request_id", "input_tokens", "cache_creation", "cache_read",
        "output_tokens", "tool_calls", "tool_results",
    )

    def __init__(self, request_id):
        self.request_id = request_id
        self.input_tokens = 0
        self.cache_creation = 0
        self.cache_read = 0
        self.output_tokens = 0
        self.tool_calls = []      # list of (tool_name, summary_str)
        self.tool_results = []    # list of (tool_use_id, size_in_chars)

    @property
    def total_tokens(self):
        return self.input_tokens + self.cache_creation + self.cache_read + self.output_tokens


class SessionStats:
    """Aggregated stats for one session run."""
    __slots__ = (
        "path", "api_calls", "total_input", "total_cache_creation",
        "total_cache_read", "total_output", "total_tokens",
        "tool_call_count", "total_result_chars",
    )

    def __init__(self, path, api_calls):
        self.path = path
        self.api_calls = api_calls
        self.total_input = sum(c.input_tokens for c in api_calls)
        self.total_cache_creation = sum(c.cache_creation for c in api_calls)
        self.total_cache_read = sum(c.cache_read for c in api_calls)
        self.total_output = sum(c.output_tokens for c in api_calls)
        self.total_tokens = sum(c.total_tokens for c in api_calls)
        self.tool_call_count = sum(len(c.tool_calls) for c in api_calls)
        self.total_result_chars = sum(
            size for c in api_calls for _, size in c.tool_results
        )


# ---------------------------------------------------------------------------
# Tool call summarisation
# ---------------------------------------------------------------------------

def _summarize_tool(name, inp):
    """Return a short human-readable summary of a tool invocation."""
    if name == "Bash":
        cmd = inp.get("command", "")
        if len(cmd) > 80:
            cmd = cmd[:77] + "..."
        return f"Bash: {cmd}"

    if name == "Grep":
        pat = inp.get("pattern", "")
        path = inp.get("path", "")
        if path:
            path = _short_path(path)
        parts = [f"Grep: /{pat}/"]
        if path:
            parts.append(path)
        return " ".join(parts)

    if name == "Read":
        fp = _short_path(inp.get("file_path", ""))
        extras = []
        if inp.get("offset"):
            extras.append(f"@{inp['offset']}")
        if inp.get("limit"):
            extras.append(f"limit={inp['limit']}")
        suffix = f" ({', '.join(extras)})" if extras else ""
        return f"Read: {fp}{suffix}"

    if name == "Glob":
        pat = inp.get("pattern", "")
        path = inp.get("path", "")
        if path:
            path = _short_path(path)
            return f"Glob: {pat} in {path}"
        return f"Glob: {pat}"

    if name == "Agent":
        desc = inp.get("description", "")
        if len(desc) > 60:
            desc = desc[:57] + "..."
        return f"Agent: {desc}"

    if name == "ToolSearch":
        return f"ToolSearch: {inp.get('query', '')}"

    if name == "Skill":
        return f"Skill: {inp.get('skill', '')}"

    # MCP wonk tools
    if name.startswith("mcp__wonk__"):
        short = name.replace("mcp__wonk__", "")
        args = " ".join(f"{k}={v}" for k, v in inp.items() if v is not None and v != "")
        if len(args) > 80:
            args = args[:77] + "..."
        return f"{short}: {args}"

    # Fallback
    return name


def _short_path(p):
    """Shorten an absolute path for display."""
    if not p:
        return ""
    # Strip common bench/repos prefix
    idx = p.find("/bench/repos/")
    if idx != -1:
        return p[idx + len("/bench/repos/"):]
    # Keep last 3 components
    parts = p.split("/")
    if len(parts) > 3:
        return ".../" + "/".join(parts[-3:])
    return p


# ---------------------------------------------------------------------------
# Session parsing
# ---------------------------------------------------------------------------

def _tool_result_size(content):
    """Compute character count of a tool_result content field."""
    if isinstance(content, str):
        return len(content)
    if isinstance(content, list):
        total = 0
        for item in content:
            if isinstance(item, dict):
                if item.get("type") == "text":
                    total += len(item.get("text", ""))
                elif item.get("type") == "tool_reference":
                    # Minimal overhead
                    total += len(item.get("tool_name", ""))
                else:
                    total += len(json.dumps(item))
            else:
                total += len(str(item))
        return total
    return 0


def parse_session(path):
    """Parse a session.jsonl file and return a SessionStats."""
    entries = []
    try:
        with open(path) as f:
            for line in f:
                line = line.strip()
                if not line:
                    continue
                try:
                    entries.append(json.loads(line))
                except json.JSONDecodeError:
                    continue
    except (OSError, IOError):
        return None

    # Pass 1: aggregate assistant entries by requestId
    req_data = {}   # requestId -> {token fields max'd, tool_calls list}
    req_order = []  # preserve order of first appearance

    # Also build tool_use_id -> requestId mapping
    tool_use_to_req = {}

    for entry in entries:
        if entry.get("type") != "assistant":
            continue
        msg = entry.get("message", {})
        rid = entry.get("requestId")
        if not rid:
            continue

        usage = msg.get("usage", {})
        if rid not in req_data:
            req_data[rid] = {
                "input_tokens": 0,
                "cache_creation": 0,
                "cache_read": 0,
                "output_tokens": 0,
                "tool_calls": [],
            }
            req_order.append(rid)

        rd = req_data[rid]
        # Take max of each token field per requestId (streaming accumulates)
        rd["input_tokens"] = max(rd["input_tokens"], usage.get("input_tokens", 0))
        rd["cache_creation"] = max(
            rd["cache_creation"], usage.get("cache_creation_input_tokens", 0)
        )
        rd["cache_read"] = max(
            rd["cache_read"], usage.get("cache_read_input_tokens", 0)
        )
        rd["output_tokens"] = max(rd["output_tokens"], usage.get("output_tokens", 0))

        # Extract tool_use blocks
        content = msg.get("content", [])
        if isinstance(content, list):
            for block in content:
                if isinstance(block, dict) and block.get("type") == "tool_use":
                    tool_name = block.get("name", "unknown")
                    tool_input = block.get("input", {})
                    tool_id = block.get("id", "")
                    summary = _summarize_tool(tool_name, tool_input)
                    rd["tool_calls"].append((tool_name, summary))
                    if tool_id:
                        tool_use_to_req[tool_id] = rid

    # Pass 2: extract tool results from user entries
    req_results = defaultdict(list)  # requestId -> [(tool_use_id, size)]

    for entry in entries:
        if entry.get("type") != "user":
            continue
        msg = entry.get("message", {})
        content = msg.get("content", [])
        if not isinstance(content, list):
            continue
        for block in content:
            if not isinstance(block, dict):
                continue
            if block.get("type") != "tool_result":
                continue
            tuid = block.get("tool_use_id", "")
            result_content = block.get("content", "")
            size = _tool_result_size(result_content)
            rid = tool_use_to_req.get(tuid)
            if rid:
                req_results[rid].append((tuid, size))

    # Build APICall objects
    api_calls = []
    for rid in req_order:
        rd = req_data[rid]
        call = APICall(rid)
        call.input_tokens = rd["input_tokens"]
        call.cache_creation = rd["cache_creation"]
        call.cache_read = rd["cache_read"]
        call.output_tokens = rd["output_tokens"]
        call.tool_calls = rd["tool_calls"]
        call.tool_results = req_results.get(rid, [])
        api_calls.append(call)

    return SessionStats(path, api_calls)


# ---------------------------------------------------------------------------
# Discovery
# ---------------------------------------------------------------------------

def discover_sessions(base_dir):
    """Find all tasks and their CLI/baseline runs.

    Returns: {task_name: {"cli": [SessionStats], "baseline": [SessionStats]}}
    """
    sessions_dir = os.path.join(base_dir, "results", "sessions")
    if not os.path.isdir(sessions_dir):
        return {}

    tasks = {}
    for task_name in sorted(os.listdir(sessions_dir)):
        task_dir = os.path.join(sessions_dir, task_name)
        if not os.path.isdir(task_dir):
            continue

        cli_runs = []
        wonk_runs = []
        baseline_runs = []

        for run_dir in sorted(os.listdir(task_dir)):
            session_path = os.path.join(task_dir, run_dir, "session.jsonl")
            if not os.path.isfile(session_path):
                continue
            stats = parse_session(session_path)
            if stats is None:
                continue
            if run_dir.startswith("cli_"):
                cli_runs.append(stats)
            elif run_dir.startswith("wonk_"):
                wonk_runs.append(stats)
            elif run_dir.startswith("baseline_"):
                baseline_runs.append(stats)

        # Prefer cli_runs over wonk_runs when both exist.
        treatment_runs = cli_runs if cli_runs else wonk_runs
        if treatment_runs or baseline_runs:
            tasks[task_name] = {"cli": treatment_runs, "baseline": baseline_runs}

    return tasks


# ---------------------------------------------------------------------------
# Regression analysis
# ---------------------------------------------------------------------------

def find_regressions(tasks, threshold):
    """Identify tasks where CLI median tokens > baseline median * threshold.

    Returns list of dicts sorted by ratio descending.
    """
    regressions = []

    for task_name, runs in sorted(tasks.items()):
        cli_runs = runs["cli"]
        baseline_runs = runs["baseline"]

        if not cli_runs or not baseline_runs:
            continue

        cli_tokens = [s.total_tokens for s in cli_runs]
        base_tokens = [s.total_tokens for s in baseline_runs]

        cli_med = median(cli_tokens)
        base_med = median(base_tokens)

        if base_med == 0:
            continue

        ratio = cli_med / base_med

        if ratio >= threshold:
            # Pick the CLI run closest to median
            cli_repr = min(cli_runs, key=lambda s: abs(s.total_tokens - cli_med))
            base_repr = min(baseline_runs, key=lambda s: abs(s.total_tokens - base_med))

            regressions.append({
                "task": task_name,
                "cli_median": cli_med,
                "base_median": base_med,
                "ratio": ratio,
                "cli_runs": cli_runs,
                "base_runs": baseline_runs,
                "cli_repr": cli_repr,
                "base_repr": base_repr,
                "cli_api_calls": len(cli_repr.api_calls),
                "base_api_calls": len(base_repr.api_calls),
                "cli_tool_calls": cli_repr.tool_call_count,
                "base_tool_calls": base_repr.tool_call_count,
            })

    regressions.sort(key=lambda r: r["ratio"], reverse=True)
    return regressions


# ---------------------------------------------------------------------------
# Report formatting
# ---------------------------------------------------------------------------

def _fmt_tokens(n):
    """Format token count with thousands separator."""
    if n >= 1_000_000:
        return f"{n / 1_000_000:.1f}M"
    if n >= 10_000:
        return f"{n / 1_000:.1f}k"
    return str(int(n))


def _fmt_chars(n):
    """Format character count."""
    if n >= 1_000_000:
        return f"{n / 1_000_000:.1f}M"
    if n >= 1_000:
        return f"{n / 1_000:.1f}k"
    return str(n)


def _call_flow_table(stats, label):
    """Render a per-API-call breakdown table."""
    lines = []
    lines.append(f"**{label}** ({len(stats.api_calls)} API calls, "
                 f"{_fmt_tokens(stats.total_tokens)} total tokens)")
    lines.append("")
    lines.append(
        "| # | Input | Cache Create | Cache Read | Output "
        "| Tool + Command | Result Chars |"
    )
    lines.append(
        "|---|------:|-------------:|-----------:|-------:"
        "|----------------|-------------:|"
    )

    for i, call in enumerate(stats.api_calls, 1):
        tool_strs = [s for _, s in call.tool_calls]
        tool_col = "; ".join(tool_strs) if tool_strs else "(thinking/text)"

        result_sizes = [size for _, size in call.tool_results]
        result_col = _fmt_chars(sum(result_sizes)) if result_sizes else "-"

        # Escape pipes in tool descriptions
        tool_col = tool_col.replace("|", "\\|")

        lines.append(
            f"| {i} "
            f"| {_fmt_tokens(call.input_tokens)} "
            f"| {_fmt_tokens(call.cache_creation)} "
            f"| {_fmt_tokens(call.cache_read)} "
            f"| {_fmt_tokens(call.output_tokens)} "
            f"| {tool_col} "
            f"| {result_col} |"
        )

    lines.append("")
    return "\n".join(lines)


def _comparison_section(reg):
    """Render comparison details for a regression."""
    cli = reg["cli_repr"]
    base = reg["base_repr"]
    lines = []

    lines.append("**Comparison Summary**")
    lines.append("")

    api_delta = reg["cli_api_calls"] - reg["base_api_calls"]
    tool_delta = reg["cli_tool_calls"] - reg["base_tool_calls"]

    base_chars = base.total_result_chars if base.total_result_chars > 0 else 1
    chars_ratio = cli.total_result_chars / base_chars

    cache_delta = cli.total_cache_read - base.total_cache_read

    lines.append(f"| Metric | CLI (wonk) | Baseline | Delta |")
    lines.append(f"|--------|------------|----------|-------|")
    lines.append(
        f"| API Calls | {reg['cli_api_calls']} | {reg['base_api_calls']} "
        f"| {api_delta:+d} |"
    )
    lines.append(
        f"| Tool Calls | {reg['cli_tool_calls']} | {reg['base_tool_calls']} "
        f"| {tool_delta:+d} |"
    )
    lines.append(
        f"| Result Chars | {_fmt_chars(cli.total_result_chars)} "
        f"| {_fmt_chars(base.total_result_chars)} "
        f"| {chars_ratio:.2f}x |"
    )
    lines.append(
        f"| Cache Read | {_fmt_tokens(cli.total_cache_read)} "
        f"| {_fmt_tokens(base.total_cache_read)} "
        f"| {_fmt_tokens(cache_delta)} |"
    )
    lines.append(
        f"| Total Tokens | {_fmt_tokens(cli.total_tokens)} "
        f"| {_fmt_tokens(base.total_tokens)} "
        f"| {reg['ratio']:.2f}x |"
    )
    lines.append("")

    # Wonk-specific commands
    wonk_calls = []
    for call in cli.api_calls:
        for name, summary in call.tool_calls:
            if name.startswith("mcp__wonk__"):
                result_sizes = [size for _, size in call.tool_results]
                total_result = sum(result_sizes)
                wonk_calls.append((summary, total_result))

    if wonk_calls:
        lines.append("**Wonk Tool Calls**")
        lines.append("")
        lines.append("| Command | Result Size |")
        lines.append("|---------|------------|")
        for summary, rsize in wonk_calls:
            lines.append(f"| {summary} | {_fmt_chars(rsize)} |")
        lines.append("")

    # Largest tool results
    all_results = []
    for call in cli.api_calls:
        for name, summary in call.tool_calls:
            for _, size in call.tool_results:
                all_results.append((summary, size))

    if all_results:
        all_results.sort(key=lambda x: x[1], reverse=True)
        top = all_results[:5]
        lines.append("**Largest Tool Results (CLI)**")
        lines.append("")
        lines.append("| Tool + Command | Result Chars |")
        lines.append("|----------------|-------------|")
        for summary, size in top:
            lines.append(f"| {summary} | {_fmt_chars(size)} |")
        lines.append("")

    # Cache analysis
    lines.append("**Cache Analysis**")
    lines.append("")
    lines.append(
        f"- CLI cache creation: {_fmt_tokens(cli.total_cache_creation)} tokens"
    )
    lines.append(
        f"- CLI cache read: {_fmt_tokens(cli.total_cache_read)} tokens"
    )
    lines.append(
        f"- Baseline cache creation: {_fmt_tokens(base.total_cache_creation)} tokens"
    )
    lines.append(
        f"- Baseline cache read: {_fmt_tokens(base.total_cache_read)} tokens"
    )
    cache_eff_cli = (
        cli.total_cache_read / (cli.total_cache_read + cli.total_cache_creation)
        if (cli.total_cache_read + cli.total_cache_creation) > 0
        else 0
    )
    cache_eff_base = (
        base.total_cache_read / (base.total_cache_read + base.total_cache_creation)
        if (base.total_cache_read + base.total_cache_creation) > 0
        else 0
    )
    lines.append(
        f"- CLI cache hit rate: {cache_eff_cli:.1%}"
    )
    lines.append(
        f"- Baseline cache hit rate: {cache_eff_base:.1%}"
    )
    lines.append("")

    return "\n".join(lines)


def generate_report(tasks, threshold):
    """Generate the full markdown regression report."""
    regressions = find_regressions(tasks, threshold)
    lines = []

    lines.append("# Session Regression Report")
    lines.append("")

    if not regressions:
        lines.append(
            f"No regressions found at threshold {threshold:.2f}x "
            f"across {len(tasks)} tasks."
        )
        lines.append("")
        # Still show summary of all tasks
        _append_all_tasks_summary(lines, tasks)
        return "\n".join(lines)

    lines.append(
        f"Found **{len(regressions)}** regression(s) at threshold "
        f"**{threshold:.2f}x** across {len(tasks)} tasks."
    )
    lines.append("")

    # ---- Summary table ----
    lines.append("## Regression Summary")
    lines.append("")
    lines.append(
        "| Task | CLI Tokens | Baseline Tokens | Ratio "
        "| CLI Calls | Base Calls |"
    )
    lines.append(
        "|------|----------:|----------------:|------:"
        "|----------:|-----------:|"
    )
    for reg in regressions:
        lines.append(
            f"| {reg['task']} "
            f"| {_fmt_tokens(reg['cli_median'])} "
            f"| {_fmt_tokens(reg['base_median'])} "
            f"| **{reg['ratio']:.2f}x** "
            f"| {reg['cli_api_calls']} "
            f"| {reg['base_api_calls']} |"
        )
    lines.append("")

    # ---- Per-regression detail ----
    for i, reg in enumerate(regressions, 1):
        lines.append(f"---")
        lines.append("")
        lines.append(
            f"## {i}. {reg['task']} ({reg['ratio']:.2f}x regression)"
        )
        lines.append("")

        # Run variance
        cli_tokens = [s.total_tokens for s in reg["cli_runs"]]
        base_tokens = [s.total_tokens for s in reg["base_runs"]]
        lines.append(
            f"CLI runs: {', '.join(_fmt_tokens(t) for t in cli_tokens)} "
            f"(median {_fmt_tokens(reg['cli_median'])})"
        )
        lines.append(
            f"Baseline runs: {', '.join(_fmt_tokens(t) for t in base_tokens)} "
            f"(median {_fmt_tokens(reg['base_median'])})"
        )
        lines.append("")

        # Call flow tables
        lines.append("### CLI Call Flow")
        lines.append("")
        lines.append(_call_flow_table(reg["cli_repr"], "CLI (wonk)"))

        lines.append("### Baseline Call Flow")
        lines.append("")
        lines.append(_call_flow_table(reg["base_repr"], "Baseline"))

        # Comparison
        lines.append("### Comparison")
        lines.append("")
        lines.append(_comparison_section(reg))

    # ---- All tasks overview ----
    _append_all_tasks_summary(lines, tasks)

    return "\n".join(lines)


def _append_all_tasks_summary(lines, tasks):
    """Append a summary table of all tasks (not just regressions)."""
    lines.append("---")
    lines.append("")
    lines.append("## All Tasks Overview")
    lines.append("")
    lines.append(
        "| Task | CLI Median | Base Median | Ratio "
        "| CLI Runs | Base Runs |"
    )
    lines.append(
        "|------|----------:|------------:|------:"
        "|---------:|----------:|"
    )
    for task_name in sorted(tasks.keys()):
        runs = tasks[task_name]
        cli_runs = runs["cli"]
        base_runs = runs["baseline"]

        cli_med = median([s.total_tokens for s in cli_runs]) if cli_runs else 0
        base_med = median([s.total_tokens for s in base_runs]) if base_runs else 0
        ratio = cli_med / base_med if base_med > 0 else float("inf") if cli_med > 0 else 0

        ratio_str = f"{ratio:.2f}x" if ratio != float("inf") else "N/A"

        lines.append(
            f"| {task_name} "
            f"| {_fmt_tokens(cli_med)} "
            f"| {_fmt_tokens(base_med) if base_med > 0 else 'N/A'} "
            f"| {ratio_str} "
            f"| {len(cli_runs)} "
            f"| {len(base_runs)} |"
        )
    lines.append("")


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

def main():
    parser = argparse.ArgumentParser(
        description="Analyze benchmark sessions and produce a regression report."
    )
    parser.add_argument(
        "--task",
        help="Analyze only this task (by name).",
    )
    parser.add_argument(
        "--threshold",
        type=float,
        default=1.0,
        help="Only show regressions above this ratio (default: 1.0).",
    )
    parser.add_argument(
        "-o", "--output",
        help="Write report to this file instead of stdout.",
    )

    args = parser.parse_args()

    # Resolve base directory: script lives at bench/analyze_sessions.py
    script_dir = Path(__file__).resolve().parent
    base_dir = script_dir

    tasks = discover_sessions(str(base_dir))

    if not tasks:
        print(f"No sessions found under {base_dir}/results/sessions/", file=sys.stderr)
        sys.exit(1)

    if args.task:
        if args.task not in tasks:
            available = ", ".join(sorted(tasks.keys()))
            print(
                f"Task '{args.task}' not found. Available: {available}",
                file=sys.stderr,
            )
            sys.exit(1)
        tasks = {args.task: tasks[args.task]}

    report = generate_report(tasks, args.threshold)

    if args.output:
        with open(args.output, "w") as f:
            f.write(report)
        print(f"Report written to {args.output}", file=sys.stderr)
    else:
        print(report)


if __name__ == "__main__":
    main()
