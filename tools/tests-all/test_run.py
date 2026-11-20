import json
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

import run


class ReportPipeline(unittest.TestCase):
    def test_runner_failure_replaces_stale_report_with_blocked_observation(self):
        """Start with an old green report. Break the runner. Publish a blocked result and exit unsuccessfully."""
        case = dict(id='demo', title='A known test', suite='Example', setup='', action='', expected='')
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory)
            (output / 'report.html').write_text('old green report')
            with patch.object(run, 'git', side_effect=['a' * 40, '']), \
                 patch.object(run, 'inventory', return_value=[case]), \
                 patch.object(run, 'execute', side_effect=RuntimeError('missing test program')):
                self.assertEqual(run.run(output), 1)
            result, = output.glob('runs/*/result.json')
            self.assertEqual(json.loads(result.read_text())['results']['demo']['status'], 'blocked')
            self.assertIn('missing test program', (output / 'report.html').read_text())

    def test_appendix_alias_keeps_explanation_without_repository_link(self):
        """Select a test with an alternate name. Embed its existing explanation without depending on another file."""
        case = {'adr05_anchor': 'ex-alias'}
        run.attach_explanations([case], '<section id="ex-primary"><h4 id="ex-alias">Why?</h4>'
                                '<p>Known bytes. <a href="log.txt">Raw log</a></p></section>')
        self.assertIn('Known bytes.', case['explanation_html'])
        self.assertNotIn('href=', case['explanation_html'])
        self.assertNotIn(' id=', case['explanation_html'])
