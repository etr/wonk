#!/usr/bin/env python3
"""
Agent Benchmark: Claude Code with vs without wonk MCP tools.

Runs Claude Code headless (claude -p) on real coding tasks, once with wonk MCP
tools and once with only built-in tools, then compares total session token usage.

Usage:
    python3 bench/agent_bench.py                           # full suite, 3 runs, sonnet
    python3 bench/agent_bench.py --tasks flask_find_blueprint --runs 1 --model haiku
    python3 bench/agent_bench.py --category symbol_location --runs 1

IMPORTANT: Must be run from a regular terminal, NOT from within a Claude Code
session (nesting guard prevents claude -p inside claude).
"""

import argparse
import csv
import json
import os
import shutil
import subprocess
import sys
import uuid
from collections import Counter
from dataclasses import dataclass, field
from pathlib import Path
from statistics import mean, median, stdev

SCRIPT_DIR = Path(__file__).resolve().parent
REPO_DIR = SCRIPT_DIR / "repos"
RESULTS_DIR = SCRIPT_DIR / "results"
TASKS_FILE = SCRIPT_DIR / "tasks.json"
NO_MCP_CONFIG = SCRIPT_DIR / "no-mcp.json"
SESSIONS_DIR = RESULTS_DIR / "sessions"
WONK_PLUGIN_DIR = Path(
    os.environ.get("WONK_PLUGIN_DIR", SCRIPT_DIR / ".." / ".." / "wonk-plugin")
).resolve()

SYSTEM_PROMPT = (
    "You are a code exploration assistant. "
    "Answer questions about this codebase by examining the actual source code. "
    "Do NOT answer from general knowledge — always verify against the code. "
    "Be concise. Use the minimum number of tool calls needed to answer accurately."
)

WONK_TOOL_GUIDE = """\
<system-reminder>
Wonk code search tools available via ToolSearch. NEVER Read a file wonk returned. STOP after 1-2 calls.

Simple questions (1-2 calls) — use wonk tools directly:
- "What is X?" → wonk_show(name="X") — batch: "X,Y,Z"; class methods: "Class.method"
- "Who calls X?" → wonk_callers(name="X")
- "What references X?" → wonk_ref(name="X")
- "Module overview" → wonk_summary(path="dir/", depth=1, budget=8000)
- "Everything about X" → wonk_context(name="X")

Complex questions (tracing flows, multi-file architecture) — use Agent:
For "How does X handle Y?", "Trace the flow", "How does error handling work?" — spawn an Explore subagent that uses wonk tools internally. This keeps research cost off your main context.

ONLY use wonk_search when you have NO function/class name to start from.
</system-reminder>"""

WONK_CLI_GUIDE = """\
<system-reminder>
You have `wonk` CLI. NEVER pipe (`| head`), NEVER redirect (`2>/dev/null`). NEVER Read a file wonk already returned.

## Route FIRST, then act — every question gets exactly ONE path:

**Path A — Simple (1 call, then answer):** Single factual lookup. "What is X?", "Who calls X?", "Show me X."
  `wonk show X --budget 4000` / `wonk callers X` / `wonk ref X` / `wonk context X`
  wonk results are comprehensive — they cover all indexed source files (tests excluded by design). Answer directly from wonk output. Do NOT cross-check with Grep or wonk search.

**Path B — Complex (use Agent immediately):** Anything with "and", "explain how", "trace", "how does X work", or needing 2+ files. ALWAYS spawn an Explore subagent on the FIRST call — do NOT run wonk yourself first.
  Agent(subagent_type="Explore", prompt="In <repo>, <task>. Use wonk via Bash: `wonk show`, `wonk search`, `wonk callers`. Summarize findings.")

Classify the question BEFORE making any tool call. If in doubt, use Path B — an unnecessary Agent is cheaper than extra main-thread calls.
</system-reminder>"""


def build_wonk_prompt(prompt: str) -> str:
    """Prepend wonk tool guide so the agent knows to search for wonk tools."""
    return f"{WONK_TOOL_GUIDE}\n\n{prompt}"


def build_cli_prompt(prompt: str) -> str:
    """Prepend wonk CLI guide so the agent knows to use wonk via Bash."""
    return f"{WONK_CLI_GUIDE}\n\n{prompt}"

# Claude Code stores session logs here
CLAUDE_PROJECTS_DIR = Path.home() / ".claude" / "projects"


@dataclass
class TokenUsage:
    input_tokens: int = 0
    cache_creation_input_tokens: int = 0
    cache_read_input_tokens: int = 0
    output_tokens: int = 0
    api_calls: int = 0

    @property
    def total_tokens(self) -> int:
        return (
            self.input_tokens
            + self.cache_creation_input_tokens
            + self.cache_read_input_tokens
            + self.output_tokens
        )


