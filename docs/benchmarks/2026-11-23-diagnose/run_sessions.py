#!/usr/bin/env python3
"""Run real read-only diagnosis sessions. No model calls occur when this module is imported.

Every started session is retained. Gold patch locations are never sent to the model and
are OS-denied to the Claude process. Patch-location overlap is a proxy, not a diagnosis
correctness or verified-repair score. Missing usage counters remain null.
"""
from __future__ import annotations

import argparse
import concurrent.futures
import csv
import datetime
import hashlib
import io
import json
import math
import os
from pathlib import Path, PurePosixPath
import random
import re
import signal
import stat
import subprocess
import threading
import time

CONDITIONS = ('terminal', 'scout', 'diagnose')
BUILTINS = ('Bash', 'Read', 'Grep', 'Glob')
MCP_TOOLS = {'scout': ('run', 'scout'), 'diagnose': ('run', 'scout', 'diagnose')}
TOOL_NAMES = {f'mcp__kiro__{name}': name for name in ('run', 'scout', 'diagnose')}
TOKEN_FIELDS = ('input_tokens', 'cache_creation_input_tokens', 'cache_read_input_tokens', 'output_tokens')
ENV_OVERRIDES = {'MCP_CONNECTION_NONBLOCKING': '0', 'ENABLE_TOOL_SEARCH': 'false',
                 'LC_ALL': 'en_US.UTF-8', 'LC_CTYPE': 'en_US.UTF-8', 'LANG': 'en_US.UTF-8'}
MAX_REGION_SOURCE_BYTES = 16 * 1024 * 1024
COMMON = '''This is a read-only investigation of a real reported issue in the supplied pre-fix production source. Do not modify files, run builds/tests/project code, install packages, access the network, or launch other agents. Do not inspect other projects, session histories, benchmark scripts, answers or reference patches. Use local source evidence. Independent reads/searches may be batched, scripted and filtered. Identify at most eight production files most directly responsible for the reported behavior, ranked most important first. Give a narrow source range supporting each responsibility, and explain the likely cause and the limits of the evidence. Do not invent a verified fix. Exclude tests, docs, generated files and dependencies. Return only valid JSON: {"files":[{"path":"repository-relative/source/file","start_line":1,"end_line":10,"reason":"verified role and evidence"}],"summary":"diagnosis and remaining uncertainty"}. Line numbers must refer to the supplied pre-fix source. Each source range must contain at most 40 lines. Multiple narrow ranges for the same file are allowed. An empty files list is allowed when evidence is insufficient.\n\nReported issue:\n'''


def utc_now():
    return datetime.datetime.now(datetime.timezone.utc).isoformat(timespec='milliseconds')


def number(value):
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        return None
    try:
        return value if math.isfinite(value) and value >= 0 else None
    except OverflowError:
        return None


def seconds(value):
    value = number(value)
    return value / 1000 if value is not None else None


def sanitized(value, root):
    if isinstance(value, str):
        return value.replace(str(root), '<PROJECT>').replace(str(Path.home()), '<HOME>')
    if isinstance(value, dict):
        return {key: sanitized(item, root) for key, item in value.items()}
    if isinstance(value, list):
        return [sanitized(item, root) for item in value]
    return value


