"""Tests for package_probe.py (no Lua runtime needed)."""

from __future__ import annotations

from pathlib import Path
import tempfile
import unittest
import zipfile

import package_probe


class PackageTests(unittest.TestCase):
    def test_zip_layout_and_determinism(self):
        with tempfile.TemporaryDirectory() as temp:
            first = Path(temp) / "a" / "aaemu_probe.zip"
            second = Path(temp) / "b" / "aaemu_probe.zip"
            digest_a = package_probe.build_zip(first)
            digest_b = package_probe.build_zip(second)
            self.assertEqual(digest_a, digest_b)
            with zipfile.ZipFile(first) as archive:
                names = archive.namelist()
                self.assertIsNone(archive.testzip())
            for name in package_probe.RUNTIME_FILES:
                self.assertIn("aaemu_probe/" + name, names)
            self.assertTrue(all(n.startswith("aaemu_probe/") for n in names))
            self.assertFalse(any(n.endswith(".py") or "tests" in n for n in names))
            main = zipfile.ZipFile(first).read("aaemu_probe/main.lua")
            self.assertIn(b"OnLoad", main)
            self.assertNotIn(b"\r\n", main)


if __name__ == "__main__":
    unittest.main()
