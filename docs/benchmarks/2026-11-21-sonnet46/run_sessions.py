#!/usr/bin/env python3
"""Real, read-only Claude Code sessions. Keep raw traces private; publish sanitized results."""
import argparse
import concurrent.futures
import datetime
import hashlib
import json
import os
import pathlib
import random
import re
import signal
import subprocess
import threading
import time

parser = argparse.ArgumentParser()
parser.add_argument('--tasks', required=True)
parser.add_argument('--binary', required=True)
parser.add_argument('--output', required=True)
parser.add_argument('--repetitions', type=int, default=5)
parser.add_argument('--model', default='claude-sonnet-4-6')
parser.add_argument('--max-parallel', type=int, default=2)
parser.add_argument('--pilot', action='store_true')
parser.add_argument('--project', action='append', default=[], metavar='DATASET=PATH')
args = parser.parse_args()
out = pathlib.Path(args.output).resolve()
out.mkdir(parents=True, exist_ok=True)
tasks = json.loads(pathlib.Path(args.tasks).read_text())
project_map = dict(value.split('=', 1) for value in args.project)
for task in tasks:
    if 'project' not in task:
        task['project'] = project_map[task['dataset']]
binary = str(pathlib.Path(args.binary).resolve())
lock = threading.Lock()
results = []

COMMON = '''This is a read-only source-code investigation. Do not modify files, run builds, install packages, access the network, or launch other agents. Inspect the local source to support your answer. Batch independent searches/reads when useful. Return only JSON with this shape: {"files":[{"path":"repository-relative/source/file","reason":"what is implemented there"}],"summary":"brief explanation"}. Include production source files only; exclude tests, documentation, generated files and dependencies. Be precise about which file implements each requested responsibility.\n\n'''

def tree_hash(root):
    digest = hashlib.sha256()
    for path in sorted(root.rglob('*')):
        if '.git' in path.relative_to(root).parts or not path.is_file():
            continue
        digest.update(str(path.relative_to(root)).encode())
        digest.update(path.read_bytes())
    return digest.hexdigest()

def sanitized(value, root):
    if isinstance(value, str):
        return value.replace(str(root), '<PROJECT>').replace(str(pathlib.Path.home()), '<HOME>')
    if isinstance(value, dict):
        return {k: sanitized(v, root) for k, v in value.items()}
    if isinstance(value, list):
        return [sanitized(v, root) for v in value]
    return value

def answer_files(answer, root):
    text = answer.strip()
    text = re.sub(r'^```(?:json)?\s*|\s*```$', '', text)
    try:
        data = json.loads(text)
    except json.JSONDecodeError:
        start, end = text.find('{'), text.rfind('}')
        try: data = json.loads(text[start:end + 1])
        except (json.JSONDecodeError, ValueError): return [], False
    files = []
    for item in data.get('files', []):
        path = item.get('path', '') if isinstance(item, dict) else item
        if not isinstance(path, str): continue
        path = path.replace(str(root) + '/', '').removeprefix('./')
        files.append(path)
    return sorted(set(files)), bool(isinstance(data.get('files'), list))

