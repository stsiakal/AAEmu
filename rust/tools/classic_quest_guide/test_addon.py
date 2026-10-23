"""Lua 5.1 regression tests for the real addon runtime.

The mock is strict: W_CTRL and X2Util exist only as documented globals, and
unknown widget methods raise. Tests exercise the real main.lua metadata,
OnLoad/OnUnload contract and visible/clickable controls.
"""

from __future__ import annotations

import json
import os
from pathlib import Path
import tempfile
import unittest

TOOL = Path(__file__).resolve().parent
sys_path = os.environ.get("AAEMU_LUA_TEST_DEPS")
if sys_path:
    import sys
    sys.path.insert(0, sys_path)
try:
    from lupa.lua51 import LuaRuntime
except Exception as error:  # pragma: no cover - missing optional dependency
    LuaRuntime = None
    LUPA_ERROR = error
else:
    LUPA_ERROR = None

import build  # noqa: E402
from test_build import extend_fixture, make_fixture  # noqa: E402


def lua_path(path: Path) -> str:
    return path.resolve().as_posix()


class LuaAddon:
    def __init__(self, lua, table):
        self.lua = lua
        self.table = table

    def call(self, method, *args):
        return self.table[method](self.table, *args)

    def mock_call(self, method, *args):
        mock = self.table["mock"]
        return mock[method](mock, *args)

    def text(self, widget_id):
        return self.mock_call("text", widget_id)

    def click(self, widget_id):
        return self.mock_call("click", widget_id)

    def exists(self, widget_id):
        return self.mock_call("exists", widget_id)

    def shown(self, widget_id):
        return self.mock_call("shown", widget_id)

    def set_text(self, widget_id, text):
        self.mock_call("set_text", widget_id, text)

    def fire(self, widget_id, action):
        mock = self.table["mock"]
        return mock["fire"](mock, widget_id, action)

    def game_event(self, name, args=None):
        return self.mock_call("event", name, self.lua.table_from(args or []))

    def update(self, ms=1000):
        self.mock_call("update", ms)

    def advance(self, ms=1000, count=1):
        self.mock_call("advance", ms, count)

    @property
    def mock(self):
        return self.table["mock"]

    @property
    def api(self):
        return self.table["api"]

    def file_paths(self):
        return list(self.mock["files"].keys())

    def file_payload(self, path):
        return self.mock["files"][path]

    def pending_count(self):
        return self.mock_call("pending_count")

    def file_count(self):
        return self.mock_call("file_count")

    def state(self):
        return self.table["active_state"](self.table)


def unit_table(lua, unit_id, name, x=0.0, y=0.0, z=0.0, buffs=None, info=None):
    id_literal = json.dumps(unit_id) if isinstance(unit_id, str) else str(unit_id)
    buffs = buffs or 0
    buff_literal = "{" + ",".join(
        "{id=%d,name=%s}" % (index, json.dumps("buff%d" % index))
        for index in range(1, buffs + 1)
    ) + "}"
    return lua.execute(
        "return {id=%s,name=%s,pos={x=%r,y=%r,z=%r},health=100,"
        "max_health=100,mana=50,max_mana=50,buffs=%s,debuffs={},info={nested={a=1}}}"
        % (id_literal, json.dumps(name), x, y, z, buff_literal)
    )


class AddonRuntimeTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        if LuaRuntime is None:
            raise unittest.SkipTest("lupa.lua51 unavailable: set AAEMU_LUA_TEST_DEPS")
        cls.temp = tempfile.TemporaryDirectory()
        root = Path(cls.temp.name)
        cls.db, cls.worlds = make_fixture(root)
        cls.output = root / "package"
        cls.manifest, _ = build.run_build(
            cls.db, cls.worlds, cls.output, code_commit="test-commit",
            data_only=True)
        cls.db_hash = cls.manifest["inputs"]["db"]["sha256"]

    @classmethod
    def tearDownClass(cls):
        cls.temp.cleanup()

    def setUp(self):
        self.lua = LuaRuntime(unpack_returned_tuples=True)
        tests = TOOL / "tests" / "lua"
        addon = TOOL / "addon"
        path = ";".join([
            lua_path(tests) + "/?.lua",
            lua_path(addon) + "/?.lua",
            lua_path(addon) + "/?/init.lua",
            lua_path(self.output) + "/?.lua",
            lua_path(self.output) + "/?/init.lua",
        ])
        self.lua.execute('package.path = "%s"' % path)
        self.harness = None

    def make_addon(self, opts=None, units=True, player_id=77, player_name="Player"):
        if units:
            self.lua.execute("AQG_UNIT_HELPERS = nil")
        main_path = TOOL / "addon" / "aaemu_quest_guide" / "main.lua"
        opts = opts or {}
        extra = ""
        if opts.get("no_wctrl"):
            extra += ",no_wctrl=true"
        if opts.get("no_x2"):
            extra += ",no_x2=true"
        table = self.lua.execute(
            'local H=require("harness"); AQG=H.new({main_path="%s"%s}); return AQG'
            % (lua_path(main_path), extra)
        )
        addon = LuaAddon(self.lua, table)
        mock = addon.mock
        if units:
            mock["units"]["player"] = unit_table(
                self.lua, player_id, player_name, 0.0, 0.0, 0.0)
            mock["units"]["target"] = unit_table(
                self.lua, 88, "Target", 10.0, 20.0, 30.0)
        mock["titles"][100] = "Classic Test Title"
        mock["bodies"][100] = "Classic body text for quest 100."
        mock["active_titles"][100] = "Classic Active Title"
        mock["completed"][100] = False
        addon.call("load")
        return addon

    def open_quest(self, addon, quest_id=100):
        addon.set_text("aqg_quest_id", str(quest_id))
        addon.click("aqg_open")

    def test_real_main_loads_visible_editable_clickable_controls(self):
        addon = self.make_addon()
        self.assertTrue(addon.exists("aqg_main"))
        self.assertTrue(addon.shown("aqg_main"))
        self.assertTrue(addon.exists("aqg_quest_id"))
        self.assertTrue(addon.shown("aqg_quest_id"))
        for button in ("aqg_open", "aqg_prev_quest", "aqg_next_quest",
                       "aqg_route", "aqg_rec_start", "aqg_nav_use",
                       "aqg_toggle_button"):
            self.assertTrue(addon.exists(button), button)
            self.assertTrue(addon.shown(button), button)
        self.assertIsNone(addon.api["W_CTRL"])
        self.assertIsNone(addon.api["X2Util"])
        self.assertIsNotNone(self.lua.execute("return W_CTRL"))
        self.assertIsNotNone(self.lua.execute("return X2Util"))
        self.assertGreater(addon.mock["skin_calls"], 0)

        self.open_quest(addon)
        content = addon.text("aqg_content")
        self.assertIn("Classic UI title (runtime): Classic Test Title", content)
        self.assertIn("Classic UI body (runtime): Classic body text", content)
        self.assertIn("Internal compact name:", content)
        self.assertIn("Concrete action: Accept at NPC", content)
        self.assertIn("template ids [500]", content)
        self.assertIn("x1", content)
        self.assertIn("Catalog status: ok", content)
        self.assertIn("Classic title: Classic Test Title", addon.text("aqg_header"))

        addon.click("aqg_hide")
        self.assertFalse(addon.shown("aqg_main"))
        self.assertTrue(addon.shown("aqg_toggle_button"))
        addon.click("aqg_toggle_button")
        self.assertTrue(addon.shown("aqg_main"))

        addon.click("aqg_page_req")
        self.assertIn("any one Start component", addon.text("aqg_content"))
        step_before = addon.text("aqg_stepline")
        self.assertTrue(addon.exists("aqg_next_text"))
        addon.click("aqg_next_text")
        addon.click("aqg_prev_text")
        self.assertEqual(step_before, addon.text("aqg_stepline"))

    def test_manual_advance_is_not_observed_completion(self):
        addon = self.make_addon()
        self.open_quest(addon)
        self.assertIn("not completed", addon.text("aqg_completion"))
        addon.click("aqg_next_step")
        self.assertIn("Manual ack: yes", addon.text("aqg_ack"))
        self.assertIn("not completion", addon.text("aqg_ack"))
        self.assertIn("not completed", addon.text("aqg_completion"))
        addon.mock["completed"][100] = True
        addon.advance(1000, 5)
        self.assertIn("completed (observed)", addon.text("aqg_completion"))
        addon.click("aqg_refresh_completion")
        self.assertIn("completed (observed)", addon.text("aqg_completion"))

    def test_navigation_opt_in_offscreen_and_capture(self):
        addon = self.make_addon()
        addon.set_text("aqg_server", "ClassicEU")
        addon.set_text("aqg_context", "test-context")
        addon.set_text("aqg_world", "main_world")
        self.open_quest(addon)
        self.assertFalse(addon.shown("aqg_waypoint"))
        self.assertIn("cleared", addon.text("aqg_navstatus").lower())
        addon.click("aqg_nav_use")
        self.assertTrue(addon.shown("aqg_waypoint"))
        self.assertIn("unverified 1.2 placement", addon.text("aqg_waypoint_label"))
        self.assertIn("unverified 1.2 placement", addon.text("aqg_banner"))

        addon.set_text("aqg_world", "")
        addon.fire("aqg_world", "OnTextChanged")
        addon.click("aqg_nav_use")
        self.assertFalse(addon.shown("aqg_waypoint"))
        self.assertIn("Enter and confirm", addon.text("aqg_navstatus"))

        addon.set_text("aqg_world", "wrong_world")
        addon.fire("aqg_world", "OnTextChanged")
        addon.click("aqg_nav_use")
        self.assertFalse(addon.shown("aqg_waypoint"))
        self.assertIn("World mismatch", addon.text("aqg_navstatus"))

        addon.set_text("aqg_world", "main_world")
        addon.fire("aqg_world", "OnTextChanged")
        addon.click("aqg_nav_use")
        addon.advance(1000)
        self.assertIn("unverified 1.2 placement", addon.text("aqg_waypoint_label"))
        addon.mock["projection"] = self.lua.eval("{sx=100,sy=100,dist=-1}")
        addon.advance(1000)
        self.assertIn("Turn to find waypoint", addon.text("aqg_waypoint_label"))
        self.assertNotIn("> ", addon.text("aqg_waypoint_label"))
        addon.mock["projection"] = self.lua.eval("{sx=5000,sy=5000,dist=10}")
        addon.advance(1000)
        self.assertIn(addon.text("aqg_waypoint_label")[0], "<>^v")

        addon.mock_call("remove_unit", "player")
        addon.advance(1000)
        self.assertFalse(addon.shown("aqg_waypoint"))
        self.assertIn("unavailable", addon.text("aqg_navstatus").lower())
        addon.mock["units"]["player"] = unit_table(
            self.lua, 77, "Player", 0.0, 0.0, 0.0)
        addon.click("aqg_nav_capture_player")
        self.assertTrue(addon.shown("aqg_waypoint"))
        self.assertIn("observed Classic waypoint", addon.text("aqg_waypoint_label"))

        # Character change clears navigation; it never completes by proximity.
        addon.mock["units"]["player"] = unit_table(
            self.lua, 99, "OtherPlayer", 0.0, 0.0, 0.0)
        addon.advance(2000)
        self.assertFalse(addon.shown("aqg_waypoint"))
        self.assertIn("no", addon.text("aqg_ack"))

    def test_missing_documented_globals_degrade_explicitly(self):
        no_edit = self.make_addon({"no_wctrl": True})
        self.assertTrue(no_edit.exists("aqg_main"))
        self.assertFalse(no_edit.exists("aqg_quest_id"))
        self.assertIsNone(self.lua.execute("return W_CTRL"))
        no_edit.click("aqg_open")
        self.assertIn("Numeric quest ID entry is unavailable",
                      no_edit.text("aqg_status"))

    def test_missing_x2util_degrades_navigation(self):
        addon = self.make_addon({"no_x2": True})
        self.assertTrue(addon.exists("aqg_quest_id"))
        self.assertIsNone(addon.api["X2Util"])
        self.assertIsNone(self.lua.execute("return X2Util"))
        addon.set_text("aqg_server", "srv")
        addon.set_text("aqg_context", "ctx")
        addon.set_text("aqg_world", "main_world")
        self.open_quest(addon)
        addon.click("aqg_nav_use")
        addon.advance(1000)
        self.assertFalse(addon.shown("aqg_waypoint"))
        self.assertIn("projection unavailable", addon.text("aqg_navstatus").lower())

    def test_opaque_string_identity_and_annotations(self):
        addon = self.make_addon(player_id="opaque-player-77", player_name="StringId")
        self.open_quest(addon)
        addon.click("aqg_rec_start")
        self.assertIn("Recording started", addon.text("aqg_recstatus"))
        addon.advance(1000)
        addon.set_text("aqg_note", "NPC absent at expected spot")
        addon.click("aqg_report_npc")
        addon.click("aqg_save")
        self.assertEqual(addon.file_count(), 1)
        path = addon.file_paths()[0]
        payload = addon.file_payload(path)
        self.assertEqual(payload["records"][1]["player"]["id"], "opaque-player-77")
        self.assertEqual(payload["session"]["catalog"]["database_sha256"], self.db_hash)
        self.assertEqual(payload["session"]["catalog"]["generator"]["code_commit"],
                         "test-commit")
        self.assertNotEqual(payload["session"]["catalog"]["worlds_sha256"], "unknown")
        annotation = payload["records"][2]
        self.assertTrue(annotation["user_annotation"])
        self.assertFalse(annotation["confirmed_event"])
        self.assertEqual(annotation["reason"], "Missing NPC")
        self.assertEqual(annotation["note"], "NPC absent at expected spot")
        self.assertTrue(annotation["context"]["user_declared"])
        self.assertFalse(annotation["context"]["api_current_world_known"])

    def test_write_failure_retention_retry_and_restart_refusal(self):
        addon = self.make_addon()
        self.open_quest(addon)
        addon.click("aqg_rec_start")
        addon.advance(1000)
        addon.mock_call("set_file_fail", True)
        addon.click("aqg_rec_stop")
        self.assertEqual(addon.pending_count(), 1)
        self.assertIn("retained", addon.text("aqg_recstatus").lower())
        addon.click("aqg_rec_start")
        self.assertIn("Cannot start a new session", addon.text("aqg_recstatus"))
        addon.mock_call("set_file_fail", False)
        addon.click("aqg_save")
        self.assertEqual(addon.pending_count(), 0)
        self.assertEqual(addon.file_count(), 1)
        addon.click("aqg_rec_start")
        self.assertIn("Recording started", addon.text("aqg_recstatus"))

    def test_identity_switch_without_update_tick_cancels_action(self):
        addon = self.make_addon()
        addon.set_text("aqg_server", "srv")
        addon.set_text("aqg_context", "ctx")
        self.open_quest(addon)
        addon.click("aqg_rec_start")
        addon.advance(1000)
        addon.mock["units"]["player"] = unit_table(
            self.lua, 900, "Switched", 1.0, 2.0, 3.0)
        addon.click("aqg_snap_target")
        self.assertEqual(addon.file_count(), 1)
        payload = addon.file_payload(addon.file_paths()[0])
        self.assertEqual(payload["session"]["player"]["name"], "Player")
        self.assertIn("Character/context changed", addon.text("aqg_status"))
        addon.click("aqg_rec_start")
        addon.advance(1000)
        addon.click("aqg_save")
        paths = addon.file_paths()
        self.assertEqual(len(paths), 2)
        self.assertNotEqual(paths[0], paths[1])

    def test_reload_retains_failed_pending_and_stale_unload_is_inert(self):
        addon = self.make_addon()
        self.open_quest(addon)
        addon.click("aqg_rec_start")
        addon.advance(1000)
        addon.mock_call("set_file_fail", "error")
        addon.click("aqg_rec_stop")
        self.assertEqual(addon.pending_count(), 1)
        old_metadata = addon.table["metadata"]
        addon.call("reload")
        self.assertIn("failed write", addon.text("aqg_recstatus").lower())
        addon.mock_call("set_file_fail", False)
        addon.click("aqg_save")
        self.assertEqual(addon.pending_count(), 0)
        self.assertEqual(addon.file_count(), 1)
        # A stale metadata object's OnUnload must not destroy the newer load.
        old_metadata["OnUnload"]()
        self.assertTrue(addon.exists("aqg_main"))
        self.assertTrue(addon.shown("aqg_main"))
        addon.click("aqg_page_info")
        self.assertIn("Expected data version", addon.text("aqg_content"))

    def test_resume_is_scoped_to_character_and_context(self):
        addon = self.make_addon()
        addon.set_text("aqg_server", "srv")
        addon.set_text("aqg_context", "ctx")
        self.open_quest(addon)
        addon.click("aqg_next_step")
        self.assertIn("Manual ack: yes", addon.text("aqg_ack"))
        addon.click("aqg_rec_start")
        addon.advance(1000)
        addon.click("aqg_rec_stop")
        addon.call("reload")
        self.assertIn("Manual ack: yes", addon.text("aqg_ack"))
        self.assertIn("Quest #100", addon.text("aqg_header"))
        # A different character must not resume this quest.
        addon.mock["units"]["player"] = unit_table(
            self.lua, 1234, "Different", 0.0, 0.0, 0.0)
        addon.call("reload")
        self.assertIn("no", addon.text("aqg_ack"))
        self.assertNotIn("Quest #100", addon.text("aqg_header"))

    def test_missing_buff_api_and_budgeted_snapshot(self):
        addon = self.make_addon()
        self.open_quest(addon)
        addon.api["Unit"]["UnitBuff"] = None
        deep = self.lua.eval(
            "{a={b={c={d={e={f={g='deep'}}}}}},big=string.rep('x', 5000)}")
        addon.mock["units"]["player"]["info"] = deep
        addon.mock["units"]["player"]["buffs"] = self.lua.eval(
            "{{id=1,name='b1'},{id=2,name='b2'}}")
        addon.click("aqg_rec_start")
        addon.advance(1000)
        addon.click("aqg_save")
        payload = addon.file_payload(addon.file_paths()[0])
        record = payload["records"][1]
        buffs = record["player_snapshot"]["buffs"]
        self.assertGreater(buffs["item_errors"], 0)
        self.assertFalse(buffs["items"][1]["ok"])
        unit_info = record["player_snapshot"]["unit_info"]["value"]
        self.assertEqual(unit_info["a"]["b"]["c"]["d"]["e"], "<max-depth>")
        self.assertLessEqual(len(unit_info["big"]), 201)

    def test_context_switch_without_update_tick_is_isolated(self):
        addon = self.make_addon()
        addon.set_text("aqg_server", "srv")
        addon.set_text("aqg_context", "ctx")
        self.open_quest(addon)
        addon.click("aqg_next_step")
        self.assertIn("Manual ack: yes", addon.text("aqg_ack"))
        addon.set_text("aqg_context", "different-context")
        addon.click("aqg_next_step")
        self.assertIn("context changed", addon.text("aqg_status").lower())
        self.assertIn("Manual ack: no", addon.text("aqg_ack"))
        self.assertNotIn("Quest #100", addon.text("aqg_header"))

    def test_catalog_chunk_cache_eviction_is_bounded(self):
        cache_root = Path(self.temp.name) / "cache-test"
        data = cache_root / "aaemu_quest_guide" / "data"
        data.mkdir(parents=True)
        (data / "index.lua").write_text(
            "return {data_version='1.2',chunk_size=1,chunk_count=8,quest_count=8,"
            "categories={},chunks={},quests={"
            + ",".join("{id=%d,chunk=%d}" % (i, i) for i in range(1, 9))
            + "}}", encoding="utf-8")
        (data / "meta.lua").write_text(
            "return {data_version='1.2',inputs={db={sha256='cache-test'}}}",
            encoding="utf-8")
        (data / "route.lua").write_text("return {quest_ids={}}", encoding="utf-8")
        for index in range(1, 9):
            (data / ("chunk_%04d.lua" % index)).write_text(
                "return {quests={[%d]={id=%d,steps={}}}}" % (index, index),
                encoding="utf-8")
        old_path = self.lua.execute("return package.path")
        self.lua.execute('package.path = "%s/?.lua;%s"' % (
            lua_path(cache_root), old_path))
        count = self.lua.execute(
            'local C=require("aaemu_quest_guide/catalog"); local c=C.new();'
            'for i=1,8 do c:quest(i) end;'
            'local n=0; for k in pairs(package.loaded) do '
            'if tostring(k):match("aaemu_quest_guide/data/chunk_") then n=n+1 end end;'
            'return n')
        self.assertLessEqual(count, 6)
        self.assertGreaterEqual(count, 1)

    def test_recording_off_makes_no_samples_or_writes(self):
        addon = self.make_addon()
        self.open_quest(addon)
        addon.advance(1000, 10)
        self.assertEqual(addon.file_count(), 0)
        self.assertEqual(addon.pending_count(), 0)
        self.assertIn("Recording off", addon.text("aqg_recstatus"))

    def test_checklist_without_reference_database(self):
        addon = self.make_addon()
        addon.click("aqg_checklist")
        before = addon.text("aqg_header")
        addon.click("aqg_cl_ref")
        self.assertIn("without --ref-db", addon.text("aqg_status"))
        self.assertEqual(before, addon.text("aqg_header"))


