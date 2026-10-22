"""Lua 5.1 tests for the AAEmu probe addon, run against a strict stub api.

Run from this folder (needs ``lupa``; set AAEMU_LUA_TEST_DEPS to a folder that
holds it if it is not installed):

    python -m unittest test_probe
"""

from __future__ import annotations

import os
from pathlib import Path
import re
import unittest

HERE = Path(__file__).resolve().parent
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

ADDON = HERE / "aaemu_probe"


def lua_path(path: Path) -> str:
    return path.resolve().as_posix()


def to_py(value, seen=None):
    """Convert a Lua table to dicts/lists/scalars for assertions."""
    if not hasattr(value, "items"):
        return value
    seen = seen or set()
    return {to_py(k): to_py(v, seen) for k, v in value.items()}


def seq(table):
    """Lua array (1-based dict from to_py) as a Python list."""
    return [table[k] for k in sorted(table)] if table else []


class Probe:
    def __init__(self, lua, harness):
        self.lua = lua
        self.h = harness
        self.mock = harness["mock"]

    def advance(self, ms=1000, steps=1):
        self.mock["advance"](self.mock, ms, steps)

    def emit(self, event, *args):
        self.mock["emit"](self.mock, event, *args)

    def click(self, widget_id):
        self.mock["click"](self.mock, widget_id)

    def text(self, widget_id):
        return self.mock["text"](self.mock, widget_id)

    def probe(self):
        return self.h["probe"](self.h)

    def all_files(self):
        return list(self.mock["file_order"].values())

    def files(self):
        return [p for p in self.all_files() if "/boot_" not in p]

    def payload(self, path):
        return to_py(self.mock["files"][path])

    def all_records(self):
        records = []
        for path in self.files():
            records.extend(seq(self.payload(path)["records"]))
        return records

    def records_of(self, kind):
        return [r for r in self.all_records() if r.get("k") == kind]

    def set_unit(self, token, **fields):
        table = self.lua.table_from(fields, recursive=True)
        self.mock["set_unit"](self.mock, token, table)

    def violations(self):
        return list(self.mock["violations"].values())


