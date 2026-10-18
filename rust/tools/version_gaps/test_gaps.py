"""Unit tests for the version-gap report with small in-memory databases."""

import json
import os
import sqlite3
import sys
import tempfile
import unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

from gaps import (
    BaseIndex,
    Db,
    bucket_counts,
    build_report,
    camel_to_snake,
    client_visible,
    detail_table_candidates,
    find_item_use_skill_gaps,
    find_loot_gaps,
    find_quest_context_gaps,
    find_quest_reference_gaps,
    find_spawn_gaps,
    has_custom_ids,
    names_compatible,
    table_locations,
)

BASE_SCHEMA = """
CREATE TABLE quest_contexts (id INTEGER PRIMARY KEY, name TEXT);
CREATE TABLE quest_components (id INTEGER PRIMARY KEY, quest_context_id INTEGER,
    component_kind_id INTEGER, next_component INTEGER, npc_id INTEGER,
    skill_id INTEGER, buff_id INTEGER, npc_spawner_id INTEGER);
CREATE TABLE quest_acts (id INTEGER PRIMARY KEY, quest_component_id INTEGER,
    act_detail_id INTEGER, act_detail_type TEXT);
CREATE TABLE quest_act_con_accept_npcs (id INTEGER PRIMARY KEY, npc_id INTEGER);
CREATE TABLE quest_act_con_accept_uis (id INTEGER PRIMARY KEY);
CREATE TABLE quest_act_obj_item_gathers (id INTEGER PRIMARY KEY, item_id INTEGER,
    count INTEGER);
CREATE TABLE quest_act_supply_exps (id INTEGER PRIMARY KEY, exp INTEGER);
CREATE TABLE quest_context_texts (id INTEGER PRIMARY KEY, quest_context_id INTEGER,
    quest_context_text_kind_id INTEGER, text TEXT);
CREATE TABLE quest_component_texts (id INTEGER PRIMARY KEY, quest_component_id INTEGER,
    quest_component_text_kind_id INTEGER, text TEXT);
CREATE TABLE quest_chat_bubbles (id INTEGER PRIMARY KEY, quest_component_id INTEGER,
    npc_id INTEGER, speech TEXT);
CREATE TABLE quest_names (id INTEGER PRIMARY KEY, quest_context_id INTEGER,
    quest_name_kind_id INTEGER, name TEXT);
CREATE TABLE npcs (id INTEGER PRIMARY KEY, name TEXT, model_id INTEGER);
CREATE TABLE items (id INTEGER PRIMARY KEY, name TEXT, use_skill_id INTEGER);
CREATE TABLE skills (id INTEGER PRIMARY KEY, name TEXT);
CREATE TABLE buffs (id INTEGER PRIMARY KEY, name TEXT);
CREATE TABLE doodad_almighties (id INTEGER PRIMARY KEY, name TEXT);
CREATE TABLE actor_models (id INTEGER PRIMARY KEY, model_file TEXT);
CREATE TABLE spheres (id INTEGER PRIMARY KEY);
CREATE TABLE loots (id INTEGER PRIMARY KEY, item_id INTEGER, loot_pack_id INTEGER);
CREATE TABLE loot_pack_dropping_npcs (id INTEGER PRIMARY KEY, npc_id INTEGER,
    loot_pack_id INTEGER);
CREATE TABLE doodad_func_loot_packs (id INTEGER PRIMARY KEY, loot_pack_id INTEGER);
CREATE TABLE npc_spawners (id INTEGER PRIMARY KEY, name TEXT);
CREATE TABLE npc_spawner_npcs (id INTEGER PRIMARY KEY, npc_spawner_id INTEGER,
    member_id INTEGER, member_type TEXT);
CREATE TABLE quest_monster_groups (id INTEGER PRIMARY KEY, name TEXT);
CREATE TABLE quest_monster_npcs (id INTEGER PRIMARY KEY, quest_monster_group_id INTEGER,
    npc_id INTEGER);
CREATE TABLE skill_effects (id INTEGER PRIMARY KEY, skill_id INTEGER, effect_id INTEGER);
"""