class ChecklistRuntimeTests(AddonRuntimeTests):
    @classmethod
    def setUpClass(cls):
        if LuaRuntime is None:
            raise unittest.SkipTest("lupa.lua51 unavailable: set AAEMU_LUA_TEST_DEPS")
        import sqlite3

        cls.temp = tempfile.TemporaryDirectory()
        root = Path(cls.temp.name)
        cls.db, cls.worlds = make_fixture(root)
        cls.extra_ids = extend_fixture(cls.db, 20)
        cls.ref = root / "reference.sqlite3"
        ref = sqlite3.connect(cls.ref)
        try:
            ref.execute("CREATE TABLE quest_contexts (id INTEGER PRIMARY KEY)")
            ref.executemany(
                "INSERT INTO quest_contexts VALUES (?)",
                [(100,), (200,), (201,), (230,), (240,), (241,)])
            ref.commit()
        finally:
            ref.close()
        cls.output = root / "package"
        cls.manifest, _ = build.run_build(
            cls.db, cls.worlds, cls.output, code_commit="test-commit",
            data_only=True, ref_db_path=cls.ref)
        cls.db_hash = cls.manifest["inputs"]["db"]["sha256"]

    def test_checklist_without_reference_database(self):
        self.skipTest("this fixture is built with a reference database")

    def row_index(self, addon, quest_id):
        rows = addon.state()["checklist"]["rows"]
        for index in range(1, len(rows) + 1):
            if rows[index]["id"] == quest_id:
                return index
        return None

    def row_text(self, addon, quest_id):
        index = self.row_index(addon, quest_id)
        if index is None:
            return None
        return addon.text("aqg_cl_row_%d" % index)

    def test_checklist_paging_filters_and_placeholders(self):
        addon = self.make_addon()
        addon.mock["active_titles"][100] = None
        addon.click("aqg_checklist")
        self.assertFalse(addon.shown("aqg_content"))
        self.assertTrue(addon.shown("aqg_cl_row_1"))
        first = addon.text("aqg_cl_row_1")
        self.assertIn("[ ] 100", first)
        self.assertIn("Classic Test Title", first)
        self.assertIn("NPC 500", first)
        self.assertIn("completed 0 / 16 visible", addon.text("aqg_header"))
        self.assertIn("25 matching", addon.text("aqg_header"))
        self.assertIn("Page 1/2 | checklist", addon.text("aqg_pageinfo"))
        self.assertFalse(addon.shown("aqg_cl_row_17"))

        addon.click("aqg_next_text")
        self.assertIn("Page 2/2 | checklist", addon.text("aqg_pageinfo"))
        self.assertEqual(addon.state()["checklist"]["page"], 2)
        addon.click("aqg_prev_text")
        self.assertEqual(addon.state()["checklist"]["page"], 1)

        addon.mock["completed"][100] = True
        addon.click("aqg_checklist")
        self.assertIn("24 matching", addon.text("aqg_header"))
        self.assertIsNone(self.row_text(addon, 100))

        addon.click("aqg_cl_done")
        self.assertIn("1 matching", addon.text("aqg_header"))
        self.assertIn("[x] 100", self.row_text(addon, 100))

        addon.click("aqg_cl_all")
        self.assertIn("25 matching", addon.text("aqg_header"))
        self.assertIn("[x] 100", self.row_text(addon, 100))

        addon.click("aqg_cl_placeholders")
        self.assertIn("27 matching", addon.text("aqg_header"))
        addon.set_text("aqg_filter", "dummy")
        addon.click("aqg_find")
        self.assertIn("dummy", addon.text("aqg_cl_row_1"))

        addon.set_text("aqg_filter", "Checklist Quest 205")
        addon.click("aqg_find")
        self.assertIn("1 matching", addon.text("aqg_header"))
        self.assertIn("Checklist Quest 205", addon.text("aqg_cl_row_1"))

    def test_checklist_completion_union_and_manual_marks(self):
        addon = self.make_addon()
        self.open_quest(addon, 102)
        addon.click("aqg_mark_done")
        addon.click("aqg_checklist")
        addon.click("aqg_cl_all")
        self.assertIn("[m] 102", self.row_text(addon, 102))

        addon.call("reload")
        addon.click("aqg_checklist")
        addon.click("aqg_cl_all")
        self.assertIn("[m] 102", self.row_text(addon, 102))

        self.open_quest(addon, 102)
        addon.click("aqg_mark_undo")
        addon.click("aqg_checklist")
        addon.click("aqg_cl_all")
        self.assertIn("[ ] 102", self.row_text(addon, 102))

        addon.game_event("CHAT_MSG_QUEST", [
            "Take this.", "Tester", "ticket", False, 1, 1, 1, 1, 103, False])
        addon.game_event("ADDED_ITEM", ["|i500,0,0,deadbeef;", 1, 35, ""])
        addon.click("aqg_checklist")
        addon.click("aqg_cl_all")
        self.assertIn("[x] 103", self.row_text(addon, 103))

        addon.call("reload")
        addon.click("aqg_checklist")
        addon.click("aqg_cl_all")
        self.assertIn("[x] 103", self.row_text(addon, 103))

        addon.mock["completed"][200] = True
        addon.click("aqg_refresh_completion")
        addon.click("aqg_cl_done")
        self.assertIn("[x] 200", self.row_text(addon, 200))

    def test_checklist_marks_are_scoped_to_character_and_context(self):
        addon = self.make_addon()
        self.open_quest(addon, 102)
        addon.click("aqg_mark_done")
        addon.mock["units"]["player"] = unit_table(
            self.lua, 4321, "OtherPlayer", 0.0, 0.0, 0.0)
        addon.advance(2000)
        addon.click("aqg_checklist")
        addon.click("aqg_cl_all")
        self.assertIn("[ ] 102", self.row_text(addon, 102))

        addon.mock["units"]["player"] = unit_table(
            self.lua, 77, "Player", 0.0, 0.0, 0.0)
        addon.advance(2000)
        addon.click("aqg_checklist")
        addon.click("aqg_cl_all")
        self.assertIn("[m] 102", self.row_text(addon, 102))

        addon.set_text("aqg_context", "different-context")
        addon.fire("aqg_context", "OnTextChanged")
        addon.click("aqg_checklist")
        addon.click("aqg_cl_all")
        self.assertIn("[ ] 102", self.row_text(addon, 102))

    def test_go_to_giver_position_and_no_position(self):
        addon = self.make_addon()
        addon.set_text("aqg_server", "ClassicEU")
        addon.set_text("aqg_context", "test-context")
        addon.set_text("aqg_world", "main_world")
        addon.fire("aqg_server", "OnTextChanged")
        addon.click("aqg_checklist")
        addon.click("aqg_cl_go_1")
        self.assertTrue(addon.shown("aqg_waypoint"))
        self.assertIn("unverified 1.2 placement",
                      addon.text("aqg_waypoint_label"))
        self.assertEqual(addon.state()["step_index"], 1)

        self.open_quest(addon, 230)
        addon.click("aqg_go_giver")
        self.assertFalse(addon.shown("aqg_waypoint"))
        self.assertIn("no 1.2 world position", addon.text("aqg_status"))
        self.assertIn("item", addon.text("aqg_status"))

        self.open_quest(addon, 101)
        addon.click("aqg_go_giver")
        self.assertIn("no accept step", addon.text("aqg_status"))

    def test_checklist_not_in_reference_filter(self):
        addon = self.make_addon()
        addon.click("aqg_checklist")
        self.assertIn("25 matching", addon.text("aqg_header"))
        addon.click("aqg_cl_ref")
        self.assertIn("4 matching", addon.text("aqg_header"))
        self.assertIn("not in 3.0 hidden", addon.text("aqg_stepline"))
        addon.click("aqg_cl_ref")
        self.assertIn("25 matching", addon.text("aqg_header"))


if __name__ == "__main__":
    unittest.main()
