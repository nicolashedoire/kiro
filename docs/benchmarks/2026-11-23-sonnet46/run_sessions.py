#!/usr/bin/env python3
"""Real Claude Code sessions; retain every started run and keep raw traces private.

This macOS runner makes paid model calls only from main(). Importing it is safe for
unit checks. Location coverage is not a semantic-correctness score. The token metric
sums disjoint CLI-reported input/cache-creation/cache-read/output counters, including
reused context; it is not a count of unique text or a billing invoice.
"""
import argparse
import concurrent.futures
import csv
import datetime
import hashlib
import io
import json
import os
import pathlib
import random
import re
import signal
import subprocess
import threading
import time

COMMON = '''This is a read-only source-code investigation. Do not modify files, run builds, install packages, access the network, or launch other agents. Inspect the local source to support your answer. Batch independent searches/reads when useful. Return only JSON with this shape: {"files":[{"path":"repository-relative/source/file","reason":"what is implemented there"}],"summary":"brief explanation"}. Include production source files only; exclude tests, documentation, generated files and dependencies. Be precise about which file implements each requested responsibility.\n\n'''
TOKEN_FIELDS = ('input_tokens', 'cache_creation_input_tokens', 'cache_read_input_tokens', 'output_tokens')
TOOL_NAMES = {'mcp__kiro__run': 'run', 'mcp__kiro__scout': 'scout'}
# Identical CLI overrides in both conditions; make tool availability explicit at startup.
ENV_OVERRIDES = {'MCP_CONNECTION_NONBLOCKING': '0', 'ENABLE_TOOL_SEARCH': 'false'}


def utc_now():
    return datetime.datetime.now(datetime.timezone.utc).isoformat(timespec='milliseconds')


def number(value, default=0):
    return value if isinstance(value, (int, float)) and not isinstance(value, bool) else default


def sanitized(value, root):
    if isinstance(value, str):
        return value.replace(str(root), '<PROJECT>').replace(str(pathlib.Path.home()), '<HOME>')
    if isinstance(value, dict):
        return {k: sanitized(v, root) for k, v in value.items()}
    if isinstance(value, list):
        return [sanitized(v, root) for v in value]
    return value