REF_SCHEMA = """
CREATE TABLE quest_contexts (id INTEGER PRIMARY KEY, name TEXT);
CREATE TABLE quest_components (id INTEGER PRIMARY KEY, quest_context_id INTEGER,
    component_kind_id INTEGER, next_component INTEGER, npc_id INTEGER,
    skill_id INTEGER, buff_id INTEGER, npc_spawner_id INTEGER);
CREATE TABLE quest_acts (id INTEGER PRIMARY KEY, quest_component_id INTEGER,
    act_detail_id INTEGER, act_detail_type TEXT);
CREATE TABLE quest_act_con_accept_npcs (id INTEGER PRIMARY KEY, npc_id INTEGER);
CREATE TABLE quest_act_obj_item_gathers (id INTEGER PRIMARY KEY, item_id INTEGER,
    count INTEGER);
CREATE TABLE quest_context_texts (id INTEGER PRIMARY KEY, quest_context_id INTEGER,
    quest_context_text_kind_id INTEGER, text TEXT);
CREATE TABLE quest_component_texts (id INTEGER PRIMARY KEY, quest_component_id INTEGER,
    quest_component_text_kind_id INTEGER, text TEXT);
CREATE TABLE quest_chat_bubbles (id INTEGER PRIMARY KEY, quest_component_id INTEGER,
    npc_id INTEGER, speech TEXT);
CREATE TABLE quest_names (id INTEGER PRIMARY KEY, quest_context_id INTEGER,
    quest_name_kind_id INTEGER, name TEXT);
CREATE TABLE npcs (id INTEGER PRIMARY KEY, name TEXT, model_id INTEGER);
CREATE TABLE items (id INTEGER PRIMARY KEY, name TEXT, use_skill_id INTEGER);
CREATE TABLE skills (id INTEGER PRIMARY KEY, name TEXT);
CREATE TABLE actor_models (id INTEGER PRIMARY KEY, model_file TEXT);
CREATE TABLE doodad_func_loot_packs (id INTEGER PRIMARY KEY, loot_pack_id INTEGER);
CREATE TABLE skill_effects (id INTEGER PRIMARY KEY, skill_id INTEGER, effect_id INTEGER);
"""

REFSRV_SCHEMA = """
CREATE TABLE loots (id INTEGER PRIMARY KEY, item_id INTEGER, loot_pack_id INTEGER);
CREATE TABLE loot_pack_dropping_npcs (id INTEGER PRIMARY KEY, npc_id INTEGER,
    loot_pack_id INTEGER);
CREATE TABLE npc_spawners (id INTEGER PRIMARY KEY, name TEXT);
CREATE TABLE npc_spawner_npcs (id INTEGER PRIMARY KEY, npc_spawner_id INTEGER,
    member_id INTEGER, member_type TEXT);
"""


def make_db(schema, rows):
    conn = sqlite3.connect(":memory:")
    conn.row_factory = sqlite3.Row
    conn.executescript(schema)
    for statement, values in rows:
        conn.execute(statement, values)
    return Db(conn=conn)


