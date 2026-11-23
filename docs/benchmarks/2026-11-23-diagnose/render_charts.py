#!/usr/bin/env python3
"""Render measured diagnosis sessions. --draft explicitly watermarks layout previews."""
from __future__ import annotations

import argparse
import json
import math
import os
from pathlib import Path
import tempfile

os.environ.setdefault('MPLCONFIGDIR', str(Path(tempfile.gettempdir()) / 'kiro-diagnose-matplotlib'))
import matplotlib
matplotlib.use('Agg')
import matplotlib.pyplot as plt
from matplotlib import font_manager
from matplotlib.lines import Line2D
from matplotlib.patches import FancyBboxPatch
from matplotlib.ticker import FuncFormatter, MaxNLocator
import numpy as np

CONDITIONS = ('terminal', 'scout', 'diagnose')
LABELS = {'terminal': 'AI + native tools', 'scout': 'AI + Scout', 'diagnose': 'AI + Diagnose available'}
COLORS = {'terminal': '#8D9FAC', 'scout': '#77AEBE', 'diagnose': '#ED8D54'}
BG, CARD, INK, MUTED, GRID = '#111315', '#191D20', '#F2E8D5', '#929DA5', '#30373C'
WIDTH, HEIGHT, DPI = 16, 10, 150


def numeric(value):
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        return None
    try:
        return float(value) if math.isfinite(value) and value >= 0 else None
    except OverflowError:
        return None


def load_results(path, draft=False):
    data = json.loads(Path(path).read_text())
    rows, tasks, meta = data.get('runs', []), data.get('tasks', []), data.get('metadata', {})
    if not rows or not tasks:
        raise ValueError('Measured runs and task definitions are required')
    task_ids = {task['id'] for task in tasks}
    if len(task_ids) != len(tasks):
        raise ValueError('Duplicate task definitions')
    keys = set()
    for row in rows:
        if row.get('condition') not in CONDITIONS or row.get('task_id') not in task_ids:
            raise ValueError('Unknown task or condition')
        rep = row.get('rep')
        if isinstance(rep, bool) or not isinstance(rep, int) or rep < 1:
            raise ValueError('Repetitions must be positive integers')
        key = row['task_id'], row['condition'], rep
        if key in keys:
            raise ValueError('Duplicate measured session')
        keys.add(key)
        if numeric(row.get('wall_seconds')) is None:
            raise ValueError('All retained sessions need measured wall time')
        if not isinstance(row.get('complete'), bool) or not isinstance(row.get('parsed_answer'), bool):
            raise ValueError('Completion and answer-parse states must be explicit')
        for field in ('path_at5_hit', 'region_overlap_at5'):
            value = row.get(field)
            if value is not None and not isinstance(value, bool):
                raise ValueError('Location proxies must be booleans or null')
            if value and not (row['complete'] and row['parsed_answer']):
                raise ValueError('A location hit requires a finished session with parsed answer')
        recall = row.get('file_recall_at5')
        if recall is not None and (numeric(recall) is None or recall > 1):
            raise ValueError('File recall must lie in [0, 1] or be null')
        if recall is not None and isinstance(row.get('path_at5_hit'), bool) and bool(recall) != row['path_at5_hit']:
            raise ValueError('File recall and file-hit proxy disagree')
        if row.get('region_overlap_at5') is True and row.get('path_at5_hit') is not True:
            raise ValueError('Range overlap requires a Top-5 reference file hit')
    if not draft:
        repetitions = meta.get('repetitions')
        if isinstance(repetitions, bool) or not isinstance(repetitions, int) or repetitions < 1:
            raise ValueError('Publication requires a prespecified repetition count')
        expected = {(task, condition, rep) for task in task_ids for condition in CONDITIONS for rep in range(1, repetitions + 1)}
        if keys != expected:
            raise ValueError('The fixed experiment is incomplete; do not drop unsuccessful sessions')
        if meta.get('experiment_valid') is not True or meta.get('datasets_unchanged') is not True:
            raise ValueError('Publication requires a valid experiment and unchanged source projections')
        if meta.get('gold_sha256') and meta.get('gold_unchanged') is not True:
            raise ValueError('Publication requires unchanged private reference annotations')
        if meta.get('pilot') or meta.get('draft') or meta.get('synthetic'):
            raise ValueError('Pilot/draft/synthetic data may only be rendered with --draft')
        if meta.get('tool_availability_verified') is not True:
            raise ValueError('Publication requires verified condition-specific tool availability')
        for field in ('date', 'model', 'kiro_commit'):
            if not meta.get(field):
                raise ValueError(f'Missing publication metadata: {field}')
    return data


