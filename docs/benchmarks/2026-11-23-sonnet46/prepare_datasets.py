#!/usr/bin/env python3
"""Create pinned source projections without separate benchmark, documentation or test trees."""
import argparse
import hashlib
import json
from pathlib import Path
import subprocess

KIRO_COMMIT = '55d26e934589e7c15d69c882d5f956493878dca5'
FASTIFY_COMMIT = '6de5abedb24424f0e8fa3612035d212d580f8d90'


def project(checkout, commit, output, dataset):
    names = subprocess.check_output(['git', '-C', str(checkout), 'ls-tree', '-r', '--name-only', '-z', commit]).decode().split('\0')
    if dataset == 'kiro':
        selected = [name for name in names if (name.startswith('src/') and name.endswith('.rs')) or name in ('Cargo.toml', 'Cargo.lock')]
    else:
        selected = [name for name in names if name in ('fastify.js', 'package.json') or (name.startswith('lib/') and name.endswith('.js'))]
    if output.exists() and any(output.iterdir()):
        raise ValueError(f'Refusing to overwrite nonempty dataset: {output}')
    output.mkdir(parents=True, exist_ok=True)
    files = []
    digest = hashlib.sha256()
    for name in sorted(selected):
        data = subprocess.check_output(['git', '-C', str(checkout), 'show', f'{commit}:{name}'])
        path = output / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(data)
        # The measured files were 0644. Keep their fingerprints independent of umask.
        path.chmod(0o644)
        for part in (name.encode(), b'file', str(path.stat().st_mode & 0o777).encode(), data):
            digest.update(len(part).to_bytes(8, 'big'))
            digest.update(part)
        files.append({'path': name, 'bytes': len(data), 'sha256': hashlib.sha256(data).hexdigest()})
    if not files:
        raise ValueError('Empty production projection')
    return {'commit': commit, 'projection': 'Production source and package metadata only; no docs, tests, examples, .git, benchmarks or previous answers.', 'tree_sha256': digest.hexdigest(), 'file_count': len(files), 'files': files, 'hash_scope': 'Sorted relative file names, kind, permission mode and contents, each length-framed; same algorithm as session runner.'}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--kiro-checkout', required=True, type=Path)
    parser.add_argument('--fastify-checkout', required=True, type=Path)
    parser.add_argument('--output', required=True, type=Path)
    parser.add_argument('--manifest', required=True, type=Path)
    args = parser.parse_args()
    manifest = {
        'kiro': project(args.kiro_checkout, KIRO_COMMIT, args.output / 'kiro', 'kiro'),
        'fastify': project(args.fastify_checkout, FASTIFY_COMMIT, args.output / 'fastify', 'fastify'),
    }
    args.manifest.parent.mkdir(parents=True, exist_ok=True)
    args.manifest.write_text(json.dumps(manifest, indent=2) + '\n')
    print(json.dumps({key: {k: value[k] for k in ('commit', 'tree_sha256', 'file_count')} for key, value in manifest.items()}, indent=2))


if __name__ == '__main__':
    main()