def base_db():
    rows = [
        ("INSERT INTO quest_contexts VALUES (?, ?)", (100, "Base quest")),
        ("INSERT INTO quest_contexts VALUES (?, ?)", (150, "Same quest")),
        ("INSERT INTO quest_contexts VALUES (?, ?)", (200, "Orphan comps")),
        ("INSERT INTO quest_contexts VALUES (?, ?)", (300, "Empty both")),
        ("INSERT INTO quest_contexts VALUES (?, ?)", (400, "Referenced quest")),
        ("INSERT INTO quest_contexts VALUES (?, ?)", (410, "Mismatch quest")),
        ("INSERT INTO quest_components VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
         (4000, 400, 2, 0, 0, 0, 0, 0)),
        ("INSERT INTO quest_components VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
         (4010, 410, 2, 0, 0, 0, 0, 0)),
        ("INSERT INTO quest_acts VALUES (?, ?, ?, ?)",
         (4001, 4000, 5001, "QuestActConAcceptNpc")),
        ("INSERT INTO quest_acts VALUES (?, ?, ?, ?)",
         (4002, 4000, 5002, "QuestActObjItemGather")),
        ("INSERT INTO quest_acts VALUES (?, ?, ?, ?)",
         (4004, 4000, 5004, "QuestActConAcceptNpc")),
        ("INSERT INTO quest_acts VALUES (?, ?, ?, ?)",
         (4011, 4010, 5011, "QuestActConAcceptNpc")),
        ("INSERT INTO quest_act_con_accept_npcs VALUES (?, ?)", (5001, 777)),
        ("INSERT INTO quest_act_con_accept_npcs VALUES (?, ?)", (5004, 600)),
        ("INSERT INTO quest_act_con_accept_npcs VALUES (?, ?)", (5011, 779)),
        ("INSERT INTO quest_act_obj_item_gathers VALUES (?, ?, ?)", (5002, 888, 1)),
        ("INSERT INTO quest_chat_bubbles VALUES (?, ?, ?, ?)",
         (9001, 4000, 777, "speech")),
        ("INSERT INTO npcs VALUES (?, ?, ?)", (1000, "Known npc", 10)),
        ("INSERT INTO npcs VALUES (?, ?, ?)", (601, "Spawned npc", 10)),
        ("INSERT INTO npcs VALUES (?, ?, ?)", (602, "Group npc", 10)),
        ("INSERT INTO npcs VALUES (?, ?, ?)", (500, "Drop npc", 11)),
        ("INSERT INTO npcs VALUES (?, ?, ?)", (502, "Droppy npc", 11)),
        ("INSERT INTO npcs VALUES (?, ?, ?)", (503, "Custom drop npc", 11)),
        ("INSERT INTO npcs VALUES (?, ?, ?)", (6499, "Known target", 10)),
        ("INSERT INTO items VALUES (?, ?, ?)", (2000, "Known item", 0)),
        ("INSERT INTO items VALUES (?, ?, ?)", (50, "Item 50", 900)),
        ("INSERT INTO items VALUES (?, ?, ?)", (51, "Item 51", 901)),
        ("INSERT INTO items VALUES (?, ?, ?)", (52, "Item 52", 902)),
        ("INSERT INTO actor_models VALUES (?, ?)", (10, "model10")),
        ("INSERT INTO loots VALUES (?, ?, ?)", (1, 2000, 10)),
        ("INSERT INTO loots VALUES (?, ?, ?)", (2, 2000, 21)),
        ("INSERT INTO loot_pack_dropping_npcs VALUES (?, ?, ?)", (100, 500, 11)),
        ("INSERT INTO loot_pack_dropping_npcs VALUES (?, ?, ?)", (101, 502, 21)),
        ("INSERT INTO doodad_func_loot_packs VALUES (?, ?)", (200, 12)),
        ("INSERT INTO npc_spawners VALUES (?, ?)", (1, "base spawner")),
        ("INSERT INTO npc_spawner_npcs VALUES (?, ?, ?, ?)", (10, 1, 601, "Npc")),
        ("INSERT INTO npc_spawner_npcs VALUES (?, ?, ?, ?)", (11, 1, 5, "NpcGroup")),
        ("INSERT INTO quest_monster_groups VALUES (?, ?)", (5, "group")),
        ("INSERT INTO quest_monster_npcs VALUES (?, ?, ?)", (1, 5, 602)),
    ]
    return make_db(BASE_SCHEMA, rows)


