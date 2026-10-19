"""Focused tests for the quest guide catalog builder.

The fixtures are deliberately tiny and broken in known ways: one optional
Fail/Drop branch without spawns, one blocked quest, one unsupported act, one
auxiliary record, one tutorial record, mixed AND/OR requirements and serialization
edge cases. Real-data generation is exercised by the build command, not here.
"""

from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path
import shutil
import sqlite3
import subprocess
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parent))
import build  # noqa: E402

LUA_DEPS = os.environ.get("AAEMU_LUA_TEST_DEPS")
if LUA_DEPS:
    sys.path.insert(0, LUA_DEPS)
try:
    from lupa.lua51 import LuaRuntime
except Exception:  # pragma: no cover - optional runtime
    LuaRuntime = None


SCHEMA = """
CREATE TABLE quest_contexts (
    id INTEGER PRIMARY KEY, name TEXT, category_id INTEGER, LEVEL INTEGER,
    REPEATABLE TEXT DEFAULT 'f', selective TEXT DEFAULT 'f',
    successive TEXT DEFAULT 'f', restart_on_fail TEXT DEFAULT 'f',
    let_it_done TEXT DEFAULT 'f', chapter_idx INTEGER, quest_idx INTEGER,
    milestone_id INTEGER, zone_id INTEGER, degree INTEGER, grade_id INTEGER,
    translate TEXT DEFAULT 't');
CREATE TABLE quest_categories (id INTEGER PRIMARY KEY, name TEXT, translate TEXT);
CREATE TABLE quest_context_texts (
    id INTEGER PRIMARY KEY, quest_context_text_kind_id INTEGER,
    quest_context_id INTEGER, text TEXT);
CREATE TABLE quest_supplies (id INTEGER PRIMARY KEY, LEVEL INTEGER, exp INTEGER, copper INTEGER);
CREATE TABLE quest_components (
    id INTEGER PRIMARY KEY, quest_context_id INTEGER, component_kind_id INTEGER,
    or_unit_reqs TEXT, npc_id INTEGER, npc_spawner_id INTEGER,
    next_component INTEGER);
CREATE TABLE unit_reqs (
    id INTEGER PRIMARY KEY, owner_type TEXT, owner_id INTEGER, kind_id INTEGER,
    value1 INTEGER, value2 INTEGER);
CREATE TABLE quest_acts (
    id INTEGER PRIMARY KEY, quest_component_id INTEGER, act_detail_id INTEGER,
    act_detail_type TEXT);
CREATE TABLE quest_monster_npcs (id INTEGER, quest_monster_group_id INTEGER, npc_id INTEGER);
CREATE TABLE quest_item_group_items (id INTEGER, quest_item_group_id INTEGER, item_id INTEGER);
CREATE TABLE craft_products (id INTEGER, craft_id INTEGER, item_id INTEGER);
CREATE TABLE quest_act_con_accept_npcs (id INTEGER PRIMARY KEY, npc_id INTEGER);
CREATE TABLE quest_act_con_report_npcs (id INTEGER PRIMARY KEY, npc_id INTEGER);
CREATE TABLE quest_act_obj_talks (id INTEGER PRIMARY KEY, npc_id INTEGER);
CREATE TABLE quest_act_obj_monster_hunts (
    id INTEGER PRIMARY KEY, npc_id INTEGER, count INTEGER);
CREATE TABLE quest_act_con_accept_npc_kills (id INTEGER PRIMARY KEY, npc_id INTEGER);
CREATE TABLE quest_act_obj_monster_group_hunts (
    id INTEGER PRIMARY KEY, quest_monster_group_id INTEGER, count INTEGER);
CREATE TABLE quest_act_obj_interactions (
    id INTEGER PRIMARY KEY, wi_id INTEGER, count INTEGER, doodad_id INTEGER);
CREATE TABLE quest_act_con_accept_doodads (id INTEGER PRIMARY KEY, doodad_id INTEGER);
CREATE TABLE quest_act_con_report_doodads (id INTEGER PRIMARY KEY, doodad_id INTEGER);
CREATE TABLE quest_act_obj_item_gathers (
    id INTEGER PRIMARY KEY, item_id INTEGER, count INTEGER);
CREATE TABLE quest_act_obj_item_uses (id INTEGER PRIMARY KEY, item_id INTEGER, count INTEGER);
CREATE TABLE quest_act_con_accept_items (id INTEGER PRIMARY KEY, item_id INTEGER);
CREATE TABLE quest_act_obj_spheres (id INTEGER PRIMARY KEY, sphere_id INTEGER);
CREATE TABLE quest_act_con_accept_spheres (id INTEGER PRIMARY KEY, sphere_id INTEGER);
CREATE TABLE quest_act_con_auto_completes (id INTEGER PRIMARY KEY);
CREATE TABLE quest_act_con_report_journals (id INTEGER PRIMARY KEY);
CREATE TABLE quest_act_con_accept_components (
    id INTEGER PRIMARY KEY, quest_context_id INTEGER);
CREATE TABLE quest_act_obj_crafts (id INTEGER PRIMARY KEY, craft_id INTEGER, count INTEGER);
CREATE TABLE quest_act_obj_zone_kills (
    id INTEGER PRIMARY KEY, count_pk INTEGER, count_npc INTEGER, zone_id INTEGER,
    lv_min INTEGER, lv_max INTEGER, lv_min_npc INTEGER, lv_max_npc INTEGER);
CREATE TABLE quest_act_etc_item_obtains (
    id INTEGER PRIMARY KEY, item_id INTEGER, count INTEGER);
CREATE TABLE quest_act_obj_complete_quests (id INTEGER PRIMARY KEY, quest_id INTEGER);
CREATE TABLE quest_act_obj_express_fires (
    id INTEGER PRIMARY KEY, express_key_id INTEGER, npc_group_id INTEGER, count INTEGER);
CREATE TABLE quest_act_obj_aggros (
    id INTEGER PRIMARY KEY, "RANGE" INTEGER, rank1 INTEGER, rank2 INTEGER, rank3 INTEGER);
CREATE TABLE quest_act_con_accept_item_gains (
    id INTEGER PRIMARY KEY, item_id INTEGER, count INTEGER);
CREATE TABLE quest_act_obj_cinemas (id INTEGER PRIMARY KEY, cinema_id INTEGER);
CREATE TABLE quest_act_obj_ability_levels (
    id INTEGER PRIMARY KEY, ability_id INTEGER, LEVEL INTEGER);
CREATE TABLE quest_act_obj_mate_levels (
    id INTEGER PRIMARY KEY, item_id INTEGER, LEVEL INTEGER);
CREATE TABLE quest_act_obj_levels (id INTEGER PRIMARY KEY, LEVEL INTEGER);
CREATE TABLE quest_act_con_accept_level_ups (id INTEGER PRIMARY KEY, LEVEL INTEGER);
CREATE TABLE quest_act_obj_item_group_gathers (
    id INTEGER PRIMARY KEY, item_group_id INTEGER, count INTEGER);
CREATE TABLE quest_act_obj_item_group_uses (
    id INTEGER PRIMARY KEY, item_group_id INTEGER, count INTEGER);
CREATE TABLE quest_act_obj_talk_npc_groups (
    id INTEGER PRIMARY KEY, npc_group_id INTEGER);
CREATE TABLE quest_act_supply_exps (id INTEGER PRIMARY KEY, exp INTEGER);
CREATE TABLE quest_act_supply_coppers (id INTEGER PRIMARY KEY, amount INTEGER);
CREATE TABLE quest_act_supply_items (
    id INTEGER PRIMARY KEY, item_id INTEGER, count INTEGER, grade_id INTEGER);
CREATE TABLE quest_act_supply_selective_items (
    id INTEGER PRIMARY KEY, item_id INTEGER, count INTEGER, grade_id INTEGER);
CREATE TABLE quest_act_supply_appellations (
    id INTEGER PRIMARY KEY, appellation_id INTEGER);
CREATE TABLE quest_act_supply_honor_points (id INTEGER PRIMARY KEY, point INTEGER);
CREATE TABLE quest_act_supply_living_points (id INTEGER PRIMARY KEY, point INTEGER);
CREATE TABLE quest_act_supply_jury_points (id INTEGER PRIMARY KEY, point INTEGER);
CREATE TABLE quest_act_supply_crime_points (id INTEGER PRIMARY KEY, point INTEGER);
CREATE TABLE quest_act_supply_aa_points (id INTEGER PRIMARY KEY, point INTEGER);
CREATE TABLE quest_act_supply_lps (id INTEGER PRIMARY KEY, lp INTEGER);
CREATE TABLE quest_act_supply_skills (id INTEGER PRIMARY KEY, skill_id INTEGER);
CREATE TABLE quest_act_supply_interactions (id INTEGER PRIMARY KEY, wi_id INTEGER);
CREATE TABLE quest_act_supply_remove_items (
    id INTEGER PRIMARY KEY, item_id INTEGER, count INTEGER);
CREATE TABLE spawn_effects (id INTEGER PRIMARY KEY, owner_type_id INTEGER, sub_type INTEGER);
CREATE TABLE effects (id INTEGER PRIMARY KEY, actual_id INTEGER, actual_type TEXT);
CREATE TABLE skill_effects (id INTEGER PRIMARY KEY, skill_id INTEGER, effect_id INTEGER);
CREATE TABLE skills (id INTEGER PRIMARY KEY);
CREATE TABLE items (id INTEGER PRIMARY KEY, loot_quest_id INTEGER, use_skill_id INTEGER);
CREATE TABLE loots (
    id INTEGER PRIMARY KEY, item_id INTEGER, drop_rate INTEGER, loot_pack_id INTEGER);
CREATE TABLE loot_pack_dropping_npcs (
    id INTEGER PRIMARY KEY, npc_id INTEGER, loot_pack_id INTEGER);
CREATE TABLE gain_loot_pack_item_effects (id INTEGER PRIMARY KEY, loot_pack_id INTEGER);
CREATE TABLE special_effects (
    id INTEGER PRIMARY KEY, special_effect_type_id INTEGER, value1 INTEGER);
CREATE TABLE skill_products (
    id INTEGER PRIMARY KEY, skill_id INTEGER, item_id INTEGER, amount INTEGER);
CREATE TABLE doodad_func_groups (id INTEGER PRIMARY KEY, doodad_almighty_id INTEGER);
CREATE TABLE doodad_funcs (
    id INTEGER PRIMARY KEY, doodad_func_group_id INTEGER, actual_func_id INTEGER,
    actual_func_type TEXT);
CREATE TABLE doodad_func_loot_items (id INTEGER PRIMARY KEY, item_id INTEGER);
CREATE TABLE doodad_func_loot_packs (id INTEGER PRIMARY KEY, loot_pack_id INTEGER);
CREATE TABLE doodad_func_purchases (id INTEGER PRIMARY KEY, item_id INTEGER);
CREATE TABLE merchant_goods (id INTEGER PRIMARY KEY, item_id INTEGER, merchant_pack_id INTEGER);
CREATE TABLE merchant_packs (id INTEGER PRIMARY KEY, owner_npc_id INTEGER);
CREATE TABLE merchants (id INTEGER PRIMARY KEY, npc_id INTEGER, merchant_pack_id INTEGER);
CREATE TABLE npcs (id INTEGER PRIMARY KEY);
CREATE TABLE doodad_almighties (id INTEGER PRIMARY KEY);
CREATE TABLE npc_spawner_npcs (id INTEGER PRIMARY KEY, npc_spawner_id INTEGER, member_id INTEGER);
CREATE TABLE npc_spawners (
    id INTEGER PRIMARY KEY, startTime INTEGER DEFAULT 0, endTime INTEGER DEFAULT 0);
CREATE TABLE game_schedule_spawners (id INTEGER PRIMARY KEY, spawner_id INTEGER, game_schedule_id INTEGER);
CREATE TABLE game_schedule_doodads (id INTEGER PRIMARY KEY, doodad_id INTEGER, game_schedule_id INTEGER);
CREATE TABLE game_schedules (id INTEGER PRIMARY KEY);
"""