def configure_style():
    for candidate in [Path('/System/Library/Fonts/Helvetica.ttc'), Path('/System/Library/Fonts/Supplemental/Arial.ttf')]:
        if candidate.exists():
            font_manager.fontManager.addfont(str(candidate))
            plt.rcParams['font.family'] = font_manager.FontProperties(fname=str(candidate)).get_name()
            break
    plt.rcParams.update({'figure.facecolor': BG, 'axes.facecolor': BG, 'savefig.facecolor': BG,
        'text.color': INK, 'axes.labelcolor': MUTED, 'xtick.color': MUTED, 'ytick.color': MUTED,
        'font.size': 12, 'axes.unicode_minus': True, 'svg.fonttype': 'path',
        'svg.hashsalt': 'kiro-diagnosis-real-ai', 'pdf.fonttype': 42})


def grouped(data, field):
    return {condition: [(float(row[field]), row) for row in data['runs']
                        if row['condition'] == condition and numeric(row.get(field)) is not None]
            for condition in CONDITIONS}


def fmt_number(value, _position=None):
    if value >= 1e6:
        return f'{value / 1e6:.1f}M'
    if value >= 1000:
        return f'{value / 1000:.0f}k'
    return f'{value:g}'


def fmt_time(value):
    return f'{value:.1f} s'


def card(fig, rect):
    fig.add_artist(FancyBboxPatch((rect[0], rect[1]), rect[2], rect[3],
        boxstyle='round,pad=0.012,rounding_size=0.018', transform=fig.transFigure,
        facecolor=CARD, edgecolor=GRID, linewidth=.7, zorder=0))


def base(data, title, subtitle, page, draft):
    fig = plt.figure(figsize=(WIDTH, HEIGHT), dpi=DPI)
    fig.text(.06, .935, 'kiro', fontsize=26, fontweight='bold', va='center')
    fig.text(.143, .935, 'REAL ISSUE DIAGNOSIS', fontsize=10.5, color=MUTED, fontweight='bold', va='center')
    fig.text(.94, .935, page, fontsize=10.5, color=MUTED, ha='right', va='center')
    fig.add_artist(Line2D([.06, .94], [.902, .902], transform=fig.transFigure, color=GRID, linewidth=.8))
    fig.text(.06, .842, title, fontsize=33, fontweight='bold', va='center')
    fig.text(.06, .795, subtitle, fontsize=12.5, color=MUTED, va='center')
    fig.add_artist(Line2D([.06, .94], [.125, .125], transform=fig.transFigure, color=GRID, linewidth=.8))
    repos = len({task.get('repository') for task in data['tasks'] if task.get('repository')})
    scope = f"{len(data['runs'])} real AI sessions  ·  {len(data['tasks'])} distinct issues  ·  {repos} public repositories  ·  macOS  ·  warm/shared cache"
    if draft:
        scope = 'DRAFT layout only — synthetic observations; not measured experiment evidence'
    fig.text(.06, .090, scope, fontsize=10.8, color=MUTED, va='center')
    offered = [row for row in data['runs'] if row['condition'] == 'diagnose']
    calls = sum(row.get('tools', {}).get('diagnose', 0) for row in offered)
    used = sum(row.get('tools', {}).get('diagnose', 0) > 0 for row in offered)
    fig.text(.94, .090, f'Diagnose calls: {calls} ({used}/{len(offered)} sessions) · availability comparison',
        fontsize=9.5, color=COLORS['diagnose'], ha='right', va='center')
    meta = data['metadata']
    fig.text(.06, .057, f"{meta.get('model', '?')}  ·  kiro {str(meta.get('kiro_commit', '?'))[:7]}  ·  {meta.get('date', '?')}", fontsize=10.2, color=MUTED, va='center')
    fig.text(.94, .057, 'Measured sample. No universal speedup claim.', fontsize=10.2, color=MUTED, ha='right', va='center')
    if draft:
        fig.text(.5, .49, 'DRAFT', fontsize=125, fontweight='bold', color=COLORS['diagnose'], alpha=.14, rotation=14, ha='center', va='center', zorder=30)
        fig.text(.5, .972, 'LAYOUT PREVIEW — NOT BENCHMARK EVIDENCE', fontsize=11, color=COLORS['diagnose'], fontweight='bold', ha='center')
    return fig


