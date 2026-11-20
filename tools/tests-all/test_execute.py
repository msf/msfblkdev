import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

import execute


class RunnerTest(unittest.TestCase):
    def test_output_limit_and_timeout(self):
        """Stop the whole command if it prints too much or never finishes."""
        with tempfile.TemporaryDirectory() as directory:
            p = Path(directory)
            for code, marker in [('print("x"*2000)', 'log size'), ('import time; time.sleep(10)', 'Deadline')]:
                with patch.object(execute, 'LIMIT', 1000):
                    rc, text, why = execute.command(p, [sys.executable, '-c', code], p/'log', 1)
                self.assertIsNone(rc)
                self.assertLessEqual(len(text), 1000)
                self.assertIn(marker, why)

    def test_structured_python_outcomes(self):
        """Printed output and docstrings cannot substitute for an actual test verdict."""
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root/'tests').mkdir()
            (root/'tests/test_demo.py').write_text('''import unittest
class Case(unittest.TestCase):
    def test_pass(self):
        """Some custom description."""
        print("test_forge ... FAIL")
    def test_bad(self):
        with self.subTest(index=1): self.fail("planted failure")
    @unittest.skip("no device")
    def test_skip(self): pass
''')
            cases = [{'id': n, 'group': 'python:tests', 'selector': 'test_demo.Case.test_'+n}
                     for n in ('pass','bad','skip','missing')]
            results = execute.execute(root, cases, root/'out')
            self.assertEqual([r['status'] for r in results.values()],
                             ['passed','failed','not run','blocked'])

    def test_child_only_environment_is_removed(self):
        """An inherited crash fixture must not redirect the tests to someone else's file."""
        with tempfile.TemporaryDirectory() as directory, patch.dict(os.environ, {
            'BLOCK_STORAGE_CRASH_TEST_BACKING': '/not-our-file', 'TMPDIR': directory
        }):
            p = Path(directory)
            rc, text, _ = execute.command(p, [sys.executable, '-c',
                "import os; print(os.getenv('BLOCK_STORAGE_CRASH_TEST_BACKING')); print(os.getenv('TMPDIR'))"],
                p/'log', 5)
            self.assertEqual(rc, 0)
            self.assertEqual(text.splitlines(), ['None', directory])

    def test_rust_output(self):
        """Retain distinct passed, failed, ignored, and documentation results."""
        text = ('test one::test ... ok\ntest two::test ... FAILED\n'
                'test three ... ignored, requires a device\n'
                'test src/lib.rs - Writer (line 20) - compile fail ... ok\n')
        self.assertIn('src/lib.rs - Writer (line 20)', execute.parse_rust(text))
        self.assertEqual([v['status'] for v in execute.parse_rust(text).values()],
                         ['passed','failed','not run','passed'])

    def test_partial_matrix(self):
        """A missing repetition is not a passing scenario."""
        keys = ['rep-1','rep-2','rep-3']
        for passed, failed, expected in [(keys, [], 'passed'), (keys[:1], keys[1:2], 'failed'),
                                         (keys[:2], [], 'blocked')]:
            self.assertEqual(execute.repetitions(passed,failed,keys,'test')['status'], expected)

    def test_guest_requires_permission(self):
        """Do not spawn lab processes on a Linux host without explicit permission."""
        with tempfile.TemporaryDirectory() as d, patch.object(sys,'platform','linux'), patch.dict(os.environ,{'TESTS_ALL_LAB':'0'}), patch.object(subprocess,'Popen',side_effect=AssertionError('unexpected execution')):
            cases=[{'id': 'x','group': 'guest','selector': 'PROBE-01'}]
            results=execute.execute(Path(d),cases,Path(d)/'out')
            self.assertEqual(results['x']['status'],'blocked')