WEIRD_NAME = '테스트 "Quest" \\ name\nline\x01\x001\x7f end'
WEIRD_TEXT = 'body "quoted" \\ slash\nnewline\ttab\x01 control 한국어 🙂'


def make_fixture(root: Path):
    db = root / "compact.sqlite3"
    worlds = root / "Worlds"
    (worlds / "main_world").mkdir(parents=True)
    (worlds / "main_world" / "npc_spawns.json").write_text(json.dumps([
        {"UnitId": 500, "Position": {"X": 10.0, "Y": 20.0, "Z": 30.0}},
    ]), encoding="utf-8")

    conn = sqlite3.connect(db)
    conn.executescript(SCHEMA)
    conn.executemany("INSERT INTO quest_categories VALUES (?, ?, 't')", [
        (3, "Test Category"), (45, "Tutorial"),
    ])
    context = (
        "INSERT INTO quest_contexts (id,name,category_id,LEVEL,REPEATABLE,selective,"
        "successive,restart_on_fail,let_it_done,chapter_idx,quest_idx,milestone_id,"
        "zone_id,degree,grade_id,translate) VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)"
    )
    conn.execute(context, (100, WEIRD_NAME, 3, 5, "f", "f", "f", "f", "f",
                           1, 2, 3, 10, 1, 1, "t"))
    conn.execute(context, (101, "Aux", 3, 5, "f", "f", "f", "f", "f",
                           None, None, None, 10, 1, 1, "t"))
    conn.execute(context, (102, "Blocked", 3, 5, "f", "f", "f", "f", "f",
                           None, None, None, 10, 1, 1, "t"))
    conn.execute(context, (103, "Partial", 3, 5, "f", "f", "f", "f", "f",
                           None, None, None, 10, 1, 1, "t"))
    conn.execute(context, (104, "Tutorial", 45, 1, "f", "f", "f", "f", "f",
                           None, None, None, 10, 1, 1, "t"))
    conn.execute("INSERT INTO quest_context_texts VALUES (1, 1, 100, ?)", (WEIRD_TEXT,))
    conn.execute("INSERT INTO quest_supplies VALUES (1, 5, 1000, 50)")

    components = [
        (1000, 100, 2, "t"), (1001, 100, 4, "f"), (1002, 100, 5, "f"),
        (1003, 100, 7, "f"), (1004, 100, 6, "f"), (1005, 100, 8, "f"),
        (1006, 100, 2, "f"), (1007, 100, 2, "f"),
        (1010, 101, 4, "f"),
        (1020, 102, 2, "f"), (1021, 102, 4, "f"),
        (1030, 103, 2, "f"), (1031, 103, 4, "f"),
        (1040, 104, 2, "f"), (1041, 104, 4, "f"),
    ]
    conn.executemany(
        "INSERT INTO quest_components (id,quest_context_id,component_kind_id,"
        "or_unit_reqs,npc_id,npc_spawner_id) VALUES (?,?,?,?,0,0)", components)

    requirements = [
        (1, "QuestComponent", 1000, 1, 5, 0),
        (2, "QuestComponent", 1000, 3, 1, 0),
        (3, "QuestComponent", 1000, 0x1F, 99, 0),
        (4, "QuestComponent", 1001, 0x24, 88, 0),
        (5, "QuestComponent", 1006, 1, 10, 0),
        (6, "QuestComponent", 1006, 4, 2, 0),
        (7, "QuestComponent", 1020, 1, 5, 0),
        (8, "QuestComponent", 1030, 1, 5, 0),
        (9, "QuestComponent", 1040, 1, 1, 0),
        (10, "Other", 1000, 1, 999, 0),
    ]
    conn.executemany("INSERT INTO unit_reqs VALUES (?,?,?,?,?,?)", requirements)

    acts = [
        (2001, 1001, 1, "QuestActObjMonsterHunt"),
        (2002, 1001, 2, "QuestActObjItemGather"),
        (2003, 1002, 3, "QuestActObjMonsterHunt"),
        (2004, 1003, 4, "QuestActObjInteraction"),
        (2005, 1004, 5, "QuestActConReportNpc"),
        (2006, 1005, 6, "QuestActSupplyItem"),
        (2007, 1005, 7, "QuestActSupplyExp"),
        (2008, 1005, 8, "QuestActSupplyCopper"),
        (2009, 1005, 9, "QuestActSupplyAppellation"),
        (2012, 1005, 12, "QuestActSupplyLp"),
        (2013, 1002, 13, "QuestActSupplyExp"),
        (2010, 1000, 10, "QuestActConAcceptNpc"),
        (2011, 1006, 11, "QuestActConAcceptNpc"),
        (2020, 1010, 20, "QuestActObjItemGather"),
        (2030, 1020, 30, "QuestActConAcceptNpc"),
        (2031, 1021, 31, "QuestActObjMonsterHunt"),
        (2040, 1030, 40, "QuestActConAcceptNpc"),
        (2041, 1031, 0, "QuestActObjMystery"),
        (2050, 1040, 50, "QuestActConAcceptNpc"),
        (2051, 1041, 51, "QuestActObjMonsterHunt"),
    ]
    conn.executemany("INSERT INTO quest_acts VALUES (?,?,?,?)", acts)
    conn.executemany("INSERT INTO quest_act_con_accept_npcs VALUES (?, ?)", [
        (10, 500), (11, 500), (30, 500), (40, 500), (50, 500),
    ])
    conn.execute("INSERT INTO quest_act_obj_monster_hunts VALUES (1, 500, 3)")
    conn.execute("INSERT INTO quest_act_obj_item_gathers VALUES (2, 900, 2)")
    conn.execute("INSERT INTO quest_act_obj_monster_hunts VALUES (3, 777, 1)")
    conn.execute("INSERT INTO quest_act_obj_interactions VALUES (4, 0, 1, 888)")
    conn.execute("INSERT INTO quest_act_con_report_npcs VALUES (5, 500)")
    conn.execute("INSERT INTO quest_act_supply_items VALUES (6, 1234, 1, 0)")
    conn.execute("INSERT INTO quest_act_supply_exps VALUES (7, 500)")
    conn.execute("INSERT INTO quest_act_supply_coppers VALUES (8, 100)")
    conn.execute("INSERT INTO quest_act_supply_appellations VALUES (9, 42)")
    conn.execute("INSERT INTO quest_act_supply_lps VALUES (12, 7)")
    conn.execute("INSERT INTO quest_act_supply_exps VALUES (13, 999)")
    conn.execute("INSERT INTO quest_act_obj_item_gathers VALUES (20, 900, 1)")
    conn.execute("INSERT INTO quest_act_obj_monster_hunts VALUES (31, 999, 1)")
    conn.execute("INSERT INTO quest_act_obj_monster_hunts VALUES (51, 999, 1)")

    conn.executemany("INSERT INTO npcs VALUES (?)", [(500,)])
    conn.executemany("INSERT INTO doodad_almighties VALUES (?)", [(888,)])
    conn.execute("INSERT INTO items VALUES (900, NULL, NULL)")
    conn.execute("INSERT INTO loots VALUES (1, 900, 10000000, 10)")
    conn.execute("INSERT INTO loot_pack_dropping_npcs VALUES (1, 500, 10)")
    conn.commit()
    conn.close()
    return db, worlds


