"""Installable package tests: folder layout, deterministic ZIP, safety."""

from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path
import sys
import tempfile
import unittest
import zipfile

TOOL = Path(__file__).resolve().parent
sys_path = os.environ.get("AAEMU_LUA_TEST_DEPS")
if sys_path:
    sys.path.insert(0, sys_path)
try:
    from lupa.lua51 import LuaRuntime
except Exception:  # pragma: no cover - optional dependency
    LuaRuntime = None

import build  # noqa: E402
import package_addon  # noqa: E402
from test_build import make_fixture  # noqa: E402

VERSION = "0.2.0"


def file_bytes(root: Path) -> dict[str, bytes]:
    return {
        path.relative_to(root).as_posix(): path.read_bytes()
        for path in sorted(root.rglob("*")) if path.is_file()
    }


class PackageTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.temp = tempfile.TemporaryDirectory()
        cls.root = Path(cls.temp.name)
        cls.db, cls.worlds = make_fixture(cls.root)
        cls.db_hash = hashlib.sha256(cls.db.read_bytes()).hexdigest()
        cls.worlds_hash = {path.relative_to(cls.worlds).as_posix():
                           hashlib.sha256(path.read_bytes()).hexdigest()
                           for path in cls.worlds.rglob("*") if path.is_file()}
        cls.first = cls.root / "first"
        cls.second = cls.root / "second"
        cls.manifest, cls.summary = build.run_build(
            cls.db, cls.worlds, cls.first, code_commit="package-test")
        build.run_build(cls.db, cls.worlds, cls.second, code_commit="package-test")

    @classmethod
    def tearDownClass(cls):
        cls.temp.cleanup()

    def test_folder_and_zip_layout(self):
        folder = self.first / "aaemu_quest_guide"
        self.assertTrue((folder / "main.lua").is_file())
        self.assertTrue((folder / "README.md").is_file())
        self.assertTrue((folder / "observations" / "README.txt").is_file())
        self.assertTrue((folder / "data" / "index.lua").is_file())
        self.assertTrue((folder / "data" / "meta.lua").is_file())
        self.assertTrue((folder / "data" / "route.lua").is_file())
        self.assertTrue((folder / "data" / "chunk_0001.lua").is_file())
        zip_path = self.first / ("aaemu_quest_guide-%s.zip" % VERSION)
        self.assertTrue(zip_path.is_file())
        with zipfile.ZipFile(zip_path) as archive:
            names = archive.namelist()
        self.assertIn("aaemu_quest_guide/main.lua", names)
        self.assertIn("aaemu_quest_guide/README.md", names)
        self.assertIn("aaemu_quest_guide/observations/README.txt", names)
        self.assertIn("aaemu_quest_guide/data/index.lua", names)
        self.assertIn("aaemu_quest_guide/data/meta.lua", names)
        self.assertIn("aaemu_quest_guide/data/route.lua", names)
        self.assertIn("aaemu_quest_guide/data/chunk_0001.lua", names)
        self.assertFalse([name for name in names if name.endswith(".sqlite3")])
        self.assertFalse([name for name in names if name.endswith(".py")])
        self.assertFalse([name for name in names if name.endswith(".pyc")])
        self.assertFalse([name for name in names if ".." in Path(name).parts])
        self.assertFalse([name for name in names
                          if name.endswith("aaemu_quest_guide_generated")])
        for name in names:
            self.assertTrue(name.startswith("aaemu_quest_guide/"), name)
        package = self.manifest["package"]
        self.assertEqual(package["version"], VERSION)
        self.assertEqual(package["zip"], "aaemu_quest_guide-0.2.0.zip")
        self.assertEqual(self.summary["package"]["zip_sha256"],
                         package["zip_sha256"])

    def test_zip_is_deterministic_and_fixed_timestamps(self):
        first_zip = self.first / ("aaemu_quest_guide-%s.zip" % VERSION)
        second_zip = self.second / ("aaemu_quest_guide-%s.zip" % VERSION)
        self.assertEqual(first_zip.read_bytes(), second_zip.read_bytes())
        with zipfile.ZipFile(first_zip) as archive:
            for info in archive.infolist():
                self.assertEqual(info.date_time, (1980, 1, 1, 0, 0, 0), info.filename)
        first_folder = self.first / "aaemu_quest_guide"
        second_folder = self.second / "aaemu_quest_guide"
        self.assertEqual(file_bytes(first_folder), file_bytes(second_folder))

    def test_user_files_survive_rebuild_but_are_not_zipped(self):
        output = self.root / "preserve"
        build.run_build(self.db, self.worlds, output, code_commit="preserve")
        folder = output / "aaemu_quest_guide"
        zip_path = output / ("aaemu_quest_guide-%s.zip" % VERSION)
        before = zip_path.read_bytes()
        recording = folder / "observations" / "user-record.lua"
        recording.write_text("return {private='recording'}\n", encoding="utf-8")
        notes = folder / "private-notes.txt"
        notes.write_text("do not delete\n", encoding="utf-8")

        build.run_build(self.db, self.worlds, output, code_commit="preserve")
        self.assertEqual(recording.read_text(encoding="utf-8"),
                         "return {private='recording'}\n")
        self.assertEqual(notes.read_text(encoding="utf-8"), "do not delete\n")
        self.assertEqual(zip_path.read_bytes(), before)
        with zipfile.ZipFile(zip_path) as archive:
            names = archive.namelist()
        self.assertNotIn("aaemu_quest_guide/observations/user-record.lua", names)
        self.assertNotIn("aaemu_quest_guide/private-notes.txt", names)
        self.assertIn("aaemu_quest_guide/observations/README.txt", names)

    def test_standalone_validates_version_overlap_and_manifest_paths(self):
        with self.assertRaises(package_addon.PackageError):
            package_addon.package_addon(TOOL / "inside-source", TOOL)
        with self.assertRaises(package_addon.PackageError):
            package_addon.package_addon(self.first, TOOL, version="../evil")

        output = self.root / "manifest-path"
        build.run_build(self.db, self.worlds, output, code_commit="paths")
        manifest_path = output / "manifest.json"
        manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
        manifest["files"].append({
            "path": "aaemu_quest_guide/data/../../../outside.lua",
            "sha256": "0" * 64,
            "bytes": 1,
        })
        manifest_path.write_text(json.dumps(manifest), encoding="utf-8")
        (self.root / "outside.lua").write_text("return {}", encoding="utf-8")
        with self.assertRaises(package_addon.PackageError):
            package_addon.package_addon(output, TOOL)

    def test_data_only_leaves_no_runtime_or_zip(self):
        output = self.root / "data-only"
        build.run_build(self.db, self.worlds, output, code_commit="data-only",
                        data_only=True)
        self.assertTrue((output / "aaemu_quest_guide" / "data" / "index.lua").is_file())
        self.assertFalse((output / "aaemu_quest_guide" / "main.lua").exists())
        self.assertFalse((output / ("aaemu_quest_guide-%s.zip" % VERSION)).exists())
        self.assertNotIn("package", json.loads(
            (output / "manifest.json").read_text(encoding="utf-8")))

    def test_inputs_are_readonly_and_foreign_folder_refused(self):
        before_db = hashlib.sha256(self.db.read_bytes()).hexdigest()
        before_worlds = {path.relative_to(self.worlds).as_posix():
                         hashlib.sha256(path.read_bytes()).hexdigest()
                         for path in self.worlds.rglob("*") if path.is_file()}
        self.assertEqual(before_db, self.db_hash)
        self.assertEqual(before_worlds, self.worlds_hash)
        foreign = self.root / "foreign"
        (foreign / "aaemu_quest_guide").mkdir(parents=True)
        (foreign / "aaemu_quest_guide" / "keep.txt").write_text("keep",
                                                               encoding="utf-8")
        with self.assertRaises(build.BuildError):
            build.run_build(self.db, self.worlds, foreign,
                            code_commit="foreign", data_only=False)
        self.assertEqual((foreign / "aaemu_quest_guide" / "keep.txt").read_text(
            encoding="utf-8"), "keep")

    def test_every_packaged_lua_file_compiles(self):
        if LuaRuntime is None:
            raise unittest.SkipTest("lupa.lua51 unavailable: set AAEMU_LUA_TEST_DEPS")
        folder = self.first / "aaemu_quest_guide"
        runtime = LuaRuntime()
        files = sorted(folder.rglob("*.lua"))
        self.assertGreater(len(files), 10)
        for path in files:
            source = path.read_text(encoding="utf-8")
            loaded = runtime.eval("function(s) return loadstring(s) end")(source)
            self.assertIsNotNone(loaded, "Lua syntax error in %s" % path)


if __name__ == "__main__":
    unittest.main()
