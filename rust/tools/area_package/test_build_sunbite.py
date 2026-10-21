"""Unit tests of the Sunbite package builder (no database or client pak needed)."""

import os
import struct
import sys
import tempfile
import unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import build_sunbite as b  # noqa: E402
import client_pak  # noqa: E402


class SqlTests(unittest.TestCase):
    def test_sql_value_escapes_quotes_and_handles_types(self):
        self.assertEqual(b.sql_value(None), "NULL")
        self.assertEqual(b.sql_value(True), "1")
        self.assertEqual(b.sql_value(3.5), "3.5")
        self.assertEqual(b.sql_value("it's"), "'it''s'")
        self.assertEqual(b.sql_value(b"\x01\x02"), "X'0102'")

    def test_insert_is_guarded_by_id(self):
        b._TARGET = None
        sql = b.insert_statement("npc_spawners", ["id", "name"], [19150, "x"])
        self.assertIn("INSERT INTO", sql)
        self.assertIn("WHERE NOT EXISTS", sql)
        self.assertIn('"id" = 19150', sql)

    def test_insert_uses_the_natural_key_when_given(self):
        b._TARGET = None
        sql = b.insert_statement(
            "npc_initial_buffs",
            ["npc_id", "buff_id"],
            [10, 20],
            natural_key=("npc_id", "buff_id"),
        )
        self.assertIn('"npc_id" = 10 AND "buff_id" = 20', sql)


class IdTests(unittest.TestCase):
    def test_allocates_free_ids_and_keeps_free_keys(self):
        ids = b.Ids(10)
        self.assertEqual(ids.maybe(3, lambda _: False), 3)
        self.assertEqual(ids.maybe(4, lambda _: True), 10)
        self.assertEqual(ids.maybe(5, lambda _: True), 11)
        self.assertEqual(ids.get("x"), 12)
        self.assertEqual(ids.get("x"), 12)


class WorldTests(unittest.TestCase):
    XML = (
        '<World name="main_world" cellXCount="35" cellYCount="38">'
        '<ZoneList>'
        '<Zone name="e_sunny_wilderness_4" id="287" originX="18" originY="5">'
        '<cellList><cell x="18" y="5"><sectorList><sector x="0" y="0"/></sectorList></cell>'
        "</cellList></Zone>"
        "</ZoneList></World>"
    )

    def test_zone_block_and_sector_owner(self):
        block = b.zone_block(self.XML, 287)
        self.assertIn('id="287"', block)
        owners = b.sector_owners(self.XML)
        self.assertEqual(owners[(16 * 18, 16 * 5)], 287)

    def test_nearest_sector_uses_distance(self):
        owners = {(1, 1): 10, (5, 5): 20}
        self.assertEqual(b.nearest_sector(owners, 2, 2)[2], 10)
        self.assertEqual(b.nearest_sector(owners, 4 * 64, 4 * 64)[2], 20)


class NpcMapTests(unittest.TestCase):
    def test_npc_map_records_are_u32_xyz(self):
        class FakePak:
            def read(self, _name):
                return struct.pack("<Ifff", 502, 19368.3, 6611.6, 263.3)

        records = b.npc_map_records(FakePak())
        self.assertEqual(len(records), 1)
        self.assertEqual(records[0][0], 502)
        self.assertAlmostEqual(records[0][1], 19368.3, places=1)


class PakTests(unittest.TestCase):
    def test_round_trip(self):
        with tempfile.TemporaryDirectory() as root:
            path = os.path.join(root, "overlay.pak")
            size = client_pak.write_pak(
                path,
                [("game/worlds/main_world/world.xml", b"<World/>"), ("game/a.bin", b"\0\1")],
            )
            self.assertGreater(size, 0)
            with client_pak.Pak(path) as pak:
                self.assertEqual(pak.kind, "A")
                self.assertEqual(pak.read("game/worlds/main_world/world.xml"), b"<World/>")
                self.assertEqual(pak.read("GAME/A.BIN"), b"\0\1")


class RenderTests(unittest.TestCase):
    def test_patch_header_and_determinism(self):
        summary = {"a": 1, "b": 2}
        text = b.render_patch("header", ["INSERT INTO t VALUES (1);"], summary)
        self.assertTrue(text.startswith("-- header\n"))
        self.assertEqual(text, b.render_patch("header", ["INSERT INTO t VALUES (1);"], summary))
        self.assertIn('"a": 1', text)


if __name__ == "__main__":
    unittest.main()
