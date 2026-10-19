"""Player-order tests with miniature quest, zone, spawn and level data."""

import os
from pathlib import Path
import sqlite3
import unittest

from player_order import (
    Orderer,
    box_distance,
    level_for_xp,
    load_quest_supplies,
    load_quest_xp,
    load_spawns,
    neighbours_of,
    quest_state,
    round_even,
    xp_for_level,
)


def database():
    conn = sqlite3.connect(":memory:")
    conn.row_factory = sqlite3.Row
    conn.executescript("""
        CREATE TABLE quest_contexts (id INTEGER, name TEXT, category_id INTEGER,
            LEVEL INTEGER, chapter_idx INTEGER, quest_idx INTEGER, zone_id INTEGER,
            let_it_done TEXT);
        CREATE TABLE quest_components (id INTEGER, quest_context_id INTEGER,
            component_kind_id INTEGER, or_unit_reqs TEXT);
        CREATE TABLE unit_reqs (id INTEGER, owner_type TEXT, owner_id INTEGER,
            kind_id INTEGER, value1 INTEGER, value2 INTEGER);
        CREATE TABLE quest_acts (id INTEGER, quest_component_id INTEGER,
            act_detail_id INTEGER, act_detail_type TEXT);
        CREATE TABLE quest_act_con_accept_npcs (id INTEGER, npc_id INTEGER);
        CREATE TABLE quest_act_con_accept_level_ups (id INTEGER, LEVEL INTEGER);
        CREATE TABLE quest_act_supply_exps (id INTEGER, exp INTEGER);
        CREATE TABLE quest_supplies (id INTEGER, LEVEL INTEGER, exp INTEGER,
            copper INTEGER);
        CREATE TABLE levels (id INTEGER, total_exp INTEGER);
        CREATE TABLE zones (id INTEGER, group_id INTEGER);
        CREATE TABLE zone_groups (id INTEGER, x REAL, y REAL, w REAL, h REAL);
    """)
    return conn


def quest(conn, qid, name, category, level, chapter=0, quest_idx=0, zone=1,
          let_it_done=False):
    conn.execute("INSERT INTO quest_contexts VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
                 (qid, name, category, level, chapter, quest_idx, zone,
                  "t" if let_it_done else "f"))


def component(conn, cid, qid, kind=2, or_group=False):
    conn.execute("INSERT INTO quest_components VALUES (?, ?, ?, ?)",
                 (cid, qid, kind, "t" if or_group else "f"))


def req(conn, rid, owner, kind, value1, value2=0):
    conn.execute("INSERT INTO unit_reqs VALUES (?, 'QuestComponent', ?, ?, ?, ?)",
                 (rid, owner, kind, value1, value2))


def act(conn, aid, cid, detail_id, act_type):
    conn.execute("INSERT INTO quest_acts VALUES (?, ?, ?, ?)",
                 (aid, cid, detail_id, act_type))


def accept_npc(conn, aid, cid, npc_id):
    conn.execute("INSERT INTO quest_act_con_accept_npcs VALUES (?, ?)", (aid, npc_id))
    act(conn, aid, cid, aid, "QuestActConAcceptNpc")


def level_up(conn, aid, cid, level):
    conn.execute("INSERT INTO quest_act_con_accept_level_ups VALUES (?, ?)",
                 (aid, level))
    act(conn, aid, cid, aid, "QuestActConAcceptLevelUp")


def supply_xp(conn, aid, cid, exp):
    conn.execute("INSERT INTO quest_act_supply_exps VALUES (?, ?)", (aid, exp))
    act(conn, aid, cid, aid, "QuestActSupplyExp")


def level_row(conn, lid, total_exp):
    conn.execute("INSERT INTO levels VALUES (?, ?)", (lid, total_exp))


def level_supply(conn, level, exp, copper=0):
    conn.execute("INSERT INTO quest_supplies VALUES (?, ?, ?, ?)",
                 (level, level, exp, copper))