def axis(ax, upper, label, money=False):
    ax.set_xlim(0, max(1e-9, upper))
    ax.set_ylim(-.45, 2.45)
    ax.set_yticks([2, 1, 0], [LABELS[condition] for condition in CONDITIONS])
    ax.tick_params(axis='y', length=0, labelsize=11.5, pad=12)
    ax.tick_params(axis='x', length=0, labelsize=10.5, pad=9)
    ax.xaxis.set_major_locator(MaxNLocator(nbins=5, min_n_ticks=3))
    ax.xaxis.set_major_formatter(FuncFormatter(lambda value, _: f'${value:.2f}') if money else FuncFormatter(fmt_number))
    ax.set_xlabel(label, fontsize=11, labelpad=11)
    ax.grid(axis='x', color=GRID, linewidth=.7, alpha=.65, zorder=0)
    ax.set_axisbelow(True)
    for spine in ax.spines.values():
        spine.set_visible(False)


def distribution(ax, values, annotate=False, formatter=fmt_number, markersize=28):
    for condition, y in zip(CONDITIONS, [2, 1, 0]):
        entries = sorted(values[condition], key=lambda item: (item[1]['task_id'], item[1]['rep']))
        if not entries:
            continue
        q1, median, q3 = np.percentile([value for value, _ in entries], [25, 50, 75])
        color = COLORS[condition]
        ax.plot([q1, q3], [y, y], lw=12, color=color, alpha=.24, solid_capstyle='round', zorder=2)
        ax.plot([median, median], [y-.13, y+.13], lw=2.3, color=INK, zorder=5)
        for offset, (value, row) in zip(np.linspace(-.08, .08, len(entries)), entries):
            finished = row['complete'] and row['parsed_answer']
            ax.scatter(value, y+offset, s=markersize, marker='o' if finished else 'x',
                color=color, alpha=.82, linewidths=1.2, edgecolors=BG if finished else None, zorder=4)
        if annotate:
            ax.annotate(formatter(median), (median, y), xytext=(0, 21), textcoords='offset points',
                ha='center', fontsize=14, fontweight='bold', color=color)


def overview(data, draft):
    fig = base(data, 'Same issues. Three ways to investigate.',
        'Complete agent session time  /  native tools in every condition  /  all started sessions retained', '01 / OVERVIEW', draft)
    fig.text(.06, .728, 'EACH DOT IS ONE SESSION', fontsize=10.1, color=MUTED, fontweight='bold')
    fig.text(.31, .728, 'Band: middle 50%  ·  Tick: median  ·  ×: unfinished or malformed answer', fontsize=10.1, color=MUTED)
    times = grouped(data, 'wall_seconds')
    ax = fig.add_axes([.18, .355, .43, .31])
    axis(ax, max(value for entries in times.values() for value, _ in entries)*1.10, 'Elapsed wall time (seconds)')
    distribution(ax, times, annotate=True, formatter=fmt_time, markersize=30)
    card(fig, [.685, .335, .245, .35])
    fig.text(.705, .650, 'REFERENCE LOCATION PROXIES', fontsize=9.9, color=MUTED, fontweight='bold')
    fig.text(.705, .619, 'Top-5 file hit / pre-fix range overlap', fontsize=9.9, color=MUTED)
    for index, condition in enumerate(CONDITIONS):
        rows = [row for row in data['runs'] if row['condition'] == condition]
        y = .575-index*.081
        fig.text(.705, y, LABELS[condition], fontsize=11, color=COLORS[condition])
        metrics = []
        for field in ('path_at5_hit', 'region_overlap_at5'):
            observed = [row[field] for row in rows if isinstance(row.get(field), bool)]
            metrics.append(f'{sum(observed)}/{len(observed)}' if observed else '—')
        fig.text(.910, y, '  /  '.join(metrics), fontsize=12, color=INK, ha='right', fontweight='bold')
        complete = sum(row['complete'] for row in rows)
        fig.text(.705, y-.027, f'Finished: {complete}/{len(rows)}', fontsize=9, color=MUTED)
    medians = {condition: float(np.median([value for value, _ in times[condition]])) for condition in CONDITIONS}
    for index, condition in enumerate(CONDITIONS):
        x = .06+index*.302
        card(fig, [x, .205, .275, .080])
        fig.text(x+.014, .260, LABELS[condition], fontsize=10, color=COLORS[condition])
        fig.text(x+.014, .222, fmt_time(medians[condition]), fontsize=22, color=INK, fontweight='bold')
        fig.text(x+.25, .229, f'n={len(times[condition])}', fontsize=10, color=MUTED, ha='right')
    fig.text(.06, .156, 'Patch locations are proxies, not correctness. Explanations and fixes require separate validation.', fontsize=10.8, color=MUTED)
    return fig