def file_sha256(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def tree_hash(root):
    """Length-framed names, kinds, permission modes and bytes; never follow symlinks."""
    digest = hashlib.sha256()
    for path in sorted(Path(root).rglob('*')):
        relative = path.relative_to(root)
        if '.git' in relative.parts:
            continue
        if path.is_symlink():
            kind, data = b'link', os.readlink(path).encode()
        elif path.is_file():
            kind, data = b'file', path.read_bytes()
        else:
            continue
        for part in (str(relative).encode(), kind, str(path.lstat().st_mode & 0o777).encode(), data):
            digest.update(len(part).to_bytes(8, 'big'))
            digest.update(part)
    return digest.hexdigest()


def normalize_path(value, root):
    if not isinstance(value, str) or not value:
        return None
    prefix = str(root) + '/'
    if value.startswith(prefix):
        value = value[len(prefix):]
    value = value.removeprefix('./')
    path = PurePosixPath(value)
    if path.is_absolute() or '..' in path.parts or str(path) in ('', '.') or '\x00' in value:
        return None
    return str(path)


def source_line_count(path, root):
    """Verify a regular in-root source and count LF/CRLF lines as Rust str.lines does."""
    try:
        canonical_root = Path(root).resolve(strict=True)
        source = (canonical_root / path).resolve(strict=True)
        if not source.is_relative_to(canonical_root):
            return None
        descriptor = os.open(source, os.O_RDONLY | os.O_NONBLOCK | getattr(os, 'O_NOFOLLOW', 0))
        with os.fdopen(descriptor, 'rb') as stream:
            metadata = os.fstat(stream.fileno())
            if not stat.S_ISREG(metadata.st_mode) or metadata.st_size > MAX_REGION_SOURCE_BYTES:
                return None
            data = stream.read(MAX_REGION_SOURCE_BYTES + 1)
        if len(data) > MAX_REGION_SOURCE_BYTES:
            return None
        return data.count(b'\n') + int(bool(data) and not data.endswith(b'\n'))
    except (OSError, ValueError, RuntimeError):
        return None


def parse_answer(answer, root, diagnostics=None):
    """Keep file order: Top-5 means the first five distinct proposed files."""
    if diagnostics is not None:
        diagnostics['invalid_regions'] = 0
    if not isinstance(answer, str):
        return [], [], False
    text = re.sub(r'^```(?:json)?\s*|\s*```$', '', answer.strip())
    try:
        data = json.loads(text)
    except (json.JSONDecodeError, ValueError):
        start, end = text.find('{'), text.rfind('}')
        if start < 0 or end < start:
            return [], [], False
        try:
            data = json.loads(text[start:end + 1])
        except (json.JSONDecodeError, ValueError):
            return [], [], False
    if not isinstance(data, dict) or not isinstance(data.get('files'), list) or not isinstance(data.get('summary'), str):
        return [], [], False
    files, regions, valid, line_counts = [], [], True, {}
    for item in data['files']:
        if not isinstance(item, dict):
            valid = False
            if diagnostics is not None:
                diagnostics['invalid_regions'] += 1
            continue
        path = normalize_path(item.get('path'), root)
        if path is None or not isinstance(item.get('reason'), str):
            valid = False
            if diagnostics is not None:
                diagnostics['invalid_regions'] += 1
            continue
        if path not in files:
            files.append(path)
        start, end = item.get('start_line'), item.get('end_line')
        valid_range = (isinstance(start, int) and not isinstance(start, bool) and isinstance(end, int)
                       and not isinstance(end, bool) and 1 <= start <= end and end - start < 40)
        if valid_range and path not in line_counts:
            line_counts[path] = source_line_count(path, root)
        if valid_range and line_counts[path] is not None and end <= line_counts[path]:
            regions.append({'path': path, 'start_line': start, 'end_line': end})
        elif diagnostics is not None:
            diagnostics['invalid_regions'] += 1
    return files, regions, valid


def location_scores(files, regions, gold, eligible):
    """Reference production patch locations are a proxy; no explanations are graded here."""
    if not gold:
        return {'path_at5_hit': None, 'file_recall_at5': None, 'gold_files_found_at5': None,
                'gold_files_total': None, 'region_overlap_at5': None, 'region_evaluable': False}
    expected = set(gold.get('expected_paths', []))
    expected_regions = gold.get('expected_regions', [])
    top = set(files[:5]) if eligible else set()
    found = len(top & expected)
    overlap = any(pred['path'] in top and pred['path'] == reference['path']
                  and pred['start_line'] <= reference['end_line']
                  and reference['start_line'] <= pred['end_line']
                  for pred in regions for reference in expected_regions) if eligible else False
    return {'path_at5_hit': bool(found) if expected else None,
            'file_recall_at5': found / len(expected) if expected else None,
            'gold_files_found_at5': found, 'gold_files_total': len(expected),
            'region_overlap_at5': overlap if expected_regions else None,
            'region_evaluable': bool(expected_regions)}


def content_blocks(event):
    message = event.get('message')
    blocks = message.get('content', []) if isinstance(message, dict) else []
    return [block for block in blocks if isinstance(block, dict)] if isinstance(blocks, list) else []


def structured_run_result(block, event):
    candidates = [block.get('structuredContent'), block.get('structured_content')]
    value = event.get('tool_use_result')
    if isinstance(value, dict):
        candidates += [value.get('structuredContent'), value.get('structured_content')]
    for candidate in candidates:
        if isinstance(candidate, dict) and candidate.get('schema_version') == 1 and candidate.get('tool') == 'run' and isinstance(candidate.get('commands'), list):
            return candidate
    return None


def parse_events(events, task, root, returncode, timed_out, gold=None):
    events = [event for event in events if isinstance(event, dict)]
    final = next((event for event in reversed(events) if event.get('type') == 'result'), {})
    init = next((event for event in events if event.get('type') == 'system' and event.get('subtype') == 'init'), {})
    names, tools, observed_models = {}, {}, set()
    refusals, unobservable, tool_errors = 0, 0, 0
    for event in events:
        if event.get('type') == 'assistant':
            message = event.get('message')
            model = message.get('model') if isinstance(message, dict) else None
            if isinstance(model, str):
                observed_models.add(model)
        for block in content_blocks(event):
            if block.get('type') == 'tool_use':
                name = block.get('name')
                if isinstance(name, str):
                    names[block.get('id')] = name
                    short = TOOL_NAMES.get(name, name)
                    tools[short] = tools.get(short, 0) + 1
            elif block.get('type') == 'tool_result':
                name = names.get(block.get('tool_use_id'))
                if name in TOOL_NAMES:
                    tool_errors += int(block.get('is_error') is True)
                if name == 'mcp__kiro__run':
                    structured = structured_run_result(block, event)
                    if structured is None:
                        unobservable += 1
                    else:
                        refusals += sum(isinstance(command, dict) and command.get('status') == 'refused' for command in structured['commands'])
    answer = final.get('result', '')
    answer_diagnostics = {}
    files, regions, parsed = parse_answer(answer, root, answer_diagnostics)
    usage = final.get('usage') if isinstance(final.get('usage'), dict) else {}
    token_values = [number(usage.get(field)) for field in TOKEN_FIELDS]
    complete = final.get('subtype') == 'success' and final.get('is_error') is not True and not timed_out and returncode == 0
    denials = final.get('permission_denials')
    available = init.get('tools')
    available = sorted(tool for tool in available if isinstance(tool, str)) if isinstance(available, list) else None
    servers = init.get('mcp_servers')
    servers = [{'name': server.get('name'), 'status': server.get('status')} for server in servers if isinstance(server, dict)] if isinstance(servers, list) else None
    model_usage = final.get('modelUsage')
    return {'cli_reported_seconds': seconds(final.get('duration_ms')), 'api_reported_seconds': seconds(final.get('duration_api_ms')),
            'turns': number(final.get('num_turns')), 'tokens': sum(token_values) if all(v is not None for v in token_values) else None,
            'usage': usage, 'cost_usd': number(final.get('total_cost_usd')),
            'reported_model_usage': sanitized(model_usage, root) if isinstance(model_usage, dict) else None,
            'complete': complete, 'parsed_answer': parsed, 'returned_files': files, 'returned_regions': regions,
            'valid_region_count': len(regions),
            'invalid_regions': answer_diagnostics['invalid_regions'],
            **location_scores(files, regions, gold, complete and parsed), 'tools': tools,
            'kiro_refusals': refusals if unobservable == 0 else None, 'kiro_refusal_count_complete': unobservable == 0,
            'kiro_run_results_without_structure': unobservable, 'kiro_error_results': tool_errors,
            'permission_denials': len(denials) if isinstance(denials, list) else None,
            'permission_denial_details': sanitized(denials, root), 'model': init.get('model'),
            'observed_assistant_models': sorted(observed_models), 'init_observed': bool(init), 'available_tools': available,
            'mcp_servers_at_init': servers, 'mcp_server_errors_at_init': sanitized(init.get('mcp_server_errors'), root),
            'returncode': returncode, 'timed_out': timed_out, 'result_subtype': final.get('subtype'),
            'result_is_error': final.get('is_error'), 'stop_reason': final.get('stop_reason'),
            'terminal_reason': final.get('terminal_reason'), 'answer': sanitized(answer, root), 'errors': sanitized(final.get('errors', []), root)}


def availability_errors(row, condition, model):
    expected = set(BUILTINS) | {f'mcp__kiro__{name}' for name in MCP_TOOLS.get(condition, ())}
    errors = []
    if not row['init_observed']:
        errors.append('No system/init event; tool availability unobservable')
    elif row['available_tools'] is None or set(row['available_tools']) != expected:
        errors.append('Initial toolset differs from the prespecified condition')
    if condition != 'terminal' and not any(server['name'] == 'kiro' and server['status'] == 'connected' for server in row['mcp_servers_at_init'] or []):
        errors.append('Kiro MCP server was not connected at initialization')
    if condition == 'terminal' and row['mcp_servers_at_init']:
        errors.append('Terminal baseline unexpectedly initialized an MCP server')
    used = {next((full for full, short in TOOL_NAMES.items() if short == name), name) for name in row['tools']}
    if not used.issubset(expected):
        errors.append('A tool outside the prescribed condition was used')
    if row['model'] is not None and row['model'] != model:
        errors.append('Initialized primary model differs from the pinned model')
    if any(observed != model for observed in row['observed_assistant_models']):
        errors.append('Observable assistant messages use another primary model')
    return errors


def sandbox_profile(root, config_path, output, denied):
    # Traces/other-condition answers are private too. The CLI may read its own MCP config.
    clauses = ['(version 1)', '(allow default)', f'(deny file-write* (subpath {json.dumps(str(root))}))',
               f'(deny file-read* (subpath {json.dumps(str(output))}))',
               f'(deny file-write* (subpath {json.dumps(str(output))}))',
               f'(allow file-read* (literal {json.dumps(str(config_path))}))']
    for path in denied:
        clauses.append(f'(deny file-read* (subpath {json.dumps(str(path))}))')
        clauses.append(f'(deny file-write* (subpath {json.dumps(str(path))}))')
    return ''.join(clauses)


class Experiment:
    def __init__(self, args):
        self.args, self.out = args, Path(args.output).resolve()
        if self.out.exists() and any(self.out.iterdir()):
            raise ValueError('Output directory must be empty; never overwrite measured sessions')
        self.tasks_path, self.binary = Path(args.tasks).resolve(), Path(args.binary).resolve()
        self.tasks = json.loads(self.tasks_path.read_text())
        self.gold_path = Path(args.gold).resolve() if args.gold else None
        self.gold = json.loads(self.gold_path.read_text()) if self.gold_path else {}
        self.denied = [Path(path).resolve() for path in args.deny_read]
        if self.gold_path:
            self.denied.append(self.gold_path.parent)
        projects = json.loads(Path(args.projects_json).read_text()) if args.projects_json else {}
        if not isinstance(projects, dict) or any(not isinstance(key, str) or not isinstance(value, str) for key, value in projects.items()):
            raise ValueError('projects JSON must map dataset IDs to directory strings')
        projects.update(dict(value.split('=', 1) for value in args.project))
        if not isinstance(self.tasks, list) or not self.tasks:
            raise ValueError('tasks must be a nonempty list')
        if not isinstance(self.gold, dict):
            raise ValueError('gold must be an object indexed by task ID')
        ids = set()
        for task in self.tasks:
            if not isinstance(task, dict) or any(not isinstance(task.get(key), str) or not task[key] for key in ('id', 'dataset', 'prompt', 'repository', 'source_commit')):
                raise ValueError('Each task needs id, dataset, prompt, repository and source_commit')
            if not re.fullmatch(r'[a-zA-Z0-9][a-zA-Z0-9_.-]*', task['id']) or task['id'] in ids:
                raise ValueError('Task IDs must be unique filesystem-safe names')
            ids.add(task['id'])
            root = Path(task.get('project') or projects[task['dataset']]).resolve()
            if not root.is_dir() or self.out.is_relative_to(root):
                raise ValueError('Source projection must exist; output must be outside it')
            if any(root.is_relative_to(path) for path in self.denied):
                raise ValueError('Source projection must not be inside a denied private directory')
            task['project'] = str(root)
            entry = self.gold.get(task['id'])
            if self.gold_path and (not isinstance(entry, dict) or not isinstance(entry.get('expected_paths'), list) or not entry['expected_paths']):
                raise ValueError('Every measured task needs nonempty expected_paths in private gold')
            if entry:
                for path in entry.get('expected_paths', []):
                    if normalize_path(path, root) != path:
                        raise ValueError('Gold paths must be repository-relative normalized paths')
                for region in entry.get('expected_regions', []):
                    if not isinstance(region, dict) or region.get('path') not in entry['expected_paths'] or not isinstance(region.get('start_line'), int) or isinstance(region.get('start_line'), bool) or not isinstance(region.get('end_line'), int) or isinstance(region.get('end_line'), bool) or not 1 <= region['start_line'] <= region['end_line']:
                        raise ValueError('Gold regions need valid inclusive pre-fix line ranges')
                    source_lines = source_line_count(region['path'], root)
                    if source_lines is None or region['end_line'] > source_lines:
                        raise ValueError('Gold regions must exist within regular in-root pre-fix source')
        if not self.binary.is_file():
            raise ValueError('Kiro binary does not exist')
        self.version = subprocess.run(['claude', '--version'], check=True, capture_output=True, text=True, timeout=15).stdout.strip()
        self.out.mkdir(parents=True, exist_ok=True)
        self.lock, self.stop, self.active = threading.Lock(), threading.Event(), set()
        self.results, self.not_started, self.failure = [], [], None
        self.started_indices = set()
        self.started_at, self.finished_at = utc_now(), None
        self.roots = sorted({Path(task['project']) for task in self.tasks})
        self.before = {str(root): tree_hash(root) for root in self.roots}
        self.binary_hash = file_sha256(self.binary)
        self.tasks_hash = file_sha256(self.tasks_path)
        self.gold_hash = file_sha256(self.gold_path) if self.gold_path else None
        self.gold_unchanged = None
        self.runner_hash = file_sha256(__file__)
        self.jobs, self.unchanged = self.schedule(), None
        self.write_results()
        (self.out / 'private-tree-hashes.json').write_text(json.dumps(self.before, indent=2) + '\n')

    def schedule(self):
        jobs = []
        for rep in range(1, self.args.repetitions + 1):
            ordered = list(self.tasks)
            random.Random(self.args.seed + rep).shuffle(ordered)
            for task in ordered:
                offset = (self.tasks.index(task) + rep - 1) % len(CONDITIONS)
                order = CONDITIONS[offset:] + CONDITIONS[:offset]
                for condition in order:
                    jobs.append((len(jobs), task, condition, rep))
        return jobs

    def kill_active(self):
        with self.lock:
            active = list(self.active)
        for process in active:
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass

    def run_one(self, job):
        index, task, condition, rep = job
        if self.stop.is_set():
            with self.lock:
                self.not_started.append(index)
            return
        root = Path(task['project'])
        run_dir = self.out / f"{task['id']}-{condition}-{rep}"
        run_dir.mkdir(exist_ok=False)
        config = {'mcpServers': {}}
        if condition in MCP_TOOLS:
            config['mcpServers']['kiro'] = {'type': 'stdio', 'command': str(self.binary),
                'args': ['mcp', '--project', str(root), '--tools', ','.join(MCP_TOOLS[condition])], 'alwaysLoad': True}
        config_path = run_dir / 'mcp.json'
        config_path.write_text(json.dumps(config))
        allowed = list(BUILTINS) + [f'mcp__kiro__{tool}' for tool in MCP_TOOLS.get(condition, ())]
        command = ['/usr/bin/sandbox-exec', '-p', sandbox_profile(root, config_path, self.out, self.denied + [other for other in self.roots if other != root]),
            'claude', '-p', '--output-format', 'stream-json', '--verbose', '--model', self.args.model,
            '--effort', 'medium', '--no-session-persistence', '--strict-mcp-config', '--mcp-config', str(config_path),
            '--tools', ','.join(BUILTINS), '--allowedTools', ','.join(allowed), '--permission-mode', 'dontAsk',
            '--disable-slash-commands', '--no-chrome', '--setting-sources', '', '--max-budget-usd', str(self.args.max_budget_usd)]
        with self.lock:
            self.started_indices.add(index)
        started_at, started = utc_now(), time.monotonic()
        process, timed_out, launch_error = None, False, None
        try:
            with (run_dir / 'trace.jsonl').open('w') as stdout, (run_dir / 'stderr.txt').open('w') as stderr:
                process = subprocess.Popen(command, cwd=root, stdin=subprocess.PIPE, stdout=stdout, stderr=stderr,
                                           text=True, start_new_session=True, env={**os.environ, **ENV_OVERRIDES})
                with self.lock:
                    self.active.add(process)
                if self.stop.is_set():
                    self.kill_active()
                try:
                    process.communicate(COMMON + task['prompt'], timeout=self.args.timeout_seconds)
                except subprocess.TimeoutExpired:
                    timed_out = True
                    try:
                        os.killpg(process.pid, signal.SIGKILL)
                    except ProcessLookupError:
                        pass
                    process.wait()
        except (OSError, ValueError) as error:
            launch_error = str(error)
            if process and process.poll() is None:
                try:
                    os.killpg(process.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                process.wait()
        finally:
            if process:
                with self.lock:
                    self.active.discard(process)
        elapsed, finished_at = time.monotonic() - started, utc_now()
        events = []
        trace = run_dir / 'trace.jsonl'
        if trace.exists():
            for line in trace.read_text().splitlines():
                try:
                    events.append(json.loads(line))
                except json.JSONDecodeError:
                    continue
        row = {'task_id': task['id'], 'condition': condition, 'rep': rep, 'schedule_index': index,
               'started_at_utc': started_at, 'finished_at_utc': finished_at, 'wall_seconds': elapsed,
               **parse_events(events, task, root, process.returncode if process else None, timed_out, self.gold.get(task['id']))}
        row['tool_availability_errors'] = availability_errors(row, condition, self.args.model)
        if launch_error:
            row['launch_error'] = sanitized(launch_error, root)
        (run_dir / 'result.json').write_text(json.dumps(row, indent=2) + '\n')
        with self.lock:
            self.results.append(row)
            self.write_results()
            print(json.dumps({key: row[key] for key in ('task_id', 'condition', 'rep', 'wall_seconds', 'turns',
                'cost_usd', 'complete', 'path_at5_hit', 'region_overlap_at5', 'tools', 'result_subtype')}), flush=True)

    def write_results(self):
        args = self.args
        metadata = {'model': args.model, 'effort': 'medium', 'kiro_commit': args.kiro_commit,
            'binary_sha256': self.binary_hash, 'tasks_sha256': self.tasks_hash, 'gold_sha256': self.gold_hash,
            'runner_sha256': self.runner_hash, 'common_prompt_sha256': hashlib.sha256(COMMON.encode()).hexdigest(),
            'date': args.date, 'repetitions': args.repetitions, 'pilot': args.pilot, 'conditions': list(CONDITIONS),
            'experiment_started_at_utc': self.started_at, 'experiment_finished_at_utc': self.finished_at,
            'measurement_scope': 'Complete CLI wall time includes startup, MCP readiness, model requests, tools and final output.',
            'baseline': 'Bash, Read, Grep and Glob in every condition; scripts/batching allowed. Scout adds run/scout; diagnose also adds diagnose.',
            'project_read_isolation': 'Every other case projection is OS-denied to prevent cross-case snapshot leakage.',
            'builtin_tools': list(BUILTINS), 'mcp_tools_by_condition': {key: list(value) for key, value in MCP_TOOLS.items()},
            'permission_mode': 'dontAsk; explicit tools allowed; OS denies writes to all projections and private gold/results, and reads of other projections/private gold/results. Own MCP config remains readable.',
            'environment_overrides': ENV_OVERRIDES, 'kiro_server_config': {'type': 'stdio', 'alwaysLoad': True},
            'tool_availability_scope': 'Exact initial toolsets, connected MCP servers and observed tool uses checked for each condition.',
            'tool_availability_verified': len(self.results) == len(self.jobs) and all(not row['tool_availability_errors'] for row in self.results),
            'tool_availability_errors': [{'task_id': row['task_id'], 'condition': row['condition'], 'rep': row['rep'], 'errors': row['tool_availability_errors']} for row in self.results if row['tool_availability_errors']],
            'max_parallel': args.max_parallel, 'filesystem_cache': 'warm/shared', 'claude_code_version': self.version,
            'cost_scope': 'CLI-reported total model cost including any auxiliary models; not a billing invoice.',
            'tokens_scope': 'Sum of the four top-level CLI input/cache-creation/cache-read/output counters; cached context included. Null if any counter is absent.',
            'missing_metric_policy': 'Missing/nonfinite/invalid numeric usage and duration values are null, never imputed as zero.',
            'region_validation': 'Ranges must be 1-based, inclusive, at most 40 lines, within EOF of a verified regular in-root source of at most 16 MiB. LF/CRLF only are line boundaries. Invalid ranges are counted and excluded without dropping file predictions or timings.',
            'kiro_refusals_scope': 'Only trusted run transport structuredContent commands with status refused; null when any run result lacks that field.',
            'quality_scope': 'Top-5 production patch-file hit/recall and pre-fix changed-region overlap are localization proxies, not diagnosis correctness or verified fixes.',
            'timeout_seconds': args.timeout_seconds, 'max_budget_usd': args.max_budget_usd, 'schedule_seed': args.seed,
            'planned_runs': len(self.jobs), 'started_runs': len(self.started_indices), 'finished_runs': len(self.results),
            'not_started_schedule_indices': sorted(set(range(len(self.jobs))) - self.started_indices),
            'not_finished_schedule_indices': sorted(self.started_indices - {row['schedule_index'] for row in self.results}), 'datasets_unchanged': self.unchanged,
            'experiment_valid': self.failure is None and (self.finished_at is None or len(self.results) == len(self.jobs))
                and all(not row['tool_availability_errors'] for row in self.results),
            'invalid_reason': self.failure, 'dataset_fingerprints': {task['dataset']: self.before[task['project']] for task in self.tasks},
            'gold_unchanged': self.gold_unchanged,
            'repositories': sorted({(task['repository'], task['source_commit']) for task in self.tasks}),
            'schedule': [{'index': index, 'task_id': task['id'], 'condition': condition, 'rep': rep} for index, task, condition, rep in self.jobs],
            'limitations': ['One primary model on one machine; fresh conversations but provider/filesystem caches are shared.',
                'One repetition per case/condition by default; variation and pairing are descriptive, not significance proof.',
                'Reference patch locations are proxies. Some valid diagnoses may name other files, and patches can touch noncausal files.',
                'No project code, builds or tests are run; no claim about verified bug repairs.']}
        tasks = [{key: value for key, value in task.items() if key not in ('project', 'expected_paths', 'expected_regions', 'ground_truth', 'patch', 'test_patch')} for task in self.tasks]
        payload = {'metadata': metadata, 'tasks': tasks, 'runs': sorted(self.results, key=lambda row: row['schedule_index'])}
        temporary = self.out / 'results.json.tmp'
        temporary.write_text(json.dumps(payload, indent=2) + '\n')
        temporary.replace(self.out / 'results.json')
        fields = ['task_id', 'condition', 'rep', 'schedule_index', 'started_at_utc', 'finished_at_utc', 'wall_seconds',
            'cli_reported_seconds', 'api_reported_seconds', 'turns', 'tokens', 'cost_usd', 'complete', 'parsed_answer',
            'path_at5_hit', 'file_recall_at5', 'gold_files_found_at5', 'gold_files_total', 'region_overlap_at5', 'region_evaluable', 'valid_region_count', 'invalid_regions',
            'kiro_refusals', 'kiro_refusal_count_complete', 'kiro_error_results', 'permission_denials', 'model', 'returncode',
            'timed_out', 'result_subtype', 'init_observed']
        buffer = io.StringIO()
        writer = csv.DictWriter(buffer, fieldnames=fields, extrasaction='ignore', lineterminator='\n')
        writer.writeheader()
        writer.writerows(payload['runs'])
        temporary = self.out / 'results.csv.tmp'
        temporary.write_text(buffer.getvalue())
        temporary.replace(self.out / 'results.csv')

    def run(self):
        pool = concurrent.futures.ThreadPoolExecutor(max_workers=self.args.max_parallel)
        futures = []
        try:
            futures = [pool.submit(self.run_one, job) for job in self.jobs]
            for future in concurrent.futures.as_completed(futures):
                future.result()
        except BaseException as error:
            self.stop.set()
            self.failure = f'Runner aborted: {type(error).__name__}: {error}'
            self.kill_active()
            for future in futures:
                future.cancel()
            raise
        finally:
            pool.shutdown(wait=True, cancel_futures=True)
            self.unchanged = all(tree_hash(root) == self.before[str(root)] for root in self.roots)
            if not self.unchanged:
                self.failure = 'Dataset changed during benchmark; results invalid'
            for path, expected, label in [(self.binary, self.binary_hash, 'binary'), (self.tasks_path, self.tasks_hash, 'tasks')]:
                if file_sha256(path) != expected:
                    self.failure = f'{label} changed during benchmark; results invalid'
            if self.gold_path:
                try:
                    self.gold_unchanged = file_sha256(self.gold_path) == self.gold_hash
                except OSError:
                    self.gold_unchanged = False
                if not self.gold_unchanged:
                    self.failure = 'Private reference annotations changed during benchmark; all results invalid'
            if file_sha256(__file__) != self.runner_hash:
                self.failure = 'Runner changed during benchmark; results invalid'
            self.finished_at = utc_now()
            self.write_results()
        costs = [row['cost_usd'] for row in self.results]
        print(json.dumps({'done': len(self.results), 'datasets_unchanged': self.unchanged,
            'experiment_valid': self.failure is None and all(not row['tool_availability_errors'] for row in self.results),
            'reported_cost_usd': sum(costs) if all(value is not None for value in costs) else None}), flush=True)
        return 0 if self.failure is None and len(self.results) == len(self.jobs) and all(not row['tool_availability_errors'] for row in self.results) else 1


def arguments(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    for flag in ('tasks', 'binary', 'output', 'kiro-commit', 'date'):
        parser.add_argument('--' + flag, required=True)
    parser.add_argument('--projects-json', help='JSON mapping dataset IDs to source directories; repeated --project entries override it')
    parser.add_argument('--gold', help='Private JSON indexed by case ID; its parent directory is OS-denied to Claude')
    parser.add_argument('--deny-read', action='append', default=[], help='Additional private directories OS-denied to Claude')
    parser.add_argument('--repetitions', type=int, default=1)
    parser.add_argument('--model', default='claude-sonnet-4-6')
    parser.add_argument('--max-parallel', type=int, default=4)
    parser.add_argument('--timeout-seconds', type=float, default=480)
    parser.add_argument('--max-budget-usd', type=float, default=3)
    parser.add_argument('--seed', type=int, default=20261123)
    parser.add_argument('--pilot', action='store_true')
    parser.add_argument('--project', action='append', default=[], metavar='DATASET=PATH')
    args = parser.parse_args(argv)
    if not re.fullmatch(r'[0-9a-fA-F]{40}', args.kiro_commit):
        parser.error('--kiro-commit must be a full 40-character commit SHA')
    try:
        datetime.date.fromisoformat(args.date)
    except ValueError:
        parser.error('--date must be YYYY-MM-DD')
    if min(args.repetitions, args.max_parallel, args.timeout_seconds, args.max_budget_usd) <= 0:
        parser.error('repetitions, parallelism, timeout and budget must be positive')
    if any('=' not in value for value in args.project):
        parser.error('--project must be DATASET=PATH')
    return args


if __name__ == '__main__':
    raise SystemExit(Experiment(arguments()).run())
