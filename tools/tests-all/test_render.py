import unittest
from html.parser import HTMLParser

from render import render


def case(name, suite='Disk recovery'):
    return dict(id=name, title=f'Can we read {name}?', suite=suite, setup='Write a known block.',
                action='Close and reopen the file.', expected='Read back the original bytes.')


def run(results, date='2030-01-01T00:00:00Z'):
    return dict(revision='a' * 40, date=date, environment='Linux', results=results)


def result(status, detail='Example observation'):
    return dict(status=status, detail=detail)


class Document(HTMLParser):
    def __init__(self, data):
        super().__init__()
        self.ids = []
        self.links = []
        self.tags = []
        self.feed(data)
        self.close()

    def handle_starttag(self, tag, attrs):
        attrs = dict(attrs)
        self.tags.append(tag)
        if 'id' in attrs:
            self.ids.append(attrs['id'])
        if 'href' in attrs:
            self.links.append(attrs['href'])


class ReportTest(unittest.TestCase):
    def test_counts_use_only_latest_run_and_include_missing_cases(self):
        cases = [case(i) for i in 'abcde']
        page = render(cases, [run({'a': result('passed')}), run({
            'b': result('passed'), 'c': result('failed'), 'd': result('blocked'),
            'e': result('not run')}, date='2030-01-02T00:00:00Z')])
        self.assertIn('1 of 5 listed tests passed.', page)
        self.assertIn('1/2 (50.0%)', page)
        self.assertIn('<td>5</td><td>1</td><td>1</td><td>1</td><td>2</td>', page)
        self.assertEqual(page.count('<td>No record</td>'), 4)
        self.assertLess(page.index('#1 · <code>'), page.index('#2 · <code>'))
        self.assertIn('Latest: no individual result was recorded.', page)

    def test_page_links_are_local_and_evidence_is_text_not_markup(self):
        c = case('a/?"<img src=x>')
        c['explanation_html'] = '<p>Region zero must remain unavailable.</p>'
        old = run({c['id']: result('passed')})
        new = run({c['id']: result('failed', '<script>broken()</script>')})
        page = render([c], [old, new])
        doc = Document(page)
        self.assertEqual(len(set(doc.ids)), len(doc.ids))
        self.assertTrue(all(h.startswith('#') and h[1:] in doc.ids for h in doc.links))
        self.assertNotIn('script', doc.tags)
        self.assertNotIn('img', doc.tags)
        self.assertIn('&lt;script&gt;broken()&lt;/script&gt;', page)
        self.assertIn(c['explanation_html'], page)

    def test_zero_decided_tests_have_no_rate(self):
        self.assertIn('no verdicts', render([case('a')], [run({'a': result('blocked')})]))
        page = render([], [])
        self.assertIn('Inventory only', page)
        self.assertIn('0/0 (no verdicts)', page)

    def test_unknown_status_cannot_render_as_a_verdict(self):
        with self.assertRaisesRegex(ValueError, 'unknown test status'):
            render([case('a')], [run({'a': result('looks fine')})])


if __name__ == '__main__':
    unittest.main()
