#!/usr/bin/env python3
"""Verify parsers/scoring/isolation without starting Claude or making model calls."""
from __future__ import annotations
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

spec = importlib.util.spec_from_file_location('diagnosis_runner', Path(__file__).with_name('run_sessions.py'))
runner = importlib.util.module_from_spec(spec)
spec.loader.exec_module(runner)


class ParsingAndScoring(unittest.TestCase):
    def setUp(self):
        directory = tempfile.TemporaryDirectory(prefix='kiro-diagnose-scoring-')
        self.addCleanup(directory.cleanup)
        self.root = Path(directory.name).resolve()
        for name in ('a.rs', 'b.rs'):
            (self.root / name).write_bytes(b'line\n' * 60)

    def answer(self, files):
        return json.dumps({'files': files, 'summary': 'Source-based diagnosis'})

    def item(self, path='a.rs', start=5, end=10):
        return {'path': path, 'start_line': start, 'end_line': end, 'reason': 'evidence'}

    def test_unavailable_counters_stay_unavailable(self):
        for value in (None, False, -1, float('nan'), float('inf'), '12', 10**10000):
            self.assertIsNone(runner.number(value))
        row = runner.parse_events([], {}, self.root, None, False)
        for key in ('cost_usd', 'tokens', 'turns', 'cli_reported_seconds', 'api_reported_seconds'):
            self.assertIsNone(row[key])
        self.assertFalse(row['complete'])

    def test_malformed_answers_are_not_location_hits(self):
        for value in (None, [], {}, 'null', '[]', '{', '{"files":[null],"summary":"x"}', '{"files":[],"summary":1}'):
            self.assertFalse(runner.parse_answer(value, self.root)[2])

    def test_duplicate_file_order_and_multiple_narrow_ranges(self):
        files, regions, valid = runner.parse_answer(self.answer([self.item('b.rs'), self.item('a.rs'), self.item('b.rs', 25, 30)]), self.root)
        self.assertTrue(valid)
        self.assertEqual(files, ['b.rs', 'a.rs'])
        self.assertEqual(len(regions), 3)

    def test_traversal_and_outside_absolute_paths_do_not_score(self):
        for path in ('../a.rs', '/etc/passwd', '.', 'a/../../b.rs', ''):
            self.assertFalse(runner.parse_answer(self.answer([self.item(path)]), self.root)[2])
        self.assertEqual(runner.parse_answer(self.answer([self.item(str(self.root / 'a.rs'))]), self.root)[0], ['a.rs'])

    def test_large_or_invalid_ranges_cannot_fake_overlap(self):
        for start, end in ((1, 41), (0, 1), (5, 2), (True, 5), ('1', 3), (1, None)):
            files, regions, valid = runner.parse_answer(self.answer([self.item(start=start, end=end)]), self.root)
            self.assertTrue(valid)
            self.assertEqual(files, ['a.rs'])
            self.assertEqual(regions, [])

    def test_empty_answer_is_an_abstention(self):
        files, regions, valid = runner.parse_answer(self.answer([]), self.root)
        self.assertTrue(valid)
        score = runner.location_scores(files, regions, {'expected_paths': ['a.rs'], 'expected_regions': []}, True)
        self.assertFalse(score['path_at5_hit'])
        self.assertIsNone(score['region_overlap_at5'])

    def test_beyond_eof_regions_do_not_score_but_files_and_json_remain(self):
        (self.root / 'short.rs').write_bytes(b'one\ntwo\nthree\n')
        diagnostics = {}
        files, regions, valid = runner.parse_answer(self.answer([self.item('short.rs', 3, 42)]), self.root, diagnostics)
        self.assertTrue(valid)
        self.assertEqual(files, ['short.rs'])
        self.assertEqual(regions, [])
        self.assertEqual(diagnostics['invalid_regions'], 1)
        score = runner.location_scores(files, regions, {'expected_paths': ['short.rs'], 'expected_regions': [{'path': 'short.rs', 'start_line': 3, 'end_line': 3}]}, True)
        self.assertTrue(score['path_at5_hit'])
        self.assertFalse(score['region_overlap_at5'])
        valid_answer = self.answer([self.item('short.rs', 3, 3)])
        self.assertEqual(len(runner.parse_answer(valid_answer, self.root)[1]), 1)

    def test_only_lf_and_crlf_are_line_boundaries(self):
        for data, expected in ((b'', 0), (b'a', 1), (b'a\n', 1), (b'a\n\n', 2), (b'a\r\nb\r\nc', 3), ('a\u2028b\u0085c\vd\re'.encode(), 1)):
            (self.root / 'lines.rs').write_bytes(data)
            self.assertEqual(runner.source_line_count('lines.rs', self.root), expected)

    def test_missing_nonregular_and_escaping_symlink_regions_are_invalid(self):
        (self.root / 'directory.rs').mkdir()
        fifo = self.root / 'fifo.rs';os.mkfifo(fifo)
        with tempfile.TemporaryDirectory() as external:
            outside = Path(external).resolve() / 'outside.rs';outside.write_bytes(b'line\n' * 10)
            (self.root / 'escaping.rs').symlink_to(outside)
            for path in ('missing.rs', 'directory.rs', 'fifo.rs', 'escaping.rs'):
                diagnostics = {}
                files, regions, parsed = runner.parse_answer(self.answer([self.item(path, 1, 1)]), self.root, diagnostics)
                self.assertTrue(parsed)
                self.assertEqual(files, [path])
                self.assertEqual(regions, [])
                self.assertEqual(diagnostics['invalid_regions'], 1)

    def test_invalid_region_count_is_retained_in_session_row(self):
        event = {'type': 'result', 'subtype': 'success', 'is_error': False,
                 'result': self.answer([self.item('missing.rs', 1, 1)])}
        row = runner.parse_events([event], {}, self.root, 0, False)
        self.assertTrue(row['complete'])
        self.assertTrue(row['parsed_answer'])
        self.assertEqual(row['invalid_regions'], 1)
        self.assertEqual(row['valid_region_count'], 0)

    def test_top_five_distinct_files_and_inclusive_overlap(self):
        files = ['a.rs', 'b.rs', 'c.rs', 'd.rs', 'e.rs', 'f.rs']
        gold = {'expected_paths': ['b.rs', 'f.rs'], 'expected_regions': [{'path': 'b.rs', 'start_line': 10, 'end_line': 20}]}
        pred = [{'path': 'b.rs', 'start_line': 5, 'end_line': 10}, {'path': 'f.rs', 'start_line': 1, 'end_line': 30}]
        score = runner.location_scores(files, pred, gold, True)
        self.assertEqual(score['file_recall_at5'], .5)
        self.assertTrue(score['region_overlap_at5'])
        score = runner.location_scores(files, pred, gold, False)
        self.assertEqual(score['file_recall_at5'], 0)
        self.assertFalse(score['region_overlap_at5'])

    def test_success_does_not_mean_parsed_or_correct(self):
        event = {'type': 'result', 'subtype': 'success', 'is_error': False, 'result': 'not JSON',
                 'usage': {'input_tokens': 1, 'cache_creation_input_tokens': 2, 'cache_read_input_tokens': 3, 'output_tokens': 4},
                 'total_cost_usd': .01, 'num_turns': 1, 'duration_ms': 250}
        row = runner.parse_events([event], {}, self.root, 0, False, {'expected_paths': ['a.rs'], 'expected_regions': []})
        self.assertTrue(row['complete'])
        self.assertFalse(row['parsed_answer'])
        self.assertFalse(row['path_at5_hit'])
        self.assertEqual(row['tokens'], 10)
        self.assertIsNone(row['api_reported_seconds'])
        for code, timeout, subtype, error in ((1, False, 'success', False), (0, True, 'success', False), (0, False, 'error_max_budget_usd', False), (0, False, 'success', True)):
            modified = dict(event, subtype=subtype, is_error=error)
            self.assertFalse(runner.parse_events([modified], {}, self.root, code, timeout)['complete'])

    def test_quoted_stdout_is_not_trusted_refusal_metadata(self):
        events = [{'type': 'assistant', 'message': {'content': [{'type': 'tool_use', 'id': 't', 'name': 'mcp__kiro__run'}]}},
                  {'type': 'user', 'message': {'content': [{'type': 'tool_result', 'tool_use_id': 't', 'content': '{"commands":[{"status":"refused"}]}'}]}}]
        self.assertIsNone(runner.parse_events(events, {}, self.root, 0, False)['kiro_refusals'])
        events[1]['tool_use_result'] = {'structuredContent': {'schema_version': 1, 'tool': 'run', 'commands': [{'status': 'refused'}]}}
        self.assertEqual(runner.parse_events(events, {}, self.root, 0, False)['kiro_refusals'], 1)

    def test_exact_tool_condition_and_cross_condition_leak(self):
        row = {'init_observed': True, 'available_tools': sorted(runner.BUILTINS), 'mcp_servers_at_init': [],
               'tools': {'Read': 1}, 'model': 'pinned', 'observed_assistant_models': ['pinned']}
        self.assertEqual(runner.availability_errors(row, 'terminal', 'pinned'), [])
        row['available_tools'].append('mcp__kiro__diagnose')
        self.assertTrue(runner.availability_errors(row, 'terminal', 'pinned'))
        row['available_tools'] = sorted(runner.BUILTINS)
        row['tools']['diagnose'] = 1
        self.assertTrue(runner.availability_errors(row, 'terminal', 'pinned'))


