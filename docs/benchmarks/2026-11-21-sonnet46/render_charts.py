#!/usr/bin/env python3
"""Render Kiro's measured AI-session benchmark as publication-ready PNG/SVG figures.

No benchmark values are fabricated here. The input is the runner's recorded results.json.
Use --draft only for explicitly marked layout previews; draft images are watermarked.
"""

from __future__ import annotations

import argparse
import json
import math
import os
from pathlib import Path
import textwrap

os.environ.setdefault("MPLCONFIGDIR", str(Path(__file__).resolve().parent / ".mplconfig"))

import matplotlib

matplotlib.use("Agg")
import matplotlib.pyplot as plt
from matplotlib import font_manager
from matplotlib.lines import Line2D
from matplotlib.patches import FancyBboxPatch
from matplotlib.ticker import FuncFormatter, MaxNLocator
import numpy as np


BG = "#111315"
CARD = "#191D20"
INK = "#F2E8D5"
MUTED = "#929DA5"
GRID = "#30373C"
COPPER = "#ED8D54"
TERMINAL = "#8D9FAC"
COLORS = {"terminal": TERMINAL, "kiro": COPPER}
LABELS = {"terminal": "AI + terminal", "kiro": "AI + Kiro MCP"}
CONDITIONS = ("terminal", "kiro")
WIDTH, HEIGHT, DPI = 16, 10, 150


def configure_style() -> None:
    candidates = [
        Path("/System/Library/Fonts/Helvetica.ttc"),
        Path("/System/Library/Fonts/SFNS.ttf"),
        Path("/System/Library/Fonts/Supplemental/Arial.ttf"),
    ]
    for candidate in candidates:
        if candidate.exists():
            font_manager.fontManager.addfont(str(candidate))
            plt.rcParams["font.family"] = font_manager.FontProperties(fname=str(candidate)).get_name()
            break
    plt.rcParams.update({
        "figure.facecolor": BG,
        "axes.facecolor": BG,
        "savefig.facecolor": BG,
        "text.color": INK,
        "axes.labelcolor": MUTED,
        "xtick.color": MUTED,
        "ytick.color": MUTED,
        "font.size": 13,
        "axes.unicode_minus": True,
        "svg.fonttype": "path",
        "pdf.fonttype": 42,
    })


def numeric(value) -> float | None:
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        return None
    number = float(value)
    return number if math.isfinite(number) and number >= 0 else None


def load_results(path: Path, draft: bool) -> dict:
    result = json.loads(path.read_text())
    runs = result.get("runs", [])
    if not runs:
        raise ValueError("No measured runs: nothing to plot")
    task_ids = {task["id"] for task in result.get("tasks", [])}
    if not task_ids:
        raise ValueError("Input must identify its benchmark tasks")
    for run in runs:
        if run.get("condition") not in CONDITIONS:
            raise ValueError(f"Unknown condition: {run.get('condition')!r}")
        if run.get("task_id") not in task_ids:
            raise ValueError(f"Unknown task: {run.get('task_id')!r}")
        if numeric(run.get("wall_seconds")) is None:
            raise ValueError("Every run, including unsuccessful ones, needs a measured wall_seconds")
    for condition in CONDITIONS:
        if not any(run["condition"] == condition for run in runs):
            raise ValueError(f"No runs for {condition}")
    meta = result.setdefault("metadata", {})
    if (meta.get("synthetic") or meta.get("draft")) and not draft:
        raise ValueError("Synthetic or draft data requires --draft and cannot be presented as benchmark evidence")
    return result


def pretty_model(model: str) -> str:
    if "sonnet" in model.lower() and ("4-6" in model or "4.6" in model):
        return "Claude Sonnet 4.6"
    return model or "Model not specified"


def groups(data: dict, field: str, task_id=None) -> dict[str, list[tuple[float, dict]]]:
    grouped = {condition: [] for condition in CONDITIONS}
    for run in data["runs"]:
        if task_id is not None and run["task_id"] != task_id:
            continue
        value = numeric(run.get(field))
        if value is not None:
            grouped[run["condition"]].append((value, run))
    for condition in CONDITIONS:
        grouped[condition].sort(key=lambda entry: (str(entry[1]["task_id"]), entry[1].get("rep", 0)))
    return grouped


def successful(run: dict) -> bool:
    return bool(run.get("complete")) and bool(run.get("expected_paths_mentioned"))