def task_times(data, draft):
    fig = base(data, 'Every issue. Every session.',
        'Paired source investigations  /  same pre-fix source and report for all three configurations', '02 / ISSUE DETAIL', draft)
    tasks = data['tasks']
    if len(tasks) > 36:
        raise ValueError('Issue figure supports at most 36 tasks; split a larger benchmark into pages')
    handles = [Line2D([0], [0], marker='o', linestyle='', color=COLORS[c], markersize=6, label=LABELS[c]) for c in CONDITIONS]
    fig.legend(handles=handles, loc='upper left', bbox_to_anchor=(.055, .752), frameon=False, ncol=3,
        fontsize=11, labelcolor=INK, handletextpad=.4, columnspacing=1.8)
    upper = max(row['wall_seconds'] for row in data['runs'])*1.06
    ax = fig.add_axes([.315, .235, .61, .460])
    ax.set_xlim(0, upper)
    ax.set_ylim(-.65, len(tasks)-.35)
    labels = []
    for index, task in enumerate(tasks):
        label = task['id']
        if len(label) > 39:
            label = label[:37]+'…'
        labels.append(f'{index+1:02d}  {label}')
        y = len(tasks)-1-index
        rows = [row for row in data['runs'] if row['task_id'] == task['id']]
        if rows:
            ax.plot([min(row['wall_seconds'] for row in rows), max(row['wall_seconds'] for row in rows)], [y,y], color=GRID, lw=1.1)
        for condition, shift in zip(CONDITIONS, [.16, 0, -.16]):
            cells = [row for row in rows if row['condition'] == condition]
            for offset, row in zip(np.linspace(-.035, .035, len(cells)), cells):
                finished = row['complete'] and row['parsed_answer']
                ax.scatter(row['wall_seconds'], y+shift+offset, s=24, color=COLORS[condition],
                    marker='o' if finished else 'x', linewidths=1, zorder=4, alpha=.9)
    ax.set_yticks(list(reversed(range(len(tasks)))), labels)
    ax.tick_params(axis='y', length=0, pad=14, labelsize=8.7)
    ax.tick_params(axis='x', length=0, pad=8, labelsize=10.5)
    ax.xaxis.set_major_locator(MaxNLocator(nbins=6))
    ax.set_xlabel('Elapsed wall time (seconds)', fontsize=11, labelpad=11)
    ax.grid(axis='x', color=GRID, linewidth=.7, alpha=.7)
    for spine in ax.spines.values():
        spine.set_visible(False)
    fig.text(.06, .156, 'All sessions shown. Each horizontal connector groups one issue; × marks unfinished or malformed answers.', fontsize=10.8, color=MUTED)
    return fig


def effort(data, draft):
    fig = base(data, 'What the agent spent.',
        'Reported model turns, context tokens and total model cost  /  median and every reported observation', '03 / AGENT EFFORT', draft)
    legend = [Line2D([0], [0], marker='o', linestyle='', color=COLORS[c], markersize=6, label=LABELS[c]) for c in CONDITIONS]
    fig.legend(handles=legend, loc='upper left', bbox_to_anchor=(.055, .752), frameon=False, ncol=3,
        fontsize=11, labelcolor=INK, handletextpad=.4, columnspacing=1.8)
    specs = [('turns', 'MODEL TURNS', 'Reported turns', lambda value: f'{value:.1f}', False),
             ('tokens', 'REPORTED CONTEXT + OUTPUT', 'Tokens per session', fmt_number, False),
             ('cost_usd', 'REPORTED MODEL COST', 'USD per session', lambda value: f'${value:.3f}', True)]
    for index, (field, title, label, formatter, money) in enumerate(specs):
        x = .06+index*.302
        card(fig, [x, .265, .275, .410])
        fig.text(x+.015, .638, title, fontsize=9.8, color=MUTED, fontweight='bold')
        values = grouped(data, field)
        observed = [value for entries in values.values() for value, _ in entries]
        for condition, y in zip(CONDITIONS, [.584, .531, .478]):
            entries = values[condition]
            text = formatter(float(np.median([value for value, _ in entries]))) if entries else '—'
            fig.text(x+.015, y, text, fontsize=25, fontweight='bold', color=COLORS[condition])
            fig.text(x+.113, y+.007, LABELS[condition], fontsize=8.5, color=MUTED)
            fig.text(x+.245, y+.006, f'n={len(entries)}', fontsize=9.6, color=MUTED, ha='right')
        if observed:
            ax = fig.add_axes([x+.022, .325, .225, .115])
            axis(ax, max(observed)*1.12, label, money)
            ax.set_facecolor(CARD)
            ax.set_yticklabels([])
            ax.tick_params(axis='x', labelsize=9)
            ax.xaxis.set_major_locator(MaxNLocator(nbins=3, min_n_ticks=2))
            distribution(ax, values, markersize=20)
    fig.text(.06, .205, 'Tokens include reported cached context. Total cost includes auxiliary model usage, as reported by the CLI.', fontsize=10.8, color=MUTED)
    fig.text(.06, .166, 'Missing counters remain unavailable: n counts observed values. Cost is not a billing invoice.', fontsize=10.8, color=MUTED)
    return fig