def ref_db():
    rows = [
        ("INSERT INTO quest_contexts VALUES (?, ?)", (100, "Ref quest renamed")),
        ("INSERT INTO quest_contexts VALUES (?, ?)", (150, "Same quest")),
        ("INSERT INTO quest_contexts VALUES (?, ?)", (300, "Empty both")),
        ("INSERT INTO quest_contexts VALUES (?, ?)", (400, "Referenced quest")),
        ("INSERT INTO quest_contexts VALUES (?, ?)", (410, "Mismatch quest renamed")),
        ("INSERT INTO quest_components VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
         (6000, 100, 2, 0, 0, 0, 0, 0)),
        ("INSERT INTO quest_components VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
         (6100, 200, 2, 0, 0, 0, 0, 0)),
        ("INSERT INTO quest_components VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
         (6200, 150, 2, 0, 0, 0, 0, 0)),
        ("INSERT INTO quest_acts VALUES (?, ?, ?, ?)",
         (7000, 6000, 8000, "QuestActConAcceptNpc")),
        ("INSERT INTO quest_acts VALUES (?, ?, ?, ?)",
         (7200, 6200, 8200, "QuestActConAcceptNpc")),
        ("INSERT INTO quest_act_con_accept_npcs VALUES (?, ?)", (8000, 6499)),
        ("INSERT INTO quest_act_con_accept_npcs VALUES (?, ?)", (8200, 6499)),
        ("INSERT INTO quest_component_texts VALUES (?, ?, ?, ?)",
         (8100, 6000, 1, "component text")),
        ("INSERT INTO quest_context_texts VALUES (?, ?, ?, ?)",
         (8200, 100, 1, "context text")),
        ("INSERT INTO quest_chat_bubbles VALUES (?, ?, ?, ?)",
         (8300, 6000, 6499, "ref speech")),
        ("INSERT INTO quest_names VALUES (?, ?, ?, ?)", (8400, 100, 1, "Ref name")),
        ("INSERT INTO npcs VALUES (?, ?, ?)", (6499, "Known target", 10)),
        ("INSERT INTO npcs VALUES (?, ?, ?)", (777, "Ref only npc", 10)),
        ("INSERT INTO npcs VALUES (?, ?, ?)", (779, "Ref only npc 779", 10)),
        ("INSERT INTO npcs VALUES (?, ?, ?)", (500, "Drop npc", 11)),
        ("INSERT INTO npcs VALUES (?, ?, ?)", (502, "Droppy npc", 11)),
        ("INSERT INTO npcs VALUES (?, ?, ?)", (503, "Custom drop npc", 11)),
        ("INSERT INTO items VALUES (?, ?, ?)", (50, "Item 50", 950)),
        ("INSERT INTO items VALUES (?, ?, ?)", (51, "Item 51", 901)),
        ("INSERT INTO items VALUES (?, ?, ?)", (52, "Item 52 renamed", 902)),
        ("INSERT INTO items VALUES (?, ?, ?)", (888, "Ref only item", 0)),
        ("INSERT INTO skills VALUES (?, ?)", (950, "New skill")),
        ("INSERT INTO skills VALUES (?, ?)", (902, "Existing ref skill")),
        ("INSERT INTO skill_effects VALUES (?, ?, ?)", (9500, 950, 1)),
        ("INSERT INTO actor_models VALUES (?, ?)", (10, "model10")),
        ("INSERT INTO doodad_func_loot_packs VALUES (?, ?)", (300, 12)),
    ]
    return make_db(REF_SCHEMA, rows)


def refsrv_db():
    rows = [
        ("INSERT INTO loots VALUES (?, ?, ?)", (10, 2000, 11)),
        ("INSERT INTO loots VALUES (?, ?, ?)", (11, 2000, 20)),
        ("INSERT INTO loots VALUES (?, ?, ?)", (12, 2000, 12)),
        ("INSERT INTO loots VALUES (?, ?, ?)", (13, 2000, 8000033)),
        ("INSERT INTO loot_pack_dropping_npcs VALUES (?, ?, ?)", (1000, 500, 20)),
        ("INSERT INTO loot_pack_dropping_npcs VALUES (?, ?, ?)", (1001, 501, 20)),
        ("INSERT INTO loot_pack_dropping_npcs VALUES (?, ?, ?)", (1002, 502, 20)),
        ("INSERT INTO loot_pack_dropping_npcs VALUES (?, ?, ?)", (1003, 503, 8000033)),
        ("INSERT INTO npc_spawners VALUES (?, ?)", (9000, "new spawner")),
        ("INSERT INTO npc_spawners VALUES (?, ?)", (8000012, "custom spawner")),
        ("INSERT INTO npc_spawner_npcs VALUES (?, ?, ?, ?)", (9000, 9000, 600, "Npc")),
        ("INSERT INTO npc_spawner_npcs VALUES (?, ?, ?, ?)", (9002, 1, 700, "Npc")),
        ("INSERT INTO npc_spawner_npcs VALUES (?, ?, ?, ?)",
         (8000012, 8000012, 8000012, "Npc")),
    ]
    return make_db(REFSRV_SCHEMA, rows)