def score_response(response_text: str, expected_facts: list[str]) -> float:
    """Score a response against expected facts. Returns fraction of facts found (0.0-1.0).

    Each fact is split into keywords; a fact is "found" when ALL its keywords
    appear somewhere in the response text.
    """
    if not expected_facts:
        return 1.0
    text_lower = response_text.lower()
    found = 0
    for fact in expected_facts:
        keywords = fact.lower().split()
        if all(kw in text_lower for kw in keywords):
            found += 1
    return found / len(expected_facts)


@dataclass
class RunResult:
    task_id: str
    mode: str
    run_index: int
    usage: TokenUsage
    session_id: str
    success: bool
    error: str = ""
    quality_score: float = 0.0


@dataclass
class TaskResult:
    task_id: str
    repo: str
    category: str
    prompt: str
    baseline_runs: list = field(default_factory=list)
    wonk_runs: list = field(default_factory=list)
    cli_runs: list = field(default_factory=list)

    def _median_total(self, runs: list) -> int:
        totals = [r.usage.total_tokens for r in runs if r.success]
        return int(median(totals)) if totals else 0

    def _median_api_calls(self, runs: list) -> int:
        calls = [r.usage.api_calls for r in runs if r.success]
        return int(median(calls)) if calls else 0

    def _mean_quality(self, runs: list) -> float:
        scores = [r.quality_score for r in runs if r.success]
        return mean(scores) if scores else 0.0

    def _stats(self, runs: list) -> tuple[float, float]:
        """Return (mean, stddev) of total tokens for successful runs."""
        totals = [r.usage.total_tokens for r in runs if r.success]
        if not totals:
            return (0.0, 0.0)
        m = mean(totals)
        s = stdev(totals) if len(totals) > 1 else 0.0
        return (m, s)

    @property
    def baseline_median_total(self) -> int:
        return self._median_total(self.baseline_runs)

    @property
    def wonk_median_total(self) -> int:
        return self._median_total(self.wonk_runs)

    @property
    def cli_median_total(self) -> int:
        return self._median_total(self.cli_runs)

    @property
    def reduction_pct(self) -> float:
        b = self.baseline_median_total
        w = self.wonk_median_total
        if b == 0:
            return 0.0
        return (b - w) / b * 100

    @property
    def baseline_median_api_calls(self) -> int:
        return self._median_api_calls(self.baseline_runs)

    @property
    def wonk_median_api_calls(self) -> int:
        return self._median_api_calls(self.wonk_runs)

    @property
    def cli_median_api_calls(self) -> int:
        return self._median_api_calls(self.cli_runs)

    @property
    def baseline_mean_quality(self) -> float:
        return self._mean_quality(self.baseline_runs)

    @property
    def wonk_mean_quality(self) -> float:
        return self._mean_quality(self.wonk_runs)

    @property
    def cli_mean_quality(self) -> float:
        return self._mean_quality(self.cli_runs)

    @property
    def baseline_stats(self) -> tuple[float, float]:
        return self._stats(self.baseline_runs)

    @property
    def wonk_stats(self) -> tuple[float, float]:
        return self._stats(self.wonk_runs)

    @property
    def cli_stats(self) -> tuple[float, float]:
        return self._stats(self.cli_runs)


def repo_to_project_hash(repo_path: Path) -> str:
    """Convert a repo path to Claude Code's project hash format.

    Claude replaces / with - and prepends -. E.g.:
    /home/etr/progs/wonk/bench/repos/flask -> -home-etr-progs-wonk-bench-repos-flask
    """
    return str(repo_path).replace("/", "-")


def find_session_jsonl(repo_path: Path, session_id: str) -> Path | None:
    """Find the JSONL session log for a given repo and session ID."""
    project_hash = repo_to_project_hash(repo_path)
    jsonl_path = CLAUDE_PROJECTS_DIR / project_hash / f"{session_id}.jsonl"
    if jsonl_path.exists():
        return jsonl_path
    # Fallback: scan all project dirs for the session
    for project_dir in CLAUDE_PROJECTS_DIR.iterdir():
        candidate = project_dir / f"{session_id}.jsonl"
        if candidate.exists():
            return candidate
    return None


def _extract_subagent_usage(jsonl_path: Path) -> tuple[int, int]:
    """Extract total tokens and tool uses by subagents (Agent tool).

    Subagent usage is embedded in tool_result text as:
        <usage>total_tokens: N\ntool_uses: M\nduration_ms: D</usage>

    Returns (total_tokens, tool_uses) summed across all subagent invocations.
    """
    import re

    total_tokens = 0
    total_tool_uses = 0
    with open(jsonl_path) as f:
        for line in f:
            line = line.strip()
            if not line or "<usage>" not in line:
                continue
            try:
                entry = json.loads(line)
            except json.JSONDecodeError:
                continue

            # Subagent usage appears in tool_result content blocks
            content = []
            if entry.get("type") == "user":
                msg = entry.get("message", {})
                content = msg.get("content", []) if isinstance(msg, dict) else []

            for block in content:
                if not isinstance(block, dict) or block.get("type") != "tool_result":
                    continue
                for c in block.get("content", []):
                    if isinstance(c, dict) and c.get("type") == "text":
                        text = c.get("text", "")
                        match = re.search(
                            r"<usage>total_tokens:\s*(\d+)", text
                        )
                        if match:
                            total_tokens += int(match.group(1))
                        match = re.search(
                            r"tool_uses:\s*(\d+)", text
                        )
                        if match:
                            total_tool_uses += int(match.group(1))
    return total_tokens, total_tool_uses


