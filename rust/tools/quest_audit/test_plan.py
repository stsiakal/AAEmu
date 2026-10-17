"""Plan tests with deliberately broken miniature quest and spawn data."""

import json
from pathlib import Path
import sqlite3
import tempfile
import unittest

from plan import closest, load_spawns, plan


def database():
    conn = sqlite3.connect(":memory:")
    conn.row_factory = sqlite3.Row
    conn.executescript("""
        CREATE TABLE quest_contexts (id INTEGER, name TEXT, category_id INTEGER, LEVEL INTEGER);
        CREATE TABLE quest_components (id INTEGER, quest_context_id INTEGER,
            component_kind_id INTEGER, npc_id INTEGER, npc_spawner_id INTEGER);
        CREATE TABLE quest_acts (id INTEGER, quest_component_id INTEGER,
            act_detail_id INTEGER, act_detail_type TEXT);
        CREATE TABLE quest_monster_npcs (id INTEGER, quest_monster_group_id INTEGER,
            npc_id INTEGER);
        CREATE TABLE quest_act_con_accept_npcs (id INTEGER, npc_id INTEGER);
        CREATE TABLE quest_act_con_report_npcs (id INTEGER, npc_id INTEGER);
        CREATE TABLE quest_act_obj_talks (id INTEGER, npc_id INTEGER);
        CREATE TABLE quest_act_obj_monster_hunts (id INTEGER, npc_id INTEGER, count INTEGER);
        CREATE TABLE quest_act_con_accept_npc_kills (id INTEGER, npc_id INTEGER);
        CREATE TABLE quest_act_obj_monster_group_hunts (id INTEGER,
            quest_monster_group_id INTEGER, count INTEGER);
        CREATE TABLE quest_act_obj_interactions (id INTEGER, wi_id INTEGER, count INTEGER,
            doodad_id INTEGER);
        CREATE TABLE quest_act_con_accept_doodads (id INTEGER, doodad_id INTEGER);
        CREATE TABLE quest_act_con_report_doodads (id INTEGER, doodad_id INTEGER);
        CREATE TABLE quest_act_obj_item_gathers (id INTEGER, item_id INTEGER, count INTEGER);
        CREATE TABLE quest_act_obj_item_uses (id INTEGER, item_id INTEGER, count INTEGER);
        CREATE TABLE quest_act_con_accept_items (id INTEGER, item_id INTEGER);
        CREATE TABLE quest_act_obj_spheres (id INTEGER, sphere_id INTEGER);
        CREATE TABLE quest_act_con_accept_spheres (id INTEGER, sphere_id INTEGER);
        CREATE TABLE quest_act_con_auto_completes (id INTEGER);
        CREATE TABLE quest_act_con_report_journals (id INTEGER);
        CREATE TABLE quest_act_con_accept_components (id INTEGER, quest_context_id INTEGER);
        CREATE TABLE quest_act_obj_crafts (id INTEGER, craft_id INTEGER, count INTEGER);
        CREATE TABLE quest_act_obj_zone_kills (id INTEGER, count_pk INTEGER, count_npc INTEGER,
            zone_id INTEGER, lv_min INTEGER, lv_max INTEGER, lv_min_npc INTEGER,
            lv_max_npc INTEGER);
        CREATE TABLE quest_act_etc_item_obtains (id INTEGER, item_id INTEGER, count INTEGER);
        CREATE TABLE quest_act_obj_complete_quests (id INTEGER, quest_id INTEGER);
        CREATE TABLE quest_act_obj_express_fires (id INTEGER, express_key_id INTEGER,
            npc_group_id INTEGER, count INTEGER);
        CREATE TABLE quest_act_obj_aggros (id INTEGER, "RANGE" INTEGER, rank1 INTEGER,
            rank2 INTEGER, rank3 INTEGER);
        CREATE TABLE quest_act_con_accept_item_gains (id INTEGER, item_id INTEGER, count INTEGER);
        CREATE TABLE quest_act_obj_cinemas (id INTEGER, cinema_id INTEGER);
        CREATE TABLE quest_act_obj_ability_levels (id INTEGER, ability_id INTEGER, LEVEL INTEGER);
        CREATE TABLE quest_act_obj_mate_levels (id INTEGER, item_id INTEGER, LEVEL INTEGER);
        CREATE TABLE quest_act_obj_levels (id INTEGER, LEVEL INTEGER);
        CREATE TABLE quest_act_con_accept_level_ups (id INTEGER, LEVEL INTEGER);
        CREATE TABLE quest_act_obj_item_group_gathers (id INTEGER, item_group_id INTEGER,
            count INTEGER);
        CREATE TABLE quest_act_obj_item_group_uses (id INTEGER, item_group_id INTEGER,
            count INTEGER);
        CREATE TABLE quest_act_obj_talk_npc_groups (id INTEGER, npc_group_id INTEGER);
        CREATE TABLE quest_item_group_items (id INTEGER, quest_item_group_id INTEGER,
            item_id INTEGER);
        CREATE TABLE craft_products (id INTEGER, craft_id INTEGER, item_id INTEGER);
        CREATE TABLE spawn_effects (id INTEGER, owner_type_id INTEGER, sub_type INTEGER);
        CREATE TABLE effects (id INTEGER, actual_id INTEGER, actual_type TEXT);
        CREATE TABLE skill_effects (id INTEGER, skill_id INTEGER, effect_id INTEGER);
        CREATE TABLE skills (id INTEGER);
    """)
    return conn