def quality(data, draft):
    fig = base(data, 'Where the investigation landed.',
        'Reference production-patch locations  /  all finished, failed and abstaining sessions retained', '04 / LOCATION PROXIES', draft)
    legend = [Line2D([0], [0], marker='o', linestyle='', color=COLORS[c], markersize=6, label=LABELS[c]) for c in CONDITIONS]
    fig.legend(handles=legend, loc='upper left', bbox_to_anchor=(.055, .752), frameon=False, ncol=3,
        fontsize=11, labelcolor=INK, handletextpad=.4, columnspacing=1.8)
    specs = [('path_at5_hit', 'AT LEAST ONE PATCH FILE', 'Top-5 file hit rate', True),
             ('file_recall_at5', 'PATCH FILES RECOVERED', 'Mean file recall in Top-5', False),
             ('region_overlap_at5', 'PRE-FIX RANGE OVERLAP', 'Top-5 range-overlap rate', True)]
    for index, (field, title, label, binary) in enumerate(specs):
        x = .06+index*.302
        card(fig, [x, .280, .275, .395])
        fig.text(x+.015, .638, title, fontsize=9.8, color=MUTED, fontweight='bold')
        fig.text(x+.015, .602, label, fontsize=11, color=INK)
        ax = fig.add_axes([x+.022, .345, .225, .205])
        axis(ax, 100, 'Percent of observed cases' if binary else 'Mean percent of reference files')
        ax.set_facecolor(CARD)
        ax.set_yticklabels([])
        ax.set_xticks([0, 50, 100])
        ax.tick_params(axis='x', labelsize=9.5)
        for condition, y in zip(CONDITIONS, [2, 1, 0]):
            values = [row[field] for row in data['runs'] if row['condition'] == condition
                and ((isinstance(row.get(field), bool)) if binary else numeric(row.get(field)) is not None)]
            if values:
                rate = float(np.mean(values))*100
                ax.barh(y, rate, height=.30, color=COLORS[condition], alpha=.8, zorder=3)
                detail = f'{sum(values)}/{len(values)}  ·  {rate:.1f}%' if binary else f'{rate:.1f}%  ·  n={len(values)}'
                ax.text(0, y+.25, detail, fontsize=10, color=COLORS[condition], va='bottom')
            else:
                ax.text(0, y, 'No reference observations', fontsize=9, color=MUTED)
    fig.text(.06, .210, 'Reference patches can touch supporting files. Other valid diagnoses may identify different locations.', fontsize=10.8, color=MUTED)
    fig.text(.06, .170, 'These are localization proxies, not explanation accuracy, causal proof, or a count of verified fixes.', fontsize=10.8, color=MUTED)
    return fig


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--input', required=True, type=Path)
    parser.add_argument('--output-dir', required=True, type=Path)
    parser.add_argument('--draft', action='store_true')
    args = parser.parse_args()
    data = load_results(args.input, args.draft)
    configure_style()
    args.output_dir.mkdir(parents=True, exist_ok=True)
    outputs = []
    for name, renderer in [('diagnose-benchmark-overview', overview), ('diagnose-benchmark-issues', task_times), ('diagnose-benchmark-effort', effort), ('diagnose-benchmark-locations', quality)]:
        fig = renderer(data, args.draft)
        for extension in ('png', 'svg'):
            path = args.output_dir / f'{name}.{extension}'
            fig.savefig(path, dpi=DPI, facecolor=BG, metadata={'Title': name,
                'Description': 'Draft layout preview, not benchmark evidence' if args.draft else 'Measured real-issue AI diagnosis experiment',
                'Date': str(data['metadata'].get('date', '?'))})
            if extension == 'svg':
                path.write_text('\n'.join(line.rstrip() for line in path.read_text().splitlines())+'\n')
            outputs.append(str(path.resolve()))
        plt.close(fig)
    print(json.dumps({'outputs': outputs, 'draft': args.draft, 'runs': len(data['runs']), 'dimensions_png': [WIDTH*DPI, HEIGHT*DPI]}, indent=2))
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