def parse_session_tokens(jsonl_path: Path) -> TokenUsage:
    """Parse token usage from a Claude Code JSONL session log.

    Counts both main-thread API usage and subagent (Agent tool) usage so that
    baseline sessions using Agent spawns are measured on equal footing with
    sessions that do all work in the main conversation.

    Each assistant entry has a usage block. Multiple streaming entries share a
    requestId — take the max output_tokens per requestId to get the final count.
    """
    request_usage: dict[str, dict] = {}

    with open(jsonl_path) as f:
        for line in f:
            line = line.strip()
            if not line:
                continue
            try:
                entry = json.loads(line)
            except json.JSONDecodeError:
                continue

            # Look for entries with usage data
            usage = entry.get("usage")
            if not usage:
                # Sometimes usage is nested inside message
                message = entry.get("message")
                if isinstance(message, dict):
                    usage = message.get("usage")
            if not usage:
                continue

            request_id = entry.get("requestId") or entry.get("request_id") or ""
            if not request_id:
                # Try to extract from message
                message = entry.get("message")
                if isinstance(message, dict):
                    request_id = message.get("requestId") or message.get("request_id") or ""

            if not request_id:
                # Generate a synthetic key for entries without requestId
                request_id = f"_synthetic_{len(request_usage)}"

            existing = request_usage.get(request_id)
            if existing is None:
                request_usage[request_id] = {
                    "input_tokens": usage.get("input_tokens", 0),
                    "cache_creation_input_tokens": usage.get(
                        "cache_creation_input_tokens", 0
                    ),
                    "cache_read_input_tokens": usage.get(
                        "cache_read_input_tokens", 0
                    ),
                    "output_tokens": usage.get("output_tokens", 0),
                }
            else:
                # Take max of each field (streaming sends incremental updates)
                for key in [
                    "input_tokens",
                    "cache_creation_input_tokens",
                    "cache_read_input_tokens",
                    "output_tokens",
                ]:
                    existing[key] = max(existing[key], usage.get(key, 0))

    result = TokenUsage()
    for req_id, usage_data in request_usage.items():
        result.input_tokens += usage_data["input_tokens"]
        result.cache_creation_input_tokens += usage_data[
            "cache_creation_input_tokens"
        ]
        result.cache_read_input_tokens += usage_data["cache_read_input_tokens"]
        result.output_tokens += usage_data["output_tokens"]
        result.api_calls += 1

    # Add subagent tokens and tool uses — these are reported in tool_result
    # text blocks and not included in the main session's API usage counters.
    subagent_tokens, subagent_tool_uses = _extract_subagent_usage(jsonl_path)
    if subagent_tokens > 0:
        result.input_tokens += subagent_tokens
    if subagent_tool_uses > 0:
        result.api_calls += subagent_tool_uses

    return result


def run_claude(
    repo_path: Path,
    prompt: str,
    session_id: str,
    mode: str,
    model: str,
    max_budget: float,
    timeout: int,
) -> tuple[bool, str, str]:
    """Run claude -p on a task. Returns (success, error_message, stdout)."""
    if mode == "wonk":
        effective_prompt = build_wonk_prompt(prompt)
    elif mode == "cli":
        effective_prompt = build_cli_prompt(prompt)
    else:
        effective_prompt = prompt

    cmd = [
        "claude",
        "-p",
        effective_prompt,
        "--model", model,
        "--output-format", "json",
        "--session-id", session_id,
        "--max-budget-usd", str(max_budget),
        "--append-system-prompt", SYSTEM_PROMPT,
        "--add-dir", str(repo_path),
        "--permission-mode", "bypassPermissions",
    ]

    if mode == "wonk":
        cmd += ["--strict-mcp-config", "--mcp-config", str(WONK_PLUGIN_DIR / ".mcp.json")]
    else:
        # Baseline and CLI: no MCP servers, no skills, only built-in tools
        cmd += ["--disable-slash-commands"]
        cmd += ["--strict-mcp-config", "--mcp-config", str(NO_MCP_CONFIG)]

    # Remove CLAUDECODE env var to bypass nesting guard
    env = {k: v for k, v in os.environ.items() if k != "CLAUDECODE"}

    try:
        result = subprocess.run(
            cmd,
            capture_output=True,
            text=True,
            timeout=timeout,
            env=env,
            cwd=str(repo_path),
        )
        if result.returncode != 0:
            return False, f"Exit code {result.returncode}: {result.stderr[:500]}", result.stdout
        return True, "", result.stdout
    except subprocess.TimeoutExpired as e:
        return False, f"Timeout after {timeout}s", e.stdout or ""
    except Exception as e:
        return False, str(e), ""


