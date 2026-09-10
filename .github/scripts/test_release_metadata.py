import importlib.util
import pathlib
import tempfile
import unittest

spec = importlib.util.spec_from_file_location('metadata', pathlib.Path(__file__).with_name('release-metadata.py'))
metadata = importlib.util.module_from_spec(spec)
spec.loader.exec_module(metadata)


class ReleaseMetadataTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = pathlib.Path(self.temp.name)
        (self.root / 'Cargo.toml').write_text('[package]\nname="chop"\nversion="0.3.0"\n')
        (self.root / 'Cargo.lock').write_text('[[package]]\nname="chop"\nversion="0.3.0"\n')
        (self.root / 'CHANGELOG.md').write_text('# Changelog\n\n## Unreleased\n\n## 0.3.0 - 2026-09-10\n\n- New feature.\n\n## 0.2.0 - 2026-09-05\n\n- Older feature.\n')

    def test_extracts_only_requested_release(self):
        self.assertEqual(metadata.release_notes(self.root, '0.3.0'), '- New feature.\n')

    def test_rejects_invalid_versions(self):
        for version in ['v0.3.0', '0.3.0-beta.1', '0.03.0', '0.3.0\n', '$(echo bad)']:
            with self.subTest(version=version), self.assertRaises(ValueError):
                metadata.release_notes(self.root, version)

    def test_rejects_stale_lockfile(self):
        (self.root / 'Cargo.lock').write_text('[[package]]\nname="chop"\nversion="0.2.0"\n')
        with self.assertRaises(ValueError):
            metadata.release_notes(self.root, '0.3.0')

    def test_rejects_missing_empty_duplicate_or_invalid_dated_notes(self):
        for notes in ['', '## 0.3.0 - 2026-09-10\n', '## 0.3.0 - 2026-02-30\n- Feature\n', '## 0.3.0 - 2026-09-10\n- First\n## 0.3.0 - 2026-09-10\n- Second\n']:
            with self.subTest(notes=notes):
                (self.root / 'CHANGELOG.md').write_text(notes)
                with self.assertRaises(ValueError):
                    metadata.release_notes(self.root, '0.3.0')


if __name__ == '__main__':
    unittest.main()
