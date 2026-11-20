"""Regression checks on inventory membership, not on whether any suite passed."""
from pathlib import Path
from tempfile import TemporaryDirectory
import unittest

from inventory import inventory, python_cases, rust_cases


class InventoryTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.rows = inventory(Path(__file__).resolve().parents[2])

    def test_cases_have_unique_ids_and_explanations(self):
        ids = [r['id'] for r in self.rows]
        self.assertEqual(ids, sorted(set(ids)))
        for r in self.rows:
            for field in ('suite','title','setup','action','expected','group','selector'):
                self.assertTrue(r[field], (r['id'],field))

    def test_profiles_count_once_but_distinct_filesystems_do_not_merge(self):
        for group in ('sqlite:xfs', 'sqlite:ext4'):
            selected = {r['selector'] for r in self.rows if r['group']==group}
            self.assertEqual(len(selected), 29)
            self.assertIn('speedtest-main/delete-extra', selected)
            self.assertNotIn('kv-long-wal-checkpoint-stress/delete-extra', selected)
        self.assertEqual(sum(r['group']=='make:test-ublk-fio' for r in self.rows), 8)
        self.assertEqual({r['selector'] for r in self.rows if r['group']=='seed'}, {'sweep','replay'})

    def test_shared_modules_and_documentation_keep_their_cargo_target(self):
        shared = [r for r in self.rows if r.get('source_key','').startswith('rust/src/bin/fs_support/mod.rs::')]
        self.assertTrue(shared)
        for key in {r['source_key'] for r in shared}:
            self.assertEqual({r['group'] for r in shared if r['source_key']==key},
                             {'rust:rust:--bin=block-storage-mount','rust:rust:--bin=block-storage-lab'})
        rows = [r for r in self.rows if r['id']=='rust:rust:--doc/io_accounting::PendingOperation']
        self.assertEqual(len(rows),1)
        self.assertRegex(rows[0]['selector'],r'^src/io_accounting.rs - io_accounting::PendingOperation \(line \d+\)$')

    def test_new_functions_are_inventoried_before_any_run(self):
        with TemporaryDirectory() as tmp:
            root = Path(tmp)
            files = {Path('Cargo.toml'),Path('src/lib.rs'),Path('test_bird.py')}
            (root/'src').mkdir()
            (root/'Cargo.toml').write_text('[package]\nname="bird"\n')
            (root/'src/lib.rs').write_text('mod unit {\n    #[test]\n    fn bird() {}\n}\n')
            (root/'test_bird.py').write_text('import unittest\nclass Bird(unittest.TestCase):\n def test_bird(self): pass\n')
            self.assertEqual([r['selector'] for r in rust_cases(root,files)],['unit::bird'])
            self.assertEqual([r['selector'] for r in python_cases(root,files)],['test_bird.Bird.test_bird'])
            self.assertFalse((root/'target').exists())

    def test_unattached_rust_test_is_not_silently_lost(self):
        with TemporaryDirectory() as tmp:
            root=Path(tmp)
            path=Path('orphan.rs')
            (root/path).write_text('#[test]\nfn bird() {}\n')
            with self.assertRaisesRegex(ValueError, 'unrecognized Rust test layout'):
                list(rust_cases(root,{path}))