def zone(conn, zid, group):
    conn.execute("INSERT INTO zones VALUES (?, ?)", (zid, group))


def group(conn, gid, x, y, w, h):
    conn.execute("INSERT INTO zone_groups VALUES (?, ?, ?, ?, ?)", (gid, x, y, w, h))


def spawns(*positions):
    return {"npc": {npc: [("main_world", x, y, z)] for npc, (x, y, z) in positions},
            "doodad": {}, "gates": {}}


def levels_1_to_6(conn):
    for lid, total in [(1, 0), (2, 100), (3, 300), (4, 600), (5, 1000), (6, 1500)]:
        level_row(conn, lid, total)


def make_orderer(conn, spawn_data, **kwargs):
    kwargs.setdefault("start_level", 1)
    kwargs.setdefault("max_level", 5)
    kwargs.setdefault("neighbor_distance", 3000.0)
    return Orderer(conn, spawn_data, "nuian", **kwargs)


class PlayerOrderTests(unittest.TestCase):
    def test_story_prologue_side_zone_and_race_selection(self):
        with database() as conn:
            levels_1_to_6(conn)
            group(conn, 1, 0.0, 0.0, 1000.0, 1000.0)
            group(conn, 5, 0.0, 0.0, 2000.0, 1000.0)
            group(conn, 2, 50000.0, 50000.0, 1000.0, 1000.0)
            zone(conn, 1, 1)
            zone(conn, 9, 5)
            zone(conn, 125, 5)
            zone(conn, 500, 2)

            # Prologue: Nuian mission 100, shared auto 101, Elf mission 102,
            # shared follow-up 103.
            quest(conn, 100, "prologue nuian", 108, 1, zone=1)
            component(conn, 1000, 100)
            req(conn, 1, 1000, 3, 1)
            accept_npc(conn, 1, 1000, 700)
            quest(conn, 101, "prologue auto", 108, 1, zone=1)
            component(conn, 1001, 101)
            act(conn, 2, 1001, 0, "QuestActConAutoComplete")
            quest(conn, 102, "prologue elf", 108, 1, zone=1)
            component(conn, 1002, 102)
            req(conn, 2, 1002, 3, 4)
            accept_npc(conn, 3, 1002, 701)
            quest(conn, 103, "prologue shared", 108, 1, zone=1)
            component(conn, 1003, 103)
            req(conn, 3, 1003, 31, 100)
            accept_npc(conn, 4, 1003, 702)

            # Story chain 10 -> 11 -> 12.
            for qid, name, level in [(10, "story 1", 2), (11, "story 2", 4),
                                     (12, "story 3", 5)]:
                quest(conn, qid, name, 3, level, chapter=1,
                      quest_idx=1 + qid - 10, zone=9)
                component(conn, 2000 + qid, qid)
                req(conn, 100 + qid, 2000 + qid, 3, 1)
                accept_npc(conn, 200 + qid, 2000 + qid, 800 + qid)
            req(conn, 301, 2011, 31, 10)
            req(conn, 302, 2012, 31, 11)

            # Side quests: local, same group, far zone, above level, wrong race,
            # no player starter, and a chained prerequisite.
            for qid, name, level, zid, npc in [
                (200, "local side", 1, 9, 500),
                (201, "same group side", 2, 125, 501),
                (202, "far side", 3, 500, 502),
                (203, "above level side", 5, 9, 503),
                (204, "wrong race side", 3, 9, 504),
                (205, "no npc side", 2, 9, None),
                (210, "chained side", 1, 9, 505),
                (211, "absent prereq side", 1, 9, 506),
            ]:
                quest(conn, qid, name, 4, level, zone=zid)
                component(conn, 3000 + qid, qid)
                if npc is not None:
                    accept_npc(conn, 300 + qid, 3000 + qid, npc)
            req(conn, 400, 3000 + 210, 31, 200)  # 210 requires 200
            req(conn, 401, 3000 + 211, 31, 999)  # 211 requires an absent quest
            req(conn, 402, 3000 + 204, 3, 4)     # 204 is Elf-only

            spawn_data = spawns(
                (500, (10.0, 10.0, 0.0)),
                (501, (1010.0, 10.0, 0.0)),
                (502, (50010.0, 50010.0, 0.0)),
                (503, (20.0, 20.0, 0.0)),
                (504, (30.0, 30.0, 0.0)),
                (505, (40.0, 40.0, 0.0)),
                (506, (50.0, 50.0, 0.0)),
                (700, (0.0, 0.0, 0.0)),
                (701, (1.0, 1.0, 0.0)),
                (702, (2.0, 2.0, 0.0)),
                (810, (10.0, 10.0, 0.0)),
                (811, (10.0, 10.0, 0.0)),
                (812, (10.0, 10.0, 0.0)),
            )
            result = make_orderer(conn, spawn_data).run()
            ids = [q["quest_id"] for q in result["quests"]]
            by_source = {}
            for q in result["quests"]:
                by_source.setdefault(q["source"], []).append(q["quest_id"])

            self.assertEqual(by_source["prologue"], [100, 101, 103])
            self.assertEqual(by_source["story"], [10, 11, 12])
            self.assertEqual(ids.index(10) < ids.index(11) < ids.index(12), True)
            self.assertIn(200, ids)
            self.assertIn(201, ids)
            self.assertIn(210, ids)
            self.assertLess(ids.index(200), ids.index(210))
            self.assertNotIn(202, ids)  # far zone
            self.assertNotIn(203, ids)  # LEVEL 5 > 1 + 2
            self.assertNotIn(204, ids)  # wrong race
            self.assertNotIn(205, ids)  # no player starter
            self.assertNotIn(211, ids)  # absent prerequisite

            report = result["report"]
            self.assertEqual(report["excluded"]["prologue_other_race"], [102])
            self.assertIn(202, report["excluded"]["side_out_of_zone"])
            self.assertIn(203, report["excluded"]["side_above_final_level"])
            self.assertIn(205, report["excluded"]["side_without_player_starter"])
            never = {e["quest_id"]: e["reason"]
                     for e in report["never_available"]["side"]}
            self.assertEqual(never.get(204), "race_mismatch")
            self.assertEqual(never.get(211), "prereq_absent")

    def test_quest_xp_moves_the_level_and_opens_side_quests(self):
        with database() as conn:
            levels_1_to_6(conn)
            group(conn, 5, 0.0, 0.0, 2000.0, 1000.0)
            zone(conn, 9, 5)

            quest(conn, 10, "story with xp", 3, 1, chapter=1, quest_idx=1, zone=9)
            component(conn, 1000, 10)
            req(conn, 1, 1000, 3, 1)
            accept_npc(conn, 1, 1000, 800)
            component(conn, 1001, 10, kind=8)
            supply_xp(conn, 2, 1001, 1000)  # level 5

            quest(conn, 11, "story after", 3, 6, chapter=1, quest_idx=2, zone=9)
            component(conn, 1002, 11)
            req(conn, 2, 1002, 3, 1)
            accept_npc(conn, 3, 1002, 801)

            quest(conn, 200, "level 5 side", 4, 5, zone=9)
            component(conn, 1003, 200)
            accept_npc(conn, 4, 1003, 500)

            result = make_orderer(conn, spawns(
                (800, (0.0, 0.0, 0.0)), (801, (1.0, 1.0, 0.0)),
                (500, (2.0, 2.0, 0.0)),
            ), max_level=6).run()
            by_id = {q["quest_id"]: q for q in result["quests"]}
            self.assertEqual(by_id[10]["level_before"], 1)
            self.assertEqual(by_id[10]["level_after"], 5)
            self.assertEqual(by_id[10]["xp"], 1000)
            self.assertIn(200, by_id)
            self.assertEqual(by_id[200]["level_before"], 5)
            self.assertEqual(by_id[200]["level_after"], 5)
            self.assertLess([q["quest_id"] for q in result["quests"]].index(10),
                            [q["quest_id"] for q in result["quests"]].index(200))
            self.assertEqual(result["report"]["summary"]["end_level"], 5)

    def test_level_supply_xp_is_ratio_scaled_rounded_and_rate_adjusted(self):
        with database() as conn:
            levels_1_to_6(conn)
            group(conn, 5, 0.0, 0.0, 2000.0, 1000.0)
            zone(conn, 9, 5)
            level_supply(conn, 2, 1001)
            level_supply(conn, 3, 5)
            level_supply(conn, 4, 15)

            # Plain quest: the level-2 supply is 1.0, banker's rounded.
            quest(conn, 10, "plain", 4, 2, zone=9)
            component(conn, 1000, 10)
            accept_npc(conn, 1, 1000, 800)
            component(conn, 1001, 10, kind=8)
            # let_it_done quest with an immediate act XP that is not scaled.
            quest(conn, 20, "let it done", 4, 2, zone=9, let_it_done=True)
            component(conn, 2000, 20)
            accept_npc(conn, 2, 2000, 801)
            component(conn, 2001, 20, kind=8)
            supply_xp(conn, 3, 2001, 100)
            # A quest with no Reward component never gets the level supply.
            quest(conn, 30, "no reward", 4, 2, zone=9)
            component(conn, 3000, 30)
            accept_npc(conn, 4, 3000, 802)

            spawn_data = spawns((800, (0.0, 0.0, 0.0)),
                                (801, (1.0, 1.0, 0.0)),
                                (802, (2.0, 2.0, 0.0)))
            orderer = make_orderer(conn, spawn_data, let_it_done_ratio=0.3)
            ctxs = {r["id"]: dict(r) for r in conn.execute("SELECT * FROM quest_contexts")}
            self.assertEqual(orderer._quest_xp(ctxs[10]), (1001, {
                "act": 0, "supply": 1001, "ratio": 1.0}))
            # act 100 + round_even(1001 * 0.3) = 100 + 300
            self.assertEqual(orderer._quest_xp(ctxs[20]), (400, {
                "act": 100, "supply": 300, "ratio": 0.3}))
            self.assertEqual(orderer._quest_xp(ctxs[30]), (0, {
                "act": 0, "supply": 0, "ratio": 1.0}))

            over = make_orderer(conn, spawn_data, let_it_done_ratio=1.2)
            self.assertEqual(over._quest_xp(ctxs[20])[0], 1301)  # 100 + 1201
            double = make_orderer(conn, spawn_data, exp_rate=2.0)
            self.assertEqual(double._quest_xp(ctxs[10])[0], 2002)

            self.assertEqual(round_even(2.5), 2)
            self.assertEqual(round_even(3.5), 4)
            self.assertEqual(round_even(1.5), 2)
            self.assertEqual(round_even(4.5), 4)

    def test_kill_xp_estimate_is_added_per_quest(self):
        with database() as conn:
            level_row(conn, 1, 0)
            level_row(conn, 2, 100)
            group(conn, 5, 0.0, 0.0, 2000.0, 1000.0)
            zone(conn, 9, 5)
            level_supply(conn, 1, 60)
            quest(conn, 10, "story", 3, 1, chapter=1, quest_idx=1, zone=9)
            component(conn, 1000, 10)
            accept_npc(conn, 1, 1000, 800)
            component(conn, 1001, 10, kind=8)
            result = make_orderer(conn, spawns((800, (0.0, 0.0, 0.0))),
                                  max_level=2, kill_xp=50).run()
            quest_row = result["quests"][0]
            self.assertEqual(quest_row["xp"], 60)
            self.assertEqual(quest_row["level_before"], 1)
            self.assertEqual(quest_row["level_after"], 2)
            self.assertEqual(result["report"]["summary"]["end_xp"], 110)

    def test_level_supply_loader(self):
        with database() as conn:
            level_supply(conn, 1, 420, 33)
            level_supply(conn, 2, 490, 42)
            self.assertEqual(load_quest_supplies(conn),
                             {1: {"exp": 420, "copper": 33},
                              2: {"exp": 490, "copper": 42}})

    def test_or_group_accept_level_up_and_prereq_output(self):
        with database() as conn:
            levels_1_to_6(conn)
            group(conn, 5, 0.0, 0.0, 2000.0, 1000.0)
            zone(conn, 9, 5)

            # OR group: wrong race, but the level alternative passes.
            quest(conn, 300, "or group", 4, 1, zone=9)
            component(conn, 3000, 300, or_group=True)
            req(conn, 1, 3000, 3, 4)
            req(conn, 2, 3000, 1, 1)
            accept_npc(conn, 1, 3000, 500)
            # Non-OR: level 3 gate is not met at level 1.
            quest(conn, 301, "level gate", 4, 3, zone=9)
            component(conn, 3001, 301)
            req(conn, 3, 3001, 3, 1)
            req(conn, 4, 3001, 1, 3)
            accept_npc(conn, 2, 3001, 501)
            # Level-up starter.
            quest(conn, 400, "level up", 4, 2, zone=9)
            component(conn, 3002, 400)
            level_up(conn, 3, 3002, 3)
            # OR prerequisite output: only the satisfied alternative counts.
            quest(conn, 303, "or prereq", 4, 1, zone=9)
            component(conn, 3003, 303, or_group=True)
            req(conn, 5, 3003, 31, 10)
            req(conn, 6, 3003, 31, 11)
            accept_npc(conn, 4, 3003, 502)

            spawn_data = spawns((500, (0.0, 0.0, 0.0)),
                                (501, (1.0, 1.0, 0.0)),
                                (502, (2.0, 2.0, 0.0)))
            result = make_orderer(conn, spawn_data).run()
            ids = [q["quest_id"] for q in result["quests"]]
            self.assertIn(300, ids)
            self.assertNotIn(301, ids)
            self.assertNotIn(400, ids)
            pending = {e["quest_id"]: e["reason"]
                       for e in result["report"]["not_scheduled"]["side"]}
            self.assertEqual(pending.get(301), "needs_level")
            # A level-up-only starter has no player-clickable NPC, so it is
            # reported with the other no-starter side quests.
            self.assertIn(400, result["report"]["excluded"]
                          ["side_without_player_starter"])

            from player_order import load_start_data
            comps = load_start_data(conn)
            ctxs = {r["id"]: dict(r) for r in conn.execute("SELECT * FROM quest_contexts")}
            at_one = quest_state(ctxs[400], comps, 1, set(), 1, None, spawn_data)
            at_three = quest_state(ctxs[400], comps, 3, set(), 1, None, spawn_data)
            self.assertFalse(at_one["runnable"])
            self.assertTrue(at_three["runnable"])
            self.assertEqual(at_three["start_kind"], "level_up")

            # 10 is not in this fixture; add it so the OR prereq has one
            # satisfied alternative (11 stays unsatisfied).
            quest(conn, 10, "a", 4, 1, zone=9)
            component(conn, 3010, 10)
            accept_npc(conn, 10, 3010, 503)
            comps = load_start_data(conn)
            ctxs = {r["id"]: dict(r) for r in conn.execute("SELECT * FROM quest_contexts")}
            state = quest_state(ctxs[303], comps, 1, {10}, 1, None, spawn_data)
            self.assertEqual(state["prereqs"], [10])

    def test_exclusive_alternative_is_deferred(self):
        with database() as conn:
            levels_1_to_6(conn)
            group(conn, 5, 0.0, 0.0, 2000.0, 1000.0)
            zone(conn, 9, 5)
            quest(conn, 500, "keeps 501 open", 4, 1, zone=9)
            component(conn, 5000, 500)
            req(conn, 1, 5000, 0x24, 501)  # not completed 501
            accept_npc(conn, 1, 5000, 700)
            quest(conn, 501, "exclusive", 4, 1, zone=9)
            component(conn, 5001, 501)
            accept_npc(conn, 2, 5001, 701)
            result = make_orderer(conn, spawns(
                (700, (10.0, 10.0, 0.0)), (701, (11.0, 11.0, 0.0)),
            )).run()
            ids = [q["quest_id"] for q in result["quests"]]
            self.assertIn(500, ids)
            self.assertIn(501, ids)
            self.assertLess(ids.index(500), ids.index(501))

    def test_story_blocked_and_out_of_range(self):
        with database() as conn:
            levels_1_to_6(conn)
            group(conn, 5, 0.0, 0.0, 2000.0, 1000.0)
            zone(conn, 9, 5)
            quest(conn, 10, "first", 3, 1, chapter=1, quest_idx=1, zone=9)
            component(conn, 1000, 10)
            accept_npc(conn, 1, 1000, 800)
            quest(conn, 11, "blocked", 3, 2, chapter=1, quest_idx=2, zone=9)
            component(conn, 1001, 11)
            req(conn, 1, 1001, 1, 5)
            accept_npc(conn, 2, 1001, 801)
            quest(conn, 12, "out of range", 3, 10, chapter=1, quest_idx=3, zone=9)
            component(conn, 1002, 12)
            accept_npc(conn, 3, 1002, 802)
            result = make_orderer(conn, spawns(
                (800, (0.0, 0.0, 0.0)), (801, (1.0, 1.0, 0.0)),
                (802, (2.0, 2.0, 0.0)),
            ), max_level=3).run()
            ids = [q["quest_id"] for q in result["quests"]]
            self.assertIn(10, ids)
            self.assertNotIn(11, ids)
            self.assertNotIn(12, ids)
            blocked = result["report"]["never_available"]["story"]
            self.assertEqual([entry["quest_id"] for entry in blocked], [11])
            self.assertEqual(blocked[0]["reason"], "level_gate")
            self.assertIn(12, result["report"]["excluded"]["story_above_level_range"])

    def test_skip_prologue_marks_it_completed(self):
        with database() as conn:
            levels_1_to_6(conn)
            group(conn, 5, 0.0, 0.0, 2000.0, 1000.0)
            zone(conn, 9, 5)
            quest(conn, 100, "prologue", 108, 1, zone=1)
            component(conn, 1000, 100)
            accept_npc(conn, 1, 1000, 700)
            quest(conn, 10, "story", 3, 2, chapter=1, quest_idx=1, zone=9)
            component(conn, 1001, 10)
            accept_npc(conn, 2, 1001, 800)
            result = make_orderer(conn, spawns(
                (700, (0.0, 0.0, 0.0)), (800, (1.0, 1.0, 0.0)),
            ), start_level=2, assume_previous=False).run()
            ids = [q["quest_id"] for q in result["quests"]]
            self.assertNotIn(100, ids)
            self.assertIn(10, ids)
            self.assertIn(100, result["report"]["assumed_completed"])

    def test_xp_loader_ignores_non_supply_exp_details(self):
        with database() as conn:
            quest(conn, 10, "xp", 4, 1, zone=9)
            component(conn, 1000, 10, kind=8)
            # A detail id collision with a supply item must not count as XP:
            # the row exists in quest_act_supply_exps but the act is not
            # QuestActSupplyExp.
            conn.execute("INSERT INTO quest_act_supply_exps VALUES (7, 999)")
            act(conn, 1, 1000, 7, "QuestActSupplyItem")
            self.assertEqual(load_quest_xp(conn).get(10, 0), 0)
            conn.execute("INSERT INTO quest_act_supply_exps VALUES (8, 500)")
            act(conn, 2, 1000, 8, "QuestActSupplyExp")
            self.assertEqual(load_quest_xp(conn).get(10), 500)

    def test_zone_neighbours_and_box_distance(self):
        with database() as conn:
            zone(conn, 1, 1)
            zone(conn, 2, 1)
            zone(conn, 3, 2)
            zone(conn, 4, 3)
            group(conn, 1, 0.0, 0.0, 100.0, 100.0)
            group(conn, 2, 200.0, 0.0, 100.0, 100.0)
            group(conn, 3, 10000.0, 10000.0, 100.0, 100.0)
            zones = {1: 1, 2: 1, 3: 2, 4: 3}
            groups = {1: (0.0, 0.0, 100.0, 100.0),
                      2: (200.0, 0.0, 100.0, 100.0),
                      3: (10000.0, 10000.0, 100.0, 100.0)}
            self.assertAlmostEqual(box_distance(groups[1], groups[2]), 100.0)
            self.assertEqual(neighbours_of(1, zones, groups, 150.0, {}),
                             {1, 2, 3})
            self.assertEqual(neighbours_of(1, zones, groups, 50.0, {}), {1, 2})
            self.assertEqual(neighbours_of(4, zones, groups, 150.0, {}), {4})

    def test_level_thresholds(self):
        levels = [{"id": 1, "total_exp": 0}, {"id": 2, "total_exp": 100},
                  {"id": 3, "total_exp": 300}]
        self.assertEqual(level_for_xp(0, levels), 1)
        self.assertEqual(level_for_xp(99, levels), 1)
        self.assertEqual(level_for_xp(100, levels), 2)
        self.assertEqual(level_for_xp(500, levels), 3)
        self.assertEqual(xp_for_level(3, levels), 300)
        self.assertEqual(xp_for_level(9, levels), 0)