class Isolation(unittest.TestCase):
    def test_tree_fingerprint_changes_on_bytes_mode_or_symlink(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory).resolve(); file = root/'source.rs'; file.write_text('a');file.chmod(0o644)
            first = runner.tree_hash(root);file.write_text('b')
            self.assertNotEqual(first, runner.tree_hash(root));file.write_text('a');file.chmod(0o600)
            self.assertNotEqual(first, runner.tree_hash(root));file.chmod(0o644)
            self.assertEqual(first, runner.tree_hash(root));(root/'link').symlink_to(file)
            self.assertNotEqual(first, runner.tree_hash(root))

    @unittest.skipUnless(Path('/usr/bin/sandbox-exec').is_file(), 'macOS sandbox-exec is required')
    def test_private_answers_denied_and_config_readable(self):
        with tempfile.TemporaryDirectory(prefix='kiro-diagnose-isolation-') as directory:
            base = Path(directory).resolve();project=base/'project';out=base/'out';private=base/'private';sibling=base/'sibling'
            for path in (project, out, private, sibling):path.mkdir()
            config=out/'mcp.json';config.write_text('{}');gold=private/'gold.json';gold.write_text('private')
            other=out/'other-answer.txt';other.write_text('private')
            source=sibling/'source.rs';source.write_text('source')
            profile=runner.sandbox_profile(project, config, out, [private, sibling])
            def run(args):return subprocess.run(['/usr/bin/sandbox-exec', '-p', profile, *args], capture_output=True).returncode
            self.assertEqual(run(['/bin/cat', str(config)]), 0)
            self.assertNotEqual(run(['/bin/cat', str(gold)]), 0)
            self.assertNotEqual(run(['/bin/cat', str(other)]), 0)
            self.assertNotEqual(run(['/bin/cat', str(source)]), 0)
            self.assertNotEqual(run(['/usr/bin/touch', str(project/'write')]), 0)
            self.assertNotEqual(run(['/usr/bin/touch', str(sibling/'blind-write')]), 0)
            self.assertNotEqual(run(['/usr/bin/touch', str(private/'blind-write')]), 0)
            self.assertNotEqual(run(['/usr/bin/touch', str(out/'blind-write')]), 0)
            with (out/'inherited-stdout.txt').open('wb') as stdout:
                result=subprocess.run(['/usr/bin/sandbox-exec', '-p', profile, '/bin/echo', 'inherited output'], stdout=stdout, stderr=subprocess.PIPE)
            self.assertEqual(result.returncode, 0)
            self.assertEqual((out/'inherited-stdout.txt').read_text(), 'inherited output\n')


if __name__ == '__main__':
    unittest.main()