def file_sha256(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def tree_hash(root):
    """Hash names, file modes and bytes; never follow a source-tree symlink."""
    digest = hashlib.sha256()
    for path in sorted(root.rglob('*')):
        relative = path.relative_to(root)
        if '.git' in relative.parts:
            continue
        if path.is_symlink():
            kind, content = b'link', os.readlink(path).encode()
        elif path.is_file():
            kind, content = b'file', path.read_bytes()
        else:
            continue
        for part in (str(relative).encode(), kind,
                     str(path.lstat().st_mode & 0o777).encode(), content):
            digest.update(len(part).to_bytes(8, 'big'))
            digest.update(part)
    return digest.hexdigest()


def answer_files(answer, root):
    """Accept a JSON object only; malformed model output cannot crash the experiment."""
    if not isinstance(answer, str):
        return [], False
    text = re.sub(r'^```(?:json)?\s*|\s*```$', '', answer.strip())
    try:
        data = json.loads(text)
    except (json.JSONDecodeError, ValueError):
        start, end = text.find('{'), text.rfind('}')
        if start < 0 or end < start:
            return [], False
        try:
            data = json.loads(text[start:end + 1])
        except (json.JSONDecodeError, ValueError):
            return [], False
    if not isinstance(data, dict) or not isinstance(data.get('files'), list):
        return [], False
    files, valid = [], True
    prefix = str(root) + '/'
    for item in data['files']:
        path = item.get('path') if isinstance(item, dict) else None
        if not isinstance(path, str) or not path:
            valid = False
            continue
        if path.startswith(prefix):
            path = path[len(prefix):]
        path = path.removeprefix('./')
        normalized = pathlib.PurePosixPath(path)
        if normalized.is_absolute() or '..' in normalized.parts or path in ('', '.'):
            valid = False
            continue
        files.append(str(normalized))
    return sorted(set(files)), valid


def content_blocks(event):
    message = event.get('message')
    blocks = message.get('content', []) if isinstance(message, dict) else []
    return [b for b in blocks if isinstance(b, dict)] if isinstance(blocks, list) else []


def structured_run_result(block, event):
    """Trust a transport field, not JSON or refusal banners quoted by arbitrary stdout."""
    candidates = [block.get('structuredContent'), block.get('structured_content')]
    value = event.get('tool_use_result')
    if isinstance(value, dict):
        candidates.extend([value.get('structuredContent'), value.get('structured_content')])
    for candidate in candidates:
        if (isinstance(candidate, dict) and candidate.get('schema_version') == 1
                and candidate.get('tool') == 'run' and isinstance(candidate.get('commands'), list)):
            return candidate
    return None


def parse_events(events, task, root, returncode, timed_out):
    events = [event for event in events if isinstance(event, dict)]
    final = next((e for e in reversed(events) if e.get('type') == 'result'), {})
    init = next((e for e in events if e.get('type') == 'system' and e.get('subtype') == 'init'), {})
    names, tools = {}, {}
    refusals, unobservable, error_results = 0, 0, 0
    for event in events:
        for block in content_blocks(event):
            if block.get('type') == 'tool_use':
                name = block.get('name')
                if not isinstance(name, str):
                    continue
                names[block.get('id')] = name
                short_name = TOOL_NAMES.get(name, name)
                tools[short_name] = tools.get(short_name, 0) + 1
            elif block.get('type') == 'tool_result':
                name = names.get(block.get('tool_use_id'))
                if name in TOOL_NAMES:
                    error_results += int(block.get('is_error') is True)
                if name == 'mcp__kiro__run':
                    structured = structured_run_result(block, event)
                    if structured is None:
                        unobservable += 1
                    else:
                        refusals += sum(isinstance(command, dict) and command.get('status') == 'refused'
                                        for command in structured['commands'])
    answer = final.get('result', '')
    files, parsed = answer_files(answer, root)
    usage = final.get('usage') if isinstance(final.get('usage'), dict) else {}
    complete = (final.get('subtype') == 'success' and final.get('is_error') is not True
                and not timed_out and returncode == 0)
    expected = set(task['expected_paths'])
    denials = final.get('permission_denials')
    reported_models = final.get('modelUsage')
    available_tools = init.get('tools')
    available_tools = sorted(t for t in available_tools if isinstance(t, str)) if isinstance(available_tools, list) else None
    servers = init.get('mcp_servers')
    servers = [{'name': server.get('name'), 'status': server.get('status')}
               for server in servers if isinstance(server, dict)] if isinstance(servers, list) else None
    connected = bool(servers and any(server['name'] == 'kiro' and server['status'] == 'connected' for server in servers))
    kiro_ready = (connected and available_tools is not None and set(TOOL_NAMES).issubset(available_tools)) if init else None
    return {
        'model_seconds': number(final.get('duration_ms')) / 1000,
        'cli_reported_seconds': number(final.get('duration_ms')) / 1000,
        'api_reported_seconds': number(final.get('duration_api_ms'), None) / 1000
            if number(final.get('duration_api_ms'), None) is not None else None,
        'turns': number(final.get('num_turns')),
        'tokens': sum(number(usage.get(key)) for key in TOKEN_FIELDS),
        'usage': usage, 'cost_usd': number(final.get('total_cost_usd')),
        'complete': complete, 'parsed_answer': parsed,
        'expected_paths_mentioned': complete and parsed and expected.issubset(files),
        'required_paths_found': len(expected & set(files)), 'required_paths_total': len(expected),
        'returned_files': files, 'tools': tools,
        'kiro_refusals': refusals if unobservable == 0 else None,
        'kiro_refusal_count_complete': unobservable == 0,
        'kiro_run_results_without_structure': unobservable,
        'kiro_error_results': error_results,
        'permission_denials': len(denials) if isinstance(denials, list) else None,
        'permission_denial_details': sanitized(denials, root),
        'model': init.get('model'),
        'init_observed': bool(init), 'available_tools': available_tools,
        'mcp_servers_at_init': servers, 'mcp_server_errors_at_init': sanitized(init.get('mcp_server_errors'), root),
        'kiro_tools_ready': kiro_ready,
        'reported_models': sorted(reported_models) if isinstance(reported_models, dict) else [],
        'returncode': returncode, 'timed_out': timed_out,
        'result_subtype': final.get('subtype'), 'result_is_error': final.get('is_error'),
        'stop_reason': final.get('stop_reason'), 'terminal_reason': final.get('terminal_reason'),
        'answer': sanitized(answer, root), 'errors': sanitized(final.get('errors', []), root),
    }


class Experiment:
    def __init__(self, args):
        self.args = args
        self.out = pathlib.Path(args.output).resolve()
        if self.out.exists() and any(self.out.iterdir()):
            raise ValueError('Output directory must be empty: measured sessions are never overwritten')
        self.out.mkdir(parents=True, exist_ok=True)
        self.tasks_path = pathlib.Path(args.tasks).resolve()
        self.tasks = json.loads(self.tasks_path.read_text())
        self.projects = dict(value.split('=', 1) for value in args.project)
        if not isinstance(self.tasks, list) or not self.tasks:
            raise ValueError('tasks must be a nonempty list')
        ids = set()
        for task in self.tasks:
            if not isinstance(task, dict) or any(not isinstance(task.get(k), str) or not task[k]
                                                for k in ('id', 'dataset', 'prompt', 'repository', 'source_commit')):
                raise ValueError('Every task needs id, dataset, prompt, repository and source_commit strings')
            if not re.fullmatch(r'[a-zA-Z0-9_-]+', task['id']) or task['id'] in ids:
                raise ValueError('Task IDs must be unique, filesystem-safe names')
            ids.add(task['id'])
            paths = task.get('expected_paths')
            if not isinstance(paths, list) or not paths or any(not isinstance(p, str) or not p
                    or pathlib.PurePosixPath(p).is_absolute() or '..' in pathlib.PurePosixPath(p).parts
                    or p == '.' for p in paths):
                raise ValueError('Expected paths must be a nonempty list of relative source paths')
            root = pathlib.Path(task.get('project') or self.projects[task['dataset']]).resolve()
            if not root.is_dir() or self.out.is_relative_to(root):
                raise ValueError('Project must exist, and benchmark output must be outside its tree')
            task['project'] = str(root)
        self.binary = pathlib.Path(args.binary).resolve()
        if not self.binary.is_file():
            raise ValueError('Kiro binary does not exist')
        self.version = subprocess.run(['claude', '--version'], check=True, capture_output=True,
                                      text=True, timeout=15).stdout.strip()
        self.lock = threading.Lock()
        self.stop = threading.Event()
        self.active = set()
        self.results = []
        self.not_started = []
        self.failure = None
        self.started_at = utc_now()
        self.finished_at = None
        self.roots = sorted({pathlib.Path(t['project']) for t in self.tasks})
        self.before = {str(root): tree_hash(root) for root in self.roots}
        self.binary_hash = file_sha256(self.binary)
        self.jobs = self.schedule()
        self.unchanged = None
        self.write_results()
        (self.out / 'private-tree-hashes.json').write_text(json.dumps(self.before, indent=2) + '\n')

    def schedule(self):
        jobs = []
        for rep in range(1, self.args.repetitions + 1):
            ordered = list(self.tasks)
            random.Random(self.args.seed + rep).shuffle(ordered)
            for task in ordered:
                conditions = ['terminal', 'kiro'] if (rep + self.tasks.index(task)) % 2 else ['kiro', 'terminal']
                for condition in conditions:
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
        root = pathlib.Path(task['project'])
        run_dir = self.out / f"{task['id']}-{condition}-{rep}"
        run_dir.mkdir(exist_ok=False)
        config = {'mcpServers': {}}
        if condition == 'kiro':
            config['mcpServers']['kiro'] = {'type': 'stdio', 'command': str(self.binary), 'args': ['mcp', '--project', str(root)], 'alwaysLoad': True}
        config_path = run_dir / 'mcp.json'
        config_path.write_text(json.dumps(config))
        profile = '(version 1)(allow default)(deny file-write* (subpath ' + json.dumps(str(root)) + '))'
        command = ['/usr/bin/sandbox-exec', '-p', profile, 'claude', '-p',
            '--output-format', 'stream-json', '--verbose', '--model', self.args.model,
            '--effort', 'medium', '--no-session-persistence', '--strict-mcp-config',
            '--mcp-config', str(config_path), '--tools', 'Bash',
            '--allowedTools', 'Bash,mcp__kiro__run,mcp__kiro__scout',
            '--permission-mode', 'dontAsk', '--disable-slash-commands', '--no-chrome',
            '--setting-sources', '', '--max-budget-usd', str(self.args.max_budget_usd)]
        started_at, started = utc_now(), time.monotonic()
        process, timed_out, launch_error = None, False, None
        try:
            with open(run_dir / 'trace.jsonl', 'w') as stdout, open(run_dir / 'stderr.txt', 'w') as stderr:
                process = subprocess.Popen(command, cwd=root, stdin=subprocess.PIPE, stdout=stdout,
                    stderr=stderr, text=True, start_new_session=True, env={**os.environ, **ENV_OVERRIDES})
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
        row = {'task_id': task['id'], 'condition': condition, 'rep': rep,
               'schedule_index': index, 'started_at_utc': started_at, 'finished_at_utc': finished_at,
               'wall_seconds': elapsed, **parse_events(events, task, root,
                   process.returncode if process else None, timed_out)}
        if condition == 'terminal':
            row['kiro_tools_ready'] = None
        row['tool_availability_errors'] = []
        if not row['init_observed']:
            row['tool_availability_errors'].append('No system/init event; tool availability unobservable')
        elif condition == 'kiro' and row['kiro_tools_ready'] is not True:
            row['tool_availability_errors'].append('Kiro run/scout not both available with a connected kiro server at init')
        elif condition == 'terminal' and row['available_tools'] != ['Bash']:
            row['tool_availability_errors'].append('Terminal baseline init toolset differs from Bash only')
        if launch_error:
            row['launch_error'] = sanitized(launch_error, root)
        (run_dir / 'result.json').write_text(json.dumps(row, indent=2) + '\n')
        changed = tree_hash(root) != self.before[str(root)]
        with self.lock:
            self.results.append(row)
            if changed:
                self.failure = 'Dataset changed during benchmark; stop and treat all results as invalid'
                self.unchanged = False
                self.stop.set()
            self.write_results()
            print(json.dumps({k: row[k] for k in ('task_id','condition','rep','wall_seconds','turns',
                  'cost_usd','complete','expected_paths_mentioned','tools','result_subtype')}), flush=True)
        if changed:
            self.kill_active()

    def write_results(self):
        args = self.args
        metadata = {
            'model': args.model, 'effort': 'medium', 'kiro_commit': args.kiro_commit,
            'binary_sha256': self.binary_hash, 'tasks_sha256': file_sha256(self.tasks_path),
            'common_prompt_sha256': hashlib.sha256(COMMON.encode()).hexdigest(),
            'date': args.date, 'repetitions': args.repetitions, 'pilot': args.pilot,
            'experiment_started_at_utc': self.started_at, 'experiment_finished_at_utc': self.finished_at,
            'measurement_scope': 'Complete Claude Code sessions; wall time includes process/MCP startup, model requests and tool execution.',
            'baseline': 'Claude Code with Bash only; independent searches may be batched.',
            'kiro': 'Same model and prompt, Bash plus Kiro run/scout over MCP; Bash fallback allowed.',
            'permission_mode': 'dontAsk; explicit tools allowed; OS denies writes to each project tree.',
            'environment_overrides': ENV_OVERRIDES,
            'kiro_server_config': {'type':'stdio', 'alwaysLoad':True},
            'tool_availability_scope': 'system/init tools and mcp_servers recorded for every run. Missing/pending Kiro tools are flagged; no measured session is excluded or rerun.',
            'tool_availability_verified': all(not row['tool_availability_errors'] for row in self.results)
                if self.finished_at else None,
            'tool_availability_errors': [{'task_id':row['task_id'], 'condition':row['condition'],
                'rep':row['rep'], 'errors':row['tool_availability_errors']} for row in self.results if row['tool_availability_errors']],
            'max_parallel': args.max_parallel, 'filesystem_cache': 'warm/shared',
            'claude_code_version': self.version,
            'cost_scope': 'Model cost reported by Claude Code, not a billing invoice.',
            'tokens_scope': 'Sum of reported input_tokens, cache_creation_input_tokens, cache_read_input_tokens and output_tokens. Includes cached context, not unique text.',
            'model_seconds_scope': 'Legacy alias of CLI duration_ms/1000; not pure model latency. api_reported_seconds is separate.',
            'kiro_refusals_scope': 'Count only transport structuredContent commands with status refused; null if a run result lacks that transport field. Never inferred from stdout banners.',
            'quality_scope': 'All required production paths present in valid parsed JSON; location coverage, not semantic correctness.',
            'timeout_seconds': args.timeout_seconds, 'max_budget_usd': args.max_budget_usd,
            'schedule_seed': args.seed,
            'planned_runs': len(self.jobs), 'started_runs': len(self.results) + len(self.active),
            'finished_runs': len(self.results),
            'not_started_schedule_indices': sorted(set(range(len(self.jobs))) - {r['schedule_index'] for r in self.results})
                if self.finished_at else sorted(self.not_started),
            'datasets_unchanged': self.unchanged,
            'experiment_valid': False if self.failure else (True if self.finished_at else None),
            'invalid_reason': self.failure,
            'dataset_fingerprints': {task['dataset']: self.before[task['project']] for task in self.tasks},
            'repositories': list({t['repository']: {'url':t['repository'], 'commit':t['source_commit']} for t in self.tasks}.values()),
            'schedule': [{'index':i,'task_id':t['id'],'condition':c,'rep':r} for i,t,c,r in self.jobs],
            'limitations': [f"{len(self.tasks)} tasks; {args.repetitions} sessions per task and condition.",
                'Provider latency and prompt caching vary; exploratory single-machine comparison.',
                'One small self-test repository and one external repository; no random task sample.',
                'No claim about editing, builds or general agent correctness.'],
        }
        tasks = [{k:v for k,v in task.items() if k != 'project'} for task in self.tasks]
        runs = sorted(self.results, key=lambda row: row['schedule_index'])
        payload = {'metadata': metadata, 'tasks': tasks, 'runs': runs}
        temporary = self.out / 'results.json.tmp'
        temporary.write_text(json.dumps(payload, indent=2) + '\n')
        temporary.replace(self.out / 'results.json')
        fields = ['task_id','condition','rep','schedule_index','started_at_utc','finished_at_utc',
                  'wall_seconds','cli_reported_seconds','api_reported_seconds','turns','tokens','cost_usd',
                  'complete','parsed_answer','expected_paths_mentioned','required_paths_found',
                  'required_paths_total','kiro_refusals','kiro_refusal_count_complete','kiro_error_results',
                  'permission_denials','model','returncode','timed_out','result_subtype','init_observed','kiro_tools_ready']
        buffer = io.StringIO()
        writer = csv.DictWriter(buffer, fieldnames=fields, extrasaction='ignore')
        writer.writeheader()
        writer.writerows(runs)
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
            if file_sha256(self.binary) != self.binary_hash:
                self.failure = 'Kiro binary changed during benchmark; results invalid'
            self.finished_at = utc_now()
            self.write_results()
        print(json.dumps({'done':len(self.results),'datasets_unchanged':self.unchanged,
              'experiment_valid':self.failure is None,
              'reported_cost_usd':sum(row['cost_usd'] for row in self.results)}), flush=True)
        return 0 if self.failure is None else 1


def arguments(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    for flag in ('tasks','binary','output','kiro-commit','date'):
        parser.add_argument('--' + flag, required=True)
    parser.add_argument('--repetitions', type=int, default=5)
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