def quest(conn, qid, category=1):
    conn.execute("INSERT INTO quest_contexts VALUES (?, 'fixture', ?, 1)", (qid, category))


def component(conn, cid, qid, step):
    conn.execute("INSERT INTO quest_components VALUES (?, ?, ?, 0, 0)", (cid, qid, step))


def act(conn, aid, cid, detail_id, cls):
    conn.execute("INSERT INTO quest_acts VALUES (?, ?, ?, ?)", (aid, cid, detail_id, cls))


def entry(unit_id, x, y, z):
    return {"UnitId": unit_id, "Position": {"X": x, "Y": y, "Z": z}}


def write_spawns(root, name, entries):
    directory = Path(root) / "main_world"
    directory.mkdir(parents=True, exist_ok=True)
    (directory / name).write_text(json.dumps(entries), encoding="utf-8")


class QuestPlanTests(unittest.TestCase):
    def test_steps_are_ordered_and_targets_resolve(self):
        with database() as conn:
            quest(conn, 1)
            component(conn, 10, 1, 2)
            component(conn, 11, 1, 4)
            component(conn, 12, 1, 6)
            component(conn, 13, 1, 8)
            conn.execute("INSERT INTO quest_act_con_accept_npcs VALUES (1, 5)")
            conn.execute("INSERT INTO quest_act_obj_monster_hunts VALUES (2, 6, 3)")
            conn.execute("INSERT INTO quest_act_con_report_npcs VALUES (3, 5)")
            act(conn, 101, 10, 1, "QuestActConAcceptNpc")
            act(conn, 102, 11, 2, "QuestActObjMonsterHunt")
            act(conn, 103, 12, 3, "QuestActConReportNpc")
            act(conn, 104, 13, 4, "QuestActSupplyItem")
            spawns = {"npc": {5: [("main_world", 10.0, 20.0, 30.0)],
                              6: [("main_world", 11.0, 20.0, 30.0)]}, "doodad": {}}
            result = plan(conn, spawns)
            quest_plan = result["quests"][0]
            self.assertEqual([s["component_kind"] for s in quest_plan["steps"]],
                             ["Start", "Progress", "Ready", "Reward"])
            accept, hunt, report, supply = quest_plan["steps"]
            self.assertEqual(accept["target"], {"kind": "npc", "ids": [5], "count": 1})
            self.assertEqual(accept["locations"],
                             [{"world": "main_world", "x": 10.0, "y": 20.0, "z": 30.0}])
            self.assertEqual((hunt["act_type"], hunt["target"]["count"]),
                             ("QuestActObjMonsterHunt", 3))
            self.assertEqual((report["status"], report["locations"][0]["x"]), ("ok", 10.0))
            self.assertEqual(supply["target"], {"kind": "none", "ids": [], "count": 1})
            self.assertEqual((quest_plan["status"], quest_plan["blockers"]), ("ok", []))

    def test_missing_spawn_blocks_only_non_optional_steps(self):
        with database() as conn:
            quest(conn, 1)
            component(conn, 10, 1, 4)
            conn.execute("INSERT INTO quest_act_obj_monster_hunts VALUES (1, 404, 1)")
            act(conn, 1, 10, 1, "QuestActObjMonsterHunt")
            quest(conn, 2)
            component(conn, 20, 2, 5)
            conn.execute("INSERT INTO quest_act_obj_monster_hunts VALUES (2, 404, 1)")
            act(conn, 2, 20, 2, "QuestActObjMonsterHunt")
            result = plan(conn, {"npc": {}, "doodad": {}})
            self.assertEqual([q["status"] for q in result["quests"]], ["blocked", "ok"])
            self.assertEqual(result["quests"][0]["blockers"],
                             ["no spawn for npc 404 (QuestActObjMonsterHunt, component 10)"])
            self.assertEqual(result["quests"][0]["steps"][0]["status"], "no_spawn")
            self.assertEqual(result["summary"]["status"], {"blocked": 1, "ok": 1})
            self.assertEqual(result["summary"]["blocked_missing_npc_spawns"], 1)

    def test_dynamic_spawn_steps_are_not_blocked(self):
        with database() as conn:
            quest(conn, 1)
            component(conn, 10, 1, 4)
            conn.execute("INSERT INTO quest_act_con_accept_npcs VALUES (1, 999)")
            act(conn, 1, 10, 1, "QuestActConAcceptNpc")
            conn.execute("INSERT INTO skills VALUES (77)")
            conn.execute("INSERT INTO effects VALUES (100, 55, 'SpawnEffect')")
            conn.execute("INSERT INTO skill_effects VALUES (1, 77, 100)")
            conn.execute("INSERT INTO spawn_effects VALUES (55, 1, 999)")
            # Control: npc 1000 has no world placement and no spawn effect.
            quest(conn, 2)
            component(conn, 20, 2, 4)
            conn.execute("INSERT INTO quest_act_con_accept_npcs VALUES (2, 1000)")
            act(conn, 2, 20, 2, "QuestActConAcceptNpc")
            result = plan(conn, {"npc": {}, "doodad": {}})
            step = result["quests"][0]["steps"][0]
            self.assertEqual(step["status"], "dynamic_spawn")
            self.assertEqual(step["sources"], [{"effect_id": 55, "skill_ids": [77]}])
            self.assertEqual(step["locations"], [])
            self.assertEqual((result["quests"][0]["status"], result["quests"][0]["blockers"]),
                             ("ok", []))
            self.assertNotIn("sources", result["quests"][1]["steps"][0])
            self.assertEqual(result["quests"][1]["status"], "blocked")
            self.assertEqual(result["summary"]["dynamic_spawn_steps"], 1)
            self.assertEqual(result["summary"]["blocked_missing_npc_spawns"], 1)

    def test_craft_zone_quest_and_item_target_mappings(self):
        with database() as conn:
            quest(conn, 1)
            component(conn, 10, 1, 4)
            conn.execute("INSERT INTO quest_act_con_accept_components VALUES (1, 42)")
            conn.execute("INSERT INTO quest_act_obj_complete_quests VALUES (2, 43)")
            conn.execute("INSERT INTO quest_act_obj_crafts VALUES (3, 500, 2)")
            conn.execute("INSERT INTO craft_products VALUES (1, 500, 60)")
            conn.execute("INSERT INTO craft_products VALUES (2, 500, 61)")
            conn.execute("INSERT INTO quest_act_obj_zone_kills VALUES (4, 0, 5, 12, 0, 0, 10, 20)")
            conn.execute("INSERT INTO quest_act_etc_item_obtains VALUES (5, 70, 3)")
            conn.execute("INSERT INTO quest_act_con_accept_item_gains VALUES (6, 71, 4)")
            conn.execute("INSERT INTO quest_act_con_report_journals VALUES (7)")
            act(conn, 1, 10, 1, "QuestActConAcceptComponent")
            act(conn, 2, 10, 2, "QuestActObjCompleteQuest")
            act(conn, 3, 10, 3, "QuestActObjCraft")
            act(conn, 4, 10, 4, "QuestActObjZoneKill")
            act(conn, 5, 10, 5, "QuestActEtcItemObtain")
            act(conn, 6, 10, 6, "QuestActConAcceptItemGain")
            act(conn, 7, 10, 7, "QuestActConReportJournal")
            result = plan(conn, {"npc": {}, "doodad": {}})
            quest_plan = result["quests"][0]
            self.assertEqual([s["target"] for s in quest_plan["steps"]], [
                {"kind": "quest", "ids": [42], "count": 1},
                {"kind": "quest", "ids": [43], "count": 1},
                {"kind": "item", "ids": [60, 61], "count": 2, "craft_id": 500},
                {"kind": "zone", "ids": [], "count": 5, "zone_id": 12,
                 "level_min": 10, "level_max": 20},
                {"kind": "item", "ids": [70], "count": 3},
                {"kind": "item", "ids": [71], "count": 4},
                {"kind": "none", "ids": [], "count": 1}])
            self.assertEqual({s["status"] for s in quest_plan["steps"]}, {"ok"})
            self.assertEqual((quest_plan["status"], result["summary"]["unsupported_act_types"]),
                             ("ok", {}))

    def test_level_cinema_aggro_and_group_target_mappings(self):
        with database() as conn:
            quest(conn, 1)
            component(conn, 10, 1, 4)
            conn.execute("INSERT INTO quest_act_obj_ability_levels VALUES (1, 2, 40)")
            conn.execute("INSERT INTO quest_act_obj_mate_levels VALUES (2, 72, 10)")
            conn.execute("INSERT INTO quest_act_obj_levels VALUES (3, 30)")
            conn.execute("INSERT INTO quest_act_con_accept_level_ups VALUES (4, 5)")
            conn.execute("INSERT INTO quest_act_obj_cinemas VALUES (5, 90)")
            conn.execute('INSERT INTO quest_act_obj_aggros VALUES (6, 300, 10, 50, 90)')
            conn.execute("INSERT INTO quest_act_obj_express_fires VALUES (7, 33, 8, 2)")
            conn.execute("INSERT INTO quest_act_obj_talk_npc_groups VALUES (8, 8)")
            conn.execute("INSERT INTO quest_act_obj_item_group_gathers VALUES (9, 3, 5)")
            conn.execute("INSERT INTO quest_act_obj_item_group_uses VALUES (10, 3, 1)")
            conn.execute("INSERT INTO quest_monster_npcs VALUES (1, 8, 21)")
            conn.execute("INSERT INTO quest_monster_npcs VALUES (2, 8, 22)")
            conn.execute("INSERT INTO quest_item_group_items VALUES (1, 3, 51)")
            conn.execute("INSERT INTO quest_item_group_items VALUES (2, 3, 52)")
            act(conn, 1, 10, 1, "QuestActObjAbilityLevel")
            act(conn, 2, 10, 2, "QuestActObjMateLevel")
            act(conn, 3, 10, 3, "QuestActObjLevel")
            act(conn, 4, 10, 4, "QuestActConAcceptLevelUp")
            act(conn, 5, 10, 5, "QuestActObjCinema")
            act(conn, 6, 10, 6, "QuestActObjAggro")
            act(conn, 7, 10, 7, "QuestActObjExpressFire")
            act(conn, 8, 10, 8, "QuestActObjTalkNpcGroup")
            act(conn, 9, 10, 9, "QuestActObjItemGroupGather")
            act(conn, 10, 10, 10, "QuestActObjItemGroupUse")
            spawns = {"npc": {21: [("main_world", 1.0, 2.0, 3.0)],
                              22: [("main_world", 4.0, 5.0, 6.0)]}, "doodad": {}}
            result = plan(conn, spawns)
            quest_plan = result["quests"][0]
            self.assertEqual([s["target"] for s in quest_plan["steps"]], [
                {"kind": "level", "ids": [], "count": 1, "level": 40, "ability_id": 2},
                {"kind": "level", "ids": [], "count": 1, "level": 10, "item_id": 72},
                {"kind": "level", "ids": [], "count": 1, "level": 30},
                {"kind": "level", "ids": [], "count": 1, "level": 5},
                {"kind": "none", "ids": [], "count": 1, "cinema_id": 90},
                {"kind": "none", "ids": [], "count": 3, "rank1": 10, "rank2": 50, "rank3": 90},
                {"kind": "npc", "ids": [21, 22], "count": 2, "emotion_id": 33},
                {"kind": "npc", "ids": [21, 22], "count": 1},
                {"kind": "item", "ids": [51, 52], "count": 5},
                {"kind": "item", "ids": [51, 52], "count": 1}])
            self.assertEqual([s["status"] for s in quest_plan["steps"]], ["ok"] * 10)
            self.assertEqual((quest_plan["status"], result["summary"]["unsupported_act_types"]),
                             ("ok", {}))

    def test_unsupported_acts_flag_partial(self):
        with database() as conn:
            quest(conn, 1)
            component(conn, 10, 1, 4)
            act(conn, 1, 10, 1, "QuestActObjEffectFire")
            act(conn, 2, 10, 2, "QuestActCheckTimer")
            result = plan(conn, {"npc": {}, "doodad": {}})
            steps = result["quests"][0]["steps"]
            self.assertEqual([s["status"] for s in steps], ["unsupported", "ok"])
            self.assertEqual(steps[0]["act_type"], "QuestActObjEffectFire")
            self.assertEqual(steps[0]["target"], {"kind": "none", "ids": [], "count": 1})
            self.assertEqual(result["quests"][0]["status"], "partial")
            self.assertEqual(result["quests"][0]["blockers"],
                             ["unsupported act QuestActObjEffectFire (act 1, component 10)"])
            self.assertEqual(result["summary"]["unsupported_act_types"],
                             {"QuestActObjEffectFire": 1})

    def test_group_hunt_and_spawner_member_indirection(self):
        with database() as conn, tempfile.TemporaryDirectory() as tmp:
            quest(conn, 1)
            component(conn, 10, 1, 4)
            conn.execute("INSERT INTO quest_act_obj_monster_group_hunts VALUES (1, 7, 2)")
            conn.execute("INSERT INTO quest_monster_npcs VALUES (1, 7, 12)")
            conn.execute("INSERT INTO quest_monster_npcs VALUES (2, 7, 13)")
            act(conn, 1, 10, 1, "QuestActObjMonsterGroupHunt")
            write_spawns(tmp, "npc_spawns.json", [entry(12, 1.0, 2.0, 3.0)])
            spawns = load_spawns(Path(tmp), {"npc": {12, 13}, "doodad": {}}, [(50, 12), (50, 13)])
            step = plan(conn, spawns)["quests"][0]["steps"][0]
            self.assertEqual(step["target"], {"kind": "npc", "ids": [12, 13], "count": 2})
            # 13 has no own spawn entry; it shares the UnitId 12 placement through
            # the cloned game-data spawner 50 (npc_spawner_npcs members).
            self.assertEqual(step["locations"],
                             [{"world": "main_world", "x": 1.0, "y": 2.0, "z": 3.0}])
            self.assertEqual(step["status"], "ok")

    def test_event_entries_do_not_expand_spawner_members(self):
        with tempfile.TemporaryDirectory() as tmp:
            write_spawns(tmp, "npc_spawns.json",
                         [{"UnitId": 20, "NpcSpawnerIds": [3],
                           "Position": {"X": 5.0, "Y": 5.0, "Z": 5.0}}])
            spawns = load_spawns(Path(tmp), {"npc": {20, 21}, "doodad": {}}, [(60, 20), (60, 21)])
            self.assertIn(20, spawns["npc"])
            self.assertNotIn(21, spawns["npc"])

    def test_split_files_are_lenient_and_deduplicated(self):
        with tempfile.TemporaryDirectory() as tmp:
            directory = Path(tmp) / "main_world"
            directory.mkdir()
            # Read first (files.reverse()): trailing comma and line comment.
            (directory / "npc_spawns_extra.json").write_text(
                '[\n  { "UnitId": 5, "Position": { "X": 1.5, "Y": 1.0, "Z": 0.0 } }, // near\n]',
                encoding="utf-8")
            # Read second: 1.0 is within 2.0 of the stored 1.5 and is dropped;
            # keys match case-insensitively (Newtonsoft).
            (directory / "npc_spawns.json").write_text(
                '[ { "unitid": 5, "position": { "x": 1.0, "y": 1.0, "z": 0.0 } },'
                ' { "UnitId": 5, "Position": { "X": 50.0, "Y": 1.0, "Z": 0.0 } } ]',
                encoding="utf-8")
            spawns = load_spawns(Path(tmp), {"npc": {5}, "doodad": {}})
            self.assertEqual(spawns["npc"][5], [("main_world", 1.5, 1.0, 0.0),
                                                ("main_world", 50.0, 1.0, 0.0)])

    def test_item_sphere_and_doodad_targets(self):
        with database() as conn:
            quest(conn, 1)
            component(conn, 10, 1, 4)
            conn.execute("INSERT INTO quest_act_obj_item_gathers VALUES (1, 100, 4)")
            conn.execute("INSERT INTO quest_act_con_accept_spheres VALUES (2, 9)")
            conn.execute("INSERT INTO quest_act_con_accept_doodads VALUES (3, 77)")
            act(conn, 1, 10, 1, "QuestActObjItemGather")
            act(conn, 2, 10, 2, "QuestActConAcceptSphere")
            act(conn, 3, 10, 3, "QuestActConAcceptDoodad")
            spawns = {"npc": {}, "doodad": {77: [("main_world", 7.0, 8.0, 9.0)]}}
            gather, sphere, doodad = plan(conn, spawns)["quests"][0]["steps"]
            self.assertEqual(gather["target"], {"kind": "item", "ids": [100], "count": 4})
            self.assertEqual((gather["status"], gather["locations"]), ("ok", []))
            self.assertEqual((sphere["target"]["kind"], sphere["status"], sphere["locations"]),
                             ("sphere", "ok", []))
            self.assertEqual((doodad["status"], doodad["locations"]),
                             ("ok", [{"world": "main_world", "x": 7.0, "y": 8.0, "z": 9.0}]))

    def test_filter_selects_requested_quests_and_skips_tutorials(self):
        with database() as conn:
            quest(conn, 1)
            quest(conn, 2)
            quest(conn, 3, category=45)
            spawns = {"npc": {}, "doodad": {}}
            self.assertEqual([q["quest_id"] for q in
                              plan(conn, spawns, quest_filter={2})["quests"]],
                             [2])
            self.assertEqual([q["quest_id"] for q in plan(conn, spawns)["quests"]], [1, 2])

    def test_closest_dedupes_and_keeps_the_anchor(self):
        locations = [("main_world", 0.0, 0.0, 0.0), ("main_world", 5.0, 0.0, 0.0),
                     ("main_world", 9.0, 0.0, 0.0), ("main_world", 1.0, 0.0, 0.0),
                     ("main_world", 2.0, 0.0, 0.0), ("main_world", 3.0, 0.0, 0.0),
                     ("main_world", 0.0, 0.0, 0.0)]
        self.assertEqual(closest(locations),
                         [("main_world", 0.0, 0.0, 0.0), ("main_world", 1.0, 0.0, 0.0),
                          ("main_world", 2.0, 0.0, 0.0), ("main_world", 3.0, 0.0, 0.0),
                          ("main_world", 5.0, 0.0, 0.0)])


if __name__ == "__main__":
    unittest.main()
