#!/usr/bin/env python3
"""Cache installation contracts, with local release artifacts."""
import contextlib
import hashlib
import io
import json
import os
import subprocess
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch
import zipfile

import gamelibs


class CacheTests(unittest.TestCase):
    def setUp(self):
        self.scratch = tempfile.TemporaryDirectory(prefix='rock-gamelibs-test-')
        self.addCleanup(self.scratch.cleanup)
        self.root = Path(self.scratch.name)
        (self.root / 'gamelibs.version').write_text('test-version\n')
        buffer = io.BytesIO()
        with zipfile.ZipFile(buffer, 'w') as archive:
            archive.writestr('lib/library', b'release bytes')
            link = zipfile.ZipInfo('lib/link')
            link.create_system = 3
            link.external_attr = (0o120777 << 16)
            archive.writestr(link, b'library')
        self.archive = buffer.getvalue()
        self.digest = hashlib.sha256(self.archive).hexdigest()
        (self.root / 'gamelibs-artifacts.json').write_text(json.dumps({'gamelibs-linux-x86_64.zip': self.digest}))
        self.addCleanup(patch.stopall)
        patch.object(gamelibs, 'ROOT', self.root).start()
        patch.dict(os.environ, {'ROCK_GAMELIBS_DIR': str(self.root / 'cache')}).start()
        self.download = patch('urllib.request.urlopen', side_effect=lambda url, **kwargs: contextlib.closing(io.BytesIO(self.archive))).start()
        patch('platform.system', return_value='Linux').start()

    def test_install_is_atomic_idempotent_and_preserves_links(self):
        destination = self.root / 'cache/gamelibs-linux-x86_64'
        real_extract = gamelibs.extract
        def inspect(archive, staged):
            self.assertFalse(destination.exists())
            real_extract(archive, staged)
        with patch.object(gamelibs, 'extract', side_effect=inspect):
            gamelibs.sync(['linux-x86_64'])
        self.assertEqual((destination / 'lib/library').read_bytes(), b'release bytes')
        self.assertTrue((destination / 'lib/link').is_symlink())
        self.assertEqual(gamelibs.platform_dir('linux-x86_64'), destination)
        gamelibs.sync(['linux-x86_64'])
        self.assertEqual(self.download.call_count, 1)
        self.assertFalse(any(destination.parent.glob('.install-*')))

    def test_failed_install_exposes_no_partial_cache_and_cleans_staging(self):
        with patch.object(gamelibs, 'extract', side_effect=RuntimeError('intentional failure')):
            with self.assertRaisesRegex(RuntimeError, 'intentional failure'):
                gamelibs.sync(['linux-x86_64'])
        self.assertEqual(list((self.root / 'cache').iterdir()), [])
        gamelibs.sync(['linux-x86_64'])
        self.assertTrue(gamelibs.platform_dir('linux-x86_64').is_dir())

    def test_checksum_failure_does_not_publish_cache(self):
        self.archive += b'corrupted'
        with self.assertRaisesRegex(ValueError, 'checksum mismatch'):
            gamelibs.sync(['linux-x86_64'])
        self.assertEqual(list((self.root / 'cache').iterdir()), [])

    def test_missing_cache_names_fix_and_expected_path(self):
        with self.assertRaisesRegex(ValueError, 'cache/gamelibs-linux-x86_64; run `python3 scripts/gamelibs.py sync`'):
            gamelibs.platform_dir('linux-x86_64')


if __name__ == '__main__':
    unittest.main()
