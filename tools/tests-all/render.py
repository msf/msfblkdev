"""Render the repository test inventory and its observations as one offline page."""

from base64 import urlsafe_b64encode
from collections import Counter, defaultdict
from html import escape


STATES = ("passed", "failed", "blocked", "not run")
MISSING = {"status": "not run", "detail": "No individual result was recorded."}
STYLE = """
:root{color-scheme:light;--paper:#f7f7f1;--ink:#162d2b;--muted:#516764;--line:#c5d1c7;--green:#00634c;--night:#102d2a}
*{box-sizing:border-box}body{margin:0;background:var(--paper);color:var(--ink);font:1rem/1.55 system-ui,sans-serif}
header,main,footer{max-width:74rem;margin:auto;padding:1.25rem clamp(1rem,4vw,3rem)}header{border-bottom:1px solid var(--line)}
nav,.badges{display:flex;flex-wrap:wrap;gap:.7em 1.25em}h1,h2,h3{line-height:1.2;text-wrap:balance}h1{font-size:clamp(2.2rem,5vw,3.8rem)}
p,dl{max-width:78ch}h2{margin-top:2.4rem}a{color:var(--green);text-underline-offset:.16em}a:hover,summary:hover{text-decoration:underline}
a:focus-visible,summary:focus-visible,[tabindex]:focus-visible{outline:3px solid #825300;outline-offset:3px}
.kicker,caption{color:var(--muted);font-weight:650}.lead{font-size:1.4em}.scroll{overflow:auto;background:white;border:1px solid var(--line);max-height:72vh}
table{border-collapse:separate;border-spacing:0;min-width:100%;font-variant-numeric:tabular-nums}caption{text-align:left;padding:.6em}
th,td{padding:.55em .85em;text-align:left;vertical-align:top;border-bottom:1px solid var(--line)}thead th{color:white;background:var(--night);position:sticky;top:0}
th[scope=row]{min-width:min(17rem,45vw);font-weight:550}td{white-space:nowrap}details{border-top:1px solid var(--line);padding:.6em 0}
summary{cursor:pointer;font-weight:650}.badge{display:inline-block;background:#e4e8e4;padding:.12em .55em;border-radius:.3em;color:var(--ink)}
.passed{background:#d6efdc;color:#13442c}.failed{background:#f9d9d5;color:#79251a}.blocked{background:#ffe2b8;color:#684100}
article{max-width:78ch;border-top:1px solid var(--line);padding-block:.5em}article:target,:target>summary{outline:2px solid var(--green);outline-offset:2px}
code,pre{font-family:ui-monospace,monospace;overflow-wrap:anywhere}pre{white-space:pre-wrap;background:white;border-left:3px solid var(--line);padding:.7em}
dt{font-weight:650}dd{margin:0 0 .8em}small{color:var(--muted)}[id]{scroll-margin:1rem}article,summary{overflow-wrap:anywhere}
"""


def anchor(kind, name):
    """Stable even if a new test changes sort order. No raw user input in IDs."""
    key = urlsafe_b64encode(name.encode()).decode().rstrip("=")
    return kind + "-" + key


def badge(status):
    if status not in STATES:
        raise ValueError(f"unknown test status: {status!r}")
    return f'<span class="badge {status.replace(" ", "-")}">{status.capitalize()}</span>'


def revision(run):
    return f'<code>{escape(run["revision"])}</code>'


def summary(cases, grouped, latest):
    counts = {suite: Counter(latest.get(c['id'], MISSING)['status'] for c in members)
              for suite, members in grouped.items()}
    all_counts = Counter()
    for count in counts.values():
        all_counts.update(count)
    passed = all_counts['passed']
    decided = passed + all_counts['failed']
    rate = f'{100 * passed / decided:.1f}%' if decided else 'no verdicts'
    rows = []
    for suite, members in grouped.items():
        cells = ''.join(f'<td>{counts[suite][s]}</td>' for s in STATES)
        rows.append(f'<tr><th scope="row"><a href="#{anchor("suite", suite)}">'
                    f'{escape(suite)}</a></th><td>{len(members)}</td>{cells}</tr>')
    return ('<h2 id="overview">Latest results by suite</h2>'
            f'<p class="lead"><b>{passed} of {len(cases)} listed tests passed.</b></p>'
            '<p class="badges">' + ' '.join(f'{badge(s)} {all_counts[s]}' for s in STATES) + '</p>'
            f'<p>Pass rate among tests with a verdict: <b>{passed}/{decided} ({rate})</b>. '
            'Blocked means a prerequisite or the runner prevented a verdict. Not run means '
            'there is no result for this test in this run. Neither counts as a pass.</p>'
            '<div class="scroll" role="region" aria-label="Latest suite totals" tabindex="0"><table>'
            '<caption>Choose a suite to find its tests.</caption><thead><tr>'
            + ''.join(f'<th scope="col">{s}</th>' for s in ['Suite', 'Listed', *(s.capitalize() for s in STATES)])
            + '</tr></thead><tbody>' + ''.join(rows) + '</tbody></table></div>')