def file_bytes(root: Path) -> dict[str, bytes]:
    return {
        path.relative_to(root).as_posix(): path.read_bytes()
        for path in sorted(root.rglob("*")) if path.is_file()
    }


def lua_values(table):
    """Values of a 1-based Lua array table returned by Lupa."""
    return [table[index] for index in range(1, len(table) + 1)]


class BuilderTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)
        self.db, self.worlds = make_fixture(self.root)
        self.output = self.root / "out"
        self.provenance = {"commit": "test-commit", "source": "test", "dirty": False}

    def tearDown(self):
        self.temp.cleanup()

    def build(self, output=None, **kwargs):
        kwargs.setdefault("data_only", True)
        return build.run_build(
            self.db, self.worlds, output or self.output,
            code_commit="test-commit", **kwargs,
        )

    def read_manifest(self):
        return json.loads((self.output / "manifest.json").read_text(encoding="utf-8"))

    def read_index(self):
        runtime = LuaRuntime(unpack_returned_tuples=True)
        return runtime.execute(
            (self.output / "aaemu_quest_guide" / "data" / "index.lua")
            .read_text(encoding="utf-8")
        )

    def read_quest(self, quest_id):
        index = self.read_index()
        chunk_file = None
        for entry in lua_values(index["quests"]):
            if entry["id"] == quest_id:
                chunk_file = self.output / "aaemu_quest_guide" / "data" / (
                    "chunk_%04d.lua" % entry["chunk"]
                )
                break
        self.assertIsNotNone(chunk_file, f"quest {quest_id} missing from index")
        runtime = LuaRuntime(unpack_returned_tuples=True)
        table = runtime.execute(chunk_file.read_text(encoding="utf-8"))
        return table["quests"][quest_id]

    def test_catalog_preserves_grouping_branches_and_rewards(self):
        manifest, summary = self.build()
        self.assertEqual(manifest["data_version"], "1.2")
        self.assertEqual(manifest["generator"]["code_commit"], "test-commit")
        self.assertEqual(manifest["catalog"]["contexts"], {
            "total": 5, "loaded": 4, "excluded_tutorial_category": 45, "excluded": 1,
        })
        self.assertEqual(summary["quests"], 4)
        self.assertEqual(summary["status"], {"auxiliary": 1, "blocked": 1, "ok": 1,
                                             "partial": 1})

        index = self.read_index()
        self.assertEqual(index["quest_count"], 4)
        self.assertEqual(index["chunk_count"], 1)
        self.assertEqual(index["categories"][3], "Test Category")

        quest = self.read_quest(100)
        self.assertEqual(quest["id"], 100)
        self.assertEqual(quest["name"], WEIRD_NAME)
        self.assertEqual(quest["status"], "ok")
        self.assertEqual(lua_values(quest["prereq_quests"]), [99])
        self.assertEqual(quest["context_texts"][1]["text"], WEIRD_TEXT)
        self.assertTrue(quest["target_id_space"])

        groups = {group["component_id"]: group for group in
                  lua_values(quest["requirement_groups"])}
        self.assertEqual(groups[1000]["mode"], "OR")
        self.assertEqual(groups[1000]["scope"], "accept")
        self.assertEqual([req["kind"] for req in
                          lua_values(groups[1000]["requirements"])],
                         ["level", "race", "complete_quest_context"])
        self.assertEqual(groups[1006]["mode"], "AND")
        self.assertEqual(groups[1001]["scope"], "component")
        self.assertEqual(groups[1001]["component_kind"], "Progress")
        self.assertTrue(groups[1007]["unrestricted"])
        self.assertEqual(lua_values(groups[1007]["requirements"]), [])
        acceptance = quest["acceptance"]
        self.assertEqual(acceptance["mode"], "any-start-component")
        self.assertEqual(lua_values(acceptance["start_components"]),
                         [1000, 1006, 1007])
        self.assertTrue(acceptance["start_components_are_alternatives"])
        self.assertIn("not a mandatory all-of", quest["prereq_quests_note"])

        normal_act_ids = {step["act_id"] for step in lua_values(quest["steps"])}
        self.assertNotIn(2003, normal_act_ids)
        self.assertNotIn(2004, normal_act_ids)
        self.assertNotIn(2013, normal_act_ids)
        self.assertEqual(len(quest["branches"]["fail"]), 1)
        self.assertEqual(len(quest["branches"]["drop"]), 1)
        self.assertEqual(quest["branches"]["fail"][1]["component_id"], 1002)
        self.assertEqual(quest["branches"]["drop"][1]["steps"][1]["act_id"], 2004)
        self.assertTrue(quest["branches"]["fail"][1]["optional_branch"])
        fail_steps = lua_values(quest["branches"]["fail"][1]["steps"])
        branch_supply = next(step for step in fail_steps if step["act_id"] == 2013)
        self.assertEqual(branch_supply["act_detail"]["exp"], 999)
        self.assertEqual(branch_supply["act_detail_id"], 13)

        hunt = next(step for step in lua_values(quest["steps"])
                    if step["act_id"] == 2001)
        self.assertEqual(hunt["target"]["count"], 3)
        self.assertEqual(hunt["target"]["id_space"], "static_template")
        self.assertEqual(hunt["act_detail_id"], 1)
        self.assertEqual(hunt["locations"][1]["x"], 10.0)
        self.assertEqual(hunt["locations_source"], "1.2-worlds-candidate")
        self.assertTrue(hunt["unverified_coordinates"])
        self.assertEqual(hunt["act_detail"]["npc_id"], 500)
        gather = next(step for step in lua_values(quest["steps"])
                      if step["act_id"] == 2002)
        self.assertTrue(gather["item_sources"])
        self.assertEqual(gather["item_sources"][1]["item_id"], 900)
        self.assertTrue(gather["item_sources"][1]["sources"])

        rewards = quest["rewards"]
        self.assertEqual(rewards["expected"]["act_exp"], 500)
        self.assertEqual(rewards["expected"]["act_copper"], 100)
        self.assertEqual(rewards["expected"]["items"][1]["item_id"], 1234)
        self.assertEqual(rewards["expected"]["appellations"][1], 42)
        self.assertEqual(rewards["expected"]["lp"], 7)
        self.assertEqual(rewards["expected"]["act_exp"], 500)
        self.assertEqual(rewards["level_supply"]["exp"], 1000)
        self.assertEqual(rewards["level_supply"]["copper"], 50)
        self.assertEqual(len(rewards["supply_acts"]), 5)
        self.assertEqual(rewards["supply_acts"][1]["detail"]["count"], 1)
        self.assertNotIn(2013, [act["act_id"] for act in
                                lua_values(rewards["supply_acts"])])

    def test_lua_chunks_compile_and_escape_strings(self):
        if LuaRuntime is None:
            self.skipTest("lupa.lua51 unavailable")
        self.build()
        data = self.output / "aaemu_quest_guide" / "data"
        runtime = LuaRuntime(unpack_returned_tuples=True)
        for path in sorted(data.glob("*.lua")):
            runtime.execute(path.read_text(encoding="utf-8"))
        index = runtime.execute((data / "index.lua").read_text(encoding="utf-8"))
        self.assertEqual(index["data_version"], "1.2")
        entry = None
        for candidate in lua_values(index["quests"]):
            if candidate["id"] == 100:
                entry = candidate
        self.assertIsNotNone(entry)
        chunk = runtime.execute(
            (data / ("chunk_%04d.lua" % entry["chunk"])).read_text(encoding="utf-8")
        )
        quest = chunk["quests"][100]
        self.assertEqual(quest["name"], WEIRD_NAME)
        self.assertEqual(quest["context_texts"][1]["text"], WEIRD_TEXT)
        route = runtime.execute((data / "route.lua").read_text(encoding="utf-8"))
        self.assertEqual(lua_values(route["quest_ids"]), [330, 6198])
        self.assertTrue(any("330" in warning and "6198" in warning
                            for warning in lua_values(route["warnings"])))
        raw = (data / "chunk_0001.lua").read_text(encoding="utf-8")
        self.assertIn("\\0001", raw)
        self.assertIn("\\127", raw)

    def test_deterministic_output_and_no_timestamp_by_default(self):
        first = self.root / "out1"
        second = self.root / "out2"
        self.build(first)
        self.build(second)
        self.assertEqual(file_bytes(first), file_bytes(second))
        manifest = json.loads((first / "manifest.json").read_text(encoding="utf-8"))
        self.assertNotIn("generated_at", manifest)
        self.assertNotIn(str(self.root), json.dumps(manifest, ensure_ascii=False))
        self.assertTrue(manifest["files"])
        for entry in manifest["files"]:
            self.assertTrue(entry["path"].startswith("aaemu_quest_guide/data/"))

    def test_inputs_are_not_modified(self):
        db_before = hashlib.sha256(self.db.read_bytes()).hexdigest()
        worlds_before = file_bytes(self.worlds)
        mtimes = {path: path.stat().st_mtime_ns for path in
                  [self.db, *self.worlds.rglob("*")] if path.is_file()}
        self.build()
        self.assertEqual(hashlib.sha256(self.db.read_bytes()).hexdigest(), db_before)
        self.assertEqual(file_bytes(self.worlds), worlds_before)
        self.assertEqual({path: path.stat().st_mtime_ns for path in mtimes},
                         mtimes)
        self.assertFalse(list(self.db.parent.glob(self.db.name + "-*")))

    def test_output_safety_rejects_overlaps_and_foreign_data(self):
        with self.assertRaises(build.BuildError):
            self.build(self.worlds / "out")
        with self.assertRaises(build.BuildError):
            self.build(self.db.parent)
        fake_tool = self.root / "tool"
        fake_tool.mkdir()
        with self.assertRaises(build.BuildError):
            build.run_build(self.db, self.worlds, fake_tool / "package",
                            source_dir=fake_tool, data_only=True)

        foreign = self.root / "foreign" / "aaemu_quest_guide" / "data"
        foreign.mkdir(parents=True)
        (foreign / "keep.txt").write_text("user data", encoding="utf-8")
        with self.assertRaises(build.BuildError):
            self.build(self.root / "foreign")
        self.assertEqual((foreign / "keep.txt").read_text(encoding="utf-8"),
                         "user data")

    def test_linked_data_directory_is_never_replaced(self):
        if not hasattr(os, "symlink"):
            self.skipTest("symlinks unavailable")
        output = self.root / "linked-out"
        (output / "aaemu_quest_guide").mkdir(parents=True)
        foreign = self.root / "foreign-data"
        foreign.mkdir()
        (foreign / build.MARKER_NAME).write_text(build.MARKER_TEXT, encoding="utf-8")
        (foreign / "keep.txt").write_text("foreign", encoding="utf-8")
        link = output / "aaemu_quest_guide" / "data"
        try:
            os.symlink(foreign, link, target_is_directory=True)
        except (OSError, NotImplementedError):
            if os.name != "nt":
                self.skipTest("directory symlinks unavailable")
            created = subprocess.run(
                ["cmd", "/c", "mklink", "/J", str(link), str(foreign)],
                capture_output=True, text=True,
            )
            if created.returncode != 0:
                self.skipTest("directory links unavailable")
        try:
            with self.assertRaises(build.BuildError):
                self.build(output)
            self.assertEqual((foreign / "keep.txt").read_text(encoding="utf-8"),
                             "foreign")
        finally:
            if link.is_symlink():
                link.unlink()
            elif getattr(link, "is_junction", lambda: False)():
                os.rmdir(link)

    def test_database_path_with_uri_special_characters(self):
        weird = self.root / "db #1.sqlite3"
        shutil.copyfile(self.db, weird)
        out = self.root / "out-special"
        manifest, _ = build.run_build(
            weird, self.worlds, out, code_commit="test-commit", data_only=True)
        self.assertEqual(manifest["inputs"]["db"]["file"], weird.name)
        self.assertTrue((out / "aaemu_quest_guide" / "data" / "index.lua").is_file())

    def test_generated_data_can_be_rebuilt(self):
        self.build()
        first = (self.output / "manifest.json").read_bytes()
        self.build()
        self.assertEqual((self.output / "manifest.json").read_bytes(), first)

    def test_timestamp_is_opt_in(self):
        self.build(timestamp=True)
        manifest = self.read_manifest()
        summary = json.loads(
            (self.output / "summary.json").read_text(encoding="utf-8"))
        self.assertIn("generated_at", manifest)
        self.assertEqual(manifest["generated_at"], summary["generated_at"])

    def test_lua_string_escape_edges(self):
        self.assertEqual(build.lua_string('a"b\\c'), '"a\\"b\\\\c"')
        self.assertEqual(build.lua_string("\x00" "1\x1f\x7f\n\r\t"),
                         '"\\0001\\031\\127\\n\\r\\t"')
        self.assertEqual(build.lua_string("한국어"), '"한국어"')
        cyclic = []
        cyclic.append(cyclic)
        with self.assertRaises(build.BuildError):
            build.lua_literal(cyclic)
        with self.assertRaises(build.BuildError):
            build.lua_literal(float("nan"))


if __name__ == "__main__":
    unittest.main()