def run_one(task, condition, rep):
    root = pathlib.Path(task['project']).resolve()
    label = f"{task['id']}-{condition}-{rep}"
    run_dir = out / label
    run_dir.mkdir(exist_ok=True)
    config = {'mcpServers': {}}
    if condition == 'kiro':
        config['mcpServers']['kiro'] = {'command': binary, 'args': ['mcp', '--project', str(root)]}
    config_path = run_dir / 'mcp.json'
    config_path.write_text(json.dumps(config))
    profile = '(version 1)(allow default)(deny file-write* (subpath ' + json.dumps(str(root)) + '))'
    command = [
        '/usr/bin/sandbox-exec', '-p', profile, 'claude', '-p',
        '--output-format', 'stream-json', '--verbose', '--model', args.model,
        '--effort', 'medium', '--no-session-persistence', '--strict-mcp-config',
        '--mcp-config', str(config_path), '--tools', 'Bash',
        '--allowedTools', 'Bash,mcp__kiro__run,mcp__kiro__scout',
        '--permission-mode', 'dontAsk', '--disable-slash-commands', '--no-chrome',
        '--setting-sources', '', '--max-budget-usd', '3',
    ]
    started = time.monotonic()
    with open(run_dir / 'trace.jsonl', 'w') as stdout, open(run_dir / 'stderr.txt', 'w') as stderr:
        process = subprocess.Popen(command, cwd=root, stdin=subprocess.PIPE, stdout=stdout,
                                   stderr=stderr, text=True, start_new_session=True)
        try:
            process.communicate(COMMON + task['prompt'], timeout=480)
            timed_out = False
        except subprocess.TimeoutExpired:
            os.killpg(process.pid, signal.SIGKILL)
            process.wait()
            timed_out = True
    elapsed = time.monotonic() - started
    events = []
    for line in (run_dir / 'trace.jsonl').read_text().splitlines():
        try: events.append(json.loads(line))
        except json.JSONDecodeError: continue
    final = next((event for event in reversed(events) if event.get('type') == 'result'), {})
    init = next((event for event in events if event.get('type') == 'system' and event.get('subtype') == 'init'), {})
    tools, refusals, denied = {}, 0, 0
    tool_names = {}
    for event in events:
        blocks = event.get('message', {}).get('content', [])
        if not isinstance(blocks, list): continue
        for block in blocks:
            if block.get('type') == 'tool_use':
                name = block.get('name', '')
                tool_names[block.get('id')] = name
                name = {'mcp__kiro__run': 'run', 'mcp__kiro__scout': 'scout'}.get(name, name)
                tools[name] = tools.get(name, 0) + 1
            elif block.get('type') == 'tool_result':
                content = block.get('content', '')
                text = content if isinstance(content, str) else '\n'.join(b.get('text', '') for b in content if isinstance(b, dict))
                # Count actual Kiro refusals, not a source file quoting the marker.
                if tool_names.get(block.get('tool_use_id')) == 'mcp__kiro__run':
                    refusals += len(re.findall(r'^\[not run:', text, re.MULTILINE))
                denied += int(bool(block.get('is_error')) and any(s in text.lower() for s in ['permission', 'denied', 'not allowed']))
    answer = final.get('result', '')
    files, parsed = answer_files(answer, root)
    usage = final.get('usage', {})
    tokens = sum(usage.get(k, 0) for k in ['input_tokens', 'cache_creation_input_tokens', 'cache_read_input_tokens', 'output_tokens'])
    complete = final.get('subtype') == 'success' and not timed_out and process.returncode == 0
    row = {
        'task_id': task['id'], 'condition': condition, 'rep': rep,
        'wall_seconds': elapsed, 'model_seconds': final.get('duration_ms', 0) / 1000,
        'turns': final.get('num_turns', 0), 'tokens': tokens,
        'usage': usage, 'cost_usd': final.get('total_cost_usd', 0),
        'complete': complete, 'parsed_answer': parsed,
        'expected_paths_mentioned': complete and parsed and set(task['expected_paths']).issubset(files),
        'required_paths_found': len(set(task['expected_paths']) & set(files)),
        'required_paths_total': len(task['expected_paths']),
        'returned_files': files, 'tools': tools, 'kiro_refusals': refusals, 'permission_denials': denied,
        'model': init.get('model'), 'returncode': process.returncode, 'timed_out': timed_out,
        'answer': sanitized(answer, root), 'errors': sanitized(final.get('errors', []), root),
    }
    (run_dir / 'result.json').write_text(json.dumps(row, indent=2) + '\n')
    with lock:
        results.append(row)
        write_results()
        print(json.dumps({k: row[k] for k in ['task_id','condition','rep','wall_seconds','turns','cost_usd','complete','expected_paths_mentioned','tools']}), flush=True)
    return row

def write_results():
    metadata = {
        'model': args.model, 'effort': 'medium', 'kiro_commit': '3c17a2ec4aa568e2e46ad636b06ee5381e00ee48',
        'date': '2026-11-21', 'repetitions': args.repetitions, 'pilot': args.pilot,
        'measurement_scope': 'Complete Claude Code sessions; wall time includes process and MCP startup, model requests and tool execution.',
        'baseline': 'Claude Code with Bash only; independent searches may be batched.',
        'kiro': 'Same model and prompt, Bash plus Kiro run/scout over MCP; Bash fallback allowed.',
        'permission_mode': 'dontAsk; explicit tools allowed; OS denies writes to the project tree.',
        'max_parallel': args.max_parallel, 'filesystem_cache': 'warm/shared',
        'claude_code_version': '2.1.173', 'cost_scope': 'Model cost reported by Claude Code, not a billing invoice.',
        'quality_scope': 'All required production paths present in parsed JSON; this is location recall, not semantic correctness.',
        'repositories': list({task['repository']: {'url':task['repository'], 'commit':task['source_commit']} for task in tasks}.values()),
        'limitations': ['3 tasks on 2 public repositories; 5 sessions per task and condition.', 'Provider latency and prompt caching vary; exploratory single-machine comparison.', 'Kiro itself is one small test repository; Fastify is the larger external repository.', 'No claim about editing, tests, builds or general agent correctness.'],
    }
    public_tasks = [{k:v for k,v in task.items() if k != 'project'} for task in tasks]
    payload = {'metadata': metadata, 'tasks': public_tasks, 'runs': results}
    temporary = out / 'results.json.tmp'
    temporary.write_text(json.dumps(payload, indent=2) + '\n')
    temporary.replace(out / 'results.json')

if __name__ == '__main__':
    roots = sorted({pathlib.Path(task['project']).resolve() for task in tasks})
    before = {str(root): tree_hash(root) for root in roots}
    (out / 'private-tree-hashes.json').write_text(json.dumps(before, indent=2))
    jobs = []
    for rep in range(1, args.repetitions + 1):
        ordered = list(tasks)
        random.Random(20261121 + rep).shuffle(ordered)
        for task in ordered:
            conditions = ['terminal','kiro'] if (rep + tasks.index(task)) % 2 else ['kiro','terminal']
            jobs.extend((task, condition, rep) for condition in conditions)
    with concurrent.futures.ThreadPoolExecutor(max_workers=args.max_parallel) as pool:
        list(pool.map(lambda job: run_one(*job), jobs))
    after = {str(root): tree_hash(root) for root in roots}
    assert before == after, 'Dataset changed during benchmark; results invalid'
    payload = json.loads((out / 'results.json').read_text())
    payload['metadata']['datasets_unchanged'] = True
    (out / 'results.json').write_text(json.dumps(payload, indent=2) + '\n')
    print(json.dumps({'done':len(results), 'datasets_unchanged':True, 'reported_cost_usd':sum(row['cost_usd'] for row in results)}), flush=True)