@unittest.skipUnless(
    os.environ.get("AAEMU_COMPACT_DB") and os.environ.get("AAEMU_COMPACT_WORLDS"),
    "set AAEMU_COMPACT_DB and AAEMU_COMPACT_WORLDS for the real-data check",
)
class RealCompactDbTests(unittest.TestCase):
    def test_nuian_1_to_15_order_and_prerequisites(self):
        db = Path(os.environ["AAEMU_COMPACT_DB"])
        worlds = Path(os.environ["AAEMU_COMPACT_WORLDS"])
        conn = sqlite3.connect(f"file:{db.as_posix()}?mode=ro", uri=True)
        conn.row_factory = sqlite3.Row
        try:
            result = Orderer(conn, load_spawns(conn, worlds), "nuian",
                             start_level=1, max_level=15).run()
        finally:
            conn.close()
        ids = [q["quest_id"] for q in result["quests"]]
        by_id = {q["quest_id"]: q for q in result["quests"]}
        self.assertIn(2531, by_id)
        self.assertIn(2532, by_id)
        self.assertIn(2255, by_id)
        self.assertLess(ids.index(2531), ids.index(2532))
        self.assertLess(ids.index(2532), ids.index(2255))
        for index, quest in enumerate(result["quests"]):
            for prereq in quest.get("prereq_quests", []):
                self.assertIn(prereq, by_id, quest["quest_id"])
                self.assertLess(ids.index(prereq), index, quest["quest_id"])
        summary = result["report"]["summary"]
        self.assertEqual(summary["scheduled"]["story"], 26)
        self.assertEqual(summary["scheduled"]["prologue"], 18)
        self.assertGreaterEqual(summary["scheduled"]["side"], 100)
        self.assertGreaterEqual(summary["end_level"], 15)
        self.assertGreater(summary["end_xp"], 100000)


if __name__ == "__main__":
    unittest.main()