def parse_tool_calls(jsonl_path: Path) -> dict:
    """Parse tool call info from a JSONL session log."""
    calls = []
    sequence = []

    with open(jsonl_path) as f:
        for line in f:
            line = line.strip()
            if not line:
                continue
            try:
                entry = json.loads(line)
            except json.JSONDecodeError:
                continue

            if entry.get("type") != "assistant":
                continue
            message = entry.get("message")
            if not isinstance(message, dict):
                continue
            content = message.get("content")
            if not isinstance(content, list):
                continue

            for block in content:
                if not isinstance(block, dict) or block.get("type") != "tool_use":
                    continue
                name = block.get("name", "")
                tool_id = block.get("id", "")
                input_data = block.get("input", {})
                input_keys = sorted(input_data.keys()) if isinstance(input_data, dict) else []
                calls.append({"name": name, "id": tool_id, "input_keys": input_keys})
                sequence.append(name)

    by_tool = dict(Counter(sequence))
    return {
        "total_calls": len(calls),
        "by_tool": by_tool,
        "sequence": sequence,
        "calls": calls,
    }


def save_run_diagnostics(
    task_id: str,
    mode: str,
    run_index: int,
    stdout: str,
    jsonl_path: Path | None,
    tool_summary: dict | None,
):
    """Save diagnostic files for a single run."""
    run_dir = SESSIONS_DIR / task_id / f"{mode}_run{run_index + 1}"
    run_dir.mkdir(parents=True, exist_ok=True)

    # Save raw stdout (agent's response)
    (run_dir / "response.json").write_text(stdout)

    # Copy JSONL session log
    if jsonl_path and jsonl_path.exists():
        shutil.copy2(jsonl_path, run_dir / "session.jsonl")

    # Save tool call summary
    if tool_summary is not None:
        (run_dir / "tools.json").write_text(json.dumps(tool_summary, indent=2))


def run_single_task(
    task: dict,
    mode: str,
    run_index: int,
    model: str,
    max_budget: float,
    timeout: int,
) -> RunResult:
    """Run a single task in a given mode and return the result."""
    task_id = task["id"]
    repo_name = task["repo"]
    prompt = task["prompt"]
    repo_path = REPO_DIR / repo_name

    session_id = str(uuid.uuid4())

    print(
        f"  [{mode:8s}] run {run_index + 1}: {task_id} (session {session_id[:8]}...) ",
        end="",
        flush=True,
    )

    success, error, stdout = run_claude(
        repo_path, prompt, session_id, mode, model, max_budget, timeout
    )

    jsonl_path = find_session_jsonl(repo_path, session_id)
    usage = TokenUsage()
    tool_summary = None

    if success:
        if jsonl_path:
            usage = parse_session_tokens(jsonl_path)
            print(
                f"OK — {usage.api_calls} calls, {usage.total_tokens:,} tokens"
            )
        else:
            print("OK — JSONL not found (no token data)")
            error = "JSONL session log not found"
            success = False
    else:
        print(f"FAILED — {error[:80]}")

    if jsonl_path:
        tool_summary = parse_tool_calls(jsonl_path)

    save_run_diagnostics(task_id, mode, run_index, stdout, jsonl_path, tool_summary)

    # Score response quality against expected facts
    expected_facts = task.get("expected_facts", [])
    quality = score_response(stdout, expected_facts) if success and stdout else 0.0

    return RunResult(
        task_id=task_id,
        mode=mode,
        run_index=run_index,
        usage=usage,
        session_id=session_id,
        success=success,
        error=error,
        quality_score=quality,
    )


def generate_csv(results: list[TaskResult], output_path: Path):
    """Write per-task results to CSV."""
    with open(output_path, "w", newline="") as f:
        writer = csv.writer(f)
        writer.writerow(
            [
                "task_id",
                "repo",
                "category",
                "mode",
                "run",
                "api_calls",
                "input_tokens",
                "cache_creation_tokens",
                "cache_read_tokens",
                "output_tokens",
                "total_tokens",
                "success",
                "quality_score",
                "error",
            ]
        )
        for task_result in results:
            for run in task_result.baseline_runs + task_result.wonk_runs + task_result.cli_runs:
                writer.writerow(
                    [
                        run.task_id,
                        task_result.repo,
                        task_result.category,
                        run.mode,
                        run.run_index + 1,
                        run.usage.api_calls,
                        run.usage.input_tokens,
                        run.usage.cache_creation_input_tokens,
                        run.usage.cache_read_input_tokens,
                        run.usage.output_tokens,
                        run.usage.total_tokens,
                        run.success,
                        f"{run.quality_score:.2f}",
                        run.error,
                    ]
                )