class VersionGapTests(unittest.TestCase):
    def setUp(self):
        self.base = base_db()
        self.ref = ref_db()
        self.refsrv = refsrv_db()
        self.index = BaseIndex(self.base)

    def test_quest_context_gap_found_and_orphan_skipped(self):
        entries, skipped = find_quest_context_gaps(self.base, self.ref, self.index)
        by_id = {e["ids"]["quest_context_id"]: e for e in entries}
        self.assertEqual(sorted(by_id), [100, 150])
        entry = by_id[100]
        self.assertEqual(entry["base"]["name"], "Base quest")
        self.assertEqual(entry["ref"]["name"], "Ref quest renamed")
        self.assertTrue(entry["name_mismatch"])
        self.assertFalse(entry["custom_origin"])
        self.assertEqual(entry["ref"]["components"], 1)
        self.assertEqual(entry["ref"]["acts"], 1)
        self.assertEqual(entry["ref"]["detail_rows"], 1)
        self.assertEqual(entry["ref"]["detail_tables"], ["quest_act_con_accept_npcs"])
        tables = {row["table"] for row in entry["borrowed"]}
        self.assertEqual(
            tables,
            {
                "quest_components",
                "quest_acts",
                "quest_act_con_accept_npcs",
                "quest_component_texts",
                "quest_context_texts",
                "quest_chat_bubbles",
                "quest_names",
            },
        )
        self.assertFalse(entry["client_visible"])
        same = by_id[150]
        self.assertFalse(same["name_mismatch"])
        self.assertFalse(same["custom_origin"])
        counts = bucket_counts(entries)
        self.assertEqual(counts, {"default": 1, "custom": 0, "name_mismatch": 1})
        self.assertEqual([s["ids"]["quest_context_id"] for s in skipped], [200])
        self.assertIn("orphaned", skipped[0]["reason"])

    def test_quest_context_both_empty_not_reported(self):
        entries, _ = find_quest_context_gaps(self.base, self.ref, self.index)
        self.assertNotIn(300, [e["ids"]["quest_context_id"] for e in entries])

    def test_item_use_skill_gap_cases(self):
        entries = find_item_use_skill_gaps(self.base, self.ref, self.index)
        by_item = {e["ids"]["item_id"]: e for e in entries}
        self.assertEqual(set(by_item), {50, 52})
        self.assertEqual(by_item[50]["ids"]["base_skill_id"], 900)
        self.assertEqual(by_item[50]["ids"]["ref_skill_id"], 950)
        self.assertEqual(by_item[50]["ref"]["skill_name"], "New skill")
        self.assertTrue(by_item[50]["client_visible"])
        self.assertFalse(by_item[50]["name_mismatch"])
        self.assertFalse(by_item[50]["custom_origin"])
        self.assertEqual(
            [row["table"] for row in by_item[50]["borrowed"]],
            ["skills", "skill_effects"],
        )
        self.assertEqual(by_item[52]["ids"]["base_skill_id"], 902)
        self.assertEqual(by_item[52]["ids"]["ref_skill_id"], 902)
        self.assertTrue(by_item[52]["name_mismatch"])
        self.assertNotIn(51, by_item)
        counts = bucket_counts(entries)
        self.assertEqual(counts, {"default": 1, "custom": 0, "name_mismatch": 1})

    def test_quest_reference_gap_npc_and_item(self):
        entries = find_quest_reference_gaps(self.base, self.ref, self.index)
        by_target = {(e["ids"]["target_kind"], e["ids"]["target_id"]): e for e in entries}
        self.assertEqual(
            set(by_target), {("npcs", 777), ("npcs", 779), ("items", 888)}
        )
        npc_entry = by_target[("npcs", 777)]
        sources = {ref["table"] for ref in npc_entry["base"]["references"]}
        self.assertEqual(sources, {"quest_act_con_accept_npcs", "quest_chat_bubbles"})
        self.assertEqual(npc_entry["ref"]["name"], "Ref only npc")
        self.assertEqual(npc_entry["borrowed"][0]["table"], "npcs")
        self.assertTrue(npc_entry["client_visible"])
        self.assertFalse(npc_entry["name_mismatch"])
        self.assertFalse(npc_entry["custom_origin"])
        self.assertEqual(npc_entry["name_check"][0]["quest_context_id"], 400)
        self.assertTrue(npc_entry["name_check"][0]["match"])
        mismatch = by_target[("npcs", 779)]
        self.assertTrue(mismatch["name_mismatch"])
        self.assertEqual(
            mismatch["name_check"][0]["base_name"], "Mismatch quest"
        )
        self.assertEqual(
            mismatch["name_check"][0]["ref_name"], "Mismatch quest renamed"
        )
        item_entry = by_target[("items", 888)]
        self.assertEqual(item_entry["borrowed"][0]["table"], "items")
        self.assertFalse(item_entry["name_mismatch"])
        self.assertEqual(
            item_entry["base"]["references"][0]["acts"][0]["quest_context_id"], 400
        )
        counts = bucket_counts(entries)
        self.assertEqual(counts, {"default": 2, "custom": 0, "name_mismatch": 1})

    def test_loot_gap_empty_pack_and_npc_without_drops(self):
        loot = find_loot_gaps(self.base, self.ref, self.refsrv, self.index)
        packs = sorted(e["ids"]["loot_pack_id"] for e in loot["empty_loot_packs"])
        self.assertEqual(packs, [11, 12])
        self.assertEqual(loot["examined"]["loot_pack_dropping_npcs"], 1)
        self.assertEqual(loot["examined"]["doodad_func_loot_packs"], 1)
        self.assertFalse(loot["empty_loot_packs"][0]["client_visible"])
        self.assertFalse(loot["empty_loot_packs"][0]["custom_origin"])
        npcs = [e["ids"]["npc_id"] for e in loot["npcs_without_drops"]]
        self.assertEqual(npcs, [500, 503])
        by_id = {e["ids"]["npc_id"]: e for e in loot["npcs_without_drops"]}
        self.assertEqual(by_id[500]["ref"]["filled_packs"], [20])
        self.assertFalse(by_id[500]["custom_origin"])
        self.assertFalse(by_id[500]["name_mismatch"])
        self.assertEqual(by_id[503]["ref"]["filled_packs"], [8000033])
        self.assertTrue(by_id[503]["custom_origin"])
        counts = bucket_counts(loot["npcs_without_drops"])
        self.assertEqual(counts, {"default": 1, "custom": 1, "name_mismatch": 0})
        self.assertEqual({s["ids"]["npc_id"] for s in loot["skipped"]}, {501})

    def test_spawn_gap_quest_npc(self):
        spawn = find_spawn_gaps(self.base, self.ref, self.refsrv, self.index)
        gaps = spawn["quest_npc_spawn_gaps"]
        self.assertEqual([e["ids"]["npc_id"] for e in gaps], [600])
        self.assertEqual(gaps[0]["ref"]["spawner_ids"], [9000])
        self.assertTrue(gaps[0]["client_visible"])
        self.assertFalse(gaps[0]["custom_origin"])
        diffs = spawn["spawn_data_differences"]
        by_spawner = {d["ids"]["npc_spawner_id"]: d for d in diffs}
        self.assertEqual(set(by_spawner), {9000, 1, 8000012})
        self.assertTrue(by_spawner[9000]["quest_linked"])
        self.assertFalse(by_spawner[1]["quest_linked"])
        self.assertFalse(by_spawner[9000]["custom_origin"])
        self.assertFalse(by_spawner[1]["custom_origin"])
        self.assertTrue(by_spawner[8000012]["custom_origin"])
        counts = bucket_counts(diffs)
        self.assertEqual(counts, {"default": 2, "custom": 1, "name_mismatch": 0})

    def test_custom_id_detection(self):
        self.assertTrue(has_custom_ids([{"table": "npcs", "id": 8000012, "row": {}}]))
        self.assertTrue(
            has_custom_ids(
                [{"table": "loots", "id": 1, "row": {"loot_pack_id": 8000033}}]
            )
        )
        self.assertTrue(
            has_custom_ids(
                [{"table": "npc_spawner_npcs", "id": 2, "row": {"member_id": 8000012}}]
            )
        )
        self.assertFalse(
            has_custom_ids(
                [
                    {
                        "table": "loots",
                        "id": 1,
                        "row": {"drop_rate": 10000000, "item_id": 2000},
                    }
                ]
            )
        )
        self.assertFalse(has_custom_ids([{"table": "skills", "id": 34430, "row": {}}]))
        self.assertFalse(has_custom_ids([]))

    def test_client_visible_helper(self):
        self.assertTrue(
            client_visible([{"table": "skills", "id": 950, "row": {}}], self.index)
        )
        self.assertFalse(
            client_visible(
                [{"table": "loots", "id": 1, "row": {"item_id": 2000}}], self.index
            )
        )
        self.assertTrue(
            client_visible(
                [{"table": "npcs", "id": 6499, "row": {"model_id": 99}}], self.index
            )
        )
        self.assertTrue(
            client_visible(
                [
                    {
                        "table": "npc_spawner_npcs",
                        "id": 1,
                        "row": {"member_id": 700, "member_type": "Npc"},
                    }
                ],
                self.index,
            )
        )

    def test_detail_table_resolution(self):
        self.assertEqual(camel_to_snake("QuestActConAcceptNpc"), "quest_act_con_accept_npc")
        candidates = detail_table_candidates("QuestActSupplyActability")
        self.assertIn("quest_act_supply_actabilities", candidates)
        self.assertEqual(
            self.base.resolve_detail_table("QuestActSupplyExp"), "quest_act_supply_exps"
        )
        self.assertEqual(
            self.base.resolve_detail_table("QuestActConAcceptUi"),
            "quest_act_con_accept_uis",
        )
        self.assertIsNone(self.base.resolve_detail_table("QuestActObjZoneQuestComplete"))

    def test_names_compatible(self):
        self.assertTrue(names_compatible("Same Name", "same  name"))
        self.assertFalse(names_compatible("Base quest", "Ref quest renamed"))
        self.assertFalse(names_compatible("", "x"))

    def test_table_locations(self):
        locations = table_locations(self.base, self.ref, self.refsrv)
        self.assertTrue(locations["loots"]["base_compact"])
        self.assertFalse(locations["loots"]["ref_compact"])
        self.assertTrue(locations["loots"]["ref_server_table"])
        self.assertTrue(locations["doodad_func_loot_packs"]["ref_compact"])
        self.assertFalse(locations["doodad_func_loot_packs"]["ref_server_table"])
        self.assertTrue(locations["npc_spawners"]["ref_server_table"])

    def test_build_report_from_files(self):
        with tempfile.TemporaryDirectory() as tmp:
            base_path = os.path.join(tmp, "base.sqlite3")
            ref_path = os.path.join(tmp, "ref.sqlite3")
            refsrv_path = os.path.join(tmp, "refsrv.sqlite3")
            for path, source in (
                (base_path, base_db()),
                (ref_path, ref_db()),
                (refsrv_path, refsrv_db()),
            ):
                source.conn.commit()
                target = sqlite3.connect(path)
                source.conn.backup(target)
                target.close()
            report = build_report(base_path, ref_path, refsrv_path)
            summary = report["summary"]
            self.assertEqual(report["rules"]["custom_id_min"], 8000000)
            self.assertEqual(
                summary["kind1_quest_contexts_without_components"],
                {"default": 1, "custom": 0, "name_mismatch": 1},
            )
            self.assertEqual(
                summary["kind2_item_use_skill_missing"],
                {"default": 1, "custom": 0, "name_mismatch": 1},
            )
            self.assertEqual(
                summary["kind3_quest_reference_missing_target"],
                {"default": 2, "custom": 0, "name_mismatch": 1},
            )
            self.assertEqual(
                summary["kind4a_empty_loot_packs_filled_in_ref"],
                {"default": 2, "custom": 0, "name_mismatch": 0},
            )
            self.assertEqual(
                summary["kind4b_npcs_without_drops_filled_in_ref"],
                {"default": 1, "custom": 1, "name_mismatch": 0},
            )
            self.assertEqual(
                summary["kind5_quest_npc_without_spawner"],
                {"default": 1, "custom": 0, "name_mismatch": 0},
            )
            self.assertEqual(
                summary["kind5_spawn_data_differences"],
                {"default": 2, "custom": 1, "name_mismatch": 0},
            )
            serialized = json.dumps(report, ensure_ascii=False)
            self.assertIn("quest_context_without_components", serialized)
            self.assertIn("custom_origin", serialized)
            self.assertIn("name_mismatch", serialized)


if __name__ == "__main__":
    unittest.main()