def history(suite, members, runs):
    headers = ''.join(f'<th scope="col">#{i+1} · <code>{escape(r["revision"][:12])}</code></th>'
                      for i, r in enumerate(runs))
    rows = []
    for case in members:
        key = anchor('test', case['id'])
        cells = []
        for i, run in enumerate(runs):
            result = run['results'].get(case['id'])
            if result is None and i + 1 < len(runs):
                cells.append('<td>No record</td>')
            else:
                status = (result or MISSING)['status']
                target = f'{key}-run-{i+1}' if result is not None else key
                cells.append(f'<td><a href="#{target}">{badge(status)}</a></td>')
        rows.append(f'<tr><th scope="row"><a href="#{key}">{escape(case["title"])}</a></th>'
                    + ''.join(cells) + '</tr>')
    return (f'<details open id="{anchor("suite", suite)}"><summary>{escape(suite)} · {len(members)} tests</summary>'
            f'<div class="scroll" tabindex="0" role="region" aria-label="{escape(suite)} timeline"><table>'
            f'<caption>{escape(suite)} — select a name for its explanation or a verdict for its observation.</caption>'
            f'<thead><tr><th scope="col">Test</th>{headers}</tr></thead><tbody>' + ''.join(rows)
            + '</tbody></table></div></details>')


def explanation(case, runs):
    key = anchor('test', case['id'])
    description = ''.join(f'<dt>{label}</dt><dd>{escape(case[field])}</dd>' for label, field in
                          [('Setup', 'setup'), ('Action or fault', 'action'), ('Expected outcome', 'expected')])
    # Only trusted, checked-in report prose may populate this field. Never pass log HTML.
    extra = case.get('explanation_html', '')
    if extra:
        extra = ('<details><summary>Fault scenario and earlier evidence</summary>'
                 '<p>These notes describe the original fault investigation. '
                 'Only the dated observations below give a verdict for this test run.</p>' + extra + '</details>')
    observed = []
    for i, run in enumerate(runs):
        result = run['results'].get(case['id'])
        if result is not None:
            observed.append(f'<details><summary>#{i+1} · '
                            f'{escape(run["date"])} · {badge(result["status"])}</summary>'
                            f'<pre id="{key}-run-{i+1}">{escape(result["detail"])}</pre></details>')
    if not runs or case['id'] not in runs[-1]['results']:
        observed.append('<p>Latest: no individual result was recorded.</p>')
    return (f'<article id="{key}"><h4>{escape(case["title"])}</h4><dl>{description}</dl>{extra}'
            '<p><b>Recorded observations</b></p>' + ''.join(observed) +
            f'<small>Test ID: <code>{escape(case["id"])}</code> · '
            f'<a href="#{anchor("suite", case["suite"])}">Back to suite</a></small></article>')


def render(cases: list[dict], runs: list[dict]) -> str:
    """Use the current inventory and chronological runs; do not carry results forward."""
    grouped = defaultdict(list)
    for case in sorted(cases, key=lambda c: (c['suite'], c['title'], c['id'])):
        grouped[case['suite']].append(case)
    latest = runs[-1]['results'] if runs else {}
    parts = ['<!doctype html><html lang="en"><head><meta charset="utf-8">',
             '<meta name="viewport" content="width=device-width, initial-scale=1">',
             '<meta http-equiv="Content-Security-Policy" content="default-src \'none\'; style-src \'unsafe-inline\'">',
             f'<title>Repository test report | msfblkdev</title><style>{STYLE}</style></head><body>',
             '<header><strong>msfblkdev / field notes</strong><nav aria-label="Report sections">',
             '<a href="#overview">Latest results</a><a href="#history">Test history</a>',
             '<a href="#guide">Test explanations</a></nav></header><main>',
             '<h1>Repository test report</h1><p>This project presents a disk to an application '
             'and stores its data in a backing file. These tests check the engine, the applications '
             'that use it, and the tools used to verify the results. A passing test can mean a checker detected a deliberately wrong answer. Some test-program entries are child-process helpers; they are listed for inventory completeness, not as separate storage guarantees.</p>']
    if runs:
        run = runs[-1]
        parts.append(f'<p>Latest run: {revision(run)} · {escape(run["date"])} · {escape(run["environment"])}</p>')
    else:
        parts.append('<p><strong>Inventory only.</strong> No run has been recorded.</p>')
    parts.extend([summary(cases, grouped, latest), '<h2 id="history">What ran at each revision?</h2>',
                  '<p>Each row is one test in the current inventory. Each column is one run, oldest first. '
                  'Two runs of the same revision can use different environments. No record means the '
                  'earlier run has no entry for that test; it does not imply a pass or failure.</p>', '<ol>'])
    parts.extend(f'<li>{revision(r)} · {escape(r["date"])} · {escape(r["environment"])}</li>' for r in runs)
    parts.append('</ol>')
    parts.extend(history(s, m, runs) for s, m in grouped.items())
    parts.append('<h2 id="guide">Test explanations</h2>')
    for suite, members in grouped.items():
        parts.append(f'<section><h3>{escape(suite)}</h3>')
        parts.extend(explanation(c, runs) for c in members)
        parts.append('</section>')
    parts.append('</main><footer><a href="#overview">Back to latest results</a></footer></body></html>')
    return '\n'.join(parts)