def generate_report(results: list[TaskResult], model: str, num_runs: int) -> str:
    """Generate a markdown report from task results."""
    has_cli = any(r.cli_runs for r in results)

    lines = []
    lines.append("# Agent Benchmark Results: Claude Code with vs without Wonk")
    lines.append("")
    lines.append(f"**Model:** `{model}` | **Runs per task:** {num_runs} (median + mean±σ reported)")
    lines.append("")

    # Per-task table
    lines.append("## Per-Task Comparison")
    lines.append("")
    header = (
        "| Task | Repo | Category | Baseline (median) | Baseline (mean±σ) | Wonk (median) | Wonk (mean±σ) "
        "| Reduction | B Quality | W Quality | B Tok/Q | W Tok/Q |"
    )
    sep = (
        "|------|------|----------|-------------------:|------------------:|--------------:|--------------:"
        "|---------:|----------:|----------:|--------:|--------:|"
    )
    if has_cli:
        header = header.rstrip("|") + " CLI (median) | CLI (mean±σ) | CLI Red. | C Quality | C Tok/Q |"
        sep = sep.rstrip("|") + "-------------:|-------------:|--------:|----------:|--------:|"
    lines.append(header)
    lines.append(sep)

    successful_results = [r for r in results if r.baseline_median_total > 0]
    wonk_ok_results = [r for r in successful_results if r.wonk_median_total > 0]

    for r in results:
        b_total = r.baseline_median_total
        w_total = r.wonk_median_total
        c_total = r.cli_median_total
        reduction = f"{r.reduction_pct:+.1f}%" if b_total > 0 else "N/A"
        b_str = f"{b_total:,}" if b_total > 0 else "FAILED"
        w_str = f"{w_total:,}" if w_total > 0 else "FAILED"
        b_mean, b_std = r.baseline_stats
        w_mean, w_std = r.wonk_stats
        b_ms = f"{b_mean:,.0f}±{b_std:,.0f}" if b_mean > 0 else "N/A"
        w_ms = f"{w_mean:,.0f}±{w_std:,.0f}" if w_mean > 0 else "N/A"
        b_tpq = f"{b_total / r.baseline_mean_quality:,.0f}" if r.baseline_mean_quality > 0 else "N/A"
        w_tpq = f"{w_total / r.wonk_mean_quality:,.0f}" if r.wonk_mean_quality > 0 else "N/A"
        row = (
            f"| {r.task_id} | {r.repo} | {r.category} "
            f"| {b_str} | {b_ms} | {w_str} | {w_ms} | {reduction} "
            f"| {r.baseline_mean_quality:.2f} | {r.wonk_mean_quality:.2f} "
            f"| {b_tpq} | {w_tpq} |"
        )
        if has_cli:
            c_str = f"{c_total:,}" if c_total > 0 else "FAILED"
            c_mean, c_std = r.cli_stats
            c_ms = f"{c_mean:,.0f}±{c_std:,.0f}" if c_mean > 0 else "N/A"
            c_red = f"{(b_total - c_total) / b_total * 100:+.1f}%" if b_total > 0 and c_total > 0 else "N/A"
            c_tpq = f"{c_total / r.cli_mean_quality:,.0f}" if r.cli_mean_quality > 0 else "N/A"
            row += f" {c_str} | {c_ms} | {c_red} | {r.cli_mean_quality:.2f} | {c_tpq} |"
        lines.append(row)

    lines.append("")

    # Per-category summary
    lines.append("## Per-Category Summary")
    lines.append("")
    cat_header = "| Category | Avg Baseline | Avg Wonk | Avg Reduction | Avg B Quality | Avg W Quality |"
    cat_sep = "|----------|-------------:|---------:|--------------:|--------------:|--------------:|"
    if has_cli:
        cat_header += " Avg CLI | Avg CLI Red. | Avg C Quality |"
        cat_sep += "--------:|-------------:|--------------:|"
    cat_header += " Tasks |"
    cat_sep += "------:|"
    lines.append(cat_header)
    lines.append(cat_sep)

    categories = sorted(set(r.category for r in results))
    for cat in categories:
        cat_results = [r for r in successful_results if r.category == cat]
        if not cat_results:
            na_cols = "| N/A | N/A | N/A | N/A | N/A |"
            if has_cli:
                na_cols += " N/A | N/A | N/A |"
            lines.append(f"| {cat} {na_cols} 0 |")
            continue
        avg_b = sum(r.baseline_median_total for r in cat_results) // len(cat_results)
        avg_bq = sum(r.baseline_mean_quality for r in cat_results) / len(cat_results)
        wonk_cat = [r for r in cat_results if r.wonk_median_total > 0]
        if wonk_cat:
            avg_w = sum(r.wonk_median_total for r in wonk_cat) // len(wonk_cat)
            avg_reduction = sum(r.reduction_pct for r in wonk_cat) / len(wonk_cat)
            avg_wq = sum(r.wonk_mean_quality for r in wonk_cat) / len(wonk_cat)
            row = (
                f"| {cat} | {avg_b:,} | {avg_w:,} "
                f"| {avg_reduction:+.1f}% | {avg_bq:.2f} | {avg_wq:.2f} |"
            )
        else:
            row = f"| {cat} | {avg_b:,} | N/A | N/A | {avg_bq:.2f} | N/A |"
        if has_cli:
            cli_cat = [r for r in cat_results if r.cli_median_total > 0]
            if cli_cat:
                avg_c = sum(r.cli_median_total for r in cli_cat) // len(cli_cat)
                avg_c_red = sum(
                    (r.baseline_median_total - r.cli_median_total) / r.baseline_median_total * 100
                    for r in cli_cat
                ) / len(cli_cat)
                avg_cq = sum(r.cli_mean_quality for r in cli_cat) / len(cli_cat)
                row += f" {avg_c:,} | {avg_c_red:+.1f}% | {avg_cq:.2f} |"
            else:
                row += " N/A | N/A | N/A |"
        row += f" {len(cat_results)} |"
        lines.append(row)

    lines.append("")

    # Overall summary
    lines.append("## Overall Summary")
    lines.append("")

    if successful_results:
        all_baseline = [r.baseline_median_total for r in successful_results]
        all_b_calls = [r.baseline_median_api_calls for r in successful_results]
        all_bq = [r.baseline_mean_quality for r in successful_results]
        total_baseline = sum(all_baseline)
        avg_bq_overall = mean(all_bq) if all_bq else 0
        b_tpq_overall = total_baseline / avg_bq_overall if avg_bq_overall > 0 else 0

        lines.append(f"- **Tasks completed:** {len(successful_results)}/{len(results)}")
        lines.append(f"- **Total baseline tokens:** {total_baseline:,}")

        # Wonk stats (only when wonk data exists)
        if wonk_ok_results:
            all_wonk = [r.wonk_median_total for r in wonk_ok_results]
            all_reductions = [r.reduction_pct for r in wonk_ok_results]
            total_wonk = sum(all_wonk)
            overall_reduction = (total_baseline - total_wonk) / total_baseline * 100
            all_w_calls = [r.wonk_median_api_calls for r in wonk_ok_results]
            all_wq = [r.wonk_mean_quality for r in wonk_ok_results]
            lines.append(f"- **Total wonk tokens:** {total_wonk:,}")
            lines.append(f"- **Overall reduction:** {overall_reduction:+.1f}%")
            lines.append(f"- **Median per-task reduction:** {median(all_reductions):+.1f}%")
            lines.append(f"- **Mean per-task reduction:** {mean(all_reductions):+.1f}%"
                          + (f" (σ={stdev(all_reductions):.1f}%)" if len(all_reductions) > 1 else ""))
            lines.append(f"- **Best reduction:** {max(all_reductions):+.1f}%")
            lines.append(f"- **Worst reduction:** {min(all_reductions):+.1f}%")
            lines.append(f"- **Avg wonk API calls:** {sum(all_w_calls) / len(all_w_calls):.1f}")
            lines.append(f"- **Avg wonk quality:** {mean(all_wq):.2f}")
            avg_wq_overall = mean(all_wq) if all_wq else 0
            if avg_bq_overall > 0 and avg_wq_overall > 0:
                w_tpq_overall = total_wonk / avg_wq_overall
                tpq_reduction = (b_tpq_overall - w_tpq_overall) / b_tpq_overall * 100
                lines.append(f"- **Tokens/quality (wonk):** {w_tpq_overall:,.0f}")
                lines.append(f"- **Quality-adjusted reduction:** {tpq_reduction:+.1f}%")

        lines.append(f"- **Avg baseline API calls:** {sum(all_b_calls) / len(all_b_calls):.1f}")
        lines.append(f"- **Avg baseline quality:** {mean(all_bq):.2f}")
        if avg_bq_overall > 0:
            lines.append(f"- **Tokens/quality (baseline):** {b_tpq_overall:,.0f}")

        # CLI overall stats
        if has_cli:
            cli_ok = [r for r in successful_results if r.cli_median_total > 0]
            if cli_ok:
                all_cli = [r.cli_median_total for r in cli_ok]
                total_cli = sum(all_cli)
                cli_overall_reduction = (total_baseline - total_cli) / total_baseline * 100
                all_c_reds = [
                    (r.baseline_median_total - r.cli_median_total) / r.baseline_median_total * 100
                    for r in cli_ok
                ]
                all_c_calls = [r.cli_median_api_calls for r in cli_ok]
                all_cq = [r.cli_mean_quality for r in cli_ok]
                lines.append(f"- **Total CLI tokens:** {total_cli:,}")
                lines.append(f"- **CLI overall reduction:** {cli_overall_reduction:+.1f}%")
                lines.append(f"- **Median per-task CLI reduction:** {median(all_c_reds):+.1f}%")
                lines.append(f"- **Mean per-task CLI reduction:** {mean(all_c_reds):+.1f}%"
                              + (f" (σ={stdev(all_c_reds):.1f}%)" if len(all_c_reds) > 1 else ""))
                lines.append(f"- **Best CLI reduction:** {max(all_c_reds):+.1f}%")
                lines.append(f"- **Worst CLI reduction:** {min(all_c_reds):+.1f}%")
                lines.append(f"- **Avg CLI API calls:** {sum(all_c_calls) / len(all_c_calls):.1f}")
                lines.append(f"- **Avg CLI quality:** {mean(all_cq):.2f}")
                avg_cq_overall = mean(all_cq) if all_cq else 0
                if b_tpq_overall > 0 and avg_cq_overall > 0:
                    c_tpq_overall = total_cli / avg_cq_overall
                    c_tpq_reduction = (b_tpq_overall - c_tpq_overall) / b_tpq_overall * 100
                    lines.append(f"- **Tokens/quality (CLI):** {c_tpq_overall:,.0f}")
                    lines.append(f"- **Quality-adjusted CLI reduction:** {c_tpq_reduction:+.1f}%")

        # Flag trivial tasks (both modes use ≤1 API call median).
        trivial = [r for r in wonk_ok_results
                   if r.baseline_median_api_calls <= 1 and r.wonk_median_api_calls <= 1]
        if trivial:
            lines.append("")
            lines.append(f"**Note:** {len(trivial)} trivial task(s) "
                         f"(both modes ≤1 API call): "
                         f"{', '.join(r.task_id for r in trivial)}. "
                         f"Consider replacing with tasks requiring code-specific verification.")
    else:
        lines.append("No successful results to summarize.")

    lines.append("")

    # Outlier Runs — wonk/cli runs that used MORE tokens than baseline median
    # Also flag any run >2× its own mode's median
    def _collect_outliers(results, mode_attr, runs_attr):
        outliers = []
        for r in results:
            b_median = r.baseline_median_total
            m_median = getattr(r, mode_attr)
            if b_median == 0:
                continue
            for run in getattr(r, runs_attr):
                if not run.success:
                    continue
                pct_over = (run.usage.total_tokens - b_median) / b_median * 100
                is_2x = m_median > 0 and run.usage.total_tokens > 2 * m_median
                marker = " **[>2× median]**" if is_2x else ""
                if run.usage.total_tokens > b_median or is_2x:
                    diag_dir = f"bench/results/sessions/{r.task_id}/{run.mode}_run{run.run_index + 1}/"
                    outliers.append((r.task_id, run.run_index + 1, run.usage.total_tokens, b_median, pct_over, run.usage.api_calls, diag_dir, marker))
        return outliers

    for mode_label, mode_attr, runs_attr in [
        ("Wonk", "wonk_median_total", "wonk_runs"),
        ("CLI", "cli_median_total", "cli_runs"),
    ]:
        if mode_label == "CLI" and not has_cli:
            continue
        outliers = _collect_outliers(results, mode_attr, runs_attr)
        if not outliers:
            continue
        outliers.sort(key=lambda x: -x[4])  # worst offenders first
        lines.append(f"## Outlier Runs ({mode_label} > Baseline)")
        lines.append("")
        lines.append(
            f"| Task | Run | {mode_label} Tokens | Baseline Median | % Over "
            "| API Calls | Notes | Diagnostics |"
        )
        lines.append(
            "|------|----:|------------:|----------------:|-------:"
            "|----------:|-------|-------------|"
        )
        for task_id, run_num, tokens, b_med, pct, api_calls, diag, marker in outliers:
            lines.append(
                f"| {task_id} | {run_num} | {tokens:,} | {b_med:,} "
                f"| {pct:+.1f}% | {api_calls} | {marker} | `{diag}` |"
            )
        lines.append("")

    return "\n".join(lines)