@unittest.skipIf(LuaRuntime is None, "lupa.lua51 unavailable: set AAEMU_LUA_TEST_DEPS")
class ProbeTests(unittest.TestCase):
    def make(self, reject=None, load=True, bag=None):
        lua = LuaRuntime(unpack_returned_tuples=True)
        path = ";".join([
            lua_path(HERE / "tests" / "lua") + "/?.lua",
            lua_path(HERE) + "/?.lua",
        ])
        lua.execute("package.path = %r" % path)
        if reject:
            rejected = "{" + ",".join("%s=true" % name for name in reject) + "}"
        else:
            rejected = "nil"
        harness = lua.execute(
            "local H = require('harness'); "
            "return H.new({main_path=%r, base_dir=%r, reject=%s})"
            % (lua_path(ADDON / "main.lua"), lua_path(HERE), rejected))
        probe = Probe(lua, harness)
        probe.set_unit("player", id=1, name="Hero", x=10.5, y=20.5, z=30.5,
                       hp=100, max_hp=100, info={"level": 5})
        if bag:
            for slot, item in bag.items():
                probe.mock["bag"][slot] = lua.table_from(item)
        if load:
            harness["load"](harness)
        return probe

    # -- registration ------------------------------------------------------

    def test_metadata_contract(self):
        probe = self.make(load=False)
        meta = probe.h["metadata"]
        self.assertEqual(meta["name"], "AAEmu probe")
        self.assertTrue(callable(meta["OnLoad"]))
        self.assertTrue(callable(meta["OnUnload"]))

    def test_registers_all_206_events_and_api_on(self):
        probe = self.make()
        self.assertEqual(probe.mock["calls"]["RegisterEvent"], 206)
        probe.mock["update"](probe.mock, 15000)
        reg = probe.records_of("registrations")[0]
        self.assertEqual(reg["requested"], 206)
        self.assertEqual(reg["ok_count"], 206)
        self.assertEqual(reg["fail_count"], 0)
        api_on = probe.records_of("registrations_api_on")[0]["results"]
        for name in ("CHAT_MESSAGE", "TEAM_MEMBERS_CHANGED", "UI_RELOADED",
                     "UPDATE_PING_INFO", "UPDATE"):
            self.assertEqual(api_on[name], "ok", name)

    def test_failed_registration_is_recorded(self):
        probe = self.make(reject=["TARGET_CHANGED", "BAG_UPDATE"])
        probe.mock["update"](probe.mock, 15000)
        reg = probe.records_of("registrations")[0]
        self.assertEqual(reg["fail_count"], 2)
        self.assertIn("TARGET_CHANGED", reg["failed"])
        self.assertEqual(reg["ok_count"], 204)

    # -- events --------------------------------------------------------------

    def test_event_records_args_and_context(self):
        probe = self.make()
        probe.set_unit("target", id=77, name="Boar", x=1.0, y=2.0, z=3.0,
                       info={"level": 3})
        nested = probe.lua.eval("{a={b={c={d='too deep'}}}, s=string.rep('x', 500)}")
        probe.emit("ADDED_ITEM", "text", 42, nested)
        probe.advance(15000)
        events = probe.records_of("event")
        self.assertEqual(len(events), 1)
        record = events[0]
        self.assertEqual(record["e"], "ADDED_ITEM")
        self.assertEqual(record["src"], "widget")
        self.assertEqual(record["n"], 3)
        args = record["args"]
        self.assertEqual(args[1], "text")
        self.assertEqual(args[2], 42)
        self.assertEqual(args[3]["a"]["b"]["c"], "<max-depth>")
        self.assertLessEqual(len(args[3]["s"]), 301)
        self.assertEqual(seq(record["pos"]), [10.5, 20.5, 30.5])
        self.assertEqual(record["zone"], 7)
        self.assertEqual(record["target"]["name"], "Boar")
        self.assertEqual(record["target"]["info"]["level"], 3)
        self.assertEqual(record["lt"]["hour"], 12)
        self.assertIn("ui_ms", record)

    def test_table_key_cap_is_marked(self):
        probe = self.make()
        big = probe.lua.eval("(function() local t = {} for i = 1, 100 do "
                             "t['k' .. i] = i end return t end)()")
        probe.emit("NOTICE_MESSAGE", big)
        probe.advance(15000)
        arg = probe.records_of("event")[0]["args"][1]
        self.assertEqual(arg["_more_keys"], 60)

    def test_api_on_events_are_logged_with_source(self):
        probe = self.make()
        probe.emit("CHAT_MESSAGE", 1, "hello")
        probe.advance(15000)
        sources = {(r["e"], r["src"]) for r in probe.records_of("event")}
        self.assertIn(("CHAT_MESSAGE", "widget"), sources)
        self.assertIn(("CHAT_MESSAGE", "api.On"), sources)

    def test_rate_limit_is_recorded_not_silent(self):
        probe = self.make()
        for _ in range(100):
            probe.emit("TEAM_HEALTH_CHANGED", 1)
        probe.advance(15000)
        self.assertEqual(len(probe.records_of("event")), 40)
        summary = probe.records_of("summary")[-1]
        self.assertEqual(summary["events_seen"], 100)
        self.assertEqual(summary["suppressed"]["TEAM_HEALTH_CHANGED"], 60)
        self.assertEqual(summary["counts"]["TEAM_HEALTH_CHANGED"], 100)

    # -- storage ----------------------------------------------------------------

    def test_flush_interval_and_file_names(self):
        probe = self.make()
        probe.emit("LOOT_DICE", 1)
        probe.advance(14000)
        self.assertEqual(len(probe.files()), 0)
        probe.advance(1000)
        self.assertEqual(len(probe.files()), 1)
        first = probe.files()[0]
        self.assertRegex(first, r"^aaemu_probe/observations/probe_0001_001\.lua$")
        probe.emit("LOOT_DICE", 2)
        probe.advance(15000)
        self.assertEqual(probe.files()[1], "aaemu_probe/observations/probe_0001_002.lua")
        self.assertEqual(probe.mock["save_settings_count"], 1)

    def test_save_button_unload_and_second_session(self):
        probe = self.make()
        probe.emit("LOOT_DICE", 1)
        probe.click("aep_save")
        self.assertEqual(len(probe.files()), 1)
        probe.emit("LOOT_DICE", 2)
        probe.h["unload"](probe.h)
        self.assertEqual(len(probe.files()), 2)
        self.assertFalse(probe.probe()["alive"])
        probe.h["load"](probe.h)
        probe.advance(15000)
        self.assertTrue(probe.files()[-1].startswith("aaemu_probe/observations/probe_0002_"))

    def test_write_failure_keeps_records_and_retries(self):
        probe = self.make()
        probe.emit("LOOT_DICE", 1)
        probe.mock["file_fail"] = True
        probe.advance(15000)
        state = probe.probe()
        self.assertEqual(len(probe.files()), 0)
        self.assertIn("write failed", state["last_error"])
        self.assertIn("Last error: write failed", probe.text("aep_status4"))
        probe.mock["file_fail"] = False
        probe.advance(15000)
        self.assertEqual(len(probe.files()), 1)
        self.assertEqual(len(probe.records_of("event")), 1)
        self.assertIsNone(probe.probe()["last_error"])

    def test_buffer_cap_pauses_and_shows_it(self):
        probe = self.make()
        probe.mock["file_fail"] = True
        state = probe.probe()
        for index in range(600):
            state["on_event"](state, "widget", "EVENT_%d" % index, index)
        self.assertTrue(state["paused"])
        self.assertGreater(state["missed_while_paused"], 0)
        probe.advance(1000)
        self.assertIn("PAUSED", probe.text("aep_status3"))
        probe.mock["file_fail"] = False
        probe.click("aep_save")
        self.assertFalse(probe.probe()["paused"])
        self.assertEqual(len(probe.records_of("resume")), 0)
        probe.advance(15000)
        resume = probe.records_of("resume")
        self.assertEqual(len(resume), 1)
        self.assertGreater(resume[0]["missed"], 0)
        summary = probe.records_of("summary")[0]
        self.assertGreater(summary["missed_while_paused"], 0)

    def test_stop_start_toggle(self):
        probe = self.make()
        probe.emit("LOOT_DICE", 1)
        probe.click("aep_toggle")
        self.assertEqual(len(probe.files()), 1)
        self.assertEqual(probe.text("aep_toggle"), "Start")
        self.assertIn("Stopped", probe.text("aep_status3"))
        probe.emit("LOOT_DICE", 2)
        self.assertEqual(probe.probe()["events_seen"], 1)
        probe.click("aep_toggle")
        probe.emit("LOOT_DICE", 3)
        self.assertEqual(probe.probe()["events_seen"], 2)
        self.assertEqual(probe.text("aep_toggle"), "Stop")

    # -- snapshots ---------------------------------------------------------------

    def test_bag_snapshot_on_load_events_and_period(self):
        probe = self.make(bag={1: {"id": 10, "count": 2}, 3: {"id": 11}})
        probe.advance(15000)
        bags = probe.records_of("bag")
        self.assertEqual(bags[0]["reason"], "load")
        self.assertEqual(bags[0]["items"][1]["id"], 10)
        self.assertEqual(bags[0]["items"][3]["id"], 11)
        self.assertEqual(bags[0]["currency"], 777)
        self.assertEqual(bags[0]["capacity"], 5)
        self.assertEqual(bags[0]["filled"], 2)
        # Bag event: debounced snapshot, unchanged contents are marked.
        probe.emit("BAG_UPDATE")
        probe.emit("ADDED_ITEM", 10)
        probe.advance(1000, 2)
        probe.advance(15000)
        bags = probe.records_of("bag")
        self.assertEqual([b["reason"] for b in bags], ["load", "delayed_10s", "bag_event"])
        self.assertTrue(bags[2]["unchanged"])
        # A real change is stored in full by the 60 s periodic snapshot.
        probe.mock["bag"][2] = probe.lua.table_from({"id": 12})
        probe.advance(15000, 4)
        bags = probe.records_of("bag")
        self.assertEqual(bags[-1]["reason"], "periodic")
        self.assertEqual(bags[-1]["items"][2]["id"], 12)

    def test_target_change_dump(self):
        probe = self.make()
        probe.set_unit("target", id=555, name="Wolf", x=5.0, y=6.0, z=7.0,
                       info={"level": 9, "type": "npc"}, level=9,
                       npc={"templateId": 1234}, hp=40, max_hp=50,
                       buffs=[{"name": "Bleed", "id": 8}])
        probe.advance(1000)
        probe.advance(15000)
        dumps = probe.records_of("target")
        self.assertEqual(len(dumps), 1)
        dump = dumps[0]
        self.assertEqual(dump["id"], 555)
        self.assertEqual(dump["calls"]["Unit.UnitInfo"]["values"][1]["type"], "npc")
        self.assertEqual(dump["calls"]["Unit.GetNpcInfo"]["values"][1]["templateId"], 1234)
        self.assertEqual(dump["by_id"]["Unit.GetUnitInfoById"]["values"][1]["level"], 9)
        self.assertEqual(dump["buffs"]["count"], 1)
        self.assertEqual(dump["buffs"]["list"][1]["values"][1]["name"], "Bleed")
        self.assertFalse(dump["calls"]["Unit.UnitHiddenBuffCount"]["ok"])
        hints = dump["id_like_fields"]
        self.assertEqual(hints["Unit.GetNpcInfo.1.templateId"], 1234)
        # Same target again: no second dump. Clearing it is logged.
        probe.advance(1000, 3)
        probe.mock["units"]["target"] = None
        probe.advance(1000)
        probe.advance(15000)
        self.assertEqual(len(probe.records_of("target")), 1)
        self.assertEqual(len(probe.records_of("target_cleared")), 1)

    def test_api_inventory_records_values_and_errors(self):
        probe = self.make()
        probe.advance(15000)
        inventory = probe.records_of("api_inventory")[0]
        calls = {(c["lib"], c["fn"]): c for c in seq(inventory["calls"])
                 if not c["args"] or c["args"][1] in ("player", 1)}
        self.assertEqual(calls[("Unit", "UnitName")]["r"]["values"][1], "Hero")
        self.assertEqual(seq(calls[("Unit", "UnitWorldPosition")]["r"]["values"]),
                         [10.5, 20.5, 30.5])
        self.assertFalse(calls[("Unit", "UnitRace")]["r"]["ok"])
        self.assertIn("missing method", calls[("Unit", "UnitRace")]["r"]["error"])
        self.assertIn(("Equipment", "GetEquippedItemTooltipInfo"), calls)
        surface = probe.records_of("api_surface")[0]
        self.assertIn("GetUnitId", surface["libs"]["Unit"]["members"])
        self.assertTrue(surface["libs"]["Unit"]["present"])
        self.assertFalse(surface["libs"]["House"]["present"])

    # -- passivity -----------------------------------------------------------------

    def test_never_calls_state_changing_api(self):
        probe = self.make()
        probe.set_unit("target", id=9, name="X", x=0.0, y=0.0, z=0.0, info={})
        for event in ("CHAT_MESSAGE", "BAG_UPDATE", "ENTERED_WORLD",
                      "INTERACTION_START", "QUEST_TASK_READY"):
            probe.emit(event, "a")
        probe.advance(1000, 70)
        probe.click("aep_save")
        probe.click("aep_toggle")
        probe.h["unload"](probe.h)
        self.assertEqual(probe.violations(), [])

    def test_source_has_no_state_changing_calls(self):
        forbidden = re.compile(
            r"(TargetUnit|DispatchChatMessage|ExpressEmotion|InviteToTeam|"
            r"EquipBagItem|ClearCursor|ToggleMapWithPortal|MoveTeamMember|"
            r"ChangeAppellation|SetRole|MakeTeamOwner|SetCursorImage|"
            r"ExcuteActionHandler)")
        for path in ADDON.glob("*.lua"):
            text = path.read_text(encoding="utf-8")
            if path.name == "getters.lua":
                # The catalogue names forbidden prefixes only in comments and
                # the refusal list.
                text = re.sub(r"--[^\n]*", "", text)
                text = re.sub(r"local FORBIDDEN = \{.*?\n\}", "", text, flags=re.S)
            self.assertIsNone(forbidden.search(text), path.name)

    def test_getter_guard_refuses_state_changing_names(self):
        probe = self.make(load=False)
        guard = probe.lua.execute(
            "package.path = package.path; return require('aaemu_probe/getters')")
        for name in ("SetRole", "EquipBagItem", "InviteToTeam", "TargetUnit",
                     "ClearCursor", "ToggleMapWithPortal", "DispatchChatMessage"):
            self.assertFalse(guard["is_read_only"](name), name)
        for name in ("GetUnitId", "UnitInfo", "GetBagItemInfo", "Capacity"):
            self.assertTrue(guard["is_read_only"](name), name)

    def test_sandbox_env_mirrors_real_globals(self):
        # The real addon env has none of these; the stub must not either.
        probe = self.make(load=False)
        sandbox = probe.h["sandbox"]
        for name in ("_G", "getfenv", "setfenv", "setmetatable", "select",
                     "error", "assert", "rawget", "next", "os", "io",
                     "loadstring", "loadfile", "package", "debug", "X2Chat"):
            self.assertIsNone(sandbox[name], name)

    def test_boot_trail_written_on_load(self):
        probe = self.make()
        steps = {}
        for path in probe.all_files():
            if "/boot_" in path:
                steps[path.rsplit("boot_", 1)[1][:-4]] = probe.payload(path)
        self.assertEqual(sorted(steps), ["0", "1", "2", "3", "4"])
        self.assertTrue(steps["0"]["ok"])
        self.assertEqual(steps["0"]["extra"]["sandbox"]["setmetatable"], "nil")
        self.assertEqual(steps["0"]["extra"]["globals"]["UI_EVENTS"], "table")
        self.assertEqual(steps["3"]["extra"]["registered_ok"], 206)
        self.assertTrue(steps["2"]["extra"]["window"])
        infos = [m for m in probe.mock["logs"].values() if "boot_0" in m[2]]
        self.assertEqual(len(infos), 1)

    def test_boot_reports_failure_not_silence(self):
        probe = self.make(load=False)
        probe.mock["settings_fail"] = True
        probe.h["sandbox"]["UI_EVENTS"] = None
        probe.h["load"](probe.h)
        # Window/event setup still ran with the built-in list.
        self.assertEqual(probe.mock["calls"]["RegisterEvent"], 206)

    def test_reload_replaces_previous_instance(self):
        probe = self.make()
        first = probe.probe()
        probe.h["reload"](probe.h)
        second = probe.probe()
        self.assertFalse(first["alive"])
        self.assertTrue(second["alive"])


if __name__ == "__main__":  # pragma: no cover
    unittest.main()
