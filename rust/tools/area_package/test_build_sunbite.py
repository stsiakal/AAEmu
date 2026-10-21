"""Unit tests of the Sunbite package builder (no database or client files needed)."""

import json
import os
import sqlite3
import sys
import tempfile
import unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import build_sunbite as b  # noqa: E402


class SqlTests(unittest.TestCase):
    def test_sql_value_escapes_quotes_and_handles_types(self):
        self.assertEqual(b.sql_value(None), "NULL")
        self.assertEqual(b.sql_value(True), "1")
        self.assertEqual(b.sql_value(3.5), "3.5")
        self.assertEqual(b.sql_value("it's"), "'it''s'")
        self.assertEqual(b.sql_value(b"\x01\x02"), "X'0102'")

    def test_insert_is_guarded_by_id_without_a_primary_key(self):
        b._TARGET = None
        sql = b.insert_statement("npc_spawners", ["id", "name"], [19150, "x"])
        self.assertIn("INSERT INTO", sql)
        self.assertIn("WHERE NOT EXISTS", sql)
        self.assertIn('"id" = 19150', sql)
        b._TARGET = None

    def test_insert_uses_the_natural_key_when_given(self):
        b._TARGET = None
        sql = b.insert_statement(
            "npc_initial_buffs",
            ["npc_id", "buff_id"],
            [10, 20],
            natural_key=("npc_id", "buff_id"),
        )
        self.assertIn('"npc_id" = 10 AND "buff_id" = 20', sql)

    def test_nulls_get_type_defaults(self):
        conn = sqlite3.connect(":memory:")
        conn.execute("CREATE TABLE t (i INT, r REAL, s TEXT, b BLOB)")
        b._TARGET = conn
        b._NULL_DEFAULTS.clear()
        sql = b.insert_statement("t", ["i", "r", "s", "b"], [None, None, None, None])
        self.assertEqual(
            sql,
            "INSERT OR IGNORE INTO \"t\" (\"i\", \"r\", \"s\", \"b\") "
            "VALUES (0, 0.0, '', X'');",
        )
        b._TARGET = None


class IdTests(unittest.TestCase):
    def test_allocates_free_ids_and_keeps_free_keys(self):
        ids = b.Ids(10)
        self.assertEqual(ids.maybe(3, lambda _: False), 3)
        self.assertEqual(ids.maybe(4, lambda _: True), 10)
        self.assertEqual(ids.maybe(5, lambda _: True), 11)
        self.assertEqual(ids.get("x"), 12)
        self.assertEqual(ids.get("x"), 12)


class LoaderTests(unittest.TestCase):
    def test_loader_table_map_is_complete(self):
        tables = b.loader_tables()
        self.assertGreater(len(tables), 50)
        self.assertEqual(
            tables["QuestActSupplyItem"], "quest_act_supply_items"
        )
        self.assertEqual(
            tables["QuestActConAcceptNpc"], "quest_act_con_accept_npcs"
        )


class SpawnTests(unittest.TestCase):
    def test_height_snap_keeps_near_terrain_and_authored_z_otherwise(self):
        self.assertEqual(
            b.resolve_z({"id": "a", "z": 100.0}, {"a": {"h": 102.0}}),
            (102.0, "terrain"),
        )
        self.assertEqual(
            b.resolve_z({"id": "a", "z": 100.0}, {"a": {"h": 130.0}}),
            (100.0, "authored"),
        )
        self.assertEqual(b.resolve_z({"id": "a", "z": 100.0}, {}), (100.0, "authored"))

    def test_collect_spawns_translates_the_zone_frame(self):
        # A synthetic 10.x world.xml origin (3, 1) and a 1.2 origin (1, 1), one spawner.
        with tempfile.TemporaryDirectory() as root:
            w10 = os.path.join(root, "w10")
            w12 = os.path.join(root, "w12")
            pak = os.path.join(root, "pak")
            for world, origin in ((w10, (3, 1)), (w12, (1, 1))):
                path = os.path.join(world, "game", "worlds", "main_world")
                os.makedirs(path)
                with open(os.path.join(path, "world.xml"), "w", encoding="utf-8") as f:
                    f.write(
                        '<World name="main_world"><Zone name="e_sunny_wilderness_4" '
                        'id="287" originX="%d" originY="%d"/></World>'
                        % origin
                    )
            spawner = os.path.join(
                pak,
                "game",
                "worlds",
                "main_world",
                "level_design",
                "zone",
                "287",
                "zone_server",
            )
            os.makedirs(spawner)
            with open(os.path.join(spawner, "npc_spawners.g"), "w", encoding="utf-8") as f:
                f.write(
                    "spawner\n spawnerId 7\n spawnerType 19798\n pos (x 10, y 20, z 30)\n"
                )
            blocks = b.collect_spawns(None, pak, w10, w12)
        self.assertEqual(len(blocks), 1)
        self.assertEqual(blocks[0]["template"], 19798)
        self.assertEqual((blocks[0]["x"], blocks[0]["y"]), (1024 + 10, 1024 + 20))
        self.assertEqual(blocks[0]["z"], 30)


    def test_cell_map_keeps_points_in_the_owned_zone_sectors(self):
        with tempfile.TemporaryDirectory() as root:
            w10, w12, pak = (os.path.join(root, x) for x in ("w10", "w12", "pak"))
            for world, cells, origin in (
                (w10, [(3, 1), (4, 1)], (3, 1)),
                (w12, [(1, 1), (1, 2)], (1, 1)),
            ):
                path = os.path.join(world, "game", "worlds", "main_world")
                os.makedirs(path)
                body = "".join(
                    '<cell x="%d" y="%d"><sectorList><sector x="0" y="0"/></sectorList></cell>'
                    % c
                    for c in cells
                )
                with open(os.path.join(path, "world.xml"), "w", encoding="utf-8") as f:
                    f.write(
                        '<World name="main_world"><Zone name="e_sunny_wilderness_4" '
                        'id="287" originX="%d" originY="%d">%s</Zone></World>'
                        % (origin[0], origin[1], body)
                    )
            spawner = os.path.join(
                pak,
                "game",
                "worlds",
                "main_world",
                "level_design",
                "zone",
                "287",
                "zone_server",
            )
            os.makedirs(spawner)
            with open(os.path.join(spawner, "npc_spawners.g"), "w", encoding="utf-8") as f:
                f.write(
                    "spawner\n spawnerId 7\n spawnerType 19798\n pos (x 10, y 20, z 30)\n"
                )
            blocks = b.collect_spawns(None, pak, w10, w12)
            blocks = b.cell_map_spawns(blocks, w10, w12)
            b.assert_spawns_in_zone(blocks, w12)
        self.assertEqual(len(blocks), 1)
        self.assertEqual(int(blocks[0]["x"] // 1024), 1)
        self.assertEqual(int(blocks[0]["y"] // 1024), 1)


class RenderTests(unittest.TestCase):
    def test_patch_header_and_determinism(self):
        summary = {"a": 1, "b": 2}
        text = b.render_patch("x.sql", "header", ["INSERT INTO t VALUES (1);"], summary)
        self.assertTrue(text.startswith("-- header\n"))
        self.assertEqual(text, b.render_patch("x.sql", "header", ["INSERT INTO t VALUES (1);"], summary))
        self.assertIn('"a": 1', text)


if __name__ == "__main__":
    unittest.main()