def time_text(value: float) -> str:
    if value < 60:
        return f"{value:.1f} s"
    minutes = int(value // 60)
    seconds = int(round(value % 60))
    if seconds == 60:
        minutes += 1
        seconds = 0
    return f"{minutes}m {seconds:02d}s"


def short_number(value: float, _position=None) -> str:
    if value >= 1_000_000:
        return f"{value / 1_000_000:.1f}M"
    if value >= 1000:
        return f"{value / 1000:.0f}k"
    return f"{value:g}"


def card(fig, rect, face=CARD):
    fig.add_artist(FancyBboxPatch(
        (rect[0], rect[1]), rect[2], rect[3],
        boxstyle="round,pad=0.012,rounding_size=0.018",
        transform=fig.transFigure, facecolor=face, edgecolor=GRID, linewidth=0.7, zorder=0,
    ))


def base_figure(data: dict, title: str, subtitle: str, page: str, draft: bool):
    fig = plt.figure(figsize=(WIDTH, HEIGHT), dpi=DPI)
    fig.text(0.06, 0.935, "kiro", fontsize=26, fontweight="bold", va="center")
    fig.text(0.143, 0.935, "REAL AI BENCHMARK", fontsize=10.5, color=MUTED, va="center", fontweight="bold")
    fig.text(0.94, 0.935, page, fontsize=10.5, color=MUTED, ha="right", va="center")
    fig.add_artist(Line2D([0.06, 0.94], [0.902, 0.902], transform=fig.transFigure, color=GRID, linewidth=0.8))
    fig.text(0.06, 0.842, title, fontsize=34, fontweight="bold", va="center")
    fig.text(0.06, 0.795, subtitle, fontsize=13.5, color=MUTED, va="center")
    if draft:
        fig.text(0.5, 0.49, "DRAFT", fontsize=125, fontweight="bold", color=COPPER, alpha=0.12,
                 rotation=14, ha="center", va="center", zorder=20)
        fig.text(0.5, 0.972, "SYNTHETIC LAYOUT PREVIEW — NOT BENCHMARK EVIDENCE", fontsize=11,
                 fontweight="bold", color=COPPER, ha="center")
    footer(fig, data, draft)
    return fig


def footer(fig, data: dict, draft: bool) -> None:
    meta = data["metadata"]
    repositories = meta.get("repositories", meta.get("repository_count", 2))
    count = len(repositories) if isinstance(repositories, list) else repositories
    repository_text = f"{count} public repositories" if count is not None else "Public repositories"
    cache = meta.get("cache_state", "warm filesystem cache")
    if not isinstance(cache, str):
        cache = "Filesystem cache state recorded with results"
    scope = f"{len(data['runs'])} real AI sessions  ·  {repository_text}  ·  macOS  ·  {cache}"
    if draft:
        scope = "SYNTHETIC LAYOUT PREVIEW — not measured benchmark results"
    fig.add_artist(Line2D([0.06, 0.94], [0.125, 0.125], transform=fig.transFigure, color=GRID, linewidth=0.8))
    fig.text(0.06, 0.090, scope, fontsize=11.2, color=MUTED, va="center")
    date = str(meta.get("date", "Date not supplied"))
    commit = str(meta.get("kiro_commit", "unspecified"))[:7]
    model = str(meta.get("model", "unspecified"))
    fig.text(0.06, 0.057, f"{model}  ·  kiro {commit}  ·  {date}", fontsize=10.5, color=MUTED, va="center")
    fig.text(0.94, 0.057, "Measured sample. No universal speedup claim.", fontsize=10.5, color=MUTED, ha="right", va="center")


def clean_axis(ax, upper: float, label: str, money=False):
    ax.set_xlim(0, max(upper, 1e-9))
    ax.set_ylim(-0.45, 1.45)
    ax.set_yticks([1, 0], [LABELS["terminal"], LABELS["kiro"]])
    ax.tick_params(axis="y", length=0, labelsize=12.5, pad=14)
    ax.tick_params(axis="x", length=0, labelsize=11, pad=10)
    ax.xaxis.set_major_locator(MaxNLocator(nbins=5, min_n_ticks=3))
    ax.xaxis.set_major_formatter(FuncFormatter(lambda v, p: f"${v:.2f}") if money else FuncFormatter(short_number))
    ax.set_xlabel(label, fontsize=11.5, labelpad=12)
    ax.grid(axis="x", color=GRID, linewidth=0.7, alpha=0.65, zorder=0)
    for spine in ax.spines.values():
        spine.set_visible(False)
    ax.set_axisbelow(True)


def draw_distribution(ax, grouped, *, annotate=False, formatter=None, marker_size=34):
    for condition, y in [("terminal", 1), ("kiro", 0)]:
        entries = grouped[condition]
        if not entries:
            ax.text(0.03, y, "No reported values", color=MUTED, va="center")
            continue
        values = np.array([entry[0] for entry in entries])
        q1, median, q3 = np.percentile(values, [25, 50, 75])
        color = COLORS[condition]
        ax.plot([q1, q3], [y, y], linewidth=13, color=color, alpha=0.24, solid_capstyle="round", zorder=2)
        ax.plot([median, median], [y - 0.15, y + 0.15], linewidth=2.4, color=INK, zorder=5)
        offsets = np.linspace(-0.065, 0.065, len(entries))
        # Deterministic order avoids invented uncertainty or random visual movement.
        for index, (value, run) in enumerate(entries):
            good = successful(run)
            ax.scatter(value, y + offsets[index], s=marker_size, marker="o" if good else "x",
                       c=color, alpha=0.88, linewidths=1.3, edgecolors=BG if good else None, zorder=4)
        if annotate:
            text = formatter(median) if formatter else f"{median:.1f}"
            ax.annotate(text, (median, y), xytext=(0, 24), textcoords="offset points",
                        ha="center", color=color, fontsize=14, fontweight="bold")


def distribution_key(fig, y=0.721):
    fig.text(0.06, y, "EACH DOT IS ONE SESSION", fontsize=10.3, color=MUTED, fontweight="bold")
    fig.text(0.32, y, "Thick line: middle 50%  ·  Tick: median  ·  ×: unfinished or missing expected paths",
             fontsize=10.5, color=MUTED)


def overview(data: dict, draft: bool):
    model = pretty_model(str(data["metadata"].get("model", "")))
    fig = base_figure(data, "Same tasks. Two ways to explore.",
                      f"End-to-end agent task time  /  {model}  /  all recorded sessions included", "01 / OVERVIEW", draft)
    distribution_key(fig)
    grouped = groups(data, "wall_seconds")
    max_time = max(value for entries in grouped.values() for value, _ in entries)
    ax = fig.add_axes([0.185, 0.345, 0.465, 0.295])
    clean_axis(ax, max_time * 1.10, "Elapsed wall time (seconds)")
    draw_distribution(ax, grouped, annotate=True, formatter=time_text, marker_size=39)

    card(fig, [0.718, 0.435, 0.21, 0.255])
    fig.text(0.738, 0.655, "MEDIAN SESSION TIME", fontsize=10.2, fontweight="bold", color=MUTED)
    medians = {condition: float(np.median([v for v, _ in grouped[condition]])) for condition in CONDITIONS}
    fig.text(0.738, 0.610, LABELS["terminal"], fontsize=11, color=TERMINAL)
    fig.text(0.738, 0.566, time_text(medians["terminal"]), fontsize=28, fontweight="bold")
    fig.text(0.738, 0.512, LABELS["kiro"], fontsize=11, color=COPPER)
    fig.text(0.738, 0.468, time_text(medians["kiro"]), fontsize=28, fontweight="bold", color=COPPER)

    card(fig, [0.718, 0.232, 0.21, 0.163])
    fig.text(0.738, 0.360, "FINISHED + EXPECTED PATHS", fontsize=9.8, fontweight="bold", color=MUTED)
    for condition, x in [("terminal", 0.738), ("kiro", 0.841)]:
        runs = [run for run in data["runs"] if run["condition"] == condition]
        count = sum(successful(run) for run in runs)
        fig.text(x, 0.308, f"{count}/{len(runs)}", fontsize=24, fontweight="bold", color=COLORS[condition])
        fig.text(x, 0.268, "Terminal" if condition == "terminal" else "Kiro", fontsize=10.8, color=MUTED)

    if medians["terminal"] > 0:
        change = (medians["kiro"] / medians["terminal"] - 1) * 100
        sign = "−" if change < 0 else "+"
        fig.text(0.06, 0.236, f"{sign}{abs(change):.0f}%", fontsize=30, color=COPPER if change < 0 else INK, fontweight="bold")
        fig.text(0.185, 0.252, "observed change in median wall time", fontsize=12.5, color=INK)
        fig.text(0.185, 0.225, "A sample comparison, not a statistical significance claim.", fontsize=10.7, color=MUTED)
    fig.text(0.06, 0.163, "Path mentions measure location coverage, not the correctness of an analysis or a proposed code change.",
             fontsize=11, color=MUTED)
    return fig


def task_times(data: dict, draft: bool):
    fig = base_figure(data, "Every task. Every session.",
                      "End-to-end wall time by task  /  shared zero-based scale  /  all sessions included", "02 / TASK DETAIL", draft)
    distribution_key(fig)
    tasks = data["tasks"]
    if len(tasks) > 4:
        raise ValueError("Task detail layout supports at most four tasks; split larger experiments into additional figures")
    upper = max(float(run["wall_seconds"]) for run in data["runs"]) * 1.10
    top, bottom, gap = 0.68, 0.270, 0.035
    height = (top - bottom - gap * (len(tasks) - 1)) / len(tasks)
    for index, task in enumerate(tasks):
        low = top - (index + 1) * height - index * gap
        fig.text(0.06, low + height * 0.66, f"{index + 1:02d}", fontsize=16, color=COPPER, fontweight="bold")
        title = str(task.get("title", task["id"]))
        wrapped = textwrap.fill(title, width=28, max_lines=3, placeholder="…")
        fig.text(0.105, low + height * 0.66, wrapped, fontsize=12.6, color=INK, va="center", linespacing=1.45)
        grouped = groups(data, "wall_seconds", task["id"])
        n = "/".join(str(len(grouped[c])) for c in CONDITIONS)
        fig.text(0.105, low + height * 0.12, f"n = {n}  ·  terminal / kiro", fontsize=9.8, color=MUTED)
        ax = fig.add_axes([0.43, low, 0.49, height])
        clean_axis(ax, upper, "Elapsed wall time (seconds)" if index == len(tasks) - 1 else "")
        if index != len(tasks) - 1:
            ax.tick_params(axis="x", labelbottom=False)
        draw_distribution(ax, grouped, annotate=False, marker_size=33)
    fig.text(0.06, 0.163, "No sessions were removed because they were slow, unfinished, or failed to mention an expected location.",
             fontsize=11, color=MUTED)
    return fig


def effort(data: dict, draft: bool):
    fig = base_figure(data, "What the agent spent.",
                      "Reported turns, tokens and API cost per session  /  medians and individual runs", "03 / AGENT EFFORT", draft)
    legend = [Line2D([0], [0], marker="o", linestyle="", color=COLORS[c], markersize=7, label=LABELS[c]) for c in CONDITIONS]
    fig.legend(handles=legend, loc="upper left", bbox_to_anchor=(0.053, 0.746), ncol=2, frameon=False,
               fontsize=12, labelcolor=INK, handletextpad=0.45, columnspacing=2)
    specs = [
        ("turns", "MODEL TURNS", "Reported turns", lambda x: f"{x:.1f}", False),
        ("tokens", "TOTAL REPORTED TOKENS", "Tokens per session", short_number, False),
        ("cost_usd", "REPORTED API COST", "USD per session", lambda x: f"${x:.3f}", True),
    ]
    for index, (field, title, axis_label, fmt, money) in enumerate(specs):
        x = 0.06 + index * 0.302
        card(fig, [x, 0.270, 0.275, 0.405])
        fig.text(x + 0.015, 0.638, title, fontsize=10.5, color=MUTED, fontweight="bold")
        grouped = groups(data, field)
        values = [value for entries in grouped.values() for value, _ in entries]
        if not values:
            fig.text(x + 0.015, 0.49, "Not reported", fontsize=23, fontweight="bold")
            continue
        for condition, ypos in [("terminal", 0.581), ("kiro", 0.522)]:
            entries = grouped[condition]
            text = fmt(float(np.median([v for v, _ in entries]))) if entries else "—"
            fig.text(x + 0.015, ypos, text, fontsize=27, fontweight="bold", color=COLORS[condition])
            fig.text(x + 0.245, ypos + 0.007, f"n={len(entries)}", fontsize=10, color=MUTED, ha="right")
        ax = fig.add_axes([x + 0.022, 0.340, 0.225, 0.125])
        clean_axis(ax, max(values) * 1.12, axis_label, money=money)
        ax.set_facecolor(CARD)
        ax.set_yticklabels([])
        ax.tick_params(axis="x", labelsize=9.7)
        ax.xaxis.set_major_locator(MaxNLocator(nbins=3, min_n_ticks=2))
        draw_distribution(ax, grouped, marker_size=24)
    fig.text(0.06, 0.205, "Tokens include reported cache usage. Cost is reported by Claude Code, not a billing invoice.",
             fontsize=11, color=MUTED)
    fig.text(0.06, 0.170, "These are usage measurements, not a correctness score. Unsuccessful sessions remain in the sample.",
             fontsize=11, color=MUTED)
    return fig


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--input", type=Path, required=True)
    parser.add_argument("--output-dir", type=Path, required=True)
    parser.add_argument("--draft", action="store_true", help="Watermark all outputs as synthetic layout previews")
    args = parser.parse_args()
    configure_style()
    data = load_results(args.input, args.draft)
    args.output_dir.mkdir(parents=True, exist_ok=True)
    outputs = []
    for name, renderer in [
        ("ai-benchmark-overview", overview),
        ("ai-benchmark-tasks", task_times),
        ("ai-benchmark-effort", effort),
    ]:
        fig = renderer(data, args.draft)
        for extension in ["png", "svg"]:
            path = args.output_dir / f"{name}.{extension}"
            fig.savefig(path, dpi=DPI, facecolor=BG, metadata={"Title": name, "Description": "Synthetic draft" if args.draft else "Measured AI-session benchmark"})
            outputs.append(str(path.resolve()))
        plt.close(fig)
    print(json.dumps({"outputs": outputs, "draft": args.draft, "runs": len(data["runs"]),
                      "dimensions_png": [WIDTH * DPI, HEIGHT * DPI]}, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