def main():
    parser = argparse.ArgumentParser(
        description="Benchmark Claude Code with vs without wonk MCP tools"
    )
    parser.add_argument(
        "--tasks",
        nargs="+",
        help="Specific task IDs to run (default: all)",
    )
    parser.add_argument(
        "--category",
        help="Run only tasks in this category",
    )
    parser.add_argument(
        "--repo",
        help="Run only tasks for this repo",
    )
    parser.add_argument(
        "--runs",
        type=int,
        default=5,
        help="Number of runs per task per mode (default: 5)",
    )
    parser.add_argument(
        "--model",
        default="sonnet",
        help="Claude model to use (default: sonnet)",
    )
    parser.add_argument(
        "--max-budget",
        type=float,
        default=0.50,
        help="Max budget in USD per run (default: 0.50)",
    )
    parser.add_argument(
        "--timeout",
        type=int,
        default=180,
        help="Timeout in seconds per run (default: 180)",
    )
    parser.add_argument(
        "--mode",
        choices=["all", "both", "baseline", "wonk", "cli", "baseline-and-cli"],
        default="both",
        help="Which modes to run: all=baseline+wonk+cli, both=baseline+wonk, baseline-and-cli=baseline+cli (default: both)",
    )
    args = parser.parse_args()

    # Load tasks
    with open(TASKS_FILE) as f:
        all_tasks = json.load(f)

    # Filter tasks
    tasks = all_tasks
    if args.tasks:
        tasks = [t for t in tasks if t["id"] in args.tasks]
    if args.category:
        tasks = [t for t in tasks if t["category"] == args.category]
    if args.repo:
        tasks = [t for t in tasks if t["repo"] == args.repo]

    if not tasks:
        print("No tasks matched filters.", file=sys.stderr)
        sys.exit(1)

    # Verify repos exist
    for task in tasks:
        repo_path = REPO_DIR / task["repo"]
        if not repo_path.is_dir():
            print(
                f"Repo not found: {repo_path}. Run token_bench.sh first to clone repos.",
                file=sys.stderr,
            )
            sys.exit(1)

    # Verify wonk plugin directory exists with expected structure
    if args.mode in ("all", "both", "wonk"):
        if not WONK_PLUGIN_DIR.is_dir():
            print(
                f"Wonk plugin directory not found: {WONK_PLUGIN_DIR}\n"
                f"Set WONK_PLUGIN_DIR env var or clone wonk-plugin as a sibling repo.",
                file=sys.stderr,
            )
            sys.exit(1)
        mcp_config = WONK_PLUGIN_DIR / ".mcp.json"
        if not mcp_config.exists():
            print(
                f"Wonk MCP config not found: {mcp_config}\n"
                f"Expected .mcp.json in {WONK_PLUGIN_DIR}.",
                file=sys.stderr,
            )
            sys.exit(1)

    mode_count = {"all": 3, "both": 2, "baseline-and-cli": 2}.get(args.mode, 1)
    print(f"Running {len(tasks)} tasks x {args.runs} runs x "
          f"{mode_count} mode{'s' if mode_count > 1 else ''} "
          f"with model={args.model}")
    print(f"Max budget: ${args.max_budget}/run, timeout: {args.timeout}s")
    print()

    # Pre-index repos for wonk/cli mode (both need the index)
    if args.mode in ("all", "both", "wonk", "cli", "baseline-and-cli"):
        indexed_repos = set()
        for task in tasks:
            repo_name = task["repo"]
            if repo_name not in indexed_repos:
                repo_path = REPO_DIR / repo_name
                print(f"Indexing {repo_name} with wonk...")
                subprocess.run(
                    ["wonk", "init", "-q"],
                    cwd=str(repo_path),
                    capture_output=True,
                )
                indexed_repos.add(repo_name)
        print()

    # Run benchmarks
    task_results: list[TaskResult] = []

    for task in tasks:
        print(f"Task: {task['id']} ({task['repo']}/{task['category']})")
        result = TaskResult(
            task_id=task["id"],
            repo=task["repo"],
            category=task["category"],
            prompt=task["prompt"],
        )

        for run_idx in range(args.runs):
            if args.mode in ("all", "both", "baseline", "baseline-and-cli"):
                run_result = run_single_task(
                    task, "baseline", run_idx, args.model,
                    args.max_budget, args.timeout,
                )
                result.baseline_runs.append(run_result)

            if args.mode in ("all", "both", "wonk"):
                run_result = run_single_task(
                    task, "wonk", run_idx, args.model,
                    args.max_budget, args.timeout,
                )
                result.wonk_runs.append(run_result)

            if args.mode in ("all", "cli", "baseline-and-cli"):
                run_result = run_single_task(
                    task, "cli", run_idx, args.model,
                    args.max_budget, args.timeout,
                )
                result.cli_runs.append(run_result)

        task_results.append(result)
        print()

    # Generate outputs
    RESULTS_DIR.mkdir(parents=True, exist_ok=True)

    csv_path = RESULTS_DIR / "agent_results.csv"
    generate_csv(task_results, csv_path)
    print(f"CSV written to {csv_path}")

    if args.mode in ("both", "all", "baseline-and-cli"):
        report = generate_report(task_results, args.model, args.runs)
        report_path = RESULTS_DIR / "agent_report.md"
        report_path.write_text(report)
        print(f"Report written to {report_path}")
        print()
        print(report)


if __name__ == "__main__":
    main()
